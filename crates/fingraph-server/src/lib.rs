//! FinGraph Server: holds the ledger and the statements, so the desktop and the phone can share them.
//!
//! ```text
//! fingraph-server            run the server
//! fingraph-server pair       make a one-time pairing code (and QR code) for a new device
//! fingraph-server devices    list paired devices
//! fingraph-server unpair ID  unpair one
//! fingraph-server backup     write a complete backup now
//! ```
pub mod api;
pub mod backups;
pub mod captures;
pub mod config;
pub mod filing;
pub mod inbox;
pub mod ledger;
pub mod state;
pub mod tls;
pub mod views;

use api::log;
use config::Config;
use state::State;
use std::time::Duration;

/// Runs the command line: the server itself or one of its commands.
pub fn run_cli() {
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("serve") => serve(cfg),
        Some("pair") => pair(&cfg),
        Some("devices") => devices(&cfg),
        Some("unpair") => unpair(&cfg, args.get(1)),
        Some("backup") => backup_now(&cfg),
        Some("--version" | "version") => {
            println!("fingraph-server {}", api::VERSION);
            Ok(())
        }
        Some(other) => Err(format!("unknown command {other:?}; try pair, devices, unpair ID or backup")),
    };
    if let Err(e) = code {
        eprintln!("fingraph-server: {e}");
        std::process::exit(1);
    }
}

fn open_state(cfg: &Config) -> Result<State, String> {
    State::open(&cfg.server_db())
}

/// Prints a pairing code, the fingerprint to compare, and a QR code the phone can scan.
fn pair(cfg: &Config) -> Result<(), String> {
    let id = tls::load_or_create(&cfg.tls_dir(), cfg.address.as_deref())?;
    let code = open_state(cfg)?.new_pairing_code()?;
    print_pairing(cfg, &code, &id.fingerprint);
    Ok(())
}

fn print_pairing(cfg: &Config, code: &str, fingerprint: &str) {
    println!();
    println!("  Pairing code:        {}", State::display_code(code));
    println!("  Server fingerprint:  {}", tls::short_fingerprint(fingerprint));
    println!("  Works once, for {} minutes.", state::CODE_MINUTES);
    match &cfg.address {
        Some(addr) => {
            let url = format!("fingraph://pair?a={addr}&c={code}&f={fingerprint}");
            if let Ok(qr) = qrcode::QrCode::new(url.as_bytes()) {
                println!();
                println!("  Scan this with the FinGraph phone app (Connect to a server):");
                println!();
                let s = qr.render::<qrcode::render::unicode::Dense1x2>().dark_color(qrcode::render::unicode::Dense1x2::Light).light_color(qrcode::render::unicode::Dense1x2::Dark).quiet_zone(true).build();
                for line in s.lines() {
                    println!("  {line}");
                }
            }
            println!();
            println!("  Or type the address {addr} and the code in FinGraph.");
        }
        None => {
            println!();
            println!("  Type this server's address (for example 192.168.1.20:8743) and the code in FinGraph.");
            println!("  Set FINGRAPH_ADDRESS to that address to get a QR code for the phone here too.");
        }
    }
    println!();
}

fn devices(cfg: &Config) -> Result<(), String> {
    let s = open_state(cfg)?;
    let list = s.devices()?;
    if list.is_empty() {
        println!("No devices are paired. Pair one with: fingraph-server pair");
    }
    let lease = s.lease();
    for d in list {
        let editing = if lease.as_ref().is_some_and(|l| l.device_id == d.id) { "  (editing now)" } else { "" };
        println!("{:>3}  {:<28} {:<8} paired {}  last seen {}{editing}", d.id, d.name, d.platform, &d.created[..10], d.last_seen.as_deref().map(|t| &t[..16]).unwrap_or("never"));
    }
    Ok(())
}

fn unpair(cfg: &Config, id: Option<&String>) -> Result<(), String> {
    let s = open_state(cfg)?;
    let arg = id.ok_or("say which device: fingraph-server unpair ID (see fingraph-server devices)")?;
    let target = match arg.parse::<i64>() {
        Ok(n) => Some(n),
        Err(_) => s.devices()?.into_iter().find(|d| d.name.eq_ignore_ascii_case(arg)).map(|d| d.id),
    };
    match target {
        Some(n) if s.revoke(n)? => {
            println!("Unpaired. That device can't reach the server any more.");
            Ok(())
        }
        _ => Err(format!("no paired device {arg:?}")),
    }
}

fn backup_now(cfg: &Config) -> Result<(), String> {
    let db = fingraph_core::db::Db::open(&cfg.ledger()).map_err(|e| e.to_string())?;
    let (path, note) = backups::write(&db, &cfg.backups(), chrono::Local::now().date_naive())?;
    println!("Backed up to {}", path.display());
    if let Some(n) = note {
        println!("Only the ledger was backed up: {n}");
    }
    Ok(())
}

/// Starts the server and runs until stopped.
pub fn serve(cfg: Config) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let handle = axum_server::Handle::new();
        let h2 = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            log("stopping");
            h2.graceful_shutdown(Some(Duration::from_secs(10)));
        });
        run(cfg, handle, true).await
    })
}

/// The server on its own runtime-less part: sets up, starts the inbox and backup loops and serves until
/// `handle` says stop. `announce` prints the first-run pairing code.
pub async fn run(cfg: Config, handle: axum_server::Handle, announce: bool) -> Result<(), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    for d in [&cfg.data, &cfg.statements] {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    for d in [cfg.inbox(), cfg.attention()] {
        std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let _ = std::fs::remove_dir_all(cfg.data.join("tmp"));
    let id = tls::load_or_create(&cfg.tls_dir(), cfg.address.as_deref())?;
    let ledger = ledger::Ledger::open(&cfg.ledger())?;
    let state = open_state(&cfg)?;
    let first_run = state.devices()?.is_empty();
    if first_run && announce {
        let code = state.new_pairing_code()?;
        log("No devices are paired yet. Pair the desktop or phone with this code:");
        print_pairing(&cfg, &code, &id.fingerprint);
    }
    let app = api::App::new(cfg.clone(), ledger, state, id.fingerprint.clone());
    {
        // statements the ledger has that aren't filed yet (a ledger moved here by hand, say)
        let a = app.clone();
        let filed = app.ledger.call(move |db| inbox::file_missing(db, &a.state(), &a.cfg.statements)).await.unwrap_or(0);
        if filed > 0 {
            log(&format!("filed {filed} statement{}", if filed == 1 { "" } else { "s" }));
        }
        tokio::spawn(inbox_loop(app.clone()));
        tokio::spawn(backup_loop(app.clone()));
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&id.cert, &id.key).await.map_err(|e| format!("TLS: {e}"))?;
        let addr: std::net::SocketAddr = cfg.listen.parse().map_err(|e| format!("FINGRAPH_LISTEN {:?}: {e}", cfg.listen))?;
        log(&format!("FinGraph Server {} listening on https://{addr} (fingerprint {})", api::VERSION, tls::short_fingerprint(&id.fingerprint)));
        axum_server::bind_rustls(addr, tls).handle(handle).serve(api::router(app).into_make_service()).await.map_err(|e| e.to_string())
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Looks at the inbox every so often. While a device holds the edit lease the inbox waits: the device
/// has its own copy of the ledger, and its upload would overwrite anything imported meanwhile.
async fn inbox_loop(app: api::Shared) {
    let mut tick = tokio::time::interval(app.cfg.inbox_every);
    let mut waiting_said = false;
    loop {
        tick.tick().await;
        // what the phone added while a computer was editing, now that it's done
        if app.state().lease().is_none() && !captures::waiting(&app.cfg.data).is_empty() {
            let data = app.cfg.data.clone();
            if let Ok((n, said)) = app.ledger.call(move |db| captures::apply_waiting(db, &data)).await {
                for m in said {
                    log(&m);
                }
                if n > 0 {
                    let _ = app.state().bump_ledger_version();
                }
            }
        }
        let files = inbox::ready_files(&app.cfg.inbox(), app.cfg.settle);
        if files.is_empty() {
            continue;
        }
        if let Some(l) = app.state().lease() {
            if !waiting_said {
                log(&format!("{} file{} in the inbox wait until {} is done editing", files.len(), if files.len() == 1 { "" } else { "s" }, l.device));
                waiting_said = true;
            }
            continue;
        }
        waiting_said = false;
        let Ok(profiles) = app.ledger.call(|db| db.profiles().unwrap_or_default()).await else { continue };
        let mut changed = false;
        for path in files {
            let p2 = path.clone();
            let pr = profiles.clone();
            // reading a PDF takes a while (pdftotext, maybe OCR): off the ledger's thread
            let scanned = tokio::task::spawn_blocking(move || -> Result<(String, Result<fingraph_core::parser::ScanResult, String>), String> {
                let sha = fingraph_core::db::file_sha256(&p2).map_err(|e| e.to_string())?;
                let scan = fingraph_core::parser::MultiPassScanner::new(&pr).scan_file(&p2).map_err(|e| e.to_string());
                Ok((sha, scan))
            })
            .await;
            let Ok(Ok((sha, scan))) = scanned else { continue };
            let a = app.clone();
            let p3 = path.clone();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match app.ledger.call(move |db| inbox::process(db, &a.state(), &a.cfg.statements, &p3, &sha, scan)).await {
                Ok(inbox::Outcome::Imported { filed, inserted, held }) => {
                    changed = true;
                    let held = if held > 0 { format!(", {held} section{} held for review", if held == 1 { "" } else { "s" }) } else { String::new() };
                    log(&format!("imported {name}: {inserted} transactions{held}; filed as {}", filed.display()));
                }
                Ok(inbox::Outcome::AlreadyImported { filed }) => log(&format!("{name} was already imported; filed as {}", filed.display())),
                Ok(inbox::Outcome::NeedsAttention { reason }) => log(&format!("{name} needs attention: {reason}")),
                Err(e) => log(&format!("{name}: {e}")),
            }
        }
        if changed {
            let _ = app.state().bump_ledger_version();
        }
    }
}

/// The nightly backup, at `FINGRAPH_BACKUP_HOUR` (03:00 unless set), and one on the first start.
async fn backup_loop(app: api::Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    loop {
        tick.tick().await;
        let now = chrono::Local::now();
        let today = now.date_naive();
        let last = backups::latest(&app.cfg.backups()).map(|(_, d)| d);
        let due = match last {
            None => true,
            Some(d) => d < today && chrono::Timelike::hour(&now) >= app.cfg.backup_hour,
        };
        if !due {
            continue;
        }
        let dir = app.cfg.backups();
        match app.ledger.call(move |db| backups::write(db, &dir, today)).await {
            Ok(Ok((path, note))) => {
                let pruned = backups::prune(&app.cfg.backups());
                let note = note.map(|n| format!(" (ledger only: {n})")).unwrap_or_default();
                log(&format!("backed up to {}{note}{}", path.display(), if pruned > 0 { format!("; {pruned} old backup(s) removed") } else { String::new() }));
            }
            Ok(Err(e)) | Err(e) => log(&format!("the backup failed: {e}")),
        }
    }
}
