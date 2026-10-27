//! The first-run walkthrough (workspaces → banks & statements → triage) and Smart Triage: one
//! conversational card per unfamiliar merchant; every answer is learned and applied retroactively.

use adw::prelude::*;
use fingraph_core::db::Category;
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind, TriageGroup, TriageKind};
use fingraph_core::{Currency, EntityKind, Reconciliation};
use gtk::{gdk, glib};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{App, clear, dim, pill};

const BANKS: [(&str, &str, &str); 5] = [
    ("butterfield_ky", "Butterfield", "Bank of Butterfield (Cayman)"),
    ("scotiabank_ky", "Scotiabank", "Scotiabank & Trust (Cayman)"),
    ("cnb_ky", "Cayman National", "Cayman National Bank"),
    ("rbc_ky", "RBC", "RBC Royal Bank (Cayman)"),
    ("cicsa_cu_ky", "Credit Union", "CICSA Co-operative Credit Union"),
];

fn page_box() -> gtk::Box {
    gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(24).margin_bottom(24).margin_start(32).margin_end(32).hexpand(true).build()
}

fn title(t: &str, sub: &str) -> gtk::Box {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    b.append(&gtk::Label::builder().label(t).xalign(0.0).css_classes(["title-1"]).wrap(true).build());
    b.append(&dim(sub));
    b
}

pub fn onboarding(app: &Rc<App>) {
    let carousel = adw::Carousel::builder().interactive(false).vexpand(true).build();
    let dots = adw::CarouselIndicatorDots::builder().carousel(&carousel).build();

    // ── 1. workspaces
    let p1 = page_box();
    p1.append(&title("Welcome to FinGraph", "Your statements stay on this computer. First, how should your money be split? Each workspace is kept apart, so a business P&L never mixes with household spending — and a Consolidated Overview puts them together when you want."));
    let g = adw::PreferencesGroup::new();
    let personal = adw::EntryRow::builder().title("Personal workspace").text("Personal").build();
    let biz_on = adw::SwitchRow::builder().title("Add a business or property workspace").active(true).build();
    let biz = adw::EntryRow::builder().title("Business workspace name").text("Business — Property").build();
    let ccy = adw::ComboRow::builder().title("Default currency").subtitle("Statements in the other currency convert at 1 KYD = 1.20 USD unless the statement prints its own rate").model(&gtk::StringList::new(&["KYD — Cayman Islands dollar", "USD — US dollar"])).build();
    biz_on.bind_property("active", &biz, "sensitive").sync_create().build();
    for r in [personal.upcast_ref::<gtk::Widget>(), biz_on.upcast_ref(), biz.upcast_ref(), ccy.upcast_ref()] {
        g.add(r);
    }
    p1.append(&g);
    let next1 = gtk::Button::builder().label("Create Workspaces").halign(gtk::Align::End).css_classes(["suggested-action", "pill"]).build();
    p1.append(&next1);
    carousel.append(&p1);

    // ── 2. banks & statements
    let p2 = page_box();
    p2.append(&title("Your Banks & Statements", "FinGraph knows these five Cayman institutions out of the box. Drop a batch of PDF statements anywhere on this page: each one's bank, account and currency are detected, then the statement is proven against its own balances."));
    let cards = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(5).min_children_per_line(2).column_spacing(10).row_spacing(10).homogeneous(true).build();
    let mut toggles = Vec::new();
    for (slug, short, long) in BANKS {
        let t = gtk::ToggleButton::builder().css_classes(["card", "bank-card"]).build();
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        b.append(&gtk::Label::builder().label(short).css_classes(["heading"]).build());
        b.append(&gtk::Label::builder().label(long).css_classes(["caption", "dim-label"]).wrap(true).justify(gtk::Justification::Center).build());
        t.set_child(Some(&b));
        t.set_tooltip_text(Some("Lights up when one of its statements is detected"));
        cards.insert(&t, -1);
        toggles.push((slug, t));
    }
    p2.append(&cards);
    let actions = gtk::Box::builder().spacing(10).halign(gtk::Align::Center).build();
    let choose = gtk::Button::builder().label("Choose PDFs…").css_classes(["suggested-action", "pill"]).build();
    let demo = gtk::Button::builder().label("Load Demo Data Instead").css_classes(["pill"]).build();
    actions.append(&choose);
    actions.append(&demo);
    p2.append(&actions);
    let results = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).visible(false).build();
    p2.append(&gtk::ScrolledWindow::builder().child(&results).vexpand(true).min_content_height(160).build());
    let next2 = gtk::Button::builder().label("Continue").halign(gtk::Align::End).css_classes(["pill"]).build();
    p2.append(&next2);
    carousel.append(&p2);

    // ── 3. triage
    let p3 = page_box();
    p3.append(&title("Smart Triage", "FinGraph groups unfamiliar merchants and asks about each one once — is HBO a subscription? Is that a credit-card payment between your own accounts? Every answer becomes a rule that applies to the past and to every future statement."));
    let count = gtk::Label::builder().css_classes(["title-3"]).build();
    p3.append(&count);
    let fin = gtk::Box::builder().spacing(10).halign(gtk::Align::Center).build();
    let start = gtk::Button::builder().label("Start Smart Triage").css_classes(["suggested-action", "pill"]).build();
    let done = gtk::Button::builder().label("Finish").css_classes(["pill"]).build();
    fin.append(&start);
    fin.append(&done);
    p3.append(&fin);
    carousel.append(&p3);

    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    body.append(&carousel);
    body.append(&dots);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
    tv.set_content(Some(&body));
    let dlg = adw::Dialog::builder().content_width(820).content_height(680).child(&tv).build();

    let go = {
        let carousel = carousel.clone();
        move |i: u32| carousel.scroll_to(&carousel.nth_page(i), true)
    };
    let update_count = {
        let (w, count, start) = (Rc::downgrade(app), count.clone(), start.clone());
        move || {
            let Some(a) = w.upgrade() else { return };
            let n = a.db.triage(&[]).map(|g| g.len()).unwrap_or(0);
            count.set_label(&if n == 0 { "Nothing to review yet".to_string() } else { format!("{n} merchants to review") });
            start.set_sensitive(n > 0);
        }
    };
    let update_count = Rc::new(update_count);
    {
        let (w, go) = (Rc::downgrade(app), go.clone());
        next1.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let c = if ccy.selected() == 1 { Currency::USD } else { Currency::KYD };
            let mut want = vec![(personal.text().trim().to_string(), EntityKind::Personal)];
            if biz_on.is_active() {
                want.push((biz.text().trim().to_string(), EntityKind::Business));
            }
            for (name, kind) in want.into_iter().filter(|(n, _)| !n.is_empty()) {
                match a.db.entity_by_name(&name) {
                    Ok(Some(id)) => {
                        let _ = a.db.update_entity(id, &name, c);
                    }
                    _ => {
                        let _ = a.db.create_entity(&name, kind, c);
                    }
                }
            }
            a.reload_entities();
            go(1);
        });
    }
    let toggles = Rc::new(toggles);
    let on_each: crate::statements::OnEach = {
        let (results, toggles) = (results.clone(), toggles.clone());
        Rc::new(move |name: &str, r| {
            results.set_visible(true);
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(name)).build();
            match r {
                Ok((scan, rep)) => {
                    if let Some((_, t)) = toggles.iter().find(|(s, _)| Some(*s) == scan.profile_slug.as_deref()) {
                        t.set_active(true);
                    }
                    let what: Vec<String> = scan.sections.iter().map(|s| format!("{} {} ••{}", s.currency, s.account_type.label(), s.account_last4.as_deref().unwrap_or("?"))).collect();
                    row.set_subtitle(&glib::markup_escape_text(&format!("{} · {} new transactions", what.join(" + "), rep.inserted)));
                    row.add_prefix(&pill(BANKS.iter().find(|b| Some(b.0) == scan.profile_slug.as_deref()).map_or("Unknown bank", |b| b.1), "bank"));
                    let ok = scan.is_reconciled();
                    let ccy = scan.sections.first().map_or(Currency::KYD, |s| s.currency);
                    let d = scan.sections.iter().find_map(|s| if let Reconciliation::Discrepancy(d) = s.reconciliation { Some(d) } else { None });
                    row.add_suffix(&if ok {
                        pill("Reconciled", "ok")
                    } else if let Some(d) = d {
                        pill(&format!("Discrepancy {}", fmt_money(d, ccy)), "bad")
                    } else {
                        pill("Unverified", "meh")
                    });
                }
                Err(e) => {
                    row.set_subtitle(&glib::markup_escape_text(&e));
                    row.add_suffix(&pill("Failed", "bad"));
                }
            }
            results.append(&row);
        })
    };
    {
        let (w, on_each) = (Rc::downgrade(app), on_each.clone());
        choose.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let filter = gtk::FileFilter::new();
            filter.add_suffix("pdf");
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let fd = gtk::FileDialog::builder().title("Choose Statements").filters(&filters).modal(true).build();
            let (w, on_each) = (Rc::downgrade(&a), on_each.clone());
            fd.open_multiple(Some(&a.win), gtk::gio::Cancellable::NONE, move |res| {
                let (Some(a), Ok(files)) = (w.upgrade(), res) else { return };
                let paths = (0..files.n_items()).filter_map(|i| files.item(i).and_downcast::<gtk::gio::File>().and_then(|f| f.path())).collect();
                crate::statements::import_paths_with(&a, paths, Some(on_each.clone()));
            });
        });
    }
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    {
        let (w, on_each) = (Rc::downgrade(app), on_each.clone());
        drop.connect_drop(move |_, v, _, _| {
            let (Some(a), Ok(files)) = (w.upgrade(), v.get::<gdk::FileList>()) else { return false };
            crate::statements::import_paths_with(&a, files.files().iter().filter_map(|f| f.path()).collect(), Some(on_each.clone()));
            true
        });
    }
    p2.add_controller(drop);
    {
        let (w, go, uc, toggles) = (Rc::downgrade(app), go.clone(), update_count.clone(), toggles.clone());
        demo.connect_clicked(move |b| {
            let Some(a) = w.upgrade() else { return };
            b.set_sensitive(false);
            match a.db.load_demo() {
                Ok(n) => {
                    toggles.iter().for_each(|(_, t)| t.set_active(true));
                    a.reload_entities();
                    a.toast(&format!("Loaded {n} demo transactions"));
                    uc();
                    go(2);
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    {
        let (go, uc) = (go.clone(), update_count.clone());
        next2.connect_clicked(move |_| {
            uc();
            go(2);
        });
    }
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        start.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.set_setting("onboarded", "1");
                dlg.close();
                triage(&a);
            }
        });
    }
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        done.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.set_setting("onboarded", "1");
                dlg.close();
                a.refresh();
            }
        });
    }
    {
        let w = Rc::downgrade(app);
        dlg.connect_closed(move |_| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.set_setting("onboarded", "1");
            }
        });
    }
    dlg.present(Some(&app.win));
}

// ──────────────────────────────────────────────────────────────── Smart Triage

const QUICK: [&str; 10] = ["Groceries", "Dining & Takeaway", "Utilities", "Shopping", "Transport", "Repairs & Maintenance", "Property Expense", "Health & Fitness", "Bank Fees & Interest", "Income"];

fn total_text(g: &TriageGroup) -> String {
    g.totals.iter().map(|(c, v)| fmt_money(v.abs(), *c)).collect::<Vec<_>>().join(" + ")
}

/// "4 payments of CI$ 125.00 (CI$ 500.00 in all)", or for varying amounts "3 payments totaling … (latest …)".
fn amounts_text(g: &TriageGroup) -> String {
    let typical = fmt_money(g.typical_amount.abs(), g.currency);
    match (g.count, g.same_amount) {
        (1, _) => format!("1 payment of {typical}"),
        (n, true) => format!("{n} payments of {typical} each ({} in all)", total_text(g)),
        (n, false) => format!("{n} payments totaling {} (latest {typical})", total_text(g)),
    }
}

fn cadence_word(days: Option<i64>) -> &'static str {
    match days {
        Some(7) => "/wk",
        Some(14) => "/2wk",
        Some(30) => "/mo",
        Some(91) => "/qtr",
        Some(365) => "/yr",
        _ => "",
    }
}

/// Forget triage answers and manual categories in the workspaces on screen, then start triage again.
pub fn reset_and_retriage(app: &Rc<App>) {
    let d = adw::AlertDialog::new(
        Some("Reset Learning?"),
        Some("Every learned rule, category, payee name and subscription flag in the workspaces you're viewing is forgotten, and Smart Triage starts over. Transfers matched between your own accounts stay matched. Your statements aren't touched."),
    );
    d.add_responses(&[("cancel", "Cancel"), ("reset", "Reset & Re-triage")]);
    d.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| {
        let Some(a) = w.upgrade() else { return };
        if r != "reset" {
            return;
        }
        match a.db.reset_learning(&a.entity_ids()) {
            Ok(n) => {
                a.toast(&format!("Forgotten — {n} transactions are back in triage"));
                a.refresh();
                triage(&a);
            }
            Err(e) => a.toast(&e.to_string()),
        }
    });
    d.present(Some(&app.win));
}

pub fn triage(app: &Rc<App>) {
    let skipped: Rc<RefCell<HashSet<String>>> = Rc::default();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(18).margin_bottom(24).margin_start(28).margin_end(28).build();
    let header_title = adw::WindowTitle::new("Smart Triage", "");
    let skip = gtk::Button::builder().label("Skip").build();
    let header = adw::HeaderBar::builder().title_widget(&header_title).build();
    header.pack_start(&skip);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&content).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build()));
    let dlg = adw::Dialog::builder().content_width(680).content_height(640).child(&tv).build();
    let current: Rc<RefCell<Option<TriageGroup>>> = Rc::default();
    let answered = Rc::new(std::cell::Cell::new(0usize));

    let show: Rc<RefCell<Option<crate::Refresh>>> = Rc::default();
    let show_fn: crate::Refresh = {
        let (w, content, header_title, skipped, current, dlg, show, answered) = (Rc::downgrade(app), content.clone(), header_title.clone(), skipped.clone(), current.clone(), dlg.clone(), show.clone(), answered.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            clear(&content);
            let groups: Vec<TriageGroup> = a.db.triage(&a.entity_ids()).unwrap_or_default().into_iter().filter(|g| !skipped.borrow().contains(&g.stem)).collect();
            let Some(g) = groups.first().cloned() else {
                header_title.set_subtitle("All done");
                let sp = adw::StatusPage::builder().icon_name("object-select-symbolic").title("All caught up").description(format!("{} answers learned. New statements will be categorized automatically.", answered.get())).build();
                let close = gtk::Button::builder().label("Close").halign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
                let d = dlg.clone();
                close.connect_clicked(move |_| {
                    d.close();
                });
                sp.set_child(Some(&close));
                content.append(&sp);
                *current.borrow_mut() = None;
                a.refresh();
                return;
            };
            header_title.set_subtitle(&format!("{} merchant{} left", groups.len(), if groups.len() == 1 { "" } else { "s" }));
            *current.borrow_mut() = Some(g.clone());
            let next = show.borrow().clone().unwrap();
            card(&a, &content, &g, next, answered.clone());
        })
    };
    *show.borrow_mut() = Some(show_fn.clone());
    {
        let (skipped, current, show) = (skipped.clone(), current.clone(), show_fn.clone());
        skip.connect_clicked(move |_| {
            if let Some(g) = current.borrow().as_ref() {
                skipped.borrow_mut().insert(g.stem.clone());
            }
            show();
        });
    }
    {
        let w = Rc::downgrade(app);
        dlg.connect_closed(move |_| {
            if let Some(a) = w.upgrade() {
                a.refresh();
            }
        });
    }
    show_fn();
    dlg.present(Some(&app.win));
}

/// Learn the answer for a group: a rule over its merchant stem, applied to every match.
#[allow(clippy::too_many_arguments)]
fn answer(app: &Rc<App>, g: &TriageGroup, category_id: Option<i64>, payee: &str, subscription: bool, scope: Option<i64>, next: &Rc<dyn Fn()>, answered: &Rc<std::cell::Cell<usize>>) {
    let rule = Rule {
        id: 0,
        kind: RuleKind::NormalizedTokens,
        pattern: g.stem.clone(),
        clean_payee: Some(payee.trim().to_string()).filter(|s| !s.is_empty()),
        category_id,
        is_subscription: subscription,
        is_transfer: false,
        expected_cadence: if subscription { g.cadence.or(Some(30)) } else { None },
        entity_id: scope,
    };
    match app.db.learn(&rule) {
        Ok(n) => {
            // the wizard's own group is confirmed even where an older rule had guessed
            let _ = app.db.set_category(&g.tx_ids, category_id);
            if subscription {
                let _ = app.db.set_subscription(&g.tx_ids, true);
            }
            answered.set(answered.get() + 1);
            app.toast(&format!("Learned “{}” · updated {} transaction{}", g.suggested_payee, n.max(g.count), if n.max(g.count) == 1 { "" } else { "s" }));
        }
        Err(e) => app.toast(&e.to_string()),
    }
    next();
}

fn card(app: &Rc<App>, content: &gtk::Box, g: &TriageGroup, next: Rc<dyn Fn()>, answered: Rc<std::cell::Cell<usize>>) {
    let cats: Vec<Category> = app.categories();
    let ents = app.entities.borrow().clone();
    let cat_id = |name: &str| cats.iter().find(|c| c.name == name).map(|c| c.id);
    let plural = if g.count == 1 { "charge" } else { "charges" };
    let typical = fmt_money(-g.typical_amount, g.currency);

    let q = gtk::Label::builder().xalign(0.0).wrap(true).use_markup(true).css_classes(["title-3"]).build();
    let esc = |s: &str| glib::markup_escape_text(s).to_string();
    q.set_markup(&match g.kind {
        TriageKind::Subscription => format!("We found {} {plural} from <tt>{}</tt> ({typical}{}). Is <b>{}</b> a <b>Subscription</b>?", g.count, esc(&g.stem), cadence_word(g.cadence), esc(&g.suggested_payee)),
        TriageKind::Transfer => format!("We found {} to <tt>{}</tt>. Is this an internal credit card payment or transfer between your accounts?", esc(&amounts_text(g)), esc(&g.stem)),
        TriageKind::Merchant => format!("We found {} from <tt>{}</tt>. How should we categorize <b>{}</b>?", esc(&amounts_text(g)), esc(&g.stem), esc(&g.suggested_payee)),
    });
    content.append(&q);
    content.append(&dim(&format!("Latest: {}", g.sample)));

    // payee + scope
    let opts = adw::PreferencesGroup::new();
    let payee = adw::EntryRow::builder().title("Clean payee").text(&g.suggested_payee).build();
    opts.add(&payee);
    // scope: the workspace(s) these charges came from, or every workspace
    let mut scope_ids: Vec<Option<i64>> = ents.iter().filter(|e| g.entity_ids.contains(&e.id)).map(|e| Some(e.id)).collect();
    let mut scope_names: Vec<String> = ents.iter().filter(|e| g.entity_ids.contains(&e.id)).map(|e| format!("Apply to {}", e.name)).collect();
    scope_ids.push(None);
    scope_names.push("Apply to all workspaces".into());
    let scope_row = adw::ComboRow::builder()
        .title("Remember for")
        .model(&gtk::StringList::new(&scope_names.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(if g.entity_ids.len() == 1 { 0 } else { scope_ids.len() as u32 - 1 })
        .build();
    opts.add(&scope_row);
    let scope = {
        let r = scope_row.clone();
        move || scope_ids.get(r.selected() as usize).copied().flatten()
    };
    let scope = Rc::new(scope);

    // category chips (hidden behind "pick category" for subscription/transfer cards)
    let chips_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).visible(g.kind == TriageKind::Merchant).build();
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(4).column_spacing(6).row_spacing(6).build();
    let mut names: Vec<&str> = Vec::new();
    if let Some(s) = g.suggested_category {
        names.push(s);
    }
    names.extend(QUICK.iter().copied().filter(|n| Some(*n) != g.suggested_category));
    for (i, name) in names.iter().enumerate() {
        let b = gtk::Button::builder().label(*name).css_classes(if i == 0 && g.suggested_category.is_some() { vec!["chip", "suggested-action"] } else { vec!["chip"] }).build();
        let (w, g2, id, payee, scope, next, ans) = (Rc::downgrade(app), g.clone(), cat_id(name), payee.clone(), scope.clone(), next.clone(), answered.clone());
        b.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                answer(&a, &g2, id, &payee.text(), false, scope(), &next, &ans);
            }
        });
        flow.insert(&b, -1);
    }
    chips_box.append(&flow);
    let more_names: Vec<String> = std::iter::once("More categories…".to_string()).chain(cats.iter().map(|c| if c.parent_id.is_some() { format!("   {}", c.name) } else { c.name.clone() })).collect();
    let more = gtk::DropDown::builder().model(&gtk::StringList::new(&more_names.iter().map(String::as_str).collect::<Vec<_>>())).halign(gtk::Align::Start).build();
    {
        let (w, g2, cats, payee, scope, next, ans) = (Rc::downgrade(app), g.clone(), cats.clone(), payee.clone(), scope.clone(), next.clone(), answered.clone());
        more.connect_selected_notify(move |dd| {
            let Some(c) = (dd.selected() as usize).checked_sub(1).and_then(|i| cats.get(i)) else { return };
            if let Some(a) = w.upgrade() {
                answer(&a, &g2, Some(c.id), &payee.text(), false, scope(), &next, &ans);
            }
        });
    }
    chips_box.append(&more);

    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    match g.kind {
        TriageKind::Subscription => {
            let yes = gtk::Button::builder().label("Yes — Mark as Subscription & Categorize as “Subscriptions & Digital”").css_classes(["suggested-action", "pill"]).build();
            let no = gtk::Button::builder().label("Not a Subscription — Pick Category…").css_classes(["pill"]).build();
            let (w, g2, id, payee2, scope2, next2, ans) = (Rc::downgrade(app), g.clone(), cat_id("Subscriptions & Digital"), payee.clone(), scope.clone(), next.clone(), answered.clone());
            yes.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    answer(&a, &g2, id, &payee2.text(), true, scope2(), &next2, &ans);
                }
            });
            let cb = chips_box.clone();
            no.connect_clicked(move |_| cb.set_visible(true));
            buttons.append(&yes);
            buttons.append(&no);
        }
        TriageKind::Transfer => {
            let yes = gtk::Button::builder().label("Yes, Internal Transfer (Exclude from Spending)").css_classes(["suggested-action", "pill"]).build();
            let no = gtk::Button::builder().label("No, Regular Expense").css_classes(["pill"]).build();
            let (w, g2, scope2, next2, ans) = (Rc::downgrade(app), g.clone(), scope.clone(), next.clone(), answered.clone());
            yes.connect_clicked(move |_| {
                let Some(a) = w.upgrade() else { return };
                match a.db.mark_transfers(&g2.tx_ids, &g2.stem, scope2()) {
                    Ok(n) => {
                        ans.set(ans.get() + 1);
                        a.toast(&format!("Marked as transfers · {n} new pair{} matched across your accounts", if n == 1 { "" } else { "s" }));
                    }
                    Err(e) => a.toast(&e.to_string()),
                }
                next2();
            });
            let cb = chips_box.clone();
            no.connect_clicked(move |_| cb.set_visible(true));
            buttons.append(&yes);
            buttons.append(&no);
        }
        TriageKind::Merchant => {}
    }
    content.append(&buttons);
    content.append(&chips_box);
    content.append(&opts);
}

// ──────────────────────────────────────────────────────────────── workspaces

pub fn workspaces_dialog(app: &Rc<App>) {
    let page = adw::PreferencesPage::new();
    let g = adw::PreferencesGroup::builder().title("Workspaces").description("Each keeps its own accounts and P&L. Changing a currency revalues its transactions.").build();
    let ents = app.entities.borrow().clone();
    let mut edits = Vec::new();
    for e in &ents {
        let name = adw::EntryRow::builder().title(format!("{} workspace", e.kind.as_str())).text(&e.name).build();
        let ccy = adw::ComboRow::builder().title(format!("{} currency", e.name)).model(&gtk::StringList::new(&["KYD", "USD"])).selected(if e.base_currency == Currency::USD { 1 } else { 0 }).build();
        g.add(&name);
        g.add(&ccy);
        edits.push((e.id, name, ccy));
    }
    page.add(&g);
    let add = adw::PreferencesGroup::builder().title("Add a Workspace").build();
    let new_name = adw::EntryRow::builder().title("Name (e.g. Business 2, Seven Mile Condo)").build();
    let new_kind = adw::ComboRow::builder().title("Kind").model(&gtk::StringList::new(&["Personal", "Business", "Rental Property"])).selected(1).build();
    add.add(&new_name);
    add.add(&new_kind);
    page.add(&add);
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().title("Workspaces").content_width(560).content_height(560).child(&tv).build();
    let (w, d) = (Rc::downgrade(app), dlg.clone());
    save.connect_clicked(move |_| {
        let Some(a) = w.upgrade() else { return };
        for (id, name, ccy) in &edits {
            let c = if ccy.selected() == 1 { Currency::USD } else { Currency::KYD };
            if let Err(e) = a.db.update_entity(*id, name.text().trim(), c) {
                a.toast(&e.to_string());
            }
        }
        let n = new_name.text().trim().to_string();
        if !n.is_empty() {
            let kind = [EntityKind::Personal, EntityKind::Business, EntityKind::RentalProperty][new_kind.selected() as usize];
            if let Err(e) = a.db.create_entity(&n, kind, Currency::KYD) {
                a.toast(&e.to_string());
            }
        }
        a.reload_entities();
        d.close();
    });
    dlg.present(Some(&app.win));
}
