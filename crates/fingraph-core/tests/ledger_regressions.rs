//! What the 0.2.13 review found the ledger could lose or misfile, each held to the fix. In-memory or
//! temp ledgers only.
use chrono::NaiveDate;
use fingraph_core::db::{Db, Target, TxFilter};
use fingraph_core::demo::{render, Bank, DemoTx, StatementSpec};
use fingraph_core::parser::{MultiPassScanner, ScanResult};
use fingraph_core::{AccountType, Currency, EntityKind};
use rust_decimal::Decimal;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;

fn d(m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, m, day).unwrap()
}
fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}
fn scan(db: &Db, specs: &[StatementSpec]) -> ScanResult {
    MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(render(specs)).unwrap()
}
fn all(db: &Db) -> Vec<fingraph_core::db::TxRow> {
    db.transactions(&TxFilter::default()).unwrap()
}
fn count(db: &Db, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn spec(last4: &str, ccy: Currency, open: &str, period: (NaiveDate, NaiveDate), txs: Vec<DemoTx>) -> StatementSpec {
    StatementSpec::new(Bank::Scotiabank, AccountType::Checking, ccy, last4, dec(open), period, txs)
}
fn first_demo(db: &Db) -> (String, String, i64, i64) {
    db.conn
        .query_row("SELECT d.sha256_hash, d.filename, a.entity_id, d.id FROM documents d JOIN accounts a ON a.id = d.account_id WHERE d.section_index = 0 ORDER BY d.id LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
}
fn demo_scan(db: &Db, name: &str) -> ScanResult {
    let doc = fingraph_core::demo::demo_documents().into_iter().find(|d| d.filename == name).unwrap();
    MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(doc.tokens).unwrap()
}
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fg-ledger-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_failed_replace_leaves_the_statement_and_your_edits() {
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let mk = || spec("2045", Currency::KYD, "1000.00", (d(1, 16), d(2, 15)), vec![DemoTx::new(d(1, 20), "POS PURCHASE", dec("-62.10")).memo("KIRK")]);
    let r = db.import_auto(ent, "a.pdf", "sha-a", &scan(&db, &[mk()])).unwrap();
    db.approve_document(r.document_ids[0]).unwrap();
    let t = all(&db)[0].clone();
    db.set_category(&[t.id], db.category_id("Groceries").unwrap()).unwrap();
    // a new account in a workspace that doesn't exist: the import fails part-way
    let s = scan(&db, &[mk()]);
    assert!(db.replace_with(&r.document_ids, 999_999, "b.pdf", "sha-b", &s, &[Target::New]).is_err());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM documents WHERE sha256_hash = 'sha-a'"), 1);
    assert_eq!(db.transaction(t.id).unwrap().category_id, db.category_id("Groceries").unwrap());
    // and a statement deleted while Rescan All runs isn't brought back
    db.delete_document(r.document_ids[0]).unwrap();
    assert!(db.replace_statement(ent, "a.pdf", "sha-a", &s).is_err());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM documents"), 0);
}

#[test]
fn rescan_keeps_your_memo_and_your_deletes() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let (sha, name, ent, doc) = first_demo(&db);
    let rows: Vec<_> = all(&db).into_iter().filter(|t| t.document_id == Some(doc)).collect();
    db.set_memo(rows[0].id, "my note: reimbursable").unwrap();
    db.delete_transactions(&[rows[1].id]).unwrap();
    db.replace_statement(ent, &name, &sha, &demo_scan(&db, &name)).unwrap();
    let after = all(&db);
    assert_eq!(after.iter().find(|x| x.fitid == rows[0].fitid).unwrap().memo, "my note: reimbursable");
    assert!(!after.iter().any(|x| x.fitid == rows[1].fitid), "a deleted row came back");
}

#[test]
fn an_unlinked_transfer_stays_unlinked() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let t = all(&db).into_iter().find(|t| t.transfer_peer.is_some()).unwrap();
    let peer = t.transfer_peer.unwrap();
    db.unlink_transfer(t.id).unwrap();
    db.match_transfers().unwrap();
    assert_eq!(db.transaction(t.id).unwrap().transfer_peer, None);
    assert_eq!(db.transaction(peer).unwrap().transfer_peer, None);
}

#[test]
fn deleting_one_of_two_overlapping_statements_keeps_the_shared_rows() {
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let a = spec("2045", Currency::KYD, "1000.00", (d(1, 16), d(2, 15)), vec![DemoTx::new(d(1, 20), "POS PURCHASE", dec("-62.10")).memo("KIRK"), DemoTx::new(d(2, 2), "ELECTRONIC CREDIT", dec("500.00")).memo("SALARY")]);
    let b = spec("2045", Currency::KYD, "1000.00", (d(2, 1), d(2, 28)), vec![DemoTx::new(d(2, 2), "ELECTRONIC CREDIT", dec("500.00")).memo("SALARY"), DemoTx::new(d(2, 20), "POS PURCHASE", dec("-10.00")).memo("FOSTERS")]);
    let ra = db.import_auto(ent, "a.pdf", "sha-a", &scan(&db, &[a])).unwrap();
    let rb = db.import_auto(ent, "b.pdf", "sha-b", &scan(&db, &[b])).unwrap();
    assert_eq!(rb.duplicates, 1);
    for id in ra.document_ids.iter().chain(&rb.document_ids) {
        let _ = db.approve_document(*id);
    }
    db.delete_document(ra.document_ids[0]).unwrap();
    let mut left: Vec<_> = all(&db).iter().map(|t| (t.posted, t.amount)).collect();
    left.sort();
    assert_eq!(left, vec![(d(2, 2), dec("500.00")), (d(2, 20), dec("-10.00"))]);
}

#[test]
fn a_held_copy_cannot_hide_a_clean_copys_rows() {
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let txs = || vec![DemoTx::new(d(1, 20), "POS PURCHASE", dec("-62.10")).memo("KIRK"), DemoTx::new(d(2, 2), "ELECTRONIC CREDIT", dec("500.00")).memo("SALARY")];
    // learn the layout first, so the clean copy isn't held as a new one
    let first = db.import_auto(ent, "dec.pdf", "sha-dec", &scan(&db, &[spec("2045", Currency::KYD, "500.00", (d(12, 16).with_year(2025).unwrap(), d(1, 15)), vec![DemoTx::new(d(1, 2), "ELECTRONIC CREDIT", dec("500.00")).memo("SALARY")])])).unwrap();
    db.approve_document(first.document_ids[0]).unwrap();
    let ocr = db.import_auto(ent, "ocr.pdf", "sha-ocr", &scan(&db, &[spec("2045", Currency::KYD, "1000.00", (d(1, 16), d(2, 15)), txs())])).unwrap();
    db.conn.execute("UPDATE documents SET review_hold = 1, review_note = 'ocr' WHERE id = ?1", [ocr.document_ids[0]]).unwrap();
    let clean = db.import_auto(ent, "real.pdf", "sha-real", &scan(&db, &[spec("2045", Currency::KYD, "1000.00", (d(1, 15), d(2, 15)), txs())])).unwrap();
    assert_eq!(clean.held, 0);
    let in_view = |db: &Db| all(db).iter().filter(|t| t.posted >= d(1, 16)).count();
    assert_eq!(in_view(&db), 2, "the clean copy's rows are out of view");
    // deleting the held copy now loses nothing
    db.delete_document(ocr.document_ids[0]).unwrap();
    assert_eq!(in_view(&db), 2);
}

trait WithYear {
    fn with_year(self, y: i32) -> Option<NaiveDate>;
}
impl WithYear for NaiveDate {
    fn with_year(self, y: i32) -> Option<NaiveDate> {
        chrono::Datelike::with_year(&self, y)
    }
}

#[test]
fn an_account_in_another_workspace_counts_in_that_workspaces_currency() {
    let db = Db::open_in_memory().unwrap();
    let personal = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let biz = db.create_entity("Biz", EntityKind::Business, Currency::USD).unwrap();
    let m = |mo: u32, open: &str| spec("2045", Currency::KYD, open, (d(mo, 16), d(mo + 1, 15)), vec![DemoTx::new(d(mo, 20), "POS PURCHASE", dec("-100.00")).memo("KIRK")]);
    db.import_auto(biz, "1.pdf", "s1", &scan(&db, &[m(1, "1000.00")])).unwrap();
    let s2 = scan(&db, &[m(2, "900.00")]);
    let plan = db.plan_import("s2", &s2).unwrap();
    db.import_scan(personal, "2.pdf", "s2", &s2, &plan.default_targets()).unwrap();
    let v: Vec<String> = db.conn.prepare("SELECT amount_in_entity_currency FROM transactions ORDER BY posted_date").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(v.iter().map(|s| dec(s)).collect::<Vec<_>>(), vec![dec("-120.00"), dec("-120.00")]);
}

#[test]
fn a_deleted_workspaces_things_never_turn_up_in_a_new_one() {
    let db = Db::open_in_memory().unwrap();
    db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let x = db.create_entity("Test", EntityKind::Business, Currency::KYD).unwrap();
    let item = fingraph_core::wealth::ManualItem { id: 0, entity_id: x, name: "House".into(), liability: false, currency: Currency::KYD, apr: None, values: vec![] };
    db.save_manual_item(&item).unwrap();
    db.delete_entity(x).unwrap();
    let y = db.create_entity("Rental", EntityKind::RentalProperty, Currency::KYD).unwrap();
    assert_ne!(x, y);
    assert!(db.manual_items(&[y]).unwrap().is_empty());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM manual_items"), 0);
}

#[test]
fn a_manual_line_after_a_delete_gets_its_own_fingerprint() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let t = all(&db)[0].clone();
    db.add_manual(t.account_id, None, t.posted, "cash", dec("-5.00"), "").unwrap();
    db.delete_transactions(&[t.id]).unwrap();
    db.add_manual(t.account_id, None, t.posted, "cash again", dec("-5.00"), "").unwrap();
}

#[test]
fn erase_keeps_what_the_bank_profiles_learned() {
    let dir = scratch("erase");
    let path = dir.join("ledger.db");
    {
        let db = Db::open(&path).unwrap();
        let mut p = db.profiles().unwrap().into_iter().find(|p| p.slug == "scotiabank_ky").unwrap();
        p.layout.verified.push("Loan".into());
        db.save_profile(&p).unwrap();
        db.erase_all().unwrap();
    }
    let db = Db::open(&path).unwrap();
    let p = db.profiles().unwrap().into_iter().find(|p| p.slug == "scotiabank_ky").unwrap();
    assert!(p.layout.verified.contains(&"Loan".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replace_keeps_your_category() {
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let mk = || spec("2045", Currency::KYD, "1000.00", (d(1, 16), d(2, 15)), vec![DemoTx::new(d(1, 20), "POS PURCHASE", dec("-62.10")).memo("KIRK")]);
    let r = db.import_auto(ent, "a.pdf", "sha-a", &scan(&db, &[mk()])).unwrap();
    db.approve_document(r.document_ids[0]).unwrap();
    let t = all(&db)[0].clone();
    let cat = db.category_id("Groceries").unwrap();
    db.set_category(&[t.id], cat).unwrap();
    let s = scan(&db, &[mk()]);
    let plan = db.plan_import("sha-b", &s).unwrap();
    let old: Vec<i64> = plan.same_period.iter().map(|d| d.0).collect();
    assert_eq!(old, r.document_ids);
    db.replace_with(&old, ent, "b.pdf", "sha-b", &s, &[Target::Existing(t.account_id)]).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM documents WHERE sha256_hash = 'sha-a'"), 0);
    let after: Option<i64> = db.conn.query_row("SELECT category_id FROM transactions WHERE fitid = ?1", [&t.fitid], |r| r.get(0)).unwrap();
    assert_eq!(after, cat);
}

#[test]
fn a_rescan_that_finds_a_new_section_keeps_each_account_its_own_rows() {
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let a = || spec("1111", Currency::KYD, "100.00", (d(1, 1), d(1, 31)), vec![DemoTx::new(d(1, 10), "POS PURCHASE", dec("-7.00")).memo("AAA")]);
    let b = || spec("2222", Currency::KYD, "500.00", (d(1, 1), d(1, 31)), vec![DemoTx::new(d(1, 12), "POS PURCHASE", dec("-9.00")).memo("BBB")]);
    db.import_auto(ent, "combo.pdf", "combo", &scan(&db, &[b()])).unwrap();
    db.import_auto(ent, "z.pdf", "z", &scan(&db, &[spec("1111", Currency::KYD, "50.00", (d(2, 1), d(2, 28)), vec![DemoTx::new(d(2, 10), "POS PURCHASE", dec("-1.00")).memo("Z")])])).unwrap();
    let s2 = scan(&db, &[a(), b()]);
    assert_eq!(s2.sections.len(), 2);
    db.replace_statement(ent, "combo.pdf", "combo", &s2).unwrap();
    let rows: Vec<(String, String)> = db
        .conn
        .prepare("SELECT a.account_number_masked, t.amount FROM transactions t JOIN accounts a ON a.id = t.account_id WHERE t.posted_date LIKE '2026-01-%' ORDER BY t.amount")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.iter().map(|(a, m)| (a.as_str(), dec(m))).collect::<Vec<_>>(), vec![("••1111", dec("-7.00")), ("••2222", dec("-9.00"))]);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM accounts"), 2, "no extra account made");
}

#[test]
fn reset_learning_puts_a_business_row_back_in_its_own_currency() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let usd = db.create_entity("US LLC", EntityKind::Business, Currency::USD).unwrap();
    let t = all(&db).into_iter().find(|t| t.currency == Currency::KYD && t.entity_currency == Currency::KYD && t.amount < Decimal::ZERO && t.transfer_peer.is_none()).unwrap();
    db.set_business(&[t.id], Some(usd)).unwrap();
    db.reset_learning(&[]).unwrap();
    let r = db.transaction(t.id).unwrap();
    assert_eq!((r.business_entity_id, r.amount_entity), (None, t.amount));
}

#[test]
fn statement_copies_only_ever_move_or_go_on_their_own() {
    let base = scratch("copies");
    let db = Db::open(&base.join("ledger.db")).unwrap();
    // a scratch ledger keeps its copies beside itself, never in the real data folder
    assert_eq!(db.statements_dir(), base.join("statements"));
    let hex = "a".repeat(64);
    std::fs::create_dir_all(db.statements_dir()).unwrap();
    std::fs::write(db.statements_dir().join(format!("{hex}.pdf")), b"%PDF").unwrap();
    std::fs::create_dir_all(base.join("Documents/Taxes")).unwrap();
    std::fs::write(base.join("Documents/notes.txt"), b"mine").unwrap();
    // a folder with your things in it: the copies get their own folder inside it
    let (n, to) = db.move_statements_dir(&base.join("Documents")).unwrap();
    assert_eq!((n, to.clone()), (1, base.join("Documents/FinGraph Statements")));
    assert_eq!(mode(&to), 0o700);
    let (n, _) = db.move_statements_dir(&base.join("Elsewhere")).unwrap();
    assert_eq!(n, 1);
    assert!(base.join("Documents/notes.txt").exists() && base.join("Documents/Taxes").is_dir());
    std::fs::write(base.join("Elsewhere/mine.txt"), b"mine").unwrap();
    assert_eq!(db.delete_kept_pdfs().unwrap(), 1);
    assert!(base.join("Elsewhere/mine.txt").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ledger_backups_and_copies_are_private() {
    let base = scratch("perms");
    let db = Db::open(&base.join("ledger.db")).unwrap();
    db.load_demo().unwrap();
    assert_eq!(mode(&base.join("ledger.db")), 0o600);
    let dest = base.join("backups/one.db");
    db.backup_to(&dest).unwrap();
    db.backup_to(&dest).unwrap();
    assert_eq!(mode(&dest), 0o600);
    let src = base.join("in.pdf");
    std::fs::write(&src, b"%PDF").unwrap();
    db.keep_pdf(&src, &"b".repeat(64)).unwrap();
    assert_eq!(mode(&db.statements_dir().join(format!("{}.pdf", "b".repeat(64)))), 0o600);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn receipts_that_would_share_a_name_are_both_exported() {
    let base = scratch("rcpt");
    let db = Db::open(&base.join("ledger.db")).unwrap();
    db.load_demo().unwrap();
    let rows = all(&db);
    std::fs::write(base.join("r1.pdf"), b"first receipt").unwrap();
    std::fs::write(base.join("r2.pdf"), b"second receipt").unwrap();
    db.attach(rows[0].id, &base.join("r1.pdf")).unwrap();
    db.attach(rows[1].id, &base.join("r2.pdf")).unwrap();
    let (mut a, mut b) = (db.transaction(rows[0].id).unwrap(), db.transaction(rows[1].id).unwrap());
    b.posted = a.posted;
    b.amount = a.amount;
    a.clean_payee = "Coffee".into();
    b.clean_payee = "Coffee".into();
    assert_eq!(db.export_receipts(&[a, b], &base.join("out")).unwrap(), 2);
    assert_eq!(std::fs::read_dir(base.join("out")).unwrap().count(), 2);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn rates_that_cannot_be_right_are_not_kept() {
    let db = Db::open_in_memory().unwrap();
    let gbp = Currency::parse("GBP").unwrap();
    let day = NaiveDate::from_ymd_opt(2025, 3, 10).unwrap();
    assert_eq!(db.store_rates(&[(day, gbp, dec("0.77"))], "test").unwrap(), 1);
    let bad = [
        (day.succ_opt().unwrap(), gbp, Decimal::from_str("10000000000000000000000000000").unwrap()),
        (day.succ_opt().unwrap(), gbp, dec("0.000000000001")),
        (day.succ_opt().unwrap(), gbp, dec("7.7")),
        (NaiveDate::from_ymd_opt(2099, 1, 1).unwrap(), gbp, dec("0.77")),
        (day, Currency::KYD, dec("0.9")),
    ];
    assert_eq!(db.store_rates(&bad, "hostile").unwrap(), 0);
    assert_eq!(db.store_rates(&[(day.succ_opt().unwrap(), gbp, dec("0.78"))], "test").unwrap(), 1);
}

/// A friend's RBC download (three accounts in one CSV) was refused, and its PDF version merged every account into one.
#[test]
fn an_rbc_csv_download_imports_as_separate_held_accounts() {
    let dir = tempfile_dir("rbc-csv");
    let p = dir.join("rbc.csv");
    std::fs::write(
        &p,
        "\"Account Type\",\"Account Number\",\"Transaction Date\",\"Cheque Number\",\"Description 1\",\"Description 2\",\"CAD$\",\"USD$\"\n\
         Savings,01234-5550001,10/2/2026,,PAYROLL,,,2480.15\n\
         Chequing,01234-5550002,10/3/2026,,GROCERY,,-82.10,\n\
         Visa,4510********7788,10/5/2026,,FUEL,,-45.00,\n",
    )
    .unwrap();
    let db = Db::open_in_memory().unwrap();
    let e = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let s = MultiPassScanner::new(&db.profiles().unwrap()).scan_file(&p).unwrap();
    db.import_auto(e, "rbc.csv", &fingraph_core::db::file_sha256(&p).unwrap(), &s).unwrap();
    let mut accts: Vec<(String, AccountType)> = db.accounts().unwrap().into_iter().map(|a| (a.account_number_masked, a.account_type)).collect();
    accts.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(accts.len(), 3, "{accts:?}");
    assert!(accts.iter().any(|a| a.1 == AccountType::CreditCard));
    // no balances in the download: nothing counts until someone approves it
    assert_eq!(count(&db, "SELECT COUNT(*) FROM documents WHERE review_hold = 1"), 3);
}

fn tempfile_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fingraph-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const RBC_HEADER: &str = "\"Account Type\",\"Account Number\",\"Transaction Date\",\"Cheque Number\",\"Description 1\",\"Description 2\",\"CAD$\",\"USD$\"\n";

fn import_download(db: &Db, e: i64, dir: &std::path::Path, name: &str, body: &str) -> fingraph_core::db::ImportReport {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    let sha = fingraph_core::db::file_sha256(&p).unwrap();
    let s = MultiPassScanner::new(&db.profiles().unwrap()).scan_file(&p).unwrap();
    let r = db.import_auto(e, name, &sha, &s).unwrap();
    db.keep_pdf(&p, &sha).unwrap();
    r
}

/// Downloads overlap (first half of the month, then all of it); a download never has balances, and approving
/// it must survive Rescan All; the next month's OFX is proven by the last statement's closing balance.
#[test]
fn downloads_dedupe_stay_approved_and_chain() {
    let dir = tempfile_dir("downloads");
    let db = Db::open(&dir.join("ledger.db")).unwrap();
    let e = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
    let row = |d: &str, what: &str, amt: &str| format!("Savings,01234-5550001,{d},,{what},,,{amt}\n");
    let half = format!("{RBC_HEADER}{}{}{}", row("10/2/2026", "PAYROLL", "2480.15"), row("10/14/2026", "COFFEE", "-4.50"), row("10/14/2026", "COFFEE", "-4.50"));
    let whole = format!("{half}{}", row("10/28/2026", "RENT", "-1200.00"));
    for (name, body) in [("oct1-15.csv", &half), ("oct1-31.csv", &whole)] {
        for id in import_download(&db, e, &dir, name, body).document_ids {
            db.approve_document(id).unwrap();
        }
    }
    // two coffees the same day are two coffees; the overlap isn't counted twice
    assert_eq!(all(&db).len(), 4);
    db.rescan_all().unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM documents WHERE review_hold = 1"), 0);
    assert_eq!(all(&db).len(), 4);

    let sep = "Date,Description,Amount,Balance,Account Number,Currency,Account Type\n2026-09-01,Pay,1000.00,1500.00,99887766,USD,Chequing\n2026-09-30,Shop,-100.00,1400.00,99887766,USD,Chequing\n";
    for id in import_download(&db, e, &dir, "sep.csv", sep).document_ids {
        db.approve_document(id).unwrap();
    }
    let ofx = |close: &str| format!("OFXHEADER:100\n<OFX><STMTRS><CURDEF>USD<BANKACCTFROM><ACCTID>99887766<ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20261001<DTEND>20261031<STMTTRN><DTPOSTED>20261005<TRNAMT>-50.00<NAME>FUEL</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>{close}</LEDGERBAL></STMTRS></OFX>");
    let status = |r: &fingraph_core::db::ImportReport| -> String { db.conn.query_row("SELECT reconciliation_status FROM documents WHERE id = ?1", [r.document_ids[0]], |x| x.get(0)).unwrap() };
    assert_eq!(status(&import_download(&db, e, &dir, "oct.ofx", &ofx("1350.00"))), "Reconciled");
    // a row missing from the download: the closing balance doesn't follow
    assert_ne!(status(&import_download(&db, e, &dir, "oct-short.ofx", &ofx("1300.00"))), "Reconciled");
    std::fs::remove_dir_all(&dir).ok();
}

/// Undo puts back a bulk edit and the rule it taught, including what the rule changed elsewhere.
#[test]
fn undo_restores_edits_and_rules() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let before = all(&db);
    let rules_before = db.rules().unwrap().len();
    assert!(!db.undo_to_checkpoint().unwrap(), "nothing to undo yet");

    db.checkpoint().unwrap();
    let ids: Vec<i64> = before.iter().take(5).map(|t| t.id).collect();
    db.set_category(&ids, None).unwrap();
    db.set_subscription(&ids, true).unwrap();
    db.set_tag(&ids, "oops").unwrap();
    let t = &before[10];
    let rule = fingraph_core::rules::Rule {
        kind: fingraph_core::rules::RuleKind::NormalizedTokens,
        pattern: fingraph_core::rules::merchant_stem(&t.raw_description, &t.memo),
        clean_payee: Some("Wrong Name".into()),
        ..Default::default()
    };
    db.learn(&rule).unwrap();
    assert_ne!(all(&db), before);

    assert!(db.undo_to_checkpoint().unwrap());
    assert_eq!(all(&db), before, "every transaction back as it was");
    assert_eq!(db.rules().unwrap().len(), rules_before, "the learned rule is gone");
    assert!(!db.undo_to_checkpoint().unwrap(), "one step only");
}

/// The picker's shortcuts: busiest categories by direction (no transfers), and recent picks newest first.
#[test]
fn picker_shortcuts() {
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let out = db.top_categories(&[], true, 6).unwrap();
    let inc = db.top_categories(&[], false, 6).unwrap();
    assert!(!out.is_empty() && out.len() <= 6);
    let cats = db.categories().unwrap();
    let kind = |id: &i64| cats.iter().find(|c| c.id == *id).map(|c| c.kind);
    assert!(out.iter().chain(&inc).all(|id| kind(id) != Some(fingraph_core::db::CategoryKind::Transfer)), "no transfers");
    assert!(cats.iter().any(|c| c.name == "Taxes"), "Taxes is seeded");
    for id in [5, 7, 5, 9] {
        db.note_recent_category(id).unwrap();
    }
    assert_eq!(db.recent_categories(), vec![9, 5, 7]);
}

/// Merchant hints carry shop names and categories only: never people, income, transfers or numbers.
#[test]
fn merchant_hints_share_only_shops() {
    use fingraph_core::hints::{merchant_hints, to_csv};
    let db = Db::open_in_memory().unwrap();
    db.load_demo().unwrap();
    let rows = all(&db);
    let cats = db.categories().unwrap();
    let cat = |n: &str| cats.iter().find(|c| c.name == n).unwrap().id;
    let ids_like = |s: &str| rows.iter().filter(|t| t.raw_description.contains(s)).map(|t| t.id).collect::<Vec<_>>();
    // a shop the directory would call Landscaping, filed by you as Repairs: worth teaching
    db.set_category(&ids_like("GENERAL LANDSCAPING"), Some(cat("Repairs & Maintenance"))).unwrap();
    // a cheque (to a person), and money coming in: never shared, whatever their category
    db.set_category(&ids_like("CHEQUE"), Some(cat("Housing"))).unwrap();
    db.set_category(&ids_like("RENT RECEIVED"), Some(cat("Rental Income"))).unwrap();
    let hints = merchant_hints(&all(&db), &db.categories().unwrap());
    let land = hints.iter().find(|h| h.merchant == "GENERAL LANDSCAPING").expect("the landscaper is a hint");
    assert_eq!((land.category.as_str(), land.parent.as_deref()), ("Repairs & Maintenance", None));
    assert!(land.times >= 1);
    for h in &hints {
        assert!(!h.merchant.contains("CHEQUE") && !h.merchant.contains("RENT RECEIVED"), "{h:?}");
        assert!(!h.merchant.chars().any(|c| c.is_ascii_digit()), "no reference or account numbers: {h:?}");
    }
    let csv = to_csv(&hints, "0.2.17");
    assert!(csv.starts_with("# FinGraph 0.2.17 merchant hints"));
    assert!(csv.contains("GENERAL LANDSCAPING,Repairs & Maintenance,,"));
    for t in all(&db) {
        assert!(!csv.contains(&format!("{:.2}", t.amount.abs())), "no amounts: {}", t.amount);
    }
}
