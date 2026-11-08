//! Budget: a monthly amount per category, suggested from what you usually spend (the median of the
//! last six months), with this month's spending against it and where the month is heading; a plan
//! built from your pay (the setup wizard) that projects what you'll save; and alerts when a category
//! goes over, or uses 85% with more of the month still to come.

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::budget::{self as core, Alert, Deduction, Envelope, Fund, PayCycle, Plan, scope, suggest};
use fingraph_core::db::{Category, TxRow};
use fingraph_core::method::Method;
use fingraph_core::plan_doc::Roles;
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
    /// Categories counted as needs (the rest are wants), for the budgeting methods.
    pub(crate) needs: Vec<String>,
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
    Snap { ccy, fx, parents: app.parents(), scope: sc, rows, cats, budgets, plan, fixed: app.db.fixed_categories(), needs: app.db.need_categories(), funds }
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
/// Left to spend in the month starting `m`: (funded, spent) across the budgeted categories.
pub fn left_in(app: &App, m: NaiveDate) -> Option<(Decimal, Decimal)> {
    let s = snap(app);
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
/// big spends land in over the next year. Without any, nothing (the strip's Next Big Bill offers to add them).
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
    // none yet: Overview's strip offers to add them, so no card stands empty here
    if s.funds.is_empty() {
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
/// Subscriptions billed this many days apart or more are big bills (quarterly, yearly), not monthly spending.
const BILL_DAYS: i64 = 85;

/// What each category usually costs a month (`suggest`), except Subscriptions & Digital: that one is
/// your active subscriptions' monthly cost (as on the Subscriptions page) plus the usual one-off
/// digital purchases (apps, domains, downloads), since a median of the two mixed reads as "subscriptions".
/// Also returns (subscriptions a month, other digital a month) for labelling.
pub(crate) fn usual(s: &Snap, m: NaiveDate) -> (BTreeMap<String, Decimal>, Decimal, Decimal) {
    let mut out = suggest(&s.rows, m, MONTHS, s.ccy, &s.fx, &s.parents);
    // quarterly and yearly ones are on the big-bills step, set aside there: not counted twice
    let subs: Decimal = fingraph_core::insights::subscriptions(&s.rows, s.ccy, &s.fx, today()).iter().filter(|x| x.active && x.cadence_days < BILL_DAYS).map(|x| x.monthly).sum();
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

/// The first time a month is needed: the newest one statements speak for, since they arrive after it ends.
fn settle_month(app: &Rc<App>) {
    if MONTH.with(Cell::get).is_none() {
        let newest = app.db.documents(&app.entity_ids()).unwrap_or_default().iter().filter_map(|d| d.period.map(|p| p.1)).max();
        let shown = crate::dashboard::headline_month(newest, today(), |m| left_in(app, m).is_some());
        MONTH.with(|c| c.set(Some(shown)));
    }
}

/// How one share of a method stands: the bar, and a tick at its target.
fn target_bar(share: f64, target: f64, color: charts::Rgb) -> gtk::DrawingArea {
    let bar = gtk::DrawingArea::builder().content_height(16).hexpand(true).build();
    bar.set_draw_func(move |w, cr, width, _| {
        let width = f64::from(width);
        let fg = w.color();
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.14);
        cr.rectangle(0.0, 3.0, width, 10.0);
        let _ = cr.fill();
        cr.set_source_rgb(color.0, color.1, color.2);
        cr.rectangle(0.0, 3.0, width * share.clamp(0.0, 1.0), 10.0);
        let _ = cr.fill();
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.95);
        cr.rectangle((width * target.clamp(0.0, 1.0) - 1.0).max(0.0), 0.0, 2.0, 16.0);
        let _ = cr.fill();
    });
    bar
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();

    let nav = gtk::Box::builder().spacing(8).build();
    let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous month").valign(gtk::Align::Center).build();
    let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next month").valign(gtk::Align::Center).build();
    let title = heading("");
    title.add_css_class("title-2");
    title.set_xalign(0.0);
    let proven_text = gtk::Label::builder().xalign(0.0).css_classes(["caption", "proven"]).build();
    let proven = gtk::Box::builder().spacing(6).build();
    proven.append(&gtk::Image::builder().icon_name("object-select-symbolic").css_classes(["proven"]).pixel_size(12).build());
    proven.append(&proven_text);
    let title_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
    title_box.append(&title);
    title_box.append(&proven);
    let setup = gtk::Button::builder().label("Set Up Budget…").valign(gtk::Align::Center).tooltip_text("Your budgeting method, your pay and deductions, your spending, your savings goal").build();
    let close = gtk::Button::builder().label("Monthly Close").valign(gtk::Align::Center).css_classes(["suggested-action"]).tooltip_text("Go through a month once its statements are in: check them, sort what's new, settle the budget, tick off the bills").build();
    let fill = gtk::Button::builder().label("Fill From My Spending").valign(gtk::Align::Center).build();
    nav.append(&prev);
    nav.append(&next);
    nav.append(&title_box);
    nav.append(&fill);
    let plan_menu = crate::planpdf::menu_button(app);
    plan_menu.set_valign(gtk::Align::Center);
    nav.append(&plan_menu);
    nav.append(&setup);
    nav.append(&close);
    body.append(&nav);

    // what needs an answer from you comes first, and only while something does
    let review = adw::Banner::builder().button_label("Review").revealed(false).build();
    review.set_action_name(Some("win.show-review"));

    // the answer first: one number, which depends on the method
    let hero = gtk::Box::builder().spacing(18).css_classes(["card", "summary"]).build();
    let hero_text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
    let hero_title = gtk::Label::builder().xalign(0.0).css_classes(["caption-heading"]).build();
    hero_text.append(&with_info(&hero_title, "The number that matters under the budgeting method you chose. Spending counts what your statements show, so it fills in once they arrive. Change the method with the button at the top."));
    let hero_value = gtk::Label::builder().xalign(0.0).css_classes(["hero-value", "tnum"]).build();
    let hero_sub = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    let hero_pace = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
    hero_text.append(&hero_value);
    hero_text.append(&hero_sub);
    hero_text.append(&hero_pace);
    hero.append(&hero_text);
    let assign = gtk::Button::builder().valign(gtk::Align::Center).css_classes(["suggested-action"]).visible(false).build();
    hero.append(&assign);
    body.append(&hero);

    let method_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    body.append(&method_box);
    let groups_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).build();
    body.append(&groups_box);
    let outside = adw::ActionRow::builder().activatable(true).build();
    outside.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let w = Rc::downgrade(app);
        outside.connect_activated(move |_| {
            if let Some(a) = w.upgrade() {
                a.show_view("review");
            }
        });
    }
    let outside_list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    outside_list.append(&outside);
    body.append(&outside_list);
    let funds_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&funds_box);
    let watch_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&watch_box);

    let empty = adw::StatusPage::builder().icon_name("x-office-spreadsheet-symbolic").title("No statements yet").description("Import your bank statements, or load the demo data to explore.").build();
    let empty_btns = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
    empty_btns.append(&gtk::Button::builder().label("Import Statements…").css_classes(["pill", "suggested-action"]).action_name("win.import").build());
    empty_btns.append(&gtk::Button::builder().label("Load Demo Data").css_classes(["pill"]).action_name("win.demo").build());
    empty.set_child(Some(&empty_btns));
    let scroller = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1100).child(&body).build()).vexpand(true).build();
    let full = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    full.append(&review);
    full.append(&scroller);
    let page = gtk::Stack::new();
    page.add_named(&empty, Some("empty"));
    page.add_named(&full, Some("full"));

    // the page redraws itself after an edit; the sidebar and Summary follow
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

    // what Fill and the zero-based button do for the month on screen, set by each refresh
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
    let on_assign: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    {
        let on_assign = on_assign.clone();
        assign.connect_clicked(move |_| {
            let f = on_assign.borrow().clone();
            if let Some(f) = f {
                f();
            }
        });
    }
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let redraw = redraw.clone();
        let page = page.clone();
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let ids = a.entity_ids();
            let waiting = a.db.triage_count(&ids).unwrap_or(0);
            review.set_title(&format!("{waiting} merchant{} to review", if waiting == 1 { "" } else { "s" }));
            review.set_revealed(waiting > 0);
            if a.all_rows().is_empty() {
                return page.set_visible_child_name("empty");
            }
            page.set_visible_child_name("full");
            let docs = a.db.documents(&ids).unwrap_or_default();
            settle_month(&a);
            let proved = docs.iter().filter(|d| !matches!(d.reconciliation, fingraph_core::Reconciliation::Discrepancy(_))).filter_map(|d| d.period.map(|p| p.1)).max();
            proven.set_visible(proved.is_some());
            proven_text.set_label(&proved.map(|d| format!("Proven to the cent through {}", d.format("%d %b %Y"))).unwrap_or_default());

            let s = snap(&a);
            let (ccy, m) = (s.ccy, month());
            let method = s.plan.as_ref().map_or(Method::Own, |p| p.method);
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
            setup.set_label(&if s.plan.is_some() { format!("{}…", method.label()) } else { "Set Up Budget…".to_string() });
            setup.set_css_classes(if s.plan.is_none() && s.budgets.is_empty() { &["suggested-action"] } else { &[] });
            {
                // fill: categories without a budget get their usual amount
                let (w, sc, redraw, usual, cats) = (Rc::downgrade(&a), s.scope.clone(), redraw.clone(), usual.clone(), s.cats.clone());
                let has: Vec<i64> = s.budgets.keys().copied().collect();
                fill.set_visible(cats.iter().any(|c| !has.contains(&c.id) && usual.contains_key(&c.name)));
                fill.set_tooltip_text(Some(&format!("Give every category without a budget what it usually costs (median of the last {MONTHS} months)")));
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
            let share = elapsed(m, through);
            let proj = if share.is_zero() { total_s } else { (total_s / share).round_dp(2) };
            let loans = s.plan.as_ref().map(|p| p.loan_payments).unwrap_or_default();
            let debt_extra = s.plan.as_ref().map(|p| p.debt_extra).unwrap_or_default();
            let variable_budgets: Vec<Decimal> = s.cats.iter().filter(|c| !s.is_fixed(&c.name)).filter_map(|c| bm.get(&c.id).copied()).collect();
            let variable_names: Vec<&str> = s.cats.iter().filter(|c| !s.is_fixed(&c.name)).map(|c| c.name.as_str()).collect();
            let weekly = fingraph_core::method::weekly_allowance(&variable_budgets);

            assign.set_visible(false);
            hero_pace.set_visible(false);
            let good = |over: bool| -> [&str; 2] { ["hero-value", if over { "amount-out" } else { "amount-in" }] };
            match (method, &s.plan) {
                (Method::EightyTwenty, Some(_)) => {
                    let month_money: Decimal = variable_budgets.iter().copied().sum();
                    let spent: Decimal = variable_names.iter().filter_map(|n| now.get(*n)).copied().sum();
                    hero_title.set_label("Your Weekly Spending Money");
                    hero_value.set_label(&format!("{} a week", money(weekly)));
                    hero_value.set_css_classes(&good(false));
                    hero_sub.set_label(&format!("{} of {}'s spending money left", money(month_money - spent), m.format("%B")));
                }
                (Method::ZeroBased, Some(p)) => {
                    let left = fingraph_core::method::to_assign(p.monthly_income(), total_b, set_aside, loans, debt_extra, p.savings_goal);
                    let over = left.is_sign_negative() && !left.is_zero();
                    hero_title.set_label(if left.is_zero() { "Every Dollar Has a Job" } else if over { "Assigned Too Much" } else { "Still Needs a Job" });
                    hero_value.set_label(&money(left.abs()));
                    hero_value.set_css_classes(if left.is_zero() { &["hero-value", "amount-in"] } else { &["hero-value", "amount-variable"] });
                    hero_sub.set_label(&format!("{} take-home a month", money(p.monthly_income())));
                    if left > Decimal::ZERO {
                        assign.set_label(&format!("Give {} to Savings", money(left)));
                        assign.set_visible(true);
                        let (w, sc, redraw) = (Rc::downgrade(&a), s.scope.clone(), redraw.clone());
                        *on_assign.borrow_mut() = Some(Rc::new(move || {
                            let Some(a) = w.upgrade() else { return };
                            let Some(mut raw) = a.db.plan(&sc) else { return };
                            let to = raw.currency.unwrap_or(ccy);
                            raw.savings_goal += a.db.fx().unwrap_or_default().convert(left, ccy, to).round_dp(2);
                            a.saved(a.db.save_plan(&sc, &raw));
                            redraw();
                        }));
                    }
                }
                _ => {
                    let spending = s.plan.is_some() && method != Method::Own;
                    let (plan_all, spent_all) = if spending { (funded + set_aside + loans, in_b + set_aside + loans) } else { (funded, in_b) };
                    hero_title.set_label(&format!("Left to Spend in {}", m.format("%B")));
                    hero_value.set_label(&money(plan_all - spent_all));
                    hero_value.set_css_classes(&good(spent_all > plan_all));
                    hero_sub.set_label(&if plan_all.is_zero() {
                        "Set a budget below, or fill it from your spending".to_string()
                    } else if spent_all > plan_all {
                        format!("Over budget: {} spent of {}", money(spent_all), money(plan_all))
                    } else {
                        format!("{} spent of {} planned", money(spent_all), money(plan_all))
                    });
                    hero_pace.set_visible(!share.is_zero() && share < Decimal::ONE);
                    hero_pace.set_label(&format!("Heading for {} at this pace, from statements to {}", money(proj), through.format("%d %b")));
                }
            }

            // ── what the method puts between the headline and the categories
            clear(&method_box);
            if let (Some(p), true) = (&s.plan, matches!(method, Method::FiftyThirtyTwenty | Method::SeventyTwentyTen)) {
                let need_total: Decimal = s.cats.iter().filter(|c| s.needs.contains(&c.name)).filter_map(|c| bm.get(&c.id)).copied().sum::<Decimal>() + set_aside + loans;
                let want_total: Decimal = s.cats.iter().filter(|c| !s.needs.contains(&c.name)).filter_map(|c| bm.get(&c.id)).copied().sum();
                let cards = gtk::Box::builder().spacing(12).homogeneous(true).build();
                let colors = [charts::BLUE, charts::ORANGE, charts::GREEN];
                let shares = method.shares(p.method_base(), need_total, want_total, debt_extra);
                for (i, sh) in shares.iter().enumerate() {
                    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "kpi"]).build();
                    let top = gtk::Box::builder().build();
                    top.append(&gtk::Label::builder().label(sh.name).xalign(0.0).hexpand(true).css_classes(["heading"]).build());
                    top.append(&gtk::Label::builder().label(format!("Target {}%", sh.target)).css_classes(["dim-label", "caption"]).build());
                    card.append(&top);
                    card.append(&gtk::Label::builder().label(format!("{}%", sh.share)).xalign(0.0).css_classes(["value", "tnum"]).build());
                    card.append(&target_bar(f(sh.share) / 100.0, f64::from(sh.target) / 100.0, colors[i % 3]));
                    card.append(&gtk::Label::builder().label(format!("{} a month · {}", money(sh.amount), sh.note)).xalign(0.0).wrap(true).css_classes([if sh.ok { "dim-label" } else { "warning" }, "caption"]).build());
                    cards.append(&card);
                }
                method_box.append(&cards);
                if method == Method::SeventyTwentyTen && !debt_extra.is_zero() {
                    if let Some(c) = crate::wealth::payoff_outlook(&a, loans, debt_extra) {
                        let card = gtk::Box::builder().spacing(12).css_classes(["card", "kpi"]).build();
                        let t = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
                        t.append(&gtk::Label::builder().label("Paying 10% Extra").xalign(0.0).css_classes(["caption-heading"]).build());
                        t.append(&gtk::Label::builder().label(format!("Debt-free by {}", add_months(month_start(today()), c.months as i32).format("%B %Y"))).xalign(0.0).css_classes(["value"]).build());
                        if let Some(saved) = c.months_saved {
                            t.append(&dim(&format!("{saved} months sooner than the regular payments alone")));
                        }
                        card.append(&t);
                        let r = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).build();
                        r.append(&gtk::Label::builder().label("Still owed").xalign(1.0).css_classes(["dim-label", "caption"]).build());
                        r.append(&gtk::Label::builder().label(money(c.owed)).xalign(1.0).css_classes(["heading", "tnum", "amount-out"]).build());
                        card.append(&r);
                        method_box.append(&card);
                    }
                }
            }
            if let (Method::EightyTwenty, Some(p)) = (method, &s.plan) {
                let row = gtk::Box::builder().spacing(12).homogeneous(true).build();
                let weeks = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["card", "kpi"]).build();
                weeks.append(&heading(&format!("Each Week in {}", m.format("%B"))));
                let mut start = m;
                while start <= end {
                    let stop = (start + chrono::Duration::days(6)).min(end);
                    let spent: Decimal = by_category(&s.rows, start, stop, ccy, &s.fx, &s.parents).into_iter().filter(|(n, _)| variable_names.contains(&n.as_str())).map(|x| x.1).sum();
                    let line = gtk::Box::builder().build();
                    line.append(&gtk::Label::builder().label(format!("{}–{}", start.format("%-d"), stop.format("%-d %b"))).xalign(0.0).hexpand(true).build());
                    let over = spent > weekly;
                    line.append(&gtk::Label::builder().label(format!("{} of {}", money(spent), money(weekly))).css_classes(if over { ["tnum", "amount-out"] } else { ["tnum", "dim-label"] }).build());
                    weeks.append(&line);
                    let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(if weekly.is_zero() { 0.0 } else { f(spent / weekly).min(1.0) }).build();
                    if over {
                        bar.add_css_class("over");
                    }
                    weeks.append(&bar);
                    start = stop + chrono::Duration::days(1);
                }
                row.append(&weeks);
                let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
                // saved first, spent after: the one thing to do when pay lands
                let payday = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["card", "kpi"]).build();
                payday.append(&heading("On Payday"));
                let per_pay = ((p.monthly_income() - total_b - set_aside - loans - debt_extra) * Decimal::from(12) / p.cycle.per_year()).round_dp(0).max(Decimal::ZERO);
                let into = p.accounts.savings.and_then(|id| a.db.accounts().unwrap_or_default().into_iter().find(|x| x.id == id)).map_or("your savings".to_string(), |x| x.name);
                let key = format!("payday_moved:{}:{}", s.scope, m.format("%Y-%m"));
                let tick = gtk::CheckButton::builder().active(a.db.setting(&key).as_deref() == Some("1")).label(format!("Move {} to {into}", money(per_pay))).build();
                {
                    let w = Rc::downgrade(&a);
                    tick.connect_toggled(move |c| {
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_setting(&key, if c.is_active() { "1" } else { "" }));
                        }
                    });
                }
                payday.append(&tick);
                payday.append(&dim("Pay yourself first, then spend the rest."));
                side.append(&payday);
                let bills = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "kpi"]).build();
                let bills_total: Decimal = s.cats.iter().filter(|c| s.is_fixed(&c.name)).filter_map(|c| bm.get(&c.id)).copied().sum::<Decimal>() + set_aside + loans;
                let top = gtk::Box::builder().build();
                top.append(&gtk::Label::builder().label("Bills That Take Care of Themselves").xalign(0.0).hexpand(true).wrap(true).css_classes(["heading"]).build());
                top.append(&gtk::Label::builder().label(money(bills_total)).css_classes(["heading", "tnum"]).build());
                bills.append(&top);
                bills.append(&dim("Regular bills, big bills and loan payments come out before your weekly number."));
                side.append(&bills);
                row.append(&side);
                method_box.append(&row);
            }
            method_box.set_visible(method_box.first_child().is_some());

            let hist: BTreeMap<String, Vec<Decimal>> = s.cats.iter().map(|c| (c.name.clone(), core::history(&s.rows, &c.name, month_start(today()), MONTHS, ccy, &s.fx, &s.parents))).collect();
            let names: Rc<Vec<(i64, String, Decimal)>> = Rc::new(s.cats.iter().map(|c| (c.id, c.name.clone(), env.get(&c.id).map(Envelope::available).unwrap_or_default())).collect());
            let (plan_col, spent_col, left_col) = (gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal));
            let line_box = || gtk::Box::builder().spacing(12).margin_top(6).margin_bottom(6).margin_start(12).margin_end(6).build();
            let cell = |text: &str, group: &gtk::SizeGroup, classes: &[&str]| {
                let l = gtk::Label::builder().label(text).xalign(1.0).valign(gtk::Align::Center).build();
                l.set_css_classes(classes);
                group.add_widget(&l);
                l
            };
            // one category: tile, name and bar, the amount you can type, what's gone and what's left
            let cat_row = |c: &Category| -> gtk::ListBoxRow {
                let sp = now.get(&c.name).copied().unwrap_or_default();
                let b = bm.get(&c.id).copied();
                let e = env.get(&c.id).copied().unwrap_or_default();
                let rolls = rollovers.contains_key(&c.id);
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
                let line = line_box();
                line.append(&a.category_tile(&c.name));
                let name = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).valign(gtk::Align::Center).build();
                name.append(&gtk::Label::builder().label(&c.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
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
                name.append(&bar);
                line.append(&name);

                let entry = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).max_width_chars(9).xalign(1.0).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).tooltip_text("Monthly amount: press Enter to save, empty or 0 removes it").build();
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
                plan_col.add_widget(&entry);
                line.append(&entry);
                line.append(&cell(&money(sp), &spent_col, &["tnum"]));
                let (left_text, left_class) = match fb {
                    Some(b) => (
                        money(b - sp),
                        match core::alert(sp, b, elapsed(m, through)) {
                            Some(Alert::Over) => "amount-out",
                            Some(Alert::Near) => "warning",
                            None => "heading",
                        },
                    ),
                    None => ("No budget".to_string(), "dim-label"),
                };
                line.append(&cell(&left_text, &left_col, &["tnum", left_class]));

                // ⋯: everything else you can do to a category
                let menu = gtk::MenuButton::builder().icon_name("view-more-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("More for this category").build();
                let pop = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
                let popover = gtk::Popover::builder().child(&pop).build();
                if let Some(u) = u.filter(|u| Some(*u) != b) {
                    let use_ = gtk::Button::builder().label(format!("Use What It Usually Costs ({})", money(u))).css_classes(["flat"]).build();
                    let (w, sc, id, redraw, popover) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone(), popover.clone());
                    use_.connect_clicked(move |_| {
                        popover.popdown();
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_budget(&sc, id, Some((u, ccy))));
                            redraw();
                        }
                    });
                    pop.append(&use_);
                }
                let is_fixed = s.is_fixed(&c.name);
                let kind = gtk::Button::builder().label(if is_fixed { "Count as Variable Spending" } else { "Count as a Fixed Bill" }).css_classes(["flat"]).build();
                {
                    let (w, name, redraw, popover) = (Rc::downgrade(&a), c.name.clone(), redraw.clone(), popover.clone());
                    kind.connect_clicked(move |_| {
                        popover.popdown();
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_fixed(&name, !is_fixed));
                            redraw();
                        }
                    });
                }
                pop.append(&kind);
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
                line.append(&menu);
                gtk::ListBoxRow::builder().child(&line).activatable(false).tooltip_text(sub.join(" · ")).build()
            };
            let plain_row = |look: (&str, &str), name: &str, amount: Decimal| -> gtk::ListBoxRow {
                let line = line_box();
                line.append(&crate::tile_for(Some(&fingraph_core::looks::Look { icon: look.0.into(), color: look.1.into() })));
                line.append(&gtk::Label::builder().label(name).xalign(0.0).hexpand(true).build());
                line.append(&cell(&money(amount), &plan_col, &["tnum"]));
                line.append(&cell("", &spent_col, &[]));
                line.append(&cell("", &left_col, &[]));
                line.append(&gtk::Box::builder().width_request(34).build());
                gtk::ListBoxRow::builder().child(&line).activatable(false).build()
            };
            let section = |title: &str| -> gtk::ListBox {
                let head = gtk::Box::builder().spacing(12).margin_start(12).margin_end(6).build();
                head.append(&gtk::Label::builder().label(title).xalign(0.0).hexpand(true).css_classes(["heading"]).build());
                head.append(&cell(if method == Method::ZeroBased { "Assigned" } else { "Planned" }, &plan_col, &["column-head"]));
                head.append(&cell("Spent", &spent_col, &["column-head"]));
                head.append(&cell("Left", &left_col, &["column-head"]));
                head.append(&gtk::Box::builder().width_request(34).build());
                groups_box.append(&head);
                let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                groups_box.append(&lb);
                lb
            };

            // ── the categories, grouped the way the method thinks about them
            clear(&groups_box);
            let mut shown: Vec<_> = s.cats.iter().filter(|c| in_budget(c.id) || now.contains_key(&c.name) || usual.contains_key(&c.name)).collect();
            shown.sort_by_key(|c| std::cmp::Reverse(now.get(&c.name).copied().unwrap_or_default().max(bm.get(&c.id).copied().unwrap_or_default())));
            for (i, g) in method.groups().iter().enumerate() {
                let mine: Vec<_> = shown.iter().filter(|c| method.group_of(s.is_fixed(&c.name), s.needs.contains(&c.name)) == Some(i)).collect();
                if mine.is_empty() {
                    continue;
                }
                let lb = section(g);
                for c in mine {
                    lb.append(&cat_row(c));
                }
            }
            if let (Some(p), false) = (&s.plan, method == Method::EightyTwenty) {
                let saving = if method == Method::ZeroBased { p.savings_goal } else { p.monthly_savings(total_b) - set_aside };
                let lb = section("Also Planned");
                let bank = "x-office-spreadsheet-symbolic";
                for (look, name, v) in [(("security-high-symbolic", "purple"), "Big bills, set aside", set_aside), ((bank, "orange"), "Loan payments", loans), ((bank, "orange"), "Extra on debts", debt_extra), (("starred-symbolic", "green"), "Saving", saving)] {
                    if !v.is_zero() {
                        lb.append(&plain_row(look, name, v));
                    }
                }
            }
            groups_box.set_visible(groups_box.first_child().is_some());

            let out = total_s - in_b;
            outside_list.set_visible(!out.is_zero());
            outside.set_title(&glib::markup_escape_text(&format!("{} spent outside this budget", money(out))));
            outside.set_subtitle(if now.contains_key("Uncategorized") { "Some of it hasn't been given a category yet. Open To Review to sort it." } else { "In categories with no budget." });

            // ── yearly and quarterly bills, a share set aside each month
            clear(&funds_box);
            {
                funds_box.append(&info_heading(
                    "Non-Monthly Bills",
                    "Each bar shows how much has been set aside so far towards that bill. A full bar means it's covered by its due date. Suggestions under the list are yearly or quarterly charges FinGraph spotted in your statements; add one to start setting money aside for it. Each month sets a share aside so the bill is covered when it comes; that comes out of what you save in Your Plan.",
                ));
                funds_box.append(&dim(
                    "Insurance, licences, registrations, annual plans: bills that come once a quarter or once a year.",
                ));
                let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                let ideas: Vec<core::FundIdea> = a.db.big_bill_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
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
        })
    };
    *me.borrow_mut() = Some(refresh.clone());
    (page.upcast(), refresh)
}

/// A Reports tab that reads the month picked on Budget: where the money went, or how the plan stands.
pub fn build_report(app: &Rc<App>, flow_tab: bool) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let nav = gtk::Box::builder().spacing(8).build();
    let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous month").build();
    let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next month").build();
    let title = heading("");
    title.add_css_class("title-2");
    nav.append(&prev);
    nav.append(&next);
    nav.append(&title);
    body.append(&nav);
    let plan_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    let split_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    let projection = Chart::new(230);
    let flow = Chart::new(420);
    let flow_card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
    let flow_title = heading("");
    flow_card.append(&with_info(
        &flow_title,
        "Money coming in is on the left and where it went is on the right; the thicker a band, the more money. If more came in than went out, the difference shows as Saved on the right. If more went out, the gap shows as From savings on the left. Hover a band for its amount.",
    ));
    flow_card.append(&flow.widget);
    if flow_tab {
        body.append(&flow_card);
    } else {
        body.append(&plan_box);
        body.append(&split_box);
    }
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1100).child(&body).build()).vexpand(true).build();
    let me: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    for (b, step) in [(&prev, -1), (&next, 1)] {
        let me = me.clone();
        b.connect_clicked(move |_| {
            MONTH.with(|m| m.set(Some(add_months(month(), step))));
            let f = me.borrow().clone();
            if let Some(f) = f {
                f();
            }
        });
    }
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            settle_month(&a);
            let s = snap(&a);
            let (ccy, m) = (s.ccy, month());
            let end = add_months(m, 1).pred_opt().unwrap_or(m);
            let (usual, _, _) = usual(&s, m);
            let now: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, ccy, &s.fx, &s.parents).into_iter().collect();
            let env = envs(&a, &s, m);
            let bm: BTreeMap<i64, Decimal> = env.iter().filter(|(_, e)| !e.budget.is_zero()).map(|(k, e)| (*k, e.budget)).collect();
            let set_aside = funds_monthly(&s);
            let money = |d: Decimal| fmt_money(d, ccy);
            title.set_label(&m.format("%B %Y").to_string());
            next.set_sensitive(m < month_start(today()));
            let total_b: Decimal = bm.values().copied().sum();
            let hist: BTreeMap<String, Vec<Decimal>> = s.cats.iter().map(|c| (c.name.clone(), core::history(&s.rows, &c.name, month_start(today()), MONTHS, ccy, &s.fx, &s.parents))).collect();
            if flow_tab {
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
                let mut right: Vec<(String, f64, charts::Rgb)> = outs.iter().enumerate().map(|(i, (n, v))| (format!("{n}  {}", money(*v)), f(*v), charts::category_color(n).unwrap_or(charts::PALETTE[(i + 1) % charts::PALETTE.len()]))).collect();
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
            } else {
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
                    "The bar splits a month's money by colour: fixed expenses (bills that don't change much, like rent, insurance or phone), variable spending (things you choose, like groceries or eating out), non-monthly bills, loans, and what's saved. The variable part is where you have room to cut back. Switch a category from its ⋯ menu on Budget.",
                ));
                split_box.append(&dim(match income {
                    Some(_) => "How your take-home divides each month. Amounts are budgets, or the usual cost where a category has none.",
                    None => "Amounts are budgets, or the usual cost where a category has none. Set Up Budget, on the Budget page, adds your pay to see them against your take-home.",
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
                        card.append(&dim("None yet: mark categories from their ⋯ menu on Budget"));
                    }
                    card.append(&grid);
                    cards.append(&card);
                }
                split_box.append(&cards);
                rules_of_thumb(&split_box, fixed_t + set_aside + loans, var_t, income, ccy);
            }
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
const MONTH_NAMES: [&str; 12] = ["Due Jan", "Due Feb", "Due Mar", "Due Apr", "Due May", "Due Jun", "Due Jul", "Due Aug", "Due Sep", "Due Oct", "Due Nov", "Due Dec"];

const EVERY: [(i32, &str); 5] = [(12, "Once a year"), (6, "Every 6 months"), (4, "Every 4 months (termly)"), (3, "Every 3 months"), (24, "Every 2 years")];

fn every_index(months: i32) -> u32 {
    EVERY.iter().position(|e| e.0 == months).unwrap_or(0) as u32
}

/// Big bills most households have, offered on the wizard's yearly-bills step:
/// (name, hint, how often, due month if it's usually fixed).
const BIG_BILLS: [(&str, &str, i32, Option<u32>); 9] = [
    ("Car insurance", "One per vehicle: use + to add a motorcycle or another car", 12, None),
    ("Home or contents insurance", "Including hurricane cover", 12, None),
    ("HOA or strata fees", "When they're billed yearly or quarterly", 12, None),
    ("Vehicle licence & registration", "Yearly licensing and inspection, one per vehicle (+ for another)", 12, None),
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
    /// You said you cancelled it: dropped from the budget, and not suggested again.
    cancelled: Rc<Cell<bool>>,
    /// You said it's really monthly: not a big bill at all.
    monthly: Rc<Cell<bool>>,
    /// Setup asked whether it's monthly; saving without saying so is a no.
    asked: bool,
    /// The subscription it is, if it's one: where a monthly answer is recorded.
    sub_stem: Option<String>,
    /// How often it comes is a guess, so a "no" also records the cycle.
    guessed: bool,
}

impl BillLine {
    fn amount(&self) -> Option<Decimal> {
        if self.cancelled.get() || self.monthly.get() {
            return None;
        }
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
    /// A need rather than a want (the budgeting methods split by it).
    need: gtk::ToggleButton,
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

/// "A savings account outside FinGraph" in the account roles.
pub(crate) const OUTSIDE: i64 = -1;

/// What's owed on each card and loan (latest statement balance), in the display currency, most first.
pub(crate) fn owed(app: &App, s: &Snap) -> Vec<(i64, Decimal)> {
    let docs = app.db.documents(&app.entity_ids()).unwrap_or_default();
    let accts = app.db.accounts().unwrap_or_default();
    let mut v: Vec<(i64, Decimal)> = fingraph_core::insights::latest_balances(&s.rows, &docs)
        .into_iter()
        .filter_map(|(id, (_, b))| {
            let a = accts.iter().find(|a| a.id == id && !a.hidden)?;
            (a.account_type.is_liability() && b < Decimal::ZERO).then(|| (id, s.fx.convert(-b, a.currency, s.ccy).round_dp(2)))
        })
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

/// First guesses at which account does what: pay lands where most income does; everyday spending
/// comes from the account with the most spending charges, when that's another one (a card).
fn guess_roles(s: &Snap, accts: &[(i64, String)]) -> Roles {
    let since = add_months(month_start(today()), -4);
    let mut income: BTreeMap<i64, Decimal> = BTreeMap::new();
    let mut charges: BTreeMap<i64, usize> = BTreeMap::new();
    for t in s.rows.iter().filter(|t| t.posted >= since && !t.is_transfer() && accts.iter().any(|a| a.0 == t.account_id)) {
        match t.category_kind {
            Some(fingraph_core::db::CategoryKind::Income) if t.amount.is_sign_positive() => *income.entry(t.account_id).or_default() += t.amount,
            Some(fingraph_core::db::CategoryKind::Expense) if t.amount.is_sign_negative() => *charges.entry(t.account_id).or_default() += 1,
            _ => {}
        }
    }
    let pay = income.iter().max_by_key(|x| *x.1).map(|x| *x.0);
    let spending = charges.iter().max_by_key(|x| *x.1).map(|x| *x.0).filter(|a| Some(*a) != pay);
    // bills: the account most regular-bill money comes out of
    let names: BTreeMap<String, i64> = accts.iter().map(|(id, n)| (n.clone(), *id)).collect();
    let mut by: BTreeMap<i64, Decimal> = BTreeMap::new();
    for b in fingraph_core::plan_doc::running_bills(&s.rows, &s.fixed, &s.parents, s.ccy, &s.fx, today(), today()) {
        if let Some(id) = names.get(&b.account) {
            *by.entry(*id).or_default() += b.amount;
        }
    }
    let bills = by.iter().max_by_key(|x| *x.1).map(|x| *x.0).filter(|a| Some(*a) != pay);
    Roles { pay, bills, spending, ..Default::default() }
}

/// Seven steps: pay & deductions → other money in and out → big yearly bills → method → spending → savings → accounts
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
        "Each category starts at what you usually spend (the median of your last six months), the budget you already have, or what your method gives it. Change any of them; leave one empty for no budget. Mark each a Need or a Want: methods keep needs whole and trim wants. Fixed expenses are marked (change which on the Budget page).",
    );
    // the method's figures again, after changes to needs and wants
    let refill = gtk::Button::builder().label("Fill Again by My Method").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).tooltip_text("Put every category back to what your method gives it").build();
    b3.append(&refill);
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
                // need or want: the methods keep needs whole and trim wants
                let is_need = s.needs.contains(&c.name);
                let need = gtk::ToggleButton::builder().label(if is_need { "Need" } else { "Want" }).active(is_need).valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("A need is something you'd pay even in a lean month (rent, groceries); a want is what you could cut (eating out). Click to switch.").build();
                need.connect_toggled(|b| b.set_label(if b.is_active() { "Need" } else { "Want" }));
                r.add_suffix(&need);
                r.add_suffix(&e);
                lb.append(&r);
                CatLine { cat: c.clone(), entry: e, row: r, usual: u, memo: RefCell::default(), need }
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
    // monthly subscriptions, each one you can mark cancelled: it comes off Subscriptions & Digital
    let sub_list: Vec<fingraph_core::insights::Subscription> = fingraph_core::insights::subscriptions(&s.rows, ccy, &s.fx, today())
        .into_iter()
        .filter(|x| x.cadence_days < BILL_DAYS && (x.active || (x.cancelled && (today() - x.last_charged).num_days() <= 2 * x.cadence_days + 7)))
        .collect();
    let sub_marks: Rc<Vec<(String, bool, gtk::ToggleButton)>> = Rc::new(if sub_list.is_empty() {
        Vec::new()
    } else {
        let g = adw::PreferencesGroup::builder()
            .title("Your Subscriptions")
            .description("What's in Subscriptions & Digital above. Mark any you've cancelled and its cost comes off. If it charges again, FinGraph counts it again.")
            .build();
        let marks = sub_list
            .iter()
            .map(|x| {
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&x.payee))
                    .subtitle(format!("{} a month · last charged {}", money(x.monthly), x.last_charged.format("%d %b %Y")))
                    .build();
                let b = gtk::ToggleButton::builder().label("Cancelled").active(x.cancelled).valign(gtk::Align::Center).tooltip_text("I've cancelled this").build();
                r.add_suffix(&b);
                g.add(&r);
                let (cat_entries, fortnightly, monthly, row) = (cat_entries.clone(), fortnightly.clone(), x.monthly, r.clone());
                b.connect_toggled(move |b| {
                    row.set_opacity(if b.is_active() { 0.55 } else { 1.0 });
                    let Some(l) = cat_entries.iter().find(|l| l.cat.name == SUBS) else { return };
                    let k = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
                    let now = parse_user(&l.entry.text()).unwrap_or_default().abs();
                    let step = (monthly * k).round_dp(2);
                    *l.memo.borrow_mut() = None;
                    l.entry.set_text(&amount_text(if b.is_active() { (now - step).max(Decimal::ZERO) } else { now + step }));
                });
                if x.cancelled {
                    r.set_opacity(0.55);
                }
                (x.stem.clone(), x.cancelled, b)
            })
            .collect();
        b3.append(&g);
        marks
    });
    let spend_total = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b3.append(&spend_total);

    // ── 4. the method
    let (p_m, b_m) = step_page(
        "Choose Your Method",
        "A budgeting method decides how your money is divided; FinGraph then fills in your budget from it, using what you really spend. Your pension counts toward saving. You can still change any amount on the next step.",
    );
    let method = Rc::new(Cell::new(start.method));
    let applied: Rc<Cell<Option<fingraph_core::method::Method>>> = Rc::new(Cell::new(Some(start.method)));
    let mg = adw::PreferencesGroup::new();
    let mut first: Option<gtk::CheckButton> = None;
    let mut method_checks: Vec<gtk::CheckButton> = Vec::new();
    for mth in fingraph_core::method::Method::ALL {
        let check = gtk::CheckButton::builder().active(mth == start.method).valign(gtk::Align::Center).build();
        if let Some(f) = &first {
            check.set_group(Some(f));
        } else {
            first = Some(check.clone());
        }
        let r = adw::ActionRow::builder().title(mth.label()).subtitle(mth.about()).subtitle_lines(4).activatable_widget(&check).build();
        r.add_prefix(&check);
        mg.add(&r);
        method_checks.push(check.clone());
        let method = method.clone();
        check.connect_toggled(move |c| {
            if c.is_active() {
                method.set(mth);
            }
        });
    }
    b_m.append(&mg);
    // the screenshot harness: FINGRAPH_METHOD=1 picks the second method
    #[cfg(debug_assertions)]
    if let Some(i) = std::env::var("FINGRAPH_METHOD").ok().and_then(|v| v.parse::<usize>().ok()) {
        if let Some(c) = method_checks.get(i) {
            c.set_active(true);
        }
        applied.set(None);
    }
    let preview = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    preview.set_margin_top(4);
    b_m.append(&preview);
    let method_notes = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b_m.append(&method_notes);
    let owed_list = owed(app, &s);
    let owed_total: Decimal = owed_list.iter().map(|x| x.1).sum();

    // ── 6. savings
    let (p4, b4) = step_page(
        "Your Savings Goal",
        "How much you want to put away each month — an amount, or a percent of take-home (20%). FinGraph does the arithmetic: take-home − spending − big yearly bills − loans = what's left, and if that's short of the goal, how much the variable categories need to shrink.",
    );
    let g4 = adw::PreferencesGroup::new();
    let goal = adw::EntryRow::builder().title(format!("Save per month ({} or %)", ccy.symbol())).text(amount_text(start.savings_goal)).build();
    g4.add(&goal);
    let debt = adw::EntryRow::builder().title(format!("Pay down debt per month, on top of the minimums ({})", ccy.symbol())).text(amount_text(start.debt_extra)).build();
    g4.add(&debt);
    if owed_total > Decimal::ZERO {
        b4.append(&dim(&format!("You owe {} on cards and loans (latest statements). Extra paid here clears it faster; set where it goes on the next step.", money(owed_total))));
    }
    b4.append(&g4);
    let maths = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    maths.set_margin_top(4);
    b4.append(&maths);
    let scale = gtk::Button::builder().label("Trim Variable Categories to Reach It").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).build();
    b4.append(&scale);
    // zero-based: whatever's not given a job yet
    let rest = gtk::Button::builder().label("Put the Rest in Savings").halign(gtk::Align::Start).css_classes(["pill", "suggested-action"]).visible(false).build();
    b4.append(&rest);
    let unassigned: Rc<Cell<Decimal>> = Rc::default();
    let ahead = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b4.append(&ahead);

    // read every field into a plan + budgets, and redo the sums
    // ── 7. where the money goes: which account does what, for the payday checklist
    let (p5, b5) = step_page(
        "Where Your Money Goes",
        "Which account your pay lands in and what each part of it is for. The Budget Plan turns this into a checklist for every payday: move this much here, leave this much there.",
    );
    let accts: Vec<(i64, String)> = app.db.accounts().unwrap_or_default().into_iter().filter(|x| !x.hidden && s.rows.iter().any(|t| t.account_id == x.id)).map(|x| (x.id, x.name)).collect();
    let mut roles = if start.accounts == Default::default() { guess_roles(&s, &accts) } else { start.accounts.clone() };
    if roles.debt.is_none() {
        roles.debt = owed_list.first().map(|x| x.0);
    }
    let g5 = adw::PreferencesGroup::new();
    // (title, help, choices beyond the accounts, current)
    let role_row = |title: &str, sub: &str, same: &str, outside: bool, cur: Option<i64>| {
        let mut ids: Vec<Option<i64>> = vec![None];
        let mut names: Vec<String> = vec![same.to_string()];
        for (id, n) in &accts {
            ids.push(Some(*id));
            names.push(n.clone());
        }
        if outside {
            ids.push(Some(OUTSIDE));
            names.push("A savings account outside FinGraph".into());
        }
        let r = adw::ComboRow::builder().title(title).subtitle(sub).model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())).build();
        r.set_selected(ids.iter().position(|i| *i == cur).unwrap_or(0) as u32);
        g5.add(&r);
        (r, ids)
    };
    let r_pay = role_row("Pay goes into", "Where your salary is deposited", "Not set", false, roles.pay);
    let r_bills = role_row("Bills are paid from", "Fixed bills and loan payments", "The pay account", false, roles.bills);
    let r_spend = role_row("Everyday spending from", "Groceries, eating out and the other variable categories (a card, say)", "The pay account", false, roles.spending);
    let r_big = role_row("Big-bill money is kept in", "Where the monthly share for yearly bills builds up", "The pay account", true, roles.big_bills);
    let r_save = role_row("Savings go to", "Where the monthly savings go", "The pay account", true, roles.savings);
    let r_debt = role_row("Extra debt payments go to", "The card or loan to pay down first (the most owed is picked first)", "Not set", false, roles.debt);
    b5.append(&g5);
    b5.append(&dim("A card used for everyday spending gets the money to pay it off: the checklist moves that much to it each payday. Pick “the pay account” when you pay straight from there."));
    let read_roles = {
        let rows = [r_pay.clone(), r_bills.clone(), r_spend.clone(), r_big.clone(), r_save.clone(), r_debt.clone()];
        move || {
            let v: Vec<Option<i64>> = rows.iter().map(|(r, ids)| ids.get(r.selected() as usize).copied().flatten()).collect();
            Roles { pay: v[0], bills: v[1], spending: v[2], big_bills: v[3], savings: v[4], debt: v[5] }
        }
    };

    let read = {
        let read_roles = read_roles.clone();
        let (cycle, next_pay, gross, deds, other_in, loans, goal, cat_entries, fortnightly) = (cycle.clone(), next_pay.clone(), gross.clone(), deds.clone(), other_in.clone(), loans.clone(), goal.clone(), cat_entries.clone(), fortnightly.clone());
        let (method, debt) = (method.clone(), debt.clone());
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
                accounts: read_roles(),
                method: method.get(),
                debt_extra: parse_user(&debt.text()).unwrap_or_default().abs(),
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
    // the share of the big bills set aside each month, in the display currency
    let aside_of = {
        let (bills, fx) = (bills.clone(), s.fx.clone());
        Rc::new(move || -> Decimal { bills.borrow().iter().filter_map(|l| Some(fx.convert(l.amount()? / Decimal::from(l.every()), l.currency, ccy))).sum::<Decimal>().round_dp(2) })
    };
    // each category as a method sees it: need or want, and what it usually costs (or its budget)
    let cat_ins = {
        let cat_entries = cat_entries.clone();
        Rc::new(move || -> Vec<fingraph_core::method::CatIn> {
            cat_entries.iter().map(|l| fingraph_core::method::CatIn { name: l.cat.name.clone(), need: l.need.is_active(), usual: l.usual.or_else(|| l.monthly(false)).unwrap_or_default() }).collect()
        })
    };
    let recalc: Rc<dyn Fn()> = {
        let (read, take_home, spend_total, maths, ahead, scale, cat_entries, factor, fixed_names) = (read.clone(), take_home.clone(), spend_total.clone(), maths.clone(), ahead.clone(), scale.clone(), cat_entries.clone(), factor.clone(), fixed_names.clone());
        let (bills, bills_total, fx) = (bills.clone(), bills_total.clone(), s.fx.clone());
        let (aside_of, cat_ins, preview, method_notes, rest, unassigned, refill) = (aside_of.clone(), cat_ins.clone(), preview.clone(), method_notes.clone(), rest.clone(), unassigned.clone(), refill.clone());
        Rc::new(move || {
            let (p, budgets) = read();
            let aside = aside_of();
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
                format!("− Paying off debt            {:>14}", money(p.debt_extra)),
                format!("= Left to save               {:>14}", money(save)),
            ];
            let pension = p.pension_monthly();
            if pension > Decimal::ZERO {
                lines.push(format!("  + Pension (saved for you)  {:>14}", money(pension)));
            }
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
            // zero-based: every dollar gets a job, so what's left after the goal must come to nothing
            let zero = p.method == fingraph_core::method::Method::ZeroBased;
            let left = save - p.savings_goal;
            unassigned.set(left);
            rest.set_visible(zero && left.abs() >= Decimal::ONE);
            if zero {
                lines.push(if left.abs() < Decimal::ONE {
                    "\nZero-based: every dollar has a job.".to_string()
                } else if left > Decimal::ZERO {
                    format!("\nZero-based: {} still has no job. Give it to a category or to savings.", money(left))
                } else {
                    format!("\nZero-based: {} more is given out than comes in. Take it from a category or the goal.", money(-left))
                });
            }
            maths.set_label(&lines.join("\n"));

            // the method step: what the chosen method means with your numbers
            let mth = p.method;
            refill.set_visible(mth.split().is_some());
            let base = p.method_base();
            let ins = cat_ins();
            let needs_u: Decimal = ins.iter().filter(|c| c.need).map(|c| c.usual).sum::<Decimal>() + aside + p.loan_payments;
            let wants_u: Decimal = ins.iter().filter(|c| !c.need).map(|c| c.usual).sum();
            let pc = |d: Decimal| fingraph_core::method::share(d, base);
            let mut pv = vec![
                format!("  Take-home + pension a month {:>14}", money(base)),
                format!("  Needs usually (with big bills, loans) {:>8}  {:>3}%", money(needs_u), pc(needs_u)),
                format!("  Wants usually               {:>14}  {:>3}%", money(wants_u), pc(wants_u)),
            ];
            match fingraph_core::method::allocate(mth, base, pension, aside, p.loan_payments, owed_total, &ins, &money) {
                Some(a) => {
                    let wants: Decimal = a.budgets.iter().zip(&ins).filter(|(_, c)| !c.need).map(|(b, _)| b.1).sum();
                    pv.push(format!("\n{} gives:", mth.label()));
                    if let (Some(n), Some(w)) = (a.target_needs, a.target_wants) {
                        pv.push(format!("  Needs up to                 {:>14}", money(n)));
                        pv.push(format!("  Wants up to                 {:>14}  → {}", money(w), money(wants)));
                    } else {
                        pv.push(format!("  All spending up to          {:>14}", money(a.target_spend)));
                        pv.push(format!("  Wants get                   {:>14}", money(wants)));
                    }
                    pv.push(format!("  Saving at least             {:>14}  → {} + pension {}", money(a.target_save), money(a.savings), money(pension)));
                    if a.target_debt > Decimal::ZERO {
                        pv.push(format!("  Paying off debt             {:>14}  → {}", money(a.target_debt), money(a.debt)));
                    }
                    method_notes.set_label(&a.notes.join("\n"));
                }
                None => {
                    method_notes.set_label(if mth == fingraph_core::method::Method::ZeroBased {
                        "Zero-based starts from what you usually spend; on the savings step, give every dollar left a job until it comes to zero."
                    } else {
                        "Your own numbers: each category starts at what it usually costs, and you set the savings goal."
                    });
                }
            }
            preview.set_label(&pv.join("\n"));
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
    type AddBill = Rc<dyn Fn(BillLine, &str, Option<i32>)>;
    let adder: Rc<RefCell<Option<AddBill>>> = Rc::default();
    // the amount and both drop-downs line up from row to row
    let widths = [gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal)];
    let add_bill: AddBill = {
        let (bills, bill_list, recalc, adder) = (bills.clone(), bill_list.clone(), recalc.clone(), adder.clone());
        let (cat_entries, fortnightly, fx) = (cat_entries.clone(), fortnightly.clone(), s.fx.clone());
        Rc::new(move |l: BillLine, hint: &str, at: Option<i32>| {
            // a bill you have or that was seen in your statements gets a second line: what FinGraph
            // knows or is asking, and the buttons to answer
            let answers = l.id != 0 || !l.stem.is_empty();
            let r = adw::ActionRow::builder().title(glib::markup_escape_text(&l.title)).subtitle(if answers { String::new() } else { glib::markup_escape_text(hint).to_string() }).build();
            let note_label = gtk::Label::builder().label(hint).xalign(0.0).hexpand(true).wrap(true).css_classes(if l.asked { ["caption", "warning"] } else { ["caption", "dim-label"] }).build();
            let second = gtk::Box::builder().spacing(8).margin_top(4).margin_bottom(8).margin_start(12).margin_end(12).build();
            second.append(&note_label);
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
            // one seen in your statements may really be monthly: FinGraph can be wrong about that
            let monthly_btn = (!l.stem.is_empty()).then(|| {
                let m = gtk::ToggleButton::builder().label(if l.asked { "Yes, Monthly" } else { "Monthly" }).valign(gtk::Align::Center).tooltip_text("This is charged every month: count it with everyday spending and don't set money aside for it").build();
                let (flag, rc, label, hint, widgets) = (l.monthly.clone(), recalc.clone(), note_label.clone(), hint.to_string(), [l.amount.clone().upcast::<gtk::Widget>(), l.every.clone().upcast(), l.month.clone().upcast()]);
                let (amount, cur, is_sub, cats, fortnightly, fx) = (l.amount.clone(), l.currency, l.sub_stem.is_some(), cat_entries.clone(), fortnightly.clone(), fx.clone());
                // what this row put into Subscriptions & Digital, so pressing again takes out the same
                let added: Rc<Cell<Decimal>> = Rc::default();
                m.connect_toggled(move |m| {
                    let on = m.is_active();
                    flag.set(on);
                    for w in &widgets {
                        w.set_sensitive(!on);
                    }
                    let subs = cats.iter().find(|c| c.cat.name == SUBS).filter(|_| is_sub);
                    if let Some(c) = subs {
                        let per = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
                        let now = parse_user(&c.entry.text()).unwrap_or_default();
                        let change = if on { (fx.convert(parse_user(&amount.text()).unwrap_or_default().abs(), cur, ccy) * per).round_dp(2) } else { added.get() };
                        added.set(if on { change } else { Decimal::ZERO });
                        c.entry.set_text(&amount_text(if on { now + change } else { (now - change).max(Decimal::ZERO) }));
                    }
                    let note = if !on {
                        hint.clone()
                    } else if subs.is_some() {
                        format!("Monthly: added to {SUBS} on the Your Spending step, and not set aside for")
                    } else {
                        "Monthly: counted with your everyday spending, and not set aside for".to_string()
                    };
                    label.set_label(&note);
                    rc();
                });
                second.append(&m);
                m
            });
            // one you had (saved, or seen in your statements) can be marked cancelled
            if l.id != 0 || !l.stem.is_empty() {
                let c = gtk::ToggleButton::builder().label("Cancelled").valign(gtk::Align::Center).tooltip_text("I've cancelled this: drop it from the budget and stop suggesting it (unless it charges again)").build();
                let (flag, rc, row, label, hint, widgets) = (l.cancelled.clone(), recalc.clone(), r.clone(), note_label.clone(), hint.to_string(), [l.amount.clone().upcast::<gtk::Widget>(), l.every.clone().upcast(), l.month.clone().upcast()]);
                c.connect_toggled(move |c| {
                    let on = c.is_active();
                    flag.set(on);
                    for w in &widgets {
                        w.set_sensitive(!on);
                    }
                    row.set_opacity(if on { 0.55 } else { 1.0 });
                    label.set_label(if on { "Cancelled: not budgeted, and not suggested again unless it charges again" } else { &hint });
                    rc();
                });
                if let Some(m) = &monthly_btn {
                    // one or the other, not both
                    m.bind_property("active", &c, "sensitive").invert_boolean().sync_create().build();
                    c.bind_property("active", m, "sensitive").invert_boolean().sync_create().build();
                }
                second.append(&c);
            }
            // another of the same kind: a second vehicle's insurance, another child's fees
            let more = gtk::Button::builder().icon_name("list-add-symbolic").valign(gtk::Align::Center).css_classes(["flat", "circular"]).tooltip_text("Add another like this (another vehicle, say)").build();
            {
                let (adder, row, title, cur, every) = (adder.clone(), r.clone(), l.name(), l.currency, l.every.clone());
                more.connect_clicked(move |_| {
                    let Some(add) = adder.borrow().clone() else { return };
                    let ev = EVERY[(every.selected() as usize).min(EVERY.len() - 1)].0;
                    let name = gtk::Entry::builder().placeholder_text("What is it? (e.g. Motorcycle insurance)").text(format!("{title} 2")).build();
                    let new = BillLine {
                        id: 0,
                        stem: String::new(),
                        currency: cur,
                        title: String::new(),
                        name: Some(name),
                        amount: gtk::Entry::builder().placeholder_text(cur.symbol()).width_chars(9).max_width_chars(9).hexpand(false).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).tooltip_text("What it costs each time").build(),
                        every: gtk::DropDown::builder().model(&gtk::StringList::new(&EVERY.map(|e| e.1))).valign(gtk::Align::Center).tooltip_text("How often it comes").selected(every_index(ev)).build(),
                        month: gtk::DropDown::builder().model(&gtk::StringList::new(&MONTH_NAMES)).valign(gtk::Align::Center).tooltip_text("The month it's next due").selected(add_months(month_start(today()), 1).month() - 1).build(),
                        due: None,
                        cancelled: Rc::default(),
                        monthly: Rc::default(),
                        asked: false,
                        sub_stem: None,
                        guessed: false,
                    };
                    add(new, "Rename it to say which (Motorcycle insurance, Wife's SUV insurance)", Some(row.index() + if answers { 2 } else { 1 }));
                });
            }
            r.add_suffix(&more);
            match at {
                Some(i) => bill_list.insert(&r, i),
                None => bill_list.append(&r),
            }
            if answers {
                let row2 = gtk::ListBoxRow::builder().child(&second).activatable(false).selectable(false).build();
                match at {
                    Some(i) => bill_list.insert(&row2, i + 1),
                    None => bill_list.append(&row2),
                }
            }
            bills.borrow_mut().push(l);
            recalc();
        })
    };
    *adder.borrow_mut() = Some(add_bill.clone());
    let months = MONTH_NAMES;
    let line = move |id: i64, stem: &str, cur: Currency, title: &str, typed: bool, amount: Decimal, every: i32, due: Option<NaiveDate>, month: u32| {
        let a = gtk::Entry::builder().placeholder_text(cur.symbol()).width_chars(9).max_width_chars(9).hexpand(false).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).text(amount_text(amount)).tooltip_text("What it costs each time").build();
        let ev = gtk::DropDown::builder().model(&gtk::StringList::new(&EVERY.map(|e| e.1))).valign(gtk::Align::Center).tooltip_text("How often it comes").build();
        ev.set_selected(every_index(every));
        let mo = gtk::DropDown::builder().model(&gtk::StringList::new(&months)).valign(gtk::Align::Center).tooltip_text("The month it's next due").build();
        mo.set_selected(month.clamp(1, 12) - 1);
        let name = typed.then(|| gtk::Entry::builder().placeholder_text("What is it?").text(title).build());
        BillLine { id, stem: stem.to_string(), currency: cur, title: title.to_string(), name, amount: a, every: ev, month: mo, due, cancelled: Rc::default(), monthly: Rc::default(), asked: false, sub_stem: None, guessed: false }
    };
    let next_month = add_months(month_start(today()), 1).month();
    // is it really a big bill? ask when the charge is small, or how often it comes is only a guess
    let all_subs = fingraph_core::insights::subscriptions(&s.rows, ccy, &s.fx, today());
    let answered = app.db.kept_big_bills();
    let question = |l: &mut BillLine, name: &str, amount: Decimal, stated: String| -> String {
        let sub = all_subs.iter().find(|x| x.stem == l.stem || fingraph_core::rules::payer_key(&x.stem, "") == l.stem || x.payee.eq_ignore_ascii_case(name));
        l.sub_stem = sub.map(|x| x.stem.clone());
        l.guessed = sub.is_some_and(|x| x.cadence_guessed);
        l.asked = !l.stem.is_empty() && core::ask_if_monthly(l.guessed, amount, answered.contains(&l.stem));
        match (l.asked, l.guessed) {
            (true, true) => format!("{stated}. Is this a monthly charge? FinGraph hasn't seen enough charges to tell"),
            (true, false) => format!("{stated}. Is this a monthly charge? It's small for a big bill"),
            _ => stated,
        }
    };
    for f in &s.funds {
        let d = f.due(today());
        let mut l = line(f.id, &f.stem, f.currency, &f.name, false, f.amount, f.every_months, Some(d), d.month());
        let hint = question(&mut l, &f.name, s.fx.convert(f.amount, f.currency, ccy), format!("Already planned · {}", fmt_money(f.amount, f.currency)));
        add_bill(l, &hint, None);
    }
    let ideas: Vec<core::FundIdea> = app.db.big_bill_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
    for i in &ideas {
        let mut l = line(0, &i.stem, ccy, &i.payee, false, i.amount.round_dp(0), i.every_months, Some(i.next_due), i.next_due.month());
        let hint = question(&mut l, &i.payee, i.amount, format!("Seen in your statements: {} {}", money(i.amount), if i.every_months == 12 { "once a year" } else { "every 3 months" }));
        add_bill(l, &hint, None);
    }
    let taken: Vec<String> = s.funds.iter().map(|f| f.name.to_lowercase()).chain(ideas.iter().map(|i| i.payee.to_lowercase())).collect();
    for (name, hint, every, month) in BIG_BILLS {
        if !taken.contains(&name.to_lowercase()) {
            add_bill(line(0, "", ccy, name, false, Decimal::ZERO, every, None, month.unwrap_or(next_month)), hint, None);
        }
    }
    {
        let add = add_bill.clone();
        add_bill_btn.connect_clicked(move |_| add(line(0, "", ccy, "", true, Decimal::ZERO, 12, None, next_month), "", None));
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

    // fill the budget by the chosen method (needs whole, wants scaled, savings and debt their share)
    let apply = {
        let (read, aside_of, cat_ins, cat_entries, goal, debt, monthly_b) = (read.clone(), aside_of.clone(), cat_ins.clone(), cat_entries.clone(), goal.clone(), debt.clone(), monthly_b.clone());
        Rc::new(move |m: fingraph_core::method::Method| {
            let (p, _) = read();
            let Some(a) = fingraph_core::method::allocate(m, p.method_base(), p.pension_monthly(), aside_of(), p.loan_payments, owed_total, &cat_ins(), &money) else { return };
            // the method's figures are monthly
            monthly_b.set_active(true);
            for (l, (_, b)) in cat_entries.iter().zip(&a.budgets) {
                *l.memo.borrow_mut() = None;
                l.entry.set_text(&amount_text(*b));
            }
            goal.set_text(&amount_text(a.savings));
            debt.set_text(&amount_text(a.debt));
        })
    };
    for c in &method_checks {
        let r = recalc.clone();
        c.connect_toggled(move |c| {
            if c.is_active() {
                r();
            }
        });
    }
    for l in cat_entries.iter() {
        let r = recalc.clone();
        l.need.connect_toggled(move |_| r());
    }
    {
        let r = recalc.clone();
        debt.connect_changed(move |_| r());
    }
    {
        let (apply, method) = (apply.clone(), method.clone());
        refill.connect_clicked(move |_| apply(method.get()));
    }
    {
        let (goal, unassigned) = (goal.clone(), unassigned.clone());
        rest.connect_clicked(move |_| {
            let now = parse_amount(&goal.text()).filter(|x| !x.1).map_or(Decimal::ZERO, |x| x.0);
            goal.set_text(&amount_text((now + unassigned.get()).max(Decimal::ZERO)));
        });
    }
    recalc();

    // ── the frame: Back / Next / Save
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let pages = [p1, p2, p_y, p_m, p3, p4, p5];
    // the method step and the savings step, by position
    let (method_step, savings_step) = (3usize, 5usize);
    let last = pages.len() - 1;
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
            fwd.set_label(if i == last { "Save Budget" } else { "Next" });
            dlg.set_title(&format!("Set Up Your Budget · {} of {}", i + 1, last + 1));
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
            // leaving the method step: fill the budget by it, once per choice
            if step.get() == method_step && applied.get() != Some(method.get()) {
                apply(method.get());
                applied.set(Some(method.get()));
            }
            if step.get() < last {
                return show(step.get() + 1);
            }
            if method.get() == fingraph_core::method::Method::ZeroBased && unassigned.get().abs() >= Decimal::ONE {
                show(savings_step);
                return a.toast(&format!("Zero-based: {} isn't given a job yet", fmt_money(unassigned.get(), ccy)));
            }
            let (p, budgets) = read();
            // needs and wants, as marked
            for l in cat_entries.iter() {
                if l.need.is_active() != s.needs.contains(&l.cat.name) {
                    a.saved(a.db.set_need(&l.cat.name, l.need.is_active()));
                }
            }
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
            // what you cancelled, remembered so it's not suggested or counted again
            let mut prefs = a.db.prefs();
            let before = (prefs.cancelled.clone(), prefs.subscription_cadence.clone());
            let day = today().format("%Y-%m-%d").to_string();
            for l in bills.borrow().iter().filter(|l| l.cancelled.get() && !l.stem.is_empty()) {
                prefs.cancelled.insert(l.stem.clone(), day.clone());
            }
            // your answer to "is it monthly?" replaces FinGraph's guess, here and on every later screen
            for l in bills.borrow().iter().filter(|l| !l.cancelled.get() && !l.stem.is_empty()) {
                if l.monthly.get() {
                    a.saved(a.db.mark_not_big_bill(&l.stem));
                    if let Some(st) = &l.sub_stem {
                        prefs.subscription_cadence.insert(st.clone(), 30);
                    }
                } else if l.asked && l.amount().is_some() {
                    a.saved(a.db.keep_big_bill(&l.stem));
                    if let (true, Some(st)) = (l.guessed, &l.sub_stem) {
                        prefs.subscription_cadence.insert(st.clone(), (l.every() * 3044 + 50) as i64 / 100);
                    }
                }
            }
            for (stem, was, b) in sub_marks.iter() {
                match (b.is_active(), *was) {
                    (true, false) => {
                        prefs.cancelled.insert(stem.clone(), day.clone());
                    }
                    (false, true) => {
                        prefs.cancelled.remove(stem);
                    }
                    _ => {}
                }
            }
            if (prefs.cancelled.clone(), prefs.subscription_cadence.clone()) != before {
                a.saved(a.db.save_prefs(&prefs));
            }
            dlg.close();
            done();
            a.toast_action("Budget saved", "Save Budget Plan…", Rc::new(|a: &Rc<App>| crate::planpdf::save_plan(a)));
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
