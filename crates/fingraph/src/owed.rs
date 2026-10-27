//! "The business owes you": business spending paid from a personal account (whole charges or split
//! lines), what's been paid back, and recording the repayment when the business transfers it.

use adw::prelude::*;
use fingraph_core::Decimal;
use fingraph_core::money::fmt_money;
use fingraph_core::workflows::OwedItem;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::{App, clear};

/// One card per business with money outstanding, on the Overview.
pub fn fill(app: &Rc<App>, into: &gtk::Box) {
    clear(into);
    let ids = app.entity_ids();
    let ents = app.entities.borrow().clone();
    let any_personal = ents.iter().any(|e| ids.contains(&e.id) && e.kind == fingraph_core::EntityKind::Personal);
    for biz in ents.iter().filter(|e| e.kind != fingraph_core::EntityKind::Personal && (ids.contains(&e.id) || any_personal)) {
        let open: Vec<OwedItem> = app.db.owed_items(biz.id).unwrap_or_default().into_iter().filter(|i| !i.reimbursed).collect();
        if open.is_empty() {
            continue;
        }
        let total: Decimal = open.iter().map(|i| i.owed).sum();
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&format!("{} owes you {}", biz.name, fmt_money(total, biz.base_currency))))
            .subtitle(format!(
                "{} business charge{} paid from your personal accounts, not yet paid back · oldest {}",
                open.len(),
                if open.len() == 1 { "" } else { "s" },
                open.iter().map(|i| i.posted).min().map(|d| d.format("%d %b %Y").to_string()).unwrap_or_default()
            ))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("folder-documents-symbolic"));
        let show = gtk::Button::builder().label("Show").valign(gtk::Align::Center).css_classes(["flat"]).build();
        let settle = gtk::Button::builder().label("Record Repayment…").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        {
            let (w, ids) = (Rc::downgrade(app), open.iter().map(|i| i.tx_id).collect::<Vec<_>>());
            show.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    a.show_transactions(ids.clone());
                }
            });
        }
        {
            let (w, id) = (Rc::downgrade(app), biz.id);
            settle.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    dialog(&a, id);
                }
            });
        }
        row.add_suffix(&show);
        row.add_suffix(&settle);
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        list.append(&row);
        into.append(&list);
    }
}

/// Tick the charges being paid back, pick the transfer that paid them, record it.
fn dialog(app: &Rc<App>, business: i64) {
    let Some(biz) = app.entities.borrow().iter().find(|e| e.id == business).cloned() else { return };
    let items: Vec<OwedItem> = app.db.owed_items(business).unwrap_or_default().into_iter().filter(|i| !i.reimbursed).collect();
    if items.is_empty() {
        return app.toast(&format!("{} doesn't owe you anything now", biz.name));
    }
    let page = adw::PreferencesPage::new();
    let g = adw::PreferencesGroup::builder().title("Charges Being Paid Back").description("Untick any the business isn't repaying yet. ✎ changes a charge's category, workspace or split.").build();
    // set once the dialog exists: editing a charge reopens this dialog with fresh totals
    let this: Rc<RefCell<Option<adw::Dialog>>> = Rc::default();
    let checks: Rc<RefCell<Vec<(gtk::CheckButton, OwedItem)>>> = Rc::default();
    let total = gtk::Label::builder().css_classes(["heading", "tnum"]).build();
    let pay = adw::ComboRow::builder().title("Paid back by").subtitle("Money that came into your accounts; nearest to the total first").build();
    let candidates: Rc<RefCell<Vec<fingraph_core::db::TxRow>>> = Rc::default();
    let recompute = {
        let (checks, total, pay, candidates, w) = (checks.clone(), total.clone(), pay.clone(), candidates.clone(), Rc::downgrade(app));
        let ccy = biz.base_currency;
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let sum: Decimal = checks.borrow().iter().filter(|(c, _)| c.is_active()).map(|(_, i)| i.owed).sum();
            total.set_label(&format!("Total {}", fmt_money(sum, ccy)));
            let c = a.db.reimbursement_candidates(business, sum).unwrap_or_default();
            let names: Vec<String> = c.iter().map(|t| format!("{} · {} · {} · {}", t.posted.format("%d %b %Y"), t.account_name, t.clean_payee, fmt_money(t.amount, t.currency))).collect();
            pay.set_model(Some(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())));
            *candidates.borrow_mut() = c;
        })
    };
    for i in &items {
        let c = gtk::CheckButton::builder().active(true).valign(gtk::Align::Center).build();
        let r = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&format!("{} · {}", i.posted.format("%d %b %Y"), i.payee)))
            .subtitle(glib::markup_escape_text(&format!("{}{}", i.account, if i.split_seq >= 0 { " · part of a split charge" } else { "" })))
            .activatable_widget(&c)
            .build();
        r.add_prefix(&c);
        r.add_suffix(&gtk::Label::builder().label(fmt_money(i.owed, i.currency)).css_classes(["tnum"]).build());
        let edit = gtk::Button::builder().icon_name("document-edit-symbolic").tooltip_text("Change this charge: category, workspace, split, receipt").valign(gtk::Align::Center).css_classes(["flat"]).build();
        {
            let (w, this, tx) = (Rc::downgrade(app), this.clone(), i.tx_id);
            edit.connect_clicked(move |_| {
                let Some(a) = w.upgrade() else { return };
                let (w2, this) = (Rc::downgrade(&a), this.clone());
                crate::txedit::dialog(
                    &a,
                    tx,
                    Some(Rc::new(move || {
                        if let Some(d) = this.borrow_mut().take() {
                            d.close();
                        }
                        if let Some(a) = w2.upgrade() {
                            dialog(&a, business);
                        }
                    })),
                );
            });
        }
        r.add_suffix(&edit);
        let rc = recompute.clone();
        c.connect_toggled(move |_| rc());
        g.add(&r);
        checks.borrow_mut().push((c, i.clone()));
    }
    page.add(&g);
    let pg = adw::PreferencesGroup::new();
    pg.add(&pay);
    page.add(&pg);
    recompute();
    let record = gtk::Button::builder().label("Record Repayment").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::new();
    header.pack_start(&total);
    header.pack_end(&record);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().title(format!("{} Owes You", biz.name)).content_width(720).content_height(640).child(&tv).build();
    *this.borrow_mut() = Some(dlg.clone());
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        record.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let chosen: Vec<OwedItem> = checks.borrow().iter().filter(|(c, _)| c.is_active()).map(|(_, i)| i.clone()).collect();
            let Some(paid) = candidates.borrow().get(pay.selected() as usize).cloned() else {
                return a.toast("Pick the transfer that paid you back (import its statement first)");
            };
            if chosen.is_empty() {
                return a.toast("Tick at least one charge");
            }
            match a.db.record_reimbursement(&chosen, paid.id) {
                Ok(()) => {
                    a.toast(&format!("{} charges marked as paid back by {}", chosen.len(), fmt_money(paid.amount, paid.currency)));
                    dlg.close();
                    a.refresh();
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    dlg.present(Some(&app.win));
}

#[cfg(debug_assertions)]
pub fn debug_open(app: &Rc<App>, business: i64) {
    dialog(app, business);
}
