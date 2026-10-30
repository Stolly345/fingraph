//! One transaction, every editable thing about it: payee, category, which workspace it counts in,
//! tag, memo, subscription — plus split, receipts and "show in Transactions".

use adw::prelude::*;
use fingraph_core::money::fmt_money;
use gtk::glib;
use std::cell::Cell;
use std::rc::Rc;

use crate::App;

/// `on_closed` runs when the dialog closes, if anything was changed.
pub fn dialog(app: &Rc<App>, tx_id: i64, on_closed: Option<Rc<dyn Fn()>>) {
    let Ok(t) = app.db.transaction(tx_id) else { return };
    let changed = Rc::new(Cell::new(false));
    let touch = {
        let (w, changed) = (Rc::downgrade(app), changed.clone());
        Rc::new(move || {
            changed.set(true);
            if let Some(a) = w.upgrade() {
                a.refresh();
            }
        })
    };

    let page = adw::PreferencesPage::new();
    let what = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(&format!("{} · {}", t.posted.format("%d %b %Y"), fmt_money(t.amount, t.currency))))
        .description(glib::markup_escape_text(&format!("{} · as printed: {}", t.account_name, t.raw_description)))
        .build();

    let payee = adw::EntryRow::builder().title("Payee").text(&t.clean_payee).show_apply_button(true).build();
    {
        let (w, touch) = (Rc::downgrade(app), touch.clone());
        payee.connect_apply(move |e| {
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_payee(tx_id, e.text().trim()));
                touch();
            }
        });
    }
    what.add(&payee);

    let cats = app.categories();
    let mut names = vec!["Uncategorized".to_string()];
    names.extend(cats.iter().map(|c| if c.parent_id.is_some() { format!("   {}", c.name) } else { c.name.clone() }));
    let category = adw::ComboRow::builder().title("Category").model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())).enable_search(true).build();
    category.set_selected(t.category_id.and_then(|id| cats.iter().position(|c| c.id == id)).map_or(0, |i| i + 1) as u32);
    if t.splits > 0 {
        category.set_sensitive(false);
        category.set_subtitle("Split into lines — edit them with Split…");
    }
    {
        let (w, touch) = (Rc::downgrade(app), touch.clone());
        category.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                let id = (c.selected() as usize).checked_sub(1).and_then(|i| cats.get(i)).map(|c| c.id);
                a.saved(a.db.set_category(&[tx_id], id));
                touch();
            }
        });
    }
    what.add(&category);

    // the workspace it counts in: its account's own, or a business it was paid for
    let ents = app.entities.borrow().clone();
    let ws_names: Vec<String> = ents.iter().map(|e| if e.id == t.account_entity_id { format!("{} (paid from)", e.name) } else { e.name.clone() }).collect();
    let workspace = adw::ComboRow::builder()
        .title("Counts in")
        .subtitle("Pick a business to mark it a business expense paid from this account")
        .model(&gtk::StringList::new(&ws_names.iter().map(String::as_str).collect::<Vec<_>>()))
        .build();
    let current = t.business_entity_id.unwrap_or(t.account_entity_id);
    workspace.set_selected(ents.iter().position(|e| e.id == current).unwrap_or(0) as u32);
    workspace.set_sensitive(t.transfer_peer.is_none() && t.splits == 0);
    {
        let (w, touch, home) = (Rc::downgrade(app), touch.clone(), t.account_entity_id);
        workspace.connect_selected_notify(move |c| {
            if let (Some(a), Some(e)) = (w.upgrade(), ents.get(c.selected() as usize)) {
                a.saved(a.db.set_business(&[tx_id], (e.id != home).then_some(e.id)));
                touch();
            }
        });
    }
    what.add(&workspace);

    let tag = adw::EntryRow::builder().title("Project / tag").text(&t.tag).show_apply_button(true).build();
    let memo = adw::EntryRow::builder().title("Memo").text(&t.memo).show_apply_button(true).build();
    {
        let (w, touch) = (Rc::downgrade(app), touch.clone());
        tag.connect_apply(move |e| {
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_tag(&[tx_id], &e.text()));
                touch();
            }
        });
    }
    {
        let (w, touch) = (Rc::downgrade(app), touch.clone());
        memo.connect_apply(move |e| {
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_memo(tx_id, &e.text()));
                touch();
            }
        });
    }
    what.add(&tag);
    what.add(&memo);
    let sub = adw::SwitchRow::builder().title("Subscription").active(t.is_subscription).build();
    {
        let (w, touch) = (Rc::downgrade(app), touch.clone());
        sub.connect_active_notify(move |s| {
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_subscription(&[tx_id], s.is_active()));
                touch();
            }
        });
    }
    what.add(&sub);
    page.add(&what);

    let more = adw::PreferencesGroup::new();
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
    let split = gtk::Button::builder().label(if t.splits > 0 { "Edit Split…" } else { "Split…" }).css_classes(["pill"]).sensitive(t.transfer_peer.is_none()).build();
    let attach = gtk::Button::builder().label(if t.attachments > 0 { format!("Receipts ({})…", t.attachments) } else { "Attach Receipt…".into() }).css_classes(["pill"]).build();
    let show = gtk::Button::builder().label("Show in Transactions").css_classes(["pill"]).build();
    for b in [&split, &attach, &show] {
        buttons.append(b);
    }
    more.add(&buttons);
    page.add(&more);

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().title(&t.clean_payee).content_width(620).content_height(620).child(&tv).build();
    {
        let (w, changed) = (Rc::downgrade(app), changed.clone());
        split.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                changed.set(true);
                crate::split::dialog(&a, tx_id);
            }
        });
    }
    {
        let (w, changed, has) = (Rc::downgrade(app), changed.clone(), t.attachments > 0);
        attach.connect_clicked(move |b| {
            if let Some(a) = w.upgrade() {
                changed.set(true);
                if has {
                    crate::split::open_attachments(&a, tx_id, b.upcast_ref());
                } else {
                    crate::split::choose_attachment(&a, tx_id);
                }
            }
        });
    }
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        show.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                dlg.close();
                a.show_transactions(vec![tx_id]);
            }
        });
    }
    // typed but not applied (no Enter or ✓) still counts: changes apply as made
    let typed = [(payee.clone(), t.clean_payee.clone()), (tag.clone(), t.tag.clone()), (memo.clone(), t.memo.clone())];
    dlg.connect_closed(move |_| {
        for (e, was) in &typed {
            if e.text() != was.as_str() {
                e.emit_by_name::<()>("apply", &[]);
            }
        }
        if changed.get()
            && let Some(f) = &on_closed
        {
            f();
        }
    });
    dlg.present(Some(&app.win));
}
