//! FinGraph: a local-first ledger for Cayman bank statements. Drop PDFs in, the 5-pass scanner reads
//! and proves them, Smart Triage learns your merchants, and the dashboard shows where money goes.

mod charts;
mod dashboard;
mod export;
mod profiles;
mod rules;
mod statements;
mod transactions;
mod wizard;

use adw::prelude::*;
use chrono::{Datelike, Local, NaiveDate};
use fingraph_core::db::{Category, Db, Entity, TxFilter};
use fingraph_core::insights::{self, Range};
use fingraph_core::money::fmt_money;
use fingraph_core::{Currency, Decimal};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

const APP_ID: &str = "dev.fingraph.FinGraph";

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
    triage_btn: gtk::Button,
    kyd_btn: gtk::ToggleButton,
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
        let workspace_btn = gtk::MenuButton::builder().tooltip_text("Workspace").always_show_arrow(true).build();
        let kyd_btn = gtk::ToggleButton::builder().label("KYD").active(true).tooltip_text("Show amounts in Cayman dollars").build();
        let usd_btn = gtk::ToggleButton::builder().label("USD").group(&kyd_btn).tooltip_text("Show amounts in US dollars").build();
        let ccy_box = gtk::Box::builder().css_classes(["linked"]).build();
        ccy_box.append(&kyd_btn);
        ccy_box.append(&usd_btn);
        let range_dd = gtk::DropDown::builder().tooltip_text("Date range").build();
        let triage_btn = gtk::Button::builder().label("Triage").css_classes(["suggested-action"]).tooltip_text("Smart Triage: categorize new merchants").visible(false).build();
        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Import Statements…"), Some("win.import"));
        s1.append(Some("Rescan All Statements…"), Some("win.rescan"));
        s1.append(Some("Reset Learning & Re-triage…"), Some("win.retriage"));
        s1.append(Some("Load Demo Data (Cayman Personal + Business across Butterfield, Scotiabank, CNB, RBC & Credit Union)"), Some("win.demo"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Workspaces…"), Some("win.workspaces"));
        s2.append(Some("Exchange Rate…"), Some("win.fx"));
        s2.append(Some("Welcome Tour"), Some("win.welcome"));
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("About FinGraph"), Some("win.about"));
        menu.append_section(None, &s3);
        let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).tooltip_text("Main Menu").build();

        let content_header = adw::HeaderBar::builder().title_widget(&content_title).build();
        content_header.pack_start(&workspace_btn);
        content_header.pack_start(&ccy_box);
        content_header.pack_start(&range_dd);
        content_header.pack_end(&menu_btn);
        content_header.pack_end(&triage_btn);
        let content_tv = adw::ToolbarView::new();
        content_tv.add_top_bar(&content_header);
        content_tv.set_content(Some(&stack));
        let content = adw::NavigationPage::builder().title("FinGraph").child(&content_tv).build();

        // ── sidebar
        let side_list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
        let side_tv = adw::ToolbarView::new();
        side_tv.add_top_bar(&adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("FinGraph", "")).build());
        side_tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&side_list).vexpand(true).build()));
        let sidebar = adw::NavigationPage::builder().title("FinGraph").child(&side_tv).build();
        let split = adw::NavigationSplitView::builder().sidebar(&sidebar).content(&content).min_sidebar_width(220.0).max_sidebar_width(260.0).build();
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
            triage_btn,
            kyd_btn,
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

        // views
        let pages: [(&'static str, &str, &str, gtk::Widget, Refresh); 6] = {
            let (d, dr) = dashboard::build(&app);
            let (s, sr) = statements::build(&app);
            let (t, tr) = transactions::build(&app);
            let (r, rr) = rules::build(&app);
            let (p, pr) = profiles::build(&app);
            let (e, er) = export::build(&app);
            [
                ("dashboard", "Dashboard & Insights", "view-grid-symbolic", d, dr),
                ("statements", "Statements & Inspector", "x-office-document-symbolic", s, sr),
                ("transactions", "Transactions", "view-list-symbolic", t, tr),
                ("rules", "Subscriptions & Rules", "media-playlist-repeat-symbolic", r, rr),
                ("profiles", "Bank Profiles", "preferences-system-symbolic", p, pr),
                ("export", "Export & Reports", "document-save-symbolic", e, er),
            ]
        };
        for (name, title, icon, w, refresh) in pages {
            app.stack.add_titled(&w, Some(name), title);
            let row = gtk::Box::builder().spacing(12).margin_top(6).margin_bottom(6).margin_start(6).build();
            row.append(&gtk::Image::from_icon_name(icon));
            row.append(&gtk::Label::new(Some(title)));
            let lr = gtk::ListBoxRow::builder().child(&row).name(name).build();
            side_list.append(&lr);
            app.views.borrow_mut().push((name, refresh));
        }
        {
            let w = Rc::downgrade(&app);
            side_list.connect_row_selected(move |_, row| {
                if let (Some(a), Some(row)) = (w.upgrade(), row) {
                    a.show_view(&row.widget_name());
                }
            });
        }
        side_list.select_row(side_list.row_at_index(0).as_ref());

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
        {
            let w = Rc::downgrade(&app);
            app.triage_btn.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    wizard::triage(&a);
                }
            });
        }
        app.actions();
        app.reload_entities();
        app.win.present();

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
        self.split.set_show_content(true);
        self.refresh();
    }

    /// Re-read the ledger into the visible view and the header.
    pub fn refresh(self: &Rc<Self>) {
        let n = self.db.uncategorized_count(&self.entity_ids()).unwrap_or(0);
        self.triage_btn.set_visible(n > 0);
        self.triage_btn.set_label(&format!("Triage ({n})"));
        self.content_title.set_subtitle(&self.range_label());
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
        app.show_view(&v);
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
            if let Some(d) = dlg
                && let Some(b) = find(d.upcast_ref(), &|w| w.downcast_ref::<gtk::Button>().filter(|b| b.has_css_class("suggested-action") && b.is_mapped()).cloned())
            {
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
