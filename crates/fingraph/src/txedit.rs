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

    // the same picker as everywhere else: the row shows the category, a click opens it
    let cats = app.categories();
    let label_of = move |id: Option<i64>| id.and_then(|id| cats.iter().find(|c| c.id == id)).map_or("Uncategorized".to_string(), |c| c.name.clone());
    let current = gtk::Label::builder().label(label_of(t.category_id)).css_classes(["dim-label"]).build();
    let category = adw::ActionRow::builder().title("Category").activatable(true).build();
    category.add_suffix(&current);
    category.add_suffix(&gtk::Image::from_icon_name("pan-down-symbolic"));
    if t.splits > 0 {
        category.set_sensitive(false);
        category.set_subtitle("Split into lines — edit them with Split…");
    } else if let Some(why) = fingraph_core::rules::explain(&app.db.rules().unwrap_or_default(), t.account_entity_id, &t.raw_description, &t.memo, t.amount) {
        // what FinGraph would go by: your rule, the directory words, a keyword, or why it left it to you
        category.set_subtitle(&glib::markup_escape_text(&why));
        category.set_subtitle_lines(3);
    }
    {
        let (w, touch, outgoing) = (Rc::downgrade(app), touch.clone(), t.amount.is_sign_negative());
        category.connect_activated(move |row| {
            let Some(a) = w.upgrade() else { return };
            let (w, touch, current, label_of) = (w.clone(), touch.clone(), current.clone(), label_of.clone());
            crate::picker::open(&a, row, crate::picker::Pick { allow_none: true, suggested: Vec::new(), outgoing: Some(outgoing) }, move |id| {
                if let Some(a) = w.upgrade()
                    && a.saved(a.db.set_category(&[tx_id], id))
                {
                    current.set_label(&label_of(id));
                    touch();
                }
            });
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
