//! The client against a real server (TLS and all) on a free local port.
use fingraph_client::{Error, Server};
use fingraph_server::config::Config;
use fingraph_server::state::State;
use std::time::Duration;

struct Running {
    dir: tempfile::TempDir,
    address: String,
    cfg: Config,
    handle: axum_server::Handle,
    _rt: tokio::runtime::Runtime,
}

fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data: dir.path().join("data"),
        statements: dir.path().join("statements"),
        listen: "127.0.0.1:0".into(),
        address: None,
        inbox_every: Duration::from_millis(200),
        settle: Duration::ZERO,
        backup_hour: 3,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = axum_server::Handle::new();
    let (h, c) = (handle.clone(), cfg.clone());
    rt.spawn(async move {
        if let Err(e) = fingraph_server::run(c, h, false).await {
            panic!("server: {e}");
        }
    });
    let addr = rt.block_on(async { tokio::time::timeout(Duration::from_secs(20), handle.listening()).await.unwrap().unwrap() });
    Running { address: format!("127.0.0.1:{}", addr.port()), cfg, handle, _rt: rt, dir }
}

fn code(r: &Running) -> String {
    State::open(&r.cfg.server_db()).unwrap().new_pairing_code().unwrap()
}

fn pair(r: &Running, name: &str) -> Server {
    // the server allows one pairing try a second
    std::thread::sleep(Duration::from_millis(1100));
    fingraph_client::pair(&r.address, &code(r), None, name, "linux").unwrap()
}

#[test]
fn a_desktop_pairs_moves_its_ledger_in_and_a_second_one_takes_over() {
    let r = start();
    let (info, fp) = fingraph_client::probe(&r.address).unwrap();
    assert_eq!(info.product, "fingraph-server");
    assert_eq!(fp.len(), 64);

    // a wrong code is refused, with how many tries are left
    code(&r);
    let bad = fingraph_client::pair(&r.address, "AAAA-BBBB", None, "x", "linux");
    assert!(matches!(&bad, Err(Error::Refused(m)) if m.contains("isn't right")), "{bad:?}");

    let desk = pair(&r, "Desktop");
    assert_eq!(desk.fingerprint, fp);
    let st = desk.status().unwrap();
    assert_eq!((st.ledger_version, st.workspaces), (0, 0));

    // move a ledger in
    let local = fingraph_core::db::Db::open(&r.dir.path().join("desk/ledger.db")).unwrap();
    local.load_demo().unwrap();
    let tar = r.dir.path().join("desk.tar");
    local.backup_archive(&tar).unwrap();
    let (lease, v) = desk.take_lease().unwrap();
    assert_eq!((lease.device.as_str(), v), ("Desktop", 0));
    assert_eq!(desk.upload_ledger(&tar, Some(0)).unwrap(), 1);
    assert_eq!(desk.status().unwrap().workspaces, 2);

    // a laptop sees who's editing, then gets it once the desktop is done
    let laptop = pair(&r, "Laptop");
    assert!(matches!(laptop.take_lease(), Err(Error::Conflict(m)) if m.contains("Desktop")));
    desk.release_lease().unwrap();
    let (_, v) = laptop.take_lease().unwrap();
    assert_eq!(v, 1);
    let got = r.dir.path().join("laptop.tar");
    assert_eq!(laptop.download_ledger(&got).unwrap(), 1);
    let copy = fingraph_core::db::Db::open(&r.dir.path().join("laptop/ledger.db")).unwrap();
    copy.restore_from(&got).unwrap();
    assert_eq!(copy.entities().unwrap().len(), 2);

    // the phone's views read the same ledger
    let summary = laptop.view("summary", "month=2026-09").unwrap();
    assert_eq!(summary["month"], "2026-09");

    // an old copy can't overwrite a newer ledger
    assert_eq!(laptop.upload_ledger(&got, Some(1)).unwrap(), 2);
    assert!(matches!(laptop.upload_ledger(&got, Some(1)), Err(Error::Conflict(_))));

    // unpairing ends access
    laptop.unpair(desk.device_id).unwrap();
    assert!(matches!(desk.status(), Err(Error::NotPaired(_))));
    r.handle.shutdown();
}

#[test]
fn a_different_server_at_the_same_address_is_refused() {
    let r = start();
    let mut s = pair(&r, "Desktop");
    s.fingerprint = "00".repeat(32);
    assert_eq!(s.status().unwrap_err(), Error::WrongServer);
    // and a QR code with the wrong fingerprint can't pair
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(fingraph_client::pair(&r.address, &code(&r), Some(&"11".repeat(32)), "x", "linux").unwrap_err(), Error::WrongServer);
    r.handle.shutdown();
}

#[test]
fn a_statement_sent_from_a_device_is_queued_in_the_inbox() {
    let r = start();
    let s = pair(&r, "Phone");
    let pdf = r.dir.path().join("Nov statement.pdf");
    std::fs::write(&pdf, b"%PDF-1.4 not really").unwrap();
    // the inbox may pick it up at once (and file it under needs-attention); either way it arrived
    assert_eq!(s.send_statement(&pdf).unwrap(), "Nov statement.pdf");
    std::thread::sleep(Duration::from_millis(600));
    let inbox = r.cfg.inbox().join("Nov statement.pdf").exists();
    let attention = r.cfg.attention().join("Nov statement.pdf").exists();
    assert!(inbox || attention);
    r.handle.shutdown();
}
