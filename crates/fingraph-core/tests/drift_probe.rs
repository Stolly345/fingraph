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

/// Same dates and amounts, row for row, and proven: for changes to the page's geometry. Descriptions are compared
/// and reported (a wrapped word may land on its other line), not required.
fn same_money(what: &str, r: &ScanResult, base: &ScanResult) -> bool {
    let money = |r: &ScanResult| {
        let mut v: Vec<(String, String)> = r.sections.iter().flat_map(|s| &s.transactions).map(|t| (t.posted.to_string(), t.amount.to_string())).collect();
        v.sort();
        v
    };
    let descs = |r: &ScanResult| r.sections.iter().flat_map(|s| &s.transactions).map(|t| t.description.clone()).collect::<Vec<_>>();
    let (a, b) = (descs(r), descs(base));
    let alike = b.iter().filter(|d| a.contains(d)).count();
    if alike * 10 < b.len() * 9 {
        // reported, not failed: distorted geometry can cost a description its wrapped line or prefix it with a
        // reference number, never an amount
        println!("     ({what}: descriptions {alike}/{} the same)", b.len());
        if std::env::var("DRIFT_VERBOSE").is_ok() {
            for (x, y) in b.iter().zip(&a).filter(|(x, y)| x != y).take(3) {
                println!("       want {x:?}\n        got {y:?}");
            }
        }
    }
    r.is_reconciled() && money(r) == money(base)
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
        // the page printed another way: stretched unevenly (A4 vs Letter), even pages drifting in a scanner, word
        // boxes 40% taller (overlapping the next line's, centres kept)
        let geometric: Vec<(&str, Vec<Token>)> = vec![
            ("stretched", t.iter().map(|x| Token { x0: x.x0 * 1.35, x1: x.x1 * 1.35, y0: x.y0 * 0.85, y1: x.y1 * 0.85, ..x.clone() }).collect()),
            ("even pages shifted", t.iter().map(|x| if x.page % 2 == 0 { Token { x0: x.x0 + 18.0, x1: x.x1 + 18.0, y0: x.y0 - 12.0, y1: x.y1 - 12.0, ..x.clone() } } else { x.clone() }).collect()),
            ("taller boxes", t.iter().map(|x| { let g = 0.2 * (x.y1 - x.y0); Token { y0: x.y0 - g, y1: x.y1 + g, ..x.clone() } }).collect()),
            // line spacing growing down the page (a scan stretched unevenly): every gap a bit wider than the last
            ("uneven vertical stretch", t.iter().map(|x| { let f = |y: f64| y + 0.0004 * y * y; Token { y0: f(x.y0), y1: f(x.y0) + (x.y1 - x.y0), ..x.clone() } }).collect()),
        ];
        for (what, toks) in geometric {
            let r = MultiPassScanner::new(&all).scan_tokens(toks).unwrap();
            let ok = same_money(what, &r, &base);
            if !ok && std::env::var("DRIFT_VERBOSE").is_ok() {
                let (a, b) = (rows(&base, &same), rows(&r, &same));
                println!("     {what}: {} rows vs {}, {:?} read_as {:?} notes {:?}", b.len(), a.len(), r.sections.iter().map(|s| &s.reconciliation).collect::<Vec<_>>(), r.diagnostics.pass3.read_as, r.diagnostics.pass3.layout_notes);
                for (x, y) in a.iter().filter(|x| !b.contains(x)).zip(b.iter().filter(|y| !a.contains(y))).take(3) {
                    println!("       want {x:?}\n        got {y:?}");
                }
            }
            check(what.into(), ok);
        }
        // the table headings replaced by gibberish (same boxes): the columns have to come from the amounts
        let header_rows: Vec<(u32, f64, f64)> = base
            .diagnostics
            .pages
            .iter()
            .filter_map(|pg| {
                let first_tx = base.sections.iter().flat_map(|s| &s.transactions).filter(|tx| tx.page == pg.page).map(|tx| tx.y).fold(f64::MAX, f64::min);
                Some((pg.page, pg.header_y?, first_tx))
            })
            .collect();
        let mut k = 0;
        let gibberish: Vec<Token> = t
            .iter()
            .map(|x| {
                let in_header = header_rows.iter().any(|&(p, hy, ty)| x.page == p && x.y0 >= hy - 0.5 && x.yc() < ty - 4.0 && x.yc() < hy + 26.0);
                if in_header && !fingraph_core::money::is_amount_core(&x.text) {
                    k += 1;
                    Token { text: format!("Xyz{k}"), ..x.clone() }
                } else {
                    x.clone()
                }
            })
            .collect();
        let r = MultiPassScanner::new(&all).scan_tokens(gibberish).unwrap();
        if std::env::var("DRIFT_VERBOSE").is_ok() && !same_money("gibberish", &r, &base) {
            let (a, b) = (rows(&base, &same), rows(&r, &same));
            println!("     gibberish: {} rows vs {}, {:?} read_as {:?} notes {:?}", b.len(), a.len(), r.sections.iter().map(|s| &s.reconciliation).collect::<Vec<_>>(), r.diagnostics.pass3.read_as, r.diagnostics.pass3.layout_notes);
            for (x, y) in a.iter().filter(|x| !b.contains(x)).zip(b.iter().filter(|y| !a.contains(y))).take(3) {
                println!("       want {x:?}\n        got {y:?}");
            }
        }
        check(format!("gibberish headings ({k} words)"), same_money("gibberish", &r, &base));
        // an OCR-style slip in one amount (`1,234.56` → `1,234,56`): repaired and held, or held — never silently wrong
        let victim = base.sections.iter().flat_map(|s| &s.transactions).nth(2).map(|tx| (tx.page, tx.y, fingraph_core::money::fmt_plain(tx.stmt_amount.abs())));
        if let Some((pg, y, _)) = victim {
            let mut done = false;
            let slipped: Vec<Token> = t
                .iter()
                .map(|x| {
                    if !done && x.page == pg && (x.yc() - y).abs() < 3.0 && fingraph_core::money::is_amount_core(&x.text) && x.text.contains('.') {
                        done = true;
                        return Token { text: x.text.replacen('.', ",", 1), ..x.clone() };
                    }
                    x.clone()
                })
                .collect();
            let r = MultiPassScanner::new(&all).scan_tokens(slipped).unwrap();
            let silent = r.is_reconciled() && r.diagnostics.pass3.layout_notes.is_empty() && r.sections.iter().all(|s| s.report.derived.is_empty());
            // (the recovered line keeps the garbled figure in its description: the money is what has to match)
            let repaired = same_money("OCR slip", &r, &base);
            check(format!("OCR slip ({})", if repaired { "repaired, held" } else { "held" }), done && !silent && (repaired || !r.is_reconciled()));
        }

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
        .map(|p| (MultiPassScanner::new(&profiles).scan_file(&p).unwrap(), p))
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



