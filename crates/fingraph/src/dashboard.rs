//! Dashboard & Insights: KPI ribbon, six interactive charts, and the "What changed?" cards.

use adw::prelude::*;
use chrono::{Datelike, Duration, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::db::DocumentRow;
use fingraph_core::insights::{self};
use rust_decimal::prelude::ToPrimitive;
use std::cell::RefCell;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, LineSeries, Series};
use crate::{App, heading, info_caption, today, with_info};

/// The last day of `d`'s month.
fn end_of(d: NaiveDate) -> NaiveDate {
    insights::add_months(insights::month_start(d), 1) - Duration::days(1)
}

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

struct Kpi {
    value: gtk::Label,
    sub: gtk::Label,
}

fn kpi_card(title: &str, help: &str) -> (gtk::Box, Kpi) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes(["card", "kpi"]).build();
    b.append(&info_caption(title, help));
    let value = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let sub = gtk::Label::builder().xalign(0.0).css_classes(["dim-label", "caption"]).wrap(true).build();
    b.append(&value);
    b.append(&sub);
    (b, Kpi { value, sub })
}

/// One number in the quiet strip under the headline: no card of its own, smaller, still with its ⓘ.
fn stat(title: &str, help: &str) -> (gtk::Box, Kpi) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).css_classes(["stat"]).build();
    b.append(&info_caption(title, help));
    let value = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let sub = gtk::Label::builder().xalign(0.0).css_classes(["dim-label", "caption"]).wrap(true).build();
    b.append(&value);
    b.append(&sub);
    (b, Kpi { value, sub })
}

/// The month Overview's headline speaks for: this month, unless its statements aren't in yet (the newest
/// statement ends before it) and the newest statement's month has a budget, which then speaks instead.
fn headline_month(newest: Option<NaiveDate>, today: NaiveDate, budgeted: impl Fn(NaiveDate) -> bool) -> NaiveDate {
    let this_month = insights::month_start(today);
    newest.map(insights::month_start).filter(|m| *m < this_month && budgeted(*m)).unwrap_or(this_month)
}

/// Change a card's title (the label in its title row).
fn set_caption(card: &gtk::Box, text: &str) {
    if let Some(l) = card.first_child().and_then(|row| row.first_child()).and_downcast::<gtk::Label>() {
        l.set_label(text);
    }
}

/// The value classes for an amount: red below zero, green otherwise.
fn sign_classes(d: Decimal, extra: &str) -> [&str; 3] {
    ["value", extra, if d.is_sign_negative() { "amount-out" } else { "amount-in" }]
}

const FLOW_PERIODS: [(&str, &str); 3] = [("Year", "overview_flow_year"), ("Month", "overview_flow_month"), ("Fortnight", "overview_flow_fortnight")];

fn chart_card(title: &str, help: &str, chart: &gtk::Widget) -> (gtk::Box, gtk::Label) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["card", "chart-card"]).build();
    let t = heading(title);
    b.append(&with_info(&t, help));
    b.append(chart);
    (b, t)
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let empty = adw::StatusPage::builder()
        .icon_name("x-office-spreadsheet-symbolic")
        .title("No transactions yet")
        .description("Drop your bank statements on the Statements page, or load the demo data to explore every chart.")
        .build();
    let demo_btn = gtk::Button::builder().label("Load Demo Data").halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).action_name("win.demo").build();
    empty.set_child(Some(&demo_btn));

    // what needs an answer from you, shown only while something does
    let review = adw::Banner::builder().button_label("Review").revealed(false).build();
    review.set_action_name(Some("win.show-review"));

    // the headline: one answer to "am I OK this month?", and how fresh the statements behind it are
    let hero = gtk::Grid::builder().column_spacing(12).column_homogeneous(true).build();
    let (c_left, k_left) = kpi_card(
        "Left to Spend This Month",
        "What's left this month in the categories you've budgeted: their budgets (with rollovers and money moved) minus what your statements show spent in them so far. Statements arrive after the month ends, so early in the month this looks high. Red means you're over.",
    );
    c_left.add_css_class("headline");
    let fresh = gtk::Label::builder().xalign(0.0).css_classes(["caption"]).build();
    let fresh_row = gtk::Box::builder().spacing(6).css_classes(["verified"]).margin_top(4).build();
    fresh_row.append(&gtk::Image::from_icon_name("object-select-symbolic"));
    fresh_row.append(&fresh);
    c_left.append(&fresh_row);
    // with no budget yet, the headline is the way to make one
    let setup = gtk::Button::builder().label("Set Up Budget…").action_name("win.setup-budget").halign(gtk::Align::Start).css_classes(["pill", "suggested-action"]).margin_top(6).visible(false).build();
    c_left.insert_child_after(&setup, c_left.first_child().as_ref());

    // net cash flow over a year, a month or a fortnight; each period keeps its own way of measuring
    let (c_flow, k_flow) = kpi_card(
        "Net Cash Flow",
        "Money in minus money out, transfers between your own accounts left out. Green: more came in than went out. Red: you spent more than came in. Pick the period beside the title. The menu picks how it's measured: Average takes your last 12 months of statements and scales them to the period, Range scales the range picked at the top, and Actual is the real period (this year so far, the last full month, or the last 14 days of statements).",
    );
    let period = Rc::new(std::cell::Cell::new(app.db.setting("overview_flow_period").and_then(|v| v.parse().ok()).filter(|p: &usize| *p < 3).unwrap_or(1)));
    let periods = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Start).build();
    let mut toggles: Vec<gtk::ToggleButton> = Vec::new();
    for (i, (name, _)) in FLOW_PERIODS.iter().enumerate() {
        let t = gtk::ToggleButton::builder().label(*name).active(i == period.get()).css_classes(["small-toggle"]).build();
        if let Some(first) = toggles.first() {
            t.set_group(Some(first));
        }
        periods.append(&t);
        toggles.push(t);
    }
    // the period switch sits on the title row, after the ⓘ
    if let Some(title_row) = c_flow.first_child().and_downcast::<gtk::Box>() {
        title_row.append(&periods);
    }
    let measure = gtk::DropDown::from_strings(&["Average", "Range", "Actual"]);
    measure.set_css_classes(&["flat", "caption"]);
    measure.set_halign(gtk::Align::Start);
    measure.set_tooltip_text(Some("How this is measured"));
    let measure_of = {
        let w = Rc::downgrade(app);
        move |p: usize| w.upgrade().and_then(|a| a.db.setting(FLOW_PERIODS[p].1)).and_then(|v| v.parse().ok()).unwrap_or(0u32)
    };
    measure.set_selected(measure_of(period.get()));
    let switching = Rc::new(std::cell::Cell::new(false));
    {
        let (w, period, switching) = (Rc::downgrade(app), period.clone(), switching.clone());
        measure.connect_selected_notify(move |d| {
            if switching.get() {
                return;
            }
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_setting(FLOW_PERIODS[period.get()].1, &d.selected().to_string()));
                a.refresh();
            }
        });
    }
    for (i, t) in toggles.iter().enumerate() {
        let (w, period, switching, measure) = (Rc::downgrade(app), period.clone(), switching.clone(), measure.clone());
        let measure_of = measure_of.clone();
        t.connect_toggled(move |t| {
            if !t.is_active() {
                return;
            }
            let Some(a) = w.upgrade() else { return };
            period.set(i);
            a.saved(a.db.set_setting("overview_flow_period", &i.to_string()));
            switching.set(true);
            measure.set_selected(measure_of(i));
            switching.set(false);
            a.refresh();
        });
    }
    c_flow.append(&measure);
    hero.attach(&c_left, 0, 0, 1, 1);
    hero.attach(&c_flow, 1, 0, 1, 1);
    body.append(&hero);

    // everything else, quieter: one strip, no card per number
    let strip = gtk::Box::builder().spacing(24).homogeneous(true).css_classes(["card", "stat-strip"]).build();
    let (c2, k_rate) = stat(
        "Savings Rate",
        "The share of your income you kept: (income − expenses) ÷ income, transfers left out. 20% means you kept 20 cents of every dollar that came in. For a business it's the operating margin. Below zero means you spent more than you earned.",
    );
    let (c_run, k_run) = stat(
        "Cash Runway",
        "How many months the money in your bank accounts would last at your usual spending if no money came in. Cash is the latest balance on each account that isn't a card or loan; usual spending is the average month over your last 12 months of statements. 3 to 6 months is a common safety net.",
    );
    let (c5, k_age) = stat(
        "Age of Money",
        "How many days money sits between arriving and being spent. Each purchase spends the oldest money still in your accounts. Under 30 days means you're spending this month's pay as it arrives; over 30 means you're living on last month's money, which is the goal.",
    );
    let (c3, k_subs) = stat(
        "Subscriptions",
        "What the subscriptions still being charged cost you a month, with the yearly total below. USD ones are converted. The Subscriptions page lists each one.",
    );
    let (c4, k_rec) = stat(
        "Statements Reconciled",
        "How many statements add up to the cent: the opening balance plus every transaction read equals the closing balance the bank printed. If one doesn't, the Statements page shows it. Uncategorized is how many transactions still have no category.",
    );
    let (c_bill, k_bill) = stat(
        "Next Big Bill",
        "The soonest of your big yearly bills (insurance, HOA fees, licences), when it's due, and how much of it has been set aside so far. Add or change them in Budget → Edit Plan.",
    );
    let add_bills = gtk::Button::builder().label("Add Big Bills…").halign(gtk::Align::Start).css_classes(["flat", "small-toggle"]).visible(false).build();
    {
        let w = Rc::downgrade(app);
        add_bills.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let w2 = Rc::downgrade(&a);
                crate::budget::wizard(&a, Rc::new(move || if let Some(a) = w2.upgrade() { a.refresh() }));
            }
        });
    }
    c_bill.insert_child_after(&add_bills, c_bill.first_child().as_ref());
    for c in [&c2, &c_run, &c5, &c3, &c4, &c_bill] {
        strip.append(c);
    }
    body.append(&strip);
    // what each business owes you for spending you paid personally
    let owed_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    body.append(&owed_box);
    let budget_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    body.append(&budget_box);
    // the big once-a-year bills, so the expensive months are planned for
    let bills_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    body.append(&bills_box);

    // charts
    let monthly = Chart::new(240);
    let donut = Chart::new(220);
    let breakdown = Chart::new(220);
    let burn = Chart::new(240);
    let pareto = Chart::new(240);
    let balances = Chart::new(260);
    let pnl = Chart::new(240);
    let grid = gtk::Grid::builder().column_spacing(12).row_spacing(12).column_homogeneous(true).build();
    let (m_card, _) = chart_card(
        "Monthly Income vs. Expenses · Net Cash Flow",
        "For each month: green bars are money in, red bars are money out, and the blue line is the difference. The line above zero means you saved that month; below zero means you spent more than came in. Hover a month for the numbers, click it to show just that month.",
        monthly.widget.upcast_ref());
    let cat_box = gtk::Box::builder().spacing(12).build();
    donut.widget.set_hexpand(true);
    breakdown.widget.set_hexpand(true);
    cat_box.append(&donut.widget);
    cat_box.append(&breakdown.widget);
    let (cat_card, cat_title) = chart_card(
        "Spending by Category",
        "Where your spending went in the date range. The ring splits it by category (bigger slice, more spent); the bars beside it rank the same categories. Click a slice to see the merchants inside that category, and click a bar to list its transactions. Transfers between your own accounts aren't spending.",
        cat_box.upcast_ref());
    let back = gtk::Button::builder()
        .child(&adw::ButtonContent::builder().label("All categories").icon_name("go-previous-symbolic").build())
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .visible(false)
        .build();
    cat_card.append(&back);
    // the latest refresh's category bars, which Back redraws
    let breakdown_now: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    let (b_card, _) = chart_card(
        "Cumulative Daily Spend Pace (this month vs its usual pace)",
        "How your spending adds up day by day through the month. Orange is this month; the dashed line is an average month; the shaded band runs from your lowest to highest recent month. Orange above the band means you're spending faster than usual, below it slower. It stops at your newest statement's date.",
        burn.widget.upcast_ref());
    let (p_card, _) = chart_card(
        "Where Most of Your Money Goes",
        "Your 10 biggest merchants in the date range, largest first. The line is a running total as a share of all your spending: over the third bar, it shows how much the top three take together. A line that climbs fast means a few places take most of your money, so that's where cutting back helps most. Click a bar to list its transactions.",
        pareto.widget.upcast_ref());
    let (bal_card, _) = chart_card(
        "Account Balance History (verified running balances)",
        "Each line is one account's balance over time, taken from the balances printed on your statements, so every point is one the bank confirmed. Credit cards and loans sit below zero: that's what you owe. Hover to see every account's balance on a date.",
        balances.widget.upcast_ref());
    let (pnl_card, _) = chart_card(
        "Workspace / Property P&L",
        "Income (green), expenses (red) and net (blue) for each workspace, property or project in the date range. A blue bar below zero means that one cost more than it brought in.",
        pnl.widget.upcast_ref());
    grid.attach(&m_card, 0, 0, 1, 1);
    grid.attach(&cat_card, 1, 0, 1, 1);
    grid.attach(&b_card, 0, 1, 1, 1);
    grid.attach(&p_card, 1, 1, 1, 1);
    grid.attach(&bal_card, 0, 2, 1, 1);
    grid.attach(&pnl_card, 1, 2, 1, 1);
    body.append(&grid);


    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    let clamp = adw::Clamp::builder().maximum_size(1500).child(&body).build();
    stack.add_named(&gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build(), Some("full"));

    // state for drill-down
    let drill: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    {
        let (drill, now) = (drill.clone(), breakdown_now.clone());
        back.connect_clicked(move |_| {
            *drill.borrow_mut() = None;
            let f = now.borrow().clone();
            if let Some(f) = f {
                f();
            }
        });
    }

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (monthly, donut, breakdown, burn, pareto, balances, pnl) = (monthly.clone(), donut.clone(), breakdown.clone(), burn.clone(), pareto.clone(), balances.clone(), pnl.clone());
        let (stack, drill, cat_title, back, review, breakdown_now) = (stack.clone(), drill.clone(), cat_title.clone(), back.clone(), review.clone(), breakdown_now.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let ids = a.entity_ids();
            let waiting = a.db.triage_count(&ids).unwrap_or(0);
            review.set_title(&format!("{waiting} merchant{} to review", if waiting == 1 { "" } else { "s" }));
            review.set_revealed(waiting > 0);
            let rows = a.all_rows();
            if rows.is_empty() {
                stack.set_visible_child_name("empty");
                return;
            }
            stack.set_visible_child_name("full");
            let docs: Vec<DocumentRow> = a.db.documents(&ids).unwrap_or_default();
            let prefs = a.db.prefs();
            let parents_early = a.parents();
            let hidden_accounts: Vec<i64> = a.db.accounts().unwrap_or_default().iter().filter(|x| x.hidden).map(|x| x.id).collect();
            // balances first (they need every row), then leave the excluded categories out of spending
            let all_rows = rows.clone();
            let top = |t: &fingraph_core::db::TxRow| t.category_id.and_then(|id| parents_early.get(&id).cloned()).or_else(|| t.category_name.clone());
            let rows: Vec<fingraph_core::db::TxRow> = rows.into_iter().filter(|t| !(t.amount.is_sign_negative() && top(t).is_some_and(|c| prefs.excluded_categories.contains(&c)))).collect();
            let fx = a.db.fx().unwrap_or_default();
            let ccy = a.display();
            let (from, to) = a.range();
            let parents = a.parents();
            let money = |d: Decimal| a.money(d);
            for c in [&monthly, &donut, &breakdown, &burn, &pareto, &balances, &pnl] {
                let sym = ccy.symbol();
                *c.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            }

            // KPIs
            crate::owed::fill(&a, &owed_box);
            crate::budget::fill_overview(&a, &budget_box);
            crate::budget::fill_big_bills(&a, &bills_box);
            // an empty section takes no room (it used to leave a gap above the charts)
            for b in [&owed_box, &budget_box, &bills_box] {
                b.set_visible(b.first_child().is_some());
            }
            let k = insights::kpis(&rows, &docs, from, to, ccy, &fx, today());
            // a currency with no exchange rate yet is left out of every total, and said so
            let unrated: Vec<&str> = insights::missing_rates(&rows, ccy, &fx).iter().map(|c| c.code()).collect();
            let unrated = if unrated.is_empty() { String::new() } else { format!("\nNot counted: {} (no exchange rate yet)", unrated.join(", ")) };
            // net cash flow: (income, expenses) over a window, scaled to the card's period
            let data_end = insights::data_span(&rows).map_or(end_of(today()), |s| s.1.min(today()));
            let data_start = insights::data_span(&rows).map_or(data_end, |s| s.0);
            {
                let (k, per) = (&k_flow, period.get());
                let days_in = [Decimal::from(365), Decimal::from(365) / Decimal::from(12), Decimal::from(14)][per];
                let (wf, wt, scale, what) = match measure.selected() {
                    // last 12 months of statements, or all of them when there are fewer
                    0 => {
                        let wf = (data_end - Duration::days(364)).max(data_start);
                        (wf, data_end, true, format!("Average of {} to {}", wf.format("%b %Y"), data_end.format("%b %Y")))
                    }
                    1 => (from, to.min(data_end), true, format!("Scaled from {} – {}", from.format("%d %b %Y"), to.min(data_end).format("%d %b %Y"))),
                    _ => match per {
                        0 => {
                            let wf = NaiveDate::from_ymd_opt(data_end.year(), 1, 1).unwrap_or(data_end);
                            (wf, data_end, false, format!("{} so far, to {}", data_end.year(), data_end.format("%d %b")))
                        }
                        1 => {
                            // the last month the statements cover in full
                            let m = insights::month_start(data_end + Duration::days(1)) - Duration::days(1);
                            let wf = insights::month_start(m);
                            (wf, m, false, wf.format("%B %Y").to_string())
                        }
                        _ => (data_end - Duration::days(13), data_end, false, format!("{} – {}", (data_end - Duration::days(13)).format("%d %b"), data_end.format("%d %b"))),
                    },
                };
                let (inc, exp) = insights::flows(&rows, wf, wt, ccy, &fx);
                let span = Decimal::from((wt - wf).num_days().max(0) + 1);
                let r = if scale { days_in / span } else { Decimal::ONE };
                let (inc, exp) = ((inc * r).round_dp(2), (exp * r).round_dp(2));
                let net = inc - exp;
                k.value.set_label(&money(net));
                k.value.set_css_classes(&sign_classes(net, "tnum"));
                k.sub.set_label(&format!("In {} · Out {}\n{what}{unrated}", money(inc), money(exp)));
            }
            // the month the headline speaks for: this one, or before its first statement is in, the last
            // month that has statements (on 2 Nov, October), so nothing reads "CI$ 0.00 spent" for want of data
            let newest = docs.iter().filter_map(|d| d.period.map(|p| p.1)).max();
            let this_month = insights::month_start(today());
            let shown = headline_month(newest, today(), |m| crate::budget::left_in(&a, m).is_some());
            set_caption(&c_left, &if shown == this_month { "Left to Spend This Month".to_string() } else { format!("Left to Spend in {}", shown.format("%B")) });
            match crate::budget::left_in(&a, shown) {
                Some((funded, spent)) => {
                    let left = funded - spent;
                    k_left.value.set_label(&money(left));
                    k_left.value.set_css_classes(&sign_classes(left, "tnum"));
                    k_left.value.set_visible(true);
                    setup.set_visible(false);
                    let waiting = if shown == this_month { String::new() } else { format!("\n{}'s statements aren't in yet", this_month.format("%B")) };
                    k_left.sub.set_label(&format!("{} spent of {} budgeted{waiting}", money(spent), money(funded)));
                }
                None => {
                    k_left.value.set_visible(false);
                    setup.set_visible(true);
                    k_left.sub.set_label("Your pay, your usual spending and a savings goal, starting from what your statements already show.");
                }
            }
            // what the numbers are measured to: the newest statement
            fresh_row.set_visible(newest.is_some());
            fresh.set_label(&newest.map(|d| format!("Verified statements to {}", d.format("%d %b %Y"))).unwrap_or_default());
            // the strip's last slot: the next big bill, or while none are planned, the way to add them
            match crate::budget::next_big_bill(&a) {
                Some((name, due, amount, aside)) => {
                    k_bill.value.set_visible(true);
                    add_bills.set_visible(false);
                    k_bill.value.set_label(&money(amount));
                    k_bill.sub.set_label(&format!("{name} · due {}\n{} set aside so far", due.format("%d %b %Y"), money(aside)));
                }
                None => {
                    k_bill.value.set_visible(false);
                    add_bills.set_visible(true);
                    k_bill.sub.set_label("None planned yet: insurance, licences, school terms");
                }
            }
            {
                // cash: the latest balance of every visible account that isn't a card or loan
                let accts = fingraph_core::wealth::account_balances(&all_rows, &docs);
                let cash: Decimal = accts
                    .iter()
                    .filter(|(id, b)| !b.liability && !hidden_accounts.contains(id))
                    .filter_map(|(_, b)| b.points.last().map(|p| fx.convert(p.1, b.currency, ccy)))
                    .sum();
                let wf = (data_end - Duration::days(364)).max(data_start);
                let (_, spent) = insights::flows(&rows, wf, data_end, ccy, &fx);
                let monthly = spent * Decimal::from(365) / Decimal::from(12) / Decimal::from(((data_end - wf).num_days() + 1).max(1));
                if monthly.is_zero() || cash.is_sign_negative() {
                    k_run.value.set_label("—");
                    k_run.sub.set_label("Needs account balances and spending");
                } else {
                    k_run.value.set_label(&format!("{:.1} months", f(cash / monthly)));
                    k_run.sub.set_label(&format!("{} cash ÷ {} a month usual spending", money(cash.round_dp(2)), money(monthly.round_dp(2))));
                }
            }
            k_rate.value.set_label(&k.savings_rate_pct.map_or("—".into(), |p| format!("{p:.1}%")));
            k_rate.sub.set_label("Of income kept, transfers left out");
            k_subs.value.set_label(&format!("{}/mo", money(k.subs_monthly)));
            k_subs.sub.set_label(&format!("{} a year", money(k.subs_annual)));
            // a statement waiting for review isn't reconciled, and isn't in `docs`
            let held = a.db.documents_all(&ids).unwrap_or_default().iter().filter(|d| !d.review.is_empty()).count();
            k_rec.value.set_label(&format!("{}/{}", k.reconciled, k.statements + held));
            let held = if held > 0 { format!(" · {held} waiting for review") } else { String::new() };
            k_rec.sub.set_label(&format!("To the cent{held} · {} uncategorized", k.uncategorized));
            // split lines add up to their charge, so they age money the same way
            match fingraph_core::outlook::age_of_money(&all_rows, &docs, ccy, &fx) {
                Some(x) => {
                    k_age.value.set_label(&format!("{} days", x.days));
                    k_age.sub.set_label(&format!("How long money sits before it's spent, as of {} · aim for 30+", x.as_of.format("%d %b")));
                }
                None => {
                    k_age.value.set_label("—");
                    k_age.sub.set_label("Needs statements with money in and out");
                }
            }

            // monthly bars + net line: 12 months ending at the range end
            let end = to.min(today());
            let mb = insights::monthly(&rows, end, prefs.history_months.max(1), ccy, &fx);
            monthly.set(Data::Bars {
                labels: mb.iter().map(|m| m.month.format("%b").to_string()).collect(),
                series: vec![
                    Series { name: "Income".into(), values: mb.iter().map(|m| f(m.income)).collect(), color: charts::GREEN },
                    Series { name: "Expenses".into(), values: mb.iter().map(|m| f(m.expenses)).collect(), color: charts::RED },
                ],
                line: Some(Series { name: "Net".into(), values: mb.iter().map(|m| f(m.net)).collect(), color: charts::BLUE }),
                tips: mb.iter().map(|m| format!("{}\nIncome {}\nExpenses {}\nNet {}\nClick to show this month", m.month.format("%B %Y"), money(m.income), money(m.expenses), money(m.net))).collect(),
            });
            {
                let (w, months) = (Rc::downgrade(&a), mb.iter().map(|m| m.month).collect::<Vec<_>>());
                monthly.on_click(move |i| {
                    if let (Some(a), Some(m)) = (w.upgrade(), months.get(i)) {
                        let last = insights::add_months(*m, 1) - Duration::days(1);
                        a.custom_range.set(Some((*m, last)));
                        let n = a.range_items.borrow().len();
                        a.range_dd.set_selected(n as u32 - 1);
                    }
                });
            }

            // categories
            let cats = insights::by_category(&rows, from, to, ccy, &fx, &parents);
            let mut items: Vec<(String, Decimal)> = cats.iter().take(9).cloned().collect();
            let rest: Decimal = cats.iter().skip(9).map(|c| c.1).sum();
            if !rest.is_zero() {
                items.push(("Other".into(), rest));
            }
            let total: Decimal = items.iter().map(|i| i.1).sum();
            donut.set(if items.is_empty() {
                Data::Empty("No spending in this range".into())
            } else {
                Data::Donut {
                    items: items.iter().map(|(n, v)| (n.clone(), f(*v))).collect(),
                    tips: items.iter().map(|(n, v)| format!("{n}\n{}\nClick to see merchants", money(*v))).collect(),
                    center: money(total),
                }
            });
            let show_breakdown = {
                let (breakdown, cat_title, back, drill) = (breakdown.clone(), cat_title.clone(), back.clone(), drill.clone());
                let (cats, money_fmt) = (cats.clone(), {
                    let a = a.clone();
                    move |d: Decimal| a.money(d)
                });
                let (rows, parents, fx) = (rows.clone(), parents.clone(), fx.clone());
                move || {
                    let sel = drill.borrow().clone();
                    let list: Vec<(String, Decimal)> = match &sel {
                        Some(c) => insights::merchants_in(&rows, c, from, to, ccy, &fx, &parents),
                        None => cats.clone(),
                    };
                    cat_title.set_label(&match &sel {
                        Some(c) => format!("Spending by Category › {c}"),
                        None => "Spending by Category".into(),
                    });
                    back.set_visible(sel.is_some());
                    let list: Vec<_> = list.into_iter().take(9).collect();
                    breakdown.set(Data::HBars {
                        items: list.iter().map(|(n, v)| (n.clone(), f(*v))).collect(),
                        values: list.iter().map(|(_, v)| money_fmt(*v)).collect(),
                        tips: list.iter().map(|(n, v)| format!("{n}: {}\nClick to list transactions", money_fmt(*v))).collect(),
                    });
                }
            };
            let show_breakdown = Rc::new(show_breakdown);
            show_breakdown();
            {
                let (drill, sb, items) = (drill.clone(), show_breakdown.clone(), items.clone());
                donut.on_click(move |i| {
                    if let Some((name, _)) = items.get(i)
                        && name != "Other"
                    {
                        *drill.borrow_mut() = Some(name.clone());
                        sb();
                    }
                });
            }
            {
                let (drill, w, cats, rows, parents, fx) = (drill.clone(), Rc::downgrade(&a), cats.clone(), rows.clone(), parents.clone(), fx.clone());
                breakdown.on_click(move |i| {
                    let Some(a) = w.upgrade() else { return };
                    let sel = drill.borrow().clone();
                    let list = match &sel {
                        Some(c) => insights::merchants_in(&rows, c, from, to, ccy, &fx, &parents),
                        None => cats.clone(),
                    };
                    let Some((name, _)) = list.get(i) else { return };
                    let ids: Vec<i64> = rows
                        .iter()
                        .filter(|t| t.posted >= from && t.posted <= to && t.amount.is_sign_negative() && !t.is_transfer())
                        .filter(|t| match &sel {
                            Some(_) => &t.clean_payee == name,
                            None => t.category_id.map_or("Uncategorized".to_string(), |id| parents.get(&id).cloned().unwrap_or_else(|| t.category_name.clone().unwrap_or_default())) == *name,
                        })
                        .map(|t| t.id)
                        .collect();
                    a.show_transactions(ids);
                });
            }
            *breakdown_now.borrow_mut() = Some(show_breakdown.clone());

            // burn curve
            let bc = insights::burn_curve(&rows, end, ccy, &fx, prefs.band_months);
            let pts = |v: &[Decimal]| v.iter().enumerate().map(|(i, d)| (i as f64 + 1.0, f(*d))).collect::<Vec<_>>();
            let days = bc.avg.len().max(bc.current.len());
            burn.set(Data::Lines {
                x_labels: [1, 5, 10, 15, 20, 25, 30].iter().map(|d| (*d as f64, d.to_string())).collect(),
                band: (!bc.lo.is_empty()).then(|| (pts(&bc.lo), pts(&bc.hi))),
                series: vec![
                    LineSeries { name: format!("{}", bc.month.format("%B")), points: pts(&bc.current), color: charts::ORANGE, dashed: false },
                    LineSeries { name: format!("{}-month average", prefs.band_months), points: pts(&bc.avg), color: charts::SLATE, dashed: true },
                ],
                stops: (0..days)
                    .map(|d| {
                        let cur = bc.current.get(d).map(|v| format!("\nThis month {}", money(*v))).unwrap_or_default();
                        let avg = bc.avg.get(d).map(|v| format!("\n{}-month average {}", prefs.band_months, money(*v))).unwrap_or_default();
                        (d as f64 + 1.0, format!("Day {}{cur}{avg}", d + 1))
                    })
                    .collect(),
            });

            // pareto
            let par = insights::pareto(&rows, from, to, ccy, &fx);
            pareto.set(if par.is_empty() {
                Data::Empty("No outflow in this range".into())
            } else {
                Data::Pareto {
                    items: par.iter().map(|(n, v, _)| (n.clone(), f(*v))).collect(),
                    cum_pct: par.iter().map(|p| p.2).collect(),
                    tips: par.iter().map(|(n, v, c)| format!("{n}\n{} · {c:.0}% of outflow so far\nClick to list", money(*v))).collect(),
                }
            });
            {
                let (w, par, rows) = (Rc::downgrade(&a), par.clone(), rows.clone());
                pareto.on_click(move |i| {
                    if let (Some(a), Some((name, _, _))) = (w.upgrade(), par.get(i)) {
                        a.show_transactions(rows.iter().filter(|t| &t.clean_payee == name && t.posted >= from && t.posted <= to).map(|t| t.id).collect());
                    }
                });
            }

            // balances (x = days since the epoch): history wants at least the twelve months up to the range end
            let ser: Vec<_> = insights::balances(&all_rows, &docs.iter().filter(|d| !hidden_accounts.contains(&d.account_id)).cloned().collect::<Vec<_>>(), ccy, &fx)
                .into_iter()
                .filter(|s| !all_rows.iter().any(|t| hidden_accounts.contains(&t.account_id) && t.account_name == s.account))
                .collect();
            let hist_from = from.min(insights::add_months(end, -11));
            let ord = |d: NaiveDate| d.num_days_from_ce() as f64;
            let mut stop_dates: Vec<NaiveDate> = ser.iter().flat_map(|s| s.points.iter().map(|p| p.0)).filter(|d| *d >= hist_from && *d <= to).collect();
            stop_dates.sort();
            stop_dates.dedup();
            let mut x_labels = Vec::new();
            let mut m = insights::month_start(hist_from);
            while m <= to {
                x_labels.push((ord(m), m.format("%b %y").to_string()));
                m = insights::add_months(m, 1);
            }
            balances.set(Data::Lines {
                x_labels,
                band: None,
                series: ser
                    .iter()
                    .enumerate()
                    .map(|(i, s)| LineSeries {
                        name: s.account.clone(),
                        points: s.points.iter().filter(|p| p.0 >= hist_from && p.0 <= to).map(|p| (ord(p.0), f(p.1))).collect(),
                        color: charts::PALETTE[i % charts::PALETTE.len()],
                        dashed: false,
                    })
                    .filter(|s| !s.points.is_empty())
                    .collect(),
                stops: stop_dates
                    .iter()
                    .map(|d| {
                        let lines: Vec<String> = ser.iter().filter_map(|s| s.points.iter().rev().find(|p| p.0 <= *d).map(|p| format!("{}: {}", s.account, money(p.1)))).collect();
                        (ord(*d), format!("{}\n{}", d.format("%d %b %Y"), lines.join("\n")))
                    })
                    .collect(),
            });

            // P&L per workspace / tag
            let ents = a.entities.borrow().clone();
            let pl = insights::pnl(&rows, &ents, from, to, ccy, &fx);
            pnl.set(if pl.is_empty() {
                Data::Empty("Nothing in this range".into())
            } else {
                Data::Bars {
                    labels: pl.iter().map(|r| r.label.rsplit(" › ").next().unwrap_or(&r.label).to_string()).collect(),
                    series: vec![
                        Series { name: "Income".into(), values: pl.iter().map(|r| f(r.income)).collect(), color: charts::GREEN },
                        Series { name: "Expenses".into(), values: pl.iter().map(|r| f(r.expenses)).collect(), color: charts::RED },
                        Series { name: "Net".into(), values: pl.iter().map(|r| f(r.net)).collect(), color: charts::BLUE },
                    ],
                    line: None,
                    tips: pl.iter().map(|r| format!("{}\nIncome {}\nExpenses {}\nNet {}", r.label, money(r.income), money(r.expenses), money(r.net))).collect(),
                }
            });

        })
    };
    let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    page.append(&review);
    page.append(&stack);
    (page.upcast(), refresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_speaks_for_the_last_month_with_statements() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // 2 Nov, statements to 31 Oct, October budgeted: October
        assert_eq!(headline_month(Some(d(10, 31)), d(11, 2), |_| true), d(10, 1));
        // no October budget: this month, whatever it shows
        assert_eq!(headline_month(Some(d(10, 31)), d(11, 2), |_| false), d(11, 1));
        // November's first statement is in: November
        assert_eq!(headline_month(Some(d(11, 15)), d(11, 20), |_| true), d(11, 1));
        // no statements at all: this month
        assert_eq!(headline_month(None, d(11, 2), |_| true), d(11, 1));
    }
}
