//! Outlook: the weeks between statements. Where each bank account is probably heading (from its
//! last verified balance, its regular payments and its usual day-to-day spending), what's safe to
//! spend, the bills due this month, and how old the money you spend is. Every number here is an
//! estimate from statements that arrive a month late, and says so.

use adw::prelude::*;
use chrono::{Duration, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::db::TxFilter;
use fingraph_core::insights::{add_months, month_start};
use fingraph_core::money::fmt_money;
use fingraph_core::outlook::{self as core, BillState, Forecast};
use gtk::glib;
use rust_decimal::prelude::ToPrimitive;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, LineSeries};
use crate::{App, clear, dim, info_heading, today};

/// How far ahead the estimate runs.
const AHEAD: i64 = 45;

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

fn list() -> gtk::ListBox {
    gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build()
}

fn forecasts(app: &App) -> Vec<Forecast> {
    let ids = app.entity_ids();
    // an account's forecast follows all its rows, business-flagged ones too
    let rows = app.db.transactions(&TxFilter { entity_ids: ids.clone(), by_account: true, ..Default::default() }).unwrap_or_default();
    let docs = app.db.documents(&ids).unwrap_or_default();
    let hidden: Vec<i64> = app.db.accounts().unwrap_or_default().iter().filter(|a| a.hidden).map(|a| a.id).collect();
    core::forecast(&rows, &docs, today() + Duration::days(AHEAD)).into_iter().filter(|x| !hidden.contains(&x.account_id)).collect()
}

/// Accounts expected to dip below the minimum you set for them (zero when unset) before the horizon:
/// (account, what happens and when), soonest first.
pub fn shortfalls(app: &App) -> Vec<(String, String)> {
    let t = today();
    let mut v: Vec<(NaiveDate, String, String)> = forecasts(app)
        .iter()
        .filter_map(|fc| fc.first_below(app.db.min_balance(fc.account_id), t, t + Duration::days(AHEAD)).map(|s| (s.date, fc.account.clone(), s.label(fc.currency))))
        .collect();
    v.sort();
    v.into_iter().map(|(_, a, l)| (a, l)).collect()
}

/// Keep each account's estimate for the end of next month, to check against its statement later
/// (Monthly Close calls this when you close a month).
pub fn remember_forecast(app: &App) {
    let when = add_months(month_start(today()), 1).pred_opt().unwrap_or(today());
    let mut checks = app.db.forecast_checks();
    for fc in forecasts(app) {
        if let Some(v) = fc.at(when) {
            checks.insert(fc.account_id, (when, v.round_dp(2), today()));
        }
    }
    let _ = app.db.save_forecast_checks(&checks);
}

/// Variable categories' money left this month, spread over the days the statements don't cover yet.
/// (daily, left, days, through)
fn daily_budget_allowance(app: &App) -> Option<(Decimal, Decimal, i64, NaiveDate)> {
    let s = crate::budget::snap(app);
    let m = month_start(today());
    let env = crate::budget::envs(app, &s, m);
    let rolls = app.db.rollovers(&s.scope);
    let left: Decimal = s
        .cats
        .iter()
        .filter(|c| !s.fixed.contains(&c.name))
        .filter_map(|c| env.get(&c.id).filter(|e| !e.budget.is_zero() || rolls.contains_key(&c.id)))
        .map(|e| e.available())
        .sum();
    if env.values().all(|e| e.budget.is_zero()) {
        return None;
    }
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    let through = crate::budget::data_through(app);
    let from = if through >= m { through } else { m.pred_opt().unwrap_or(m) };
    let days = (end - from).num_days().max(1);
    Some(((left / Decimal::from(days)).round_dp(2), left, days, through))
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    // the answer first: what you can spend a day, then what's ahead
    let (summary, nums) = crate::summary_row(
        ("Daily Budget Allowance", "Remaining variable budget divided by the days your statements do not cover. This is a planning allowance, not available cash or a guarantee that you can afford another purchase. Recent spending and outstanding commitments may reduce it."),
        &[("Lowest Balance Ahead", "The lowest an account is expected to get before your next statement, and roughly when. It shows an account heading below zero first; otherwise your main account. Red means it would go overdrawn unless something changes."), ("Bills This Month", "All your regular bills for this month added up, and how many already show on a statement. The rest should appear once the next statements arrive. The Bills list further down shows each one."), ("Age of Money", "How many days money sits between arriving and being spent. Each purchase spends the oldest money still in your accounts. Under 30 days means you're spending this month's pay as it arrives; over 30 means you're living on last month's money, which is the goal.")],
    );
    let [(safe_v, safe_s), (low_v, low_s), (bill_v, bill_s), (age_v, age_s)] = <[(gtk::Label, gtk::Label); 4]>::try_from(nums).unwrap();
    body.append(&summary);
    let chart = Chart::new(260);
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
    card.append(&info_heading(
        "Balances Until Your Next Statement",
        "Each line is one bank account's estimated balance, starting from its last verified balance. Drops are bills and card payments on their usual days, jumps are pay, and the gentle slope is everyday spending. If a line dips below zero, that account is likely to go overdrawn around that date. Hover for each day's estimate. Regular payments in and out (pay, bills, card payments, standing orders) land on their usual days; everything else is spread evenly at the last 90 days' rate.",
    ));
    card.append(&dim(&format!(
        "An estimate from each account's last verified balance, {AHEAD} days ahead. The next statement replaces it."
    )));
    card.append(&chart.widget);
    body.append(&card);
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).build();
    body.append(&content);
    let age_chart = Chart::new(200);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1000).child(&body).build()).vexpand(true).build();

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let ccy = a.display();
            let fx = a.db.fx().unwrap_or_default();
            let money = |d: Decimal| fmt_money(d, ccy);
            let ids = a.entity_ids();
            let rows = a.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
            let docs = a.db.documents(&ids).unwrap_or_default();
            let fcs = forecasts(&a);
            let t = today();

            // ── the ribbon
            match daily_budget_allowance(&a) {
                Some((daily, left, days, through)) => {
                    safe_v.set_label(&format!("{} a day", money(daily.max(Decimal::ZERO))));
                    safe_v.set_css_classes(&["value", "tnum", if daily.is_sign_negative() { "amount-out" } else { "amount-in" }]);
                    safe_s.set_label(&format!(
                        "{} left in variable budgets over the {days} days {}. Not an available cash balance. Subtract spending since your statements.",
                        money(left),
                        if through >= month_start(t) { format!("after your newest statement ({})", through.format("%d %b")) } else { "of this month".into() }
                    ));
                }
                None => {
                    safe_v.set_label("—");
                    safe_s.set_label("Set up a budget to see a daily planning allowance");
                }
            }
            let aom = core::age_of_money(&rows, &docs, ccy, &fx);
            match &aom {
                Some(x) => {
                    age_v.set_label(&format!("{} days", x.days));
                    let prev = x.by_month.iter().rev().nth(3).map(|p| p.1);
                    age_s.set_label(&format!(
                        "As of {}. {}Aim for 30+: you're spending last month's money, not waiting for payday.",
                        x.as_of.format("%d %b"),
                        prev.map(|p| format!("{} {} days since 3 months ago. ", if x.days >= p { "Up" } else { "Down" }, (x.days - p).abs())).unwrap_or_default()
                    ));
                }
                None => {
                    age_v.set_label("—");
                    age_s.set_label("Needs statements with money in and out");
                }
            }
            // an account heading below zero, else the one most bills go through (your main account)
            let lows: Vec<(&Forecast, (NaiveDate, Decimal))> = fcs.iter().filter_map(|fc| fc.low(t, t + Duration::days(AHEAD)).map(|l| (fc, l))).collect();
            // an account going below the minimum you set for it comes first, then one heading below zero
            let below = |fc: &Forecast, v: Decimal| v < a.db.min_balance(fc.account_id);
            let low = lows.iter().filter(|x| below(x.0, x.1.1)).min_by_key(|x| x.1.0).or_else(|| lows.iter().max_by_key(|x| x.0.events.len())).copied();
            match low {
                Some((fc, (d, v))) => {
                    let min = a.db.min_balance(fc.account_id);
                    low_v.set_label(&fmt_money(v, fc.currency));
                    low_v.set_css_classes(&["value", "tnum", if v.is_sign_negative() { "amount-out" } else if v < min { "warning" } else { "" }]);
                    let why = if v.is_sign_negative() {
                        " — overdrawn unless something changes".to_string()
                    } else if v < min {
                        format!(" — under your {} minimum", fmt_money(min, fc.currency))
                    } else {
                        String::new()
                    };
                    low_s.set_label(&format!("{} around {}{why}", fc.account, d.format("%d %b")));
                }
                None => {
                    low_v.set_label("—");
                    low_s.set_label("No bank account balances yet");
                }
            }
            let bills = core::bills_for(&rows, &core::covered(&docs), month_start(t), ccy, &fx);
            let paid = bills.iter().filter(|b| matches!(b.state, BillState::Paid(_))).count();
            let total: Decimal = bills.iter().map(|b| b.amount).sum();
            bill_v.set_label(&money(total));
            bill_s.set_label(&format!("{} expected payments: {paid} seen on statements so far, the rest after the next ones arrive", bills.len()));

            // ── the estimate, one line per account (in the display currency)
            // a dormant account's old statement shouldn't stretch the chart back for months
            let start = fcs.iter().map(|fc| fc.start.0).min().unwrap_or(t).max(t - Duration::days(30));
            let end = t + Duration::days(AHEAD);
            let x = |d: NaiveDate| (d - start).num_days() as f64;
            let mut labels: Vec<(f64, String)> = Vec::new();
            let mut d = start;
            while d <= end {
                labels.push((x(d), d.format("%d %b").to_string()));
                d += Duration::days(7);
            }
            let series: Vec<LineSeries> = fcs
                .iter()
                .enumerate()
                .map(|(i, fc)| LineSeries { name: crate::transactions::short_account(&fc.account), points: fc.points.iter().filter(|p| p.0 >= start).map(|p| (x(p.0), f(fx.convert(p.1, fc.currency, ccy).unwrap_or_default()))).collect(), color: charts::PALETTE[i % charts::PALETTE.len()], dashed: false })
                .collect();
            let mut stops = Vec::new();
            let mut d = start;
            while d <= end {
                let parts: Vec<String> = fcs.iter().filter_map(|fc| fc.at(d).map(|v| format!("{} {}", fc.account, fmt_money(v, fc.currency)))).collect();
                stops.push((x(d), format!("{}{}: {}", d.format("%a %d %b"), if d == t { " (today)" } else { "" }, parts.join(" · "))));
                d += Duration::days(1);
            }
            let sym = ccy.symbol();
            *chart.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            chart.set(if fcs.is_empty() { Data::Empty("No bank account statements yet".into()) } else { Data::Lines { x_labels: labels, series, band: None, stops } });

            clear(&content);
            if !fcs.is_empty() {
                content.append(&info_heading(
                    "By Account",
                    "Each bank account's estimated balance today: its last verified balance plus the payments expected since. The line below gives its low point ahead, the next regular payment, and the usual day-to-day spend. Once a newer statement arrives, it also shows how close the last estimate was.",
                ));
                let checks = a.db.forecast_checks();
                let l = list();
                for fc in &fcs {
                    let m = |d: Decimal| fmt_money(d, fc.currency);
                    let today_est = fc.at(t).unwrap_or(fc.start.1);
                    // two short lines instead of one run-on: where it stands, then what's coming
                    let mut first = vec![format!("Verified {} on {}", m(fc.start.1), fc.start.0.format("%d %b"))];
                    if let Some((d, v)) = fc.low(t, end) {
                        first.push(format!("lowest {} around {}", m(v), d.format("%d %b")));
                    }
                    let mut second = Vec::new();
                    if let Some(e) = fc.events.iter().find(|e| e.0 > t) {
                        second.push(format!("Next: {} {} on {}", e.1, m(e.2), e.0.format("%d %b")));
                    }
                    if !fc.daily.is_zero() {
                        second.push(format!("{} about {} a day otherwise", if fc.daily.is_sign_negative() { "spends" } else { "gains" }, m(fc.daily.abs())));
                    }
                    let mut sub = vec![first.join(" · "), second.join(" · ")];
                    // the minimum you keep in it, and the first payment that would take it under
                    let min = a.db.min_balance(fc.account_id);
                    match fc.first_below(min, t, end) {
                        Some(sh) => sub.push(sh.label(fc.currency)),
                        None if !min.is_zero() => sub.push(format!("Stays above your {} minimum for the next {AHEAD} days", m(min))),
                        None => {}
                    }
                    // how the last remembered estimate did, once a statement covers its date
                    if let Some((when, est, made)) = checks.get(&fc.account_id)
                        && fc.start.0 >= *when
                    {
                        let actual = fingraph_core::wealth::account_balances(&rows, &docs).get(&fc.account_id).and_then(|x| fingraph_core::wealth::value_at(&x.points, *when));
                        if let Some(act) = actual {
                            sub.push(format!("estimated {} for {} on {}; the statement says {} (off by {})", m(*est), when.format("%d %b"), made.format("%d %b"), m(act), m((act - est).abs())));
                        }
                    }
                    let r = adw::ActionRow::builder().title(glib::markup_escape_text(&fc.account)).subtitle(glib::markup_escape_text(&sub.iter().filter(|l| !l.is_empty()).cloned().collect::<Vec<_>>().join("\n"))).subtitle_lines(5).build();
                    let keep = gtk::Entry::builder()
                        .placeholder_text("Minimum")
                        .text(if min.is_zero() { String::new() } else { min.normalize().to_string() })
                        .width_chars(10)
                        .max_width_chars(10)
                        .valign(gtk::Align::Center)
                        .tooltip_text(format!("The lowest balance you want this account to reach ({}). Enter to save; empty means just not overdrawn.", fc.currency.code()))
                        .build();
                    {
                        let (w, id) = (Rc::downgrade(&a), fc.account_id);
                        keep.connect_activate(move |e| {
                            if let Some(a) = w.upgrade() {
                                let v = fingraph_core::money::parse_user(&e.text());
                                if a.saved(a.db.set_min_balance(id, v)) {
                                    a.refresh();
                                }
                            }
                        });
                    }
                    r.add_suffix(&keep);
                    r.add_suffix(&gtk::Label::builder().label(format!("≈ {} today", m(today_est))).css_classes(["tnum", "heading", if today_est.is_sign_negative() { "amount-out" } else { "" }]).build());
                    l.append(&r);
                }
                content.append(&l);

                // ── the next 30 days
                let mut coming: Vec<(NaiveDate, String, Decimal, String, fingraph_core::Currency)> = fcs.iter().flat_map(|fc| fc.events.iter().filter(|e| e.0 > t && e.0 <= t + Duration::days(30)).map(|e| (e.0, e.1.clone(), e.2, fc.account.clone(), fc.currency))).collect();
                coming.sort_by_key(|c| c.0);
                if !coming.is_empty() {
                    content.append(&info_heading(
                        "Coming Up · Next 30 Days",
                        "Regular payments expected in the next 30 days, placed on the day they usually happen in your past statements. A plus sign means money coming in.",
                    ));
                    content.append(&dim("Regular payments in and out of your bank accounts, by their usual day."));
                    let l = list();
                    for (d, who, v, acct, cur) in coming {
                        let r = adw::ActionRow::builder().title(glib::markup_escape_text(&who)).subtitle(format!("{} · {acct}", d.format("%a %d %b"))).build();
                        r.add_suffix(&gtk::Label::builder().label(format!("{}{}", if v.is_sign_positive() { "+" } else { "" }, fmt_money(v, cur))).css_classes(["tnum", if v.is_sign_positive() { "amount-in" } else { "" }]).build());
                        l.append(&r);
                    }
                    content.append(&l);
                }
            }

            // ── this month's bills
            content.append(&info_heading(
                &format!("Bills in {}", t.format("%B")),
                "Each regular bill and whether this month's statements show it. A tick means paid. A warning means it was due but isn't on the statement, so check whether it was missed or paid another way. The waiting icon means it's due after your newest statement. A star means a new regular payment.",
            ));
            if bills.is_empty() {
                content.append(&dim("No regular bills found yet."));
            } else {
                let l = list();
                for b in &bills {
                    let (sub, icon) = match &b.state {
                        BillState::Paid(d) => (format!("Paid {}", d.iter().map(|d| d.format("%d %b").to_string()).collect::<Vec<_>>().join(", ")), "emblem-ok-symbolic"),
                        BillState::Missed => (format!("Due around {} — not on the statement", b.due.format("%d %b")), "dialog-warning-symbolic"),
                        BillState::NotYetSeen => (format!("Due around {} — after your newest statement", b.due.format("%d %b")), "content-loading-symbolic"),
                        BillState::New => (format!("New, first on {}", b.due.format("%d %b")), "starred-symbolic"),
                    };
                    let r = adw::ActionRow::builder().title(glib::markup_escape_text(&b.payee)).subtitle(format!("{sub} · {}", b.account)).build();
                    r.add_prefix(&gtk::Image::from_icon_name(icon));
                    r.add_suffix(&gtk::Label::builder().label(money(b.amount)).css_classes(["tnum"]).build());
                    l.append(&r);
                }
                content.append(&l);
            }

            // ── age of money
            if let Some(x) = aom {
                let c = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
                c.append(&info_heading(
                    "Age of Money",
                    "Your age of money at each month end, with a dashed line at 30 days. Above that line means you're paying for things with money that arrived more than a month earlier, a sign of a healthy buffer. A rising line means the buffer is growing.",
                ));
                c.append(&dim("How long money sits between arriving and being spent: income is stacked oldest first and each purchase spends the oldest money still there (the average of the last 10 purchases). Card purchases count when made, not when the card is paid."));
                let pts: Vec<(f64, f64)> = x.by_month.iter().enumerate().map(|(i, p)| (i as f64, p.1 as f64)).collect();
                *age_chart.axis.borrow_mut() = Rc::new(|v| format!("{v:.0}d"));
                age_chart.set(Data::Lines {
                    x_labels: x.by_month.iter().enumerate().map(|(i, p)| (i as f64, p.0.format("%b").to_string())).collect(),
                    series: vec![
                        LineSeries { name: "Age of money".into(), points: pts, color: charts::TEAL, dashed: false },
                        LineSeries { name: "30 days".into(), points: (0..x.by_month.len()).map(|i| (i as f64, 30.0)).collect(), color: charts::SLATE, dashed: true },
                    ],
                    band: None,
                    stops: x.by_month.iter().enumerate().map(|(i, p)| (i as f64, format!("{}: {} days", p.0.format("%d %b %Y"), p.1))).collect(),
                });
                if let Some(p) = age_chart.widget.parent().and_downcast::<gtk::Box>() {
                    p.remove(&age_chart.widget);
                }
                c.append(&age_chart.widget);
                content.append(&c);
            }
        })
    };
    (page.upcast(), refresh)
}
