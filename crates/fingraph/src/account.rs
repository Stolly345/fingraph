//! One account's overview (opened from the sidebar's Accounts): what went out and came in, where it
//! went by category and merchant, month by month, and its balance, in the account's own currency.

use adw::prelude::*;
use chrono::Datelike;
use fingraph_core::Decimal;
use fingraph_core::db::{TxFilter, TxRow};
use fingraph_core::insights;
use fingraph_core::money::fmt_money;
use rust_decimal::prelude::ToPrimitive;
use std::cell::Cell;
use std::rc::Rc;

use crate::charts::{self, Chart, Data, LineSeries, Series};
use crate::{App, dim, heading, today};

thread_local! {
    static CURRENT: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Open the overview for this account.
pub fn show(app: &Rc<App>, id: i64) {
    CURRENT.with(|c| c.set(Some(id)));
    app.show_view("account");
}

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

fn card(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).css_classes(["card", "chart-card"]).build();
    b.append(&heading(title));
    b.append(child);
    b
}

fn kpi(title: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes(["card", "kpi"]).build();
    b.append(&gtk::Label::builder().label(title).xalign(0.0).css_classes(["dim-label", "caption-heading"]).build());
    let v = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let s = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
    b.append(&v);
    b.append(&s);
    (b, v, s)
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let head = gtk::Box::builder().spacing(12).build();
    let titles = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
    let name = heading("");
    name.add_css_class("title-2");
    let sub = dim("");
    titles.append(&name);
    titles.append(&sub);
    head.append(&titles);
    let all = gtk::Button::builder().label("Show Transactions").css_classes(["pill"]).valign(gtk::Align::Center).build();
    head.append(&all);
    body.append(&head);

    let ribbon = gtk::Box::builder().spacing(12).homogeneous(true).build();
    let (k1, spent, spent_s) = kpi("Spent");
    let (k2, recv, recv_s) = kpi("Received");
    let (k3, xfer, xfer_s) = kpi("Transfers");
    let (k4, bal, bal_s) = kpi("Balance");
    for k in [&k1, &k2, &k3, &k4] {
        ribbon.append(k);
    }
    body.append(&ribbon);

    let cats = Chart::new(260);
    let merchants = Chart::new(260);
    let monthly = Chart::new(240);
    let balance = Chart::new(240);
    let grid = gtk::Grid::builder().column_spacing(12).row_spacing(12).column_homogeneous(true).build();
    grid.attach(&card("Where It Went · by Category", &cats.widget), 0, 0, 1, 1);
    grid.attach(&card("Top Merchants", &merchants.widget), 1, 0, 1, 1);
    grid.attach(&card("Month by Month", &monthly.widget), 0, 1, 1, 1);
    grid.attach(&card("Balance", &balance.widget), 1, 1, 1, 1);
    body.append(&grid);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1400).child(&body).build()).vexpand(true).build();

    {
        let w = Rc::downgrade(app);
        all.connect_clicked(move |_| {
            if let (Some(a), Some(id)) = (w.upgrade(), CURRENT.with(Cell::get)) {
                crate::transactions::show_account(&a, id);
            }
        });
    }

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let (Some(a), Some(id)) = (w.upgrade(), CURRENT.with(Cell::get)) else { return };
            let Some(acct) = a.db.accounts().unwrap_or_default().into_iter().find(|x| x.id == id) else { return };
            let ccy = acct.currency;
            let money = |d: Decimal| fmt_money(d, ccy);
            let fx = a.db.fx().unwrap_or_default();
            let parents = a.parents();
            let (from, to) = a.range();
            let all_rows: Vec<TxRow> = a.db.transactions(&TxFilter { account_id: Some(id), split_lines: true, ..Default::default() }).unwrap_or_default();
            let rows: Vec<TxRow> = all_rows.iter().filter(|t| t.posted >= from && t.posted <= to).cloned().collect();
            let docs: Vec<_> = a.db.documents(&[]).unwrap_or_default().into_iter().filter(|d| d.account_id == id).collect();

            name.set_label(&acct.name);
            a.content_title.set_title(&acct.name);
            let reconciled = docs.iter().filter(|d| d.reconciliation == fingraph_core::Reconciliation::Reconciled).count();
            sub.set_label(&format!("{} · {} · {} statements, {reconciled} reconciled · amounts in {ccy}", if acct.bank_short.is_empty() { "Bank" } else { &acct.bank_short }, acct.account_type.label(), docs.len()));

            let (mut out, mut inn, mut t_out, mut t_in) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
            for t in &rows {
                match (t.is_transfer(), t.amount.is_sign_negative()) {
                    (false, true) => out -= t.amount,
                    (false, false) => inn += t.amount,
                    (true, true) => t_out -= t.amount,
                    (true, false) => t_in += t.amount,
                }
            }
            let n_out = rows.iter().filter(|t| !t.is_transfer() && t.amount.is_sign_negative()).count();
            spent.set_label(&money(out));
            spent_s.set_label(&format!("{n_out} purchases and charges"));
            recv.set_label(&money(inn));
            recv_s.set_label(if acct.account_type.is_liability() { "Refunds and credits (payments are transfers)" } else { "Income, refunds and other credits" });
            xfer.set_label(&format!("{} in", money(t_in)));
            xfer_s.set_label(&format!(
                "{} out · {}",
                money(t_out),
                if acct.account_type == fingraph_core::AccountType::CreditCard { "card payments and moves between your accounts" } else { "moves between your own accounts and card payments" }
            ));
            match insights::latest_balances(&all_rows, &docs).get(&id) {
                Some((d, v)) => {
                    bal.set_label(&money(*v));
                    bal_s.set_label(&format!("As of {} (from your statements){}", d.format("%d %b %Y"), if acct.account_type.is_liability() { " · negative = owed" } else { "" }));
                }
                None => {
                    bal.set_label("—");
                    bal_s.set_label("No balance printed yet");
                }
            }

            // where it went
            let by = insights::by_category(&rows, from, to, ccy, &fx, &parents);
            cats.set(if by.is_empty() {
                Data::Empty("No spending in this range".into())
            } else {
                Data::HBars {
                    items: by.iter().take(10).map(|(n, v)| (n.clone(), f(*v))).collect(),
                    values: by.iter().take(10).map(|(_, v)| money(*v)).collect(),
                    tips: by.iter().take(10).map(|(n, v)| format!("{n}: {}\nClick to list them", money(*v))).collect(),
                }
            });
            {
                let (w, by, rows, parents) = (Rc::downgrade(&a), by.clone(), rows.clone(), parents.clone());
                cats.on_click(move |i| {
                    let (Some(a), Some((cat, _))) = (w.upgrade(), by.get(i)) else { return };
                    let top = |t: &TxRow| t.category_id.map_or("Uncategorized".to_string(), |c| parents.get(&c).cloned().unwrap_or_else(|| t.category_name.clone().unwrap_or_default()));
                    a.show_transactions(rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer() && top(t) == *cat).map(|t| t.id).collect());
                });
            }
            let top = insights::pareto(&rows, from, to, ccy, &fx);
            merchants.set(if top.is_empty() {
                Data::Empty("No spending in this range".into())
            } else {
                Data::HBars {
                    items: top.iter().map(|(n, v, _)| (n.clone(), f(*v))).collect(),
                    values: top.iter().map(|(_, v, _)| money(*v)).collect(),
                    tips: top.iter().map(|(n, v, c)| format!("{n}: {}\n{c:.0}% of this account's spending so far\nClick to list them", money(*v))).collect(),
                }
            });
            {
                let (w, top, rows) = (Rc::downgrade(&a), top.clone(), rows.clone());
                merchants.on_click(move |i| {
                    if let (Some(a), Some((n, _, _))) = (w.upgrade(), top.get(i)) {
                        a.show_transactions(rows.iter().filter(|t| &t.clean_payee == n).map(|t| t.id).collect());
                    }
                });
            }

            // month by month: twelve months up to the range end
            let end = to.min(today());
            let mb = insights::monthly(&all_rows, end, 12, ccy, &fx);
            let sym = ccy.symbol();
            for c in [&cats, &merchants, &monthly, &balance] {
                *c.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            }
            monthly.set(Data::Bars {
                labels: mb.iter().map(|m| m.month.format("%b").to_string()).collect(),
                series: vec![
                    Series { name: "In".into(), values: mb.iter().map(|m| f(m.income)).collect(), color: charts::GREEN },
                    Series { name: "Out".into(), values: mb.iter().map(|m| f(m.expenses)).collect(), color: charts::RED },
                ],
                line: None,
                tips: mb.iter().map(|m| format!("{}\nIn {}\nOut {}", m.month.format("%B %Y"), money(m.income), money(m.expenses))).collect(),
            });

            // balance over the same twelve months
            let pts: Vec<(chrono::NaiveDate, Decimal)> = insights::balances(&all_rows, &docs, ccy, &fx).into_iter().flat_map(|s| s.points).collect();
            let start = insights::add_months(insights::month_start(end), -11);
            let shown: Vec<_> = pts.iter().filter(|p| p.0 >= start && p.0 <= end).collect();
            let ord = |d: chrono::NaiveDate| d.num_days_from_ce() as f64;
            let mut labels = Vec::new();
            let mut m = start;
            while m <= end {
                labels.push((ord(m), m.format("%b").to_string()));
                m = insights::add_months(m, 1);
            }
            balance.set(if shown.is_empty() {
                Data::Empty("No balances printed for this account".into())
            } else {
                Data::Lines {
                    x_labels: labels,
                    band: None,
                    series: vec![LineSeries { name: acct.name.clone(), points: shown.iter().map(|p| (ord(p.0), f(p.1))).collect(), color: charts::BLUE, dashed: false }],
                    stops: shown.iter().map(|p| (ord(p.0), format!("{}\n{}", p.0.format("%d %b %Y"), money(p.1)))).collect(),
                }
            });
        })
    };
    (page.upcast(), refresh)
}
