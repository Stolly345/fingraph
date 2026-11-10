//! What the phone adds: a quick purchase or a receipt photo. Applied to the ledger at once, or, while a
//! computer holds the edit lease (and has its own copy that would overwrite them), kept in
//! `/data/captures` and applied as soon as it's done.
use chrono::NaiveDate;
use fingraph_core::db::Db;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Capture {
    Purchase { account: i64, date: NaiveDate, payee: String, amount: Decimal },
    /// The photo is kept beside the capture's JSON file, with the same name and `.jpg`.
    Receipt { transaction: i64 },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseBody {
    pub account: i64,
    pub date: String,
    pub payee: String,
    pub amount: String,
}

impl PurchaseBody {
    /// Checks what the phone sent.
    pub fn validate(&self, today: NaiveDate) -> Result<Capture, String> {
        let date: NaiveDate = self.date.parse().map_err(|_| "That date isn't valid.".to_string())?;
        if date > today || (today - date).num_days() > 366 {
            return Err("The date should be within the last year.".into());
        }
        let amount: Decimal = self.amount.trim().parse().map_err(|_| "That amount isn't a number.".to_string())?;
        if amount.is_zero() || amount.abs() > Decimal::from(10_000_000) {
            return Err("That amount doesn't look right.".into());
        }
        let payee: String = self.payee.trim().chars().filter(|c| !c.is_control()).take(120).collect();
        if payee.is_empty() {
            return Err("Say where the money went.".into());
        }
        Ok(Capture::Purchase { account: self.account, date, payee, amount: amount.round_dp(2) })
    }
}

/// Applies one capture; `photo` is the receipt's image for a receipt.
pub fn apply(db: &Db, c: &Capture, photo: Option<&Path>) -> Result<String, String> {
    match c {
        Capture::Purchase { account, date, payee, amount } => {
            if !db.accounts().map_err(|e| e.to_string())?.iter().any(|a| a.id == *account) {
                return Err("That account isn't in the ledger any more.".into());
            }
            db.add_phone_purchase(*account, *date, payee, *amount).map_err(|e| e.to_string())?;
            Ok(format!("added {payee} {amount}"))
        }
        Capture::Receipt { transaction } => {
            let photo = photo.ok_or("the photo is missing")?;
            db.attach(*transaction, photo).map_err(|e| e.to_string())?;
            Ok(format!("receipt attached to transaction {transaction}"))
        }
    }
}

fn dir(data: &Path) -> PathBuf {
    data.join("captures")
}

/// Keeps a capture for later. Returns its file.
pub fn queue(data: &Path, c: &Capture, device: &str, photo: Option<&[u8]>) -> Result<PathBuf, String> {
    let d = dir(data);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    // named by time, so they're applied in the order they were made
    let stem = format!("{}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%.6f"), crate::state::sha256_hex(&crate::state::random_bytes::<4>()));
    if let Some(bytes) = photo {
        std::fs::write(d.join(format!("{stem}.jpg")), bytes).map_err(|e| e.to_string())?;
    }
    let mut v = serde_json::to_value(c).map_err(|e| e.to_string())?;
    v["device"] = serde_json::json!(device);
    let path = d.join(format!("{stem}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(path)
}

/// The captures waiting, oldest first.
pub fn waiting(data: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir(data)) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    v.sort();
    v
}

/// Applies every waiting capture. Returns (applied, messages). One that can't be applied moves to
/// `captures/failed/` with the reason, so it doesn't block the rest.
pub fn apply_waiting(db: &Db, data: &Path) -> (usize, Vec<String>) {
    let mut n = 0;
    let mut said = Vec::new();
    for path in waiting(data) {
        let photo = path.with_extension("jpg");
        let r = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Capture>(&b).map_err(|e| e.to_string())).and_then(|c| apply(db, &c, photo.exists().then_some(photo.as_path())));
        match r {
            Ok(msg) => {
                n += 1;
                said.push(msg);
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&photo);
            }
            Err(e) => {
                said.push(format!("a capture couldn't be applied: {e}"));
                let failed = dir(data).join("failed");
                let _ = std::fs::create_dir_all(&failed);
                for p in [&path, &photo] {
                    if p.exists() {
                        let _ = std::fs::rename(p, failed.join(p.file_name().unwrap_or_default()));
                    }
                }
                let _ = std::fs::write(failed.join(format!("{}.txt", path.file_stem().unwrap_or_default().to_string_lossy())), e);
            }
        }
    }
    (n, said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fingraph_core::parser::MultiPassScanner;

    #[test]
    fn checks_what_the_phone_sends() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let ok = PurchaseBody { account: 1, date: "2026-11-09".into(), payee: " Kirk Market ".into(), amount: "-23.456".into() }.validate(today).unwrap();
        assert_eq!(ok, Capture::Purchase { account: 1, date: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(), payee: "Kirk Market".into(), amount: "-23.46".parse().unwrap() });
        for (d, p, a) in [("2026-11-11", "x", "-1"), ("2024-01-01", "x", "-1"), ("2026-11-09", "", "-1"), ("2026-11-09", "x", "0"), ("2026-11-09", "x", "abc")] {
            assert!(PurchaseBody { account: 1, date: d.into(), payee: p.into(), amount: a.into() }.validate(today).is_err(), "{d} {p} {a}");
        }
    }

    #[test]
    fn a_phone_purchase_gives_way_to_the_statement_line_and_keeps_its_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("ledger.db")).unwrap();
        let ent = db.create_entity("Personal", fingraph_core::EntityKind::Personal, fingraph_core::money::Currency::KYD).unwrap();
        let docs = fingraph_core::demo::demo_documents();
        let profiles = db.profiles().unwrap();
        let scanner = MultiPassScanner::new(&profiles);
        let first = scanner.scan_tokens(docs[0].tokens.clone()).unwrap();
        db.import_auto(ent, "one.pdf", &"a".repeat(64), &first).unwrap();
        let account = db.accounts().unwrap()[0].id;
        // the next month's Scotiabank statement has a cash withdrawal; the phone saw it first, a day earlier
        let second_doc = docs.iter().skip(1).find(|d| d.filename.starts_with("Scotiabank")).unwrap();
        let second = scanner.scan_tokens(second_doc.tokens.clone()).unwrap();
        let abm = second.sections[0].transactions.iter().find(|t| t.description.contains("ABM")).unwrap().clone();
        let c = Capture::Purchase { account, date: abm.posted - chrono::Duration::days(1), payee: "Cash".into(), amount: abm.amount };
        apply(&db, &c, None).unwrap();
        let phone_id = db.transactions(&Default::default()).unwrap().into_iter().find(|t| t.memo == fingraph_core::db::PHONE_MEMO).unwrap().id;
        let photo = dir.path().join("r.jpg");
        std::fs::write(&photo, b"jpeg").unwrap();
        apply(&db, &Capture::Receipt { transaction: phone_id }, Some(&photo)).unwrap();

        db.import_auto(ent, "two.pdf", &"b".repeat(64), &second).unwrap();
        let rows = db.transactions(&Default::default()).unwrap();
        assert!(rows.iter().all(|t| t.memo != fingraph_core::db::PHONE_MEMO), "the phone's row gave way");
        let line = rows.iter().find(|t| t.posted == abm.posted && t.amount == abm.amount).unwrap();
        assert_eq!(line.attachments, 1, "its receipt moved to the statement's line");
    }

    #[test]
    fn waiting_captures_are_applied_in_order_and_bad_ones_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let account = db.accounts().unwrap()[0].id;
        let d = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        queue(dir.path(), &Capture::Purchase { account, date: d, payee: "Coffee".into(), amount: "-4.50".parse().unwrap() }, "Phone", None).unwrap();
        queue(dir.path(), &Capture::Purchase { account: 999_999, date: d, payee: "Nowhere".into(), amount: "-1".parse().unwrap() }, "Phone", None).unwrap();
        assert_eq!(waiting(dir.path()).len(), 2);
        let (n, said) = apply_waiting(&db, dir.path());
        assert_eq!(n, 1, "{said:?}");
        assert!(waiting(dir.path()).is_empty());
        assert_eq!(std::fs::read_dir(dir.path().join("captures/failed")).unwrap().count(), 2, "the capture and its reason");
    }
}
