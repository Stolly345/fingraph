//! Budget: a monthly amount per category, suggested from what you usually spend (the median of the
//! last six months), with this month's spending against it and where the month is heading; a plan
//! built from your pay (the setup wizard) that projects what you'll save; and alerts when a category
//! goes over, or uses 85% with more of the month still to come.

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::budget::{self as core, Alert, Deduction, PayCycle, Plan, scope, suggest};
use fingraph_core::db::{Category, TxFilter, TxRow};
use fingraph_core::insights::{add_months, by_category, month_start};
use fingraph_core::money::{Currency, Fx, fmt_money, parse_user};
use gtk::glib;
use rust_decimal::prelude::ToPrimitive;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, LineSeries};
use crate::{App, clear, dim, heading, today};

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
struct Snap {
    ccy: Currency,
    fx: Fx,
    parents: BTreeMap<i64, String>,
    scope: String,
    rows: Vec<TxRow>,
    /// Top-level expense categories.
    cats: Vec<Category>,
    budgets: BTreeMap<i64, Decimal>,
    plan: Option<Plan>,
}

fn snap(app: &App) -> Snap {
    let ccy = app.display();
    let fx = app.db.fx().unwrap_or_default();
    let sc = scope(&app.entity_ids());
    let rows = crate::changes::counted_rows(app, app.db.transactions(&TxFilter { entity_ids: app.entity_ids(), split_lines: true, ..Default::default() }).unwrap_or_default());
    let cats = app.categories().into_iter().filter(|c| c.parent_id.is_none() && c.kind == fingraph_core::db::CategoryKind::Expense).collect();
    let budgets = app.db.budgets(&sc).unwrap_or_default().into_iter().map(|(c, (v, k))| (c, fx.convert(v, k, ccy))).collect();
    let plan = app.db.plan(&sc).map(|p| plan_in(p, ccy, &fx));
    Snap { ccy, fx, parents: app.parents(), scope: sc, rows, cats, budgets, plan }
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

fn elapsed(m: NaiveDate) -> Decimal {
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    if m == month_start(today()) { Decimal::from(today().day()) / Decimal::from(end.day()) } else { Decimal::ONE }
}

/// This month's categories that are over budget, or 85% used with more than 15% of the month to go.
pub fn alerts(app: &App) -> Vec<(String, Decimal, Decimal, Alert)> {
    let s = snap(app);
    let m = month_start(today());
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    let now: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, s.ccy, &s.fx, &s.parents).into_iter().collect();
    let mut out: Vec<_> = s
        .cats
        .iter()
        .filter_map(|c| {
            let b = *s.budgets.get(&c.id)?;
            let spent = now.get(&c.name).copied().unwrap_or_default();
            core::alert(spent, b, elapsed(m)).map(|a| (c.name.clone(), spent, b, a))
        })
        .collect();
    out.sort_by_key(|x| (x.3 != Alert::Over, std::cmp::Reverse(x.1 - x.2)));
    out
}

fn alert_text(a: &(String, Decimal, Decimal, Alert), ccy: Currency) -> String {
    let left = add_months(month_start(today()), 1).pred_opt().unwrap_or(today()).day() - today().day();
    match a.3 {
        Alert::Over => format!("{} over by {}", a.0, fmt_money(a.1 - a.2, ccy)),
        Alert::Near => format!("{} {}% used with {left} days to go", a.0, (a.1 / a.2 * Decimal::ONE_HUNDRED).round_dp(0)),
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
        .subtitle(glib::markup_escape_text(&a.iter().map(|x| alert_text(x, ccy)).collect::<Vec<_>>().join(" · ")))
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

fn kpi(title: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes(["card", "kpi"]).build();
    b.append(&gtk::Label::builder().label(title).xalign(0.0).css_classes(["dim-label", "caption-heading"]).build());
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
    let fill = gtk::Button::builder().label("Fill From My Spending").tooltip_text(format!("Give every category without a budget what it usually costs (median of the last {MONTHS} months)")).build();
    nav.append(&prev);
    nav.append(&next);
    nav.append(&title);
    nav.append(&setup);
    nav.append(&fill);
    body.append(&nav);

    let ribbon = gtk::Box::builder().spacing(12).homogeneous(true).build();
    let (k1, budgeted, budgeted_s) = kpi("Budgeted");
    let (k2, spent, spent_s) = kpi("Spent");
    let (k3, left, left_s) = kpi("Left");
    let (k4, pace, pace_s) = kpi("Heading For");
    for k in [&k1, &k2, &k3, &k4] {
        ribbon.append(k);
    }
    body.append(&ribbon);

    // the plan: pay in, budget out, what's left to save, and where that leads
    let plan_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    body.append(&plan_box);
    let projection = Chart::new(230);

    body.append(&heading("Categories"));
    body.append(&dim(&format!(
        "Type a monthly amount for any category and press Enter (empty or 0 removes it). “Usually” is the median of the last {MONTHS} months — a one-off doesn't count. Transfers and categories left out in Preferences aren't spending."
    )));
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    body.append(&list);
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
            let usual = suggest(&s.rows, m, MONTHS, ccy, &s.fx, &s.parents);
            let now: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, ccy, &s.fx, &s.parents).into_iter().collect();
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
                            let _ = a.db.set_budget(&sc, c.id, Some((*v, ccy)));
                            n += 1;
                        }
                    }
                    a.toast(&format!("Budgeted {n} categories from your last {MONTHS} months"));
                    redraw();
                }));
            }

            // the month so far, and where it's heading at this pace
            let total_b: Decimal = s.budgets.values().copied().sum();
            let total_s: Decimal = now.values().copied().sum();
            let in_b: Decimal = s.cats.iter().filter(|c| s.budgets.contains_key(&c.id)).filter_map(|c| now.get(&c.name)).copied().sum();
            let days = Decimal::from(end.day());
            let is_now = m == month_start(today());
            let gone = if is_now { Decimal::from(today().day()) } else { days };
            budgeted.set_label(&money(total_b));
            budgeted_s.set_label(&format!("{} categories", s.budgets.len()));
            spent.set_label(&money(total_s));
            spent_s.set_label(&format!("{} in budgeted categories · {} outside", money(in_b), money(total_s - in_b)));
            left.set_label(&money(total_b - in_b));
            left_s.set_label(if total_b.is_zero() { "Set a budget below, or fill it from your spending" } else if in_b > total_b { "Over budget" } else { "Left in budgeted categories" });
            left.set_css_classes(&["value", "tnum", if in_b > total_b { "amount-out" } else { "amount-in" }]);
            let proj = if gone.is_zero() { total_s } else { (total_s / gone * days).round_dp(2) };
            pace.set_label(&money(proj));
            pace_s.set_label(&if is_now { format!("At this pace (day {} of {}); a big bill early in the month overstates it", today().day(), end.day()) } else { "The whole month".into() });

            // each category's last six months, for its trend line
            let hist: BTreeMap<String, Vec<Decimal>> = s.cats.iter().map(|c| (c.name.clone(), core::history(&s.rows, &c.name, month_start(today()), MONTHS, ccy, &s.fx, &s.parents))).collect();

            // ── the plan
            clear(&plan_box);
            if let Some(p) = &s.plan {
                plan_box.append(&heading("Your Plan"));
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
                        "{} a payday ({}), {} × {} pays a year ÷ 12{}{}",
                        money(p.net_pay()),
                        p.cycle.label().to_lowercase(),
                        money(p.net_pay()),
                        p.cycle.per_year(),
                        if p.other_income.is_zero() { String::new() } else { format!(" + {} other income", money(p.other_income)) },
                        if pays.is_empty() { String::new() } else { format!(" · next: {pays}") }
                    ),
                    money(p.monthly_income()),
                    "amount-in",
                );
                line("Spending budget", format!("{} categories", s.budgets.len()), format!("− {}", money(total_b)), "");
                if !p.loan_payments.is_zero() {
                    line("Loan payments", "Instalments, which are transfers and not in the spending budget".into(), format!("− {}", money(p.loan_payments)), "");
                }
                let save = p.monthly_savings(total_b);
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
                    ct += p.monthly_income() - p.loan_payments - spend;
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
                card.append(&heading("Savings Over the Next 12 Months"));
                card.append(&dim(&format!(
                    "Green: take-home − budget − loans, every month ({} a month). Orange: each category's last six months drawn as a straight line (least squares) and carried forward — what happens if spending keeps drifting the way it has.",
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

            // ── one row per category that has a budget, spending or a usual amount
            list.remove_all();
            let mut shown: Vec<_> = s.cats.iter().filter(|c| s.budgets.contains_key(&c.id) || now.contains_key(&c.name) || usual.contains_key(&c.name)).collect();
            shown.sort_by_key(|c| std::cmp::Reverse(now.get(&c.name).copied().unwrap_or_default().max(s.budgets.get(&c.id).copied().unwrap_or_default())));
            let use_col = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            for c in shown {
                let sp = now.get(&c.name).copied().unwrap_or_default();
                let b = s.budgets.get(&c.id).copied();
                let u = usual.get(&c.name).copied();
                let mut sub = vec![match u {
                    Some(u) => format!("Usually {}", money(u)),
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
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).subtitle(sub.join(" · ")).build();

                let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(260).build();
                let status = gtk::Label::builder().xalign(1.0).css_classes(["tnum", "caption"]).build();
                match b {
                    Some(b) => match core::alert(sp, b, elapsed(m)) {
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
                if let Some(b) = b.filter(|b| !b.is_zero()) {
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
                        let _ = a.db.set_budget(&sc, id, v.map(|v| (v, ccy)));
                        redraw();
                    });
                }
                row.add_suffix(&entry);
                if let Some(u) = u.filter(|u| Some(*u) != b) {
                    let use_ = gtk::Button::builder().label(format!("Use {}", money(u))).valign(gtk::Align::Center).css_classes(["flat"]).build();
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), s.scope.clone(), c.id, redraw.clone());
                    use_.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            let _ = a.db.set_budget(&sc, id, Some((u, ccy)));
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
                list.append(&row);
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

// ──────────────────────────────────────────────────────────────── the setup wizard

fn parse_date(t: &str) -> Option<NaiveDate> {
    let t = t.trim();
    ["%d %b %Y", "%d %B %Y", "%Y-%m-%d", "%d/%m/%Y", "%d-%m-%Y", "%b %d %Y", "%B %d %Y"].iter().find_map(|f| NaiveDate::parse_from_str(&t.replace(',', ""), f).ok())
}

/// An amount, or "5%" (a percent).
fn parse_amount(t: &str) -> Option<(Decimal, bool)> {
    let pct = t.trim().ends_with('%');
    parse_user(t.trim().trim_end_matches('%')).map(|d| (d.abs(), pct))
}

fn amount_text(d: Decimal) -> String {
    if d.is_zero() { String::new() } else { d.round_dp(2).normalize().to_string() }
}

fn step_page(title: &str, intro: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(24).margin_end(24).build();
    let h = heading(title);
    h.add_css_class("title-2");
    b.append(&h);
    b.append(&dim(intro));
    (gtk::ScrolledWindow::builder().child(&b).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build(), b)
}

/// Four steps: pay & deductions → other money in and out → spending → savings goal (with the maths).
pub fn wizard(app: &Rc<App>, done: crate::Refresh) {
    let s = Rc::new(snap(app));
    let (ccy, m) = (s.ccy, month_start(today()));
    let money = move |d: Decimal| fmt_money(d, ccy);
    let detected = core::detect_salary(&s.rows, today());
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

    // ── 3. spending
    let (p3, b3) = step_page(
        "Your Spending",
        "Each category starts at what you usually spend (the median of your last six months) or the budget you already have. Change any of them; leave one empty for no budget. Fixed bills are marked — the savings step only trims the flexible ones.",
    );
    let usual = suggest(&s.rows, m, MONTHS, ccy, &s.fx, &s.parents);
    let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let cat_entries: Rc<Vec<(Category, gtk::Entry)>> = Rc::new(
        s.cats
            .iter()
            .filter(|c| s.budgets.contains_key(&c.id) || usual.contains_key(&c.name))
            .map(|c| {
                let u = usual.get(&c.name).copied();
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&c.name))
                    .subtitle(format!("{}{}", if core::is_fixed(&c.name) { "Fixed bill" } else { "Flexible" }, u.map(|u| format!(" · usually {}", money(u))).unwrap_or_default()))
                    .build();
                let e = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).build();
                e.set_text(&amount_text(s.budgets.get(&c.id).copied().or(u).unwrap_or_default()));
                r.add_suffix(&e);
                lb.append(&r);
                (c.clone(), e)
            })
            .collect(),
    );
    b3.append(&lb);
    let spend_total = gtk::Label::builder().xalign(0.0).css_classes(["heading", "tnum"]).build();
    b3.append(&spend_total);

    // ── 4. savings
    let (p4, b4) = step_page(
        "Your Savings Goal",
        "How much you want to put away each month — an amount, or a percent of take-home (20%). FinGraph does the arithmetic: take-home − spending − loans = what's left, and if that's short of the goal, how much the flexible categories need to shrink.",
    );
    let g4 = adw::PreferencesGroup::new();
    let goal = adw::EntryRow::builder().title(format!("Save per month ({} or %)", ccy.symbol())).text(amount_text(start.savings_goal)).build();
    g4.add(&goal);
    b4.append(&g4);
    let maths = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    maths.set_margin_top(4);
    b4.append(&maths);
    let scale = gtk::Button::builder().label("Trim Flexible Categories to Reach It").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).build();
    b4.append(&scale);
    let ahead = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b4.append(&ahead);

    // read every field into a plan + budgets, and redo the sums
    let read = {
        let (cycle, next_pay, gross, deds, other_in, loans, goal, cat_entries) = (cycle.clone(), next_pay.clone(), gross.clone(), deds.clone(), other_in.clone(), loans.clone(), goal.clone(), cat_entries.clone());
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
            let budgets: Vec<(i64, Option<Decimal>)> = cat_entries.iter().map(|(c, e)| (c.id, parse_user(&e.text()).map(|d| d.abs()).filter(|d| !d.is_zero()))).collect();
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
        let (read, take_home, spend_total, maths, ahead, scale, cat_entries, factor) = (read.clone(), take_home.clone(), spend_total.clone(), maths.clone(), ahead.clone(), scale.clone(), cat_entries.clone(), factor.clone());
        Rc::new(move || {
            let (p, budgets) = read();
            let deducted: Vec<String> = p.deductions.iter().map(|d| format!("{} {}", if d.name.is_empty() { "deduction" } else { &d.name }, money(p.deduction(d)))).collect();
            take_home.set_label(&format!(
                "Take-home {} a payday{} → {} a month ({} × {} ÷ 12)",
                money(p.net_pay()),
                if deducted.is_empty() { String::new() } else { format!(" (gross {} − {})", money(p.gross), deducted.join(" − ")) },
                money(p.monthly_income() - p.other_income),
                money(p.net_pay()),
                p.cycle.per_year()
            ));
            let total: Decimal = budgets.iter().filter_map(|b| b.1).sum();
            let fixed: Decimal = cat_entries.iter().zip(&budgets).filter(|((c, _), _)| core::is_fixed(&c.name)).filter_map(|(_, b)| b.1).sum();
            let flexible = total - fixed;
            spend_total.set_label(&format!("Spending budget {} a month — fixed bills {}, flexible {}", money(total), money(fixed), money(flexible)));
            let save = p.monthly_savings(total);
            let mut lines = vec![
                format!("  Take-home per month        {:>14}", money(p.monthly_income())),
                format!("− Spending budget            {:>14}", money(total)),
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
                    match p.flexible_factor(fixed, flexible) {
                        Some(k) => {
                            lines.push(format!(
                                "Solve take-home − loans − fixed − k × flexible = goal:\nk = ({} − {} − {} − {}) ÷ {} = {:.3}\nTrim every flexible category by {}% ({} → {}).",
                                money(p.monthly_income()),
                                money(p.loan_payments),
                                money(fixed),
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
                        None => lines.push("Even with no flexible spending it doesn't add up — lower the goal or the fixed bills.".into()),
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
    for (_, e) in cat_entries.iter() {
        let r = recalc.clone();
        e.connect_changed(move |_| r());
    }
    {
        let (cat_entries, factor) = (cat_entries.clone(), factor.clone());
        scale.connect_clicked(move |_| {
            let Some(k) = factor.get() else { return };
            for (_, e) in cat_entries.iter().filter(|(c, _)| !core::is_fixed(&c.name)) {
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
    let pages = [p1, p2, p3, p4];
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
            fwd.set_label(if i == 3 { "Save Budget" } else { "Next" });
            dlg.set_title(&format!("Set Up Your Budget · {} of 4", i + 1));
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
            if step.get() < 3 {
                return show(step.get() + 1);
            }
            let (p, budgets) = read();
            let _ = a.db.save_plan(&s.scope, &p);
            for (id, v) in budgets {
                let _ = a.db.set_budget(&s.scope, id, v.map(|v| (v, ccy)));
            }
            a.toast("Budget saved");
            dlg.close();
            done();
        });
    }
    dlg.present(Some(&app.win));
}
