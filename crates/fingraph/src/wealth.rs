//! Net Worth: what you own minus what you owe over time (statement balances plus things you value
//! by hand), your goals, and how to pay the debts off fastest.

use adw::prelude::*;
use chrono::NaiveDate;
use fingraph_core::Decimal;
use fingraph_core::budget::scope;
use fingraph_core::db::TxFilter;
use fingraph_core::insights::month_start;
use fingraph_core::money::{Currency, fmt_money, parse_user};
use fingraph_core::wealth::{self as core, Debt, Goal, GoalStatus, ManualItem, Strategy};
use gtk::glib;
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::budget::{amount_text, parse_date};
use crate::charts::{self, Chart, Data, LineSeries};
use crate::{App, clear, dim, info_caption, info_heading, today};

const MONTHS: i32 = 24;

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

fn list() -> gtk::ListBox {
    gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build()
}

/// Everything the page reads.
struct Snap {
    ccy: Currency,
    fx: fingraph_core::Fx,
    rows: Vec<fingraph_core::db::TxRow>,
    accounts: BTreeMap<i64, core::AccountBalances>,
    items: Vec<ManualItem>,
    scope: String,
}

fn snap(app: &App) -> Snap {
    let ids = app.entity_ids();
    let rows = app.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
    let docs = app.db.documents(&ids).unwrap_or_default();
    let hidden: Vec<i64> = app.db.accounts().unwrap_or_default().iter().filter(|a| a.hidden).map(|a| a.id).collect();
    let mut accounts = core::account_balances(&rows, &docs);
    accounts.retain(|id, _| !hidden.contains(id));
    Snap { ccy: app.display(), fx: app.db.fx().unwrap_or_default(), accounts, items: app.db.manual_items(&ids).unwrap_or_default(), scope: scope(&ids), rows }
}

/// Net worth on a date, for Monthly Close's summary.
pub fn net_worth_at(app: &App, d: NaiveDate) -> Option<Decimal> {
    let s = snap(app);
    (!s.accounts.is_empty() || !s.items.is_empty()).then(|| core::net_worth_on(&s.accounts, &s.items, d, s.ccy, &s.fx).net())
}

/// A rate you typed for a card or loan beats the one read from its interest charges.
fn apr_for(app: &App, s: &Snap, id: i64, owed: Decimal) -> (Option<Decimal>, bool) {
    if let Some(v) = app.db.setting(&format!("apr:{id}")).and_then(|v| parse_user(&v)) {
        return (Some(v), true);
    }
    (core::estimate_apr(&s.rows, id, owed), false)
}

/// A linked account's balance in the goal's currency, and contributions recorded.
fn goal_inputs(app: &App, s: &Snap, g: &Goal) -> (Option<Decimal>, Decimal) {
    let bal = g.account_id.and_then(|id| s.accounts.get(&id)).and_then(|a| a.points.last().map(|p| s.fx.convert(p.1, a.currency, g.currency)));
    let entries = app.db.goal_entries(g.id).unwrap_or_default().values().copied().sum();
    (bal, entries)
}

fn status_pill(st: GoalStatus) -> gtk::Label {
    let class = match st {
        GoalStatus::Done | GoalStatus::Ahead => "success",
        GoalStatus::OnTrack => "accent",
        GoalStatus::AtRisk => "warning",
    };
    gtk::Label::builder().label(st.label()).valign(gtk::Align::Center).css_classes(["caption-heading", class]).build()
}

fn goal_row(app: &App, s: &Snap, g: &Goal) -> adw::ActionRow {
    let (bal, entries) = goal_inputs(app, s, g);
    let p = core::progress(g, bal, entries, today());
    let money = |d: Decimal| fmt_money(d, g.currency);
    let mut sub = vec![if g.paydown { format!("{} of {} paid off", money(p.done), money(g.target)) } else { format!("{} of {} saved", money(p.done), money(g.target)) }];
    if !g.monthly.is_zero() {
        sub.push(format!("{} a month", money(g.monthly)));
    }
    match (p.finish, g.target_date) {
        (Some(fin), Some(t)) => sub.push(format!("done around {} (target {})", fin.format("%b %Y"), t.format("%b %Y"))),
        (Some(fin), None) => sub.push(format!("done around {}", fin.format("%b %Y"))),
        _ => {}
    }
    if let (Some(n), GoalStatus::AtRisk) = (p.needed, p.status) {
        sub.push(format!("needs {} a month to make it", money(n)));
    }
    if let Some(a) = g.account_id.and_then(|id| s.accounts.get(&id)) {
        sub.push(format!("tracks {}", a.name));
    }
    let r = adw::ActionRow::builder().title(glib::markup_escape_text(&g.name)).subtitle(glib::markup_escape_text(&sub.join(" · "))).build();
    r.add_prefix(&gtk::Image::from_icon_name(if g.paydown { "go-down-symbolic" } else { "starred-symbolic" }));
    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(160).build();
    col.append(&status_pill(p.status));
    let share = if g.target.is_zero() { 0.0 } else { f(p.done / g.target).clamp(0.0, 1.0) };
    col.append(&gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(share).build());
    r.add_suffix(&col);
    r
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let ribbon = gtk::Box::builder().spacing(12).homogeneous(true).build();
    let kpi = |t: &str, help: &str| {
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes(["card", "kpi"]).build();
        b.append(&info_caption(t, help));
        let v = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).build();
        let s = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
        b.append(&v);
        b.append(&s);
        ribbon.append(&b);
        (v, s)
    };
    let (net_v, net_s) = kpi(
        "Net Worth",
        "Everything you own minus everything you owe. Bank balances come from your latest statements, and the things you've added by hand (a house, car, pension) are included too. Cards, loans and other debts are taken off.",
    );
    let (as_v, as_s) = kpi("Assets", "What you own: the balances of your bank accounts plus the assets you've added by hand.");
    let (de_v, de_s) = kpi("Debts", "What you owe: credit card and loan balances from your statements, plus the debts you've added by hand.");
    let (ch_v, ch_s) = kpi("Over 12 Months", "How much your net worth changed over the past year. Green means it grew, red means it shrank.");
    body.append(&ribbon);
    let chart = Chart::new(240);
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
    card.append(&info_heading(
        "Net Worth Over Time",
        "The blue line is your net worth at each month end. The dashed green line is what you own and the dashed red line is what you owe; blue is the gap between them. A rising blue line means you're getting ahead. Hover a month for the numbers.",
    ));
    card.append(&dim("Each month end: every account's last verified balance on or before it, plus the latest value of what you've added by hand. An account counts from its first statement; a month whose statement isn't in yet repeats the last balance."));
    card.append(&chart.widget);
    body.append(&card);
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).build();
    body.append(&content);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1000).child(&body).build()).vexpand(true).build();

    let me: Rc<std::cell::RefCell<Option<crate::Refresh>>> = Rc::default();
    let redraw: crate::Refresh = {
        let me = me.clone();
        Rc::new(move || {
            let f = me.borrow().clone();
            if let Some(f) = f {
                f();
            }
        })
    };
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let redraw = redraw.clone();
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let s = snap(&a);
            let ccy = s.ccy;
            let money = |d: Decimal| fmt_money(d, ccy);
            let end = today();
            // from the first month anything has a balance (not a run of empty months before it)
            let first = s.accounts.values().filter_map(|x| x.points.first().map(|p| p.0)).chain(s.items.iter().filter_map(|i| i.values.first().map(|v| v.0))).min();
            let series: Vec<core::NetWorth> = core::net_worth_series(&s.accounts, &s.items, end, MONTHS, ccy, &s.fx).into_iter().filter(|p| first.is_some_and(|f| p.date >= f)).collect();
            let now = *series.last().unwrap_or(&core::NetWorth { date: end, assets: Decimal::ZERO, debts: Decimal::ZERO });
            let year = series.get(series.len().saturating_sub(13)).copied().unwrap_or(now);
            let newest = s.accounts.values().filter_map(|x| x.points.last().map(|p| p.0)).max();
            net_v.set_label(&money(now.net()));
            net_s.set_label(&newest.map(|d| format!("Statements to {}", d.format("%d %b %Y"))).unwrap_or_else(|| "No statements yet".into()));
            as_v.set_label(&money(now.assets));
            as_s.set_label(&format!("{} accounts + {} items", s.accounts.values().filter(|x| !x.liability).count(), s.items.iter().filter(|i| !i.liability).count()));
            de_v.set_label(&money(now.debts));
            de_s.set_label("Cards, loans and debts you added");
            let ch = now.net() - year.net();
            ch_v.set_label(&format!("{}{}", if ch.is_sign_negative() { "−" } else { "+" }, money(ch.abs())));
            ch_v.set_css_classes(&["value", "tnum", if ch.is_sign_negative() { "amount-out" } else { "amount-in" }]);
            ch_s.set_label(&format!("Since {}", year.date.format("%b %Y")));
            let labels: Vec<(f64, String)> = series.iter().enumerate().map(|(i, p)| (i as f64, p.date.format("%b %y").to_string())).collect();
            let pts = |g: &dyn Fn(&core::NetWorth) -> Decimal| series.iter().enumerate().map(|(i, p)| (i as f64, f(g(p)))).collect::<Vec<_>>();
            let sym = ccy.symbol();
            *chart.axis.borrow_mut() = Rc::new(move |v| format!("{sym}{}", charts::compact(v)));
            chart.set(Data::Lines {
                x_labels: labels,
                band: None,
                series: vec![
                    LineSeries { name: "Net worth".into(), points: pts(&|p| p.net()), color: charts::BLUE, dashed: false },
                    LineSeries { name: "Assets".into(), points: pts(&|p| p.assets), color: charts::GREEN, dashed: true },
                    LineSeries { name: "Debts".into(), points: pts(&|p| p.debts), color: charts::RED, dashed: true },
                ],
                stops: series.iter().enumerate().map(|(i, p)| (i as f64, format!("{}: net {} · assets {} · debts {}", p.date.format("%d %b %Y"), money(p.net()), money(p.assets), money(p.debts)))).collect(),
            });

            clear(&content);
            // ── what it's made of
            content.append(&info_heading("Accounts", "Each account's latest balance a statement confirmed, and the date of that statement. Red is money owed, such as on a card or loan."));
            let l = list();
            let mut accts: Vec<(&i64, &core::AccountBalances)> = s.accounts.iter().filter(|(_, x)| !x.points.is_empty()).collect();
            accts.sort_by_key(|(_, x)| (x.liability, x.name.clone()));
            for (_, x) in accts {
                let (d, v) = *x.points.last().unwrap_or(&(end, Decimal::ZERO));
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(&x.name)).subtitle(format!("Verified {}", d.format("%d %b %Y"))).build();
                r.add_suffix(&gtk::Label::builder().label(fmt_money(v, x.currency)).css_classes(["tnum", if v.is_sign_negative() { "amount-out" } else { "" }]).build());
                l.append(&r);
            }
            content.append(&l);
            content.append(&info_heading(
                "Other Assets & Debts",
                "Things you own or owe that no statement shows. Use Update Value… when the value changes; the chart uses each value from its date onward. Debts show with a minus and count against your net worth.",
            ));
            content.append(&dim("Things no statement shows: a house or car, a pension, an investment account, a loan from family. Update the value now and then; the chart uses each one from its date on."));
            let l = list();
            for i in &s.items {
                let last = i.values.last();
                let sub = match last {
                    Some((d, _)) => format!("{} · valued {}{}", if i.liability { "Debt" } else { "Asset" }, d.format("%d %b %Y"), i.apr.map(|a| format!(" · {a}% a year")).unwrap_or_default()),
                    None => "No value yet".into(),
                };
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(&i.name)).subtitle(sub).build();
                if let Some((_, v)) = last {
                    r.add_suffix(&gtk::Label::builder().label(format!("{}{}", if i.liability { "−" } else { "" }, fmt_money(*v, i.currency))).css_classes(["tnum", if i.liability { "amount-out" } else { "" }]).build());
                }
                let upd = gtk::Button::builder().label("Update Value…").valign(gtk::Align::Center).css_classes(["flat"]).build();
                let edit = gtk::Button::builder().icon_name("document-edit-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Change").build();
                let rm = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Remove").build();
                {
                    let (w, i, r) = (Rc::downgrade(&a), i.clone(), redraw.clone());
                    upd.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            value_dialog(&a, &i, r.clone());
                        }
                    });
                }
                {
                    let (w, i, r) = (Rc::downgrade(&a), i.clone(), redraw.clone());
                    edit.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            item_dialog(&a, Some(i.clone()), r.clone());
                        }
                    });
                }
                {
                    let (w, id, r) = (Rc::downgrade(&a), i.id, redraw.clone());
                    rm.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            let _ = a.db.delete_manual_item(id);
                            r();
                        }
                    });
                }
                for b in [&upd, &edit, &rm] {
                    r.add_suffix(b);
                }
                l.append(&r);
            }
            let add = adw::ButtonRow::builder().title("Add an Asset or Debt…").start_icon_name("list-add-symbolic").build();
            {
                let (w, r) = (Rc::downgrade(&a), redraw.clone());
                add.connect_activated(move |_| {
                    if let Some(a) = w.upgrade() {
                        item_dialog(&a, None, r.clone());
                    }
                });
            }
            l.append(&add);
            content.append(&l);

            // ── goals
            content.append(&info_heading(
                "Goals",
                "The bar shows how far along each goal is, and the label says whether you're on track to reach it by its date. A linked account updates by itself from each statement.",
            ));
            content.append(&dim("Save up for something, or pay a card or loan down. Link an account and FinGraph follows its balance from each statement; otherwise record what you put in during Monthly Close."));
            let l = list();
            for g in a.db.goals(&s.scope).unwrap_or_default() {
                let r = goal_row(&a, &s, &g);
                let edit = gtk::Button::builder().icon_name("document-edit-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Change").build();
                let rm = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Remove").build();
                {
                    let (w, g, r) = (Rc::downgrade(&a), g.clone(), redraw.clone());
                    edit.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            goal_dialog(&a, Some(g.clone()), r.clone());
                        }
                    });
                }
                {
                    let (w, id, r) = (Rc::downgrade(&a), g.id, redraw.clone());
                    rm.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            let _ = a.db.delete_goal(id);
                            r();
                        }
                    });
                }
                r.add_suffix(&edit);
                r.add_suffix(&rm);
                l.append(&r);
            }
            let add = adw::ButtonRow::builder().title("Add a Goal…").start_icon_name("list-add-symbolic").build();
            {
                let (w, r) = (Rc::downgrade(&a), redraw.clone());
                add.connect_activated(move |_| {
                    if let Some(a) = w.upgrade() {
                        goal_dialog(&a, None, r.clone());
                    }
                });
            }
            l.append(&add);
            content.append(&l);

            payoff_section(&a, &s, &content, redraw.clone());
        })
    };
    *me.borrow_mut() = Some(refresh.clone());
    (page.upcast(), refresh)
}

/// Cards, loans and debts added by hand, with a rate each, and the two ways to pay them off.
fn payoff_section(app: &Rc<App>, s: &Snap, into: &gtk::Box, redraw: crate::Refresh) {
    let ccy = s.ccy;
    let money = |d: Decimal| fmt_money(d, ccy);
    // (name, owed in the display currency, rate, typed?, account id or item)
    type Line = (String, Decimal, Option<Decimal>, bool, Result<i64, ManualItem>);
    let mut debts: Vec<Line> = Vec::new();
    for (id, x) in s.accounts.iter().filter(|(_, x)| x.liability) {
        let Some((_, v)) = x.points.last() else { continue };
        let owed = -*v;
        if owed <= Decimal::ZERO {
            continue;
        }
        let (apr, typed) = apr_for(app, s, *id, owed);
        debts.push((x.name.clone(), s.fx.convert(owed, x.currency, ccy), apr, typed, Ok(*id)));
    }
    for i in s.items.iter().filter(|i| i.liability) {
        if let Some((_, v)) = i.values.last().filter(|v| v.1 > Decimal::ZERO) {
            debts.push((i.name.clone(), s.fx.convert(*v, i.currency, ccy), i.apr, true, Err(i.clone())));
        }
    }
    into.append(&info_heading(
        "Paying Off Debt",
        "Compares two ways to clear your debts with the monthly amount you enter. Both pay every minimum, then put the extra on one debt at a time. Highest rate first (avalanche) usually costs the least interest. Smallest balance first (snowball) clears whole debts sooner, which some people find easier to stick with. Each card shows when you'd be debt-free and the total interest.",
    ));
    if debts.is_empty() {
        into.append(&dim("Nothing owed on a card or loan in your statements, and no debts added by hand."));
        return;
    }
    into.append(&dim(&format!(
        "Each debt's rate comes from the interest on its statements (the last three months, against what's owed now) — type one to replace it. The plan pays every minimum ({}% of the balance, at least {}) and puts the rest on one debt at a time.",
        core::MIN_PERCENT,
        fmt_money(core::MIN_FLOOR, Currency::KYD)
    )));
    let l = list();
    for (name, owed, apr, typed, src) in &debts {
        let r = adw::ActionRow::builder()
            .title(glib::markup_escape_text(name))
            .subtitle(format!(
                "{} owed · minimum about {} · {}",
                money(*owed),
                money(core::min_payment(*owed)),
                match (apr, typed) {
                    (Some(_), true) => "rate typed in",
                    (Some(_), false) => "rate from its interest charges",
                    (None, _) => "no interest seen — type its rate",
                }
            ))
            .build();
        let e = gtk::Entry::builder().placeholder_text("% a year").width_chars(7).max_width_chars(7).valign(gtk::Align::Center).text(apr.map(|a| a.normalize().to_string()).unwrap_or_default()).tooltip_text("Interest a year, in percent — Enter to save").build();
        let (w, src, r2) = (Rc::downgrade(app), src.clone(), redraw.clone());
        e.connect_activate(move |e| {
            let Some(a) = w.upgrade() else { return };
            let v = parse_user(e.text().trim_end_matches('%'));
            match &src {
                Ok(id) => {
                    let _ = a.db.set_setting(&format!("apr:{id}"), &v.map(|v| v.to_string()).unwrap_or_default());
                }
                Err(item) => {
                    let mut i = item.clone();
                    i.apr = v;
                    let _ = a.db.save_manual_item(&i);
                }
            }
            r2();
        });
        r.add_suffix(&e);
        r.add_suffix(&gtk::Label::builder().label("%").css_classes(["dim-label"]).build());
        l.append(&r);
    }
    into.append(&l);

    let list: Vec<Debt> = debts.iter().map(|d| Debt { name: d.0.clone(), balance: d.1, apr: d.2.unwrap_or_default() }).collect();
    let mins: Decimal = list.iter().map(|d| core::min_payment(d.balance)).sum();
    let key = format!("payoff_monthly:{}", s.scope);
    let planned = app.db.setting(&key).and_then(|v| parse_user(&v)).or_else(|| app.db.plan(&s.scope).map(|p| p.loan_payments).filter(|v| !v.is_zero())).unwrap_or(mins.round_dp(0));
    let g = adw::PreferencesGroup::new();
    let amount = adw::EntryRow::builder().title(format!("Paying towards debt each month ({})", ccy.symbol())).text(amount_text(planned)).show_apply_button(true).build();
    {
        let (w, r2) = (Rc::downgrade(app), redraw.clone());
        amount.connect_apply(move |e| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.set_setting(&key, &parse_user(&e.text()).map(|v| v.abs().to_string()).unwrap_or_default());
                r2();
            }
        });
    }
    g.add(&amount);
    into.append(&g);
    let cards = gtk::Box::builder().spacing(12).homogeneous(true).build();
    let results: Vec<(Strategy, Option<core::Payoff>)> = [Strategy::Avalanche, Strategy::Snowball].into_iter().map(|st| (st, core::payoff(&list, planned, st))).collect();
    for (st, p) in &results {
        let c = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "kpi"]).build();
        c.append(&gtk::Label::builder().label(match st { Strategy::Avalanche => "Highest Rate First (Avalanche)", Strategy::Snowball => "Smallest Balance First (Snowball)" }).xalign(0.0).css_classes(["dim-label", "caption-heading"]).build());
        match p {
            Some(p) => {
                let free = fingraph_core::insights::add_months(month_start(today()), p.months as i32);
                c.append(&gtk::Label::builder().label(format!("Debt-free {}", free.format("%B %Y"))).xalign(0.0).css_classes(["value", "tnum"]).build());
                c.append(&dim(&format!("{} months · {} interest", p.months, money(p.interest))));
                for (n, mth) in &p.order {
                    c.append(&gtk::Label::builder().label(format!("{n} — paid off {}", fingraph_core::insights::add_months(month_start(today()), *mth as i32).format("%b %Y"))).xalign(0.0).css_classes(["caption"]).build());
                }
            }
            None => {
                c.append(&gtk::Label::builder().label("Not enough").xalign(0.0).css_classes(["value", "amount-out"]).build());
                c.append(&dim(&if planned < mins {
                    format!("The minimums come to about {} a month.", money(mins))
                } else {
                    "The interest grows faster than this pays it off — pay more, or check the rates above.".to_string()
                }));
            }
        }
        cards.append(&c);
    }
    into.append(&cards);
    if let [(_, Some(a)), (_, Some(b))] = &results[..] {
        let saved = b.interest - a.interest;
        into.append(&dim(&if saved > Decimal::ZERO {
            format!("Highest rate first saves {} in interest; smallest first clears its first debt sooner, which some people find keeps them going.", money(saved))
        } else {
            "Both orders cost the same here.".to_string()
        }));
    }
}

fn item_dialog(app: &Rc<App>, item: Option<ManualItem>, done: crate::Refresh) {
    let ccy = app.display();
    let i = item.unwrap_or(ManualItem { id: 0, entity_id: app.entity_ids().first().copied().unwrap_or(1), name: String::new(), liability: false, currency: ccy, apr: None, values: Vec::new() });
    let g = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder().title("Name (e.g. House, Car, Pension)").text(&i.name).build();
    let kind = adw::ComboRow::builder().title("It's").model(&gtk::StringList::new(&["Something I own", "Something I owe"])).selected(i.liability as u32).build();
    let cur = crate::preferences::currency_row("Currency", i.currency);
    let apr = adw::EntryRow::builder().title("Interest a year, % (debts)").text(i.apr.map(|a| a.normalize().to_string()).unwrap_or_default()).build();
    let value = adw::EntryRow::builder().title("Value today").text(i.values.last().map(|v| amount_text(v.1)).unwrap_or_default()).build();
    for r in [name.upcast_ref::<gtk::Widget>(), kind.upcast_ref(), cur.upcast_ref(), apr.upcast_ref(), value.upcast_ref()] {
        g.add(r);
    }
    let dlg = adw::AlertDialog::builder().heading(if i.id == 0 { "Add an Asset or Debt" } else { "Change" }).extra_child(&g).build();
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    dlg.connect_response(Some("save"), move |_, _| {
        let Some(a) = w.upgrade() else { return };
        if name.text().trim().is_empty() {
            return a.toast("Give it a name");
        }
        let mut n = i.clone();
        n.name = name.text().trim().to_string();
        n.liability = kind.selected() == 1;
        n.currency = crate::preferences::currency_at(cur.selected());
        n.apr = parse_user(apr.text().trim_end_matches('%'));
        let Ok(id) = a.db.save_manual_item(&n) else { return };
        if let Some(v) = parse_user(&value.text()) {
            let unchanged = i.values.last().is_some_and(|l| l.1 == v.abs());
            if !unchanged {
                let _ = a.db.set_manual_value(id, today(), v);
            }
        }
        done();
    });
    dlg.present(Some(&app.win));
}

fn value_dialog(app: &Rc<App>, i: &ManualItem, done: crate::Refresh) {
    let g = adw::PreferencesGroup::new();
    let value = adw::EntryRow::builder().title(format!("Value ({})", i.currency.symbol())).text(i.values.last().map(|v| amount_text(v.1)).unwrap_or_default()).build();
    let when = adw::EntryRow::builder().title("As of").text(today().format("%d %b %Y").to_string()).build();
    g.add(&value);
    g.add(&when);
    let history = i.values.iter().rev().take(4).map(|(d, v)| format!("{} {}", d.format("%d %b %Y"), fmt_money(*v, i.currency))).collect::<Vec<_>>().join(" · ");
    let dlg = adw::AlertDialog::builder().heading(format!("Update {}", i.name)).body(if history.is_empty() { String::new() } else { format!("Before: {history}") }).extra_child(&g).build();
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let (w, id) = (Rc::downgrade(app), i.id);
    dlg.connect_response(Some("save"), move |_, _| {
        let Some(a) = w.upgrade() else { return };
        let (Some(v), Some(d)) = (parse_user(&value.text()), parse_date(&when.text())) else { return a.toast("Type the value, and the date like 27 Oct 2026") };
        let _ = a.db.set_manual_value(id, d, v);
        done();
    });
    dlg.present(Some(&app.win));
}

fn goal_dialog(app: &Rc<App>, goal: Option<Goal>, done: crate::Refresh) {
    let s = snap(app);
    let ccy = s.ccy;
    let g0 = goal.unwrap_or(Goal { id: 0, name: String::new(), paydown: false, target: Decimal::ZERO, currency: ccy, target_date: None, monthly: Decimal::ZERO, account_id: None, start_value: Decimal::ZERO, created: month_start(today()) });
    let accts: Vec<(i64, String, bool)> = s.accounts.iter().map(|(id, a)| (*id, a.name.clone(), a.liability)).collect();
    let g = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder().title("Goal (e.g. Emergency fund, Pay off the Visa)").text(&g0.name).build();
    let kind = adw::ComboRow::builder().title("Kind").model(&gtk::StringList::new(&["Save up", "Pay down"])).selected(g0.paydown as u32).build();
    let target = adw::EntryRow::builder().title(format!("Amount ({}) — to save, or to pay off", ccy.symbol())).text(amount_text(g0.target)).build();
    let monthly = adw::EntryRow::builder().title(format!("Each month ({})", ccy.symbol())).text(amount_text(g0.monthly)).build();
    let by = adw::EntryRow::builder().title("By (optional, e.g. Dec 2027)").text(g0.target_date.map(|d| d.format("%b %Y").to_string()).unwrap_or_default()).build();
    let names: Vec<String> = std::iter::once("No account — I'll record what I put in".to_string()).chain(accts.iter().map(|a| a.1.clone())).collect();
    let link = adw::ComboRow::builder().title("Follow an account").model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    link.set_selected(g0.account_id.and_then(|id| accts.iter().position(|a| a.0 == id)).map_or(0, |p| p as u32 + 1));
    let start = adw::EntryRow::builder().title("Already saved (no account)").text(amount_text(if g0.account_id.is_none() && !g0.paydown { g0.start_value } else { Decimal::ZERO })).build();
    for r in [name.upcast_ref::<gtk::Widget>(), kind.upcast_ref(), target.upcast_ref(), monthly.upcast_ref(), by.upcast_ref(), link.upcast_ref(), start.upcast_ref()] {
        g.add(r);
    }
    let dlg = adw::AlertDialog::builder().heading(if g0.id == 0 { "Add a Goal" } else { "Change Goal" }).extra_child(&g).build();
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    dlg.connect_response(Some("save"), move |_, _| {
        let Some(a) = w.upgrade() else { return };
        let s = snap(&a);
        let mut n = g0.clone();
        n.name = name.text().trim().to_string();
        n.paydown = kind.selected() == 1;
        n.account_id = (link.selected() as usize).checked_sub(1).and_then(|i| accts.get(i)).map(|x| x.0);
        n.monthly = parse_user(&monthly.text()).unwrap_or_default().abs();
        let by = by.text();
        n.target_date = if by.trim().is_empty() { None } else { parse_date(&format!("1 {}", by.trim())).or_else(|| parse_date(&by)) };
        // where it starts: the linked account now, else what's already saved
        let bal = n.account_id.and_then(|id| s.accounts.get(&id)).and_then(|x| x.points.last().map(|p| s.fx.convert(p.1, x.currency, ccy)));
        let changed_link = n.account_id != g0.account_id || n.paydown != g0.paydown || g0.id == 0;
        if changed_link {
            n.start_value = match (n.paydown, bal) {
                (true, Some(b)) => (-b).max(Decimal::ZERO),
                (false, Some(b)) => b,
                (_, None) => parse_user(&start.text()).unwrap_or_default().abs(),
            };
            n.created = month_start(today());
        } else if n.account_id.is_none() && !n.paydown {
            n.start_value = parse_user(&start.text()).unwrap_or_default().abs();
        }
        n.target = parse_user(&target.text()).map(|v| v.abs()).filter(|v| !v.is_zero()).unwrap_or(if n.paydown { n.start_value } else { Decimal::ZERO });
        if n.name.is_empty() || n.target.is_zero() {
            return a.toast("Give the goal a name and an amount");
        }
        let _ = a.db.save_goal(&s.scope, &n);
        done();
    });
    dlg.present(Some(&app.win));
}

/// Monthly Close's goals step: progress, and what you put towards each unlinked goal in `m`.
pub fn fill_close_goals(app: &Rc<App>, m: NaiveDate, b: &gtk::Box, redo: crate::Refresh) {
    let s = snap(app);
    let goals = app.db.goals(&s.scope).unwrap_or_default();
    if goals.is_empty() {
        b.append(&dim("No goals yet — add one on the Net Worth page."));
        return;
    }
    b.append(&dim(&format!("Goals that follow an account update from its statements. For the others, type what you put towards them in {} and press Enter.", m.format("%B"))));
    let l = list();
    for g in &goals {
        let r = goal_row(app, &s, g);
        if g.account_id.is_none() {
            let have = app.db.goal_entries(g.id).unwrap_or_default().get(&month_start(m)).copied();
            let e = gtk::Entry::builder().placeholder_text(g.currency.symbol()).width_chars(8).max_width_chars(8).valign(gtk::Align::Center).text(have.map(amount_text).unwrap_or_default()).tooltip_text(format!("Put towards it in {} — Enter to save", m.format("%B"))).build();
            let (w, id, redo) = (Rc::downgrade(app), g.id, redo.clone());
            e.connect_activate(move |e| {
                if let Some(a) = w.upgrade() {
                    let _ = a.db.set_goal_entry(id, m, parse_user(&e.text()).map(|v| v.abs()));
                    redo();
                }
            });
            r.add_suffix(&e);
        }
        l.append(&r);
    }
    b.append(&l);
}
