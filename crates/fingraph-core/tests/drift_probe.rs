//! Your real statements read under simulated bank layout changes: renamed headings and balance labels,
//! another date format, no currency symbols, `DR` for minus, a moved page, a bank nobody knows.
//! `cargo test -p fingraph-core --test drift_probe -- --ignored --nocapture`
//!
//! Each changed statement must read the same dates, amounts and descriptions as the unchanged one; a read
//! that isn't the saved layout's must say so (so it's held); and once its layout is learned (what approving
//! it does), the changed statement reads without a note, while the unchanged one still reads.
use fingraph_core::parser::{profiles, tokens_from_pdf, MultiPassScanner, ScanResult, Token};
use regex::Regex;

type Mutate = Box<dyn Fn(&str) -> Option<String>>;

fn map(t: &[Token], f: &Mutate) -> Vec<Token> {
    t.iter()
        .filter_map(|x| {
            let mut y = x.clone();
            y.text = f(&x.text)?;
            Some(y)
        })
        .collect()
}

/// (date, amount, description) of every row, with the description changed the way the words were.
fn rows(r: &ScanResult, f: &Mutate) -> Vec<(String, String, String)> {
    let mut v: Vec<_> = r
        .sections
        .iter()
        .flat_map(|s| &s.transactions)
        .map(|t| (t.posted.to_string(), t.amount.to_string(), t.description.split_whitespace().filter_map(f).collect::<Vec<_>>().join(" ")))
        .collect();
    v.sort();
    v
}

#[test]
#[ignore]
fn drift_probe() {
    let dir = fingraph_core::data_dir().join("documents");
    let all = profiles::builtins();
    let mon = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
    let dre = Regex::new(r"^(\d{2})(JAN|FEB|MAR|APR|MAY|JUN|JUL|AUG|SEP|OCT|NOV|DEC)(\d{2})?$").unwrap();
    let same: Mutate = Box::new(|s| Some(s.to_string()));
    let cases: Vec<(&str, Mutate)> = vec![
        ("rebrand", Box::new(|s| Some(if s.to_lowercase().contains("scotia") || s == "Nova" { "Acme".into() } else { s.to_string() }))),
        (
            "headings",
            Box::new(|s| {
                Some(match s {
                    "Description" | "DESCRIPTION" => "Narrative".into(),
                    "DEBIT" | "Debit" => "Out".into(),
                    "CREDIT" | "Credit" => "In".into(),
                    "Balance" | "BALANCE" => "Running".into(),
                    _ => s.to_string(),
                })
            }),
        ),
        (
            "labels",
            Box::new(|s| {
                Some(match s {
                    "OPENING" | "Opening" | "PREVIOUS" | "Previous" => "BROUGHT".into(),
                    "CLOSING" | "Closing" | "NEW" | "New" => "CARRIED".into(),
                    _ => s.to_string(),
                })
            }),
        ),
        (
            "dates",
            Box::new(move |s| {
                Some(match dre.captures(s) {
                    Some(c) => format!("{}/{:02}/20{}", &c[1], mon.iter().position(|x| *x == &c[2]).unwrap() + 1, c.get(3).map_or("26", |y| y.as_str())),
                    None => s.to_string(),
                })
            }),
        ),
        ("no-ccy", Box::new(|s| if s == "CI$" || s == "US$" { None } else { Some(s.to_string()) })),
        ("dr-sign", Box::new(|s| Some(if s == "-" { "DR".into() } else { s.to_string() }))),
    ];
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("statement_")).collect();
    files.sort();
    let mut failed = Vec::new();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let t = tokens_from_pdf(f).unwrap();
        let base = MultiPassScanner::new(&all).scan_tokens(t.clone()).unwrap();
        assert!(base.is_reconciled(), "{name}");
        let mut check = |what: String, ok: bool| {
            println!("{} {name}: {what}", if ok { "ok  " } else { "FAIL" });
            if !ok {
                failed.push(format!("{name}: {what}"));
            }
        };
        // a bank nobody set up, and a page moved and shrunk
        let unknown = MultiPassScanner::new(&[]).scan_tokens(t.clone()).unwrap();
        check("unknown bank".into(), unknown.is_reconciled() && rows(&unknown, &same) == rows(&base, &same));
        let moved: Vec<Token> = t.iter().map(|x| Token { x0: x.x0 * 0.9 + 30.0, x1: x.x1 * 0.9 + 30.0, ..x.clone() }).collect();
        let r = MultiPassScanner::new(&all).scan_tokens(moved).unwrap();
        check("page moved".into(), r.is_reconciled() && rows(&r, &same) == rows(&base, &same));

        for (case, m) in &cases {
            let changed = map(&t, m);
            let r = MultiPassScanner::new(&all).scan_tokens(changed.clone()).unwrap();
            let want = rows(&base, m);
            let got = rows(&r, &same);
            let saved = r.diagnostics.pass3.read_as.is_none() && r.diagnostics.pass3.layout_notes.is_empty();
            for (a, b) in want.iter().filter(|w| !got.contains(w)).zip(got.iter().filter(|g| !want.contains(g))).take(3) {
                println!("     {case} expected {a:?}\n     {case}      got {b:?}");
            }
            check(format!("{case}: reads the same{}", if saved { "" } else { ", held" }), r.is_reconciled() && got == want && (saved || !r.diagnostics.pass3.layout_notes.is_empty() || r.profile_slug.is_none()));
            if saved || r.profile_slug.is_none() {
                continue;
            }
            // approve it: the bank's profile learns the new layout
            let mut learned = all.clone();
            let p = learned.iter_mut().find(|p| Some(&p.slug) == r.profile_slug.as_ref()).unwrap();
            p.learn(&r.diagnostics.observed);
            let again = MultiPassScanner::new(&learned).scan_tokens(changed).unwrap();
            check(
                format!("{case}: learned ({:?})", again.diagnostics.pass3.layout_notes),
                again.is_reconciled() && again.diagnostics.pass3.read_as.is_none() && again.diagnostics.pass3.layout_notes.is_empty() && rows(&again, &same) == want,
            );
            let old = MultiPassScanner::new(&learned).scan_tokens(t.clone()).unwrap();
            check(format!("{case}: old layout still reads"), old.is_reconciled() && old.diagnostics.pass3.layout_notes.is_empty() && rows(&old, &same) == rows(&base, &same));
        }
    }
    assert!(!files.is_empty(), "no statements in the data folder");
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Your statements, imported fresh in date order: none may be held because its layout "changed" (they
/// are all the same layout), and each must import without a hold.
#[test]
#[ignore]
fn real_statements_import_without_layout_holds() {
    use fingraph_core::db::Db;
    let db = Db::open_in_memory().unwrap();
    let ent = db.create_entity("Personal", fingraph_core::EntityKind::Personal, fingraph_core::Currency::KYD).unwrap();
    let dir = fingraph_core::data_dir().join("documents");
    let profiles = db.profiles().unwrap();
    let mut scans: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("statement_"))
        .map(|p| (MultiPassScanner::new(&profiles).scan_pdf(&p).unwrap(), p))
        .collect();
    scans.sort_by_key(|s| s.0.period);
    for (i, (scan, p)) in scans.iter().enumerate() {
        let rep = db.import_auto(ent, &p.file_name().unwrap().to_string_lossy(), &format!("real{i}"), scan).unwrap();
        for id in &rep.document_ids {
            let d = db.documents_all(&[]).unwrap().into_iter().find(|d| d.id == *id).unwrap();
            println!("{} {:?}", d.filename, d.review);
            assert!(d.review.is_empty(), "{} held: {:?} {:?}", d.filename, d.review, db.document_report(*id).unwrap().0.pass3.layout_notes);
        }
    }
}

/// Rescan All on a copy of a ledger (`FINGRAPH_LEDGER_COPY=…/ledger.db`): nothing approved is held again.
#[test]
#[ignore]
fn rescan_a_ledger_copy() {
    let Ok(path) = std::env::var("FINGRAPH_LEDGER_COPY") else { return };
    let db = fingraph_core::db::Db::open(std::path::Path::new(&path)).unwrap();
    let held = |db: &fingraph_core::db::Db| db.documents_all(&[]).unwrap().into_iter().filter(|d| !d.review.is_empty()).map(|d| (d.filename, d.review)).collect::<Vec<_>>();
    let before = held(&db);
    let (n, txs, missing) = db.rescan_all().unwrap();
    println!("rescanned {n} files, {txs} rows, missing {missing:?}; held before {before:?}");
    assert_eq!(held(&db), before);
}

