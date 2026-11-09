//! FinGraph: a local-first ledger for Cayman bank statements. Drop PDFs in, the 5-pass scanner reads
//! and proves them, Smart Triage learns your merchants, and the dashboard shows where money goes.

mod account;
mod budget;
mod changes;
mod charts;
mod close;
mod dashboard;
mod directory;
mod export;
mod guide;
mod outlook;
mod owed;
mod picker;
mod planpdf;
mod preferences;
mod profiles;
mod reports;
mod review;
mod rules;
mod share;
mod split;
mod statement;
mod statements;
mod transactions;
mod txedit;
mod update;
mod wealth;
mod wizard;

use adw::prelude::*;
use chrono::{Datelike, Local, NaiveDate};
use fingraph_core::db::{Category, Db, Entity, TxFilter, TxRow};
use fingraph_core::insights::{self, Range};
use fingraph_core::money::fmt_money;
use fingraph_core::{Currency, Decimal};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

const APP_ID: &str = "dev.fingraph.FinGraph";

/// Pages reached from Budget (To Review) or the main menu, not listed in the sidebar.
const OFF_SIDEBAR: [&str; 5] = ["review", "rules", "directory", "profiles", "export"];
/// The sidebar's pages, Ctrl+1 onward. FinGraph always opens on the first.
const MAIN_PAGES: [&str; 6] = ["budget", "transactions", "reports", "statements", "statement", "summary"];
/// The tabs of Reports, each a page of its own under the one sidebar row.
const REPORT_TABS: [(&str, &str); 7] = [("plan", "Plan"), ("outlook", "Outlook"), ("changes", "What Changed"), ("subscriptions", "Subscriptions"), ("cashflow", "Cash Flow"), ("trends", "Trends"), ("flow", "Where It Went")];

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
    /// The date range and its "change the custom dates" button.
    range_box: gtk::Box,
    range_edit: gtk::Button,
    /// True while the range list is rebuilt: its selection changes aren't the user's.
    range_quiet: Cell<bool>,
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
    /// A refresh is queued for just before the next frame.
    refresh_queued: Cell<bool>,
    /// Every row of the workspaces on screen, kept only while a refresh runs (several parts want it).
    rows_cache: RefCell<Option<(Vec<i64>, Vec<TxRow>)>>,
    refreshing: Cell<bool>,
    /// The currency amounts are shown in: KYD, USD and any other currency in use.
    display_dd: gtk::DropDown,
    display_list: RefCell<Vec<Currency>>,
    /// True while the currency list is rebuilt: its selection changes aren't the user's.
    display_quiet: Cell<bool>,
    /// An exchange-rate download is running.
    rates_busy: Cell<bool>,
    /// The watched folder (Preferences → Import), while watching.
    pub watch: RefCell<Option<gio::FileMonitor>>,
    scope: RefCell<Scope>,
    entities: RefCell<Vec<Entity>>,
    display: Cell<Currency>,
    views: RefCell<Vec<(&'static str, Refresh)>>,
    /// Transactions view: show only these ids (from an insight card), until cleared.
    pub focus_ids: RefCell<Option<Vec<i64>>>,
    /// Every category's look, read once per refresh (each table row wants one).
    looks_now: RefCell<HashMap<String, fingraph_core::looks::Look>>,
    /// The strip of Reports tabs, and the tab last open.
    report_bar: gtk::ScrolledWindow,
    report_tabs: RefCell<Vec<(&'static str, gtk::ToggleButton)>>,
    last_report: Cell<&'static str>,
    /// What's folded in the sidebar.
    statements_open: Cell<bool>,
    accounts_open: Cell<bool>,
    statements_arrow: gtk::Button,
    accounts_arrow: gtk::Image,
    accounts_row: gtk::ListBoxRow,
    /// Beside the main menu: flips between light and dark.
    theme_btn: gtk::Button,
}

/// Sets a "this change isn't the user's" flag until dropped, so a panic halfway can't leave it stuck on.
pub struct Quiet<'a>(&'a Cell<bool>);

impl<'a> Quiet<'a> {
    pub fn on(c: &'a Cell<bool>) -> Self {
        c.set(true);
        Quiet(c)
    }
}

impl Drop for Quiet<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
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
        let content_title = adw::WindowTitle::new("Budget", "");

        // ── header: workspace, currency, range, triage
        // the workspace decides everything below it, so it heads the sidebar
        let workspace_btn = gtk::MenuButton::builder().tooltip_text("Workspace").always_show_arrow(true).hexpand(true).margin_start(8).margin_end(8).margin_bottom(6).build();
        let display_dd = gtk::DropDown::builder().tooltip_text("Show amounts in this currency").build();
        let ccy_box = gtk::Box::builder().build();
        ccy_box.append(&display_dd);
        let range_dd = gtk::DropDown::builder().tooltip_text("Date range").build();
        let range_edit = gtk::Button::builder().icon_name("document-edit-symbolic").tooltip_text("Change the custom dates").visible(false).build();
        let range_box = gtk::Box::builder().css_classes(["linked"]).build();
        range_box.append(&range_dd);
        range_box.append(&range_edit);
        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Import Statements…"), Some("win.import"));
        s1.append(Some("Export"), Some("win.show-export"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Rules & Categories"), Some("win.show-rules"));
        s2.append(Some("Business Directory"), Some("win.show-directory"));
        s2.append(Some("Bank Profiles"), Some("win.show-profiles"));
        s2.append(Some("Preferences"), Some("win.preferences"));
        s2.append(Some("Getting Started"), Some("win.welcome"));
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Share Merchant Hints…"), Some("win.share-hints"));
        s3.append(Some("About FinGraph"), Some("win.about"));
        menu.append_section(None, &s3);
        let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).tooltip_text("Main Menu").build();

        let content_header = adw::HeaderBar::builder().title_widget(&content_title).build();
        content_header.pack_start(&ccy_box);
        content_header.pack_start(&range_box);
        content_header.pack_end(&menu_btn);
        let theme_btn = gtk::Button::new();
        content_header.pack_end(&theme_btn);
        let content_tv = adw::ToolbarView::new();
        content_tv.add_top_bar(&content_header);
        let report_box = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).margin_top(6).margin_bottom(6).margin_start(12).margin_end(12).build();
        let report_bar = gtk::ScrolledWindow::builder().child(&report_box).vscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).visible(false).build();
        content_tv.add_top_bar(&report_bar);
        content_tv.set_content(Some(&stack));
        let content = adw::NavigationPage::builder().title("FinGraph").child(&content_tv).build();

        // ── sidebar
        let side_list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
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

        let open_setting = |k: &str| db.setting(k).as_deref() != Some("0");
        let (statements_open, accounts_open) = (open_setting("sidebar_statements"), open_setting("sidebar_accounts"));
        let app = Rc::new(App {
            win,
            toasts,
            db,
            stack,
            split,
            content_title,
            workspace_btn,
            range_dd,
            range_box,
            range_edit,
            range_quiet: Cell::new(false),
            range_items: RefCell::new(Vec::new()),
            custom_range: Cell::new(None),
            side_list: side_list.clone(),
            ccy_box: ccy_box.clone(),
            badges: RefCell::new(HashMap::new()),
            accounts_key: RefCell::new(String::new()),
            quiet: Cell::new(false),
            refresh_queued: Cell::new(false),
            rows_cache: RefCell::new(None),
            refreshing: Cell::new(false),
            display_dd,
            display_list: RefCell::new(Vec::new()),
            display_quiet: Cell::new(false),
            rates_busy: Cell::new(false),
            watch: RefCell::new(None),
            scope: RefCell::new(Scope::Consolidated(Vec::new())),
            entities: RefCell::new(Vec::new()),
            display: Cell::new(Currency::KYD),
            views: RefCell::new(Vec::new()),
            focus_ids: RefCell::new(None),
            theme_btn,
            report_bar,
            report_tabs: RefCell::default(),
            last_report: Cell::new(REPORT_TABS[0].0),
            statements_open: Cell::new(statements_open),
            accounts_open: Cell::new(accounts_open),
            statements_arrow: gtk::Button::builder().valign(gtk::Align::Center).css_classes(["flat", "circular"]).tooltip_text("Show or hide Combined Statement").build(),
            accounts_arrow: gtk::Image::new(),
            accounts_row: gtk::ListBoxRow::builder().name("accounts-header").selectable(false).activatable(true).tooltip_text("Show or hide your accounts").build(),
            looks_now: RefCell::default(),
        });
        APP.with(|w| *w.borrow_mut() = Rc::downgrade(&app));
        app.display.set(app.db.setting("display_currency").and_then(|c| Currency::parse(&c)).unwrap_or(Currency::KYD));
        app.rebuild_display_list();

        // pages, in the order you use them; Accounts follow (see update_sidebar)
        let (o, or) = dashboard::build(&app);
        let (rv, rvr) = review::build(&app);
        let (c, cr) = changes::build(&app);
        let (t, tr) = transactions::build(&app);
        let (cs, csr) = statement::build(&app);
        let (bu, bur) = budget::build(&app);
        let (fl, flr) = budget::build_report(&app, true);
        let (pl, plr) = budget::build_report(&app, false);
        let (ol, olr) = outlook::build(&app);
        let (cf, cfr) = reports::build_cash_flow(&app);
        let (tn, tnr) = reports::build_trends(&app);
        let (sb, sbr) = rules::build(&app, rules::Part::Subscriptions);
        let (st, str_) = statements::build(&app);
        let (e, er) = export::build(&app);
        let (r, rr) = rules::build(&app, rules::Part::Rules);
        let (p, pr) = profiles::build(&app);
        let (dir, dirr) = directory::build(&app);
        let (ac, acr) = account::build(&app);
        // an account's overview is reached from its row under Accounts, not listed itself
        app.stack.add_titled(&ac, Some("account"), "Account");
        app.views.borrow_mut().push(("account", acr));
        let pages: Vec<(&'static str, &str, gtk::Widget, Refresh)> = vec![
            ("summary", "Summary", o, or),
            ("cashflow", "Cash Flow", cf, cfr),
            ("trends", "Trends", tn, tnr),
            ("budget", "Budget", bu, bur),
            ("flow", "Where It Went", fl, flr),
            ("plan", "Plan", pl, plr),
            ("outlook", "Outlook", ol, olr),
            ("transactions", "Transactions", t, tr),
            ("statement", "Combined Statement", cs, csr),
            ("statements", "Statements", st, str_),
            ("changes", "What Changed", c, cr),
            ("subscriptions", "Subscriptions", sb, sbr),
            ("review", "To Review", rv, rvr),
            ("export", "Export", e, er),
            ("rules", "Rules & Categories", r, rr),
            ("directory", "Business Directory", dir, dirr),
            ("profiles", "Bank Profiles", p, pr),
        ];
        for (name, title, w, refresh) in pages {
            if matches!(name, "summary" | "cashflow" | "trends" | "budget" | "flow" | "plan" | "outlook" | "statement" | "changes" | "subscriptions" | "export") {
                let wrapper = gtk::Stack::new();
                wrapper.add_named(&w, Some("ready"));
                let missing = adw::StatusPage::builder().title("Exchange rate needed")
                    .icon_name("dialog-information-symbolic").build();
                missing.set_child(Some(&gtk::Button::builder().label("Open Preferences → Money").action_name("win.money-preferences").halign(gtk::Align::Center).build()));
                wrapper.add_named(&missing, Some("missing"));
                app.stack.add_titled(&wrapper, Some(name), title);
                let weak = Rc::downgrade(&app);
                app.views.borrow_mut().push((name, Rc::new(move || {
                    let Some(a) = weak.upgrade() else { return };
                    match a.valuation_error() {
                        Some(message) => { missing.set_description(Some(&message)); wrapper.set_visible_child_name("missing"); }
                        None => { wrapper.set_visible_child_name("ready"); refresh(); }
                    }
                })));
            } else {
                app.stack.add_titled(&w, Some(name), title);
                app.views.borrow_mut().push((name, refresh));
            }
        }
        for (name, title, icon) in [
            ("budget", "Budget", "accessories-calculator-symbolic"),
            ("transactions", "Transactions", "view-list-bullet-symbolic"),
            ("reports", "Reports", "x-office-spreadsheet-symbolic"),
            ("statements", "Statements", "x-office-document-symbolic"),
            ("statement", "Combined Statement", "view-paged-symbolic"),
            ("summary", "Summary", "view-grid-symbolic"),
        ] {
            side_list.append(&app.side_row(name, title, icon));
        }
        app.build_sidebar_folds();
        app.build_report_tabs(&report_box);
        {
            let w = Rc::downgrade(&app);
            side_list.connect_row_selected(move |_, row| {
                let (Some(a), Some(row)) = (w.upgrade(), row) else { return };
                if a.quiet.get() {
                    return;
                }
                let name = row.widget_name().to_string();
                if let Some(id) = name.strip_prefix("account:").and_then(|s| s.parse::<i64>().ok()) {
                    return account::show(&a, id);
                }
                if a.stack.visible_child_name().as_deref() != Some(name.as_str()) {
                    a.show_view(&name);
                }
            });
        }
        {
            let w = Rc::downgrade(&app);
            app.display_dd.connect_selected_notify(move |dd| {
                let Some(a) = w.upgrade() else { return };
                if a.display_quiet.get() {
                    return;
                }
                if let Some(c) = a.display_list.borrow().get(dd.selected() as usize).copied() {
                    a.display.set(c);
                    a.saved(a.db.set_setting("display_currency", c.code()));
                }
                a.refresh();
            });
        }
        {
            let w = Rc::downgrade(&app);
            app.range_dd.connect_selected_notify(move |dd| {
                let Some(a) = w.upgrade() else { return };
                let custom = matches!(a.range_items.borrow().get(dd.selected() as usize), Some(Range::Custom(..)));
                a.range_edit.set_visible(custom);
                if a.range_quiet.get() {
                    return;
                }
                if custom {
                    a.ask_custom_range();
                } else {
                    a.refresh();
                }
            });
            let w = Rc::downgrade(&app);
            app.range_edit.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    a.ask_custom_range();
                }
            });
        }
        app.actions();
        app.build_theme_toggle();
        // where to start: Preferences → General
        let prefs = app.db.prefs();
        preferences::apply_appearance(&prefs.appearance);
        let start = if prefs.start_workspace == "last" { prefs.last_workspace.clone() } else { prefs.start_workspace.clone() };
        if let Ok(id) = start.parse::<i64>() {
            *app.scope.borrow_mut() = Scope::One(id);
        }
        app.reload_entities();
        app.show_view("budget");
        app.win.present();
        // today's rates (and any a statement still lacks), quietly, when downloads are on
        app.update_rates(false);
        update::check(&app);
        statements::watch(&app);
        {
            let w = Rc::downgrade(&app);
            app.win.connect_is_active_notify(move |win| {
                if let (true, Some(a)) = (win.is_active(), w.upgrade()) {
                    statements::resweep(&a);
                }
            });
        }

        let shot = cfg!(debug_assertions) && std::env::var_os("FINGRAPH_SHOT").is_some();
        if !shot && app.db.setting("onboarded").is_none() {
            guide::open(&app);
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
                    guard(&a, || f(&a));
                }
            });
            self.win.add_action(&act);
        };
        add("import", Rc::new(statements::choose_files));
        add(
            "demo",
            Rc::new(|a| match a.db.load_demo() {
                Ok(n) => {
                    a.saved(a.db.set_setting("onboarded", "1"));
                    a.reload_entities();
                    let docs = a.db.documents_all(&[]).map_or(0, |d| d.len());
                    a.toast_action(&format!("Loaded {n} demo transactions from {docs} simulated statements"), "Categorize", Rc::new(wizard::triage));
                }
                Err(e) => a.toast(&e.to_string()),
            }),
        );
        add("workspaces", Rc::new(wizard::workspaces_dialog));
        add("rescan", Rc::new(statements::rescan_all));
        add("preferences", Rc::new(preferences::show));
        add("money-preferences", Rc::new(|a| preferences::show_at(a, "money")));
        for (i, page) in MAIN_PAGES.iter().enumerate() {
            add(&format!("page-{}", i + 1), Rc::new(move |a: &Rc<App>| a.show_view(page)));
        }
        for page in OFF_SIDEBAR {
            add(&format!("show-{page}"), Rc::new(move |a: &Rc<App>| a.show_view(page)));
        }
        add("retriage", Rc::new(wizard::reset_and_retriage));
        add("share-hints", Rc::new(share::open));
        add("setup-budget", Rc::new(|a: &Rc<App>| budget::wizard(a, { let w = Rc::downgrade(a); Rc::new(move || w.upgrade().iter().for_each(App::refresh)) })));
        add("fx", Rc::new(|a| a.fx_dialog()));
        add("welcome", Rc::new(guide::open));
        add(
            "about",
            Rc::new(|a| {
                let d = adw::AboutDialog::builder()
                    .application_name("FinGraph")
                    .application_icon(APP_ID)
                    .version(env!("CARGO_PKG_VERSION"))
                    .comments("Local-first ledger and 5-pass statement scanner for bank statements. Your statements and ledger never leave this computer. It checks GitHub once a day for a newer release (can be turned off), and with exchange-rate downloads on, only currency codes and dates are sent.")
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
            for i in 0..MAIN_PAGES.len().min(9) {
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
        self.rebuild_display_list();
        self.refresh();
    }

    fn workspace_label(&self) -> String {
        let ents = self.entities.borrow();
        match &*self.scope.borrow() {
            Scope::One(id) => ents.iter().find(|e| e.id == *id).map_or("Workspace".into(), |e| e.name.clone()),
            Scope::Consolidated(v) if v.len() == ents.len() => "Consolidated Overview".into(),
            Scope::Consolidated(v) => format!("Consolidated ({})", v.len()),
        }
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
        self.workspace_btn.set_label(&self.workspace_label());
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
                // the last one stays: consolidating nothing isn't a view
                let last = matches!(&*a.scope.borrow(), Scope::Consolidated(v) if v.as_slice() == [id]);
                if last && !cb.is_active() {
                    return cb.set_active(true);
                }
                if let Scope::Consolidated(v) = &mut *a.scope.borrow_mut() {
                    v.retain(|x| *x != id);
                    if cb.is_active() {
                        v.push(id);
                    }
                }
                a.workspace_btn.set_label(&a.workspace_label());
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
        let _q = Quiet::on(&self.range_quiet);
        self.range_dd.set_model(Some(&model));
        self.range_dd.set_selected(pick as u32);
        self.range_edit.set_visible(matches!(items.get(pick), Some(Range::Custom(..))));
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
        d.connect_response(None, move |_, r| crate::shielded(|| {
            let Some(a) = w.upgrade() else { return };
            let p = |e: &adw::EntryRow| NaiveDate::parse_from_str(e.text().trim(), "%Y-%m-%d").ok();
            if r == "ok" {
                match (p(&from), p(&to)) {
                    (Some(f), Some(t)) => a.custom_range.set(Some((f.min(t), f.max(t)))),
                    _ => a.toast("Those dates weren't understood (use YYYY-MM-DD, e.g. 2026-10-31); the range is unchanged"),
                }
            }
            a.refresh();
        }));
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

    /// Show amounts in `c` from now on: remembered, picked in the header, and every page redrawn.
    pub fn set_display(self: &Rc<Self>, c: Currency) {
        self.display.set(c);
        self.saved(self.db.set_setting("display_currency", c.code()));
        // selects `c` quietly (adding it when no account uses it)
        self.rebuild_display_list();
        self.refresh();
    }

    /// The header's currency list: KYD, USD, every other currency an account or workspace uses, and the one shown.
    fn rebuild_display_list(&self) {
        let mut list = vec![Currency::KYD, Currency::USD];
        list.extend(self.db.floating_currencies().unwrap_or_default());
        if !list.contains(&self.display.get()) {
            list.push(self.display.get());
        }
        let codes: Vec<&str> = list.iter().map(|c| c.code()).collect();
        let _q = Quiet::on(&self.display_quiet);
        *self.display_list.borrow_mut() = list.clone();
        self.display_dd.set_model(Some(&gtk::StringList::new(&codes)));
        self.display_dd.set_selected(list.iter().position(|c| *c == self.display.get()).unwrap_or(0) as u32);
        self.display_dd.set_visible(list.len() > 1);
    }

    /// Download what exchange rates are missing (when switched on in Preferences), off the main thread, then keep
    /// them and show amounts converted with them. `announce`: say so when there's nothing to do or it fails.
    pub fn update_rates(self: &Rc<Self>, announce: bool) {
        if !self.db.prefs().download_rates || self.rates_busy.get() {
            if announce {
                self.toast(if self.rates_busy.get() { "Already updating exchange rates…" } else { "Exchange-rate downloads are off (Preferences → Money)" });
            }
            return;
        }
        let needed = self.db.rates_needed(today()).unwrap_or_default();
        if needed.is_empty() {
            if announce {
                self.toast("Exchange rates are up to date");
            }
            return;
        }
        self.rates_busy.set(true);
        let w = Rc::downgrade(self);
        crate::spawn_guarded(w.clone(), async move {
            let got = gio::spawn_blocking(move || needed.iter().map(|(cs, from, to)| fingraph_core::rates::download(cs, *from, *to).map_err(|e| e.0)).collect::<Vec<_>>()).await.unwrap_or_default();
            let Some(a) = w.upgrade() else { return };
            a.rates_busy.set(false);
            let (mut stored, mut failed, mut sources) = (0, Vec::new(), Vec::new());
            for g in got {
                match g {
                    Ok(d) => {
                        stored += a.db.store_rates(&d.rates, d.source).unwrap_or(0);
                        if !d.rates.is_empty() && !sources.contains(&d.source) {
                            sources.push(d.source);
                        }
                    }
                    Err(e) => failed.push(e),
                }
            }
            if stored > 0 {
                a.toast(&format!("Exchange rates updated from {}", sources.join(" and ")));
                a.rebuild_display_list();
                a.refresh();
            } else if announce || !failed.is_empty() {
                a.toast(&if failed.is_empty() { "No new exchange rates".to_string() } else { format!("Couldn't download exchange rates; amounts use the last ones kept ({})", failed[0]) });
            }
        });
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
    pub fn looks(&self) -> HashMap<String, fingraph_core::looks::Look> {
        let cats = self.categories();
        cats.iter().map(|c| (c.name.clone(), fingraph_core::looks::look_of(c, &cats))).collect()
    }

    /// A small coloured tile with the category's icon: its identity on every screen.
    pub fn category_tile(&self, name: &str) -> gtk::Box {
        tile_for(self.looks_now.borrow().get(name))
    }

    pub fn parents(&self) -> BTreeMap<i64, String> {
        let cats = self.categories();
        cats.iter().filter_map(|c| Some((c.id, cats.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect()
    }

    // ── navigation & refresh

    pub fn show_view(self: &Rc<Self>, name: &str) {
        // Reports is its tabs: the row opens the one last used
        let name = if name == "reports" { self.last_report.get() } else { name };
        let tab = REPORT_TABS.iter().find(|t| t.0 == name).map(|t| t.0);
        if let Some(t) = tab {
            self.last_report.set(t);
        }
        self.report_bar.set_visible(tab.is_some());
        {
            let _q = Quiet::on(&self.quiet);
            for (n, b) in self.report_tabs.borrow().iter() {
                b.set_active(Some(*n) == tab);
            }
        }
        if name == "statement" && !self.statements_open.get() {
            self.set_statements_open(true);
        }
        self.stack.set_visible_child_name(name);
        if let Some(c) = self.stack.child_by_name(name) {
            self.content_title.set_title(&self.stack.page(&c).title().unwrap_or_default());
        }
        // highlight it in the sidebar (an account row stays highlighted over its transactions)
        let on_account = self.side_list.selected_row().is_some_and(|r| r.widget_name().starts_with("account:"));
        if !(on_account && matches!(name, "transactions" | "account"))
            && let Some(r) = self.row_named(if tab.is_some() { "reports" } else { name })
            && self.side_list.selected_row().as_ref() != Some(&r)
        {
            self.side_list.select_row(Some(&r));
        } else if OFF_SIDEBAR.contains(&name) {
            self.side_list.unselect_all();
        }
        // the header shows only what this page uses
        self.range_box.set_visible(matches!(name, "summary" | "changes" | "transactions" | "statement" | "export" | "account"));
        self.ccy_box.set_visible(matches!(name, "summary" | "changes" | "transactions" | "statement" | "budget" | "subscriptions" | "export" | "outlook" | "cashflow" | "trends" | "flow" | "plan"));
        self.split.set_show_content(true);
        self.refresh();
    }

    fn side_row(&self, name: &str, title: &str, icon: &str) -> gtk::ListBoxRow {
        let row = gtk::Box::builder().spacing(12).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
        // a plain symbolic icon, as GNOME's own sidebars do: the page names carry the meaning, not colour
        row.append(&gtk::Image::builder().icon_name(icon).valign(gtk::Align::Center).build());
        row.append(&gtk::Label::builder().label(title).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
        let badge = gtk::Label::builder().css_classes(["dim-label", "caption", "tnum"]).build();
        row.append(&badge);
        if let Some(n) = MAIN_PAGES.iter().position(|p| *p == name).filter(|n| *n < 9) {
            row.set_tooltip_text(Some(&format!("Ctrl+{}", n + 1)));
        }
        if let Some(&key) = ["changes", "subscriptions", "statements", "budget"].iter().find(|k| **k == name) {
            self.badges.borrow_mut().insert(key, badge);
        }
        let out = gtk::ListBoxRow::builder().child(&row).name(name).build();
        if name == "statement" {
            row.set_margin_start(36);
            out.set_visible(self.statements_open.get());
        }
        if name == "statements" {
            row.append(&self.statements_arrow);
        }
        out
    }

    /// The sun or moon beside the menu: it picks Light or Dark in Preferences → Appearance, so the two
    /// always agree. It shows what a press switches to, and follows the system while that's in charge.
    fn build_theme_toggle(self: &Rc<Self>) {
        let show = {
            let b = self.theme_btn.clone();
            move |dark: bool| {
                b.set_icon_name(if dark { "weather-clear-symbolic" } else { "weather-clear-night-symbolic" });
                let tip = if dark { "Switch to Light Style" } else { "Switch to Dark Style" };
                b.set_tooltip_text(Some(tip));
                b.update_property(&[gtk::accessible::Property::Label(tip)]);
            }
        };
        let sm = adw::StyleManager::default();
        show(sm.is_dark());
        sm.connect_dark_notify(move |sm| show(sm.is_dark()));
        let w = Rc::downgrade(self);
        self.theme_btn.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let style = if adw::StyleManager::default().is_dark() { "light" } else { "dark" };
            let mut p = a.db.prefs();
            p.appearance = style.into();
            a.saved(a.db.save_prefs(&p));
            preferences::apply_appearance(style);
        });
    }

    /// The two folds: Combined Statement under Statements, and the Accounts list.
    fn build_sidebar_folds(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.statements_arrow.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                a.set_statements_open(!a.statements_open.get());
            }
        });
        let head = gtk::Box::builder().spacing(6).margin_top(8).margin_bottom(2).margin_start(6).margin_end(6).build();
        head.append(&gtk::Label::builder().label("Accounts").xalign(0.0).hexpand(true).css_classes(["caption-heading", "dim-label"]).build());
        head.append(&self.accounts_arrow);
        self.accounts_row.set_child(Some(&head));
        self.side_list.append(&self.accounts_row);
        let w = Rc::downgrade(self);
        self.side_list.connect_row_activated(move |_, row| {
            if let (Some(a), "accounts-header") = (w.upgrade(), row.widget_name().as_str()) {
                a.set_accounts_open(!a.accounts_open.get());
            }
        });
        self.show_fold_state();
    }

    fn show_fold_state(&self) {
        let (s, a) = (self.statements_open.get(), self.accounts_open.get());
        self.statements_arrow.set_icon_name(if s { "pan-down-symbolic" } else { "pan-end-symbolic" });
        self.statements_arrow.update_state(&[gtk::accessible::State::Expanded(Some(s))]);
        self.accounts_arrow.set_icon_name(Some(if a { "pan-down-symbolic" } else { "pan-end-symbolic" }));
        self.accounts_row.update_state(&[gtk::accessible::State::Expanded(Some(a))]);
    }

    fn set_statements_open(&self, open: bool) {
        self.statements_open.set(open);
        let _ = self.db.set_setting("sidebar_statements", if open { "1" } else { "0" });
        if let Some(r) = self.row_named("statement") {
            r.set_visible(open);
        }
        self.show_fold_state();
    }

    fn set_accounts_open(&self, open: bool) {
        self.accounts_open.set(open);
        let _ = self.db.set_setting("sidebar_accounts", if open { "1" } else { "0" });
        let mut i = 0;
        while let Some(r) = self.side_list.row_at_index(i) {
            if r.widget_name().starts_with("account:") {
                r.set_visible(open);
            }
            i += 1;
        }
        self.show_fold_state();
    }

    fn build_report_tabs(self: &Rc<Self>, into: &gtk::Box) {
        let mut tabs: Vec<(&'static str, gtk::ToggleButton)> = Vec::new();
        for (label, names) in [
            ("Plan", &["plan", "outlook"][..]),
            ("Monitor", &["changes", "subscriptions"][..]),
            ("Analyze", &["cashflow", "trends", "flow"][..]),
        ] {
            let group = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3).margin_end(16).build();
            group.append(&gtk::Label::builder().label(label).xalign(0.0).css_classes(["caption", "dim-label"]).build());
            let buttons = gtk::Box::builder().spacing(4).build();
            for (name, title) in REPORT_TABS.iter().copied().filter(|(name, _)| names.contains(name)) {
                let b = gtk::ToggleButton::builder().label(title).build();
                if let Some((_, first)) = tabs.first() { b.set_group(Some(first)); }
                let weak = Rc::downgrade(self);
                b.connect_toggled(move |b| if let Some(a) = weak.upgrade() { if b.is_active() && !a.quiet.get() { a.show_view(name); } });
                buttons.append(&b); tabs.push((name, b));
            }
            group.append(&buttons); into.append(&group);
        }
        *self.report_tabs.borrow_mut() = tabs;
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
        let rows = self.all_rows();
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
        let changes = changes::count(self);
        set("changes", if changes > 0 { changes.to_string() } else { String::new() }, false);
        let subs: Decimal = insights::subscriptions(&rows, self.display(), &fx, today()).iter().filter(|s| s.active).map(|s| s.monthly).sum();
        set("subscriptions", if subs.is_zero() { String::new() } else { format!("{}/mo", fmt_money(subs.round_dp(0), self.display())) }, false);
        // a statement that doesn't balance, a month with no statement, or a break between statements
        let cov = self.db.coverage(&ids, 12, today()).unwrap_or_default();
        let missing = cov.iter().map(|c| c.months.iter().filter(|m| m.1 == fingraph_core::workflows::MonthState::Missing).count() + c.breaks.len()).sum::<usize>();
        let waiting = self.db.documents_all(&ids).unwrap_or_default().iter().filter(|d| !d.review.is_empty()).count();
        let bad = docs.iter().filter(|d| matches!(d.reconciliation, fingraph_core::Reconciliation::Discrepancy(_))).count() + missing + waiting;
        set("statements", if bad > 0 { format!("⚠ {bad}") } else { String::new() }, bad > 0);
        let (b, over) = if self.valuation_error().is_none() { budget::badge(self) } else { ("Rate needed".into(), false) };
        set("budget", b, over);

        // accounts: rebuilt in place at the end, only when they or their balances changed
        let balances = insights::latest_balances(&rows, &docs);
        let accounts: Vec<_> = self.db.accounts().unwrap_or_default().into_iter().filter(|x| ids.contains(&x.entity_id) && !x.hidden).collect();
        let key = format!("{:?}|{}", accounts.iter().map(|x| (x.id, &x.name, balances.get(&x.id))).collect::<Vec<_>>(), fingraph_core::money::fmt_money(Decimal::ONE, self.display()));
        if *self.accounts_key.borrow() == key {
            return;
        }
        *self.accounts_key.borrow_mut() = key;
        let _q = Quiet::on(&self.quiet);
        let selected = self.side_list.selected_row().map(|r| r.widget_name().to_string());
        let mut i = 0;
        while let Some(r) = self.side_list.row_at_index(i) {
            if r.widget_name().starts_with("account:") {
                self.side_list.remove(&r);
            } else {
                i += 1;
            }
        }
        for acct in &accounts {
            let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(4).margin_bottom(4).margin_start(6).margin_end(6).build();
            b.append(&gtk::Label::builder().label(&acct.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
            let (bal, tip) = match balances.get(&acct.id) {
                Some((d, v)) => (fmt_money(*v, acct.currency), format!("Last verified balance, as of {} (from your statements, not live)", d.format("%d %b %Y"))),
                None => ("—".into(), "No balance printed yet".into()),
            };
            let owed = balances.get(&acct.id).is_some_and(|(_, v)| v.is_sign_negative() && !v.is_zero());
            b.append(&gtk::Label::builder().label(&bal).xalign(1.0).css_classes(["caption", "tnum", if owed { "amount-out" } else { "dim-label" }]).build());
            let row = gtk::ListBoxRow::builder().child(&b).name(format!("account:{}", acct.id)).tooltip_text(tip).visible(self.accounts_open.get()).build();
            self.side_list.append(&row);
            if selected.as_deref() == Some(row.widget_name().as_str()) {
                self.side_list.select_row(Some(&row));
            }
        }
        self.accounts_row.set_visible(!accounts.is_empty());
    }

    /// Re-read the ledger into the visible view and the header, once, just before the next frame:
    /// a burst of changes (start-up fills three dropdowns, an edit also re-picks a page) costs one.
    pub fn refresh(self: &Rc<Self>) {
        if self.refresh_queued.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(a) = w.upgrade() {
                a.refresh_queued.set(false);
                a.refresh_now();
            }
            glib::ControlFlow::Break
        });
    }

    /// Every transaction (split lines apart) of the workspaces on screen. Read once per refresh.
    pub fn all_rows(&self) -> Vec<TxRow> {
        let ids = self.entity_ids();
        if let Some((k, rows)) = &*self.rows_cache.borrow()
            && *k == ids
        {
            return rows.clone();
        }
        let rows = self.db.transactions(&TxFilter { entity_ids: ids.clone(), split_lines: true, ..Default::default() }).unwrap_or_default();
        if self.refreshing.get() {
            *self.rows_cache.borrow_mut() = Some((ids, rows.clone()));
        }
        rows
    }

    pub fn refresh_now(self: &Rc<Self>) {
        self.refreshing.set(true);
        *self.rows_cache.borrow_mut() = None;
        let looks = self.looks();
        charts::set_category_colors(looks.iter().map(|(n, l)| (n.clone(), fingraph_core::looks::rgb_of(&l.color))).collect());
        *self.looks_now.borrow_mut() = looks;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.update_sidebar()));
        self.content_title.set_subtitle(&if self.range_box.is_visible() { self.range_label() } else { String::new() });
        let visible = self.stack.visible_child_name().unwrap_or_default();
        let f = self.views.borrow().iter().find(|(n, _)| *n == visible.as_str()).map(|v| v.1.clone());
        if let Some(f) = f {
            // a bug in one page shouldn't take the app down: a panic in a GTK callback aborts
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f())).is_err() {
                self.toast("Something went wrong showing this page. Details are in crash.log in the data folder.");
            }
        }
        self.refreshing.set(false);
        *self.rows_cache.borrow_mut() = None;
    }

    pub fn show_transactions(self: &Rc<Self>, ids: Vec<i64>) {
        *self.focus_ids.borrow_mut() = Some(ids);
        self.show_view("transactions");
    }

    pub(crate) fn valuation_error(&self) -> Option<String> {
        match self.db.missing_conversion_rates(&self.entity_ids(), self.display()) {
            Ok(missing) if missing.is_empty() => None,
            Ok(missing) => Some(format!("No rate for {} in {}. Combined totals and plans are unavailable until you add or download a rate in Preferences → Money. Original transactions and statements remain available.", missing.iter().map(|c| c.code()).collect::<Vec<_>>().join(", "), self.display())),
            Err(e) => Some(format!("Couldn't read the data needed for financial totals: {e}")),
        }
    }

    pub(crate) fn financial_ready(&self) -> bool {
        if let Some(e) = self.valuation_error() { self.toast(&e); false } else { true }
    }

    /// Whether a change was saved; when it wasn't, says why.
    pub fn saved<T>(&self, r: fingraph_core::Result<T>) -> bool {
        r.map_err(|e| self.toast(&format!("Couldn't save that: {e}"))).is_ok()
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
        d.connect_response(None, move |_, r| crate::shielded(|| {
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
        }));
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
                guard(&a, || f(&a));
            }
        });
        self.toasts.add_toast(t);
    }

    /// An edit Undo can take back: a checkpoint, then `edit` (the toast text, or None when nothing
    /// changed), then a toast whose Undo restores the checkpoint. One step: the next edit replaces it.
    pub fn undoable(self: &Rc<Self>, edit: impl FnOnce(&Rc<Self>) -> Option<String>) {
        let ready = self.db.checkpoint().is_ok();
        let Some(msg) = edit(self) else { return };
        if !ready {
            return self.toast(&msg);
        }
        self.toast_action(
            &msg,
            "Undo",
            Rc::new(|a: &Rc<App>| match a.db.undo_to_checkpoint() {
                Ok(true) => {
                    a.toast("Undone");
                    a.refresh();
                }
                Ok(false) => {}
                Err(e) => a.toast(&format!("Couldn't undo that: {e}")),
            }),
        );
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
        d.connect_response(None, move |_, r| crate::shielded(|| {
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
        }));
        d.present(Some(&self.win));
    }
}

/// Run a callback so a bug in it shows a message instead of closing the app: a panic in a GTK callback
/// aborts. The panic hook has already written crash.log.
pub fn guard(a: &App, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        a.toast("Something went wrong. Details are in crash.log in the data folder.");
    }
}

thread_local! {
    /// The window's app, for `shielded` (dialog callbacks that don't carry it).
    static APP: RefCell<std::rc::Weak<App>> = RefCell::default();
}

/// `guard` for callbacks without the app at hand (dialog responses).
pub fn shielded(f: impl FnOnce()) {
    if let Some(a) = APP.with(|w| w.borrow().upgrade()) {
        guard(&a, f);
    } else {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }
}

/// `glib::spawn_future_local`, but a panic inside shows a message instead of closing the app.
pub fn spawn_guarded(w: std::rc::Weak<App>, f: impl std::future::Future<Output = ()> + 'static) {
    let mut f = Box::pin(f);
    glib::spawn_future_local(std::future::poll_fn(move |cx| match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.as_mut().poll(cx))) {
        Ok(p) => p,
        Err(_) => {
            if let Some(a) = w.upgrade() {
                a.toast("Something went wrong. Details are in crash.log in the data folder.");
            }
            std::task::Poll::Ready(())
        }
    }));
}

/// Small helpers shared by the views.
pub fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(["title-4"]).build()
}

/// A title with an ⓘ after it: hover to read how to read what's below, click to keep it open.
pub fn with_info(title: &impl IsA<gtk::Widget>, help: &str) -> gtk::Box {
    let b = gtk::Box::builder().spacing(6).build();
    // a long title wraps rather than widening its card past the window
    if let Some(l) = title.dynamic_cast_ref::<gtk::Label>() {
        l.set_wrap(true);
        l.set_hexpand(true);
    }
    b.append(title);
    let text = gtk::Label::builder().label(help).wrap(true).max_width_chars(48).xalign(0.0).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let pop = gtk::Popover::builder().child(&text).build();
    b.append(&gtk::MenuButton::builder().icon_name("help-about-symbolic").popover(&pop).tooltip_text(help).valign(gtk::Align::Center).css_classes(["flat", "circular", "info-button"]).build());
    b
}

/// A section heading with its ⓘ.
pub fn info_heading(text: &str, help: &str) -> gtk::Box {
    with_info(&heading(text), help)
}

/// The top of a summary page (Net Worth, Outlook, Budget), as Overview does it: one card, the page's
/// answer large on the left (two fifths), the other numbers quiet beside it. Returns the card and,
/// for each number, its (value, line below) labels: the lead first.
pub fn summary_row(lead: (&str, &str), rest: &[(&str, &str)]) -> (gtk::Grid, Vec<(gtk::Label, gtk::Label)>) {
    let grid = gtk::Grid::builder().column_spacing(24).column_homogeneous(true).css_classes(["card", "summary"]).build();
    let cell = |title: &str, help: &str, class: &str| {
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).css_classes([class]).build();
        b.append(&info_caption(title, help));
        let v = gtk::Label::builder().xalign(0.0).css_classes(["value", "tnum"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let s = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
        b.append(&v);
        b.append(&s);
        (b, (v, s))
    };
    let (b, first) = cell(lead.0, lead.1, "lead");
    grid.attach(&b, 0, 0, 2, 1);
    let mut out = vec![first];
    for (i, (t, h)) in rest.iter().enumerate() {
        let (b, n) = cell(t, h, "stat");
        grid.attach(&b, 2 + i as i32, 0, 1, 1);
        out.push(n);
    }
    (grid, out)
}

/// A small card title (the KPI tiles) with its ⓘ.
pub fn info_caption(text: &str, help: &str) -> gtk::Box {
    with_info(&gtk::Label::builder().label(text).xalign(0.0).css_classes(["dim-label", "caption-heading"]).build(), help)
}

pub fn dim(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["dim-label"]).build()
}

pub fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

/// The tile for a look; none (Uncategorized) is a plain grey one.
pub fn tile_for(look: Option<&fingraph_core::looks::Look>) -> gtk::Box {
    let (icon, color) = look.map_or(("dialog-question-symbolic", "slate"), |l| (l.icon.as_str(), l.color.as_str()));
    let b = gtk::Box::builder().css_classes(["cat-tile", &format!("cat-{color}")]).halign(gtk::Align::Center).valign(gtk::Align::Center).hexpand(false).build();
    b.append(&gtk::Image::builder().icon_name(icon).pixel_size(16).hexpand(true).halign(gtk::Align::Center).valign(gtk::Align::Center).build());
    b
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
         .kpi.headline { padding: 18px 20px; }
         .kpi.headline .value { font-size: 2.6em; }
         .stat-strip { padding: 14px 20px; }
         .summary { padding: 18px 20px; }
         .summary .lead .value { font-size: 2.4em; font-weight: 800; }
         .stat .value { font-size: 1.2em; font-weight: 700; }
         .verified { color: @success_color; }
         .verified image { -gtk-icon-size: 12px; }
         .small-toggle { min-height: 0; padding: 2px 10px; font-size: 0.85em; }
         button.row-badge { min-height: 0; padding: 1px 8px; border-radius: 99px; font-size: 0.8em; font-weight: 600; background: alpha(@window_fg_color, 0.08); }
         label.row-badge { padding: 1px 8px; border-radius: 99px; font-size: 0.8em; font-weight: 600; }
         .row-badge.needs { background: alpha(@warning_bg_color, 0.18); color: @warning_color; }
         button.row-badge.business { background: alpha(@accent_bg_color, 0.18); color: @accent_color; }
         .chart-card { padding: 12px; }
         .card { border-radius: 14px; }
         .info-button { min-width: 22px; min-height: 22px; padding: 0; opacity: 0.28; }
         .card:hover .info-button, .info-button:focus-visible { opacity: 0.7; }
         .info-button:hover, .info-button:checked { opacity: 1; }
         .cat-tile { min-width: 28px; min-height: 28px; border-radius: 8px; }
         .cat-choice { min-width: 34px; min-height: 34px; padding: 0; border-radius: 9px; }
         .proven { color: @success_color; font-weight: 600; }
         .hero-value { font-size: 2.9em; font-weight: 800; }
         .column-head { font-size: 0.85em; opacity: 0.65; }
         .amount-out { color: @error_color; font-feature-settings: 'tnum'; }
         .amount-in { color: @success_color; font-feature-settings: 'tnum'; }
         levelbar.over block.filled { background-color: @error_color; }
         levelbar block.near { background-color: @warning_color; }
         .kind-pill { border-radius: 99px; padding: 2px 10px; min-height: 0; font-size: 0.8em; font-weight: 600; }
         .tnum { font-feature-settings: 'tnum'; }
         .chip { border-radius: 99px; padding: 4px 12px; }
         .guide-num { min-width: 24px; min-height: 24px; border-radius: 99px; font-weight: 700; font-size: 0.9em; background: alpha(@window_fg_color, 0.10); }
         .guide-num.done { background: alpha(@success_color, 0.20); color: @success_color; }
         .guide-card { padding: 18px; }
         button.cell { padding: 2px 6px; min-height: 0; font-weight: normal; }
         .bank-card { padding: 12px; min-width: 150px; }",
    );
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        // fixed and variable keep the hues of the Fixed vs Variable bar (blue, orange) in a shade that
        // reads as text on either background, swapped when the theme changes
        let kinds = gtk::CssProvider::new();
        let load = |p: &gtk::CssProvider, dark: bool| {
            let (blue, orange) = if dark { ("#62a0ea", "#ffa348") } else { ("#1c71d8", "#c64600") };
            let tiles: String = fingraph_core::looks::COLORS.iter().map(|(n, on_dark, on_light, _)| {
                let c = if dark { on_dark } else { on_light };
                format!(".cat-{n} {{ background: alpha({c}, 0.18); color: {c}; }}\n")
            }).collect();
            p.load_from_string(&format!(
                ".kind-fixed {{ background: alpha({blue}, 0.16); color: {blue}; }}
                 .kind-variable {{ background: alpha({orange}, 0.16); color: {orange}; }}
                 .amount-fixed {{ color: {blue}; }}
                 .amount-variable {{ color: {orange}; }}
                 {tiles}"
            ));
        };
        let sm = adw::StyleManager::default();
        load(&kinds, sm.is_dark());
        let k2 = kinds.clone();
        sm.connect_dark_notify(move |sm| load(&k2, sm.is_dark()));
        gtk::style_context_add_provider_for_display(&d, &kinds, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
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
    // FINGRAPH_EXPORT=plan:/tmp/p.pdf (or card:…, ics:…): write it without asking where
    if let Some((what, path)) = std::env::var("FINGRAPH_EXPORT").ok().as_deref().and_then(|s| s.split_once(':')) {
        planpdf::export_debug(app, what, path.into());
    }
    // FINGRAPH_FOLD=1: press both sidebar folds, as a click would
    if std::env::var_os("FINGRAPH_FOLD").is_some() {
        app.accounts_row.emit_activate();
        app.statements_arrow.emit_clicked();
    }
    // FINGRAPH_THEME=1: press the light/dark button
    if std::env::var_os("FINGRAPH_THEME").is_some() {
        app.theme_btn.emit_clicked();
    }
    match std::env::var("FINGRAPH_DIALOG").as_deref() {
        Ok("triage") => wizard::triage(app),
        Ok("onboarding") => guide::open_at(app, 0),
        Ok(d) if d.starts_with("guide:") => guide::open_at(app, d[6..].parse().unwrap_or(0)),
        Ok("report") => statements::open_first_report(app),
        Ok(d) if d.starts_with("split:") => split::dialog(app, d[6..].parse().unwrap_or(0)),
        Ok(d) if d.starts_with("owed:") => owed::debug_open(app, d[5..].parse().unwrap_or(0)),
        Ok("budget") => budget::wizard(app, Rc::new(|| {})),
        Ok("close") => close::open(app, None, Rc::new(|| {})),
        Ok(d) if d.starts_with("tx:") => txedit::dialog(app, d[3..].parse().unwrap_or(0), None),
        Ok("share") => share::open(app),
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
    let typed = Cell::new(false);
    let clicked = Cell::new(0usize);
    let budget_edited = Cell::new(false);
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
        // FINGRAPH_CLICK=label[,label…]: then click buttons with those labels, one per tick
        if let Some(label) = std::env::var("FINGRAPH_CLICK").ok().and_then(|l| l.split(',').nth(clicked.get()).map(str::to_string)) {
            clicked.set(clicked.get() + 1);
            fn find_button(w: &gtk::Widget, label: &str) -> Option<gtk::Button> {
                if let Some(b) = w.downcast_ref::<gtk::Button>().filter(|b| b.is_mapped() && (b.label().as_deref() == Some(label) || b.child().and_downcast::<gtk::Label>().is_some_and(|l| l.label() == label) || b.child().and_downcast::<adw::ButtonContent>().is_some_and(|c| c.label() == label) || b.tooltip_text().as_deref() == Some(label))) {
                    return Some(b.clone());
                }
                let mut c = w.first_child();
                while let Some(ch) = c {
                    if let Some(x) = find_button(&ch, label) {
                        return Some(x);
                    }
                    c = ch.next_sibling();
                }
                None
            }
            if let Some(b) = find_button(&a.win.clone().upcast(), &label) {
                b.emit_clicked();
            }
            return glib::ControlFlow::Continue;
        }
        // FINGRAPH_TYPE=text: type into the open popover's search box (the category picker)
        if let Some(text) = std::env::var("FINGRAPH_TYPE").ok().filter(|_| !typed.replace(true)) {
            fn find_search(w: &gtk::Widget) -> Option<gtk::SearchEntry> {
                if let Some(e) = w.downcast_ref::<gtk::SearchEntry>().filter(|e| e.is_mapped() && e.ancestor(gtk::Popover::static_type()).is_some()) {
                    return Some(e.clone());
                }
                let mut c = w.first_child();
                while let Some(ch) = c {
                    if let Some(x) = find_search(&ch) {
                        return Some(x);
                    }
                    c = ch.next_sibling();
                }
                None
            }
            if let Some(e) = find_search(&a.win.clone().upcast()) {
                e.set_text(&text);
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
                // FINGRAPH_SCROLL=0.5 scrolls halfway; any other value to the bottom
                let frac = std::env::var("FINGRAPH_SCROLL").ok().and_then(|v| v.parse::<f64>().ok()).filter(|f| *f < 1.0).unwrap_or(1.0);
                adj.set_value(adj.upper() * frac);
            }
            return glib::ControlFlow::Continue;
        }
        // Debug-only end-to-end check: edit a real budget field, then leave it without Enter.
        // FINGRAPH_BUDGET_EDIT=Groceries=725 (or Groceries=invalid for validation).
        if let Ok(edit) = std::env::var("FINGRAPH_BUDGET_EDIT") {
            if !budget_edited.replace(true) {
                fn budget_entry(w: &gtk::Widget, name: &str) -> Option<gtk::Entry> {
                    if let Some(entry) = w.downcast_ref::<gtk::Entry>().filter(|e| e.widget_name() == name) { return Some(entry.clone()); }
                    let mut c = w.first_child();
                    while let Some(child) = c {
                        if let Some(entry) = budget_entry(&child, name) { return Some(entry); }
                        c = child.next_sibling();
                    }
                    None
                }
                if let Some((category, value)) = edit.split_once('=') {
                    if let Some(entry) = budget_entry(a.win.upcast_ref(), &format!("budget:{category}")) {
                        eprintln!("Budget smoke test acquired focus: {}", entry.grab_focus());
                        entry.set_text(value);
                        gtk::prelude::GtkWindowExt::set_focus(&a.win, None::<&gtk::Widget>);
                    } else { eprintln!("Budget smoke test: category field not found"); }
                }
                return glib::ControlFlow::Continue;
            }
        }
        let paint = gtk::WidgetPaintable::new(Some(&a.win));
        let snap = gtk::Snapshot::new();
        paint.snapshot(&snap, a.win.width() as f64, a.win.height() as f64);
        // an open popover is its own surface: draw it over the window where it sits
        fn open_popover(w: &gtk::Widget) -> Option<gtk::Popover> {
            if let Some(p) = w.downcast_ref::<gtk::Popover>().filter(|p| p.is_visible()) {
                return Some(p.clone());
            }
            let mut c = w.first_child();
            while let Some(ch) = c {
                if let Some(x) = open_popover(&ch) {
                    return Some(x);
                }
                c = ch.next_sibling();
            }
            None
        }
        if let Some(p) = open_popover(&a.win.clone().upcast())
            && let Some(at) = p.compute_point(&a.win, &gtk::graphene::Point::new(0.0, 0.0))
        {
            snap.save();
            snap.translate(&at);
            gtk::WidgetPaintable::new(Some(&p)).snapshot(&snap, p.width() as f64, p.height() as f64);
            snap.restore();
        }
        if let (Some(node), Some(r)) = (snap.to_node(), a.win.renderer()) {
            let _ = r.render_texture(node, None).save_to_png(&out);
        }
        a.win.application().unwrap().quit();
        glib::ControlFlow::Break
    });
}

fn main() -> glib::ExitCode {
    // keep a record of any crash: the window just closes (or shows "Not Responding") otherwise
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log = fingraph_core::data_dir().join("crash.log");
        let entry = format!("── {} FinGraph {}\n{info}\n{}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), env!("CARGO_PKG_VERSION"), std::backtrace::Backtrace::force_capture());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            use std::io::Write;
            let _ = f.write_all(entry.as_bytes());
        }
        default_hook(info);
    }));
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
