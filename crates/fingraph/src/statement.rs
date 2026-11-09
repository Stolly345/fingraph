//! One statement across all your accounts (or the ones you tick): every transaction in date order,
//! a heading per month with what came in and went out, and the combined balance after each line.
//! Click a line to change it.

use adw::prelude::*;
use chrono::NaiveDate;
use fingraph_core::{Currency, Decimal};
use fingraph_core::db::{TxFilter, TxRow};
use fingraph_core::money::fmt_money;
use gtk::glib;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::{App, clear, dim, heading};

thread_local! {
    /// Accounts left out (empty = all of them).
    static HIDDEN: RefCell<BTreeSet<i64>> = RefCell::default();
}

/// A statement line and the combined balance after it.
type Line = (TxRow, Option<Decimal>);

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let outer = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let bar = gtk::Box::builder().spacing(8).margin_top(8).margin_bottom(8).margin_start(12).margin_end(12).build();
    let acct_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let accts = gtk::MenuButton::builder().label("All accounts").popover(&gtk::Popover::builder().child(&acct_box).build()).build();
    let jump = gtk::DropDown::builder().tooltip_text("Jump to a month").build();
    let summary = dim("");
    summary.set_hexpand(true);
    bar.append(&accts);
    bar.append(&jump);
    bar.append(&summary);
    outer.append(&bar);

    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let scroller = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1100).child(&body).build()).vexpand(true).build();
    outer.append(&scroller);
    let refresh_scroller = scroller.clone();

    // month headings in order, for the jump list
    let months: Rc<RefCell<Vec<gtk::Widget>>> = Rc::default();
    let quiet = Rc::new(std::cell::Cell::new(false));
    // a jump waiting for layout; a refresh meanwhile repeats it rather than restoring the old position
    let pending: Rc<std::cell::Cell<Option<usize>>> = Rc::default();
    let scroll_to = {
        let (months, scroller, body, pending) = (months.clone(), scroller.clone(), body.clone(), pending.clone());
        Rc::new(move |i: usize| {
            pending.set(Some(i));
            let (months, scroller, body, pending) = (months.clone(), scroller.clone(), body.clone(), pending.clone());
            // once laid out, so the heading has a position (retry while the page is still sizing)
            let tries = std::cell::Cell::new(0);
            glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
                tries.set(tries.get() + 1);
                let adj = scroller.vadjustment();
                match months.borrow().get(i).and_then(|m| m.compute_point(&body, &gtk::graphene::Point::new(0.0, 0.0))) {
                    Some(p) if adj.upper() > adj.page_size() || tries.get() > 20 => {
                        adj.set_value(f64::from(p.y()));
                        pending.set(None);
                        glib::ControlFlow::Break
                    }
                    _ if tries.get() > 20 => glib::ControlFlow::Break,
                    _ => glib::ControlFlow::Continue,
                }
            });
        })
    };
    {
        let (quiet, scroll_to) = (quiet.clone(), scroll_to.clone());
        jump.connect_selected_notify(move |d| {
            if !quiet.get() {
                scroll_to(d.selected() as usize);
            }
        });
    }

    let refresh: crate::Refresh = {
        let (w, scroller) = (Rc::downgrade(app), refresh_scroller);
        // what the page shows now: rebuilding ~1000 rows costs ~300 ms, so skip it when nothing changed
        let shown: RefCell<Option<(Currency, Vec<Line>)>> = RefCell::default();
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let ids = a.entity_ids();
            let all: Vec<_> = a.db.accounts().unwrap_or_default().into_iter().filter(|x| !x.hidden && (ids.is_empty() || ids.contains(&x.entity_id))).collect();

            // the account picker
            clear(&acct_box);
            for acct in &all {
                let c = gtk::CheckButton::builder().label(format!("{} · {}", acct.name, acct.currency)).active(!HIDDEN.with(|h| h.borrow().contains(&acct.id))).build();
                let (w, id) = (Rc::downgrade(&a), acct.id);
                c.connect_toggled(move |c| {
                    HIDDEN.with(|h| {
                        let mut h = h.borrow_mut();
                        if c.is_active() { h.remove(&id) } else { h.insert(id) };
                    });
                    if let Some(a) = w.upgrade() {
                        a.refresh();
                    }
                });
                acct_box.append(&c);
            }
            let chosen: Vec<_> = all.iter().filter(|x| !HIDDEN.with(|h| h.borrow().contains(&x.id))).collect();
            accts.set_label(&if chosen.len() == all.len() { "All accounts".into() } else { format!("{} of {} accounts", chosen.len(), all.len()) });

            let ccy = a.display();
            let fx = a.db.fx().unwrap_or_default();
            let (from, to) = a.range();
            let picked: BTreeSet<i64> = chosen.iter().map(|x| x.id).collect();
            let liab: BTreeMap<i64, bool> = chosen.iter().map(|x| (x.id, x.account_type.is_liability())).collect();
            let mut rows: Vec<TxRow> = a.db.transactions(&TxFilter::default()).unwrap_or_default().into_iter().filter(|t| picked.contains(&t.account_id)).collect();
            rows.reverse(); // oldest first, statement order within a day
            let openings: BTreeMap<i64, Decimal> = a.db.documents(&[]).unwrap_or_default().into_iter().filter_map(|d| Some((d.id, d.opening?))).collect();

            // each account's balance after every line (printed when the bank printed one; otherwise
            // carried from the statement's opening balance), owed amounts negative
            let mut bal: BTreeMap<i64, Decimal> = BTreeMap::new();
            let mut lines: Vec<(TxRow, Option<Decimal>)> = Vec::new();
            for t in rows {
                let sign = if liab.get(&t.account_id).copied().unwrap_or(false) { -Decimal::ONE } else { Decimal::ONE };
                match t.balance {
                    Some(b) => {
                        bal.insert(t.account_id, b * sign);
                    }
                    None => {
                        if let Some(b) = bal.get_mut(&t.account_id) {
                            *b += t.amount;
                        } else if let Some(o) = t.document_id.and_then(|d| openings.get(&d)) {
                            bal.insert(t.account_id, *o * sign + t.amount);
                        }
                    }
                }
                if t.posted >= from && t.posted <= to {
                    // the combined balance, once every chosen account has one
                    let total = (bal.len() == picked.len()).then(|| bal.iter().map(|(id, v)| fx.convert(*v, chosen.iter().find(|x| x.id == *id).map_or(ccy, |x| x.currency), ccy).unwrap_or_default()).sum());
                    lines.push((t, total));
                }
            }

            let keep = scroller.vadjustment().value();
            if shown.borrow().as_ref().is_some_and(|(c, l)| *c == ccy && *l == lines) {
                if let Some(i) = pending.get() {
                    scroll_to(i);
                }
                return;
            }
            *shown.borrow_mut() = Some((ccy, lines.clone()));
            clear(&body);
            months.borrow_mut().clear();
            let mut labels: Vec<String> = Vec::new();
            let (mut tin, mut tout) = (Decimal::ZERO, Decimal::ZERO);
            let mut by_month: BTreeMap<NaiveDate, Vec<&(TxRow, Option<Decimal>)>> = BTreeMap::new();
            for l in &lines {
                by_month.entry(fingraph_core::insights::month_start(l.0.posted)).or_default().push(l);
            }
            for (m, ls) in &by_month {
                let (mut i, mut o) = (Decimal::ZERO, Decimal::ZERO);
                for (t, _) in ls.iter().map(|x| (&x.0, x.1)).filter(|(t, _)| !t.is_transfer()) {
                    let v = fx.convert(t.amount, t.currency, ccy).unwrap_or_default();
                    if v.is_sign_negative() { o -= v } else { i += v }
                }
                tin += i;
                tout += o;
                let head = gtk::Box::builder().spacing(12).margin_top(12).build();
                let title = heading(&m.format("%B %Y").to_string());
                title.add_css_class("title-3");
                title.set_hexpand(true);
                head.append(&title);
                let end = ls.last().and_then(|x| x.1).map(|b| format!(" · balance {}", fmt_money(b, ccy))).unwrap_or_default();
                head.append(&dim(&format!("in {} · out {} · net {}{end}", fmt_money(i, ccy), fmt_money(o, ccy), fmt_money(i - o, ccy))));
                body.append(&head);
                months.borrow_mut().push(head.clone().upcast());
                labels.push(m.format("%B %Y").to_string());

                let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                for (t, total) in ls.iter().map(|x| (&x.0, x.1)) {
                    let cat = if t.splits > 0 { format!("split in {}", t.splits) } else { t.category_name.clone().unwrap_or_else(|| "Uncategorized".into()) };
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&format!("{} · {}", t.posted.format("%d %b"), t.clean_payee)))
                        .subtitle(glib::markup_escape_text(&format!("{} · {} · {}", t.account_name, cat, t.raw_description)))
                        .subtitle_lines(1)
                        .activatable(true)
                        .build();
                    if t.attachments > 0 {
                        row.add_prefix(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
                    }
                    let amt = gtk::Label::builder().label(fmt_money(t.amount, t.currency)).css_classes(["tnum", if t.is_transfer() { "dim-label" } else if t.amount.is_sign_negative() { "amount-out" } else { "amount-in" }]).build();
                    if t.is_transfer() {
                        amt.set_tooltip_text(Some("Between your own accounts — not counted as in or out"));
                    }
                    let b = gtk::Label::builder().label(total.map(|b| fmt_money(b, ccy)).unwrap_or_default()).width_chars(14).xalign(1.0).css_classes(["tnum", "dim-label"]).tooltip_text("Combined balance of the chosen accounts after this line").build();
                    row.add_suffix(&amt);
                    row.add_suffix(&b);
                    let (w, id) = (Rc::downgrade(&a), t.id);
                    row.connect_activated(move |_| {
                        if let Some(a) = w.upgrade() {
                            crate::txedit::dialog(&a, id, None);
                        }
                    });
                    list.append(&row);
                }
                body.append(&list);
            }
            if lines.is_empty() {
                body.append(&adw::StatusPage::builder().icon_name("x-office-document-symbolic").title("Nothing in this range").description("Pick other accounts or a wider date range").build());
            }
            summary.set_label(&format!("{} lines · in {} · out {} · transfers between your accounts not counted", lines.len(), fmt_money(tin, ccy), fmt_money(tout, ccy)));

            // keep the jump list and scroll position when nothing structural changed
            let same = jump.model().and_downcast::<gtk::StringList>().is_some_and(|m| m.n_items() as usize == labels.len() && (0..m.n_items()).all(|i| m.string(i).as_deref() == labels.get(i as usize).map(String::as_str)));
            if !same {
                quiet.set(true);
                jump.set_model(Some(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
                jump.set_selected(labels.len().saturating_sub(1) as u32);
                quiet.set(false);
                scroll_to(labels.len().saturating_sub(1));
            } else if let Some(i) = pending.get() {
                scroll_to(i);
            } else {
                let sc = scroller.clone();
                glib::idle_add_local_once(move || sc.vadjustment().set_value(keep));
            }
        })
    };
    (outer.upcast(), refresh)
}
