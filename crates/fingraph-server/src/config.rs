//! Settings from the environment, with the defaults the Docker image uses.
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    /// The ledger, the server's own state, its certificate and the backups.
    pub data: PathBuf,
    /// Every statement file, filed as `YYYY/MM Mon/<original name>`, with `inbox/` and `needs-attention/`.
    pub statements: PathBuf,
    /// Where the server listens (`0.0.0.0:8743`).
    pub listen: String,
    /// How phones and computers reach it (`192.168.1.20:8743`), for the pairing QR code. Optional.
    pub address: Option<String>,
    /// How often the inbox is looked at. Polling, not file events, so NFS and SMB mounts work too.
    pub inbox_every: Duration,
    /// A file younger than this may still be arriving over the network and is left for the next look.
    pub settle: Duration,
    /// The hour (local time, `TZ`) of the nightly backup.
    pub backup_hour: u32,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let secs = |k: &str, d: u64| var(k).and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(d));
        Config {
            data: var("FINGRAPH_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data")),
            statements: var("FINGRAPH_STATEMENTS").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/statements")),
            listen: var("FINGRAPH_LISTEN").unwrap_or_else(|| "0.0.0.0:8743".into()),
            address: var("FINGRAPH_ADDRESS"),
            inbox_every: secs("FINGRAPH_INBOX_SECONDS", 15),
            settle: secs("FINGRAPH_SETTLE_SECONDS", 10),
            backup_hour: var("FINGRAPH_BACKUP_HOUR").and_then(|v| v.parse().ok()).filter(|h| *h < 24).unwrap_or(3),
        }
    }

    pub fn ledger(&self) -> PathBuf {
        self.data.join("ledger.db")
    }
    pub fn server_db(&self) -> PathBuf {
        self.data.join("server.db")
    }
    pub fn tls_dir(&self) -> PathBuf {
        self.data.join("tls")
    }
    pub fn backups(&self) -> PathBuf {
        self.data.join("backups")
    }
    pub fn inbox(&self) -> PathBuf {
        self.statements.join("inbox")
    }
    pub fn attention(&self) -> PathBuf {
        self.statements.join("needs-attention")
    }
}
