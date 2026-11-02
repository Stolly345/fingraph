//! Budget: a monthly amount per category, suggested from what you usually spend (the median of the
//! last six months), with this month's spending against it and where the month is heading; a plan
//! built from your pay (the setup wizard) that projects what you'll save; and alerts when a category
//! goes over, or uses 85% with more of the month still to come.

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::budget::{self as core, Alert, Deduction, Envelope, Fund, PayCycle, Plan, scope, suggest};
use fingraph_core::db::{Category, TxRow};
use fingraph_core::insights::{add_months, by_category, month_start};
use fingraph_core::money::{Currency, Fx, fmt_money, parse_user};
use gtk::glib;
use rust_decimal::prelude::ToPrimitive;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, LineSeries};
use crate::{App, clear, dim, heading, info_caption, info_heading, today, with_info};

const MONTHS: i32 = 6;

thread_local! {
    static MONTH: Cell<Option<NaiveDate>> = const { Cell::new(None) };
}

fn month() -> NaiveDate {
    MONTH.with(Cell::get).unwrap_or_else(|| month_start(today()))
}

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// Everything the page, the alerts and the wizard read, in the display currency.
pub(crate) struct Snap {
    pub(crate) ccy: Currency,
    pub(crate) fx: Fx,
    pub(crate) parents: BTreeMap<i64, String>,
    pub(crate) scope: String,
    pub(crate) rows: Vec<TxRow>,
    /// Top-level expense categories.
    pub(crate) cats: Vec<Category>,
    pub(crate) budgets: BTreeMap<i64, Decimal>,
    pub(crate) plan: Option<Plan>,
    /// Categories counted as fixed expenses.
    pub(crate) fixed: Vec<String>,
    /// Yearly and quarterly bills set aside for.
    pub(crate) funds: Vec<Fund>,
}

impl Snap {
    fn is_fixed(&self, category: &str) -> bool {
        self.fixed.iter().any(|c| c == category)
    }
}

pub(crate) fn snap(app: &App) -> Snap {
    let ccy = app.display();
    let fx = app.db.fx().unwrap_or_default();
    let sc = scope(&app.entity_ids());
    let rows = crate::changes::counted_rows(app, app.all_rows());
    let cats = app.categories().into_iter().filter(|c| c.parent_id.is_none() && c.kind == fingraph_core::db::CategoryKind::Expense).collect();
    let budgets = app.db.budgets(&sc).unwrap_or_default().into_iter().map(|(c, (v, k))| (c, fx.convert(v, k, ccy))).collect();
    let plan = app.db.plan(&sc).map(|p| plan_in(p, ccy, &fx));
    let funds = app.db.funds(&sc).unwrap_or_default();
    Snap { ccy, fx, parents: app.parents(), scope: sc, rows, cats, budgets, plan, fixed: app.db.fixed_categories(), funds }
}

/// Each category's month: budget then, rollover, moves, spending.
pub(crate) fn envs(app: &App, s: &Snap, m: NaiveDate) -> BTreeMap<i64, Envelope> {
    let pairs: Vec<(i64, String)> = s.cats.iter().map(|c| (c.id, c.name.clone())).collect();
    core::envelopes(&app.db, &s.scope, &s.rows, &pairs, m, s.ccy, &s.fx, &s.parents).unwrap_or_default()
}

/// What the non-monthly bills take each month, in the display currency.
fn funds_monthly(s: &Snap) -> Decimal {
    s.funds.iter().map(|f| s.fx.convert(f.monthly(), f.currency, s.ccy)).sum()
}

/// A saved plan's amounts in the display currency (percent deductions stay percent).
fn plan_in(mut p: Plan, ccy: Currency, fx: &Fx) -> Plan {
    let from = p.currency.unwrap_or(ccy);
    let c = |d: Decimal| fx.convert(d, from, ccy).round_dp(2);
    p.gross = c(p.gross);
    p.other_income = c(p.other_income);
    p.loan_payments = c(p.loan_payments);
    p.savings_goal = c(p.savings_goal);
    for d in p.deductions.iter_mut().filter(|d| !d.percent) {
        d.amount = c(d.amount);
    }
    p.currency = Some(ccy);
    p
}

/// The last day the statements cover (the newest statement's end), never after today.
pub(crate) fn data_through(app: &App) -> NaiveDate {
    app.db.documents(&app.entity_ids()).unwrap_or_default().iter().filter_map(|d| d.period.map(|p| p.1)).max().unwrap_or(today()).min(today())
}

/// How much of `m` the statements cover (0–1): statements arrive a month late, so this month is
/// measured to the newest statement, not to today.
fn elapsed(m: NaiveDate, through: NaiveDate) -> Decimal {
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    if through >= end {
        Decimal::ONE
    } else if through < m {
        Decimal::ZERO
    } else {
        Decimal::from(through.day()) / Decimal::from(end.day())
    }
}

/// This month's categories that are over budget, or 85% used with more than 15% of the month to go.
pub fn alerts(app: &App) -> Vec<(String, Decimal, Decimal, Alert)> {
    let s = snap(app);
    let m = month_start(today());
    let env = envs(app, &s, m);
    let rolls = app.db.rollovers(&s.scope);
    let done = elapsed(m, data_through(app));
    let mut out: Vec<_> = s
        .cats
        .iter()
        .filter_map(|c| {
            let e = env.get(&c.id)?;
            if e.budget.is_zero() && !rolls.contains_key(&c.id) {
                return None;
            }
            core::alert(e.spent, e.funded(), done).map(|a| (c.name.clone(), e.spent, e.funded(), a))
        })
        .collect();
    // watchlists: a merchant over its monthly limit
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    for wl in app.db.watchlists(&s.scope) {
        let limit = s.fx.convert(wl.limit, wl.currency, s.ccy);
        let (spent, _) = core::watched(&s.rows, &wl, m, end, s.ccy, &s.fx);
        if let Some(al) = core::alert(spent, limit, elapsed(m, data_through(app))) {
            out.push((format!("Watch: {}", wl.name), spent, limit, al));
        }
    }
    out.sort_by_key(|x| (x.3 != Alert::Over, std::cmp::Reverse(x.1 - x.2)));
    out
}

/// This month's budget for Overview: (funded, spent) in budgeted categories, None without a budget.
pub fn left_this_month(app: &App) -> Option<(Decimal, Decimal)> {
    let s = snap(app);
    let m = month_start(today());
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    let env = envs(app, &s, m);
    let rolls = app.db.rollovers(&s.scope);
    let now: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, s.ccy, &s.fx, &s.parents).into_iter().collect();
    let budgeted: Vec<&Category> = s.cats.iter().filter(|c| env.get(&c.id).is_some_and(|e| !e.budget.is_zero()) || rolls.contains_key(&c.id)).collect();
    let funded: Decimal = budgeted.iter().filter_map(|c| env.get(&c.id)).map(Envelope::funded).sum();
    let spent: Decimal = budgeted.iter().filter_map(|c| now.get(&c.name)).copied().sum();
    (!funded.is_zero()).then_some((funded, spent))
}

/// The soonest big bill: (name, due, amount, set aside so far) in the display currency.
pub fn next_big_bill(app: &App) -> Option<(String, NaiveDate, Decimal, Decimal)> {
    let s = snap(app);
    let t = today();
    let fd = s.funds.iter().min_by_key(|f| f.due(t))?;
    Some((fd.name.clone(), fd.due(t), s.fx.convert(fd.amount, fd.currency, s.ccy), s.fx.convert(fd.set_aside(t), fd.currency, s.ccy)))
}

fn alert_text(a: &(String, Decimal, Decimal, Alert), ccy: Currency, through: NaiveDate) -> String {
    match a.3 {
        Alert::Over => format!("{} over by {}", a.0, fmt_money(a.1 - a.2, ccy)),
        Alert::Near => format!("{} {}% used by {}", a.0, (a.1 / a.2 * Decimal::ONE_HUNDRED).round_dp(0), through.format("%d %b")),
    }
}

/// The sidebar badge: "2 over", or "1 near".
pub fn badge(app: &App) -> (String, bool) {
    let a = alerts(app);
    let over = a.iter().filter(|x| x.3 == Alert::Over).count();
    let near = a.len() - over;
    if over > 0 {
        (format!("{over} over"), true)
    } else if near > 0 {
        (format!("{near} near"), false)
    } else {
        (String::new(), false)
    }
}

/// The Overview's budget card, when something needs a look.
pub fn fill_overview(app: &Rc<App>, into: &gtk::Box) {
    clear(into);
    let a = alerts(app);
    if a.is_empty() {
        return;
    }
    let ccy = app.display();
    let row = adw::ActionRow::builder()
        .title(if a.iter().any(|x| x.3 == Alert::Over) { "Budget: over in this month" } else { "Budget: running low" })
        .subtitle(glib::markup_escape_text(&a.iter().map(|x| alert_text(x, ccy, data_through(app))).collect::<Vec<_>>().join(" · ")))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let open = gtk::Button::builder().label("Open Budget").valign(gtk::Align::Center).build();
    let w = Rc::downgrade(app);
    open.connect_clicked(move |_| {
        if let Some(a) = w.upgrade() {
            a.show_view("budget");
        }
    });
    row.add_suffix(&open);
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    list.append(&row);
    into.append(&list);
}

/// Overview's two big-bill cards: the next ones due with what's set aside, and which months the
/// big spends land in over the next year. Without any, one card offers to add them.
pub fn fill_big_bills(app: &Rc<App>, into: &gtk::Box) {
    clear(into);
    let s = snap(app);
    let (ccy, t) = (s.ccy, today());
    let money = |d: Decimal| fmt_money(d, ccy);
    let card = |title: &str, help: &str| {
        let c = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["card", "chart-card"]).build();
        c.append(&info_heading(title, help));
        c
    };
    if s.funds.is_empty() {
        let c = card(
            "Big Yearly Bills",
            "Car and home insurance, HOA fees, licences, school terms: bills that don't come every month. Add them and FinGraph sets a share aside each month, shows them here before they're due, and counts them in your budget.",
        );
        c.append(&dim("None planned yet. Add the big bills you pay once or a few times a year, so they're saved for before they land."));
        let add = gtk::Button::builder().label("Add Big Bills…").halign(gtk::Align::Start).css_classes(["pill"]).build();
        let w = Rc::downgrade(app);
        add.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let w2 = Rc::downgrade(&a);
                wizard(&a, Rc::new(move || if let Some(a) = w2.upgrade() { a.refresh() }));
            }
        });
        c.append(&add);
        into.append(&c);
        return;
    }
    let row = gtk::Box::builder().spacing(12).homogeneous(true).build();

    // ── the next ones due
    let next = card(
        "Big Bills Coming Up",
        "Your next big bills, soonest first. The bar shows how much has been set aside so far towards each one; a full bar means the money is there by its due date. Change them on the Budget page under Non-Monthly Bills.",
    );
    let mut funds: Vec<&Fund> = s.funds.iter().collect();
    funds.sort_by_key(|f| f.due(t));
    let l = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    for fd in funds.iter().take(5) {
        let due = fd.due(t);
        let months = (due.year() - t.year()) * 12 + due.month() as i32 - t.month() as i32;
        let when = match months {
            0 => "this month".to_string(),
            1 => "next month".to_string(),
            n => format!("in {n} months"),
        };
        let r = adw::ActionRow::builder().title(glib::markup_escape_text(&fd.name)).subtitle(format!("Due {} · {when}", due.format("%d %b %Y"))).build();
        let (aside, whole) = (s.fx.convert(fd.set_aside(t), fd.currency, ccy), s.fx.convert(fd.amount, fd.currency, ccy));
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(170).build();
        col.append(&gtk::Label::builder().label(format!("{} of {}", money(aside), money(whole))).xalign(1.0).css_classes(["tnum", "caption"]).build());
        col.append(&gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(if whole.is_zero() { 0.0 } else { f(aside / whole).min(1.0) }).build());
        r.add_suffix(&col);
        l.append(&r);
    }
    next.append(&l);
    if funds.len() > 5 {
        next.append(&dim(&format!("and {} more on the Budget page", funds.len() - 5)));
    }
    row.append(&next);

    // ── the year ahead, month by month
    let year = card(
        "Big Bills Over the Next 12 Months",
        "Each bar is what your big bills add up to in that month, so you can see the expensive months coming. The monthly set-aside below is what covers them: put that much away every month and the tall bars are already paid for. Hover a bar to see which bills land then.",
    );
    let start = month_start(t);
    let mut due: Vec<(Decimal, Vec<String>)> = vec![(Decimal::ZERO, Vec::new()); 12];
    for f in &s.funds {
        let mut d = month_start(f.due(t));
        while d < add_months(start, 12) {
            let k = ((d.year() - start.year()) * 12 + d.month() as i32 - start.month() as i32) as usize;
            if let Some(slot) = due.get_mut(k) {
                let v = s.fx.convert(f.amount, f.currency, ccy);
                slot.0 += v;
                slot.1.push(format!("{} {}", f.name, money(v)));
            }
            d = add_months(d, f.every_months.max(1));
        }
    }
    let chart = Chart::new(200);
    let sym = ccy.symbol();
    *chart.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
    chart.set(Data::Bars {
        labels: (0..12).map(|k| add_months(start, k).format("%b").to_string()).collect(),
        series: vec![charts::Series { name: "Due".into(), values: due.iter().map(|d| f(d.0)).collect(), color: charts::PURPLE }],
        line: None,
        tips: (0..12)
            .map(|k| {
                let (v, names) = &due[k as usize];
                let m = add_months(start, k).format("%B %Y");
                if names.is_empty() { format!("{m}\nNo big bills") } else { format!("{m}\n{}\nTotal {}", names.join("\n"), money(*v)) }
            })
            .collect(),
    });
    year.append(&chart.widget);
    let monthly: Decimal = s.funds.iter().map(|f| s.fx.convert(f.monthly(), f.currency, ccy)).sum();
    let total: Decimal = due.iter().map(|d| d.0).sum();
    year.append(&dim(&format!("{} due over the next 12 months · set aside {} a month to cover it", money(total), money(monthly))));
    row.append(&year);
    into.append(&row);
}

const SUBS: &str = "Subscriptions & Digital";

/// What each category usually costs a month (`suggest`), except Subscriptions & Digital: that one is
/// your active subscriptions' monthly cost (as on the Subscriptions page) plus the usual one-off
/// digital purchases (apps, domains, downloads), since a median of the two mixed reads as "subscriptions".
/// Also returns (subscriptions a month, other digital a month) for labelling.
pub(crate) fn usual(s: &Snap, m: NaiveDate) -> (BTreeMap<String, Decimal>, Decimal, Decimal) {
    let mut out = suggest(&s.rows, m, MONTHS, s.ccy, &s.fx, &s.parents);
    let subs: Decimal = fingraph_core::insights::subscriptions(&s.rows, s.ccy, &s.fx, today()).iter().filter(|x| x.active).map(|x| x.monthly).sum();
    let others: Vec<TxRow> = s.rows.iter().filter(|t| !t.is_subscription).cloned().collect();
    let one_off = suggest(&others, m, MONTHS, s.ccy, &s.fx, &s.parents).get(SUBS).copied().unwrap_or_default();
    let subs = subs.round_dp(0);
    let total = subs + one_off;
    if total.is_zero() {
        out.remove(SUBS);
    } else {
        out.insert(SUBS.into(), total);
    }
    (out, subs, one_off)
}

fn usual_text(name: &str, u: Decimal, subs: Decimal, one_off: Decimal, ccy: Currency) -> String {
    if name == SUBS {
        format!("usually {}: subscriptions {} a month + other digital purchases {}", fmt_money(u, ccy), fmt_money(subs, ccy), fmt_money(one_off, ccy))
    } else {
        format!("usually {}", fmt_money(u, ccy))
    }
}

fn kpi(title: &str, help: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes(["card", "kpi"]).build();
    b.append(&info_caption(title, help));
    let v = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).build();
    let s = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
    b.append(&v);
    b.append(&s);
    (b, v, s)
}

/// Loan and card instalments (Loan Payments) per month: the median of the last six months.
fn usual_loans(s: &Snap, m: NaiveDate) -> Decimal {
    let mut v: Vec<Decimal> = (1..=MONTHS)
        .map(|i| {
            let from = add_months(m, -i);
            let to = add_months(from, 1).pred_opt().unwrap_or(from);
            s.rows
                .iter()
                .filter(|t| t.posted >= from && t.posted <= to && t.amount.is_sign_negative())
                .filter(|t| t.category_id.and_then(|c| s.parents.get(&c)).or(t.category_name.as_ref()).is_some_and(|n| n == "Loan Payments"))
                .map(|t| -s.fx.convert(t.amount, t.currency, s.ccy))
                .sum()
        })
        .collect();
    v.sort();
    v[v.len() / 2].round_dp(0)
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();

    let nav = gtk::Box::builder().spacing(8).build();
    let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous month").build();
    let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next month").build();
    let title = heading("");
    title.add_css_class("title-2");
    title.set_hexpand(true);
    title.set_xalign(0.0);
    let setup = gtk::Button::builder().label("Set Up Budget…").tooltip_text("A guided setup: your pay and deductions, your spending, your savings goal").build();
    let close = gtk::Button::builder().label("Monthly Close…").tooltip_text("Go through a month once its statements are in: check them, sort what's new, settle the budget, tick off the bills").build();
    let fill = gtk::Button::builder().label("Fill From My Spending").tooltip_text(format!("Give every category without a budget what it usually costs (median of the last {MONTHS} months)")).build();
    nav.append(&prev);
    nav.append(&next);
    nav.append(&title);
    nav.append(&close);
    nav.append(&setup);
    nav.append(&fill);
    body.append(&nav);

    let ribbon = gtk::Box::builder().spacing(12).homogeneous(true).build();
    let (k1, budgeted, budgeted_s) = kpi(
        "Budgeted",
        "This month's budgets added up across your categories. If money rolled over from last month or you moved money between categories, the line below shows what you actually have to spend.",
    );
    let (k2, spent, spent_s) = kpi(
        "Spent",
        "What you've spent this month as far as your statements go, split into categories with a budget and categories without one. Statements arrive after the month ends, so it fills in late.",
    );
    let (k3, left, left_s) = kpi("Left", "What's left in your budgeted categories: the budget (with rollovers and moves) minus what's been spent in them. Red means you're over.");
    let (k4, pace, pace_s) = kpi(
        "Heading For",
        "Where this month's spending ends up if you keep the same pace: what's spent up to your newest statement, stretched to the whole month. A big bill early in the month makes it look higher than it will be.",
    );
    for k in [&k1, &k2, &k3, &k4] {
        ribbon.append(k);
    }
    body.append(&ribbon);

    // the plan: pay in, budget out, what's left to save, and where that leads
    let plan_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&plan_box);
    let split_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&split_box);
    let funds_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&funds_box);
    let projection = Chart::new(230);

    body.append(&info_heading(
        "Categories",
        "One row per category. The pill says whether it's a fixed or variable expense (click it to switch). Below the name: what it usually costs, last month, and any trend, rollover or money moved. On the right: what you've spent of its budget. The bar turns amber near the limit and red when over.",
    ));
    body.append(&dim(&format!(
        "Type a monthly amount for any category and press Enter (empty or 0 removes it). “Usually” is the median of the last {MONTHS} months — a one-off doesn't count. Transfers and categories left out in Preferences aren't spending."
    )));
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    body.append(&list);
    let watch_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&watch_box);
    let flow = Chart::new(300);
    let flow_card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
    let flow_title = heading("");
    flow_card.append(&with_info(
        &flow_title,
        "Money coming in is on the left and where it went is on the right; the thicker a band, the more money. If more came in than went out, the difference shows as Saved on the right. If more went out, the gap shows as From savings on the left. Hover a band for its amount.",
    ));
    flow_card.append(&dim("Money in on the left, where it went on the right (top-level categories; transfers between your own accounts left out). The difference is what you saved — or took from savings."));
    flow_card.append(&flow.widget);
    body.append(&flow_card);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1000).child(&body).build()).vexpand(true).build();

    // the page redraws itself after an edit; the sidebar and Overview follow
    let me: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    let redraw: crate::Refresh = {
        let (me, w) = (me.clone(), Rc::downgrade(app));
        Rc::new(move || {
            let f = me.borrow().clone(); // release the borrow before running it
            if let Some(f) = f {
                f();
            }
            if let Some(a) = w.upgrade() {
                a.update_sidebar();
            }
        })
    };
    for (b, step) in [(&prev, -1), (&next, 1)] {
        let r = redraw.clone();
        b.connect_clicked(move |_| {
            MONTH.with(|m| m.set(Some(add_months(month(), step))));
            r();
        });
    }
    {
        let (w, r) = (Rc::downgrade(app), redraw.clone());
        close.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                crate::close::open(&a, None, r.clone());
            }
        });
    }
    {
        let (w, r) = (Rc::downgrade(app), redraw.clone());
        setup.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                wizard(&a, r.clone());
            }
        });
    }

    // what Fill does for the month on screen, set by each refresh
    let on_fill: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    {
        let on_fill = on_fill.clone();
        fill.connect_clicked(move |_| {
            let f = on_fill.borrow().clone(); // the redraw it triggers replaces on_fill
            if let Some(f) = f {
                f();
            }
        });
    }
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let redraw = redraw.clone();
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let s = snap(&a);
            let (ccy, m) = (s.ccy, month());
            let end = add_months(m, 1).pred_opt().unwrap_or(m);
            let (usual, subs_m, one_off) = usual(&s, m);
            let now: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, ccy, &s.fx, &s.parents).into_iter().collect();
            let env = envs(&a, &s, m);
            // the budgets that applied in this month, and the categories that roll over
            let bm: BTreeMap<i64, Decimal> = env.iter().filter(|(_, e)| !e.budget.is_zero()).map(|(k, e)| (*k, e.budget)).collect();
            let rollovers = a.db.rollovers(&s.scope);
            let in_budget = |id: i64| bm.contains_key(&id) || rollovers.contains_key(&id);
            let set_aside = funds_monthly(&s);
            let through = data_through(&a);
            let last: BTreeMap<String, Decimal> = by_category(&s.rows, add_months(m, -1), m.pred_opt().unwrap_or(m), ccy, &s.fx, &s.parents).into_iter().collect();
            let money = |d: Decimal| fmt_money(d, ccy);

            title.set_label(&m.format("%B %Y").to_string());
            next.set_sensitive(m < month_start(today()));
            setup.set_label(if s.plan.is_some() { "Edit Plan…" } else { "Set Up Budget…" });
            setup.set_css_classes(if s.plan.is_none() { &["suggested-action"] } else { &[] });
            {
                // fill: categories without a budget get their usual amount
                let (w, sc, redraw, usual, cats) = (Rc::downgrade(&a), s.scope.clone(), redraw.clone(), usual.clone(), s.cats.clone());
                let has: Vec<i64> = s.budgets.keys().copied().collect();
                fill.set_sensitive(cats.iter().any(|c| !has.contains(&c.id) && usual.contains_key(&c.name)));
                *on_fill.borrow_mut() = Some(Rc::new(move || {
                    let Some(a) = w.upgrade() else { return };
                    let mut n = 0;
                    for c in cats.iter().filter(|c| !has.contains(&c.id)) {
                        if let Some(v) = usual.get(&c.name) {
                            a.saved(a.db.set_budget(&sc, c.id, Some((*v, ccy))));
                            n += 1;
                        }
                    }
                    a.toast(&format!("Budgeted {n} categories from your last {MONTHS} months"));
                    redraw();
                }));
            }

            // the month so far, and where it's heading at this pace
            let total_b: Decimal = bm.values().copied().sum();
            let total_s: Decimal = now.values().copied().sum();
            let in_b: Decimal = s.cats.iter().filter(|c| in_budget(c.id)).filter_map(|c| now.get(&c.name)).copied().sum();
            let funded: Decimal = s.cats.iter().filter(|c| in_budget(c.id)).filter_map(|c| env.get(&c.id)).map(Envelope::funded).sum();
            let days = Decimal::from(end.day());
            let share = elapsed(m, through);
            budgeted.set_label(&money(total_b));
            budgeted_s.set_label(&if funded != total_b { format!("{} categories · {} with rollovers and moves", bm.len(), money(funded)) } else { format!("{} categories", bm.len()) });
            spent.set_label(&money(total_s));
            spent_s.set_label(&format!("{} in budgeted categories · {} outside", money(in_b), money(total_s - in_b)));
            left.set_label(&money(funded - in_b));
            left_s.set_label(if funded.is_zero() { "Set a budget below, or fill it from your spending" } else if in_b > funded { "Over budget" } else { "Left in budgeted categories" });
            left.set_css_classes(&["value", "tnum", if in_b > funded { "amount-out" } else { "amount-in" }]);
            let proj = if share.is_zero() { total_s } else { (total_s / share).round_dp(2) };
            pace.set_label(&money(proj));
            pace_s.set_label(&if share.is_zero() {
                "No statement for this month yet — they arrive after the month ends".to_string()
            } else if share < Decimal::ONE {
                format!("At this pace: your statements run to {} (day {} of {}); a big bill early in the month overstates it", through.format("%d %b"), through.day(), days)
            } else {
                "The whole month".into()
            });

            // each category's last six months, for its trend line
            let hist: BTreeMap<String, Vec<Decimal>> = s.cats.iter().map(|c| (c.name.clone(), core::history(&s.rows, &c.name, month_start(today()), MONTHS, ccy, &s.fx, &s.parents))).collect();

            // ── the plan
            clear(&plan_box);
            if let Some(p) = &s.plan {
                plan_box.append(&info_heading(
                    "Your Plan",
                    "Your month on paper. Take-home pay is at the top, and everything that comes out of it is below: the spending budget, the share set aside for non-monthly bills, and loan payments. The last line is what's left to save. Green means you meet your savings goal, red means you're short.",
                ));
                let g = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                let line = |t: &str, sub: String, v: String, class: &str| {
                    let r = adw::ActionRow::builder().title(t).subtitle(glib::markup_escape_text(&sub)).build();
                    r.add_suffix(&gtk::Label::builder().label(v).css_classes(["tnum", "heading", class]).build());
                    g.append(&r);
                };
                let pays = p.paydays(today(), 3).iter().map(|d| d.format("%a %d %b").to_string()).collect::<Vec<_>>().join(", ");
                line(
                    "Take-home pay",
                    format!(
                        "{} a payday ({}; gross {} a payday, {} a month), {} × {} pays a year ÷ 12{}{}",
                        money(p.net_pay()),
                        p.cycle.label().to_lowercase(),
                        money(p.gross),
                        money((p.gross * p.cycle.per_year() / Decimal::from(12)).round_dp(2)),
                        money(p.net_pay()),
                        p.cycle.per_year(),
                        if p.other_income.is_zero() { String::new() } else { format!(" + {} other income", money(p.other_income)) },
                        if pays.is_empty() { String::new() } else { format!(" · next: {pays}") }
                    ),
                    money(p.monthly_income()),
                    "amount-in",
                );
                line("Spending budget", format!("{} categories", bm.len()), format!("− {}", money(total_b)), "");
                if !set_aside.is_zero() {
                    line("Non-monthly bills", format!("Set aside each month for {} yearly and quarterly bills", s.funds.len()), format!("− {}", money(set_aside)), "");
                }
                if !p.loan_payments.is_zero() {
                    line("Loan payments", "Instalments, which are transfers and not in the spending budget".into(), format!("− {}", money(p.loan_payments)), "");
                }
                let save = p.monthly_savings(total_b) - set_aside;
                let pct = if p.monthly_income().is_zero() { Decimal::ZERO } else { (save / p.monthly_income() * Decimal::ONE_HUNDRED).round_dp(0) };
                let goal = if p.savings_goal.is_zero() {
                    format!("{pct}% of take-home")
                } else if save >= p.savings_goal {
                    format!("{pct}% of take-home · goal {} met, {} to spare", money(p.savings_goal), money(save - p.savings_goal))
                } else {
                    format!("{pct}% of take-home · goal {} — short by {}", money(p.savings_goal), money(p.savings_goal - save))
                };
                line("Left to save each month", goal, money(save), if save < p.savings_goal || save.is_sign_negative() { "amount-out" } else { "amount-in" });
                plan_box.append(&g);

                // twelve months ahead: following the budget, and if spending keeps its trend
                let start = add_months(month_start(today()), 1);
                let mut labels = Vec::new();
                let (mut by_budget, mut by_trend, mut by_goal) = (Vec::new(), Vec::new(), Vec::new());
                let (mut cb, mut ct) = (Decimal::ZERO, Decimal::ZERO);
                for k in 0..12 {
                    let mm = add_months(start, k as i32);
                    labels.push((k as f64, mm.format("%b").to_string()));
                    cb += save;
                    // a budgeted category follows its trend; others too, so nothing is missed
                    let spend: Decimal = hist.values().map(|v| core::forecast(v, k + 1)).sum();
                    ct += p.monthly_income() - p.loan_payments - set_aside - spend;
                    by_budget.push((k as f64, f(cb)));
                    by_trend.push((k as f64, f(ct)));
                    by_goal.push((k as f64, f(p.savings_goal * Decimal::from(k + 1))));
                }
                let mut series = vec![
                    LineSeries { name: "Following your budget".into(), points: by_budget.clone(), color: charts::GREEN, dashed: false },
                    LineSeries { name: "If spending keeps its trend".into(), points: by_trend.clone(), color: charts::ORANGE, dashed: false },
                ];
                if !p.savings_goal.is_zero() {
                    series.push(LineSeries { name: "Goal".into(), points: by_goal, color: charts::SLATE, dashed: true });
                }
                let sym = ccy.symbol();
                *projection.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
                projection.set(Data::Lines {
                    x_labels: labels.clone(),
                    band: None,
                    series,
                    stops: labels
                        .iter()
                        .zip(by_budget.iter().zip(&by_trend))
                        .map(|((x, l), (b, t))| (*x, format!("{l}: saved {} on budget · {} on trend", money(Decimal::try_from(b.1).unwrap_or_default().round_dp(0)), money(Decimal::try_from(t.1).unwrap_or_default().round_dp(0)))))
                        .collect(),
                });
                let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
                card.append(&info_heading(
                    "Savings Over the Next 12 Months",
                    "How much you'd have saved in total by each month ahead. Green follows your budget exactly. Orange assumes your spending keeps changing the way it has lately. A widening gap between them means your spending is drifting from the plan. The dashed line is your savings goal; stay on or above it to be on track.",
                ));
                card.append(&dim(&format!(
                    "Green: take-home − budget − non-monthly bills − loans, every month ({} a month). Orange: each category's last six months drawn as a straight line (least squares) and carried forward — what happens if spending keeps drifting the way it has.",
                    money(save)
                )));
                if let Some(parent) = projection.widget.parent() {
                    if let Ok(b) = parent.downcast::<gtk::Box>() {
                        b.remove(&projection.widget);
                    }
                }
                card.append(&projection.widget);
                plan_box.append(&card);
            }

            // ── fixed vs variable
            clear(&split_box);
            {
                // a category's monthly amount: its budget, else what it usually costs
                let amount = |c: &Category| bm.get(&c.id).copied().or_else(|| usual.get(&c.name).copied()).unwrap_or_default();
                let group = |fixed: bool| -> Vec<&Category> {
                    let mut v: Vec<&Category> = s.cats.iter().filter(|c| s.is_fixed(&c.name) == fixed && !amount(c).is_zero()).collect();
                    v.sort_by_key(|c| std::cmp::Reverse(amount(c)));
                    v
                };
                let (fx_cats, var_cats) = (group(true), group(false));
                let total = |v: &[&Category]| v.iter().map(|c| amount(c)).sum::<Decimal>();
                let spent_in = |v: &[&Category]| v.iter().filter_map(|c| now.get(&c.name)).copied().sum::<Decimal>();
                let (fixed_t, var_t) = (total(&fx_cats), total(&var_cats));
                let income = s.plan.as_ref().map(|p| p.monthly_income()).filter(|i| !i.is_zero());
                let loans = s.plan.as_ref().map(|p| p.loan_payments).unwrap_or_default();
                let save = income.map(|i| i - fixed_t - var_t - set_aside - loans);

                split_box.append(&info_heading(
                    "Fixed vs Variable",
                    "The bar splits a month's money by colour: fixed expenses (bills that don't change much, like rent, insurance or phone), variable spending (things you choose, like groceries or eating out), non-monthly bills, loans, and what's saved. The variable part is where you have room to cut back. Switch a category with its Fixed/Variable pill under Categories.",
                ));
                split_box.append(&dim(match income {
                    Some(_) => "How your take-home divides each month: fixed expenses you can't easily change, variable spending you can, loan payments, and what's left to save. Amounts are budgets, or the usual cost where a category has no budget.",
                    None => "Fixed expenses you can't easily change against variable spending you can. Amounts are budgets, or the usual cost where a category has no budget. Set up your pay (Set Up Budget…) to see them against your take-home.",
                }));
                // one bar, split in proportion
                let mut parts: Vec<(&str, Decimal, charts::Rgb)> = vec![("Fixed", fixed_t, charts::BLUE), ("Variable", var_t, charts::ORANGE)];
                if !set_aside.is_zero() {
                    parts.push(("Non-monthly", set_aside, charts::PURPLE));
                }
                if !loans.is_zero() {
                    parts.push(("Loans", loans, charts::SLATE));
                }
                if let Some(sv) = save.filter(|v| v.is_sign_positive()) {
                    parts.push(("Saved", sv, charts::GREEN));
                }
                let whole: Decimal = parts.iter().map(|p| p.1).sum();
                let bar = gtk::DrawingArea::builder().content_height(22).hexpand(true).build();
                {
                    let parts: Vec<(f64, charts::Rgb)> = parts.iter().map(|p| (if whole.is_zero() { 0.0 } else { f(p.1 / whole) }, p.2)).collect();
                    bar.set_draw_func(move |_, cr, w, h| {
                        let (w, h) = (f64::from(w), f64::from(h));
                        let r = 6.0;
                        // rounded clip, then the segments side by side
                        cr.new_sub_path();
                        cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, 0.0);
                        cr.arc(w - r, h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
                        cr.arc(r, h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
                        cr.arc(r, r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
                        cr.close_path();
                        cr.clip();
                        let mut x = 0.0;
                        for (share, (rr, g, b)) in &parts {
                            cr.set_source_rgb(*rr, *g, *b);
                            cr.rectangle(x, 0.0, w * share + 0.5, h);
                            let _ = cr.fill();
                            x += w * share;
                        }
                    });
                }
                split_box.append(&bar);
                let legend = gtk::Box::builder().spacing(18).build();
                for (name, v, (rr, g, b)) in &parts {
                    let item = gtk::Box::builder().spacing(6).build();
                    let sw = gtk::DrawingArea::builder().content_width(10).content_height(10).valign(gtk::Align::Center).build();
                    let (rr, g, b) = (*rr, *g, *b);
                    sw.set_draw_func(move |_, cr, _, _| {
                        cr.set_source_rgb(rr, g, b);
                        cr.arc(5.0, 5.0, 5.0, 0.0, std::f64::consts::TAU);
                        let _ = cr.fill();
                    });
                    item.append(&sw);
                    let pct = if whole.is_zero() { Decimal::ZERO } else { (*v / whole * Decimal::ONE_HUNDRED).round_dp(0) };
                    item.append(&gtk::Label::builder().label(format!("{name} {} · {pct}%", money(*v))).css_classes(["caption", "tnum"]).build());
                    legend.append(&item);
                }
                split_box.append(&legend);

                // a card each, with its categories
                let cards = gtk::Box::builder().spacing(12).homogeneous(true).build();
                for (title, cats, t, class) in [("Fixed Expenses", &fx_cats, fixed_t, "amount-fixed"), ("Variable Expenses", &var_cats, var_t, "amount-variable")] {
                    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "kpi"]).build();
                    card.append(&info_caption(
                        title,
                        "This group's monthly total, using each category's budget or its usual cost if it has no budget (marked usual). Below: what's been spent in it this month, and its share of your take-home pay.",
                    ));
                    card.append(&gtk::Label::builder().label(format!("{} a month", money(t))).xalign(0.0).css_classes(["value", "tnum", class]).build());
                    let sp = spent_in(cats);
                    let of = match income {
                        Some(i) => format!(" · {}% of take-home", (t / i * Decimal::ONE_HUNDRED).round_dp(0)),
                        None => String::new(),
                    };
                    card.append(&dim(&format!("{} spent in {}{of}", money(sp), m.format("%B"))));
                    let grid = gtk::Grid::builder().column_spacing(12).row_spacing(2).margin_top(6).build();
                    for (i, c) in cats.iter().enumerate() {
                        let budgeted = bm.contains_key(&c.id);
                        grid.attach(&gtk::Label::builder().label(&c.name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["caption"]).build(), 0, i as i32, 1, 1);
                        let l = gtk::Label::builder()
                            .label(format!("{}{}", money(amount(c)), if budgeted { "" } else { " usual" }))
                            .xalign(1.0)
                            .css_classes(["caption", "tnum", if budgeted { "" } else { "dim-label" }])
                            .build();
                        grid.attach(&l, 1, i as i32, 1, 1);
                    }
                    if cats.is_empty() {
                        card.append(&dim("None yet — mark categories below"));
                    }
                    card.append(&grid);
                    cards.append(&card);
                }
                split_box.append(&cards);
                rules_of_thumb(&split_box, fixed_t + set_aside + loans, var_t, income, ccy);
            }

            // ── yearly and quarterly bills, a share set aside each month
            clear(&funds_box);
            {
                funds_box.append(&info_heading(
                    "Non-Monthly Bills",
                    "Each bar shows how much has been set aside so far towards that bill. A full bar means it's covered by its due date. Suggestions under the list are yearly or quarterly charges FinGraph spotted in your statements; add one to start setting money aside for it.",
                ));
                funds_box.append(&dim(
                    "Bills that come once a quarter or once a year — insurance, licences, registrations, annual plans. Each month sets a share aside so the bill is covered when it comes; that comes out of what you save in Your Plan.",
                ));
                let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                let ideas: Vec<core::FundIdea> = core::fund_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
                for fd in &s.funds {
                    let due = fd.due(today());
                    let aside = s.fx.convert(fd.set_aside(today()), fd.currency, ccy);
                    let whole = s.fx.convert(fd.amount, fd.currency, ccy);
                    let r = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&fd.name))
                        .subtitle(format!(
                            "{} {} · due {} · {} a month",
                            fmt_money(fd.amount, fd.currency),
                            match fd.every_months {
                                12 => "once a year".to_string(),
                                n => format!("every {n} months"),
                            },
                            due.format("%d %b %Y"),
                            fmt_money(fd.monthly(), fd.currency)
                        ))
                        .build();
                    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(220).build();
                    col.append(&gtk::Label::builder().label(format!("{} of {} set aside", money(aside), money(whole))).xalign(1.0).css_classes(["tnum", "caption"]).build());
                    let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(if whole.is_zero() { 0.0 } else { f(aside / whole).min(1.0) }).build();
                    col.append(&bar);
                    r.add_suffix(&col);
                    let edit = gtk::Button::builder().icon_name("document-edit-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Change").build();
                    let rm = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Remove").build();
                    {
                        let (w, sc, fd, redraw) = (Rc::downgrade(&a), s.scope.clone(), fd.clone(), redraw.clone());
                        edit.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                fund_dialog(&a, &sc, Some(fd.clone()), ccy, redraw.clone());
                            }
                        });
                    }
                    {
                        let (w, id, redraw) = (Rc::downgrade(&a), fd.id, redraw.clone());
                        rm.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                a.saved(a.db.delete_fund(id));
                                redraw();
                            }
                        });
                    }
                    r.add_suffix(&edit);
                    r.add_suffix(&rm);
                    lb.append(&r);
                }
                for i in &ideas {
                    let r = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&i.payee))
                        .subtitle(format!("Seen in your statements: {} {} · next around {}", money(i.amount), if i.every_months == 12 { "once a year" } else { "every 3 months" }, i.next_due.format("%d %b %Y")))
                        .build();
                    r.add_prefix(&gtk::Image::from_icon_name("starred-symbolic"));
                    let add = gtk::Button::builder().label("Set Aside For It").valign(gtk::Align::Center).css_classes(["flat"]).build();
                    let (w, sc, i, redraw) = (Rc::downgrade(&a), s.scope.clone(), i.clone(), redraw.clone());
                    add.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            let fd = Fund { id: 0, name: i.payee.clone(), amount: i.amount.round_dp(0), currency: ccy, every_months: i.every_months, next_due: i.next_due, stem: i.stem.clone() };
                            a.saved(a.db.save_fund(&sc, &fd));
                            redraw();
                        }
                    });
                    r.add_suffix(&add);
                    lb.append(&r);
                }
                let add = adw::ButtonRow::builder().title("Add a Bill…").start_icon_name("list-add-symbolic").build();
                {
                    let (w, sc, redraw) = (Rc::downgrade(&a), s.scope.clone(), redraw.clone());
                    add.connect_activated(move |_| {
                        if let Some(a) = w.upgrade() {
                            fund_dialog(&a, &sc, None, ccy, redraw.clone());
                        }
                    });
                }
                lb.append(&add);
                funds_box.append(&lb);
                if !s.funds.is_empty() {
                    let total: Decimal = s.funds.iter().map(|fd| s.fx.convert(fd.set_aside(today()), fd.currency, ccy)).sum();
                    funds_box.append(&dim(&format!("{} a month in all; {} should be set aside by now.", money(set_aside), money(total))));
                }
            }

            // ── one row per category that has a budget, spending or a usual amount
            list.remove_all();
            let mut shown: Vec<_> = s.cats.iter().filter(|c| in_budget(c.id) || now.contains_key(&c.name) || usual.contains_key(&c.name)).collect();
            shown.sort_by_key(|c| std::cmp::Reverse(now.get(&c.name).copied().unwrap_or_default().max(bm.get(&c.id).copied().unwrap_or_default())));
            let names: Rc<Vec<(i64, String, Decimal)>> = Rc::new(s.cats.iter().map(|c| (c.id, c.name.clone(), env.get(&c.id).map(Envelope::available).unwrap_or_default())).collect());
            let use_col = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            let kind_col = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            for c in shown {
                let sp = now.get(&c.name).copied().unwrap_or_default();
                let b = bm.get(&c.id).copied();
                let e = env.get(&c.id).copied().unwrap_or_default();
                let rolls = rollovers.contains_key(&c.id);
                // what it had to spend: the budget, plus what rolled in and money moved
                let fb = (b.is_some() || rolls).then(|| e.funded());
                let u = usual.get(&c.name).copied();
                let mut sub = vec![match u {
                    Some(u) => {
                        let t = usual_text(&c.name, u, subs_m, one_off, ccy);
                        format!("U{}", &t[1..])
                    }
                    None => "Nothing in the last six months".into(),
                }];
                if let Some(l) = last.get(&c.name) {
                    sub.push(format!("last month {}", money(*l)));
                }
                if let (Some(h), Some(u)) = (hist.get(&c.name), u) {
                    let slope = core::trend(h).1.round_dp(0);
                    if slope.abs() >= (u / Decimal::from(20)).max(Decimal::from(5)) {
                        sub.push(format!("trending {} {} a month", if slope.is_sign_positive() { "up" } else { "down" }, money(slope.abs())));
                    }
                }
                if !e.carried.is_zero() {
                    sub.push(if e.carried.is_sign_positive() { format!("{} rolled over", money(e.carried)) } else { format!("{} overspent last month", money(-e.carried)) });
                }
                if !e.moved.is_zero() {
                    sub.push(format!("{} {}", if e.moved.is_sign_positive() { "moved in" } else { "moved out" }, money(e.moved.abs())));
                }
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).subtitle(sub.join(" · ")).build();
                let is_fixed = s.is_fixed(&c.name);
                let kind = gtk::Button::builder()
                    .label(if is_fixed { "Fixed" } else { "Variable" })
                    .valign(gtk::Align::Center)
                    .css_classes(["kind-pill", if is_fixed { "kind-fixed" } else { "kind-variable" }])
                    .tooltip_text(if is_fixed { "A fixed expense — click to count it as variable" } else { "Variable spending — click to count it as a fixed expense" })
                    .build();
                kind_col.add_widget(&kind);
                {
                    let (w, name, redraw) = (Rc::downgrade(&a), c.name.clone(), redraw.clone());
                    kind.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_fixed(&name, !is_fixed));
                            redraw();
                        }
                    });
                }
                row.add_prefix(&kind);

                let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(260).build();
                let status = gtk::Label::builder().xalign(1.0).css_classes(["tnum", "caption"]).build();
                match fb {
                    Some(b) => match core::alert(sp, b, elapsed(m, through)) {
                        Some(Alert::Over) => {
                            status.set_label(&format!("{} of {} · over by {}", money(sp), money(b), money(sp - b)));
                            status.add_css_class("amount-out");
                        }
                        Some(Alert::Near) => {
                            status.set_label(&format!("{} of {} · {}% used, {} left", money(sp), money(b), (sp / b * Decimal::ONE_HUNDRED).round_dp(0), money(b - sp)));
                            status.add_css_class("warning");
                        }
                        None => status.set_label(&format!("{} of {} · {} left", money(sp), money(b), money(b - sp))),
                    },
                    None => {
                        status.set_label(&format!("{} spent · no budget", money(sp)));
                        status.add_css_class("dim-label");
                    }
                }
                let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).build();
                bar.remove_offset_value(Some("low"));
                bar.remove_offset_value(Some("high"));
                bar.remove_offset_value(Some("full"));
                bar.add_offset_value("ok", 0.85);
                bar.add_offset_value("near", 1.0);
                if let Some(b) = fb.filter(|b| *b > Decimal::ZERO) {
                    let r = f(sp / b);
                    bar.set_value(r.min(1.0));
                    if r > 1.0 {
                        bar.add_css_class("over");
                    }
                } else {
                    bar.set_visible(false);
                }
                col.append(&status);
                col.append(&bar);
                row.add_suffix(&col);

                let entry = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(8).max_width_chars(8).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).tooltip_text("Monthly budget — Enter to save").build();
                if let Some(b) = b {
                    entry.set_text(&b.round_dp(2).normalize().to_string());
                }
                {
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone());
                    entry.connect_activate(move |e| {
                        let Some(a) = w.upgrade() else { return };
                        let t = e.text();
                        let v = if t.trim().is_empty() { None } else { parse_user(&t) };
                        if v.is_none() && !t.trim().is_empty() {
                            return a.toast("Type an amount, like 600 or 1,250.00");
                        }
                        a.saved(a.db.set_budget(&sc, id, v.map(|v| (v, ccy))));
                        redraw();
                    });
                }
                row.add_suffix(&entry);
                if let Some(u) = u.filter(|u| Some(*u) != b) {
                    let use_ = gtk::Button::builder().label(format!("Use {}", money(u))).valign(gtk::Align::Center).css_classes(["flat"]).build();
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone());
                    use_.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_budget(&sc, id, Some((u, ccy))));
                            redraw();
                        }
                    });
                    use_col.add_widget(&use_);
                    row.add_suffix(&use_);
                } else {
                    // keep the entries lined up
                    let pad = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    use_col.add_widget(&pad);
                    row.add_suffix(&pad);
                }
                // ⋯: roll over, move money
                let menu = gtk::MenuButton::builder().icon_name("view-more-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Rollover and moving money").build();
                let pop = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
                let roll = gtk::CheckButton::builder().label("Roll leftovers into next month").active(rolls).tooltip_text("What's left (or overspent) carries into the next month's budget").build();
                pop.append(&roll);
                let move_in = gtk::Button::builder().label(if e.available().is_sign_negative() { "Cover Overspending…" } else { "Move Money In…" }).css_classes(["flat"]).build();
                let move_out = gtk::Button::builder().label("Move Money Out…").css_classes(["flat"]).build();
                pop.append(&move_in);
                pop.append(&move_out);
                let mine: Vec<core::Move> = a.db.moves(&s.scope, m).unwrap_or_default().into_iter().filter(|mv| mv.from == c.id || mv.to == c.id).collect();
                if !mine.is_empty() {
                    let undo = gtk::Button::builder().label(format!("Undo {} Move{} This Month", mine.len(), if mine.len() == 1 { "" } else { "s" })).css_classes(["flat"]).build();
                    let (w, redraw) = (Rc::downgrade(&a), redraw.clone());
                    undo.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            for mv in &mine {
                                a.saved(a.db.delete_move(mv.id));
                            }
                            redraw();
                        }
                    });
                    pop.append(&undo);
                }
                let popover = gtk::Popover::builder().child(&pop).build();
                menu.set_popover(Some(&popover));
                {
                    let (w, sc, id, redraw, popover) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone(), popover.clone());
                    roll.connect_toggled(move |cb| {
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_rollover(&sc, id, cb.is_active().then_some(m)));
                            popover.popdown();
                            redraw();
                        }
                    });
                }
                for (btn, into) in [(&move_in, true), (&move_out, false)] {
                    let (w, sc, id, redraw, names, popover) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone(), names.clone(), popover.clone());
                    btn.connect_clicked(move |_| {
                        popover.popdown();
                        if let Some(a) = w.upgrade() {
                            move_dialog(&a, &sc, m, id, into, &names, ccy, redraw.clone());
                        }
                    });
                }
                row.add_suffix(&menu);
                list.append(&row);
            }
            // ── watchlists
            clear(&watch_box);
            {
                watch_box.append(&info_heading(
                    "Watchlists",
                    "Each row shows this month's spending at a merchant (or anything matching the wording you chose) against the limit you set, whatever category it's in. The bar turns amber near the limit and red when over.",
                ));
                watch_box.append(&dim("A monthly limit on a merchant or any wording on your statements (Amazon, Foster's, DoorDash) — it counts across categories, and going over shows up with the budget alerts."));
                let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                let watches = a.db.watchlists(&s.scope);
                for (i, wl) in watches.iter().enumerate() {
                    let limit = s.fx.convert(wl.limit, wl.currency, ccy);
                    let (sp, ids) = core::watched(&s.rows, wl, m, end, ccy, &s.fx);
                    let r = adw::ActionRow::builder().title(glib::markup_escape_text(&wl.name)).subtitle(glib::markup_escape_text(&format!("Matches “{}” · {} charge{} in {}", wl.pattern, ids.len(), if ids.len() == 1 { "" } else { "s" }, m.format("%B")))).build();
                    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(220).build();
                    let st = gtk::Label::builder().label(format!("{} of {}", money(sp), money(limit))).xalign(1.0).css_classes(["tnum", "caption"]).build();
                    match core::alert(sp, limit, elapsed(m, through)) {
                        Some(Alert::Over) => st.add_css_class("amount-out"),
                        Some(Alert::Near) => st.add_css_class("warning"),
                        None => {}
                    }
                    col.append(&st);
                    let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(if limit.is_zero() { 0.0 } else { f(sp / limit).min(1.0) }).build();
                    if sp > limit {
                        bar.add_css_class("over");
                    }
                    col.append(&bar);
                    r.add_suffix(&col);
                    if !ids.is_empty() {
                        let show = gtk::Button::builder().icon_name("view-list-bullet-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Show these transactions").build();
                        let w = Rc::downgrade(&a);
                        show.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                a.show_transactions(ids.clone());
                            }
                        });
                        r.add_suffix(&show);
                    }
                    let rm = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Stop watching").build();
                    let (w, sc, watches, redraw) = (Rc::downgrade(&a), s.scope.clone(), watches.clone(), redraw.clone());
                    rm.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            let mut v = watches.clone();
                            v.remove(i);
                            a.saved(a.db.save_watchlists(&sc, &v));
                            redraw();
                        }
                    });
                    r.add_suffix(&rm);
                    lb.append(&r);
                }
                // add one: what to match, and the limit
                let add = gtk::Box::builder().spacing(6).margin_top(6).margin_bottom(6).margin_start(12).margin_end(12).build();
                let pat = gtk::Entry::builder().placeholder_text("Merchant or wording, e.g. Amazon").hexpand(true).build();
                let lim = gtk::Entry::builder().placeholder_text(format!("{} a month", ccy.symbol())).width_chars(10).input_purpose(gtk::InputPurpose::Number).build();
                let go = gtk::Button::builder().label("Watch").build();
                add.append(&pat);
                add.append(&lim);
                add.append(&go);
                let add_row = gtk::ListBoxRow::builder().child(&add).activatable(false).build();
                lb.append(&add_row);
                {
                    let (w, sc, watches, redraw, pat, lim2) = (Rc::downgrade(&a), s.scope.clone(), watches.clone(), redraw.clone(), pat.clone(), lim.clone());
                    let save = move || {
                        let lim = &lim2;
                        let Some(a) = w.upgrade() else { return };
                        let p = pat.text().trim().to_string();
                        let Some(l) = parse_user(&lim.text()).map(|v| v.abs()).filter(|v| !v.is_zero()) else { return a.toast("Type a monthly limit, like 150") };
                        if p.is_empty() {
                            return a.toast("Type the merchant or wording to watch");
                        }
                        let mut v = watches.clone();
                        v.push(core::Watch { name: fingraph_core::rules::title_case(&p), pattern: p, limit: l, currency: ccy });
                        a.saved(a.db.save_watchlists(&sc, &v));
                        redraw();
                    };
                    let save = Rc::new(save);
                    let s2 = save.clone();
                    go.connect_clicked(move |_| s2());
                    lim.connect_activate(move |_| save());
                }
                watch_box.append(&lb);
            }

            // ── where the money went
            {
                flow_title.set_label(&format!("Where the Money Went · {}", m.format("%B %Y")));
                let mut income: BTreeMap<String, Decimal> = BTreeMap::new();
                for t in s.rows.iter().filter(|t| t.posted >= m && t.posted <= end && !t.is_transfer()) {
                    let v = fingraph_core::insights::value(t, ccy, &s.fx);
                    if v.is_sign_positive() {
                        let who = if t.clean_payee.is_empty() { fingraph_core::rules::title_case(&fingraph_core::rules::merchant_stem(&t.raw_description, &t.memo)) } else { t.clean_payee.clone() };
                        *income.entry(who).or_default() += v;
                    }
                }
                // the biggest few, the rest together
                let top = |mut v: Vec<(String, Decimal)>, n: usize| -> Vec<(String, Decimal)> {
                    v.sort_by_key(|x| std::cmp::Reverse(x.1));
                    if v.len() > n {
                        let rest: Decimal = v[n - 1..].iter().map(|x| x.1).sum();
                        v.truncate(n - 1);
                        v.push(("Everything else".into(), rest));
                    }
                    v
                };
                let ins = top(income.into_iter().collect(), 5);
                let outs = top(now.iter().map(|(k, v)| (k.clone(), *v)).collect(), 9);
                let (ti, to): (Decimal, Decimal) = (ins.iter().map(|x| x.1).sum(), outs.iter().map(|x| x.1).sum());
                let mut left: Vec<(String, f64, charts::Rgb)> = ins.iter().map(|(n, v)| (format!("{n}  {}", money(*v)), f(*v), charts::GREEN)).collect();
                let mut right: Vec<(String, f64, charts::Rgb)> = outs.iter().enumerate().map(|(i, (n, v))| (format!("{n}  {}", money(*v)), f(*v), charts::PALETTE[(i + 1) % charts::PALETTE.len()])).collect();
                let mut tips: Vec<String> = ins.iter().map(|(n, v)| format!("{n}: {} in", money(*v))).collect();
                if to > ti {
                    left.push((format!("From savings  {}", money(to - ti)), f(to - ti), charts::SLATE));
                    tips.push(format!("Spent {} more than came in", money(to - ti)));
                }
                tips.extend(outs.iter().map(|(n, v)| format!("{n}: {} ({}% of spending)", money(*v), if to.is_zero() { Decimal::ZERO } else { (*v / to * Decimal::ONE_HUNDRED).round_dp(0) })));
                if ti > to {
                    right.push((format!("Saved  {}", money(ti - to)), f(ti - to), charts::GREEN));
                    tips.push(format!("Kept {} ({}% of what came in)", money(ti - to), ((ti - to) / ti * Decimal::ONE_HUNDRED).round_dp(0)));
                }
                flow.set(Data::Flow { left, right, tips });
            }

            if let Some(u) = now.get("Uncategorized") {
                let row = adw::ActionRow::builder().title("Uncategorized").subtitle(format!("{} this month can't count against a budget until it has a category — To Review sorts it", money(*u))).activatable(true).build();
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let w = Rc::downgrade(&a);
                row.connect_activated(move |_| {
                    if let Some(a) = w.upgrade() {
                        a.show_view("review");
                    }
                });
                list.append(&row);
            }
        })
    };
    *me.borrow_mut() = Some(refresh.clone());
    (page.upcast(), refresh)
}

/// Move money between two categories in month `m`. `into`: this category receives (from the one
/// with the most left), else it gives. An overspent category is offered exactly its overspend.
#[allow(clippy::too_many_arguments)]
pub(crate) fn move_dialog(app: &Rc<App>, sc: &str, m: NaiveDate, this: i64, into: bool, names: &[(i64, String, Decimal)], ccy: Currency, done: crate::Refresh) {
    let others: Vec<&(i64, String, Decimal)> = names.iter().filter(|n| n.0 != this).collect();
    let Some(me) = names.iter().find(|n| n.0 == this) else { return };
    if others.is_empty() {
        return;
    }
    // the most left over goes first
    let mut others = others;
    others.sort_by_key(|n| std::cmp::Reverse(n.2));
    let labels: Vec<String> = others.iter().map(|n| format!("{} ({} left)", n.1, fmt_money(n.2, ccy))).collect();
    let pick = gtk::DropDown::builder().model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let amount = gtk::Entry::builder().placeholder_text(ccy.symbol()).input_purpose(gtk::InputPurpose::Number).activates_default(true).build();
    let suggest = if into && me.2.is_sign_negative() { -me.2 } else if !into && me.2.is_sign_positive() { me.2 } else { Decimal::ZERO };
    if !suggest.is_zero() {
        amount.set_text(&amount_text(suggest));
    }
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    body.append(&gtk::Label::builder().label(if into { "From" } else { "To" }).xalign(0.0).css_classes(["caption-heading"]).build());
    body.append(&pick);
    body.append(&gtk::Label::builder().label("Amount").xalign(0.0).css_classes(["caption-heading"]).build());
    body.append(&amount);
    let dlg = adw::AlertDialog::builder()
        .heading(if into { format!("Move Money Into {}", me.1) } else { format!("Move Money Out of {}", me.1) })
        .body(format!("For {} only. Both categories' budgets stay as they are; Undo is on the ⋯ menu.", m.format("%B %Y")))
        .extra_child(&body)
        .build();
    dlg.add_responses(&[("cancel", "Cancel"), ("move", "Move")]);
    dlg.set_response_appearance("move", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("move"));
    let (w, sc, ids) = (Rc::downgrade(app), sc.to_string(), others.iter().map(|n| n.0).collect::<Vec<_>>());
    dlg.connect_response(Some("move"), move |_, _| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        let Some(v) = parse_user(&amount.text()).map(|v| v.abs()).filter(|v| !v.is_zero()) else { return a.toast("Type an amount to move") };
        let other = ids[(pick.selected() as usize).min(ids.len() - 1)];
        let (from, to) = if into { (other, this) } else { (this, other) };
        a.saved(a.db.add_move(&sc, m, from, to, v, ccy));
        done();
    }));
    dlg.present(Some(&app.win));
}

/// Add (`f` = None) or change a yearly / quarterly bill.
fn fund_dialog(app: &Rc<App>, sc: &str, f: Option<Fund>, ccy: Currency, done: crate::Refresh) {
    let f = f.unwrap_or(Fund { id: 0, name: String::new(), amount: Decimal::ZERO, currency: ccy, every_months: 12, next_due: add_months(month_start(today()), 6), stem: String::new() });
    let g = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder().title("Bill (e.g. Car insurance)").text(&f.name).build();
    let amount = adw::EntryRow::builder().title(format!("Amount each time ({})", f.currency.symbol())).text(amount_text(f.amount)).build();
    let every = adw::ComboRow::builder().title("Comes").model(&gtk::StringList::new(&EVERY.map(|e| e.1))).build();
    every.set_selected(every_index(f.every_months));
    let due = adw::EntryRow::builder().title("Next due (e.g. 15 Mar 2027)").text(f.next_due.format("%d %b %Y").to_string()).build();
    for r in [name.upcast_ref::<gtk::Widget>(), amount.upcast_ref(), every.upcast_ref(), due.upcast_ref()] {
        g.add(r);
    }
    let dlg = adw::AlertDialog::builder()
        .heading(if f.id == 0 { "Add a Non-Monthly Bill" } else { "Change Bill" })
        .body("Set aside a share every month so the bill is covered when it comes.")
        .extra_child(&g)
        .build();
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let (w, sc) = (Rc::downgrade(app), sc.to_string());
    dlg.connect_response(Some("save"), move |_, _| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        let (Some(v), Some(d)) = (parse_user(&amount.text()), parse_date(&due.text())) else { return a.toast("Type the amount, and the due date like 15 Mar 2027") };
        let mut nf = f.clone();
        nf.name = if name.text().trim().is_empty() { "Bill".into() } else { name.text().trim().to_string() };
        nf.amount = v.abs();
        nf.every_months = EVERY[(every.selected() as usize).min(EVERY.len() - 1)].0;
        nf.next_due = d;
        a.saved(a.db.save_fund(&sc, &nf));
        done();
    }));
    dlg.present(Some(&app.win));
}

/// How often a non-monthly bill comes.
const EVERY: [(i32, &str); 5] = [(12, "Once a year"), (6, "Every 6 months"), (4, "Every 4 months (termly)"), (3, "Every 3 months"), (24, "Every 2 years")];

fn every_index(months: i32) -> u32 {
    EVERY.iter().position(|e| e.0 == months).unwrap_or(0) as u32
}

/// Big bills most households have, offered on the wizard's yearly-bills step:
/// (name, hint, how often, due month if it's usually fixed).
const BIG_BILLS: [(&str, &str, i32, Option<u32>); 9] = [
    ("Car insurance", "Usually renewed once a year", 12, None),
    ("Home or contents insurance", "Including hurricane cover", 12, None),
    ("HOA or strata fees", "When they're billed yearly or quarterly", 12, None),
    ("Vehicle licence & registration", "The yearly licensing and inspection", 12, None),
    ("Health insurance", "Only if you pay it yourself, not through payroll", 12, None),
    ("School fees", "Per term", 4, None),
    ("Memberships & annual plans", "Gym, clubs, Costco, Amazon Prime, software", 12, None),
    ("Travel", "A yearly trip, or flights home", 12, None),
    ("Christmas & birthdays", "Gifts and celebrations", 12, Some(12)),
];

/// One bill on the wizard's yearly-bills step. An empty amount means you don't have it.
struct BillLine {
    /// The saved bill it came from (0 = new).
    id: i64,
    stem: String,
    currency: Currency,
    /// Its name: fixed for a listed bill, typed for one you add.
    title: String,
    name: Option<gtk::Entry>,
    amount: gtk::Entry,
    every: gtk::DropDown,
    month: gtk::DropDown,
    /// The saved due date, kept while its month isn't changed.
    due: Option<NaiveDate>,
}

impl BillLine {
    fn amount(&self) -> Option<Decimal> {
        parse_user(&self.amount.text()).map(|d| d.abs()).filter(|d| !d.is_zero())
    }
    fn every(&self) -> i32 {
        EVERY[(self.every.selected() as usize).min(EVERY.len() - 1)].0
    }
    fn name(&self) -> String {
        let typed = self.name.as_ref().map(|e| e.text().trim().to_string()).unwrap_or_default();
        if self.name.is_some() { if typed.is_empty() { "Bill".into() } else { typed } } else { self.title.clone() }
    }
    fn next_due(&self) -> NaiveDate {
        let m = self.month.selected() + 1;
        match self.due {
            Some(d) if d.month() == m && month_start(d) >= month_start(today()) => d,
            _ => core::due_in_month(m, today()),
        }
    }
}

// ──────────────────────────────────────────────────────────────── the setup wizard

pub(crate) fn parse_date(t: &str) -> Option<NaiveDate> {
    let t = t.trim();
    ["%d %b %Y", "%d %B %Y", "%Y-%m-%d", "%d/%m/%Y", "%d-%m-%Y", "%b %d %Y", "%B %d %Y"].iter().find_map(|f| NaiveDate::parse_from_str(&t.replace(',', ""), f).ok())
}

/// An amount, or "5%" (a percent).
fn parse_amount(t: &str) -> Option<(Decimal, bool)> {
    let pct = t.trim().ends_with('%');
    parse_user(t.trim().trim_end_matches('%')).map(|d| (d.abs(), pct))
}

pub(crate) fn amount_text(d: Decimal) -> String {
    if d.is_zero() { String::new() } else { d.round_dp(2).normalize().to_string() }
}

/// One category on the wizard's spending step.
struct CatLine {
    cat: Category,
    entry: gtk::Entry,
    row: adw::ActionRow,
    /// Usually, a month.
    usual: Option<Decimal>,
    /// Shown fortnightly: (the text we put in, the monthly figure it came from).
    memo: RefCell<Option<(String, String)>>,
}

impl CatLine {
    /// The monthly budget typed (or converted from fortnightly); `None` = no budget.
    fn monthly(&self, fortnightly: bool) -> Option<Decimal> {
        let text = self.entry.text();
        let from_memo = self.memo.borrow().as_ref().filter(|(shown, _)| fortnightly && *shown == text.as_str()).and_then(|(_, m)| parse_user(m));
        let v = from_memo.or_else(|| parse_user(&text).map(|d| if fortnightly { (d.abs() * Decimal::from(26) / Decimal::from(12)).round_dp(2) } else { d.abs() }))?;
        (!v.is_zero()).then_some(v)
    }
}

pub(crate) fn step_page(title: &str, intro: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(24).margin_end(24).build();
    let h = heading(title);
    h.add_css_class("title-2");
    b.append(&h);
    b.append(&dim(intro));
    (gtk::ScrolledWindow::builder().child(&b).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build(), b)
}

/// Five steps: pay & deductions → other money in and out → big yearly bills → spending → savings
/// goal (with the maths, the yearly bills' monthly share included).
pub fn wizard(app: &Rc<App>, done: crate::Refresh) {
    let s = Rc::new(snap(app));
    let (ccy, m) = (s.ccy, month_start(today()));
    let money = move |d: Decimal| fmt_money(d, ccy);
    // pay is looked for up to the newest statement: they arrive a month late
    let detected = core::detect_salary(&s.rows, data_through(app));
    let existing = s.plan.clone();
    let start = existing.clone().unwrap_or_else(|| {
        let mut p = Plan { currency: Some(ccy), loan_payments: usual_loans(&s, m), ..Default::default() };
        if let Some(d) = &detected {
            p.cycle = d.cycle;
            // the first payday still to come
            p.next_pay = Some(d.cycle.next(d.last));
            p.next_pay = p.paydays(today(), 1).first().copied();
            p.gross = s.fx.convert(d.amount, d.currency, ccy);
        }
        p
    });

    // ── 1. pay
    let (p1, b1) = step_page(
        "Your Pay",
        "How often you're paid, the gross amount on your payslip, and what comes off it. In Cayman that's usually your 5% pension contribution and your share of health insurance. Type a deduction as an amount or a percent of gross (5%).",
    );
    if let Some(d) = &detected {
        let l = gtk::Label::builder()
            .label(format!(
                "From your statements: {} from {} {}, last on {}. That's take-home — if it matches, leave gross as it is and add no deductions; otherwise put in the payslip's gross and the deductions.",
                money(s.fx.convert(d.amount, d.currency, ccy)),
                d.payee,
                d.cycle.label().to_lowercase(),
                d.last.format("%d %b %Y")
            ))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["card", "caption"])
            .build();
        l.set_margin_top(4);
        b1.append(&l);
    }
    let g = adw::PreferencesGroup::new();
    let cycle = adw::ComboRow::builder().title("Paid").model(&gtk::StringList::new(&PayCycle::ALL.map(PayCycle::label))).build();
    cycle.set_selected(PayCycle::ALL.iter().position(|c| *c == start.cycle).unwrap_or(3) as u32);
    let next_pay = adw::EntryRow::builder().title("Next payday (e.g. 31 Oct 2026)").text(start.next_pay.map(|d| d.format("%d %b %Y").to_string()).unwrap_or_default()).build();
    let gross = adw::EntryRow::builder().title(format!("Gross pay per payday ({})", ccy.symbol())).text(amount_text(start.gross)).input_purpose(gtk::InputPurpose::Number).build();
    g.add(&cycle);
    g.add(&next_pay);
    g.add(&gross);
    b1.append(&g);
    let dg = adw::PreferencesGroup::builder().title("Deductions per payday").build();
    let deds: Rc<RefCell<Vec<(adw::EntryRow, adw::EntryRow, gtk::Box)>>> = Rc::default();
    let ded_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    dg.add(&ded_box);
    let presets = gtk::Box::builder().spacing(6).build();
    let (pension, health, other) = (gtk::Button::with_label("+ Pension 5%"), gtk::Button::with_label("+ Health Insurance"), gtk::Button::with_label("+ Other"));
    for b in [&pension, &health, &other] {
        b.add_css_class("pill");
        presets.append(b);
    }
    dg.add(&presets);
    b1.append(&dg);
    let take_home = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b1.append(&take_home);

    // ── 2. other money
    let (p2, b2) = step_page(
        "Other Money In and Out",
        "Anything else that comes in every month (rent from a property, side work) and loan or car instalments that go out. Instalments are transfers, so they're not in the spending budget — but they still come out of what you can save.",
    );
    let g2 = adw::PreferencesGroup::new();
    let other_in = adw::EntryRow::builder().title(format!("Other income per month ({})", ccy.symbol())).text(amount_text(start.other_income)).build();
    let loans = adw::EntryRow::builder().title(format!("Loan payments per month ({})", ccy.symbol())).text(amount_text(start.loan_payments)).build();
    g2.add(&other_in);
    g2.add(&loans);
    b2.append(&g2);
    b2.append(&dim(&format!("Loan payments are filled in from the median of your last {MONTHS} months of Loan Payments.")));

    // ── 3. big yearly bills
    let (p_y, b_y) = step_page(
        "Big Yearly Bills",
        "The costs that don't come every month — insurance, fees, licences, school terms — so a normal month never shows them and they land all at once. Put in what each costs and the month it's next due; FinGraph sets a share aside every month so the money is there when it comes. Leave any you don't have empty.",
    );
    let bill_list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    b_y.append(&bill_list);
    let add_bill_btn = gtk::Button::builder().label("+ Add Another").halign(gtk::Align::Start).css_classes(["pill"]).build();
    b_y.append(&add_bill_btn);
    let bills_total = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b_y.append(&bills_total);
    let bills: Rc<RefCell<Vec<BillLine>>> = Rc::default();

    // ── 4. spending
    let (p3, b3) = step_page(
        "Your Spending",
        "Each category starts at what you usually spend (the median of your last six months) or the budget you already have. Change any of them; leave one empty for no budget. Fixed expenses are marked (change which on the Budget page) — the savings step only trims the variable ones.",
    );
    let (usual, subs_m, one_off) = usual(&s, m);
    // Monthly | Fortnightly: what the amounts below mean (saved as monthly either way)
    let units = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Start).build();
    let monthly_b = gtk::ToggleButton::builder().label("Monthly").active(true).build();
    let fortnight_b = gtk::ToggleButton::builder().label("Fortnightly").group(&monthly_b).build();
    units.append(&monthly_b);
    units.append(&fortnight_b);
    b3.append(&units);
    let fortnightly = Rc::new(Cell::new(false));
    let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let cat_entries: Rc<Vec<CatLine>> = Rc::new(
        s.cats
            .iter()
            .filter(|c| s.budgets.contains_key(&c.id) || usual.contains_key(&c.name))
            .map(|c| {
                let u = usual.get(&c.name).copied();
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).build();
                let e = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).build();
                e.set_text(&amount_text(s.budgets.get(&c.id).copied().or(u).unwrap_or_default()));
                r.add_suffix(&e);
                lb.append(&r);
                CatLine { cat: c.clone(), entry: e, row: r, usual: u, memo: RefCell::default() }
            })
            .collect(),
    );
    // each row's note, in the chosen unit
    let fixed_names: Rc<Vec<String>> = Rc::new(s.fixed.clone());
    let subtitles = {
        let (cat_entries, fortnightly, fixed_names) = (cat_entries.clone(), fortnightly.clone(), fixed_names.clone());
        move || {
            let k = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
            for l in cat_entries.iter() {
                let note = l.usual.map(|u| format!(" · {}", usual_text(&l.cat.name, (u * k).round_dp(2), (subs_m * k).round_dp(2), (one_off * k).round_dp(2), ccy))).unwrap_or_default();
                l.row.set_subtitle(&format!("{}{note}", if fixed_names.contains(&l.cat.name) { "Fixed expense" } else { "Variable" }));
            }
        }
    };
    subtitles();
    b3.append(&lb);
    let spend_total = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b3.append(&spend_total);

    // ── 5. savings
    let (p4, b4) = step_page(
        "Your Savings Goal",
        "How much you want to put away each month — an amount, or a percent of take-home (20%). FinGraph does the arithmetic: take-home − spending − big yearly bills − loans = what's left, and if that's short of the goal, how much the variable categories need to shrink.",
    );
    let g4 = adw::PreferencesGroup::new();
    let goal = adw::EntryRow::builder().title(format!("Save per month ({} or %)", ccy.symbol())).text(amount_text(start.savings_goal)).build();
    g4.add(&goal);
    b4.append(&g4);
    let maths = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    maths.set_margin_top(4);
    b4.append(&maths);
    let scale = gtk::Button::builder().label("Trim Variable Categories to Reach It").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).build();
    b4.append(&scale);
    let ahead = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b4.append(&ahead);

    // read every field into a plan + budgets, and redo the sums
    let read = {
        let (cycle, next_pay, gross, deds, other_in, loans, goal, cat_entries, fortnightly) = (cycle.clone(), next_pay.clone(), gross.clone(), deds.clone(), other_in.clone(), loans.clone(), goal.clone(), cat_entries.clone(), fortnightly.clone());
        Rc::new(move || -> (Plan, Vec<(i64, Option<Decimal>)>) {
            let mut p = Plan {
                cycle: PayCycle::ALL[(cycle.selected() as usize).min(3)],
                next_pay: parse_date(&next_pay.text()),
                gross: parse_user(&gross.text()).unwrap_or_default().abs(),
                deductions: deds
                    .borrow()
                    .iter()
                    .filter_map(|(n, a, _)| {
                        let (amount, percent) = parse_amount(&a.text())?;
                        Some(Deduction { name: n.text().to_string(), amount, percent })
                    })
                    .collect(),
                other_income: parse_user(&other_in.text()).unwrap_or_default().abs(),
                loan_payments: parse_user(&loans.text()).unwrap_or_default().abs(),
                currency: Some(ccy),
                ..Default::default()
            };
            let budgets: Vec<(i64, Option<Decimal>)> = cat_entries.iter().map(|l| (l.cat.id, l.monthly(fortnightly.get()))).collect();
            p.savings_goal = match parse_amount(&goal.text()) {
                Some((v, true)) => (p.monthly_income() * v / Decimal::ONE_HUNDRED).round_dp(0),
                Some((v, false)) => v,
                None => Decimal::ZERO,
            };
            (p, budgets)
        })
    };
    let factor: Rc<Cell<Option<Decimal>>> = Rc::default();
    let recalc: Rc<dyn Fn()> = {
        let (read, take_home, spend_total, maths, ahead, scale, cat_entries, factor, fixed_names) = (read.clone(), take_home.clone(), spend_total.clone(), maths.clone(), ahead.clone(), scale.clone(), cat_entries.clone(), factor.clone(), fixed_names.clone());
        let (bills, bills_total, fx) = (bills.clone(), bills_total.clone(), s.fx.clone());
        Rc::new(move || {
            let (p, budgets) = read();
            // the share of the big bills set aside each month, in the display currency
            let aside: Decimal = bills.borrow().iter().filter_map(|l| Some(fx.convert(l.amount()? / Decimal::from(l.every()), l.currency, ccy))).sum::<Decimal>().round_dp(2);
            let yearly: Decimal = bills.borrow().iter().filter_map(|l| Some(fx.convert(l.amount()? * Decimal::from(12) / Decimal::from(l.every()), l.currency, ccy))).sum::<Decimal>().round_dp(2);
            let n = bills.borrow().iter().filter(|l| l.amount().is_some()).count();
            bills_total.set_label(&if n == 0 {
                "No big bills yet — add any you have, or skip this step.".to_string()
            } else {
                format!("{n} bill{} · {} a year · set aside {} a month", if n == 1 { "" } else { "s" }, money(yearly), money(aside))
            });
            let deducted: Vec<String> = p.deductions.iter().map(|d| format!("{} {}", if d.name.is_empty() { "deduction" } else { &d.name }, money(p.deduction(d)))).collect();
            take_home.set_label(&format!(
                "Gross {} a payday = {} a month.\nTake-home {} a payday{} → {} a month ({} × {} ÷ 12)",
                money(p.gross),
                money((p.gross * p.cycle.per_year() / Decimal::from(12)).round_dp(2)),
                money(p.net_pay()),
                if deducted.is_empty() { String::new() } else { format!(" (gross {} − {})", money(p.gross), deducted.join(" − ")) },
                money(p.monthly_income() - p.other_income),
                money(p.net_pay()),
                p.cycle.per_year()
            ));
            let total: Decimal = budgets.iter().filter_map(|b| b.1).sum();
            let fixed: Decimal = cat_entries.iter().zip(&budgets).filter(|(l, _)| fixed_names.contains(&l.cat.name)).filter_map(|(_, b)| b.1).sum();
            let flexible = total - fixed;
            let fn_ = |d: Decimal| money((d * Decimal::from(12) / Decimal::from(26)).round_dp(2));
            spend_total.set_label(&format!(
                "Spending budget {} a month ({} a fortnight) — fixed {}, variable {} a month",
                money(total),
                fn_(total),
                money(fixed),
                money(flexible)
            ));
            let save = p.monthly_savings(total) - aside;
            let mut lines = vec![
                format!("  Take-home per month        {:>14}", money(p.monthly_income())),
                format!("− Spending budget            {:>14}", money(total)),
                format!("− Big yearly bills set aside {:>14}", money(aside)),
                format!("− Loan payments              {:>14}", money(p.loan_payments)),
                format!("= Left to save               {:>14}", money(save)),
            ];
            factor.set(None);
            scale.set_visible(false);
            if !p.savings_goal.is_zero() {
                lines.push(format!("  Goal                       {:>14}", money(p.savings_goal)));
                if save >= p.savings_goal {
                    lines.push(format!("\nOn track, with {} a month to spare.", money(save - p.savings_goal)));
                } else {
                    lines.push(format!("\nShort by {} a month.", money(p.savings_goal - save)));
                    // the set-aside is as fixed as rent: only variable spending is trimmed
                    match p.flexible_factor(fixed + aside, flexible) {
                        Some(k) => {
                            lines.push(format!(
                                "Solve take-home − loans − fixed − big bills − k × variable = goal:\nk = ({} − {} − {} − {} − {}) ÷ {} = {:.3}\nTrim every variable category by {}% ({} → {}).",
                                money(p.monthly_income()),
                                money(p.loan_payments),
                                money(fixed),
                                money(aside),
                                money(p.savings_goal),
                                money(flexible),
                                k,
                                ((Decimal::ONE - k) * Decimal::ONE_HUNDRED).round_dp(0),
                                money(flexible),
                                money((flexible * k).round_dp(2))
                            ));
                            factor.set(Some(k));
                            scale.set_visible(true);
                        }
                        None => lines.push("Even with no variable spending it doesn't add up — lower the goal or the fixed expenses.".into()),
                    }
                }
            }
            maths.set_label(&lines.join("\n"));
            let months_left = 12 - today().month() as i32;
            ahead.set_label(&format!(
                "At {} a month: {} by the end of {} ({} months), {} in a year, {} in five years (before any interest).",
                money(save),
                money(save * Decimal::from(months_left.max(0))),
                today().year(),
                months_left,
                money(save * Decimal::from(12)),
                money(save * Decimal::from(60))
            ));
        })
    };

    // deduction rows
    type AddDeduction = Rc<dyn Fn(&str, &str)>;
    let add_ded: AddDeduction = {
        let (deds, ded_box, recalc) = (deds.clone(), ded_box.clone(), recalc.clone());
        Rc::new(move |name: &str, amount: &str| {
            let row = gtk::Box::builder().spacing(6).build();
            let n = adw::EntryRow::builder().title("Name").text(name).hexpand(true).build();
            let a = adw::EntryRow::builder().title("Amount or %").text(amount).build();
            let rm = gtk::Button::builder().icon_name("list-remove-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Remove").build();
            let l = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).hexpand(true).build();
            l.append(&n);
            let l2 = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).width_request(170).build();
            l2.append(&a);
            row.append(&l);
            row.append(&l2);
            row.append(&rm);
            for e in [&n, &a] {
                let r = recalc.clone();
                e.connect_changed(move |_| r());
            }
            {
                let (deds, ded_box, row2, recalc) = (deds.clone(), ded_box.clone(), row.clone(), recalc.clone());
                rm.connect_clicked(move |_| {
                    deds.borrow_mut().retain(|x| x.2 != row2);
                    ded_box.remove(&row2);
                    recalc();
                });
            }
            ded_box.append(&row);
            deds.borrow_mut().push((n, a, row));
            recalc();
        })
    };
    // yearly-bill rows: what's saved, then what the statements show, then the usual big bills
    type AddBill = Rc<dyn Fn(BillLine, &str)>;
    // the amount and both drop-downs line up from row to row
    let widths = [gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal)];
    let add_bill: AddBill = {
        let (bills, bill_list, recalc) = (bills.clone(), bill_list.clone(), recalc.clone());
        Rc::new(move |l: BillLine, hint: &str| {
            let r = adw::ActionRow::builder().title(glib::markup_escape_text(&l.title)).subtitle(glib::markup_escape_text(hint)).build();
            if let Some(e) = &l.name {
                e.set_hexpand(true);
                e.set_valign(gtk::Align::Center);
                r.add_prefix(e);
            }
            for (w, g) in [l.amount.upcast_ref::<gtk::Widget>(), l.every.upcast_ref(), l.month.upcast_ref()].into_iter().zip(&widths) {
                g.add_widget(w);
                r.add_suffix(w);
            }
            for e in std::iter::once(&l.amount).chain(l.name.as_ref()) {
                let rc = recalc.clone();
                e.connect_changed(move |_| rc());
            }
            for d in [&l.every, &l.month] {
                let rc = recalc.clone();
                d.connect_selected_notify(move |_| rc());
            }
            bill_list.append(&r);
            bills.borrow_mut().push(l);
            recalc();
        })
    };
    let months: [&str; 12] = ["Due Jan", "Due Feb", "Due Mar", "Due Apr", "Due May", "Due Jun", "Due Jul", "Due Aug", "Due Sep", "Due Oct", "Due Nov", "Due Dec"];
    let line = move |id: i64, stem: &str, cur: Currency, title: &str, typed: bool, amount: Decimal, every: i32, due: Option<NaiveDate>, month: u32| {
        let a = gtk::Entry::builder().placeholder_text(cur.symbol()).width_chars(9).max_width_chars(9).hexpand(false).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).text(amount_text(amount)).tooltip_text("What it costs each time").build();
        let ev = gtk::DropDown::builder().model(&gtk::StringList::new(&EVERY.map(|e| e.1))).valign(gtk::Align::Center).tooltip_text("How often it comes").build();
        ev.set_selected(every_index(every));
        let mo = gtk::DropDown::builder().model(&gtk::StringList::new(&months)).valign(gtk::Align::Center).tooltip_text("The month it's next due").build();
        mo.set_selected(month.clamp(1, 12) - 1);
        let name = typed.then(|| gtk::Entry::builder().placeholder_text("What is it?").text(title).build());
        BillLine { id, stem: stem.to_string(), currency: cur, title: title.to_string(), name, amount: a, every: ev, month: mo, due }
    };
    let next_month = add_months(month_start(today()), 1).month();
    for f in &s.funds {
        let d = f.due(today());
        add_bill(line(f.id, &f.stem, f.currency, &f.name, false, f.amount, f.every_months, Some(d), d.month()), &format!("Already planned · {}", fmt_money(f.amount, f.currency)));
    }
    let ideas: Vec<core::FundIdea> = core::fund_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
    for i in &ideas {
        add_bill(
            line(0, &i.stem, ccy, &i.payee, false, i.amount.round_dp(0), i.every_months, Some(i.next_due), i.next_due.month()),
            &format!("Seen in your statements: {} {}", money(i.amount), if i.every_months == 12 { "once a year" } else { "every 3 months" }),
        );
    }
    let taken: Vec<String> = s.funds.iter().map(|f| f.name.to_lowercase()).chain(ideas.iter().map(|i| i.payee.to_lowercase())).collect();
    for (name, hint, every, month) in BIG_BILLS {
        if !taken.contains(&name.to_lowercase()) {
            add_bill(line(0, "", ccy, name, false, Decimal::ZERO, every, None, month.unwrap_or(next_month)), hint);
        }
    }
    {
        let add = add_bill.clone();
        add_bill_btn.connect_clicked(move |_| add(line(0, "", ccy, "", true, Decimal::ZERO, 12, None, next_month), ""));
    }
    for d in &start.deductions {
        add_ded(&d.name, &if d.percent { format!("{}%", d.amount.normalize()) } else { amount_text(d.amount) });
    }
    for (b, n, a) in [(&pension, "Pension", "5%"), (&health, "Health insurance", ""), (&other, "", "")] {
        let add = add_ded.clone();
        b.connect_clicked(move |_| add(n, a));
    }
    for e in [next_pay.upcast_ref::<gtk::Editable>(), gross.upcast_ref(), other_in.upcast_ref(), loans.upcast_ref(), goal.upcast_ref()] {
        let r = recalc.clone();
        e.connect_changed(move |_| r());
    }
    {
        let r = recalc.clone();
        cycle.connect_selected_notify(move |_| r());
    }
    for l in cat_entries.iter() {
        let r = recalc.clone();
        l.entry.connect_changed(move |_| r());
    }
    {
        let (cat_entries, fortnightly, recalc) = (cat_entries.clone(), fortnightly.clone(), recalc.clone());
        fortnight_b.connect_toggled(move |b| {
            if b.is_active() == fortnightly.get() {
                return;
            }
            // convert what's typed, remembering the monthly figure so switching back is exact
            let to_fn = b.is_active();
            for l in cat_entries.iter() {
                let monthly = l.monthly(!to_fn);
                let text = match (to_fn, monthly) {
                    (true, Some(v)) => {
                        let t = amount_text((v * Decimal::from(12) / Decimal::from(26)).round_dp(2));
                        *l.memo.borrow_mut() = Some((t.clone(), amount_text(v)));
                        t
                    }
                    (false, Some(v)) => amount_text(v),
                    (_, None) => String::new(),
                };
                l.entry.set_text(&text);
                if !to_fn {
                    *l.memo.borrow_mut() = None;
                }
            }
            fortnightly.set(to_fn);
            subtitles();
            recalc();
        });
    }
    {
        let (cat_entries, factor, fixed_names) = (cat_entries.clone(), factor.clone(), fixed_names.clone());
        scale.connect_clicked(move |_| {
            let Some(k) = factor.get() else { return };
            for l in cat_entries.iter().filter(|l| !fixed_names.contains(&l.cat.name)) {
                let e = &l.entry;
                if let Some(v) = parse_user(&e.text()) {
                    // down to the nearest 5
                    let five = Decimal::from(5);
                    e.set_text(&amount_text((v.abs() * k / five).floor() * five));
                }
            }
        });
    }
    recalc();

    // ── the frame: Back / Next / Save
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let pages = [p1, p2, p_y, p3, p4];
    for (i, p) in pages.iter().enumerate() {
        stack.add_named(p, Some(&i.to_string()));
    }
    let back = gtk::Button::builder().label("Back").sensitive(false).build();
    let fwd = gtk::Button::builder().label("Next").css_classes(["suggested-action"]).build();
    let step = Rc::new(Cell::new(0usize));
    let header = adw::HeaderBar::new();
    header.pack_start(&back);
    header.pack_end(&fwd);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&stack));
    let dlg = adw::Dialog::builder().title("Set Up Your Budget").content_width(720).content_height(720).child(&tv).build();
    let show = {
        let (stack, back, fwd, step, dlg) = (stack.clone(), back.clone(), fwd.clone(), step.clone(), dlg.clone());
        move |i: usize| {
            step.set(i);
            stack.set_visible_child_name(&i.to_string());
            back.set_sensitive(i > 0);
            fwd.set_label(if i == 4 { "Save Budget" } else { "Next" });
            dlg.set_title(&format!("Set Up Your Budget · {} of 5", i + 1));
        }
    };
    show(0);
    {
        let (show, step) = (show.clone(), step.clone());
        back.connect_clicked(move |_| show(step.get().saturating_sub(1)));
    }
    {
        let (w, dlg, s) = (Rc::downgrade(app), dlg.clone(), s.clone());
        fwd.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            if step.get() == 0 && !next_pay.text().trim().is_empty() && parse_date(&next_pay.text()).is_none() {
                return a.toast("Type the payday like 31 Oct 2026");
            }
            if step.get() < 4 {
                return show(step.get() + 1);
            }
            let (p, budgets) = read();
            a.saved(a.db.save_plan(&s.scope, &p));
            for (id, v) in budgets {
                a.saved(a.db.set_budget(&s.scope, id, v.map(|v| (v, ccy))));
            }
            for l in bills.borrow().iter() {
                match l.amount() {
                    Some(v) => {
                        let f = Fund { id: l.id, name: l.name(), amount: v, currency: l.currency, every_months: l.every(), next_due: l.next_due(), stem: l.stem.clone() };
                        a.saved(a.db.save_fund(&s.scope, &f));
                    }
                    // emptied: you don't have it any more
                    None if l.id != 0 => {
                        a.saved(a.db.delete_fund(l.id));
                    }
                    None => {}
                }
            }
            a.toast("Budget saved");
            dlg.close();
            done();
        });
    }
    dlg.present(Some(&app.win));
}

/// Your month against the well-known budgeting rules (50/30/20 and friends), each explained.
fn rules_of_thumb(into: &gtk::Box, needs: Decimal, wants: Decimal, income: Option<Decimal>, ccy: Currency) {
    into.append(&info_heading(
        "Budgeting Rules of Thumb",
        "Popular ways to divide take-home pay, with your month against each. Needs are your fixed expenses, non-monthly bills and loan payments; wants are your variable spending; saving is what's left, including anything paid off debt above the minimums. Mark categories Fixed or Variable to sort needs from wants.",
    ));
    let shares = income.and_then(|i| core::shares(needs, wants, i));
    let Some((n, w, sv)) = shares else {
        into.append(&dim("Set up your pay (Set Up Budget…) to see your month against each rule."));
        return;
    };
    let money = |d: Decimal| fmt_money(d, ccy);
    into.append(&dim(&format!("Yours: needs {n}% ({}) · wants {w}% ({}) · saving {sv}%.", money(needs), money(wants))));
    let l = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    for r in core::RULES {
        let d = |v: u8| Decimal::from(v);
        // what's off, in points of take-home
        let mut off: Vec<String> = Vec::new();
        match (r.needs, r.wants) {
            (Some(tn), Some(tw)) => {
                if n > d(tn) {
                    off.push(format!("needs {} points over", n - d(tn)));
                }
                if w > d(tw) {
                    off.push(format!("wants {} points over", w - d(tw)));
                }
            }
            _ if n + w > d(r.spend) => off.push(format!("spending {} points over", n + w - d(r.spend))),
            _ => {}
        }
        if sv < d(r.save) {
            off.push(format!("saving {} points short", d(r.save) - sv));
        }
        let verdict = if off.is_empty() { "You're within this rule.".to_string() } else { format!("Off by: {}.", off.join(", ")) };
        let row = adw::ActionRow::builder().title(r.name).subtitle(glib::markup_escape_text(&format!("{}\n{verdict}", r.about))).subtitle_lines(0).build();
        row.add_suffix(&gtk::Image::builder().icon_name(if off.is_empty() { "emblem-ok-symbolic" } else { "dialog-warning-symbolic" }).css_classes([if off.is_empty() { "success" } else { "warning" }]).build());
        l.append(&row);
    }
    let zb = adw::ActionRow::builder()
        .title("Zero-Based Budget")
        .subtitle("Give every dollar of take-home a job — a category, a bill, savings or a debt — until nothing is left unassigned. It's what this page does: budgets per category, non-monthly bills set aside, and what's left goes to saving. Monthly Close shows how the month really went against it.")
        .subtitle_lines(0)
        .build();
    l.append(&zb);
    into.append(&l);
}
