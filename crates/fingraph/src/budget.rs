//! Budget: a monthly amount per category, suggested from what you usually spend (the median of the
//! last six months), with this month's spending against it and where the month is heading.

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::budget::{scope, suggest};
use fingraph_core::db::TxFilter;
use fingraph_core::insights::{add_months, by_category, month_start};
use fingraph_core::money::{fmt_money, parse_user};
use gtk::glib;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::{App, dim, heading, today};

const MONTHS: i32 = 6;

thread_local! {
    static MONTH: Cell<Option<NaiveDate>> = const { Cell::new(None) };
}

fn month() -> NaiveDate {
    MONTH.with(Cell::get).unwrap_or_else(|| month_start(today()))
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

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();

    let nav = gtk::Box::builder().spacing(8).build();
    let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous month").build();
    let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next month").build();
    let title = heading("");
    title.add_css_class("title-2");
    title.set_hexpand(true);
    title.set_xalign(0.0);
    let fill = gtk::Button::builder()
        .label("Fill From My Spending")
        .css_classes(["suggested-action"])
        .tooltip_text(format!("Give every category without a budget what it usually costs (median of the last {MONTHS} months)"))
        .build();
    nav.append(&prev);
    nav.append(&next);
    nav.append(&title);
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
    body.append(&dim(&format!(
        "Type a monthly amount for any category and press Enter (empty or 0 removes it). “Usually” is the median of the last {MONTHS} months — a one-off doesn't count. Transfers and categories left out in Preferences aren't spending."
    )));
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    body.append(&list);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1000).child(&body).build()).vexpand(true).build();

    // the page redraws itself after an edit (the rest of the app doesn't depend on budgets)
    let me: Rc<std::cell::RefCell<Option<crate::Refresh>>> = Rc::default();
    let redraw = {
        let me = me.clone();
        move || {
            if let Some(f) = me.borrow().clone() {
                f();
            }
        }
    };
    for (b, step) in [(&prev, -1), (&next, 1)] {
        let r = redraw.clone();
        b.connect_clicked(move |_| {
            MONTH.with(|m| m.set(Some(add_months(month(), step))));
            r();
        });
    }

    // what Fill does for the month on screen, set by each refresh
    let on_fill: Rc<std::cell::RefCell<Option<crate::Refresh>>> = Rc::default();
    {
        let on_fill = on_fill.clone();
        fill.connect_clicked(move |_| {
            if let Some(f) = on_fill.borrow().clone() {
                f();
            }
        });
    }
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let redraw = redraw.clone();
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let m = month();
            let end = add_months(m, 1).pred_opt().unwrap_or(m);
            let ccy = a.display();
            let fx = a.db.fx().unwrap_or_default();
            let parents = a.parents();
            let sc = scope(&a.entity_ids());
            let rows = crate::changes::counted_rows(&a, a.db.transactions(&TxFilter { entity_ids: a.entity_ids(), split_lines: true, ..Default::default() }).unwrap_or_default());
            let cats: Vec<_> = a.categories().into_iter().filter(|c| c.parent_id.is_none() && c.kind == fingraph_core::db::CategoryKind::Expense).collect();
            let budgets: BTreeMap<i64, Decimal> = a.db.budgets(&sc).unwrap_or_default().into_iter().map(|(c, (v, k))| (c, fx.convert(v, k, ccy))).collect();
            let usual = suggest(&rows, m, MONTHS, ccy, &fx, &parents);
            let now: BTreeMap<String, Decimal> = by_category(&rows, m, end, ccy, &fx, &parents).into_iter().collect();
            let last: BTreeMap<String, Decimal> = by_category(&rows, add_months(m, -1), m.pred_opt().unwrap_or(m), ccy, &fx, &parents).into_iter().collect();
            let money = |d: Decimal| fmt_money(d, ccy);

            title.set_label(&m.format("%B %Y").to_string());
            next.set_sensitive(m < month_start(today()));
            {
                // fill: categories without a budget get their usual amount
                let (w, sc, redraw, usual, cats) = (Rc::downgrade(&a), sc.clone(), redraw.clone(), usual.clone(), cats.clone());
                let has: Vec<i64> = budgets.keys().copied().collect();
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
            let total_b: Decimal = budgets.values().copied().sum();
            let total_s: Decimal = now.values().copied().sum();
            let in_b: Decimal = cats.iter().filter(|c| budgets.contains_key(&c.id)).filter_map(|c| now.get(&c.name)).copied().sum();
            let days = Decimal::from(end.day());
            let is_now = m == month_start(today());
            let gone = if is_now { Decimal::from(today().day()) } else { days };
            budgeted.set_label(&money(total_b));
            budgeted_s.set_label(&format!("{} categories", budgets.len()));
            spent.set_label(&money(total_s));
            spent_s.set_label(&format!("{} in budgeted categories · {} outside", money(in_b), money(total_s - in_b)));
            left.set_label(&money(total_b - in_b));
            left_s.set_label(if total_b.is_zero() { "Set a budget below, or fill it from your spending" } else if in_b > total_b { "Over budget" } else { "Left in budgeted categories" });
            left.set_css_classes(&["value", "tnum", if in_b > total_b { "amount-out" } else { "amount-in" }]);
            let proj = if gone.is_zero() { total_s } else { (total_s / gone * days).round_dp(2) };
            pace.set_label(&money(proj));
            pace_s.set_label(&if is_now { format!("At this pace (day {} of {}); a big bill early in the month overstates it", today().day(), end.day()) } else { "The whole month".into() });

            // one row per category that has a budget, spending or a usual amount
            list.remove_all();
            let mut shown: Vec<_> = cats.iter().filter(|c| budgets.contains_key(&c.id) || now.contains_key(&c.name) || usual.contains_key(&c.name)).collect();
            shown.sort_by_key(|c| std::cmp::Reverse(now.get(&c.name).copied().unwrap_or_default().max(budgets.get(&c.id).copied().unwrap_or_default())));
            let use_col = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            for c in shown {
                let s = now.get(&c.name).copied().unwrap_or_default();
                let b = budgets.get(&c.id).copied();
                let u = usual.get(&c.name).copied();
                let mut sub = vec![match u {
                    Some(u) => format!("Usually {}", money(u)),
                    None => "Nothing in the last six months".into(),
                }];
                if let Some(l) = last.get(&c.name) {
                    sub.push(format!("last month {}", money(*l)));
                }
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).subtitle(sub.join(" · ")).build();

                let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).width_request(240).build();
                let status = gtk::Label::builder().xalign(1.0).css_classes(["tnum", "caption"]).build();
                match b {
                    Some(b) if s > b => {
                        status.set_label(&format!("{} of {} · over by {}", money(s), money(b), money(s - b)));
                        status.add_css_class("amount-out");
                    }
                    Some(b) => status.set_label(&format!("{} of {} · {} left", money(s), money(b), money(b - s))),
                    None => {
                        status.set_label(&format!("{} spent · no budget", money(s)));
                        status.add_css_class("dim-label");
                    }
                }
                let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).build();
                bar.remove_offset_value(Some("low"));
                bar.remove_offset_value(Some("high"));
                bar.remove_offset_value(Some("full"));
                bar.add_offset_value("ok", 0.85);
                bar.add_offset_value("warn", 1.0);
                if let Some(b) = b.filter(|b| !b.is_zero()) {
                    let r = rust_decimal::prelude::ToPrimitive::to_f64(&(s / b)).unwrap_or(0.0);
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
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), sc.clone(), c.id, redraw.clone());
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
                    let (w, sc, id, redraw) = (Rc::downgrade(&a), sc.clone(), c.id, redraw.clone());
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
