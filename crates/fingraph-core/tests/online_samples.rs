//! Third-party sample statements (synthetic, downloaded from public test sets) read against their published
//! answers. Never touches your ledger: the scanner runs on its own, imports go into an in-memory one.
//! `FINGRAPH_SAMPLES=<dir> cargo test -p fingraph-core --test online_samples -- --ignored --nocapture`
//!
//! `<dir>/x.pdf` is checked against `<dir>/x.json` when present: a list of `{date, deposit, withdrawal}`
//! (DD-MM-YYYY), or `{transactions: [{date, amount, direction}]}` (YYYY-MM-DD), or `{transactionCount}`.
use fingraph_core::parser::{profiles, MultiPassScanner, ScanResult};
use rust_decimal::Decimal;
use std::str::FromStr;

/// The published answer: every (date, signed amount), or only how many rows there are.
enum Truth {
    Rows(Vec<(String, Decimal)>),
    Count(usize),
}

fn truth(json: &serde_json::Value) -> Option<Truth> {
    let dec = |v: &serde_json::Value| -> Option<Decimal> {
        let s = v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|f| format!("{f:.2}")))?;
        Decimal::from_str(&s.replace(',', "")).ok().map(|d| d.round_dp(2))
    };
    if let Some(list) = json.as_array() {
        // DD-MM-YYYY, deposit / withdrawal
        return Some(Truth::Rows(
            list.iter()
                .filter_map(|t| {
                    let d = t["date"].as_str()?;
                    let iso = format!("{}-{}-{}", &d[6..10], &d[3..5], &d[0..2]);
                    let amt = dec(&t["deposit"]).filter(|x| !x.is_zero()).or_else(|| dec(&t["withdrawal"]).map(|w| -w))?;
                    Some((iso, amt))
                })
                .collect(),
        ));
    }
    if let Some(list) = json["transactions"].as_array() {
        return Some(Truth::Rows(
            list.iter()
                .filter_map(|t| {
                    let a = dec(&t["amount"])?;
                    Some((t["date"].as_str()?.to_string(), if t["direction"] == "debit" { -a } else { a }))
                })
                .collect(),
        ));
    }
    json["transactionCount"].as_u64().map(|n| Truth::Count(n as usize))
}

fn rows(r: &ScanResult) -> Vec<(String, Decimal)> {
    r.sections.iter().flat_map(|s| &s.transactions).map(|t| (t.posted.to_string(), t.amount.round_dp(2))).collect()
}

#[test]
#[ignore]
fn online_samples() {
    let Ok(dir) = std::env::var("FINGRAPH_SAMPLES") else { return };
    let mut pdfs: Vec<_> = walk(std::path::Path::new(&dir)).into_iter().filter(|p| p.extension().is_some_and(|e| e == "pdf")).collect();
    pdfs.sort();
    let profiles = profiles::builtins();
    println!("{:<44} {:>6} {:>9} {:>5}  read", "statement", "rows", "answer", "match");
    for pdf in &pdfs {
        let name = pdf.file_name().unwrap().to_string_lossy().into_owned();
        let r = match MultiPassScanner::new(&profiles).scan_pdf(pdf) {
            Ok(r) => r,
            Err(e) => {
                println!("{name:<44} ERROR {e}");
                continue;
            }
        };
        let got = rows(&r);
        let answer = [pdf.with_extension("json"), pdf.with_file_name(name.replace("-statement.pdf", "-statement-info.json"))]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .and_then(|j| serde_json::from_str(&j).ok())
            .and_then(|j| truth(&j));
        let (want, matched) = match &answer {
            Some(Truth::Rows(w)) => {
                let mut left = w.clone();
                let hits = got.iter().filter(|g| left.iter().position(|x| x == *g).map(|i| left.remove(i)).is_some()).count();
                (w.len().to_string(), format!("{hits}/{}", w.len()))
            }
            Some(Truth::Count(n)) => (format!("{n} rows"), if got.len() == *n { "count".into() } else { "-".into() }),
            None => ("?".into(), "-".into()),
        };
        let recon: Vec<String> = r.sections.iter().map(|s| format!("{:?}", s.reconciliation)).collect();
        println!(
            "{name:<44} {:>6} {:>9} {:>5}  {} | {} sections {:?} | read_as {:?} | skipped {}",
            got.len(),
            want,
            matched,
            r.profile_slug.as_deref().map_or_else(|| format!("unknown bank {:?}", r.diagnostics.pass1.identifiers), str::to_string),
            r.sections.len(),
            recon,
            r.diagnostics.pass3.read_as,
            r.sections.iter().map(|s| s.report.skipped.len()).sum::<usize>()
        );
        // `FINGRAPH_SAMPLES_DUMP=<dir>`: each read as JSON, for comparing with answer keys elsewhere
        if let Ok(out) = std::env::var("FINGRAPH_SAMPLES_DUMP") {
            let read = serde_json::json!({
                "bank": r.profile_slug, "identifiers": r.diagnostics.pass1.identifiers, "notes": r.diagnostics.pass3.layout_notes,
                "sections": r.sections.iter().map(|s| serde_json::json!({
                    "reconciliation": format!("{:?}", s.reconciliation), "opening": s.opening, "closing": s.closing, "currency": s.currency,
                    "transactions": s.transactions.iter().map(|t| serde_json::json!({"date": t.posted.to_string(), "amount": t.amount, "balance": t.balance, "description": t.description})).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            });
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(std::path::Path::new(&out).join(name.replace(".pdf", ".read.json")), serde_json::to_string_pretty(&read).unwrap()).unwrap();
        }
        if std::env::var("FINGRAPH_SAMPLES_VERBOSE").is_ok() {
            for s in &r.sections {
                println!("    section {:?} {:?} open {:?} close {:?}", s.currency, s.account_type, s.opening, s.closing);
                for t in &s.transactions {
                    println!("      {} {:>12} bal {:?} {}", t.posted, t.amount, t.balance, t.description);
                }
                for l in &s.report.skipped {
                    println!("      skipped ({}) {}", l.reason, l.text);
                }
            }
        }
        // the same statement after a redesign that renames its column headings: the same rows, or it says why not
        if r.is_reconciled() {
            let rename = |w: &str| -> Option<&'static str> {
                Some(match w.to_lowercase().trim_end_matches(':') {
                    "description" | "details" | "particulars" => "Narrative",
                    "balance" => "Running",
                    "deposit" | "deposits" | "credit" | "credits" => "In",
                    "withdrawal" | "withdrawals" | "debit" | "debits" => "Out",
                    "date" => "When",
                    _ => return None,
                })
            };
            let (tokens, _) = fingraph_core::parser::page_tokens(pdf).unwrap();
            let renamed: Vec<_> = tokens.into_iter().map(|t| fingraph_core::parser::Token { text: rename(&t.text).map_or(t.text.clone(), str::to_string), ..t }).collect();
            let again = MultiPassScanner::new(&profiles).scan_tokens(renamed).unwrap();
            let verdict = match (rows(&again) == got, again.is_reconciled()) {
                (true, true) => "same rows, adds up",
                (true, false) => "same rows, but nothing left to check them against (held)",
                _ => "DIFFERENT rows",
            };
            if std::env::var("FINGRAPH_SAMPLES_VERBOSE").is_ok() && rows(&again) != got {
                {
                    for s in &again.sections {
                        println!("      renamed read: {:?} open {:?} close {:?}", s.reconciliation, s.opening, s.closing);
                        for t in &s.transactions {
                            println!("        {} {:>12} bal {:?} {}", t.posted, t.amount, t.balance, t.description);
                        }
                    }
                }
            }
            println!("    renamed headings: {verdict} ({} rows, read_as {:?})", rows(&again).len(), again.diagnostics.pass3.read_as);
        }
        // the import rules, in a throwaway in-memory ledger
        let db = fingraph_core::db::Db::open_in_memory().unwrap();
        let ent = db.create_entity("Test", fingraph_core::EntityKind::Personal, fingraph_core::Currency::KYD).unwrap();
        match db.import_auto(ent, &name, "sample", &r) {
            Ok(rep) => {
                let holds: Vec<_> = db.documents_all(&[]).unwrap().into_iter().map(|d| d.review).collect();
                println!("    import: {} rows, held {:?}", rep.inserted, holds);
            }
            Err(e) => println!("    import refused: {e}"),
        }
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
