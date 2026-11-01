//! The built-in business directory and keyword guesses, read-only and searchable, with how often
//! each business turns up in your statements and "Override…" to make your own rule win.

use adw::prelude::*;
use fingraph_core::db::TxFilter;
use fingraph_core::merchants::{self, DIRECTORY, KEYWORDS};
use fingraph_core::rules::{Rule, RuleKind};
use gtk::glib;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{App, dim, heading};

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let search = gtk::SearchEntry::builder().placeholder_text("Search businesses, the words they match, or a category").build();
    body.append(&search);
    body.append(&dim(
        "Built into FinGraph: when a statement line contains a business's words it's filed under that category automatically. \
         Your own rules (Rules & Categories, or answers in To Review) always win — use Override… to change one. \
         The list itself is crates/fingraph-core/data/merchants.tsv in the source.",
    ));

    let biz_head = heading("");
    body.append(&biz_head);
    let only_seen = gtk::CheckButton::builder().label("Only businesses in my statements").build();
    body.append(&only_seen);
    let biz = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    body.append(&biz);

    body.append(&heading("Keyword Guesses"));
    body.append(&dim("When no business matches, these words suggest a category in To Review (or file it, if Preferences → Triage → keyword auto-filing is on). The first matching line wins; a * matches any ending."));
    let kw = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    for (words, cat) in KEYWORDS {
        let r = adw::ActionRow::builder().title(glib::markup_escape_text(&words.replace('|', " · "))).subtitle(glib::markup_escape_text(cat)).build();
        r.set_widget_name(&format!("{words} {cat}").to_lowercase());
        kw.append(&r);
    }
    body.append(&kw);

    // rows built once (the directory never changes while running); counts refreshed
    let mut counts: Vec<gtk::Label> = Vec::new();
    for m in DIRECTORY.iter() {
        let keys = m.keys();
        let r = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&m.name))
            .subtitle(glib::markup_escape_text(&format!("{}{} · matches {}", m.category, if m.fallback { " · payment platform (gives way to the service it billed)" } else { "" }, keys.join(" · "))))
            .subtitle_lines(2)
            .build();
        r.set_widget_name(&format!("{} {} {}", m.name, m.category, keys.join(" ")).to_lowercase());
        let seen = gtk::Label::builder().css_classes(["dim-label", "caption", "tnum"]).build();
        counts.push(seen.clone());
        let over = gtk::Button::builder().label("Override…").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Make a rule of your own for this business").build();
        let (w, name, cat, key) = (Rc::downgrade(app), m.name.clone(), m.category.clone(), keys.first().cloned().unwrap_or_default());
        over.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let category_id = a.categories().iter().find(|c| c.name == cat).map(|c| c.id);
            crate::rules::rule_dialog(&a, Some(Rule { kind: RuleKind::NormalizedTokens, pattern: key.clone(), clean_payee: Some(name.clone()), category_id, ..Default::default() }));
        });
        r.add_suffix(&seen);
        r.add_suffix(&over);
        biz.append(&r);
    }

    let seen_n: Rc<std::cell::RefCell<HashMap<String, usize>>> = Rc::default();
    let apply_filter = {
        let (biz, kw, search, only_seen, seen_n, biz_head) = (biz.clone(), kw.clone(), search.clone(), only_seen.clone(), seen_n.clone(), biz_head.clone());
        Rc::new(move || {
            let q = search.text().to_lowercase();
            let only = only_seen.is_active();
            let seen = seen_n.borrow().clone();
            biz.set_filter_func(move |r| {
                let n = r.widget_name();
                n.contains(q.as_str()) && (!only || r.downcast_ref::<adw::ActionRow>().is_some_and(|a| seen.contains_key(a.title().as_str())))
            });
            let q = search.text().to_lowercase();
            kw.set_filter_func(move |r| r.widget_name().contains(q.as_str()));
            biz_head.set_label(&format!("Businesses · {} built in · {} in your statements", merchants::count(), seen_n.borrow().len()));
        })
    };
    {
        let f = apply_filter.clone();
        search.connect_search_changed(move |_| f());
    }
    {
        let f = apply_filter.clone();
        only_seen.connect_toggled(move |_| f());
    }

    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(900).child(&body).build()).vexpand(true).build();
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            // how often each business appears in the ledger (same text the categoriser reads)
            let mut n: HashMap<String, usize> = HashMap::new();
            for t in a.db.transactions(&TxFilter::default()).unwrap_or_default() {
                if let Some(m) = merchants::lookup(&format!("{} {}", t.raw_description, t.memo)) {
                    *n.entry(m.name.clone()).or_default() += 1;
                }
            }
            for (m, l) in DIRECTORY.iter().zip(&counts) {
                l.set_label(&n.get(&m.name).map(|c| format!("seen {c}×")).unwrap_or_default());
            }
            *seen_n.borrow_mut() = n;
            apply_filter();
        })
    };
    (page.upcast(), refresh)
}
