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

    // KPI ribbon
    let ribbon = gtk::Box::builder().spacing(12).homogeneous(true).build();
    // what needs an answer from you, shown only while something does
    let review = adw::Banner::builder().button_label("Review").revealed(false).build();
    review.set_action_name(Some("win.show-review"));

    let (c1, k_net) = kpi_card(
        "Net Cash Flow",
        "Money in minus money out over the date range at the top. Transfers between your own accounts don't count as either. Green: more came in than went out. Red: you spent more than came in. The percentage compares it with the same length of time just before.",
    );
    let (c2, k_rate) = kpi_card(
        "Savings / Operating Margin",
        "The share of your income you kept: (income − expenses) ÷ income. 20% means you kept 20 cents of every dollar that came in. For a business it's the operating margin. Below zero means you spent more than you earned.",
    );
    let (c3, k_subs) = kpi_card(
        "Active Subscription Burn",
        "What the subscriptions still being charged cost you a month, with the yearly total below. USD ones are converted. The Subscriptions page lists each one.",
    );
    let (c4, k_rec) = kpi_card(
        "Reconciliation & Triage",
        "How many statements add up to the cent: the opening balance plus every transaction read equals the closing balance the bank printed. If one doesn't, the Statements page shows it. Uncategorized is how many transactions still have no category.",
    );
    let (c5, k_age) = kpi_card(
        "Age of Money",
        "How many days money sits between arriving and being spent. Each purchase spends the oldest money still in your accounts. Under 30 days means you're spending this month's pay as it arrives; over 30 means you're living on last month's money, which is the goal.",
    );
    for c in [&c1, &c2, &c3, &c4, &c5] {
        ribbon.append(c);
    }
    body.append(&ribbon);
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
    let back = gtk::Button::builder().label("All categories").icon_name("go-previous-symbolic").css_classes(["flat"]).halign(gtk::Align::Start).visible(false).build();
    cat_card.append(&back);
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

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (monthly, donut, breakdown, burn, pareto, balances, pnl) = (monthly.clone(), donut.clone(), breakdown.clone(), burn.clone(), pareto.clone(), balances.clone(), pnl.clone());
        let (stack, drill, cat_title, back, review) = (stack.clone(), drill.clone(), cat_title.clone(), back.clone(), review.clone());
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
            let k = insights::kpis(&rows, &docs, from, to, ccy, &fx, today());
            k_net.value.set_label(&money(k.net));
            k_net.value.set_css_classes(&["value", "tnum", if k.net.is_sign_negative() { "amount-out" } else { "amount-in" }]);
            k_net.sub.set_label(&format!(
                "Income {} · Expenses {}{}",
                money(k.income),
                money(k.expenses),
                k.net_delta_pct.map(|p| format!(" · {}{p:.0}% vs prior period", if p >= 0.0 { "+" } else { "" })).unwrap_or_default()
            ));
            k_rate.value.set_label(&k.savings_rate_pct.map_or("—".into(), |p| format!("{p:.1}%")));
            k_rate.sub.set_label("(Income − Expenses) ÷ Income, transfers excluded");
            k_subs.value.set_label(&format!("{}/mo", money(k.subs_monthly)));
            k_subs.sub.set_label(&format!("{} a year across KYD & USD", money(k.subs_annual)));
            k_rec.value.set_label(&format!("{}/{} Reconciled", k.reconciled, k.statements));
            k_rec.sub.set_label(&format!("{} statements verified to the cent · {} uncategorized", k.reconciled, k.uncategorized));
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
            {
                let (drill, sb) = (drill.clone(), show_breakdown.clone());
                let back2 = back.clone();
                // one handler: reconnecting on each refresh would stack them
                if back2.widget_name() != "wired" {
                    back2.set_widget_name("wired");
                    back2.connect_clicked(move |_| {
                        *drill.borrow_mut() = None;
                        sb();
                    });
                }
            }

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
