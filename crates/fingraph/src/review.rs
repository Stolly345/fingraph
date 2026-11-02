//! To Review: every merchant Smart Triage wants to ask about, in a list you can work through in any
//! order. Pick one on the left, answer on the right; the answer is learned and applied to the past.

use adw::prelude::*;
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{TriageGroup, TriageKind};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::{App, clear};

fn kind_text(k: TriageKind) -> &'static str {
    match k {
        TriageKind::Subscription => "Recurring",
        TriageKind::Transfer => "Transfer?",
        TriageKind::Merchant => "Category?",
    }
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
    let side = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).width_request(330).build();
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(18).margin_bottom(24).margin_start(28).margin_end(28).build();
    let detail = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(760).child(&card).build()).hexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let paned = gtk::Paned::builder().start_child(&side).end_child(&detail).shrink_start_child(false).resize_start_child(false).build();
    let done = adw::StatusPage::builder().icon_name("object-select-symbolic").title("All caught up").description("Every merchant has an answer. New statements are categorized by what you've taught FinGraph; anything new shows up here.").build();
    let stack = gtk::Stack::new();
    stack.add_named(&paned, Some("list"));
    stack.add_named(&done, Some("done"));

    let groups: Rc<RefCell<Vec<TriageGroup>>> = Rc::default();
    let answered = Rc::new(Cell::new(0usize));
    // keep your place: after an answer, the next merchant down is selected
    // debug screenshots: FINGRAPH_REVIEW_ROW=n opens on the nth merchant
    let first = if cfg!(debug_assertions) { std::env::var("FINGRAPH_REVIEW_ROW").ok().and_then(|v| v.parse().ok()).unwrap_or(0) } else { 0 };
    let place = Rc::new(Cell::new(first));
    let refresh_slot: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();

    {
        let (w, groups, card, answered, place, slot) = (Rc::downgrade(app), groups.clone(), card.clone(), answered.clone(), place.clone(), refresh_slot.clone());
        list.connect_row_selected(move |_, row| {
            let (Some(a), Some(row)) = (w.upgrade(), row) else { return };
            let i = row.index() as usize;
            let Some(g) = groups.borrow().get(i).cloned() else { return };
            place.set(i);
            clear(&card);
            let slot = slot.clone();
            let next: Rc<dyn Fn()> = Rc::new(move || {
                let f = slot.borrow().clone();
                if let Some(f) = f {
                    f();
                }
            });
            crate::wizard::card(&a, &card, &g, next, answered.clone());
        });
    }

    // the keyboard: 1–6 pick a chip, Enter takes the highlighted answer, J and K move between merchants.
    // Bubble phase, so a field you're typing in (the payee) keeps its own keys.
    {
        let (list, card) = (list.clone(), card.clone());
        let keys = gtk::EventControllerKey::builder().propagation_phase(gtk::PropagationPhase::Bubble).build();
        keys.connect_key_pressed(move |_, key, _, mods| {
            if mods.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            fn buttons(w: &gtk::Widget, class: &str, out: &mut Vec<gtk::Button>) {
                if let Some(b) = w.downcast_ref::<gtk::Button>().filter(|b| b.has_css_class(class) && b.is_mapped() && b.is_sensitive()) {
                    out.push(b.clone());
                }
                let mut c = w.first_child();
                while let Some(ch) = c {
                    buttons(&ch, class, out);
                    c = ch.next_sibling();
                }
            }
            let root: gtk::Widget = card.clone().upcast();
            let step = |d: i32| {
                let i = list.selected_row().map_or(0, |r| r.index()) + d;
                if let Some(r) = list.row_at_index(i.max(0)) {
                    list.select_row(Some(&r));
                }
            };
            match key {
                gtk::gdk::Key::_1 | gtk::gdk::Key::_2 | gtk::gdk::Key::_3 | gtk::gdk::Key::_4 | gtk::gdk::Key::_5 | gtk::gdk::Key::_6 => {
                    let n = key.to_unicode().and_then(|c| c.to_digit(10)).unwrap_or(0) as usize;
                    let mut chips = Vec::new();
                    buttons(&root, "chip", &mut chips);
                    match chips.get(n.saturating_sub(1)) {
                        Some(b) => b.emit_clicked(),
                        None => return glib::Propagation::Proceed,
                    }
                }
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                    let mut lead = Vec::new();
                    buttons(&root, "suggested-action", &mut lead);
                    match lead.first() {
                        Some(b) => b.emit_clicked(),
                        None => return glib::Propagation::Proceed,
                    }
                }
                gtk::gdk::Key::j => step(1),
                gtk::gdk::Key::k => step(-1),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        stack.add_controller(keys);
    }

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (list, groups, stack, place) = (list.clone(), groups.clone(), stack.clone(), place.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let gs = a.db.triage(&a.entity_ids()).unwrap_or_default();
            stack.set_visible_child_name(if gs.is_empty() { "done" } else { "list" });
            list.remove_all();
            for g in &gs {
                let total = g.totals.iter().map(|(c, v)| fmt_money(v.abs(), *c)).collect::<Vec<_>>().join(" + ");
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&g.suggested_payee))
                    .subtitle(glib::markup_escape_text(&format!("{} · {} · {total}", kind_text(g.kind), if g.count == 1 { "1 charge".to_string() } else { format!("{} charges", g.count) })))
                    .build();
                list.append(&r);
            }
            *groups.borrow_mut() = gs;
            let n = groups.borrow().len();
            if n > 0 {
                list.select_row(list.row_at_index(place.get().min(n - 1) as i32).as_ref());
            }
            a.update_sidebar();
        })
    };
    *refresh_slot.borrow_mut() = Some(refresh.clone());
    (stack.upcast(), refresh)
}
