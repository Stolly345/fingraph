//! Two of the Reports tabs: Cash Flow and Trends. Each is one large chart with a ranked breakdown
//! beside it, over the twelve months ending with the newest statement.

use adw::prelude::*;
use chrono::Duration;
use fingraph_core::Decimal;
use fingraph_core::insights::{self, add_months, month_start};
use fingraph_core::money::fmt_money;
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, Series};
use crate::{App, clear, heading, info_heading, today};

const MONTHS: i32 = 12;
/// Trends names this many categories; the rest are added together.
const NAMED: usize = 9;

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// The chart on the left, its breakdown on the right.
fn layout(title: &str, help: &str, chart: &Chart) -> (gtk::Widget, gtk::Box, gtk::Box) {
    let stats = gtk::Box::builder().spacing(24).homogeneous(true).css_classes(["card", "stat-strip"]).build();
    let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).width_request(310).css_classes(["card", "kpi"]).build();
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["card", "chart-card"]).build();
    card.append(&info_heading(title, help));
    card.append(&chart.widget);
    let row = gtk::Box::builder().spacing(12).build();
    row.append(&card);
    row.append(&side);
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    body.append(&stats);
    body.append(&row);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1300).child(&body).build()).vexpand(true).build();
    (page.upcast(), stats, side)
}

fn stat(into: &gtk::Box, title: &str, value: &str, class: &str) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).css_classes(["stat"]).build();
    b.append(&gtk::Label::builder().label(title).xalign(0.0).css_classes(["caption-heading", "dim-label"]).build());
    b.append(&gtk::Label::builder().label(value).xalign(0.0).css_classes(["value", "tnum", class]).build());
    into.append(&b);
}

/// The month the twelve months end with: the newest with statements, not an empty current one.
fn last_month(app: &App) -> chrono::NaiveDate {
    app.db.documents(&app.entity_ids()).unwrap_or_default().iter().filter_map(|d| d.period.map(|p| p.1)).max().unwrap_or(today()).min(today())
}

pub(crate) fn average(app: &App, rows: &[fingraph_core::db::TxRow]) -> fingraph_core::reporting::FlowAverage {
    app.db.cash_flow_average(rows, &app.entity_ids(), last_month(app), MONTHS, app.display()).unwrap_or_default()
}

pub fn build_cash_flow(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let chart = Chart::new(420);
    let (page, stats, side) = layout(
        "Money In and Out",
        "For each month: money in, money out, and the difference as a line. Transfers between your own accounts aren't counted. Hover a month for the numbers.",
        &chart,
    );
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let s = crate::budget::snap(&a);
            let bars = insights::monthly(&s.rows, last_month(&a), MONTHS, s.ccy, &s.fx);
            let money = |d: Decimal| fmt_money(d, s.ccy);
            let average = average(&a, &s.rows);
            let monthly = average.monthly();
            clear(&stats);
            let amount = |v: Option<Decimal>| v.map(money).unwrap_or_else(|| "—".into());
            stat(&stats, "Average In", &amount(monthly.map(|v| v.0)), "");
            stat(&stats, "Average Out", &amount(monthly.map(|v| v.1)), "");
            let net = monthly.map(|(i, e)| i - e);
            stat(&stats, "Average Net", &amount(net), if net.is_some_and(|v| v.is_sign_negative()) { "amount-out" } else { "amount-in" });
            stats.set_tooltip_text(Some(&average.label()));
            let sym = s.ccy.symbol();
            *chart.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            chart.set(Data::Bars {
                labels: bars.iter().map(|b| format!("{}{}", b.month.format("%b"), if average.months.contains(&b.month) { "" } else { "*" })).collect(),
                series: vec![Series { name: "In".into(), values: bars.iter().map(|b| f(b.income)).collect(), color: charts::BLUE }, Series { name: "Out".into(), values: bars.iter().map(|b| f(b.expenses)).collect(), color: charts::ORANGE }],
                line: Some(Series { name: "Net".into(), values: bars.iter().map(|b| f(b.net)).collect(), color: charts::GREEN }),
                tips: bars.iter().map(|b| format!("{}\nIn {} · Out {} · Net {}", b.month.format("%B %Y"), money(b.income), money(b.expenses), money(b.net))).collect(),
            });
            clear(&side);
            let note = crate::dim(&average.label()); note.set_max_width_chars(36); side.append(&note);
            let note = crate::dim("* Partial or missing coverage; excluded from averages"); note.set_max_width_chars(36); side.append(&note);
            side.append(&heading("Month by Month"));
            for b in bars.iter().rev() {
                let line = gtk::Box::builder().build();
                line.append(&gtk::Label::builder().label(format!("{}{}", b.month.format("%B %Y"), if average.months.contains(&b.month) { "" } else { "*" })).xalign(0.0).hexpand(true).build());
                line.append(&gtk::Label::builder().label(money(b.net)).css_classes(["tnum", if b.net.is_sign_negative() { "amount-out" } else { "amount-in" }]).build());
                side.append(&line);
            }
        })
    };
    (page, refresh)
}

pub fn build_trends(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let chart = Chart::new(420);
    let (page, stats, side) = layout(
        "Spending Trends",
        "Each month's spending, stacked by category, so you can see a category growing or shrinking over time. Hover a month for the numbers. Transfers aren't spending.",
        &chart,
    );
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let s = crate::budget::snap(&a);
            let money = |d: Decimal| fmt_money(d, s.ccy);
            let end = month_start(last_month(&a));
            let months: Vec<_> = (0..MONTHS).map(|i| add_months(end, i - MONTHS + 1)).collect();
            let per: Vec<BTreeMap<String, Decimal>> = months.iter().map(|m| insights::by_category(&s.rows, *m, add_months(*m, 1) - Duration::days(1), s.ccy, &s.fx, &s.parents).into_iter().collect()).collect();
            let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
            for (k, v) in per.iter().flatten() {
                *totals.entry(k.clone()).or_default() += *v;
            }
            let mut ranked: Vec<(String, Decimal)> = totals.into_iter().collect();
            ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
            let all: Decimal = ranked.iter().map(|x| x.1).sum();
            let named: Vec<&(String, Decimal)> = ranked.iter().take(NAMED).collect();
            let color = |n: &str, i: usize| if n == "Uncategorized" { charts::SLATE } else { charts::category_color(n).unwrap_or(charts::PALETTE[i % charts::PALETTE.len()]) };
            let mut series: Vec<Series> = named.iter().enumerate().map(|(i, (n, _))| Series { name: n.clone(), values: per.iter().map(|p| f(p.get(n).copied().unwrap_or_default())).collect(), color: color(n, i) }).collect();
            if ranked.len() > NAMED {
                let others: Vec<f64> = per.iter().map(|p| f(p.iter().filter(|(k, _)| !named.iter().any(|x| x.0 == **k)).map(|x| *x.1).sum())).collect();
                series.push(Series { name: "Everything else".into(), values: others, color: charts::BROWN });
            }
            clear(&stats);
            stat(&stats, "Total Spending", &money(all), "");
            let average = average(&a, &s.rows);
            stat(&stats, "Average a Complete Month", &average.monthly().map(|v| money(v.1)).unwrap_or_else(|| "—".into()), "");
            stats.set_tooltip_text(Some(&average.label()));
            stat(&stats, "Biggest Category", named.first().map_or("—", |x| x.0.as_str()), "");
            let sym = s.ccy.symbol();
            *chart.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            chart.set(Data::Stacked {
                labels: months.iter().map(|m| format!("{}{}", m.format("%b"), if average.months.contains(m) { "" } else { "*" })).collect(),
                tips: months.iter().zip(&per).map(|(m, p)| {
                    let mut top: Vec<(&String, &Decimal)> = p.iter().collect();
                    top.sort_by_key(|x| std::cmp::Reverse(*x.1));
                    let lines: Vec<String> = top.iter().take(5).map(|(n, v)| format!("{n}: {}", money(**v))).collect();
                    format!("{}: {}\n{}", m.format("%B %Y"), money(p.values().copied().sum()), lines.join("\n"))
                }).collect(),
                series,
            });
            clear(&side);
            let note = crate::dim(&average.label()); note.set_max_width_chars(36); side.append(&note);
            side.append(&crate::dim("* Partial or missing coverage"));
            side.append(&heading("Categories"));
            let top = named.first().map_or(Decimal::ONE, |x| x.1).max(Decimal::ONE);
            for (n, v) in named {
                let row = gtk::Box::builder().spacing(10).build();
                row.append(&a.category_tile(n));
                let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3).hexpand(true).valign(gtk::Align::Center).build();
                let line = gtk::Box::builder().build();
                line.append(&gtk::Label::builder().label(n).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
                line.append(&gtk::Label::builder().label(money(*v)).css_classes(["tnum", "caption"]).build());
                col.append(&line);
                let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(f(*v / top)).build();
                for o in ["low", "high", "full"] {
                    bar.remove_offset_value(Some(o));
                }
                col.append(&bar);
                row.append(&col);
                side.append(&row);
            }
        })
    };
    (page, refresh)
}
