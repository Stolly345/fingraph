//! FinGraph: a local-first ledger for Cayman bank statements. Drop PDFs in, the 5-pass scanner reads
//! and proves them, Smart Triage learns your merchants, and the dashboard shows where money goes.

mod account;
mod changes;
mod charts;
mod dashboard;
mod export;
mod owed;
mod preferences;
mod profiles;
mod review;
mod rules;
mod split;
mod statement;
mod statements;
mod transactions;
mod txedit;
mod wizard;

use adw::prelude::*;
use chrono::{Datelike, Local, NaiveDate};
use fingraph_core::db::{Category, Db, Entity, TxFilter};
use fingraph_core::insights::{self, Range};
use fingraph_core::money::fmt_money;
use fingraph_core::{Currency, Decimal};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

const APP_ID: &str = "dev.fingraph.FinGraph";

/// Ctrl+1 to Ctrl+7.
const MAIN_PAGES: [&str; 8] = ["overview", "review", "changes", "transactions", "subscriptions", "statements", "export", "statement"];

/// Which workspaces the window shows.
#[derive(Clone, PartialEq)]
enum Scope {
    One(i64),
    /// The "Consolidated Overview": these workspaces together.
    Consolidated(Vec<i64>),
}

/// A view's "re-read the ledger" callback.
pub type Refresh = Rc<dyn Fn()>;
/// Something to run against the app later (toast buttons, menu actions).
pub type AppFn = Rc<dyn Fn(&Rc<App>)>;

pub struct App {
    win: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    db: Db,
    stack: gtk::Stack,
    split: adw::NavigationSplitView,
    content_title: adw::WindowTitle,
    workspace_btn: gtk::MenuButton,
    range_dd: gtk::DropDown,
    range_items: RefCell<Vec<Range>>,
    custom_range: Cell<Option<(NaiveDate, NaiveDate)>>,
    side_list: gtk::ListBox,
    ccy_box: gtk::Box,
    /// Sidebar counts, by page name.
    badges: RefCell<HashMap<&'static str, gtk::Label>>,
    /// What the account rows were last built from; unchanged → left alone.
    accounts_key: RefCell<String>,
    /// True while the sidebar rebuilds itself: its selection changes aren't the user's.
    quiet: Cell<bool>,
    kyd_btn: gtk::ToggleButton,
    usd_btn: gtk::ToggleButton,
    /// The watched folder (Preferences → Import), while watching.
    pub watch: RefCell<Option<gio::FileMonitor>>,
    scope: RefCell<Scope>,
    entities: RefCell<Vec<Entity>>,
    display: Cell<Currency>,
    views: RefCell<Vec<(&'static str, Refresh)>>,
    /// Transactions view: show only these ids (from an insight card), until cleared.
    pub focus_ids: RefCell<Option<Vec<i64>>>,
}

pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl App {
    fn new(gapp: &adw::Application) -> Rc<App> {
        let db_path = std::env::var_os("FINGRAPH_DB").map(std::path::PathBuf::from).unwrap_or_else(|| fingraph_core::data_dir().join("ledger.db"));
        let db = match Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("fingraph: {e}");
                std::process::exit(1);
            }
        };
        let win = adw::ApplicationWindow::builder().application(gapp).title("FinGraph").default_width(1360).default_height(900).build();
        let toasts = adw::ToastOverlay::new();
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).build();
        let content_title = adw::WindowTitle::new("Dashboard & Insights", "");

        // ── header: workspace, currency, range, triage
        // the workspace decides everything below it, so it heads the sidebar
        let workspace_btn = gtk::MenuButton::builder().tooltip_text("Workspace").always_show_arrow(true).hexpand(true).margin_start(8).margin_end(8).margin_bottom(6).build();
        let kyd_btn = gtk::ToggleButton::builder().label("KYD").active(true).tooltip_text("Show amounts in Cayman dollars").build();
        let usd_btn = gtk::ToggleButton::builder().label("USD").group(&kyd_btn).tooltip_text("Show amounts in US dollars").build();
        let ccy_box = gtk::Box::builder().css_classes(["linked"]).build();
        ccy_box.append(&kyd_btn);
        ccy_box.append(&usd_btn);
        let range_dd = gtk::DropDown::builder().tooltip_text("Date range").build();
        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Import Statements…"), Some("win.import"));
        s1.append(Some("Load Demo Data (Cayman Personal + Business across Butterfield, Scotiabank, CNB, RBC & Credit Union)"), Some("win.demo"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Preferences"), Some("win.preferences"));
        s2.append(Some("Welcome Tour"), Some("win.welcome"));
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("About FinGraph"), Some("win.about"));
        menu.append_section(None, &s3);
        let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).tooltip_text("Main Menu").build();

        let content_header = adw::HeaderBar::builder().title_widget(&content_title).build();
        content_header.pack_start(&ccy_box);
        content_header.pack_start(&range_dd);
        content_header.pack_end(&menu_btn);
        let content_tv = adw::ToolbarView::new();
        content_tv.add_top_bar(&content_header);
        content_tv.set_content(Some(&stack));
        let content = adw::NavigationPage::builder().title("FinGraph").child(&content_tv).build();

        // ── sidebar
        let side_list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
        side_list.set_header_func(|row, before| {
            let name = row.widget_name();
            let prev = before.map(|b| b.widget_name().to_string()).unwrap_or_default();
            let title = if name.starts_with("account:") && !prev.starts_with("account:") {
                Some("Accounts")
            } else if name == "rules" {
                Some("Setup")
            } else {
                None
            };
            row.set_header(title.map(|t| gtk::Label::builder().label(t).xalign(0.0).margin_start(12).margin_top(14).margin_bottom(4).css_classes(["caption-heading", "dim-label"]).build()).as_ref());
        });
        let side_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        side_box.append(&workspace_btn);
        side_box.append(&gtk::ScrolledWindow::builder().child(&side_list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build());
        let side_tv = adw::ToolbarView::new();
        side_tv.add_top_bar(&adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("FinGraph", "")).build());
        side_tv.set_content(Some(&side_box));
        let sidebar = adw::NavigationPage::builder().title("FinGraph").child(&side_tv).build();
        let split = adw::NavigationSplitView::builder().sidebar(&sidebar).content(&content).min_sidebar_width(240.0).max_sidebar_width(290.0).build();
        toasts.set_child(Some(&split));
        win.set_content(Some(&toasts));
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 900sp").unwrap());
        bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        win.add_breakpoint(bp);

        let app = Rc::new(App {
            win,
            toasts,
            db,
            stack,
            split,
            content_title,
            workspace_btn,
            range_dd,
            range_items: RefCell::new(Vec::new()),
            custom_range: Cell::new(None),
            side_list: side_list.clone(),
            ccy_box: ccy_box.clone(),
            badges: RefCell::new(HashMap::new()),
            accounts_key: RefCell::new(String::new()),
            quiet: Cell::new(false),
            kyd_btn,
            usd_btn: usd_btn.clone(),
            watch: RefCell::new(None),
            scope: RefCell::new(Scope::Consolidated(Vec::new())),
            entities: RefCell::new(Vec::new()),
            display: Cell::new(Currency::KYD),
            views: RefCell::new(Vec::new()),
            focus_ids: RefCell::new(None),
        });
        app.display.set(match app.db.setting("display_currency").as_deref() {
            Some("USD") => Currency::USD,
            _ => Currency::KYD,
        });
        if app.display.get() == Currency::USD {
            usd_btn.set_active(true);
        }

        // pages, in the order you use them; Accounts and Setup follow (see update_sidebar)
        let (o, or) = dashboard::build(&app);
        let (rv, rvr) = review::build(&app);
        let (c, cr) = changes::build(&app);
        let (t, tr) = transactions::build(&app);
        let (cs, csr) = statement::build(&app);
        let (sb, sbr) = rules::build(&app, rules::Part::Subscriptions);
        let (st, str_) = statements::build(&app);
        let (e, er) = export::build(&app);
        let (r, rr) = rules::build(&app, rules::Part::Rules);
        let (p, pr) = profiles::build(&app);
        let (ac, acr) = account::build(&app);
        // an account's overview is reached from its row under Accounts, not listed itself
        app.stack.add_titled(&ac, Some("account"), "Account");
        app.views.borrow_mut().push(("account", acr));
        let pages: Vec<(&'static str, &str, &str, gtk::Widget, Refresh)> = vec![
            ("overview", "Overview", "view-grid-symbolic", o, or),
            ("review", "To Review", "mail-unread-symbolic", rv, rvr),
            ("changes", "What Changed", "dialog-information-symbolic", c, cr),
            ("transactions", "Transactions", "view-list-symbolic", t, tr),
            ("statement", "Combined Statement", "view-paged-symbolic", cs, csr),
            ("subscriptions", "Subscriptions", "media-playlist-repeat-symbolic", sb, sbr),
            ("statements", "Statements", "x-office-document-symbolic", st, str_),
            ("export", "Reports & Export", "document-save-symbolic", e, er),
            ("rules", "Rules & Categories", "edit-find-replace-symbolic", r, rr),
            ("profiles", "Bank Profiles", "preferences-system-symbolic", p, pr),
        ];
        for (name, title, icon, w, refresh) in pages {
            app.stack.add_titled(&w, Some(name), title);
            side_list.append(&app.side_row(name, title, icon));
            app.views.borrow_mut().push((name, refresh));
        }
        side_list.append(&app.side_row("preferences", "Preferences", "emblem-system-symbolic"));
        {
            let w = Rc::downgrade(&app);
            side_list.connect_row_selected(move |list, row| {
                let (Some(a), Some(row)) = (w.upgrade(), row) else { return };
                if a.quiet.get() {
                    return;
                }
                let name = row.widget_name().to_string();
                if let Some(id) = name.strip_prefix("account:").and_then(|s| s.parse::<i64>().ok()) {
                    return account::show(&a, id);
                }
                if name == "preferences" {
                    // a dialog, not a page: put the highlight back where you were
                    let here = a.stack.visible_child_name().unwrap_or_default();
                    if let Some(r) = a.row_named(&here) {
                        list.select_row(Some(&r));
                    }
                    return preferences::show(&a);
                }
                if a.stack.visible_child_name().as_deref() != Some(name.as_str()) {
                    a.show_view(&name);
                }
            });
        }
        {
            let w = Rc::downgrade(&app);
            app.kyd_btn.connect_toggled(move |b| {
                if let Some(a) = w.upgrade() {
                    a.display.set(if b.is_active() { Currency::KYD } else { Currency::USD });
                    let _ = a.db.set_setting("display_currency", a.display.get().code());
                    a.refresh();
                }
            });
        }
        {
            let w = Rc::downgrade(&app);
            app.range_dd.connect_selected_notify(move |dd| {
                let Some(a) = w.upgrade() else { return };
                let items = a.range_items.borrow().clone();
                match items.get(dd.selected() as usize) {
                    Some(Range::Custom(..)) => a.ask_custom_range(),
                    Some(_) => a.refresh(),
                    None => {}
                }
            });
        }
        app.actions();
        // where to start: Preferences → General
        let prefs = app.db.prefs();
        preferences::apply_appearance(&prefs.appearance);
        let start = if prefs.start_workspace == "last" { prefs.last_workspace.clone() } else { prefs.start_workspace.clone() };
        if let Ok(id) = start.parse::<i64>() {
            *app.scope.borrow_mut() = Scope::One(id);
        }
        app.reload_entities();
        let last = app.db.prefs().last_page;
        app.show_view(if app.stack.child_by_name(&last).is_some() { &last } else { "overview" });
        app.win.present();
        statements::watch(&app);

        let shot = cfg!(debug_assertions) && std::env::var_os("FINGRAPH_SHOT").is_some();
        if !shot && app.db.setting("onboarded").is_none() {
            wizard::onboarding(&app);
        }
        #[cfg(debug_assertions)]
        debug_shot(&app);
        app
    }

    fn actions(self: &Rc<Self>) {
        let add = |name: &str, f: AppFn| {
            let act = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            act.connect_activate(move |_, _| {
                if let Some(a) = w.upgrade() {
                    f(&a);
                }
            });
            self.win.add_action(&act);
        };
        add("import", Rc::new(statements::choose_files));
        add(
            "demo",
            Rc::new(|a| match a.db.load_demo() {
                Ok(n) => {
                    let _ = a.db.set_setting("onboarded", "1");
                    a.reload_entities();
                    a.toast_action(&format!("Loaded {n} demo transactions from 84 simulated statements"), "Start Triage", Rc::new(wizard::triage));
                }
                Err(e) => a.toast(&e.to_string()),
            }),
        );
        add("workspaces", Rc::new(wizard::workspaces_dialog));
        add("rescan", Rc::new(statements::rescan_all));
        add("preferences", Rc::new(preferences::show));
        for (i, page) in MAIN_PAGES.iter().enumerate() {
            add(&format!("page-{}", i + 1), Rc::new(move |a: &Rc<App>| a.show_view(page)));
        }
        add("retriage", Rc::new(wizard::reset_and_retriage));
        add("fx", Rc::new(|a| a.fx_dialog()));
        add("welcome", Rc::new(wizard::onboarding));
        add(
            "about",
            Rc::new(|a| {
                let d = adw::AboutDialog::builder()
                    .application_name("FinGraph")
                    .application_icon(APP_ID)
                    .version(env!("CARGO_PKG_VERSION"))
                    .comments("Local-first ledger and 5-pass statement scanner for Cayman bank statements. Nothing leaves this computer.")
                    .license_type(gtk::License::MitX11)
                    .build();
                d.present(Some(&a.win));
            }),
        );
        add(
            "find",
            Rc::new(|a| {
                a.show_view("transactions");
                transactions::focus_search(a);
            }),
        );
        if let Some(g) = self.win.application() {
            g.set_accels_for_action("win.find", &["<Control>f"]);
            g.set_accels_for_action("win.import", &["<Control>o"]);
            g.set_accels_for_action("win.preferences", &["<Control>comma"]);
            for i in 0..MAIN_PAGES.len() {
                g.set_accels_for_action(&format!("win.page-{}", i + 1), &[&format!("<Control>{}", i + 1)]);
            }
        }
    }

    // ── selection state

    pub fn reload_entities(self: &Rc<Self>) {
        let ents = self.db.entities().unwrap_or_default();
        let scope = self.scope.borrow().clone();
        let valid = match &scope {
            Scope::One(id) => ents.iter().any(|e| e.id == *id),
            Scope::Consolidated(_) => true,
        };
        if !valid || (matches!(scope, Scope::Consolidated(ref v) if v.is_empty()) && ents.len() == 1) {
            *self.scope.borrow_mut() = match ents.first() {
                Some(e) if ents.len() == 1 => Scope::One(e.id),
                _ => Scope::Consolidated(ents.iter().map(|e| e.id).collect()),
            };
        }
        if let Scope::Consolidated(v) = &mut *self.scope.borrow_mut() {
            v.retain(|id| ents.iter().any(|e| e.id == *id));
            if v.is_empty() {
                v.extend(ents.iter().map(|e| e.id));
            }
        }
        *self.entities.borrow_mut() = ents;
        self.rebuild_workspace_menu();
        self.rebuild_ranges();
        self.refresh();
    }

    fn rebuild_workspace_menu(self: &Rc<Self>) {
        let ents = self.entities.borrow().clone();
        let scope = self.scope.borrow().clone();
        // remembered for "open where I left off"
        let mut p = self.db.prefs();
        p.last_workspace = match &scope {
            Scope::One(id) => id.to_string(),
            Scope::Consolidated(_) => "consolidated".into(),
        };
        let _ = self.db.save_prefs(&p);
        let label = match &scope {
            Scope::One(id) => ents.iter().find(|e| e.id == *id).map_or("Workspace".into(), |e| e.name.clone()),
            Scope::Consolidated(v) if v.len() == ents.len() => "Consolidated Overview".into(),
            Scope::Consolidated(v) => format!("Consolidated ({})", v.len()),
        };
        self.workspace_btn.set_label(&label);
        let pop = gtk::Popover::new();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
        let mut group: Option<gtk::CheckButton> = None;
        for e in &ents {
            let cb = gtk::CheckButton::builder().label(&e.name).active(scope == Scope::One(e.id)).build();
            cb.set_group(group.as_ref());
            group.get_or_insert(cb.clone());
            let (w, id, p) = (Rc::downgrade(self), e.id, pop.clone());
            cb.connect_toggled(move |cb| {
                if let (true, Some(a)) = (cb.is_active(), w.upgrade()) {
                    *a.scope.borrow_mut() = Scope::One(id);
                    p.popdown();
                    a.rebuild_workspace_menu();
                    a.refresh();
                }
            });
            col.append(&cb);
        }
        col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let cons = gtk::CheckButton::builder().label("Consolidated Overview").active(matches!(scope, Scope::Consolidated(_))).build();
        cons.set_group(group.as_ref());
        col.append(&cons);
        let inc = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_start(24).sensitive(matches!(scope, Scope::Consolidated(_))).build();
        for e in &ents {
            let on = matches!(&scope, Scope::Consolidated(v) if v.contains(&e.id));
            let cb = gtk::CheckButton::builder().label(&e.name).active(on).build();
            let (w, id) = (Rc::downgrade(self), e.id);
            cb.connect_toggled(move |cb| {
                let Some(a) = w.upgrade() else { return };
                if let Scope::Consolidated(v) = &mut *a.scope.borrow_mut() {
                    v.retain(|x| *x != id);
                    if cb.is_active() {
                        v.push(id);
                    }
                }
                a.refresh();
            });
            inc.append(&cb);
        }
        col.append(&inc);
        {
            let (w, inc) = (Rc::downgrade(self), inc.clone());
            cons.connect_toggled(move |cb| {
                inc.set_sensitive(cb.is_active());
                if let (true, Some(a)) = (cb.is_active(), w.upgrade()) {
                    let all: Vec<i64> = a.entities.borrow().iter().map(|e| e.id).collect();
                    *a.scope.borrow_mut() = Scope::Consolidated(all);
                    a.rebuild_workspace_menu();
                    a.refresh();
                }
            });
        }
        if ents.is_empty() {
            col.append(&gtk::Label::new(Some("No workspaces yet — import a statement or load the demo")));
        }
        pop.set_child(Some(&col));
        self.workspace_btn.set_popover(Some(&pop));
    }

    fn rebuild_ranges(self: &Rc<Self>) {
        let span = insights::data_span(&self.db.transactions(&TxFilter::default()).unwrap_or_default());
        let this_year = today().year();
        let mut years: Vec<i32> = span.map(|(a, b)| (a.year()..=b.year()).rev().collect()).unwrap_or_default();
        if years.is_empty() {
            years = vec![this_year, this_year - 1];
        }
        let mut items = vec![Range::ThisMonth, Range::LastMonth, Range::Last90, Range::Ytd];
        items.extend(years.into_iter().map(Range::Year));
        items.push(Range::All);
        items.push(Range::Custom(today(), today()));
        let labels: Vec<String> = items.iter().map(|r| if matches!(r, Range::Custom(..)) { "Custom…".into() } else { r.label() }).collect();
        let prev = self.range_items.borrow().get(self.range_dd.selected() as usize).copied();
        *self.range_items.borrow_mut() = items.clone();
        let model = gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        // first time: Preferences → General → Date range
        let prev = prev.or_else(|| {
            Some(match self.db.prefs().default_range.as_str() {
                "This Month" => Range::ThisMonth,
                "Last Month" => Range::LastMonth,
                "Last 90 Days" => Range::Last90,
                "Year to Date" => Range::Ytd,
                "All Time" => Range::All,
                _ => Range::Year(this_year),
            })
        });
        let pick = prev.and_then(|p| items.iter().position(|x| std::mem::discriminant(x) == std::mem::discriminant(&p) && (!matches!(p, Range::Year(_)) || *x == p))).unwrap_or(items.iter().position(|r| *r == Range::Year(this_year)).unwrap_or(3));
        self.range_dd.set_model(Some(&model));
        self.range_dd.set_selected(pick as u32);
    }

    fn ask_custom_range(self: &Rc<Self>) {
        let (f0, t0) = self.custom_range.get().unwrap_or_else(|| (insights::month_start(today()), today()));
        let from = adw::EntryRow::builder().title("From (YYYY-MM-DD)").text(f0.to_string()).build();
        let to = adw::EntryRow::builder().title("To (YYYY-MM-DD)").text(t0.to_string()).build();
        let group = adw::PreferencesGroup::new();
        group.add(&from);
        group.add(&to);
        let d = adw::AlertDialog::new(Some("Custom Date Range"), None);
        d.set_extra_child(Some(&group));
        d.add_responses(&[("cancel", "Cancel"), ("ok", "Apply")]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let w = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(a) = w.upgrade() else { return };
            let p = |e: &adw::EntryRow| NaiveDate::parse_from_str(e.text().trim(), "%Y-%m-%d").ok();
            if r == "ok"
                && let (Some(f), Some(t)) = (p(&from), p(&to))
            {
                a.custom_range.set(Some((f.min(t), f.max(t))));
            }
            a.refresh();
        });
        d.present(Some(&self.win));
    }

    pub fn entity_ids(&self) -> Vec<i64> {
        match &*self.scope.borrow() {
            Scope::One(id) => vec![*id],
            Scope::Consolidated(v) => v.clone(),
        }
    }

    /// The workspace a new statement goes into: the selected one, else the first.
    pub fn import_entity(&self) -> Option<i64> {
        match &*self.scope.borrow() {
            Scope::One(id) => Some(*id),
            Scope::Consolidated(v) => v.first().copied().or_else(|| self.entities.borrow().first().map(|e| e.id)),
        }
    }

    pub fn set_display(&self, c: Currency) {
        match c {
            Currency::KYD => self.kyd_btn.set_active(true),
            Currency::USD => self.usd_btn.set_active(true),
        }
    }

    pub fn display(&self) -> Currency {
        self.display.get()
    }

    pub fn range(&self) -> (NaiveDate, NaiveDate) {
        let r = self.range_items.borrow().get(self.range_dd.selected() as usize).copied().unwrap_or(Range::All);
        match r {
            Range::Custom(..) => self.custom_range.get().unwrap_or_else(|| (insights::month_start(today()), today())),
            Range::All => {
                let rows = self.db.transactions(&TxFilter { entity_ids: self.entity_ids(), ..Default::default() }).unwrap_or_default();
                r.resolve(today(), insights::data_span(&rows))
            }
            r => r.resolve(today(), None),
        }
    }

    pub fn range_label(&self) -> String {
        let (a, b) = self.range();
        format!("{} – {}", a.format("%d %b %Y"), b.format("%d %b %Y"))
    }

    pub fn money(&self, d: Decimal) -> String {
        fmt_money(d, self.display())
    }

    pub fn categories(&self) -> Vec<Category> {
        self.db.categories().unwrap_or_default()
    }

    /// Sub-category id → its parent's name, for rolling spending up to top-level categories.
    pub fn parents(&self) -> BTreeMap<i64, String> {
        let cats = self.categories();
        cats.iter().filter_map(|c| Some((c.id, cats.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect()
    }

    // ── navigation & refresh

    pub fn show_view(self: &Rc<Self>, name: &str) {
        self.stack.set_visible_child_name(name);
        if let Some(c) = self.stack.child_by_name(name) {
            self.content_title.set_title(&self.stack.page(&c).title().unwrap_or_default());
        }
        // highlight it in the sidebar (an account row stays highlighted over its transactions)
        let on_account = self.side_list.selected_row().is_some_and(|r| r.widget_name().starts_with("account:"));
        if !(on_account && matches!(name, "transactions" | "account"))
            && let Some(r) = self.row_named(name)
            && self.side_list.selected_row().as_ref() != Some(&r)
        {
            self.side_list.select_row(Some(&r));
        }
        // the header shows only what this page uses
        self.range_dd.set_visible(matches!(name, "overview" | "changes" | "transactions" | "statement" | "export" | "account"));
        self.ccy_box.set_visible(matches!(name, "overview" | "changes" | "transactions" | "statement" | "subscriptions" | "export"));
        if MAIN_PAGES.contains(&name) {
            let mut p = self.db.prefs();
            if p.last_page != name {
                p.last_page = name.into();
                let _ = self.db.save_prefs(&p);
            }
        }
        self.split.set_show_content(true);
        self.refresh();
    }

    fn side_row(&self, name: &str, title: &str, icon: &str) -> gtk::ListBoxRow {
        let row = gtk::Box::builder().spacing(12).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
        row.append(&gtk::Image::from_icon_name(icon));
        row.append(&gtk::Label::builder().label(title).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
        let badge = gtk::Label::builder().css_classes(["dim-label", "caption", "tnum"]).build();
        row.append(&badge);
        if let Some(n) = MAIN_PAGES.iter().position(|p| *p == name) {
            row.set_tooltip_text(Some(&format!("Ctrl+{}", n + 1)));
        }
        if let Some(&key) = ["review", "changes", "subscriptions", "statements"].iter().find(|k| **k == name) {
            self.badges.borrow_mut().insert(key, badge);
        }
        gtk::ListBoxRow::builder().child(&row).name(name).build()
    }

    fn row_named(&self, name: &str) -> Option<gtk::ListBoxRow> {
        let mut i = 0;
        while let Some(r) = self.side_list.row_at_index(i) {
            if r.widget_name() == name {
                return Some(r);
            }
            i += 1;
        }
        None
    }

    /// Counts beside the pages, and one row per account with its latest verified balance.
    pub fn update_sidebar(self: &Rc<Self>) {
        let ids = self.entity_ids();
        let rows = self.db.transactions(&TxFilter { entity_ids: ids.clone(), split_lines: true, ..Default::default() }).unwrap_or_default();
        let docs = self.db.documents(&ids).unwrap_or_default();
        let fx = self.db.fx().unwrap_or_default();
        let set = |k: &str, text: String, warn: bool| {
            if let Some(l) = self.badges.borrow().get(k) {
                l.set_label(&text);
                if warn {
                    l.add_css_class("error");
                } else {
                    l.remove_css_class("error");
                }
            }
        };
        let review = self.db.triage(&ids).map(|g| g.len()).unwrap_or(0);
        set("review", if review > 0 { review.to_string() } else { String::new() }, false);
        let changes = changes::count(self);
        set("changes", if changes > 0 { changes.to_string() } else { String::new() }, false);
        let subs: Decimal = insights::subscriptions(&rows, self.display(), &fx, today()).iter().filter(|s| s.active).map(|s| s.monthly).sum();
        set("subscriptions", if subs.is_zero() { String::new() } else { format!("{}/mo", fmt_money(subs.round_dp(0), self.display())) }, false);
        // a statement that doesn't balance, a month with no statement, or a break between statements
        let cov = self.db.coverage(&ids, 12, today()).unwrap_or_default();
        let missing = cov.iter().map(|c| c.months.iter().filter(|m| m.1 == fingraph_core::workflows::MonthState::Missing).count() + c.breaks.len()).sum::<usize>();
        let bad = docs.iter().filter(|d| matches!(d.reconciliation, fingraph_core::Reconciliation::Discrepancy(_))).count() + missing;
        set("statements", if bad > 0 { format!("⚠ {bad}") } else { String::new() }, bad > 0);

        // accounts: rebuilt in place, just above Setup, only when they or their balances changed
        let balances = insights::latest_balances(&rows, &docs);
        let accounts: Vec<_> = self.db.accounts().unwrap_or_default().into_iter().filter(|x| ids.contains(&x.entity_id) && !x.hidden).collect();
        let key = format!("{:?}|{}", accounts.iter().map(|x| (x.id, &x.name, balances.get(&x.id))).collect::<Vec<_>>(), fingraph_core::money::fmt_money(Decimal::ONE, self.display()));
        if *self.accounts_key.borrow() == key {
            return;
        }
        *self.accounts_key.borrow_mut() = key;
        self.quiet.set(true);
        let selected = self.side_list.selected_row().map(|r| r.widget_name().to_string());
        let mut i = 0;
        while let Some(r) = self.side_list.row_at_index(i) {
            if r.widget_name().starts_with("account:") {
                self.side_list.remove(&r);
            } else {
                i += 1;
            }
        }
        let at = self.row_named("rules").map_or(-1, |r| r.index());
        for (k, acct) in accounts.iter().enumerate() {
            let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(4).margin_bottom(4).margin_start(6).margin_end(6).build();
            b.append(&gtk::Label::builder().label(&acct.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
            let (bal, tip) = match balances.get(&acct.id) {
                Some((d, v)) => (fmt_money(*v, acct.currency), format!("Last verified balance, as of {} (from your statements, not live)", d.format("%d %b %Y"))),
                None => ("—".into(), "No balance printed yet".into()),
            };
            let owed = balances.get(&acct.id).is_some_and(|(_, v)| v.is_sign_negative() && !v.is_zero());
            b.append(&gtk::Label::builder().label(&bal).xalign(1.0).css_classes(["caption", "tnum", if owed { "amount-out" } else { "dim-label" }]).build());
            let row = gtk::ListBoxRow::builder().child(&b).name(format!("account:{}", acct.id)).tooltip_text(tip).build();
            self.side_list.insert(&row, if at < 0 { -1 } else { at + k as i32 });
            if selected.as_deref() == Some(row.widget_name().as_str()) {
                self.side_list.select_row(Some(&row));
            }
        }
        self.side_list.invalidate_headers();
        self.quiet.set(false);
    }

    /// Re-read the ledger into the visible view and the header.
    pub fn refresh(self: &Rc<Self>) {
        self.update_sidebar();
        self.content_title.set_subtitle(&if self.range_dd.is_visible() { self.range_label() } else { String::new() });
        let visible = self.stack.visible_child_name().unwrap_or_default();
        let f = self.views.borrow().iter().find(|(n, _)| *n == visible.as_str()).map(|v| v.1.clone());
        if let Some(f) = f {
            f();
        }
    }

    pub fn show_transactions(self: &Rc<Self>, ids: Vec<i64>) {
        *self.focus_ids.borrow_mut() = Some(ids);
        self.show_view("transactions");
    }

    pub fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(msg)).timeout(4).build());
    }

    /// Pick the business workspace an expense paid from `home`'s account belongs to: the only one
    /// there is, a choice when there are several, or a new one when there are none.
    pub fn with_business(self: &Rc<Self>, home: i64, then: impl Fn(&Rc<App>, i64) + 'static, cancelled: impl Fn(&Rc<App>) + 'static) {
        let ents = self.entities.borrow().clone();
        let biz: Vec<_> = ents.iter().filter(|e| e.id != home && e.kind != fingraph_core::EntityKind::Personal).cloned().collect();
        if biz.len() == 1 {
            return then(self, biz[0].id);
        }
        let group = adw::PreferencesGroup::new();
        let pick = adw::ComboRow::builder().title("Business").model(&gtk::StringList::new(&biz.iter().map(|e| e.name.as_str()).collect::<Vec<_>>())).build();
        let name = adw::EntryRow::builder().title("New business workspace name").text("Business").build();
        let (title, body) = if biz.is_empty() {
            group.add(&name);
            ("Create a Business Workspace", "Business expenses are booked to their own workspace, so its P&L and tax summary include them and your personal spending doesn't.")
        } else {
            group.add(&pick);
            ("Which Business?", "The expense moves to that workspace's P&L.")
        };
        let d = adw::AlertDialog::new(Some(title), Some(body));
        d.set_extra_child(Some(&group));
        d.add_responses(&[("cancel", "Cancel"), ("ok", "Book to Business")]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let w = Rc::downgrade(self);
        let then = Rc::new(then);
        d.connect_response(None, move |_, r| {
            let Some(a) = w.upgrade() else { return };
            if r != "ok" {
                return cancelled(&a);
            }
            let id = if biz.is_empty() {
                let n = name.text().trim().to_string();
                let base = a.entities.borrow().iter().find(|e| e.id == home).map_or(Currency::KYD, |e| e.base_currency);
                match a.db.entity_by_name(&n).ok().flatten().map(Ok).unwrap_or_else(|| a.db.create_entity(&n, fingraph_core::EntityKind::Business, base)) {
                    Ok(id) => {
                        a.reload_entities();
                        id
                    }
                    Err(e) => return a.toast(&e.to_string()),
                }
            } else {
                biz[pick.selected() as usize].id
            };
            then(&a, id);
        });
        d.present(Some(&self.win));
    }

    pub fn entity_name(&self, id: i64) -> String {
        self.entities.borrow().iter().find(|e| e.id == id).map_or_else(|| "?".into(), |e| e.name.clone())
    }

    pub fn toast_action(self: &Rc<Self>, msg: &str, label: &str, f: AppFn) {
        let t = adw::Toast::builder().title(glib::markup_escape_text(msg)).button_label(label).timeout(8).build();
        let w = Rc::downgrade(self);
        t.connect_button_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                f(&a);
            }
        });
        self.toasts.add_toast(t);
    }

    fn fx_dialog(self: &Rc<Self>) {
        let fx = self.db.fx().unwrap_or_default();
        let row = adw::EntryRow::builder().title("US dollars per 1 Cayman dollar").text(fx.kyd_to_usd.to_string()).build();
        let group = adw::PreferencesGroup::builder().description("The Cayman peg is 1.20. Statement FX lines (e.g. “USD 145.00 @ 0.84”) always win over this rate for the transactions they're printed on.").build();
        group.add(&row);
        let d = adw::AlertDialog::new(Some("Exchange Rate"), None);
        d.set_extra_child(Some(&group));
        d.add_responses(&[("cancel", "Cancel"), ("ok", "Save")]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let w = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(a) = w.upgrade() else { return };
            if r != "ok" {
                return;
            }
            match row.text().trim().parse::<Decimal>().map_err(|e| e.to_string()).and_then(|v| a.db.set_fx(v).map_err(|e| e.to_string())) {
                Ok(()) => {
                    a.toast("Exchange rate saved; amounts revalued");
                    a.refresh();
                }
                Err(e) => a.toast(&format!("Couldn't save the rate: {e}")),
            }
        });
        d.present(Some(&self.win));
    }
}

/// Small helpers shared by the views.
pub fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(["title-4"]).build()
}

pub fn dim(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["dim-label"]).build()
}

pub fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

pub fn pill(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["pill-badge", class]).valign(gtk::Align::Center).build()
}

fn css() {
    let p = gtk::CssProvider::new();
    p.load_from_string(
        ".pill-badge { border-radius: 99px; padding: 2px 10px; font-size: 0.85em; font-weight: 600; }
         .pill-badge.ok { background: alpha(@success_color, 0.18); color: @success_color; }
         .pill-badge.bad { background: alpha(@error_color, 0.18); color: @error_color; }
         .pill-badge.meh { background: alpha(@window_fg_color, 0.10); }
         .pill-badge.bank { background: alpha(@accent_bg_color, 0.18); color: @accent_color; }
         .kpi { padding: 14px 16px; }
         .kpi .value { font-size: 1.6em; font-weight: 800; }
         .chart-card { padding: 12px; }
         .amount-out { color: @error_color; font-feature-settings: 'tnum'; }
         .amount-in { color: @success_color; font-feature-settings: 'tnum'; }
         .tnum { font-feature-settings: 'tnum'; }
         .chip { border-radius: 99px; padding: 4px 12px; }
         .bank-card { padding: 12px; min-width: 150px; }",
    );
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

#[cfg(debug_assertions)]
fn debug_shot(app: &Rc<App>) {
    // FINGRAPH_SHOT=out.png FINGRAPH_VIEW=transactions FINGRAPH_DIALOG=triage|onboarding|report
    let Some(out) = std::env::var_os("FINGRAPH_SHOT") else { return };
    if let Some((w, h)) = std::env::var("FINGRAPH_SHOT_SIZE").ok().and_then(|s| Some((s.split_once('x')?.0.parse().ok()?, s.split_once('x')?.1.parse().ok()?))) {
        app.win.set_default_size(w, h);
    }
    if std::env::var_os("FINGRAPH_DEMO").is_some() && app.db.documents(&[]).is_ok_and(|d| d.is_empty()) {
        let _ = app.db.load_demo();
        app.reload_entities();
    }
    if let Ok(v) = std::env::var("FINGRAPH_VIEW") {
        match v.strip_prefix("account:").and_then(|id| app.row_named(&format!("account:{id}"))) {
            Some(r) => app.side_list.select_row(Some(&r)),
            None => app.show_view(&v),
        }
    }
    if let Ok(r) = std::env::var("FINGRAPH_RANGE") {
        let items = app.range_items.borrow().clone();
        if let Some(i) = items.iter().position(|x| x.label() == r) {
            app.range_dd.set_selected(i as u32);
        }
    }
    match std::env::var("FINGRAPH_DIALOG").as_deref() {
        Ok("triage") => wizard::triage(app),
        Ok("onboarding") => wizard::onboarding(app),
        Ok("report") => statements::open_first_report(app),
        Ok(d) if d.starts_with("split:") => split::dialog(app, d[6..].parse().unwrap_or(0)),
        Ok(d) if d.starts_with("owed:") => owed::debug_open(app, d[5..].parse().unwrap_or(0)),
        Ok(d) if d.starts_with("tx:") => txedit::dialog(app, d[3..].parse().unwrap_or(0), None),
        Ok("preferences") => {
            preferences::show(app);
            if let (Some(d), Ok(page)) = (app.win.visible_dialog().and_downcast::<adw::PreferencesDialog>(), std::env::var("FINGRAPH_PAGE")) {
                d.set_visible_page_name(&page);
            }
        }
        _ => {}
    }
    let a = app.clone();
    let wait = std::env::var("FINGRAPH_SHOT_WAIT").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    // FINGRAPH_SHOT_STEPS=n: press the open dialog's suggested button n times first (answers triage cards)
    let steps: u32 = std::env::var("FINGRAPH_SHOT_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let n = Rc::new(Cell::new(0u32));
    let scrolled = Cell::new(false);
    glib::timeout_add_local(std::time::Duration::from_secs(wait), move || {
        if n.get() < steps {
            n.set(n.get() + 1);
            fn find<T: IsA<gtk::Widget>>(w: &gtk::Widget, f: &dyn Fn(&gtk::Widget) -> Option<T>) -> Option<T> {
                if let Some(x) = f(w) {
                    return Some(x);
                }
                let mut c = w.first_child();
                while let Some(ch) = c {
                    if let Some(x) = find(&ch, f) {
                        return Some(x);
                    }
                    c = ch.next_sibling();
                }
                None
            }
            let root: gtk::Widget = a.win.clone().upcast();
            let dlg = find(&root, &|w| w.downcast_ref::<adw::Dialog>().cloned());
            let scope: gtk::Widget = dlg.map(|d| d.upcast()).unwrap_or_else(|| a.stack.visible_child().unwrap_or(root.clone()));
            if let Some(b) = find(&scope, &|w| w.downcast_ref::<gtk::Button>().filter(|b| b.has_css_class("suggested-action") && b.is_mapped()).cloned()) {
                b.emit_clicked();
            }
            return glib::ControlFlow::Continue;
        }
        if std::env::var_os("FINGRAPH_SCROLL").is_some() && !scrolled.replace(true) {
            fn first_scroller(w: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
                if let Some(sw) = w.downcast_ref::<gtk::ScrolledWindow>().filter(|s| s.is_mapped()) {
                    return Some(sw.clone());
                }
                let mut c = w.first_child();
                while let Some(ch) = c {
                    if let Some(x) = first_scroller(&ch) {
                        return Some(x);
                    }
                    c = ch.next_sibling();
                }
                None
            }
            if let Some(sw) = a.stack.visible_child().as_ref().and_then(first_scroller) {
                let adj = sw.vadjustment();
                adj.set_value(adj.upper());
            }
            return glib::ControlFlow::Continue;
        }
        let paint = gtk::WidgetPaintable::new(Some(&a.win));
        let snap = gtk::Snapshot::new();
        paint.snapshot(&snap, a.win.width() as f64, a.win.height() as f64);
        if let (Some(node), Some(r)) = (snap.to_node(), a.win.renderer()) {
            let _ = r.render_texture(node, None).save_to_png(&out);
        }
        a.win.application().unwrap().quit();
        glib::ControlFlow::Break
    });
}

fn main() -> glib::ExitCode {
    let mut flags = gio::ApplicationFlags::HANDLES_OPEN;
    if cfg!(debug_assertions) && std::env::var_os("FINGRAPH_SHOT").is_some() {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let gapp = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    let current: Rc<RefCell<Option<Rc<App>>>> = Rc::new(RefCell::new(None));
    gapp.connect_startup(|_| {
        gtk::Window::set_default_icon_name(APP_ID);
        css();
    });
    {
        let current = current.clone();
        gapp.connect_activate(move |g| {
            let existing = current.borrow().clone();
            match existing {
                Some(a) => a.win.present(),
                None => *current.borrow_mut() = Some(App::new(g)),
            }
        });
    }
    // `fingraph statement.pdf`, or Files → Open With → FinGraph
    gapp.connect_open(move |g, files, _| {
        let existing = current.borrow().clone();
        let a = existing.unwrap_or_else(|| {
            let a = App::new(g);
            *current.borrow_mut() = Some(a.clone());
            a
        });
        let paths: Vec<_> = files.iter().filter_map(|f| f.path()).collect();
        statements::import_paths(&a, paths);
    });
    gapp.run()
}
