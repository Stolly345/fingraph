//! A command-line client for trying a FinGraph server by hand.
//!
//! ```text
//! cargo run -p fingraph-client --example fingraph-remote -- pair ADDRESS CODE [NAME]   (saves ./remote.json)
//! cargo run -p fingraph-client --example fingraph-remote -- status | devices | view NAME [QUERY]
//! cargo run -p fingraph-client --example fingraph-remote -- send FILE.pdf | upload BACKUP.tar | download OUT.tar
//! ```
use fingraph_client::Server;

fn saved() -> Server {
    let s = std::fs::read_to_string("remote.json").expect("pair first (remote.json not found)");
    serde_json::from_str(&s).expect("remote.json")
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| a.get(i).cloned().unwrap_or_else(|| panic!("missing argument {i}"));
    let r: Result<String, fingraph_client::Error> = match a.first().map(String::as_str) {
        Some("probe") => fingraph_client::probe(&arg(1)).map(|(i, fp)| format!("{} {} api {} · fingerprint {}", i.product, i.version, i.api, fingraph_client::short_fingerprint(&fp))),
        Some("pair") => fingraph_client::pair(&arg(1), &arg(2), None, a.get(3).map(String::as_str).unwrap_or("fingraph-remote"), "cli").map(|s| {
            std::fs::write("remote.json", serde_json::to_string_pretty(&s).unwrap()).unwrap();
            format!("paired as device {}; saved remote.json", s.device_id)
        }),
        Some("status") => saved().status().map(|s| format!("{s:#?}")),
        Some("devices") => saved().devices().map(|d| format!("{d:#?}")),
        Some("view") => saved().view(&arg(1), a.get(2).map(String::as_str).unwrap_or("")).map(|v| serde_json::to_string_pretty(&v).unwrap()),
        Some("send") => saved().send_statement(std::path::Path::new(&arg(1))).map(|n| format!("queued {n}")),
        Some("lease") => saved().take_lease().map(|(l, v)| format!("{l:?} at version {v}")),
        Some("release") => saved().release_lease().map(|_| "released".into()),
        Some("upload") => {
            let s = saved();
            s.take_lease().and_then(|(_, v)| s.upload_ledger(std::path::Path::new(&arg(1)), Some(v))).map(|v| format!("server ledger now version {v}"))
        }
        Some("demo-tar") => {
            // a demo ledger as a complete backup, to try a server without real data
            let dir = tempfile::tempdir().unwrap();
            let db = fingraph_core::db::Db::open(&dir.path().join("ledger.db")).unwrap();
            db.load_demo().unwrap();
            db.backup_archive(std::path::Path::new(&arg(1))).unwrap();
            Ok(format!("wrote {}", arg(1)))
        }
        Some("download") => saved().download_ledger(std::path::Path::new(&arg(1))).map(|v| format!("version {v} saved")),
        _ => Ok("commands: probe ADDRESS | pair ADDRESS CODE [NAME] | status | devices | view NAME [QUERY] | send PDF | lease | release | upload TAR | download TAR".into()),
    };
    match r {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
