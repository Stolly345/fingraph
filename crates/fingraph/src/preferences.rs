//! Preferences: General, Money, Import & Scanning, Triage & Rules, Dashboard, Workspaces & Accounts,
//! Data & Privacy. Every control saves as soon as it changes.

use adw::prelude::*;
use fingraph_core::db::TxFilter;
use fingraph_core::insights::InsightKind;
use fingraph_core::prefs::Prefs;
use fingraph_core::{Currency, Decimal, EntityKind};
use gtk::{gio, glib};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::App;

pub const RANGES: [&str; 6] = ["This Month", "Last Month", "Last 90 Days", "Year to Date", "This Year", "All Time"];

/// Change one preference and save it.
fn set(app: &Rc<App>, f: impl FnOnce(&mut Prefs)) {
    let mut p = app.db.prefs();
    f(&mut p);
    if let Err(e) = app.db.save_prefs(&p) {
        app.toast(&e.to_string());
    }
}

fn switch(app: &Rc<App>, title: &str, subtitle: &str, on: bool, f: impl Fn(&Rc<App>, &mut Prefs, bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(glib::markup_escape_text(title)).subtitle(glib::markup_escape_text(subtitle)).active(on).build();
    let w = Rc::downgrade(app);
    row.connect_active_notify(move |r| {
        if let Some(a) = w.upgrade() {
            let v = r.is_active();
            set(&a, |p| f(&a, p, v));
            a.refresh();
        }
    });
    row
}

/// Every currency, as pickers list them (`GBP — Pound sterling`), KYD and USD first.
pub fn currency_labels() -> Vec<String> {
    Currency::all().map(Currency::label).collect()
}

/// A picker's position for `c`, and the currency at a picker's position.
pub fn currency_index(c: Currency) -> u32 {
    Currency::all().position(|x| x == c).unwrap_or(0) as u32
}

pub fn currency_at(i: u32) -> Currency {
    Currency::all().nth(i as usize).unwrap_or(Currency::KYD)
}

/// A picker of every currency, `c` selected.
pub fn currency_row(title: &str, c: Currency) -> adw::ComboRow {
    let labels = currency_labels();
    adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())).selected(currency_index(c)).build()
}

fn combo(title: &str, subtitle: &str, items: &[String], selected: usize) -> adw::ComboRow {
    adw::ComboRow::builder()
        .title(title)
        .subtitle(subtitle)
        .model(&gtk::StringList::new(&items.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(selected as u32)
        .build()
}

fn spin(title: &str, subtitle: &str, lo: f64, hi: f64, value: f64) -> adw::SpinRow {
    adw::SpinRow::builder().title(glib::markup_escape_text(title)).subtitle(glib::markup_escape_text(subtitle)).adjustment(&gtk::Adjustment::new(value, lo, hi, 1.0, 5.0, 0.0)).build()
}

fn size_of(p: &Path) -> u64 {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|d| d.flatten().map(|e| size_of(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} bytes"),
    }
}

fn open_folder(app: &Rc<App>, dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(Some(&app.win), gio::Cancellable::NONE, |_| {});
}

fn button(label: &str) -> gtk::Button {
    gtk::Button::builder().label(label).valign(gtk::Align::Center).css_classes(["flat"]).build()
}

/// Ask, then (after an automatic backup) do something drastic.
pub fn confirm(app: &Rc<App>, title: &str, body: &str, verb: &str, then: impl Fn(&Rc<App>) + 'static) {
    let d = adw::AlertDialog::new(Some(title), Some(body));
    d.add_responses(&[("cancel", "Cancel"), ("go", verb)]);
    d.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| crate::shielded(|| {
        if let (Some(a), "go") = (w.upgrade(), r) {
            then(&a);
        }
    }));
    d.present(Some(&app.win));
}

pub fn show(app: &Rc<App>) {
    show_at(app, "general");
}

pub(crate) fn show_at(app: &Rc<App>, page: &str) {
    let dlg = adw::PreferencesDialog::builder().search_enabled(true).build();
    dlg.add(&general(app));
    dlg.add(&money(app));
    dlg.add(&import(app));
    dlg.add(&triage(app));
    dlg.add(&dashboard(app));
    dlg.add(&workspaces(app, &dlg));
    dlg.add(&data(app, &dlg));
    dlg.add(&server_page(app, &dlg));
    dlg.set_visible_page_name(page);
    dlg.present(Some(&app.win));
}

// ──────────────────────────────────────────────────────────────── Server

fn server_page(app: &Rc<App>, dlg: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().name("server").title("Server").icon_name("network-server-symbolic").build();
    let g = adw::PreferencesGroup::builder()
        .title("FinGraph Server")
        .description("Keep the ledger and your statement files on a FinGraph server (a Docker container on a machine that's always on), so the phone app and your other computers use the same ledger. One device edits at a time; this computer works on its own copy and sends it back after imports, after ten quiet minutes and on closing.")
        .build();
    page.add(&g);
    // reopened to show the new state after connecting or disconnecting
    let reopen = {
        let dlg = dlg.clone();
        Rc::new(move |a: &Rc<App>| {
            dlg.close();
            show_at(a, "server");
        })
    };
    match crate::server::describe() {
        None => {
            let how = adw::ActionRow::builder().title("To pair this computer").subtitle("On the server, run  docker exec fingraph fingraph-server pair  and type its address and the code it shows.").subtitle_selectable(true).build();
            let addr = adw::EntryRow::builder().title("Server address (192.168.1.20:8743)").build();
            let code = adw::EntryRow::builder().title("Pairing code").build();
            let go = adw::ButtonRow::builder().title("Connect").start_icon_name("network-transmit-receive-symbolic").build();
            g.add(&how);
            g.add(&addr);
            g.add(&code);
            g.add(&go);
            let w = Rc::downgrade(app);
            go.connect_activated(move |_| {
                let Some(a) = w.upgrade() else { return };
                let (address, c) = (addr.text().trim().to_string(), code.text().trim().to_string());
                if address.is_empty() || c.is_empty() {
                    return a.toast("Type the server's address and the pairing code");
                }
                let reopen = reopen.clone();
                crate::server::connect(&a, address, c, Rc::new(move |a: &Rc<App>| {
                    a.toast("Connected to FinGraph Server");
                    reopen(a);
                }));
            });
        }
        Some((address, fingerprint, state)) => {
            g.add(&adw::ActionRow::builder().title(address.as_str()).subtitle(format!("Fingerprint {fingerprint}")).subtitle_selectable(true).build());
            g.add(&adw::ActionRow::builder().title("Status").subtitle(state.as_str()).build());
            let actions = adw::PreferencesGroup::new();
            page.add(&actions);
            let sync = adw::ButtonRow::builder().title("Sync Now").start_icon_name("view-refresh-symbolic").build();
            let send = adw::ButtonRow::builder().title("Send This Computer’s Ledger…").build();
            let fetch = adw::ButtonRow::builder().title("Use the Server’s Ledger…").build();
            let leave = adw::ButtonRow::builder().title("Disconnect…").build();
            leave.add_css_class("destructive-action");
            for r in [&sync, &send, &fetch, &leave] {
                actions.add(r);
            }
            let w = Rc::downgrade(app);
            sync.connect_activated(move |_| {
                if let Some(a) = w.upgrade() {
                    crate::server::sync_in(&a, false);
                }
            });
            let w = Rc::downgrade(app);
            send.connect_activated(move |_| {
                if let Some(a) = w.upgrade() {
                    confirm(&a, "Send This Computer’s Ledger?", "The ledger on the server is replaced by this computer’s, statements and receipts included. The server keeps a backup of what it had.", "Send", crate::server::send_ledger);
                }
            });
            let w = Rc::downgrade(app);
            fetch.connect_activated(move |_| {
                if let Some(a) = w.upgrade() {
                    confirm(&a, "Use the Server’s Ledger?", "This computer’s ledger is replaced by the server’s. It’s backed up first when automatic backups are on.", "Replace", crate::server::use_server_ledger);
                }
            });
            let w = Rc::downgrade(app);
            leave.connect_activated(move |_| {
                if let Some(a) = w.upgrade() {
                    let reopen = reopen.clone();
                    confirm(&a, "Disconnect From the Server?", "This computer stops using FinGraph Server and keeps its own ledger as it is now. The server keeps its copy.", "Disconnect", move |a| {
                        crate::server::disconnect(a);
                        reopen(a);
                    });
                }
            });
        }
    }
    page
}

// ──────────────────────────────────────────────────────────────── General

fn general(app: &Rc<App>) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("general").title("General").icon_name("preferences-system-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("When FinGraph Opens").build();
    let ents = app.entities.borrow().clone();
    let mut opts = vec!["Where I left off".to_string(), "Consolidated Overview".to_string()];
    opts.extend(ents.iter().map(|e| e.name.clone()));
    let sel = match p.start_workspace.as_str() {
        "last" => 0,
        "consolidated" => 1,
        id => ents.iter().position(|e| e.id.to_string() == id).map_or(0, |i| i + 2),
    };
    let ws = combo("Workspace", "", &opts, sel);
    {
        let (w, ents) = (Rc::downgrade(app), ents.clone());
        ws.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                let v = match c.selected() {
                    0 => "last".to_string(),
                    1 => "consolidated".to_string(),
                    i => ents.get(i as usize - 2).map_or("last".into(), |e| e.id.to_string()),
                };
                set(&a, |p| p.start_workspace = v);
            }
        });
    }
    let ranges: Vec<String> = RANGES.iter().map(|s| s.to_string()).collect();
    let range = combo("Date range", "", &ranges, RANGES.iter().position(|r| *r == p.default_range).unwrap_or(4));
    {
        let w = Rc::downgrade(app);
        range.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                set(&a, |p| p.default_range = RANGES[c.selected() as usize].into());
            }
        });
    }
    let ccy = combo("Display currency", "The currency menu in the header changes it any time", &currency_labels(), currency_index(app.display()) as usize);
    {
        let w = Rc::downgrade(app);
        ccy.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                a.set_display(currency_at(c.selected()));
            }
        });
    }
    g.add(&ws);
    g.add(&range);
    g.add(&ccy);
    g.add(&switch(app, "Check for updates", "Once a day, ask GitHub whether a newer version is out. Nothing about you or your ledger is sent", p.check_updates, |_, p, v| p.check_updates = v));
    page.add(&g);

    let look = adw::PreferencesGroup::builder().title("Appearance").build();
    let scheme = combo("Style", "", &["Follow system".into(), "Light".into(), "Dark".into()], ["system", "light", "dark"].iter().position(|s| *s == p.appearance).unwrap_or(0));
    {
        let w = Rc::downgrade(app);
        scheme.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                let v = ["system", "light", "dark"][c.selected() as usize];
                set(&a, |p| p.appearance = v.into());
                apply_appearance(v);
            }
        });
    }
    look.add(&scheme);
    page.add(&look);
    page
}

pub fn apply_appearance(v: &str) {
    adw::StyleManager::default().set_color_scheme(match v {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
}

// ──────────────────────────────────────────────────────────────── Money

fn money(app: &Rc<App>) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("money").title("Money").icon_name("x-office-spreadsheet-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("Exchange Rate").description("Used to show KYD amounts in USD and the other way round. The Cayman peg is 1.20.").build();
    let fx = app.db.fx().unwrap_or_default();
    let rate = adw::EntryRow::builder().title("US dollars per 1 Cayman dollar").text(fx.kyd_to_usd.to_string()).show_apply_button(true).build();
    {
        let w = Rc::downgrade(app);
        rate.connect_apply(move |r| {
            let Some(a) = w.upgrade() else { return };
            match r.text().trim().parse::<Decimal>().map_err(|e| e.to_string()).and_then(|v| a.db.set_fx(v).map_err(|e| e.to_string())) {
                Ok(()) => {
                    a.toast("Exchange rate saved; amounts revalued");
                    a.refresh();
                }
                Err(e) => a.toast(&format!("Couldn't save the rate: {e}")),
            }
        });
    }
    g.add(&rate);
    g.add(&switch(
        app,
        "Statement rates win",
        "A rate printed on the statement (“USD 145.00 @ 0.84”, “(59.14 KYD)”) is used for that charge instead of the rate above",
        p.statement_fx_wins,
        |a, p, v| {
            p.statement_fx_wins = v;
            fingraph_core::money::set_style(p.currency_codes, p.brackets, v);
            a.saved(a.db.revalue_all());
        },
    ));
    page.add(&g);
    page.add(&rates_group(app, &p));
    let d = adw::PreferencesGroup::builder().title("How Amounts Look").build();
    d.add(&switch(app, "Currency codes", "KYD 1,250.00 instead of CI$ 1,250.00", p.currency_codes, |_, p, v| p.currency_codes = v));
    d.add(&switch(app, "Brackets for money out", "(CI$ 45.00) instead of -CI$ 45.00", p.brackets, |_, p, v| p.brackets = v));
    page.add(&d);
    page
}

// ──────────────────────────────────────────────────────────────── Import & Scanning

fn import(app: &Rc<App>) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("import").title("Import").icon_name("document-open-symbolic").build();

    let keep = adw::PreferencesGroup::builder().title("Statement Copies").description("Every imported PDF is kept here so it can be rescanned later.").build();
    let dir = app.db.statements_dir();
    let row = adw::ActionRow::builder().title(glib::markup_escape_text(&dir.display().to_string())).subtitle(human(size_of(&dir))).subtitle_selectable(true).build();
    let open = button("Open");
    let mv = button("Move…");
    row.add_suffix(&open);
    row.add_suffix(&mv);
    keep.add(&row);
    {
        let w = Rc::downgrade(app);
        open.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                open_folder(&a, &a.db.statements_dir());
            }
        });
    }
    {
        let (w, row) = (Rc::downgrade(app), row.clone());
        mv.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let row = row.clone();
            pick_folder(&a, "Move Statement Copies To", move |a, to| match a.db.move_statements_dir(&to) {
                Ok((n, to)) => {
                    row.set_title(&glib::markup_escape_text(&to.display().to_string()));
                    a.toast(&format!("Moved {n} statements"));
                }
                Err(e) => a.toast(&e.to_string()),
            });
        });
    }
    page.add(&keep);

    let watch = adw::PreferencesGroup::builder()
        .title("Watch a Folder")
        .description("New bank statements saved here (say, from your bank's website) are imported into the workspace you're viewing. Other PDFs are ignored.")
        .build();
    let folder = p.watch_folder.clone().unwrap_or_else(|| glib::user_special_dir(glib::UserDirectory::Downloads).unwrap_or_else(glib::home_dir));
    let on = switch(app, "Watch for new statements", "Checked when FinGraph starts and while it's open", p.watch_enabled, {
        let folder = folder.clone();
        move |a, p, v| {
            p.watch_enabled = v;
            p.watch_folder.get_or_insert(folder.clone());
            let _ = a;
        }
    });
    {
        let w = Rc::downgrade(app);
        on.connect_active_notify(move |_| {
            if let Some(a) = w.upgrade() {
                crate::statements::watch(&a);
            }
        });
    }
    let frow = adw::ActionRow::builder().title("Folder").subtitle(glib::markup_escape_text(&folder.display().to_string())).build();
    let choose = button("Choose…");
    frow.add_suffix(&choose);
    {
        let (w, frow) = (Rc::downgrade(app), frow.clone());
        choose.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let frow = frow.clone();
            pick_folder(&a, "Watch This Folder", move |a, dir| {
                frow.set_subtitle(&glib::markup_escape_text(&dir.display().to_string()));
                set(a, |p| p.watch_folder = Some(dir));
                crate::statements::watch(a);
            });
        });
    }
    watch.add(&on);
    watch.add(&frow);
    page.add(&watch);

    let after = adw::PreferencesGroup::builder().title("After Importing").build();
    after.add(&switch(app, "Open Smart Triage", "Go straight to new merchants instead of showing a notification", p.triage_after_import, |_, p, v| p.triage_after_import = v));
    let dup = spin("Double-charge window", "Days apart the same charge can be and still be flagged as a possible double charge", 0.0, 14.0, p.duplicate_days as f64);
    {
        let w = Rc::downgrade(app);
        dup.connect_value_notify(move |s| {
            if let Some(a) = w.upgrade() {
                set(&a, |p| p.duplicate_days = s.value() as i64);
            }
        });
    }
    after.add(&dup);
    page.add(&after);
    page
}

fn pick_folder(app: &Rc<App>, title: &str, then: impl Fn(&Rc<App>, PathBuf) + 'static) {
    let fd = gtk::FileDialog::builder().title(title).modal(true).build();
    let w = Rc::downgrade(app);
    fd.select_folder(Some(&app.win), gio::Cancellable::NONE, move |res| {
        if let (Some(a), Some(p)) = (w.upgrade(), res.ok().and_then(|f| f.path())) {
            then(&a, p);
        }
    });
}

// ──────────────────────────────────────────────────────────────── Triage & Rules

fn triage(app: &Rc<App>) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("triage").title("Triage").icon_name("media-playlist-repeat-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("Smart Triage").build();
    g.add(&switch(
        app,
        "Group shortened merchant names",
        "Banks cut names short differently on each statement (DISCOVERYDI HELP / HEL / HE): treat them as one merchant",
        p.fuzzy_merchants,
        |_, p, v| p.fuzzy_merchants = v,
    ));
    page.add(&g);
    let dir = adw::PreferencesGroup::builder()
        .title("Business Directory")
        .description(format!(
            "{} Cayman businesses and online services FinGraph recognises on sight (supermarkets, restaurants, insurers, gas stations, telecoms, garages…). Your own rules always come first.",
            fingraph_core::merchants::count()
        ))
        .build();
    dir.add(&switch(app, "File known businesses automatically", "Charges from a business in the directory are categorized on import, marked for review", p.auto_directory, |_, p, v| p.auto_directory = v));
    dir.add(&switch(
        app,
        "File keyword guesses too",
        "Words like MARKET (groceries), DIGITAL (software & subscriptions), INSURANCE or PHARMACY. Off: they're only suggested in To Review",
        p.auto_keywords,
        |_, p, v| p.auto_keywords = v,
    ));
    let now = adw::ButtonRow::builder().title("Apply to Uncategorized Transactions").start_icon_name("view-refresh-symbolic").build();
    {
        let w = Rc::downgrade(app);
        now.connect_activated(move |_| {
            if let Some(a) = w.upgrade() {
                match a.db.apply_directory() {
                    Ok(n) => a.toast(&format!("Filed {n} transaction{}", if n == 1 { "" } else { "s" })),
                    Err(e) => a.toast(&e.to_string()),
                }
                a.refresh();
            }
        });
    }
    dir.add(&now);
    let g = adw::PreferencesGroup::new();
    page.add(&dir);
    g.add(&switch(app, "Ask about subscriptions", "Offer “Is this a subscription?” for regular same-price charges", p.ask_subscriptions, |_, p, v| p.ask_subscriptions = v));
    let scope = combo("New rules apply to", "", &["The workspace the charge came from".into(), "All workspaces".into()], p.rules_all_workspaces as usize);
    {
        let w = Rc::downgrade(app);
        scope.connect_selected_notify(move |c| {
            if let Some(a) = w.upgrade() {
                set(&a, |p| p.rules_all_workspaces = c.selected() == 1);
            }
        });
    }
    g.add(&scope);
    g.add(&switch(
        app,
        "Rescan keeps my changes",
        "Categories, payees, tags and business flags you set by hand survive a rescan",
        p.rescan_keeps_edits,
        |_, p, v| p.rescan_keeps_edits = v,
    ));
    page.add(&g);

    let again = adw::PreferencesGroup::builder().title("Start Over").description("An automatic backup is made first when that's switched on (Data page).").build();
    let reapply = adw::ButtonRow::builder().title("Re-apply Learned Rules").start_icon_name("view-refresh-symbolic").build();
    {
        let w = Rc::downgrade(app);
        reapply.connect_activated(move |_| {
            if let Some(a) = w.upgrade() {
                match a.db.reapply_rules() {
                    Ok(n) => a.toast(&format!("Re-applied — {n} transactions updated")),
                    Err(e) => a.toast(&e.to_string()),
                }
                a.refresh();
            }
        });
    }
    let rescan = adw::ButtonRow::builder().title("Rescan All Statements…").start_icon_name("document-open-symbolic").action_name("win.rescan").build();
    let reset = adw::ButtonRow::builder().title("Reset Learning &amp; Re-triage…").start_icon_name("edit-clear-all-symbolic").action_name("win.retriage").css_classes(["destructive-action"]).build();
    again.add(&reapply);
    again.add(&rescan);
    again.add(&reset);
    page.add(&again);
    page
}

// ──────────────────────────────────────────────────────────────── Dashboard

fn dashboard(app: &Rc<App>) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("dashboard").title("Summary").icon_name("view-grid-symbolic").build();

    let cards = adw::PreferencesGroup::builder().title("What Changed? Cards").build();
    let hidden: Vec<String> = app.db.setting("hidden_insights").unwrap_or_default().split(',').map(str::to_string).collect();
    for kind in InsightKind::ALL {
        let key = format!("{kind:?}");
        let row = adw::SwitchRow::builder().title(glib::markup_escape_text(kind.label())).active(!hidden.contains(&key)).build();
        let w = Rc::downgrade(app);
        row.connect_active_notify(move |r| {
            let Some(a) = w.upgrade() else { return };
            let mut h: Vec<String> = a.db.setting("hidden_insights").unwrap_or_default().split(',').filter(|s| !s.is_empty() && *s != key).map(str::to_string).collect();
            if !r.is_active() {
                h.push(key.clone());
            }
            a.saved(a.db.set_setting("hidden_insights", &h.join(",")));
            a.refresh();
        });
        cards.add(&row);
    }
    page.add(&cards);

    let tune = adw::PreferencesGroup::builder().title("Sensitivity").build();
    type Number = (&'static str, &'static str, f64, f64, f64, fn(&mut Prefs, f64));
    let numbers: [Number; 4] = [
        ("Bill spike", "Percent above a bill's own average before it's flagged", 5.0, 200.0, p.spike_percent as f64, |p, v| p.spike_percent = v as i64),
        ("Category drift", "Percent above a category's 3-month average before it's flagged", 5.0, 200.0, p.drift_percent as f64, |p, v| p.drift_percent = v as i64),
        ("Months in the income & expenses chart", "", 3.0, 36.0, p.history_months as f64, |p, v| p.history_months = v as i32),
        ("Months in the spending-pace band", "This month is compared with this many months before it", 2.0, 12.0, p.band_months as f64, |p, v| p.band_months = v as i32),
    ];
    for (title, sub, lo, hi, val, f) in numbers {
        let s = spin(title, sub, lo, hi, val);
        let w = Rc::downgrade(app);
        s.connect_value_notify(move |s| {
            if let Some(a) = w.upgrade() {
                set(&a, |p| f(p, s.value()));
                a.refresh();
            }
        });
        tune.add(&s);
    }
    page.add(&tune);

    let ex = adw::PreferencesGroup::builder().title("Leave Out of Spending").description("Money out in these categories isn't counted as spending on the dashboard. Transfers never are.").build();
    let exp = adw::ExpanderRow::builder().title("Categories").subtitle(if p.excluded_categories.is_empty() { "None left out".to_string() } else { p.excluded_categories.join(", ") }).build();
    for c in app.categories().into_iter().filter(|c| c.parent_id.is_none() && c.kind == fingraph_core::db::CategoryKind::Expense) {
        let row = adw::SwitchRow::builder().title(glib::markup_escape_text(&c.name)).active(p.excluded_categories.contains(&c.name)).build();
        let (w, name, ex2) = (Rc::downgrade(app), c.name.clone(), exp.clone());
        row.connect_active_notify(move |r| {
            let Some(a) = w.upgrade() else { return };
            let on = r.is_active();
            set(&a, |p| {
                p.excluded_categories.retain(|x| *x != name);
                if on {
                    p.excluded_categories.push(name.clone());
                }
                ex2.set_subtitle(&if p.excluded_categories.is_empty() { "None left out".to_string() } else { p.excluded_categories.join(", ") });
            });
            a.refresh();
        });
        exp.add_row(&row);
    }
    ex.add(&exp);
    page.add(&ex);
    page
}

// ──────────────────────────────────────────────────────────────── Workspaces & Accounts

fn reopen(app: &Rc<App>, dlg: &adw::PreferencesDialog, page: &str) {
    dlg.close();
    show(app);
    if let Some(d) = app.win.visible_dialog().and_downcast::<adw::PreferencesDialog>() {
        d.set_visible_page_name(page);
    }
}

fn workspaces(app: &Rc<App>, dlg: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().name("workspaces").title("Workspaces").icon_name("avatar-default-symbolic").build();
    let ents = app.entities.borrow().clone();
    let g = adw::PreferencesGroup::builder().title("Workspaces").description("Each keeps its own accounts and P&amp;L. Changing a currency revalues its transactions.").build();
    for e in &ents {
        let exp = adw::ExpanderRow::builder().title(glib::markup_escape_text(&e.name)).subtitle(format!("{} · {}", if e.kind == EntityKind::Personal { "Personal" } else if e.kind == EntityKind::Business { "Business" } else { "Rental property" }, e.base_currency)).build();
        let name = adw::EntryRow::builder().title("Name").text(&e.name).show_apply_button(true).build();
        let ccy = combo("Currency", "", &currency_labels(), currency_index(e.base_currency) as usize);
        let del = adw::ButtonRow::builder().title("Delete Workspace…").css_classes(["destructive-action"]).build();
        {
            let (w, id, ccy) = (Rc::downgrade(app), e.id, ccy.clone());
            name.connect_apply(move |n| {
                if let Some(a) = w.upgrade() {
                    let c = currency_at(ccy.selected());
                    match a.db.update_entity(id, n.text().trim(), c) {
                        Ok(()) => a.reload_entities(),
                        Err(e) => a.toast(&e.to_string()),
                    }
                }
            });
        }
        {
            let (w, id, name) = (Rc::downgrade(app), e.id, name.clone());
            ccy.connect_selected_notify(move |c| {
                if let Some(a) = w.upgrade() {
                    if a.saved(a.db.update_entity(id, name.text().trim(), currency_at(c.selected()))) {
                        // the header and amounts follow the new currency; it needs that currency's rates
                        a.reload_entities();
                        a.update_rates(false);
                    }
                }
            });
        }
        {
            let (w, id, n, dlg) = (Rc::downgrade(app), e.id, e.name.clone(), dlg.clone());
            del.connect_activated(move |_| {
                let Some(a) = w.upgrade() else { return };
                let count = a.db.transactions(&TxFilter { entity_ids: vec![id], ..Default::default() }).map(|r| r.len()).unwrap_or(0);
                let dlg = dlg.clone();
                confirm(&a, &format!("Delete “{n}”?"), &format!("Its accounts, statements, {count} transactions and its rules are removed. A backup is made first when automatic backups are on."), "Delete", move |a| {
                    if let Err(e) = a.db.auto_backup("before-delete-workspace") {
                        return a.toast(&format!("Couldn't make the backup first, so nothing was deleted: {e}"));
                    }
                    match a.db.delete_entity(id) {
                        Ok(()) => a.toast("Workspace deleted"),
                        Err(e) => a.toast(&e.to_string()),
                    }
                    a.reload_entities();
                    reopen(a, &dlg, "workspaces");
                });
            });
        }
        exp.add_row(&name);
        exp.add_row(&ccy);
        exp.add_row(&del);
        g.add(&exp);
    }
    let add_name = adw::EntryRow::builder().title("Add a workspace (e.g. Business 2, Seven Mile Condo)").show_apply_button(true).build();
    let add_kind = combo("Kind of new workspace", "", &["Personal".into(), "Business".into(), "Rental property".into()], 1);
    {
        let (w, kind, dlg) = (Rc::downgrade(app), add_kind.clone(), dlg.clone());
        add_name.connect_apply(move |n| {
            let Some(a) = w.upgrade() else { return };
            let name = n.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let k = [EntityKind::Personal, EntityKind::Business, EntityKind::RentalProperty][kind.selected() as usize];
            match a.db.create_entity(&name, k, Currency::KYD) {
                Ok(_) => {
                    a.reload_entities();
                    reopen(&a, &dlg, "workspaces");
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    g.add(&add_name);
    g.add(&add_kind);
    page.add(&g);

    let ag = adw::PreferencesGroup::builder().title("Accounts").description("Created automatically from your statements.").build();
    for acct in app.db.accounts().unwrap_or_default() {
        let exp = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&acct.name))
            .subtitle(format!("{} · {}{}", ents.iter().find(|e| e.id == acct.entity_id).map_or("?", |e| e.name.as_str()), acct.currency, if acct.hidden { " · hidden" } else { "" }))
            .build();
        let name = adw::EntryRow::builder().title("Name").text(&acct.name).show_apply_button(true).build();
        let ws = combo("Workspace", "", &ents.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), ents.iter().position(|e| e.id == acct.entity_id).unwrap_or(0));
        let hide = adw::SwitchRow::builder().title("Closed account").subtitle("Hide it from lists and the balance chart; its history still counts").active(acct.hidden).build();
        let cur = currency_row("Currency", acct.currency);
        cur.set_subtitle("What its statements are in (read from them; put it right if a statement didn't say)");
        let bank = adw::EntryRow::builder().title("OFX bank id (routing / transit)").text(&acct.ofx_bank_id).show_apply_button(true).build();
        let bid = adw::EntryRow::builder().title("QuickBooks INTU.BID (3000 works for most)").text(&acct.ofx_intu_bid).show_apply_button(true).build();
        let save = {
            let (w, acct, name, ws, hide, bank, bid, ents, cur) = (Rc::downgrade(app), acct.clone(), name.clone(), ws.clone(), hide.clone(), bank.clone(), bid.clone(), ents.clone(), cur.clone());
            Rc::new(move || {
                let Some(a) = w.upgrade() else { return };
                let mut x = acct.clone();
                x.name = name.text().trim().to_string();
                x.entity_id = ents.get(ws.selected() as usize).map_or(x.entity_id, |e| e.id);
                x.hidden = hide.is_active();
                x.ofx_bank_id = bank.text().trim().to_string();
                x.ofx_intu_bid = bid.text().trim().to_string();
                x.currency = currency_at(cur.selected());
                match a.db.update_account(&x) {
                    Ok(()) => {
                        // another currency may need its rates
                        a.update_rates(false);
                        a.refresh();
                    }
                    Err(e) => a.toast(&e.to_string()),
                }
            })
        };
        for e in [&name, &bank, &bid] {
            let s = save.clone();
            e.connect_apply(move |_| s());
        }
        {
            let s = save.clone();
            ws.connect_selected_notify(move |_| s());
        }
        {
            let s = save.clone();
            hide.connect_active_notify(move |_| s());
        }
        {
            let s = save.clone();
            cur.connect_selected_notify(move |_| s());
        }
        exp.add_row(&name);
        exp.add_row(&ws);
        exp.add_row(&cur);
        exp.add_row(&hide);
        exp.add_row(&bank);
        exp.add_row(&bid);
        ag.add(&exp);
    }
    page.add(&ag);
    page
}

// ──────────────────────────────────────────────────────────────── Data & Privacy

fn data(app: &Rc<App>, dlg: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let p = app.db.prefs();
    let page = adw::PreferencesPage::builder().name("data").title("Data").icon_name("drive-harddisk-symbolic").build();
    let where_ = adw::PreferencesGroup::builder().title("Your Ledger").description("Your statements and ledger stay on this computer. Only two requests are ever sent: a daily check for a newer release on GitHub (Check for updates, General), and exchange rates (currency codes and dates), only with Download exchange rates on (Money).").build();
    let path = app.db.path().unwrap_or_default();
    let ledger = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&path.display().to_string()))
        .subtitle(human(size_of(&path) + size_of(&path.with_extension("db-wal"))))
        .subtitle_selectable(true)
        .build();
    let open = button("Open Folder");
    ledger.add_suffix(&open);
    {
        let (w, dir) = (Rc::downgrade(app), path.parent().map(Path::to_path_buf).unwrap_or_default());
        open.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                open_folder(&a, &dir);
            }
        });
    }
    where_.add(&ledger);
    page.add(&where_);

    let b = adw::PreferencesGroup::builder().title("Backups").build();
    b.add(&switch(
        app,
        "Back up before risky changes",
        "Includes ledger, statement copies and receipts before Reset, Rescan, deleting a workspace, restoring or erasing. The last ten are kept.",
        p.auto_backup,
        |_, p, v| p.auto_backup = v,
    ));
    let now = adw::ButtonRow::builder().title("Complete Backup (Ledger and Documents)…").start_icon_name("document-save-symbolic").build();
    let restore = adw::ButtonRow::builder().title("Restore from a Backup…").start_icon_name("document-open-recent-symbolic").build();
    let folder = adw::ButtonRow::builder().title("Open Backups Folder").start_icon_name("folder-documents-symbolic").build();
    {
        let w = Rc::downgrade(app);
        now.connect_activated(move |_| {
            let Some(a) = w.upgrade() else { return };
            let fd = gtk::FileDialog::builder().title("Complete Backup").initial_name(format!("fingraph-backup-{}.tar", crate::today())).modal(true).build();
            let w = Rc::downgrade(&a);
            fd.save(Some(&a.win), gio::Cancellable::NONE, move |res| {
                let (Some(a), Some(p)) = (w.upgrade(), res.ok().and_then(|f| f.path())) else { return };
                match a.db.backup_archive(&p) {
                    Ok(()) => a.toast(&format!("Backed up to {}", p.display())),
                    Err(e) => a.toast(&e.to_string()),
                }
            });
        });
    }
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        restore.connect_activated(move |_| {
            let Some(a) = w.upgrade() else { return };
            let mut fd = gtk::FileDialog::builder().title("Restore From Backup").modal(true);
            if let Some(dir) = a.db.backups_dir() {
                fd = fd.initial_folder(&gio::File::for_path(dir));
            }
            let (w, dlg) = (Rc::downgrade(&a), dlg.clone());
            fd.build().open(Some(&a.win), gio::Cancellable::NONE, move |res| {
                let (Some(a), Some(src)) = (w.upgrade(), res.ok().and_then(|f| f.path())) else { return };
                let dlg = dlg.clone();
                confirm(&a, "Restore this backup?", &format!("Everything in FinGraph is replaced by “{}”. Your current ledger is backed up first when automatic backups are on.", src.display()), "Restore", move |a| {
                    if let Err(e) = a.db.auto_backup("before-restore") {
                        return a.toast(&format!("Couldn't back up your current ledger first, so nothing was restored: {e}"));
                    }
                    match a.db.restore_from(&src) {
                        Ok(()) => {
                            a.toast("Restored");
                            dlg.close();
                            a.reload_entities();
                        }
                        Err(e) => a.toast(&format!("Couldn't restore: {e}")),
                    }
                });
            });
        });
    }
    {
        let w = Rc::downgrade(app);
        folder.connect_activated(move |_| {
            if let Some(a) = w.upgrade()
                && let Some(d) = a.db.backups_dir()
            {
                open_folder(&a, &d);
            }
        });
    }
    let database_only = adw::ButtonRow::builder().title("Database-Only Backup (No Documents)…").tooltip_text("Ledger and settings only; statement files and receipts are not included").build();
    {
        let w = Rc::downgrade(app);
        database_only.connect_activated(move |_| {
            let Some(a) = w.upgrade() else { return };
            let fd = gtk::FileDialog::builder().title("Database-Only Backup").initial_name(format!("fingraph-ledger-{}.db", crate::today())).build();
            let weak = Rc::downgrade(&a);
            fd.save(Some(&a.win), gio::Cancellable::NONE, move |result| {
                if let (Some(a), Some(path)) = (weak.upgrade(), result.ok().and_then(|f| f.path())) {
                    if a.saved(a.db.backup_to(&path)) { a.toast("Database saved; documents are not included"); }
                }
            });
        });
    }
    b.add(&database_only);
    b.add(&now);
    b.add(&restore);
    b.add(&folder);
    page.add(&b);

    let out = adw::PreferencesGroup::builder().title("Take Your Data With You").build();
    let all = adw::ButtonRow::builder().title("Export Every Transaction (CSV)…").start_icon_name("x-office-spreadsheet-symbolic").build();
    {
        let w = Rc::downgrade(app);
        all.connect_activated(move |_| {
            let Some(a) = w.upgrade() else { return };
            let mut rows = a.db.transactions(&TxFilter::default()).unwrap_or_default();
            rows.reverse();
            let content = fingraph_core::export::csv(&rows);
            let fd = gtk::FileDialog::builder().title("Export Everything").initial_name(format!("fingraph-all-{}.csv", crate::today())).modal(true).build();
            let w = Rc::downgrade(&a);
            fd.save(Some(&a.win), gio::Cancellable::NONE, move |res| {
                let (Some(a), Some(p)) = (w.upgrade(), res.ok().and_then(|f| f.path())) else { return };
                match std::fs::write(&p, &content) {
                    Ok(()) => a.toast(&format!("Saved {} transactions", rows.len())),
                    Err(e) => a.toast(&e.to_string()),
                }
            });
        });
    }
    out.add(&all);
    page.add(&out);

    let danger = adw::PreferencesGroup::builder().title("Erase").build();
    let erase = adw::ButtonRow::builder().title("Erase All Data…").css_classes(["destructive-action"]).build();
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        erase.connect_activated(move |_| {
            if let Some(a) = w.upgrade() {
                erase_dialog(&a, &dlg);
            }
        });
    }
    danger.add(&erase);
    page.add(&danger);
    page
}

fn erase_dialog(app: &Rc<App>, prefs: &adw::PreferencesDialog) {
    let typed = adw::EntryRow::builder().title("Type ERASE to confirm").build();
    let pdfs = adw::SwitchRow::builder().title("Also delete the kept statement PDFs").build();
    let g = adw::PreferencesGroup::new();
    g.add(&typed);
    g.add(&pdfs);
    let d = adw::AlertDialog::new(
        Some("Erase all data?"),
        Some("Every workspace, account, statement, transaction and learned rule is removed. Bank profiles, categories and preferences stay. A backup is made first when automatic backups are on."),
    );
    d.set_extra_child(Some(&g));
    d.add_responses(&[("cancel", "Cancel"), ("erase", "Erase Everything")]);
    d.set_response_appearance("erase", adw::ResponseAppearance::Destructive);
    d.set_response_enabled("erase", false);
    {
        let d = d.clone();
        typed.connect_changed(move |e| d.set_response_enabled("erase", e.text().trim() == "ERASE"));
    }
    let (w, prefs) = (Rc::downgrade(app), prefs.clone());
    d.connect_response(None, move |_, r| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        if r != "erase" {
            return;
        }
        if let Err(e) = a.db.auto_backup("before-erase") {
            return a.toast(&format!("Couldn't make the backup first, so nothing was erased: {e}"));
        }
        if pdfs.is_active()
            && let Err(e) = a.db.delete_kept_pdfs()
        {
            return a.toast(&format!("Couldn't delete the kept statement PDFs, so nothing was erased: {e}"));
        }
        match a.db.erase_all() {
            Ok(()) => {
                prefs.close();
                a.reload_entities();
                a.toast("Erased");
                crate::guide::open_at(&a, 0);
            }
            Err(e) => a.toast(&e.to_string()),
        }
    }));
    d.present(Some(&app.win));
}

/// Exchange rates for currencies other than KYD and USD: the download switch, what's kept, and Update Now.
fn rates_group(app: &Rc<App>, p: &fingraph_core::prefs::Prefs) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("Exchange Rates")
        .description("KYD and USD always convert at the peg, 1 KYD = 1.20 USD. Other currencies convert at each day's rate: from Frankfurter (central-bank rates), or the fawazahmed0 currency API when it's down. Only currency codes and dates are sent; rates are kept, so each day is fetched once.")
        .build();
    g.add(&switch(app, "Download exchange rates", "For accounts, budgets, bills, assets and goals in other currencies (GBP, CAD, JMD…)", p.download_rates, |a, p, v| {
        p.download_rates = v;
        if v {
            let a = a.clone();
            glib::idle_add_local_once(move || a.update_rates(true));
        }
    }));
    match app.db.rates_status() {
        Ok(v) if v.is_empty() => g.add(&adw::ActionRow::builder().title("No other currencies in use").subtitle("All recorded and planned amounts are in KYD or USD").build()),
        Ok(v) => {
            for (c, last) in v {
                let sub = match last {
                    Some((day, src)) => format!("Latest rate {day}, from {src}"),
                    None => "No rate yet: combined totals and plans need a rate before these amounts can be converted".into(),
                };
                g.add(&adw::ActionRow::builder().title(glib::markup_escape_text(&c.label())).subtitle(glib::markup_escape_text(&sub)).build());
            }
        }
        Err(e) => g.add(&adw::ActionRow::builder().title("Couldn't read the rates kept").subtitle(glib::markup_escape_text(&e.to_string())).build()),
    }
    let now = adw::ButtonRow::builder().title("Update Now").build();
    let w = Rc::downgrade(app);
    now.connect_activated(move |_| {
        if let Some(a) = w.upgrade() {
            a.update_rates(true);
        }
    });
    g.add(&now);
    g
}
