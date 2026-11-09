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

mod setup;
pub use setup::wizard;
pub(crate) use setup::{amount_text, owed, parse_date, step_page, OUTSIDE};
use setup::{EVERY, every_index};

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
    let budgets = app.db.budgets(&sc).unwrap_or_default().into_iter().map(|(c, (v, k))| (c, fx.convert(v, k, ccy).unwrap_or_default())).collect();
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
    s.funds.iter().map(|f| s.fx.convert(f.monthly(), f.currency, s.ccy).unwrap_or_default()).sum()
}

/// A saved plan's amounts in the display currency (percent deductions stay percent).
fn plan_in(mut p: Plan, ccy: Currency, fx: &Fx) -> Plan {
    let from = p.currency.unwrap_or(ccy);
    let c = |d: Decimal| fx.convert(d, from, ccy).unwrap_or_default().round_dp(2);
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

/// Oldest available statement end for planning. The coverage label separately identifies
/// accounts without verified history; this date alone never proves aggregate completeness.
pub(crate) fn data_through(app: &App) -> NaiveDate {
    let coverage = app.db.statement_coverage(&app.entity_ids()).unwrap_or_default();
    coverage.common_end().or_else(|| coverage.accounts.iter().filter_map(|a| a.verified_end).min())
        .or_else(|| app.db.documents(&app.entity_ids()).ok()?.iter().filter_map(|d| d.period.map(|p| p.1)).min())
        .unwrap_or(today()).min(today())
}

/// Fraction through the planning month at the oldest available source date.
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
        let limit = s.fx.convert(wl.limit, wl.currency, s.ccy).unwrap_or_default();
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
    Some((fd.name.clone(), fd.due(t), s.fx.convert(fd.amount, fd.currency, s.ccy).unwrap_or_default(), s.fx.convert(fd.set_aside(t), fd.currency, s.ccy).unwrap_or_default()))
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
        "Your next big bills, soonest first. The bar shows the savings target by now, not money deposited. Recorded allocations are shown separately. Change them on the Budget page under Non-Monthly Bills.",
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
        let (aside, whole) = (s.fx.convert(fd.set_aside(t), fd.currency, ccy).unwrap_or_default(), s.fx.convert(fd.amount, fd.currency, ccy).unwrap_or_default());
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(170).build();
        col.append(&gtk::Label::builder().label(format!("Target: {} of {}", money(aside), money(whole))).xalign(1.0).css_classes(["tnum", "caption"]).build());
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
                let v = s.fx.convert(f.amount, f.currency, ccy).unwrap_or_default();
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
    let monthly: Decimal = s.funds.iter().map(|f| s.fx.convert(f.monthly(), f.currency, ccy).unwrap_or_default()).sum();
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
                .map(|t| -s.fx.convert(t.amount, t.currency, s.ccy).unwrap_or_default())
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
            settle_month(&a);
            let coverage = a.db.statement_coverage(&ids).unwrap_or_default();
            proven.set_visible(true);
            proven_text.set_label(&coverage.label());
            proven_text.set_tooltip_text(Some(&coverage.details()));

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
                            raw.savings_goal += a.db.fx().unwrap_or_default().convert(left, ccy, to).unwrap_or_default().round_dp(2);
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

                let entry = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).max_width_chars(9).xalign(1.0).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).tooltip_text("Monthly amount: saves on Enter or when you leave this field; empty or 0 removes it").build();
                if let Some(b) = b {
                    entry.set_text(&b.round_dp(2).normalize().to_string());
                }
                entry.set_widget_name(&format!("budget:{}", c.name));
                entry.update_property(&[gtk::accessible::Property::Label(&format!("{} monthly budget ({})", c.name, ccy))]);
                {
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone());
                    let last = Rc::new(Cell::new(b.filter(|v| !v.is_zero())));
                    let weak_entry = entry.downgrade();
                    let save: Rc<dyn Fn()> = Rc::new(move || {
                        let (Some(a), Some(e)) = (w.upgrade(), weak_entry.upgrade()) else { return };
                        let t = e.text();
                        let v = if t.trim().is_empty() { None } else {
                            let Some(v) = parse_user(&t).filter(|v| *v >= Decimal::ZERO) else {
                                e.add_css_class("error");
                                return a.toast("Budget not saved: type a non-negative amount, like 600 or 1,250.00");
                            };
                            (!v.is_zero()).then_some(v)
                        };
                        if v == last.get() { e.remove_css_class("error"); e.remove_css_class("warning"); return; }
                        if a.saved(a.db.set_budget_from(&sc, id, v.map(|v| (v, ccy)), m)) {
                            last.set(v);
                            e.remove_css_class("error"); e.remove_css_class("warning");
                            // Refresh after the focus event; rebuilding inside it steals the next click.
                            let redraw = redraw.clone();
                            glib::idle_add_local_once(move || redraw());
                        } else { e.add_css_class("error"); }
                    });
                    let on_enter = save.clone(); entry.connect_activate(move |_| on_enter());
                    let focus = gtk::EventControllerFocus::new();
                    focus.connect_leave(move |_| save()); entry.add_controller(focus);
                    entry.connect_changed(|e| { e.remove_css_class("error"); e.add_css_class("warning"); });
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
                    "Each bar shows the savings target by now, not a bank balance or deposits. Use Record Allocation to say how much you have reserved; this is separate from the target. Suggestions under the list are yearly or quarterly charges FinGraph spotted in your statements; add one to start setting money aside for it. Each month sets a share aside so the bill is covered when it comes; that comes out of what you save in Your Plan.",
                ));
                funds_box.append(&dim(
                    "Insurance, licences, registrations, annual plans: bills that come once a quarter or once a year.",
                ));
                let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                let ideas: Vec<core::FundIdea> = a.db.big_bill_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
                for fd in &s.funds {
                    let due = fd.due(today());
                    let aside = s.fx.convert(fd.set_aside(today()), fd.currency, ccy).unwrap_or_default();
                    let whole = s.fx.convert(fd.amount, fd.currency, ccy).unwrap_or_default();
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
                    col.append(&gtk::Label::builder().label(format!("Target by now: {} of {}", money(aside), money(whole))).xalign(1.0).css_classes(["tnum", "caption"]).build());
                    let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(if whole.is_zero() { 0.0 } else { f(aside / whole).min(1.0) }).build();
                    col.append(&bar);
                    r.add_suffix(&col);
                    let allocation = a.db.fund_allocation(fd.id, due);
                    col.append(&dim(&allocation.map(|v| format!("Recorded allocation: {}", fmt_money(v, fd.currency))).unwrap_or_else(|| "Allocation not recorded".into())));
                    let record = gtk::Button::builder().label("Record Allocation…").valign(gtk::Align::Center).build();
                    let (w, allocation_fund, allocation_redraw) = (Rc::downgrade(&a), fd.clone(), redraw.clone());
                    record.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() { allocation_dialog(&a, &allocation_fund, allocation_redraw.clone()); }
                    });
                    r.add_suffix(&record);
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
                    let total: Decimal = s.funds.iter().map(|fd| s.fx.convert(fd.set_aside(today()), fd.currency, ccy).unwrap_or_default()).sum();
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
                    let limit = s.fx.convert(wl.limit, wl.currency, ccy).unwrap_or_default();
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

fn allocation_dialog(app: &Rc<App>, fund: &Fund, done: crate::Refresh) {
    let due = fund.due(today());
    let entry = gtk::Entry::builder().placeholder_text("Leave empty if unknown").input_purpose(gtk::InputPurpose::Number).build();
    if let Some(v) = app.db.fund_allocation(fund.id, due) { entry.set_text(&amount_text(v)); }
    let dlg = adw::AlertDialog::builder().heading(format!("Allocation for {}", fund.name))
        .body(format!("Amount you have reserved in {} for the bill due {}. This is your record, not a verified bank balance. Do not allocate the same money to more than one bill.", fund.currency, due))
        .extra_child(&entry).build();
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let (w, id) = (Rc::downgrade(app), fund.id);
    dlg.connect_response(Some("save"), move |_, _| {
        let Some(a) = w.upgrade() else { return };
        let value = if entry.text().trim().is_empty() { None } else {
            let Some(v) = parse_user(&entry.text()).filter(|v| *v >= Decimal::ZERO) else { return a.toast("Type a non-negative amount, or leave it empty"); };
            Some(v)
        };
        if a.saved(a.db.set_fund_allocation(id, due, value)) { done(); a.refresh(); }
    });
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
