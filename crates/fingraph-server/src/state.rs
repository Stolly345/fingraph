//! The server's own state, apart from the ledger: paired devices, the pairing code, the edit lease and
//! which statements have been filed. A small SQLite file, so `fingraph-server pair` (run with `docker
//! exec` while the server runs) and the server can both use it safely.
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

pub type Result<T> = std::result::Result<T, String>;

fn e(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// Letters and digits that can't be mistaken for each other when read off a screen.
const CODE_CHARS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// How long a pairing code works.
pub const CODE_MINUTES: i64 = 15;
/// Wrong attempts before a code stops working.
pub const CODE_TRIES: i64 = 5;
/// A lease nobody has renewed for this long has been abandoned (a crash, a closed lid).
pub const LEASE_MINUTES: i64 = 30;

pub struct State {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub platform: String,
    pub created: String,
    pub last_seen: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Lease {
    pub device_id: i64,
    pub device: String,
    pub since: String,
    pub renewed: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system's random number source failed");
    b
}

/// `ABCD-EFGH` → `ABCDEFGH`, forgiving case, spaces and dashes.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// What a device sends to prove it knows the code *and* is talking to this server's certificate: a
/// relay in the middle has a different certificate, so its proof can't pass here.
pub fn pairing_proof(code: &str, fingerprint_hex: &str) -> String {
    sha256_hex(format!("fingraph-pair:{}:{}", normalize_code(code), fingerprint_hex.to_ascii_lowercase()).as_bytes())
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse(t: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(t).ok().map(|t| t.with_timezone(&Utc))
}

impl State {
    pub fn open(path: &Path) -> Result<State> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(e)?;
        }
        let conn = Connection::open(path).map_err(e)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(e)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(e)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS devices (id INTEGER PRIMARY KEY, name TEXT NOT NULL, platform TEXT NOT NULL,
                 token_sha256 TEXT NOT NULL UNIQUE, created TEXT NOT NULL, last_seen TEXT);
             CREATE TABLE IF NOT EXISTS pairing (id INTEGER PRIMARY KEY CHECK (id = 1), code TEXT NOT NULL,
                 expires TEXT NOT NULL, failures INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS lease (id INTEGER PRIMARY KEY CHECK (id = 1), device_id INTEGER NOT NULL,
                 since TEXT NOT NULL, renewed TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS filed (sha256 TEXT PRIMARY KEY, path TEXT NOT NULL, filed TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(e)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(State { conn })
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2", params![key, value]).map_err(e)?;
        Ok(())
    }

    /// Goes up by one each time the ledger changes, so a device can tell whether its copy is current.
    pub fn ledger_version(&self) -> i64 {
        self.meta("ledger_version").and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    pub fn bump_ledger_version(&self) -> Result<i64> {
        let v = self.ledger_version() + 1;
        self.set_meta("ledger_version", &v.to_string())?;
        self.set_meta("ledger_changed", &ts(now()))?;
        Ok(v)
    }

    // ------------------------------------------------------------------ pairing

    /// A new one-time pairing code (any earlier one stops working).
    pub fn new_pairing_code(&self) -> Result<String> {
        let code: String = random_bytes::<8>().iter().map(|b| CODE_CHARS[*b as usize % CODE_CHARS.len()] as char).collect();
        self.conn
            .execute(
                "INSERT INTO pairing (id, code, expires, failures) VALUES (1, ?1, ?2, 0)
                 ON CONFLICT(id) DO UPDATE SET code = ?1, expires = ?2, failures = 0",
                params![code, ts(now() + Duration::minutes(CODE_MINUTES))],
            )
            .map_err(e)?;
        Ok(code)
    }

    /// `ABCDEFGH` → `ABCD-EFGH`, as shown to people.
    pub fn display_code(code: &str) -> String {
        format!("{}-{}", &code[..4], &code[4..])
    }

    /// Checks a device's proof and, if it's right, pairs the device and returns its token. The code works
    /// once; a few wrong tries end it.
    pub fn pair(&self, proof: &str, fingerprint_hex: &str, name: &str, platform: &str) -> Result<(Device, String)> {
        let row: Option<(String, String, i64)> =
            self.conn.query_row("SELECT code, expires, failures FROM pairing WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().map_err(e)?;
        let Some((code, expires, failures)) = row else {
            return Err("No pairing code is active. Make one with: docker exec fingraph fingraph-server pair".into());
        };
        if failures >= CODE_TRIES || parse(&expires).is_none_or(|t| t < now()) {
            return Err("That pairing code has expired. Make a new one with: docker exec fingraph fingraph-server pair".into());
        }
        if !same(&pairing_proof(&code, fingerprint_hex), &proof.to_ascii_lowercase()) {
            self.conn.execute("UPDATE pairing SET failures = failures + 1 WHERE id = 1", []).map_err(e)?;
            let left = CODE_TRIES - failures - 1;
            return Err(if left > 0 {
                format!("That code isn't right ({left} {} left).", if left == 1 { "try" } else { "tries" })
            } else {
                "That code isn't right, and it has now stopped working. Make a new one with: docker exec fingraph fingraph-server pair".into()
            });
        }
        self.conn.execute("DELETE FROM pairing", []).map_err(e)?;
        let name = name.trim();
        let name = if name.is_empty() { "Device" } else { name };
        let name: String = name.chars().take(60).collect();
        let token: String = random_bytes::<32>().iter().map(|b| format!("{b:02x}")).collect();
        let created = ts(now());
        self.conn
            .execute(
                "INSERT INTO devices (name, platform, token_sha256, created) VALUES (?1, ?2, ?3, ?4)",
                params![name, platform.chars().take(20).collect::<String>(), sha256_hex(token.as_bytes()), created],
            )
            .map_err(e)?;
        let id = self.conn.last_insert_rowid();
        Ok((Device { id, name, platform: platform.into(), created, last_seen: None }, token))
    }

    /// The device a token belongs to (and notes that it was seen).
    pub fn device_for_token(&self, token: &str) -> Option<Device> {
        let hash = sha256_hex(token.as_bytes());
        let d = self.conn
            .query_row("SELECT id, name, platform, created, last_seen FROM devices WHERE token_sha256 = ?1", [&hash], |r| {
                Ok(Device { id: r.get(0)?, name: r.get(1)?, platform: r.get(2)?, created: r.get(3)?, last_seen: r.get(4)? })
            })
            .optional()
            .ok()
            .flatten()?;
        let _ = self.conn.execute("UPDATE devices SET last_seen = ?1 WHERE id = ?2", params![ts(now()), d.id]);
        Some(d)
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let mut st = self.conn.prepare("SELECT id, name, platform, created, last_seen FROM devices ORDER BY id").map_err(e)?;
        st.query_map([], |r| Ok(Device { id: r.get(0)?, name: r.get(1)?, platform: r.get(2)?, created: r.get(3)?, last_seen: r.get(4)? }))
            .map_err(e)?
            .collect::<rusqlite::Result<_>>()
            .map_err(e)
    }

    /// Unpairs a device: its token stops working at once, and a lease it held is released.
    pub fn revoke(&self, id: i64) -> Result<bool> {
        self.conn.execute("DELETE FROM lease WHERE device_id = ?1", [id]).map_err(e)?;
        Ok(self.conn.execute("DELETE FROM devices WHERE id = ?1", [id]).map_err(e)? > 0)
    }

    // ------------------------------------------------------------------ the edit lease

    /// Who may change the ledger right now; an abandoned lease is cleared.
    pub fn lease(&self) -> Option<Lease> {
        let l: Option<Lease> = self.conn
            .query_row("SELECT l.device_id, d.name, l.since, l.renewed FROM lease l JOIN devices d ON d.id = l.device_id WHERE l.id = 1", [], |r| {
                Ok(Lease { device_id: r.get(0)?, device: r.get(1)?, since: r.get(2)?, renewed: r.get(3)? })
            })
            .optional()
            .ok()
            .flatten();
        match l {
            Some(l) if parse(&l.renewed).is_some_and(|t| t + Duration::minutes(LEASE_MINUTES) > now()) => Some(l),
            Some(_) => {
                let _ = self.conn.execute("DELETE FROM lease", []);
                None
            }
            None => None,
        }
    }

    /// Takes or renews the lease for `device`. Fails with the holder's name if another device has it.
    pub fn take_lease(&self, device: &Device) -> std::result::Result<Lease, Lease> {
        if let Some(l) = self.lease() {
            if l.device_id != device.id {
                return Err(l);
            }
            let renewed = ts(now());
            let _ = self.conn.execute("UPDATE lease SET renewed = ?1 WHERE id = 1", [&renewed]);
            return Ok(Lease { renewed, ..l });
        }
        let t = ts(now());
        let _ = self.conn.execute("INSERT OR REPLACE INTO lease (id, device_id, since, renewed) VALUES (1, ?1, ?2, ?2)", params![device.id, t]);
        Ok(Lease { device_id: device.id, device: device.name.clone(), since: t.clone(), renewed: t })
    }

    /// Gives the lease back; only its holder can.
    pub fn release_lease(&self, device: &Device) -> bool {
        self.conn.execute("DELETE FROM lease WHERE device_id = ?1", [device.id]).map(|n| n > 0).unwrap_or(false)
    }

    // ------------------------------------------------------------------ filed statements

    pub fn filed(&self, sha256: &str) -> Option<String> {
        self.conn.query_row("SELECT path FROM filed WHERE sha256 = ?1", [sha256], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_filed(&self, sha256: &str, rel: &str) -> Result<()> {
        self.conn
            .execute("INSERT INTO filed (sha256, path, filed) VALUES (?1, ?2, ?3) ON CONFLICT(sha256) DO UPDATE SET path = ?2", params![sha256, rel, ts(now())])
            .map_err(e)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let s = State::open(&dir.path().join("server.db")).unwrap();
        (dir, s)
    }

    #[test]
    fn pairing_works_once_and_only_with_the_right_proof_and_certificate() {
        let (_d, s) = state();
        let code = s.new_pairing_code().unwrap();
        assert_eq!(code.len(), 8);
        let fp = "ab".repeat(32);
        // a relay in the middle presents its own certificate: same code, different fingerprint
        assert!(s.pair(&pairing_proof(&code, &"cd".repeat(32)), &fp, "Phone", "android").is_err());
        let shown = State::display_code(&code).to_lowercase();
        let (dev, token) = s.pair(&pairing_proof(&shown, &fp), &fp, " Alex's S24 ", "android").unwrap();
        assert_eq!(dev.name, "Alex's S24");
        assert_eq!(s.device_for_token(&token).unwrap().id, dev.id);
        assert!(s.device_for_token("nope").is_none());
        // used up
        assert!(s.pair(&pairing_proof(&code, &fp), &fp, "Again", "android").is_err());
    }

    #[test]
    fn a_code_stops_working_after_too_many_wrong_tries() {
        let (_d, s) = state();
        let code = s.new_pairing_code().unwrap();
        let fp = "00".repeat(32);
        for _ in 0..CODE_TRIES {
            assert!(s.pair(&pairing_proof("WRONGONE", &fp), &fp, "x", "y").is_err());
        }
        let err = s.pair(&pairing_proof(&code, &fp), &fp, "x", "y").unwrap_err();
        assert!(err.contains("expired"), "{err}");
    }

    #[test]
    fn one_device_holds_the_lease_and_revoking_frees_it() {
        let (_d, s) = state();
        let fp = "11".repeat(32);
        let c = s.new_pairing_code().unwrap();
        let (desk, _) = s.pair(&pairing_proof(&c, &fp), &fp, "Desktop", "linux").unwrap();
        let c = s.new_pairing_code().unwrap();
        let (phone, _) = s.pair(&pairing_proof(&c, &fp), &fp, "Phone", "android").unwrap();
        assert!(s.take_lease(&desk).is_ok());
        assert_eq!(s.take_lease(&phone).unwrap_err().device, "Desktop");
        assert!(!s.release_lease(&phone));
        assert!(s.take_lease(&desk).is_ok(), "the holder renews");
        assert!(s.revoke(desk.id).unwrap());
        assert!(s.lease().is_none());
        assert!(s.take_lease(&phone).is_ok());
    }

    #[test]
    fn an_abandoned_lease_expires() {
        let (_d, s) = state();
        let fp = "22".repeat(32);
        let c = s.new_pairing_code().unwrap();
        let (desk, _) = s.pair(&pairing_proof(&c, &fp), &fp, "Desktop", "linux").unwrap();
        s.take_lease(&desk).unwrap();
        let old = ts(now() - Duration::minutes(LEASE_MINUTES + 1));
        s.conn.execute("UPDATE lease SET renewed = ?1", [old]).unwrap();
        assert!(s.lease().is_none());
    }
}
