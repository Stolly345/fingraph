//! Statements & Multi-Pass Inspector: drop PDFs, see which bank and account each became, whether it
//! reconciled, and the scanner's own report for every pass, with 1-click fixes for skipped lines.

use adw::prelude::*;
use chrono::NaiveDate;
use fingraph_core::db::{self, DocumentRow, ImportPlan, Review, Target};
use fingraph_core::money::{fmt_money, parse_user};
use fingraph_core::parser::{MultiPassScanner, ScanResult};
use fingraph_core::{Currency, Decimal, Reconciliation};
use gtk::{gdk, gio, glib};
use std::path::PathBuf;
use std::rc::Rc;

use crate::{App, dim, pill};

pub fn reconciliation_pill(r: &Reconciliation, ccy: Currency) -> gtk::Label {
    match r {
        Reconciliation::Reconciled => pill(&format!("Reconciled · {} diff", fmt_money(Decimal::ZERO, ccy)), "ok"),
        Reconciliation::Discrepancy(d) => pill(&format!("Discrepancy {}", fmt_money(*d, ccy)), "bad"),
        Reconciliation::Unverified => pill("Unverified", "meh"),
    }
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let import = gtk::Button::builder().label("Import PDFs…").css_classes(["suggested-action", "pill"]).action_name("win.import").build();
    let target = dim("");
    let top = gtk::Box::builder().spacing(12).build();
    let intro = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
    intro.append(&crate::heading("Statements"));
    intro.append(&target);
    top.append(&intro);
    top.append(&import);
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    body.append(&top);
    let drop_hint = adw::StatusPage::builder()
        .icon_name("document-open-symbolic")
        .title("Drop bank statements here")
        .description("PDF statements from Butterfield, Scotiabank, Cayman National, RBC or the CICSA Credit Union. Each is read five times over and checked against its own balances.")
        .build();
    body.append(&drop_hint);
    // coverage: one glance tells you which statement you forgot to download
    let cov_card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["card", "chart-card"]).build();
    let cov_head = gtk::Box::builder().spacing(8).build();
    let cov_title = crate::heading("Statement Coverage");
    cov_title.set_hexpand(true);
    cov_head.append(&cov_title);
    cov_head.append(&gtk::Label::builder().label("● verified   ◐ doesn't balance   ○ missing   · before the account's first statement").css_classes(["dim-label", "caption"]).build());
    cov_card.append(&cov_head);
    let cov_grid = gtk::Grid::builder().column_spacing(6).row_spacing(4).build();
    cov_card.append(&cov_grid);
    let cov_breaks = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    cov_card.append(&cov_breaks);
    body.append(&cov_card);
    body.append(&list);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1100).child(&body).build()).vexpand(true).build();

    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    {
        let w = Rc::downgrade(app);
        drop.connect_drop(move |_, v, _, _| {
            let (Some(a), Ok(files)) = (w.upgrade(), v.get::<gdk::FileList>()) else { return false };
            import_paths(&a, files.files().iter().filter_map(|f| f.path()).collect());
            true
        });
    }
    page.add_controller(drop);

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            let ents = a.entities.borrow().clone();
            target.set_label(&match a.import_entity().and_then(|id| ents.iter().find(|e| e.id == id)) {
                Some(e) => format!("New statements go into “{}”. Switch workspace in the header to import elsewhere.", e.name),
                None => "Imports create a “Personal” workspace.".into(),
            });
            list.remove_all();
            let docs = a.db.documents_all(&a.entity_ids()).unwrap_or_default();
            fill_coverage(&a, &cov_grid, &cov_breaks);
            cov_card.set_visible(!docs.is_empty());
            drop_hint.set_visible(docs.is_empty());
            list.set_visible(!docs.is_empty());
            for d in docs {
                list.append(&doc_row(&a, &d));
            }
        })
    };
    (page.upcast(), refresh)
}

fn doc_row(app: &Rc<App>, d: &DocumentRow) -> adw::ActionRow {
    let period = d.period.map(|(a, b)| format!("{} – {}", a.format("%d %b %Y"), b.format("%d %b %Y"))).unwrap_or_else(|| "period unknown".into());
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&d.filename))
        .subtitle(glib::markup_escape_text(&format!("{} · {period} · {} transactions · confidence {:.0}%", d.account_name, d.tx_count, d.confidence * 100.0)))
        .build();
    row.add_prefix(&pill(if d.bank_short.is_empty() { "Unknown bank" } else { &d.bank_short }, "bank"));
    row.add_suffix(&reconciliation_pill(&d.reconciliation, d.currency));
    if !d.review.is_empty() {
        row.set_subtitle(&glib::markup_escape_text(&format!("{} · waiting for review: {}", row.subtitle().unwrap_or_default(), d.review.iter().map(|r| r.text()).collect::<Vec<_>>().join("; "))));
        let approve = gtk::Button::builder().label("Approve…").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        row.add_suffix(&approve);
        let (w, id, name, why, n) = (Rc::downgrade(app), d.id, d.filename.clone(), d.review.clone(), d.tx_count);
        approve.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                approve_dialog(&a, id, &name, &why, n);
            }
        });
    }
    let report = gtk::Button::builder().label("Scan Pass Report").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Remove this statement and its transactions").valign(gtk::Align::Center).css_classes(["flat"]).build();
    row.add_suffix(&report);
    row.add_suffix(&del);
    {
        let (w, id) = (Rc::downgrade(app), d.id);
        report.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                report_dialog(&a, id);
            }
        });
    }
    {
        let (w, id, name) = (Rc::downgrade(app), d.id, d.filename.clone());
        del.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let dlg = adw::AlertDialog::new(Some("Remove statement?"), Some(&format!("“{name}” and all of its transactions leave the ledger. Rules you've learned stay.")));
            dlg.add_responses(&[("cancel", "Cancel"), ("del", "Remove")]);
            dlg.set_response_appearance("del", adw::ResponseAppearance::Destructive);
            let w = Rc::downgrade(&a);
            dlg.connect_response(None, move |_, r| {
                if let (Some(a), "del") = (w.upgrade(), r) {
                    match a.db.delete_document(id) {
                        Ok(()) => a.toast("Statement removed"),
                        Err(e) => a.toast(&e.to_string()),
                    }
                    a.reload_entities();
                }
            });
            dlg.present(Some(&a.win));
        });
    }
    row
}

/// Approving a held statement lets its transactions count; the dialog says what was wrong first.
fn approve_dialog(app: &Rc<App>, id: i64, name: &str, why: &[Review], n: i64) {
    let reasons = why.iter().map(|r| format!("• {}", r.text())).collect::<Vec<_>>().join("\n");
    let dlg = adw::AlertDialog::new(
        Some("Approve this statement?"),
        Some(&format!(
            "“{name}” is waiting because:\n{reasons}\n\nOpen its Scan Pass Report to compare it with the PDF. Once approved, its {n} transactions count in your budgets and reports."
        )),
    );
    dlg.add_responses(&[("cancel", "Cancel"), ("report", "Scan Pass Report"), ("ok", "Approve")]);
    dlg.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    dlg.connect_response(None, move |_, r| {
        let Some(a) = w.upgrade() else { return };
        match r {
            "report" => report_dialog(&a, id),
            "ok" => {
                match a.db.approve_document(id) {
                    Ok(ap) if ap.created.is_some() => a.toast(&format!(
                        "Approved: {} is set up as a bank now, so its next statements are recognised{}",
                        ap.created.as_deref().unwrap_or_default(),
                        if ap.rereads > 0 { format!(" ({} other held statement{} re-read)", ap.rereads, if ap.rereads == 1 { "" } else { "s" }) } else { String::new() }
                    )),
                    Ok(ap) if ap.unnamed => a.toast("Approved. Its first page doesn't name the bank, so FinGraph can't recognise its next statements: set it up in Bank Profiles → New from Sample"),
                    Ok(ap) => a.toast(&match (&ap.learned, ap.rereads) {
                        (Some(bank), 0) => format!("Approved: its transactions now count, and {bank}'s saved layout learned from it"),
                        (Some(bank), n) => format!("Approved: {bank}'s saved layout learned from it, and {n} other held statement{} re-read", if n == 1 { " was" } else { "s were" }),
                        (None, _) => "Approved: its transactions now count".into(),
                    }),
                    Err(e) => a.toast(&e.to_string()),
                }
                a.reload_entities();
            }
            _ => {}
        }
    });
    dlg.present(Some(&app.win));
}

/// Ask about what an import can't decide alone. `None` skips the file.
async fn decide(app: &Rc<App>, name: &str, plan: &ImportPlan) -> Option<Vec<Target>> {
    if !plan.same_period.is_empty() {
        let old = plan.same_period.iter().map(|d| format!("“{}”", d.1)).collect::<Vec<_>>().join(", ");
        let d = adw::AlertDialog::new(
            Some("This period is already imported"),
            Some(&format!("“{name}” covers the same account and period as {old}, already in FinGraph: probably another copy of the same statement.\n\nReplace the earlier one with this file, or skip this one?")),
        );
        d.add_responses(&[("skip", "Skip"), ("replace", "Replace")]);
        d.set_close_response("skip");
        if d.choose_future(Some(&app.win)).await != "replace" {
            return None;
        }
        for (id, _) in &plan.same_period {
            let _ = app.db.delete_document(*id);
        }
    }
    let mut targets = Vec::new();
    for s in &plan.sections {
        if let Some(id) = s.matched {
            targets.push(Target::Existing(id));
            continue;
        }
        let d = if s.number_read {
            let d = adw::AlertDialog::new(
                Some("New account found"),
                Some(&format!("“{name}” is from {}, which isn't in FinGraph yet.\n\nIs this your account? A statement from someone else's account shouldn't be imported.", s.new_name)),
            );
            d.add_responses(&[("skip", "Not Mine"), ("new", "Add Account")]);
            d.set_response_appearance("new", adw::ResponseAppearance::Suggested);
            d
        } else {
            let d = adw::AlertDialog::new(
                Some("Which account is this?"),
                Some(&format!("No account number could be read from “{name}” (by its layout: {}). Pick the account it belongs to.", s.new_name.trim_end())),
            );
            for (id, n) in s.candidates.iter().take(4) {
                d.add_response(&format!("a{id}"), n);
            }
            d.add_responses(&[("new", "New Account"), ("skip", "Skip")]);
            d
        };
        d.set_close_response("skip");
        let r = d.choose_future(Some(&app.win)).await;
        targets.push(match r.as_str() {
            "new" => Target::New,
            "skip" => Target::Skip,
            a => a.strip_prefix('a').and_then(|i| i.parse().ok()).map_or(Target::Skip, Target::Existing),
        });
    }
    targets.iter().any(|t| *t != Target::Skip).then_some(targets)
}

/// Account × month dots for the last twelve months, then any break in the chain of balances.
fn fill_coverage(app: &Rc<App>, grid: &gtk::Grid, breaks: &gtk::Box) {
    use fingraph_core::workflows::MonthState;
    while let Some(c) = grid.first_child() {
        grid.remove(&c);
    }
    crate::clear(breaks);
    let cov = app.db.coverage(&app.entity_ids(), 12, crate::today()).unwrap_or_default();
    if let Some(first) = cov.first() {
        for (i, (m, _)) in first.months.iter().enumerate() {
            grid.attach(&gtk::Label::builder().label(m.format("%b").to_string()).css_classes(["caption", "dim-label"]).width_chars(4).build(), i as i32 + 1, 0, 1, 1);
        }
    }
    for (r, c) in cov.iter().enumerate() {
        let row = r as i32 + 1;
        let name = gtk::Label::builder().label(&c.account).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(34).build();
        grid.attach(&name, 0, row, 1, 1);
        for (i, (m, s)) in c.months.iter().enumerate() {
            let (glyph, class, what) = match s {
                MonthState::Verified => ("●", "amount-in", "statement present and verified to the cent"),
                MonthState::Discrepancy => ("◐", "warning", "statement present but it doesn't balance (see its Scan Pass Report)"),
                MonthState::Missing => ("○", "error", "no statement — download it from your bank and drop it here"),
                MonthState::Pending => ("·", "dim-label", "this month: the statement isn't out yet"),
                MonthState::Before => ("·", "dim-label", "before this account's first statement"),
            };
            let l = gtk::Label::builder().label(glyph).css_classes([class, "heading"]).tooltip_text(format!("{} {}: {what}", c.account, m.format("%B %Y"))).build();
            grid.attach(&l, i as i32 + 1, row, 1, 1);
        }
    }
    for c in &cov {
        for b in &c.breaks {
            breaks.append(&gtk::Label::builder().label(format!("⚠ {} — {b}", c.account)).xalign(0.0).wrap(true).css_classes(["warning", "caption"]).build());
        }
    }
}

pub fn choose_files(app: &Rc<App>) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("PDF statements"));
    filter.add_mime_type("application/pdf");
    filter.add_suffix("pdf");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let fd = gtk::FileDialog::builder().title("Import Statements").modal(true).filters(&filters).build();
    let w = Rc::downgrade(app);
    fd.open_multiple(Some(&app.win), gio::Cancellable::NONE, move |res| {
        let (Some(a), Ok(files)) = (w.upgrade(), res) else { return };
        let paths: Vec<PathBuf> = (0..files.n_items()).filter_map(|i| files.item(i).and_downcast::<gio::File>().and_then(|f| f.path())).collect();
        import_paths(&a, paths);
    });
}

/// Scan off the main thread, store on it. Called for drops, the file chooser, `fingraph file.pdf`
/// and the onboarding walkthrough (which passes `on_each` to show live results).
pub fn import_paths(app: &Rc<App>, paths: Vec<PathBuf>) {
    import_paths_with(app, paths, None, None);
}

pub type OnEach = Rc<dyn Fn(&str, Result<(&ScanResult, &db::ImportReport), String>)>;

/// `on_done` replaces the summary toast and the jump to triage (the Getting Started guide runs
/// those steps itself).
pub fn import_paths_with(app: &Rc<App>, paths: Vec<PathBuf>, on_each: Option<OnEach>, on_done: Option<crate::Refresh>) {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))).collect();
    if paths.is_empty() {
        app.toast("Only PDF statements can be imported");
        return;
    }
    let entity = match app.import_entity() {
        Some(id) => id,
        None => match app.db.create_entity("Personal", fingraph_core::EntityKind::Personal, Currency::KYD) {
            Ok(id) => {
                app.reload_entities();
                id
            }
            Err(e) => return app.toast(&e.to_string()),
        },
    };
    let profiles = app.db.profiles().unwrap_or_default();
    let w = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let (mut inserted, mut files, mut reconciled, mut held) = (0, 0, 0, 0);
        for p in paths {
            let profiles = profiles.clone();
            let p2 = p.clone();
            let scanned = gio::spawn_blocking(move || -> Result<(ScanResult, String), String> {
                let sha = db::file_sha256(&p2).map_err(|e| e.to_string())?;
                let r = MultiPassScanner::new(&profiles).scan_pdf(&p2).map_err(|e| e.to_string())?;
                Ok((r, sha))
            })
            .await
            .unwrap_or_else(|_| Err("the scanner crashed".into()));
            let Some(a) = w.upgrade() else { return };
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            remember_seen(&a, std::slice::from_ref(&p));
            let scanned = scanned.and_then(|(scan, sha)| {
                let plan = a.db.plan_import(&sha, &scan).map_err(|e| e.to_string())?;
                if plan.no_transactions {
                    return Err("no transactions or balances were found in it".into());
                }
                Ok((scan, sha, plan))
            });
            let scanned = match scanned {
                Ok((scan, sha, plan)) => {
                    let targets = if plan.already_imported { Some(Vec::new()) } else { plan.automatic() };
                    match targets {
                        Some(t) => Ok((scan, sha, t)),
                        None => match decide(&a, &name, &plan).await {
                            Some(t) => Ok((scan, sha, t)),
                            None => {
                                a.toast(&format!("{name}: skipped"));
                                continue;
                            }
                        },
                    }
                }
                Err(e) => Err(e),
            };
            match scanned {
                Ok((scan, sha, targets)) => match a.db.import_scan(entity, &name, &sha, &scan, &targets) {
                    Ok(rep) => {
                        // kept so the statement can be rescanned after a scanner or profile change
                        if let Err(e) = a.db.keep_pdf(&p, &sha) {
                            a.toast(&format!("{name}: couldn't keep a copy for rescanning ({e})"));
                        }
                        files += 1;
                        inserted += rep.inserted;
                        held += rep.held;
                        reconciled += scan.is_reconciled() as usize;
                        if rep.already_imported {
                            a.toast(&format!("{name} was already imported"));
                        }
                        if let Some(f) = &on_each {
                            f(&name, Ok((&scan, &rep)));
                        }
                    }
                    Err(e) => {
                        a.toast(&format!("{name}: {e}"));
                        if let Some(f) = &on_each {
                            f(&name, Err(e.to_string()));
                        }
                    }
                },
                Err(e) => {
                    a.toast(&format!("{name}: {e}"));
                    if let Some(f) = &on_each {
                        f(&name, Err(e));
                    }
                }
            }
        }
        let Some(a) = w.upgrade() else { return };
        a.reload_entities();
        // a statement in another currency needs its days' rates
        a.update_rates(false);
        if let Some(f) = on_done {
            return f();
        }
        if held > 0 {
            a.toast_action(
                &format!("{held} statement section{} waiting for review: they don't count until you approve them", if held == 1 { " is" } else { "s are" }),
                "Review",
                Rc::new(|a: &Rc<App>| a.show_view("statements")),
            );
        }
        if files > 0 {
            let msg = format!("Imported {inserted} transactions from {files} statement{} · {reconciled} reconciled", if files == 1 { "" } else { "s" });
            let waiting = a.db.uncategorized_count(&a.entity_ids()).unwrap_or(0) > 0;
            if waiting && a.db.prefs().triage_after_import {
                a.toast(&msg);
                crate::wizard::triage(&a);
            } else if waiting {
                a.toast_action(&msg, "Triage", Rc::new(crate::wizard::triage));
            } else {
                a.toast(&msg);
            }
        }
    });
}

/// Preferences → Import → Watch a folder: import new bank statements that land there, now and
/// while FinGraph is open. A PDF that isn't from a known bank is noted and left alone.
pub fn watch(app: &Rc<App>) {
    *app.watch.borrow_mut() = None;
    let p = app.db.prefs();
    let Some(dir) = p.watch_folder.clone().filter(|_| p.watch_enabled) else { return };
    sweep(app, &dir);
    let Ok(mon) = gio::File::for_path(&dir).monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) else {
        return app.toast(&format!("Can't watch {}", dir.display()));
    };
    let w = Rc::downgrade(app);
    mon.connect_changed(move |_, f, other, ev| {
        let Some(a) = w.upgrade() else { return };
        let file = if ev == gio::FileMonitorEvent::MovedIn || ev == gio::FileMonitorEvent::Renamed { other.cloned().unwrap_or_else(|| f.clone()) } else { f.clone() };
        if matches!(ev, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::MovedIn | gio::FileMonitorEvent::Renamed)
            && let Some(p) = file.path()
        {
            check(&a, vec![p]);
        }
    });
    *app.watch.borrow_mut() = Some(mon);
}

fn sweep(app: &Rc<App>, dir: &std::path::Path) {
    check(app, pdfs_in(dir));
}

/// Every PDF in a folder and the folders inside it (people file statements by bank or by year).
pub fn pdfs_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for p in std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()) {
            if p.is_dir() {
                todo.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// What the watcher remembers a file by: path, size and time.
fn seen_key(p: &std::path::Path) -> Option<String> {
    std::fs::metadata(p).ok().map(|m| format!("{}|{}|{:?}", p.display(), m.len(), m.modified().ok()))
}

/// Files already read some other way (the Getting Started guide), so the watcher doesn't scan them again.
pub fn remember_seen(app: &App, paths: &[PathBuf]) {
    let mut seen: std::collections::HashSet<String> = app.db.setting("watch_seen").and_then(|j| serde_json_lite(&j)).unwrap_or_default();
    seen.extend(paths.iter().filter_map(|p| seen_key(p)));
    let _ = app.db.set_setting("watch_seen", &seen.iter().map(|s| format!("{s}\n")).collect::<String>());
}

/// Seen before (by path, size and time) → skip; otherwise scan, and import only real statements.
fn check(app: &Rc<App>, paths: Vec<PathBuf>) {
    let key = |p: &std::path::Path| seen_key(p);
    let seen: std::collections::HashSet<String> = app.db.setting("watch_seen").and_then(|j| serde_json_lite(&j)).unwrap_or_default();
    let todo: Vec<(PathBuf, String)> = paths.into_iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))).filter_map(|p| Some((p.clone(), key(&p)?))).filter(|(_, k)| !seen.contains(k)).collect();
    if todo.is_empty() {
        return;
    }
    let profiles = app.db.profiles().unwrap_or_default();
    let w = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        for (p, k) in todo {
            let (profiles, p2) = (profiles.clone(), p.clone());
            let r = gio::spawn_blocking(move || -> Option<(ScanResult, String)> {
                let sha = db::file_sha256(&p2).ok()?;
                let scan = MultiPassScanner::new(&profiles).scan_pdf(&p2).ok()?;
                Some((scan, sha))
            })
            .await
            .ok()
            .flatten();
            let Some(a) = w.upgrade() else { return };
            let seen_now = |a: &Rc<App>| {
                let mut seen: std::collections::HashSet<String> = a.db.setting("watch_seen").and_then(|j| serde_json_lite(&j)).unwrap_or_default();
                seen.insert(k.clone());
                let _ = a.db.set_setting("watch_seen", &seen.iter().map(|s| format!("{s}\n")).collect::<String>());
            };
            let Some((scan, sha)) = r.filter(|(s, _)| s.profile_slug.is_some() && !s.sections.is_empty()) else {
                seen_now(&a);
                continue;
            };
            let Some(entity) = a.import_entity() else { continue };
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Ok(plan) = a.db.plan_import(&sha, &scan) else { continue };
            if plan.already_imported || plan.no_transactions {
                seen_now(&a);
                continue;
            }
            // a new account, an unreadable account number or a period already imported needs a person:
            // nothing is imported, and until it's decided the file is offered again each time
            let Some(targets) = plan.automatic() else {
                let p2 = p.clone();
                a.toast_action(&format!("{name} in your watched folder needs a decision before it's imported"), "Review", Rc::new(move |a: &Rc<App>| import_paths(a, vec![p2.clone()])));
                continue;
            };
            seen_now(&a);
            if let Ok(rep) = a.db.import_scan(entity, &name, &sha, &scan, &targets) {
                let _ = a.db.keep_pdf(&p, &sha);
                a.reload_entities();
                if rep.held > 0 {
                    a.toast_action(&format!("{name} from your watched folder is waiting for review"), "Review", Rc::new(|a: &Rc<App>| a.show_view("statements")));
                } else {
                    a.toast_action(&format!("Imported {name} from your watched folder · {} transactions", rep.inserted), "Triage", Rc::new(crate::wizard::triage));
                }
            }
        }
    });
}

/// The seen-list is stored one entry per line.
fn serde_json_lite(s: &str) -> Option<std::collections::HashSet<String>> {
    Some(s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Re-read every statement with the current scanner and profiles, after asking.
pub fn rescan_all(app: &Rc<App>) {
    let d = adw::AlertDialog::new(
        Some("Rescan All Statements?"),
        Some("Every statement is read again with the current scanner and bank profiles. Learned rules are re-applied, but categories you set by hand on single transactions are lost."),
    );
    d.add_responses(&[("cancel", "Cancel"), ("go", "Rescan")]);
    d.set_response_appearance("go", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| {
        let Some(a) = w.upgrade() else { return };
        if r != "go" {
            return;
        }
        let _ = a.db.auto_backup("before-rescan");
        let files = a.db.statement_files().unwrap_or_default();
        let profiles = a.db.profiles().unwrap_or_default();
        let w = Rc::downgrade(&a);
        glib::spawn_future_local(async move {
            let (mut n, mut txs, mut missing) = (0, 0, Vec::new());
            for (sha, name, entity) in files {
                let Some(path) = a.db.stored_pdf(&sha, &name) else {
                    missing.push(name);
                    continue;
                };
                let profiles = profiles.clone();
                let scan = gio::spawn_blocking(move || MultiPassScanner::new(&profiles).scan_pdf(&path).map_err(|e| e.to_string())).await.unwrap_or_else(|_| Err("scanner crashed".into()));
                let Some(a) = w.upgrade() else { return };
                match scan.and_then(|s| a.db.replace_statement(entity, &name, &sha, &s).map_err(|e| e.to_string())) {
                    Ok(rep) => {
                        n += 1;
                        txs += rep.inserted;
                    }
                    Err(e) => a.toast(&format!("{name}: {e}")),
                }
            }
            let Some(a) = w.upgrade() else { return };
            a.reload_entities();
            let mut msg = format!("Rescanned {n} statements · {txs} transactions");
            if !missing.is_empty() {
                msg += &format!(" · {} not found (re-import them)", missing.len());
            }
            a.toast(&msg);
        });
    });
    d.present(Some(&app.win));
}

#[cfg(debug_assertions)]
pub fn open_first_report(app: &Rc<App>) {
    let docs = app.db.documents_all(&app.entity_ids()).unwrap_or_default();
    if let Some(d) = docs.iter().find(|d| d.reconciliation != Reconciliation::Reconciled).or(docs.first()) {
        report_dialog(app, d.id);
    }
}

fn kv(group: &adw::PreferencesGroup, k: &str, v: &str) {
    let row = adw::ActionRow::builder().title(k).subtitle(glib::markup_escape_text(v)).subtitle_selectable(true).css_classes(["property"]).build();
    group.add(&row);
}

/// Pass 1–5 diagnostics for one statement section.
pub fn report_dialog(app: &Rc<App>, doc_id: i64) {
    let Ok((diag, report)) = app.db.document_report(doc_id) else { return app.toast("No scan report stored for this statement") };
    let docs = app.db.documents_all(&[]).unwrap_or_default();
    let Some(d) = docs.iter().find(|d| d.id == doc_id).cloned() else { return };
    let m = |x: Option<Decimal>| x.map_or("—".to_string(), |v| fmt_money(v, d.currency));
    let page = adw::PreferencesPage::new();

    let g0 = adw::PreferencesGroup::builder().title("Verdict").build();
    let verdict = adw::ActionRow::builder().title(glib::markup_escape_text(&d.filename)).subtitle(glib::markup_escape_text(&d.account_name)).build();
    verdict.add_suffix(&reconciliation_pill(&d.reconciliation, d.currency));
    g0.add(&verdict);
    page.add(&g0);

    let p1 = &diag.pass1;
    let g1 = adw::PreferencesGroup::builder().title("Pass 1 · Spatial tokens &amp; institution fingerprint").build();
    if !d.review.is_empty() {
        kv(&g1, "Waiting for review", &d.review.iter().map(|r| r.text()).collect::<Vec<_>>().join("\n"));
    }
    kv(&g1, "Detected profile", p1.profile.as_deref().unwrap_or("none matched"));
    if p1.ocr {
        kv(&g1, "Read by OCR", "the PDF is only pictures of pages: amounts are checked against the balances, but check names for typos");
    }
    kv(&g1, "Fingerprint scores", &p1.scores.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(" · "));
    kv(&g1, "Statement period", &p1.period.map(|(a, b)| format!("{a} → {b} (locks years and DD/MM order)")).unwrap_or("not found".into()));
    kv(&g1, "Account", &format!("{:?} · ••{} · {}", p1.account_type.unwrap_or(fingraph_core::AccountType::Checking), p1.account_last4.as_deref().unwrap_or("????"), p1.currency.map_or("?", |c| c.code())));
    if !p1.currency_columns.is_empty() {
        kv(&g1, "Summary columns", &p1.currency_columns.iter().map(|c| c.code()).collect::<Vec<_>>().join(" | "));
    }
    kv(&g1, "Summary anchors", &if p1.summary_lines.is_empty() { "none (using the table's own balance rows)".to_string() } else { p1.summary_lines.join("\n") });
    page.add(&g1);

    let g2 = adw::PreferencesGroup::builder().title("Pass 2 · Column geometry").description("Each page's table columns, locked from the header words and then from where the amounts' right edges actually line up").build();
    for pg in &diag.pages {
        let cols = if pg.columns.is_empty() {
            "no table on this page".to_string()
        } else {
            pg.columns.iter().map(|c| format!("{:?} {:.0}–{:.0}", c.role, c.x0, c.x1)).collect::<Vec<_>>().join(" · ")
        };
        let extra = if pg.inherited { " (columns carried over from the previous page)" } else { "" };
        let noise = if pg.noise_stripped.is_empty() { String::new() } else { format!("\nStripped {} noise line(s): {}", pg.noise_stripped.len(), pg.noise_stripped.join(" / ")) };
        kv(&g2, &format!("Page {} · {} table rows{extra}", pg.page, pg.rows), &format!("{cols}{noise}"));
    }
    page.add(&g2);

    let p3 = &diag.pass3;
    let g3 = adw::PreferencesGroup::builder().title("Pass 3 · Transactions &amp; multi-line stitching").build();
    kv(&g3, "Transaction rows", &p3.anchors.to_string());
    kv(&g3, "Description lines stitched into memos", &p3.stitched_lines.to_string());
    kv(&g3, "Dates inherited from the row above (RBC-style)", &p3.inherited_dates.to_string());
    kv(&g3, "Rows whose amount came on a later line (CNB-style)", &p3.joined_rows.to_string());
    for n in &p3.layout_notes {
        kv(&g3, "⚠ Doesn't match the bank's declared layout", n);
    }
    kv(&g3, "Account sections in this PDF", &p3.sections.to_string());
    page.add(&g3);

    let s = &report.summary;
    let g4 = adw::PreferencesGroup::builder().title("Pass 4 · The math").build();
    let eq = match (s.opening, s.closing) {
        (Some(o), Some(c)) => format!(
            "Opening {} + credits {} − debits {} = {} (statement says {}) → difference {}",
            m(Some(o)),
            m(Some(s.extracted_credits)),
            m(Some(s.extracted_debits)),
            m(Some(c - s.difference.unwrap_or_default())),
            m(Some(c)),
            m(s.difference)
        ),
        _ => "No opening/closing balance printed".to_string(),
    };
    kv(&g4, "Statement equation", &eq);
    kv(&g4, "Printed totals", &format!("debits {} · credits {}", m(s.total_debits), m(s.total_credits)));
    let ok = report.segments.iter().filter(|g| g.ok).count();
    kv(&g4, "Running balance checks  Bal[i−1] + Amt[i] = Bal[i]", &format!("{ok} of {} stretches verified", report.segments.len()));
    for g in report.segments.iter().filter(|g| !g.ok) {
        kv(&g4, "✗ Gap", &format!("between row {} and row {}: balance moved {} but rows add to {} — {} missing", g.from_tx.map_or("opening".into(), |i| (i + 1).to_string()), g.to_tx.map_or("?".into(), |i| (i + 1).to_string()), m(Some(g.expected)), m(Some(g.got)), m(Some(g.expected - g.got))));
    }
    for h in &report.sign_heals {
        kv(&g4, "Sign self-healed", h);
    }
    for b in &report.misread_balances {
        kv(&g4, "Balance ignored", b);
    }
    page.add(&g4);

    let g5 = adw::PreferencesGroup::builder().title("Pass 5 · Self-healing re-scan").build();
    kv(&g5, "Re-scans", &report.rescans.to_string());
    for r in &report.recovered {
        kv(&g5, "Recovered", r);
    }
    page.add(&g5);
    if !report.skipped.is_empty() {
        let g6 = adw::PreferencesGroup::builder()
            .title("Unparsed lines")
            .description(if d.reconciliation == Reconciliation::Reconciled { "Lines that looked like transactions but weren't needed to balance — usually summaries or payment slips." } else { "The statement doesn't balance yet. One of these is probably the missing row: convert it in one click." })
            .build();
        for (i, l) in report.skipped.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&l.text))
                .subtitle(glib::markup_escape_text(&format!("page {} · {}{}{}", l.page, l.reason, l.date.map(|d| format!(" · {d}")).unwrap_or_default(), l.amount.map(|v| format!(" · {v}")).unwrap_or_default())))
                .build();
            let b = gtk::Button::builder().label("Convert…").valign(gtk::Align::Center).build();
            if d.reconciliation != Reconciliation::Reconciled {
                b.add_css_class("suggested-action");
            }
            let (w, line, dd) = (Rc::downgrade(app), l.clone(), d.clone());
            b.connect_clicked(move |b| {
                if let Some(a) = w.upgrade() {
                    convert_dialog(&a, &dd, i, &line, b);
                }
            });
            row.add_suffix(&b);
            g6.add(&row);
        }
        page.add(&g6);
    }

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Scan Pass Report", &d.filename)).build());
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().content_width(820).content_height(760).child(&tv).build();
    dlg.present(Some(&app.win));
}

fn convert_dialog(app: &Rc<App>, d: &DocumentRow, index: usize, line: &fingraph_core::parser::SkippedLine, from: &gtk::Button) {
    let date = adw::EntryRow::builder().title("Date (YYYY-MM-DD)").text(line.date.or(d.period.map(|p| p.1)).map(|x| x.to_string()).unwrap_or_default()).build();
    let gap = match d.reconciliation {
        Reconciliation::Discrepancy(x) => Some(x),
        _ => None,
    };
    // the gap is in statement terms; a card or loan's ledger sign is the other way round
    let liability = app.db.accounts().unwrap_or_default().iter().find(|x| x.id == d.account_id).is_some_and(|x| x.account_type.is_liability());
    let guess = line.amount.map(|v| -v).or(gap.map(|g| if liability { -g } else { g }));
    let amount = adw::EntryRow::builder().title("Amount (negative = money out)").text(guess.map(|v| v.to_string()).unwrap_or_default()).build();
    let desc = adw::EntryRow::builder().title("Description").text(line.text.clone()).build();
    let group = adw::PreferencesGroup::builder()
        .description(gap.map(|g| format!("The statement is {} out; a row worth that would balance it.", fmt_money(g, d.currency))).unwrap_or_default())
        .build();
    group.add(&date);
    group.add(&amount);
    group.add(&desc);
    let dlg = adw::AlertDialog::new(Some("Convert to Transaction"), None);
    dlg.set_extra_child(Some(&group));
    dlg.add_responses(&[("cancel", "Cancel"), ("ok", "Add to Ledger")]);
    dlg.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let (w, doc, from) = (Rc::downgrade(app), d.id, from.clone());
    dlg.connect_response(None, move |_, r| {
        let Some(a) = w.upgrade() else { return };
        if r != "ok" {
            return;
        }
        let (Ok(day), Some(v)) = (NaiveDate::parse_from_str(date.text().trim(), "%Y-%m-%d"), parse_user(&amount.text())) else {
            return a.toast("Enter a date as YYYY-MM-DD and an amount");
        };
        match a.db.convert_skipped_line(doc, index, day, v, desc.text().trim()) {
            Ok(_) => {
                a.toast("Added — the statement was re-checked");
                if let Some(dl) = from.ancestor(adw::Dialog::static_type()).and_downcast::<adw::Dialog>() {
                    dl.close();
                }
                a.refresh();
                report_dialog(&a, doc);
            }
            Err(e) => a.toast(&e.to_string()),
        }
    });
    dlg.present(Some(&app.win));
}
