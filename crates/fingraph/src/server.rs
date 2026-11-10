//! Working with a FinGraph server (Preferences → Server). One device edits at a time: while this computer
//! is in use it holds the server's *edit lease* and works on its own copy as always (fast, and fine
//! offline); it sends the ledger back after an import, after ten quiet minutes (letting go of the lease)
//! and on closing. When it's used again it takes the lease back, first fetching the server's ledger if
//! another device changed it meanwhile.
use crate::App;
use adw::prelude::*;
use fingraph_client::{Error, Server};
use gtk::{gio, glib};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Quiet for this long, the lease goes back so the phone and other computers see the latest.
/// (`FINGRAPH_SERVER_IDLE_SECONDS` shortens it, for trying it out.)
fn idle_after() -> Duration {
    std::env::var("FINGRAPH_SERVER_IDLE_SECONDS").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(10 * 60))
}
/// The server forgets a lease after 30 minutes without news; renewed well before.
const RENEW: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    server: Option<Server>,
    /// The server's ledger version this computer's copy matches.
    #[serde(default)]
    synced_version: i64,
}

fn file() -> PathBuf {
    fingraph_core::config_dir().join("server.json")
}

fn load() -> Saved {
    std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(s: &Saved) {
    let path = file();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // the token is a password: this computer's user only
    let tmp = path.with_extension("json.partial");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Where things stand this session.
struct Link {
    server: Server,
    synced: i64,
    holding: bool,
    /// Another device has the lease (its name).
    elsewhere: Option<String>,
    /// The ledger's change count when it last matched the server's.
    baseline: u64,
    last_activity: Instant,
    busy: bool,
    error: Option<String>,
}

/// What a sync brings back: the server's ledger if newer, its version, its workspace count, and the
/// lease (or who has it).
type Pulled = (Option<PathBuf>, i64, usize, Result<(), String>);
/// Run after pairing.
type Then = Rc<dyn Fn(&Rc<App>)>;

thread_local! {
    static LINK: RefCell<Option<Link>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Link) -> R) -> Option<R> {
    LINK.with(|l| l.borrow_mut().as_mut().map(f))
}

pub fn connected() -> bool {
    LINK.with(|l| l.borrow().is_some())
}

/// For Preferences: (address, fingerprint, what's happening).
pub fn describe() -> Option<(String, String, String)> {
    with(|l| {
        let state = if let Some(e) = &l.error {
            format!("Can't reach it: {e}")
        } else if l.holding {
            "Editing on this computer".to_string()
        } else if let Some(other) = &l.elsewhere {
            format!("Being edited on {other}: changes here aren't sent until it's done")
        } else {
            "Up to date".to_string()
        };
        (l.server.address.clone(), fingraph_client::short_fingerprint(&l.server.fingerprint), format!("{state} · version {}", l.synced))
    })
}

fn set_synced(v: i64) {
    with(|l| l.synced = v);
    let mut s = load();
    s.synced_version = v;
    save(&s);
}

/// Runs `f` off the main thread and hands its answer back to `then` on it.
fn background<T: Send + 'static>(app: &Rc<App>, f: impl FnOnce() -> T + Send + 'static, then: impl FnOnce(&Rc<App>, T) + 'static) {
    let w = Rc::downgrade(app);
    crate::spawn_guarded(w.clone(), async move {
        let r = gio::spawn_blocking(f).await;
        if let (Some(a), Ok(r)) = (w.upgrade(), r) {
            then(&a, r);
        }
    });
}

fn server() -> Option<Server> {
    with(|l| l.server.clone())
}

thread_local! {
    static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// At start-up (and after pairing): connects if this computer is paired, and keeps the lease in step
/// with use.
pub fn start(app: &Rc<App>) {
    let saved = load();
    let Some(server) = saved.server else { return };
    LINK.with(|l| {
        *l.borrow_mut() = Some(Link { server, synced: saved.synced_version, holding: false, elsewhere: None, baseline: app.db.change_count(), last_activity: Instant::now(), busy: false, error: None })
    });
    sync_in(app, false);
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    watch_activity(app);
    // renew the lease while working; give it back after a quiet spell
    let w = Rc::downgrade(app);
    glib::timeout_add_local(Duration::from_secs(if std::env::var_os("FINGRAPH_SERVER_IDLE_SECONDS").is_some() { 5 } else { 60 }), move || {
        let Some(a) = w.upgrade() else { return glib::ControlFlow::Break };
        if !connected() {
            return glib::ControlFlow::Continue;
        }
        let (holding, idle, busy) = with(|l| (l.holding, l.last_activity.elapsed(), l.busy)).unwrap_or_default();
        if holding && !busy {
            if idle >= idle_after() {
                check_in(&a, true);
            } else {
                renew(&a);
            }
        }
        glib::ControlFlow::Continue
    });
    let w = Rc::downgrade(app);
    app.win.connect_close_request(move |_| {
        if let Some(a) = w.upgrade() {
            closing(&a);
        }
        glib::Propagation::Proceed
    });
}

thread_local! {
    static LAST_RENEW: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

fn renew(app: &Rc<App>) {
    let due = LAST_RENEW.with(|r| r.borrow().is_none_or(|t| t.elapsed() >= RENEW));
    let Some(s) = server().filter(|_| due) else { return };
    LAST_RENEW.with(|r| *r.borrow_mut() = Some(Instant::now()));
    background(app, move || s.take_lease(), |_, r| {
        if let Err(e) = r {
            with(|l| {
                l.holding = false;
                l.error = Some(e.to_string());
            });
        }
    });
}

/// Any click or key press counts as using FinGraph: after a quiet spell it takes the lease back.
fn watch_activity(app: &Rc<App>) {
    let touched = {
        let w = Rc::downgrade(app);
        move || {
            if let Some(a) = w.upgrade() {
                let resume = with(|l| {
                    let was_idle = l.last_activity.elapsed() >= Duration::from_secs(30);
                    l.last_activity = Instant::now();
                    !l.holding && !l.busy && was_idle
                })
                .unwrap_or(false);
                if resume {
                    sync_in(&a, false);
                }
            }
        }
    };
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let t = touched.clone();
    keys.connect_key_pressed(move |_, _, _, _| {
        t();
        glib::Propagation::Proceed
    });
    app.win.add_controller(keys);
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |_, _, _, _| touched());
    app.win.add_controller(click);
}

/// Brings this computer up to date with the server and takes the lease (or notes who has it).
pub fn sync_in(app: &Rc<App>, quiet: bool) {
    let Some(s) = server() else { return };
    if with(|l| std::mem::replace(&mut l.busy, true)).unwrap_or(true) {
        return;
    }
    let synced = with(|l| l.synced).unwrap_or(0);
    let dir = fingraph_core::data_dir().join("server-tmp");
    background(
        app,
        move || -> Result<Pulled, Error> {
            let status = s.status()?;
            let lease = s.take_lease();
            let lease = match lease {
                Ok(_) => Ok(()),
                Err(Error::Conflict(m)) => Err(status.lease.map(|l| l.device).unwrap_or(m)),
                Err(e) => return Err(e),
            };
            // newer on the server (another device, the phone, the inbox): fetch it first
            let fetched = if status.ledger_version > synced {
                let _ = std::fs::create_dir_all(&dir);
                let p = dir.join("from-server.tar");
                Some((p.clone(), s.download_ledger(&p)?))
            } else {
                None
            };
            let v = fetched.as_ref().map(|f| f.1).unwrap_or(status.ledger_version);
            Ok((fetched.map(|f| f.0), v, status.workspaces, lease))
        },
        move |a, r| {
            with(|l| l.busy = false);
            match r {
                Err(e) => {
                    with(|l| l.error = Some(e.to_string()));
                    if !quiet {
                        a.toast(&format!("FinGraph Server: {e}"));
                    }
                }
                Ok((fetched, version, workspaces, lease)) => {
                    with(|l| l.error = None);
                    if let Some(p) = fetched {
                        if let Err(e) = a.db.auto_backup("before-server") {
                            let _ = std::fs::remove_file(&p);
                            return a.toast(&format!("Couldn't back up this computer's ledger first, so the server's wasn't fetched: {e}"));
                        }
                        match a.db.restore_from(&p) {
                            Ok(()) => {
                                set_synced(version);
                                a.reload_entities();
                                a.toast("Updated from FinGraph Server");
                            }
                            Err(e) => a.toast(&format!("Couldn't use the server's ledger: {e}")),
                        }
                        let _ = std::fs::remove_file(&p);
                    } else if version != synced && workspaces > 0 {
                        set_synced(version);
                    }
                    let baseline = a.db.change_count();
                    match lease {
                        Ok(()) => {
                            with(|l| {
                                l.holding = true;
                                l.elsewhere = None;
                                l.baseline = baseline;
                            });
                            if workspaces == 0 && version == 0 {
                                a.toast_action("FinGraph Server is empty", "Send My Ledger", Rc::new(|a: &Rc<App>| send_ledger(a)));
                            }
                        }
                        Err(other) => {
                            with(|l| {
                                l.holding = false;
                                l.elsewhere = Some(other.clone());
                                l.baseline = baseline;
                            });
                            a.toast_action(&format!("The ledger is being edited on {other}; what you change here won't reach the server"), "Try Again", Rc::new(|a: &Rc<App>| sync_in(a, false)));
                        }
                    }
                }
            }
        },
    );
}

/// Something changed (an import, say): send the ledger now, keeping the lease.
pub fn changed(app: &Rc<App>) {
    if with(|l| l.holding).unwrap_or(false) {
        check_in(app, false);
    }
}

fn snapshot(app: &App) -> Result<PathBuf, String> {
    let dir = fingraph_core::data_dir().join("server-tmp");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = dir.join("to-server.tar");
    app.db.backup_archive(&p).map_err(|e| e.to_string())?;
    Ok(p)
}

/// Sends the ledger if it changed since it last matched the server's; `release` gives the lease back.
fn check_in(app: &Rc<App>, release: bool) {
    let Some(s) = server() else { return };
    let (holding, baseline, synced, busy) = with(|l| (l.holding, l.baseline, l.synced, l.busy)).unwrap_or_default();
    if !holding || busy {
        return;
    }
    let count = app.db.change_count();
    let dirty = count > baseline;
    let tar = if dirty {
        match snapshot(app) {
            Ok(p) => Some(p),
            Err(e) => return app.toast(&format!("Couldn't prepare the ledger for FinGraph Server: {e}")),
        }
    } else {
        None
    };
    with(|l| l.busy = true);
    background(
        app,
        move || -> Result<Option<i64>, Error> {
            let v = match &tar {
                Some(p) => {
                    let v = s.upload_ledger(p, Some(synced));
                    let _ = std::fs::remove_file(p);
                    Some(v?)
                }
                None => None,
            };
            if release {
                s.release_lease()?;
            }
            Ok(v)
        },
        move |a, r| {
            with(|l| l.busy = false);
            match r {
                Ok(v) => {
                    if let Some(v) = v {
                        set_synced(v);
                    }
                    with(|l| {
                        l.baseline = count;
                        l.error = None;
                        if release {
                            l.holding = false;
                        }
                    });
                }
                Err(Error::Conflict(m)) => {
                    with(|l| l.holding = false);
                    a.toast_action(&format!("FinGraph Server didn't take this computer's ledger: {m}"), "Fetch the Server's", Rc::new(|a: &Rc<App>| sync_in(a, false)));
                }
                Err(e) => {
                    with(|l| l.error = Some(e.to_string()));
                }
            }
        },
    );
}

/// On closing: send what changed and give the lease back, waiting a little (not forever) for it.
fn closing(app: &Rc<App>) {
    let Some(s) = server() else { return };
    let (holding, baseline, synced) = with(|l| (l.holding, l.baseline, l.synced)).unwrap_or_default();
    if !holding {
        return;
    }
    let tar = (app.db.change_count() > baseline).then(|| snapshot(app).ok()).flatten();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let v = tar.as_ref().map(|p| s.upload_ledger(p, Some(synced)));
        if let Some(p) = &tar {
            let _ = std::fs::remove_file(p);
        }
        let _ = s.release_lease();
        let _ = tx.send(v);
    });
    if let Ok(Some(Ok(v))) = rx.recv_timeout(Duration::from_secs(60)) {
        let mut saved = load();
        saved.synced_version = v;
        save(&saved);
    }
}

// ──────────────────────────────────────────────────────────────── from Preferences

/// Pairs this computer: probes the address, has the person compare the fingerprint, then pairs.
pub fn connect(app: &Rc<App>, address: String, code: String, done: Then) {
    let addr = address.clone();
    background(app, move || fingraph_client::probe(&addr), move |a, r| {
        let fp = match r {
            Ok((_, fp)) => fp,
            Err(e) => return a.toast(&e.to_string()),
        };
        let d = adw::AlertDialog::new(
            Some("Is This Your Server?"),
            Some(&format!("The server printed a fingerprint next to the pairing code. It should be\n\n{}", fingraph_client::short_fingerprint(&fp))),
        );
        d.add_responses(&[("cancel", "No"), ("go", "Yes, It Matches")]);
        d.set_response_appearance("go", adw::ResponseAppearance::Suggested);
        let (w, address, code, done) = (Rc::downgrade(a), address.clone(), code.clone(), done.clone());
        d.connect_response(None, move |_, resp| {
            let Some(a) = w.upgrade().filter(|_| resp == "go") else { return };
            let (address, code, fp, done) = (address.clone(), code.clone(), fp.clone(), done.clone());
            let name = glib::host_name().to_string();
            background(&a, move || fingraph_client::pair(&address, &code, Some(&fp), &name, "linux"), move |a, r| match r {
                Ok(server) => {
                    save(&Saved { server: Some(server), synced_version: 0 });
                    start(a);
                    done(a);
                }
                Err(e) => a.toast(&e.to_string()),
            });
        });
        d.present(Some(&a.win));
    });
}

/// Replaces the server's ledger with this computer's (moving to the server, or overriding it).
pub fn send_ledger(app: &Rc<App>) {
    let Some(s) = server() else { return };
    let tar = match snapshot(app) {
        Ok(p) => p,
        Err(e) => return app.toast(&format!("Couldn't prepare the ledger: {e}")),
    };
    let count = app.db.change_count();
    with(|l| l.busy = true);
    app.toast("Sending the ledger to FinGraph Server…");
    background(
        app,
        move || -> Result<i64, Error> {
            s.take_lease()?;
            let v = s.upload_ledger(&tar, None);
            let _ = std::fs::remove_file(&tar);
            v
        },
        move |a, r| {
            with(|l| l.busy = false);
            match r {
                Ok(v) => {
                    set_synced(v);
                    with(|l| {
                        l.holding = true;
                        l.elsewhere = None;
                        l.baseline = count;
                        l.error = None;
                    });
                    a.toast("The ledger is on FinGraph Server");
                }
                Err(e) => a.toast(&format!("Couldn't send the ledger: {e}")),
            }
        },
    );
}

/// Replaces this computer's ledger with the server's.
pub fn use_server_ledger(app: &Rc<App>) {
    set_synced(-1);
    sync_in(app, false);
}

/// Unpairs this computer and forgets the server; the ledger here stays as it is.
pub fn disconnect(app: &Rc<App>) {
    let Some(s) = server() else { return };
    let holding = with(|l| l.holding).unwrap_or(false);
    LINK.with(|l| *l.borrow_mut() = None);
    save(&Saved::default());
    background(app, move || {
        if holding {
            let _ = s.release_lease();
        }
        s.unpair(s.device_id)
    }, |a, r| {
        a.toast(match r {
            Ok(()) => "Disconnected from FinGraph Server",
            Err(_) => "Disconnected here (the server couldn't be told; unpair this computer there with fingraph-server unpair)",
        });
    });
}
