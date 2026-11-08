//! Getting Started: the guided first run, and the guide in the main menu. Eight steps, in the order
//! the work really happens — download statements from the bank, save them in one folder, point
//! FinGraph at it, let it read them (money in and out, proven against each statement's balances),
//! sort what it didn't recognise, prepare a budget, and what to do each month after.
//!
//! Every step can be revisited from the list on the left, which ticks off what's done; the guide
//! remembers where you stopped. Statements arrive a month at a time from Cayman banks, so the last
//! step sets up the monthly routine rather than pretending anything is live.

use adw::prelude::*;
use fingraph_core::db::TxFilter;
use fingraph_core::insights::{data_span, month_start};
use fingraph_core::money::fmt_money;
use fingraph_core::workflows::MonthState;
use fingraph_core::{Currency, Decimal, EntityKind, Reconciliation};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use crate::{App, clear, dim, heading, pill, today};

const BANKS: [(&str, &str, &str); 5] = [
    ("butterfield_ky", "Butterfield", "Bank of Butterfield (Cayman)"),
    ("scotiabank_ky", "Scotiabank", "Scotiabank & Trust (Cayman)"),
    ("cnb_ky", "Cayman National", "Cayman National Bank"),
    ("rbc_ky", "RBC", "RBC Royal Bank (Cayman)"),
    ("cicsa_cu_ky", "Credit Union", "CICSA Co-operative Credit Union"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Workspaces,
    Download,
    Folder,
    Read,
    Sort,
    Budget,
    Monthly,
}

const STEPS: [Step; 8] = [Step::Welcome, Step::Workspaces, Step::Download, Step::Folder, Step::Read, Step::Sort, Step::Budget, Step::Monthly];

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Workspaces => "Who It's For",
            Step::Download => "Download Statements",
            Step::Folder => "One Folder",
            Step::Read => "Read Them",
            Step::Sort => "Sort Transactions",
            Step::Budget => "Your Budget",
            Step::Monthly => "Every Month",
        }
    }
}

/// The guide's state while it's open.
struct Guide {
    app: Weak<App>,
    dlg: adw::Dialog,
    side: gtk::ListBox,
    body: gtk::Box,
    scroller: gtk::ScrolledWindow,
    back: gtk::Button,
    next: gtk::Button,
    note: gtk::Label,
    step: Cell<usize>,
    /// What Next does on this step (set by each step as it's drawn).
    on_next: RefCell<Option<Rc<dyn Fn()>>>,
    /// Statements being read right now, and that read's progress and results (shown again on coming back).
    reading: Cell<bool>,
    read_view: RefCell<Option<gtk::Box>>,
    /// Merchants skipped on the Sort step this time.
    skipped: RefCell<HashSet<String>>,
    answered: Rc<Cell<usize>>,
}

type G = Rc<Guide>;

fn list() -> gtk::ListBox {
    gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build()
}

fn row(title: &str, sub: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(glib::markup_escape_text(title)).subtitle(glib::markup_escape_text(sub)).subtitle_lines(0).build()
}

/// A numbered row for a sequence (1, 2, 3…).
fn numbered(n: usize, title: &str, sub: &str) -> adw::ActionRow {
    let r = row(title, sub);
    r.add_prefix(&gtk::Label::builder().label(n.to_string()).css_classes(["guide-num"]).valign(gtk::Align::Center).build());
    r
}

fn icon_row(icon: &str, title: &str, sub: &str) -> adw::ActionRow {
    let r = row(title, sub);
    r.add_prefix(&gtk::Image::builder().icon_name(icon).pixel_size(20).valign(gtk::Align::Center).build());
    r
}

/// The folder statements live in (the watched folder, once chosen).
fn folder(app: &App) -> Option<PathBuf> {
    app.db.prefs().watch_folder
}

fn suggested_folder() -> PathBuf {
    glib::user_special_dir(glib::UserDirectory::Documents).unwrap_or_else(glib::home_dir).join("Bank Statements")
}

fn display_path(p: &std::path::Path) -> String {
    let home = glib::home_dir();
    match p.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// PDFs in the folder not yet read (the watcher's memory of path, size and time).
fn unread(app: &App, dir: &std::path::Path) -> Vec<PathBuf> {
    let seen: HashSet<String> = app.db.setting("watch_seen").map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default();
    crate::statements::pdfs_in(dir)
        .into_iter()
        .filter(|p| std::fs::metadata(p).ok().map(|m| format!("{}|{}|{:?}", p.display(), m.len(), m.modified().ok())).is_none_or(|k| !seen.contains(&k)))
        .collect()
}

/// Whether a step's work is done (for the ticks on the left).
fn done(app: &App, s: Step) -> bool {
    let ids = app.entity_ids();
    match s {
        Step::Welcome => app.db.setting("guide_step").is_some_and(|v| v != "0"),
        Step::Workspaces => !app.entities.borrow().is_empty(),
        Step::Download | Step::Read => app.db.documents_all(&ids).is_ok_and(|d| !d.is_empty()),
        Step::Folder => folder(app).is_some(),
        Step::Sort => app.db.documents_all(&ids).is_ok_and(|d| !d.is_empty()) && app.db.uncategorized_count(&ids).unwrap_or(1) == 0,
        Step::Budget => !app.db.budgets(&fingraph_core::budget::scope(&ids)).unwrap_or_default().is_empty(),
        Step::Monthly => app.db.setting("onboarded").is_some(),
    }
}

/// Open the guide where you left it (or at the start).
pub fn open(app: &Rc<App>) {
    let at = app.db.setting("guide_step").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(STEPS.len() - 1);
    open_at(app, at);
}

pub fn open_at(app: &Rc<App>, at: usize) {
    let side = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
    let side_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).width_request(230).build();
    side_box.append(&gtk::Label::builder().label("Getting Started").xalign(0.0).css_classes(["heading"]).margin_top(18).margin_start(18).margin_bottom(6).build());
    side_box.append(&side);
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(8).margin_bottom(24).margin_start(28).margin_end(28).build();
    let scroller = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(720).child(&body).build()).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let back = gtk::Button::builder().label("Back").build();
    let next = gtk::Button::builder().css_classes(["suggested-action", "pill"]).build();
    let note = gtk::Label::builder().css_classes(["dim-label", "caption"]).hexpand(true).wrap(true).xalign(0.5).build();
    let bar = gtk::Box::builder().spacing(12).margin_top(10).margin_bottom(10).margin_start(18).margin_end(18).build();
    bar.append(&back);
    bar.append(&note);
    bar.append(&next);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
    tv.set_content(Some(&scroller));
    tv.add_bottom_bar(&bar);
    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.append(&side_box);
    split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    tv.set_hexpand(true);
    split.append(&tv);
    let dlg = adw::Dialog::builder().title("Getting Started").content_width(1000).content_height(760).child(&split).build();

    let g: G = Rc::new(Guide {
        app: Rc::downgrade(app),
        dlg: dlg.clone(),
        side: side.clone(),
        body,
        scroller,
        back: back.clone(),
        next: next.clone(),
        note,
        step: Cell::new(at),
        on_next: RefCell::default(),
        reading: Cell::new(false),
        read_view: RefCell::new(None),
        skipped: RefCell::default(),
        answered: Rc::default(),
    });
    for (i, s) in STEPS.iter().enumerate() {
        let b = gtk::Box::builder().spacing(10).margin_top(6).margin_bottom(6).margin_start(6).build();
        b.append(&gtk::Label::builder().label((i + 1).to_string()).css_classes(["guide-num"]).build());
        b.append(&gtk::Label::builder().label(s.title()).xalign(0.0).hexpand(true).build());
        side.append(&gtk::ListBoxRow::builder().child(&b).build());
    }
    {
        let g = Rc::downgrade(&g);
        side.connect_row_activated(move |_, r| {
            if let Some(g) = g.upgrade() {
                show(&g, r.index() as usize);
            }
        });
    }
    {
        let g = Rc::downgrade(&g);
        back.connect_clicked(move |_| {
            if let Some(g) = g.upgrade() {
                show(&g, g.step.get().saturating_sub(1));
            }
        });
    }
    {
        let g = Rc::downgrade(&g);
        next.connect_clicked(move |_| {
            let f = g.upgrade().and_then(|g| g.on_next.borrow().clone());
            if let Some(f) = f {
                f();
            }
        });
    }
    {
        // closing early keeps your place; the guide is in the main menu
        let w = Rc::downgrade(app);
        dlg.connect_closed(move |_| {
            if let Some(a) = w.upgrade() {
                a.saved(a.db.set_setting("onboarded", "1"));
                a.refresh();
            }
        });
    }
    {
        // the buttons hold the guide weakly; the dialog keeps it alive until it closes
        let keep = RefCell::new(Some(g.clone()));
        dlg.connect_closed(move |_| {
            keep.borrow_mut().take();
        });
    }
    show(&g, at);
    dlg.present(Some(&app.win));
}

/// Mark the steps on the left: the one you're on, and a tick for each one that's done.
fn mark(g: &Guide, app: &App) {
    let mut i = 0;
    while let Some(r) = g.side.row_at_index(i) {
        if let Some(num) = r.child().and_then(|b| b.first_child()).and_downcast::<gtk::Label>() {
            let ok = done(app, STEPS[i as usize]);
            num.set_label(&if ok { "✓".to_string() } else { (i + 1).to_string() });
            num.set_css_classes(if ok { &["guide-num", "done"] } else { &["guide-num"] });
        }
        if i as usize == g.step.get() {
            g.side.select_row(Some(&r));
        }
        i += 1;
    }
}

fn show(g: &G, i: usize) {
    let Some(app) = g.app.upgrade() else { return };
    g.step.set(i);
    app.saved(app.db.set_setting("guide_step", &i.to_string()));
    clear(&g.body);
    g.scroller.vadjustment().set_value(0.0);
    g.back.set_visible(i > 0);
    g.note.set_label("");
    g.next.set_sensitive(true);
    let s = STEPS[i];
    let h = gtk::Label::builder().label(format!("Step {} of {}", i + 1, STEPS.len())).xalign(0.0).css_classes(["dim-label", "caption-heading"]).build();
    g.body.append(&h);
    match s {
        Step::Welcome => welcome(g, &app),
        Step::Workspaces => workspaces(g, &app),
        Step::Download => download(g, &app),
        Step::Folder => choose_folder(g, &app),
        Step::Read => read(g, &app),
        Step::Sort => sort(g, &app),
        Step::Budget => budget(g, &app),
        Step::Monthly => monthly(g, &app),
    }
    mark(g, &app);
}

/// Next goes to the following step (after `then`, if given).
fn next_to(g: &G, label: &str, then: Option<Rc<dyn Fn() -> bool>>) {
    g.next.set_label(label);
    let w = Rc::downgrade(g);
    *g.on_next.borrow_mut() = Some(Rc::new(move || {
        let Some(g) = w.upgrade() else { return };
        if then.as_ref().is_none_or(|f| f()) {
            show(&g, (g.step.get() + 1).min(STEPS.len() - 1));
        }
    }));
}

fn title(g: &Guide, t: &str, sub: &str) {
    g.body.append(&gtk::Label::builder().label(t).xalign(0.0).wrap(true).css_classes(["title-1"]).build());
    if !sub.is_empty() {
        g.body.append(&gtk::Label::builder().label(sub).xalign(0.0).wrap(true).css_classes(["body"]).build());
    }
}

// ──────────────────────────────────────────────────────────────── 1. welcome

fn welcome(g: &G, app: &Rc<App>) {
    title(
        g,
        "Let's Set Up FinGraph",
        "FinGraph reads the PDF statements your bank already gives you, checks that every one adds up to the cent, sorts each transaction into a category, and helps you plan a budget from what you really spend.",
    );
    g.body.append(&heading("How it works"));
    let l = list();
    l.append(&numbered(1, "Download your statements", "From each bank's online banking, as PDF files — the longest part, about 10 to 15 minutes."));
    l.append(&numbered(2, "Save them in one folder", "FinGraph reads that folder now, and checks it for new statements every month after."));
    l.append(&numbered(3, "FinGraph reads them", "Every transaction on every account — money in and money out — checked against each statement's opening and closing balances."));
    l.append(&numbered(4, "Sort your transactions", "Most are recognised on their own. For the rest, you answer once per shop or payee, and FinGraph remembers."));
    l.append(&numbered(5, "Prepare your budget", "Starting from what you usually spend, with your pay and a savings goal."));
    g.body.append(&l);
    let l = list();
    l.append(&icon_row("channel-secure-symbolic", "Private", "Nothing is uploaded. Your statements, and everything FinGraph learns from them, stay on this computer."));
    l.append(&icon_row("x-office-calendar-symbolic", "A month at a time", "Cayman banks don't connect to budgeting apps, so FinGraph works from statements: each month you add the new ones, and it catches up."));
    l.append(&icon_row("document-revert-symbolic", "Stop whenever you like", "The guide remembers where you were. It's in the main menu as Getting Started."));
    g.body.append(&l);
    // trying it out without your own statements
    let l = list();
    let r = icon_row("applications-science-symbolic", "Just looking?", "Load a year of made-up statements to explore every page. They mix with your own, so before adding real statements, clear them with Preferences → Data → Erase All.");
    let demo = gtk::Button::builder().label("Load Demo Data").valign(gtk::Align::Center).build();
    {
        let (w, g2) = (Rc::downgrade(app), Rc::downgrade(g));
        demo.connect_clicked(move |b| {
            let (Some(a), Some(g)) = (w.upgrade(), g2.upgrade()) else { return };
            b.set_sensitive(false);
            match a.db.load_demo() {
                Ok(n) => {
                    a.reload_entities();
                    a.toast(&format!("Loaded {n} demo transactions"));
                    a.saved(a.db.set_setting("onboarded", "1"));
                    g.dlg.close();
                    a.show_view("budget");
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    r.add_suffix(&demo);
    l.append(&r);
    g.body.append(&l);
    next_to(g, "Get Started", None);
}

// ──────────────────────────────────────────────────────────────── 2. workspaces

fn workspaces(g: &G, app: &Rc<App>) {
    title(
        g,
        "Whose Money Is This?",
        "A workspace keeps one set of money apart from another. Most people need just one. Add a second if you also run a business or a rental property, so its income and costs never mix with the household's — a combined view puts them together whenever you want.",
    );
    let ents = app.entities.borrow().clone();
    let personal_name = ents.iter().find(|e| e.kind == EntityKind::Personal).map_or("Personal".to_string(), |e| e.name.clone());
    let biz_existing = ents.iter().find(|e| e.kind != EntityKind::Personal).map(|e| e.name.clone());
    let gr = adw::PreferencesGroup::new();
    let personal = adw::EntryRow::builder().title("Your household").text(&personal_name).build();
    let biz_on = adw::SwitchRow::builder().title("I also have a business or rental property").active(biz_existing.is_some()).build();
    let biz = adw::EntryRow::builder().title("Its name").text(biz_existing.clone().unwrap_or_else(|| "Business".into())).build();
    let ccy = crate::preferences::currency_row("Show totals in", ents.first().map_or(Currency::KYD, |e| e.base_currency));
    ccy.set_subtitle("KYD and USD convert at 1 KYD = 1.20 USD; other currencies at each day's rate once exchange-rate downloads are on (Preferences → Money). A rate printed on a statement wins");
    biz_on.bind_property("active", &biz, "visible").sync_create().build();
    for r in [personal.upcast_ref::<gtk::Widget>(), biz_on.upcast_ref(), biz.upcast_ref(), ccy.upcast_ref()] {
        gr.add(r);
    }
    g.body.append(&gr);
    g.body.append(&dim("A business's spending paid from your personal card can still be counted in the business — you'll see how when you sort transactions. Names can be changed any time in Preferences."));
    let w = Rc::downgrade(app);
    next_to(
        g,
        "Save and Continue",
        Some(Rc::new(move || {
            let Some(a) = w.upgrade() else { return false };
            let c = crate::preferences::currency_at(ccy.selected());
            let mut want = vec![(personal.text().trim().to_string(), EntityKind::Personal)];
            if biz_on.is_active() {
                want.push((biz.text().trim().to_string(), EntityKind::Business));
            }
            if want[0].0.is_empty() {
                a.toast("Give the household workspace a name");
                return false;
            }
            for (name, kind) in want.into_iter().filter(|(n, _)| !n.is_empty()) {
                // rename the existing one of this kind rather than adding another
                let existing = a.entities.borrow().iter().find(|e| (e.kind == EntityKind::Personal) == (kind == EntityKind::Personal)).map(|e| e.id);
                let _ = match existing.or_else(|| a.db.entity_by_name(&name).ok().flatten()) {
                    Some(id) => a.db.update_entity(id, &name, c),
                    None => a.db.create_entity(&name, kind, c).map(|_| ()),
                };
            }
            a.reload_entities();
            a.set_display(c);
            true
        })),
    );
}

// ──────────────────────────────────────────────────────────────── 3. download

fn download(g: &G, app: &Rc<App>) {
    title(g, "Download Your Statements", "Sign in to each bank's online banking and download your statements as PDF files. Here's what to get, so FinGraph has everything it needs the first time.");
    g.body.append(&heading("Which banks do you use?"));
    let chosen: HashSet<String> = app.db.setting("guide_banks").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default();
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(3).min_children_per_line(2).column_spacing(10).row_spacing(10).homogeneous(true).build();
    let where_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
    let toggles: Rc<RefCell<Vec<(String, gtk::ToggleButton)>>> = Rc::default();
    let opts: Vec<(&str, &str, &str)> = BANKS.iter().copied().chain(std::iter::once(("other", "Another bank", "Not one of these"))).collect();
    for (slug, short, long) in opts {
        let t = gtk::ToggleButton::builder().css_classes(["card", "bank-card"]).active(chosen.contains(slug)).build();
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        b.append(&gtk::Label::builder().label(short).css_classes(["heading"]).build());
        b.append(&gtk::Label::builder().label(long).css_classes(["caption", "dim-label"]).wrap(true).justify(gtk::Justification::Center).build());
        t.set_child(Some(&b));
        flow.insert(&t, -1);
        toggles.borrow_mut().push((slug.to_string(), t));
    }
    g.body.append(&flow);
    g.body.append(&where_box);
    let redo_where = {
        let (w, toggles, where_box) = (Rc::downgrade(app), toggles.clone(), where_box.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let on: Vec<String> = toggles.borrow().iter().filter(|t| t.1.is_active()).map(|t| t.0.clone()).collect();
            a.saved(a.db.set_setting("guide_banks", &on.join(",")));
            clear(&where_box);
            if on.is_empty() {
                return;
            }
            where_box.append(&heading("Where to find them"));
            let l = list();
            for slug in &on {
                let r = match BANKS.iter().find(|b| b.0 == slug) {
                    Some((_, short, long)) => {
                        let r = row(long, "Sign in to online banking, open each account and look for Statements or eStatements. Choose the PDF version and save every month offered.");
                        r.add_prefix(&pill(short, "bank"));
                        r
                    }
                    None => {
                        let r = row("Another bank", "Download its PDF statements too. FinGraph doesn't know that bank's layout yet: after this guide, main menu → Bank Profiles → New Profile From a Sample teaches it from one statement.");
                        r.add_prefix(&pill("Other", "meh"));
                        r
                    }
                };
                l.append(&r);
            }
            where_box.append(&l);
        })
    };
    for (_, t) in toggles.borrow().iter() {
        let r = redo_where.clone();
        t.connect_toggled(move |_| r());
    }
    redo_where();

    g.body.append(&heading("What to download"));
    let l = list();
    l.append(&icon_row(
        "view-list-bullet-symbolic",
        "Every account",
        "Chequing, savings, each credit card, loans and credit-union shares. A credit card has its own statement, separate from your bank account — get both. Payments between your own accounts are matched up, so nothing is counted twice.",
    ));
    l.append(&icon_row(
        "x-office-document-symbolic",
        "The PDF statement — not a spreadsheet or “transaction history”",
        "The statement prints the opening and closing balance; that's how FinGraph proves no transaction was missed. On-screen history printouts don't have them. If your bank only offers a download (RBC: CSV or OFX), import that — prefer OFX, which has the closing balance — and check each one before approving it.",
    ));
    l.append(&icon_row(
        "x-office-calendar-symbolic",
        "The last 12 months, if your bank offers them",
        "At least the last 6. Your budget starts from what you usually spend in a month, which needs a few months; bills that come once a year only show up with a full year.",
    ));
    l.append(&icon_row(
        "dialog-information-symbolic",
        "The files just as the bank gives them",
        "One file per statement; no need to rename them. Don't print them to PDF or scan paper statements — those have no text in them for FinGraph to read.",
    ));
    g.body.append(&l);
    g.body.append(&dim("Your browser probably saves them to your Downloads folder. That's fine — the next step puts them in one place."));
    next_to(g, "I've Downloaded Them", None);
    g.note.set_label("Not finished? Close this window — the guide will be here when you come back.");
}

// ──────────────────────────────────────────────────────────────── 4. the folder

fn choose_folder(g: &G, app: &Rc<App>) {
    title(
        g,
        "Put Them All in One Folder",
        "Choose a folder for your bank statements and move every PDF into it. Folders inside it — one per bank, or per year — are fine. FinGraph reads the whole folder now, and from then on picks up new statements you save there.",
    );
    let current = folder(app);
    let l = list();
    let status = adw::ActionRow::builder().title(current.as_ref().map_or("No folder chosen yet".to_string(), |p| display_path(p))).subtitle_lines(0).build();
    status.add_prefix(&gtk::Image::builder().icon_name("folder-symbolic").pixel_size(24).build());
    l.append(&status);
    g.body.append(&l);
    let buttons = gtk::Box::builder().spacing(8).build();
    let suggested = suggested_folder();
    let make = gtk::Button::builder().label(format!("Use {}", display_path(&suggested))).css_classes(["pill"]).build();
    let pick = gtk::Button::builder().label("Choose Another Folder…").css_classes(["pill"]).build();
    let open = gtk::Button::builder().label("Open in Files").css_classes(["pill"]).sensitive(current.is_some()).build();
    let look = gtk::Button::builder().label("Look Again").css_classes(["pill", "flat"]).sensitive(current.is_some()).build();
    if current.is_none() {
        make.add_css_class("suggested-action");
    }
    for b in [&make, &pick, &open, &look] {
        buttons.append(b);
    }
    g.body.append(&buttons);
    let found = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    g.body.append(&found);

    let watch = adw::SwitchRow::builder()
        .title("Check this folder for new statements")
        .subtitle("Each month, save the new statements here and FinGraph reads them when it opens (or straight away if it's open). Anything that isn't a bank statement is left alone.")
        .active(app.db.prefs().watch_enabled || current.is_none())
        .build();
    let wg = adw::PreferencesGroup::new();
    wg.add(&watch);
    g.body.append(&wg);
    g.body.append(&heading("Moving the files"));
    let tips = list();
    tips.append(&numbered(1, "Open in Files", "Opens the statements folder."));
    tips.append(&numbered(2, "Open Downloads in a second window", "In Files, press Ctrl+N and go to Downloads."));
    tips.append(&numbered(3, "Drag the statements across", "Select them all (Ctrl+click each one) and drag them into the statements folder. Come back here and press Look Again."));
    g.body.append(&tips);

    // what's in the folder now
    let refresh: Rc<dyn Fn()> = {
        let (w, g2, found, status, open, look) = (Rc::downgrade(app), Rc::downgrade(g), found.clone(), status.clone(), open.clone(), look.clone());
        Rc::new(move || {
            let (Some(a), Some(g)) = (w.upgrade(), g2.upgrade()) else { return };
            clear(&found);
            let Some(dir) = folder(&a) else {
                g.next.set_sensitive(false);
                g.note.set_label("Choose a folder first");
                return;
            };
            status.set_title(&glib::markup_escape_text(&display_path(&dir)));
            open.set_sensitive(true);
            look.set_sensitive(true);
            let all = crate::statements::pdfs_in(&dir);
            let new = unread(&a, &dir);
            let (text, class) = match (all.len(), new.len()) {
                (0, _) => ("No PDF files in it yet — move your statements in, then press Look Again.".to_string(), "warning"),
                (n, 0) => (format!("{n} PDF file{} — all read already.", if n == 1 { "" } else { "s" }), "success"),
                (n, k) if k == n => (format!("{n} PDF file{} ready to read.", if n == 1 { "" } else { "s" }), "success"),
                (n, k) => (format!("{n} PDF files: {k} new, {} read already.", n - k), "success"),
            };
            status.set_subtitle(&text);
            status.set_css_classes(&[class]);
            let have_docs = a.db.documents_all(&a.entity_ids()).is_ok_and(|d| !d.is_empty());
            g.next.set_sensitive(!all.is_empty() || have_docs);
            g.note.set_label(if all.is_empty() && !have_docs { "Move your statements into the folder to continue" } else { "" });
            if !all.is_empty() {
                let names: Vec<String> = all.iter().take(6).map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
                found.append(&dim(&format!("{}{}", names.join(" · "), if all.len() > 6 { format!(" · and {} more", all.len() - 6) } else { String::new() })));
            }
        })
    };
    let set_folder = {
        let (w, refresh, make) = (Rc::downgrade(app), refresh.clone(), make.clone());
        Rc::new(move |dir: PathBuf| {
            let Some(a) = w.upgrade() else { return };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return a.toast(&format!("Couldn't create {}: {e}", dir.display()));
            }
            let mut p = a.db.prefs();
            p.watch_folder = Some(dir);
            a.saved(a.db.save_prefs(&p));
            make.remove_css_class("suggested-action");
            refresh();
        })
    };
    {
        let set_folder = set_folder.clone();
        make.connect_clicked(move |_| set_folder(suggested.clone()));
    }
    {
        let (w, set_folder) = (Rc::downgrade(app), set_folder.clone());
        pick.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let fd = gtk::FileDialog::builder().title("Folder for Your Statements").modal(true).build();
            if let Some(f) = folder(&a) {
                fd.set_initial_folder(Some(&gtk::gio::File::for_path(f)));
            }
            let set_folder = set_folder.clone();
            fd.select_folder(Some(&a.win), gtk::gio::Cancellable::NONE, move |r| {
                if let Some(p) = r.ok().and_then(|f| f.path()) {
                    set_folder(p);
                }
            });
        });
    }
    {
        let w = Rc::downgrade(app);
        open.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            if let Some(dir) = folder(&a) {
                gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(dir))).launch(Some(&a.win), gtk::gio::Cancellable::NONE, |_| {});
            }
        });
    }
    {
        let refresh = refresh.clone();
        look.connect_clicked(move |_| refresh());
    }
    refresh();
    let w = Rc::downgrade(app);
    next_to(
        g,
        "Read My Statements",
        Some(Rc::new(move || {
            let Some(a) = w.upgrade() else { return false };
            let mut p = a.db.prefs();
            p.watch_enabled = watch.is_active();
            a.saved(a.db.save_prefs(&p));
            true
        })),
    );
}

// ──────────────────────────────────────────────────────────────── 5. read

fn read(g: &G, app: &Rc<App>) {
    title(g, "Reading Your Statements", "FinGraph recognises each statement's bank, account and currency, takes out every transaction, and checks that the opening balance plus everything in and out comes to the closing balance — to the cent.");
    let Some(dir) = folder(app) else {
        g.body.append(&dim("Choose your statements folder first (step 4)."));
        next_to(g, "Continue", None);
        return;
    };
    // still reading from an earlier visit: that read's own progress, not a summary of half of it
    if g.reading.get() {
        if let Some(v) = &*g.read_view.borrow() {
            g.body.append(v);
        }
        next_to(g, "Continue", None);
        g.next.set_sensitive(false);
        g.note.set_label("Still reading… this takes a few seconds a statement");
        return;
    }
    let todo = unread(app, &dir);
    let progress = gtk::ProgressBar::builder().show_text(true).build();
    let results = list();
    let summary = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    let view = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    view.append(&progress);
    view.append(&summary);
    view.append(&results);
    g.body.append(&view);
    *g.read_view.borrow_mut() = Some(view);
    results.set_visible(false);

    let finish: Rc<dyn Fn()> = {
        let (w, g2, summary, progress) = (Rc::downgrade(app), Rc::downgrade(g), summary.clone(), progress.clone());
        Rc::new(move || {
            let (Some(a), Some(g)) = (w.upgrade(), g2.upgrade()) else { return };
            g.reading.set(false);
            progress.set_visible(false);
            read_summary(&g, &a, &summary);
            // the buttons are this step's only while it's showing
            if STEPS[g.step.get()] == Step::Read {
                g.next.set_sensitive(a.db.documents_all(&a.entity_ids()).is_ok_and(|d| !d.is_empty()));
                g.note.set_label("");
            }
            mark(&g, &a);
        })
    };
    next_to(g, "Continue", None);
    if todo.is_empty() {
        progress.set_visible(false);
        finish();
        return;
    }
    // read the new files, one row each as they finish
    g.reading.set(true);
    g.next.set_sensitive(false);
    g.note.set_label("Reading… this takes a few seconds a statement");
    let total = todo.len();
    let n = Rc::new(Cell::new(0usize));
    progress.set_fraction(0.0);
    progress.set_text(Some(&format!("0 of {total}")));
    let on_each: crate::statements::OnEach = {
        let (results, progress, n) = (results.clone(), progress.clone(), n.clone());
        Rc::new(move |name: &str, r| {
            n.set(n.get() + 1);
            progress.set_fraction(n.get() as f64 / total as f64);
            progress.set_text(Some(&format!("{} of {total} · {name}", n.get())));
            results.set_visible(true);
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(name)).build();
            match r {
                Ok((scan, rep)) => {
                    let what: Vec<String> = scan.sections.iter().map(|s| format!("{} {} ••{}", s.currency, s.account_type.label(), s.account_last4.as_deref().unwrap_or("?"))).collect();
                    let period = scan.period.map(|(a, b)| format!("{} – {} · ", a.format("%d %b"), b.format("%d %b %Y"))).unwrap_or_default();
                    row.set_subtitle(&glib::markup_escape_text(&if rep.already_imported { format!("{period}{} · read before", what.join(" + ")) } else { format!("{period}{} · {} transactions", what.join(" + "), rep.inserted) }));
                    row.add_prefix(&pill(BANKS.iter().find(|b| Some(b.0) == scan.profile_slug.as_deref()).map_or("Unknown bank", |b| b.1), "bank"));
                    let ccy = scan.sections.first().map_or(Currency::KYD, |s| s.currency);
                    let d = scan.sections.iter().find_map(|s| if let Reconciliation::Discrepancy(d) = s.reconciliation { Some(d) } else { None });
                    row.add_suffix(&if scan.is_reconciled() {
                        pill("Adds up", "ok")
                    } else if let Some(d) = d {
                        pill(&format!("Off by {}", fmt_money(d.abs(), ccy)), "bad")
                    } else {
                        pill("Unchecked", "meh")
                    });
                }
                Err(e) => {
                    row.set_subtitle(&glib::markup_escape_text(&e));
                    row.add_suffix(&pill("Couldn't read", "bad"));
                }
            }
            results.append(&row);
        })
    };
    let done_cb: crate::Refresh = {
        let (w, todo2, finish) = (Rc::downgrade(app), todo.clone(), finish.clone());
        Rc::new(move || {
            if let Some(a) = w.upgrade() {
                // the watcher needn't read these again
                crate::statements::remember_seen(&a, &todo2);
            }
            finish();
        })
    };
    crate::statements::import_paths_with(app, todo, Some(on_each), Some(done_cb));
}

/// After reading: what came in, per account and month, what didn't add up, and what's missing.
fn read_summary(g: &G, app: &Rc<App>, into: &gtk::Box) {
    clear(into);
    let ids = app.entity_ids();
    let rows = app.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
    let docs = app.db.documents_all(&ids).unwrap_or_default();
    if docs.is_empty() {
        into.append(&dim("No statements read yet. If the folder's files were all marked “Couldn't read”, they may be scans or from a bank FinGraph doesn't know — see step 3."));
        return;
    }
    let ccy = app.display();
    let fx = app.db.fx().unwrap_or_default();
    let money = |d: Decimal| fmt_money(d, ccy);
    let (first, last) = data_span(&rows).unwrap_or((today(), today()));
    let n_months = ((last - first).num_days() / 30).max(1);
    let bars = fingraph_core::insights::monthly(&rows, last, n_months as i32 + 1, ccy, &fx);
    let (inc, exp): (Decimal, Decimal) = bars.iter().fold((Decimal::ZERO, Decimal::ZERO), |a, b| (a.0 + b.income, a.1 + b.expenses));
    let accounts: HashSet<i64> = docs.iter().map(|d| d.account_id).collect();
    into.append(&heading("What FinGraph found"));
    let l = list();
    let r = icon_row("emblem-ok-symbolic", &format!("{} statements · {} accounts · {} transactions", docs.len(), accounts.len(), rows.len()), &format!("From {} to {}", first.format("%d %b %Y"), last.format("%d %b %Y")));
    l.append(&r);
    let nm = Decimal::from(bars.len().max(1));
    let r = icon_row("value-increase-symbolic", "Money in", &format!("{} in all · about {} a month (not counting transfers between your own accounts)", money(inc), money((inc / nm).round_dp(0))));
    l.append(&r);
    let r = icon_row("value-decrease-symbolic", "Money out", &format!("{} in all · about {} a month", money(exp), money((exp / nm).round_dp(0))));
    l.append(&r);
    into.append(&l);

    // statements that didn't add up
    let bad: Vec<_> = docs.iter().filter(|d| matches!(d.reconciliation, Reconciliation::Discrepancy(_))).collect();
    if !bad.is_empty() {
        into.append(&heading("Didn't add up"));
        into.append(&dim("A line was probably misread — a page break or a smudge in the layout. Usually one click fixes it: on the Statements page, open the statement's Scan Report, find the skipped line and press Convert. Until then the rest of its transactions are in."));
        let l = list();
        for d in bad {
            let off = if let Reconciliation::Discrepancy(x) = d.reconciliation { fmt_money(x.abs(), d.currency) } else { String::new() };
            l.append(&row(&d.account_name, &format!("{} · off by {off}", d.filename)));
        }
        into.append(&l);
    }

    // months each account has, and the gaps
    let cov = app.db.coverage(&ids, 13, today()).unwrap_or_default();
    if !cov.is_empty() {
        into.append(&heading("Months covered"));
        let missing: usize = cov.iter().map(|c| c.months.iter().filter(|m| m.1 == MonthState::Missing).count()).sum();
        into.append(&dim(&if missing == 0 {
            "Every account has a statement for every month since its first one. ● read and adds up · ◐ doesn't add up · ○ missing".to_string()
        } else {
            format!("{missing} month{} missing. Download those statements into the folder and press Read Again. ● read and adds up · ◐ doesn't add up · ○ missing", if missing == 1 { "" } else { "s" })
        }));
        let l = list();
        for c in &cov {
            let marks: Vec<String> = c
                .months
                .iter()
                .filter(|m| m.1 != MonthState::Before)
                .map(|(m, st)| {
                    format!(
                        "{} {}",
                        match st {
                            MonthState::Verified => "●",
                            MonthState::Discrepancy => "◐",
                            MonthState::Missing => "○",
                            _ => "·",
                        },
                        m.format("%b")
                    )
                })
                .collect();
            let r = row(&c.account, &marks.join("  "));
            if c.months.iter().any(|m| m.1 == MonthState::Missing) {
                r.add_suffix(&pill("Gaps", "bad"));
            }
            l.append(&r);
        }
        into.append(&l);
    }
    let again = gtk::Button::builder().label("Read Again").halign(gtk::Align::Start).css_classes(["pill"]).tooltip_text("Reads only the files that are new since last time").build();
    let g2 = Rc::downgrade(g);
    again.connect_clicked(move |_| {
        if let Some(g) = g2.upgrade() {
            show(&g, g.step.get());
        }
    });
    into.append(&again);
}

// ──────────────────────────────────────────────────────────────── 6. sort

fn sort(g: &G, app: &Rc<App>) {
    title(g, "Sort Your Transactions", "");
    let ids = app.entity_ids();
    let rows = app.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
    if rows.is_empty() {
        g.body.append(&dim("Read your statements first (step 5)."));
        next_to(g, "Continue", None);
        return;
    }
    let transfers = rows.iter().filter(|t| t.is_transfer()).count();
    let sorted = rows.iter().filter(|t| t.category_id.is_some()).count();
    let waiting = rows.len() - sorted;
    let l = list();
    l.append(&icon_row(
        "emblem-ok-symbolic",
        &format!("{sorted} of {} sorted already", rows.len()),
        "Recognised from FinGraph's directory of Cayman shops, services and online stores, and from anything you've taught it before.",
    ));
    l.append(&icon_row(
        "media-playlist-repeat-symbolic",
        &format!("{transfers} moves between your own accounts"),
        "Card payments, savings transfers and the like. They're matched in pairs and never counted as income or spending.",
    ));
    l.append(&icon_row(if waiting == 0 { "emblem-ok-symbolic" } else { "mail-unread-symbolic" }, &format!("{waiting} waiting for you"), "Grouped by shop or payee, so you answer each one once — the biggest amounts first."));
    g.body.append(&l);

    // one card at a time, the biggest money first; skipped ones come back next time
    let mut groups = app.db.triage(&ids).unwrap_or_default();
    let skipped = g.skipped.borrow().clone();
    groups.retain(|x| !skipped.contains(&x.stem));
    groups.sort_by_key(|x| std::cmp::Reverse(x.totals.values().map(|v| v.abs()).sum::<Decimal>()));
    next_to(g, "Continue", None);
    if groups.is_empty() {
        let st = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .title(if waiting == 0 { "Everything is sorted" } else { "That's all for now" })
            .description(if waiting == 0 {
                "Every transaction has a category. New statements are sorted the same way, and anything new waits in To Review."
            } else {
                "You skipped the rest. They wait in To Review (Budget shows a Review button while any are left) whenever you're ready."
            })
            .build();
        st.add_css_class("compact");
        g.body.append(&st);
        return;
    }
    let first = &groups[0];
    let hdr = gtk::Box::builder().spacing(8).build();
    hdr.append(&gtk::Label::builder().label(format!("{} to go · answered {} so far", groups.len(), g.answered.get())).xalign(0.0).hexpand(true).css_classes(["heading"]).build());
    let skip = gtk::Button::builder().label("Skip This One").css_classes(["flat"]).build();
    hdr.append(&skip);
    g.body.append(&hdr);
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).css_classes(["card", "guide-card"]).build();
    g.body.append(&card);
    let next_card: Rc<dyn Fn()> = {
        let g2 = Rc::downgrade(g);
        Rc::new(move || {
            if let Some(g) = g2.upgrade() {
                show(&g, g.step.get());
            }
        })
    };
    crate::wizard::card(app, &card, first, next_card.clone(), g.answered.clone());
    {
        let (g2, stem) = (Rc::downgrade(g), first.stem.clone());
        skip.connect_clicked(move |_| {
            if let Some(g) = g2.upgrade() {
                g.skipped.borrow_mut().insert(stem.clone());
                show(&g, g.step.get());
            }
        });
    }
    g.body.append(&dim("Every answer is remembered: the same shop on future statements is sorted without asking, and the answer is applied to the past too. You don't have to finish — stop whenever you like and the rest wait in To Review."));
    g.note.set_label("Continue whenever you like");
}

// ──────────────────────────────────────────────────────────────── 7. budget

fn budget(g: &G, app: &Rc<App>) {
    title(
        g,
        "Prepare Your Budget",
        "A budget here is a monthly amount for each kind of spending. FinGraph starts each one at what you usually spend — the middle of your last six months, so a one-off doesn't count — and you adjust from there.",
    );
    let s = crate::budget::snap(app);
    let ccy = s.ccy;
    let money = |d: Decimal| fmt_money(d, ccy);
    let m = month_start(today());
    let months = data_span(&s.rows).map_or(0, |(a, b)| ((b - a).num_days() / 30) as i32);
    if months < 3 {
        let l = list();
        l.append(&icon_row("dialog-warning-symbolic", "A few more months would help", &format!("Your statements cover about {months} month{}. Suggestions from fewer than three are rough — add earlier statements to the folder (step 4) for better ones.", if months == 1 { "" } else { "s" })));
        g.body.append(&l);
    }
    let (mut usual, _, _) = crate::budget::usual(&s, m);
    // not a kind of spending: it's what still needs sorting
    let unsorted = usual.remove("Uncategorized");
    if !usual.is_empty() {
        let mut v: Vec<(String, Decimal)> = usual.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        let total: Decimal = v.iter().map(|x| x.1).sum();
        g.body.append(&heading(&format!("What you usually spend · about {} a month", money(total))));
        let l = list();
        for (name, amt) in v.iter().take(8) {
            let r = row(name, "");
            r.add_suffix(&gtk::Label::builder().label(money(*amt)).css_classes(["tnum"]).build());
            l.append(&r);
        }
        if v.len() > 8 {
            l.append(&row(&format!("and {} more", v.len() - 8), ""));
        }
        g.body.append(&l);
    }
    if let Some(u) = unsorted {
        let l = list();
        l.append(&icon_row("mail-unread-symbolic", &format!("About {} a month isn't sorted yet", money(u)), "It can't be part of a budget until it has a category. Sorting it (step 6) moves it into the right one — you can do that before or after the budget."));
        g.body.append(&l);
    }
    if let Some(p) = fingraph_core::budget::detect_salary(&s.rows, crate::budget::data_through(app)) {
        let l = list();
        l.append(&icon_row("starred-symbolic", "Your pay", &format!("{} {} from {} — the setup starts from this", fmt_money(p.amount, p.currency), p.cycle.label().to_lowercase(), p.payee)));
        g.body.append(&l);
    }
    // the big bills a normal month never shows: what's planned, else what the statements suggest
    let ideas = app.db.big_bill_ideas(&s.rows, ccy, &s.fx, today());
    if !s.funds.is_empty() || !ideas.is_empty() {
        let l = list();
        let (title, sub) = if s.funds.is_empty() {
            (
                format!("{} big bill{} found in your statements", ideas.len(), if ideas.len() == 1 { "" } else { "s" }),
                format!("{} — step 3 of the setup starts from these.", ideas.iter().take(4).map(|i| i.payee.clone()).collect::<Vec<_>>().join(", ")),
            )
        } else {
            let a_month: Decimal = s.funds.iter().map(|f| s.fx.convert(f.monthly(), f.currency, ccy)).sum();
            (format!("{} big yearly bill{} planned", s.funds.len(), if s.funds.len() == 1 { "" } else { "s" }), format!("Setting aside {} a month for them.", money(a_month)))
        };
        l.append(&icon_row("x-office-calendar-symbolic", &title, &sub));
        g.body.append(&l);
    }
    g.body.append(&heading("The setup takes five short steps"));
    let l = list();
    l.append(&numbered(1, "Your pay", "How often you're paid, the gross amount, and what comes off it (pension, health insurance)."));
    l.append(&numbered(2, "Other money in and out", "Rent you receive, side work, and loan or car payments."));
    l.append(&numbered(
        3,
        "Big yearly bills",
        "Car and home insurance, HOA fees, licences, school terms: what each costs and the month it's due. FinGraph sets a share aside every month, so the expensive months are already paid for.",
    ));
    l.append(&numbered(4, "Your spending", "Each category, starting from what you usually spend; change any of them."));
    l.append(&numbered(5, "Your savings goal", "An amount or a percentage. If the numbers don't reach it, FinGraph works out how much to trim from the flexible categories."));
    g.body.append(&l);
    let has = !s.budgets.is_empty();
    let go = gtk::Button::builder().label(if has { "Change My Budget…" } else { "Set Up My Budget…" }).halign(gtk::Align::Start).css_classes(["suggested-action", "pill"]).build();
    {
        let (w, g2) = (Rc::downgrade(app), Rc::downgrade(g));
        go.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let g3 = g2.clone();
            crate::budget::wizard(
                &a,
                Rc::new(move || {
                    if let Some(g) = g3.upgrade() {
                        show(&g, g.step.get() + 1);
                    }
                }),
            );
        });
    }
    g.body.append(&go);
    g.body.append(&dim("Later, on the Budget page: let leftovers roll into next month, and move money to cover an overspend. Your big bills show on Summary before they're due."));
    next_to(g, if has { "Continue" } else { "Skip for Now" }, None);
}

// ──────────────────────────────────────────────────────────────── 8. every month

fn monthly(g: &G, app: &Rc<App>) {
    title(g, "Every Month From Now On", "That's the setup done. From here it's a short routine once a month, when the new statements are out — usually a few days into the month.");
    let f = folder(app).map(|p| display_path(&p)).unwrap_or_else(|| "your statements folder".into());
    let watching = app.db.prefs().watch_enabled;
    let l = list();
    l.append(&numbered(
        1,
        "Download last month's statements",
        &format!("Every account, as before, saved into {f}. {}", if watching { "FinGraph reads them the next time it opens." } else { "Then drop them on the Statements page." }),
    ));
    l.append(&numbered(
        2,
        "Run Monthly Close",
        "On the Budget page. It walks you through the month: are all the statements in, sort anything new, cover overspending, tick off the regular bills, update your goals, then close the month with a summary.",
    ));
    l.append(&numbered(3, "In between, look at Outlook under Reports", "Where each account's balance is probably heading until the next statement, what's safe to spend, and the bills still to come."));
    g.body.append(&l);
    g.body.append(&heading("When you have a moment"));
    let l = list();
    l.append(&icon_row("starred-symbolic", "Summary", "Your net worth and the bigger picture. Under the charts, add what you own and owe that no statement shows — a car, a house, a pension — and set goals: saving up, or paying a card off."));
    l.append(&icon_row("media-playlist-repeat-symbolic", "Subscriptions, under Reports", "Everything charging you on a schedule, with its yearly cost and a link to cancel the ones FinGraph knows."));
    // the 5th: a few days after most statements are issued
    l.append(&icon_row("x-office-calendar-symbolic", "A reminder", "Put a monthly reminder in your calendar for the 5th — “Download statements, Monthly Close”."));
    g.body.append(&l);
    g.next.set_label("Finish");
    let (w, g2) = (Rc::downgrade(app), Rc::downgrade(g));
    *g.on_next.borrow_mut() = Some(Rc::new(move || {
        let (Some(a), Some(g)) = (w.upgrade(), g2.upgrade()) else { return };
        a.saved(a.db.set_setting("onboarded", "1"));
        g.dlg.close();
        crate::statements::watch(&a);
        a.show_view("budget");
    }));
}
