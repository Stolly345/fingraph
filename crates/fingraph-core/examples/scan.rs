//! `cargo run -p fingraph-core --example scan -- statement.pdf [--json]`: run the 5-pass scanner on a PDF.

use fingraph_core::money::fmt_plain;
use fingraph_core::parser::{MultiPassScanner, profiles};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let profiles = profiles::builtins();
    let scanner = MultiPassScanner::new(&profiles);
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        println!("== {path}");
        let r = match scanner.scan_file(path.as_ref()) {
            Ok(r) => r,
            Err(e) => {
                println!("  error: {e}");
                continue;
            }
        };
        if args.iter().any(|a| a == "--json") {
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
            continue;
        }
        println!("  profile {:?} period {:?} scores {:?}", r.profile_slug, r.period, r.diagnostics.pass1.scores);
        for p in &r.diagnostics.pages {
            println!("  page {} header {:?} inherited {} rows {} cols {:?}", p.page, p.header_y, p.inherited, p.rows, p.columns.iter().map(|c| format!("{:?}[{:.0}-{:.0}]", c.role, c.x0, c.x1)).collect::<Vec<_>>());
        }
        for s in &r.sections {
            println!(
                "  section {} {:?} ••{} open {:?} close {:?} dr {:?} cr {:?} → {:?} conf {:.2} ({} tx, {} rescans)",
                s.currency, s.account_type, s.account_last4.as_deref().unwrap_or("?"), s.opening, s.closing, s.total_debits, s.total_credits, s.reconciliation, s.confidence, s.transactions.len(), s.report.rescans
            );
            for t in &s.transactions {
                println!("    {} {:>12} {:>12} {} | {}{}", t.posted, fmt_plain(t.amount), t.balance.map(fmt_plain).unwrap_or_default(), t.description, t.memo, if t.recovered { " [recovered]" } else { "" });
            }
            for g in s.report.segments.iter().filter(|g| !g.ok) {
                println!("    GAP between tx {:?} and {:?}: expected {} got {}", g.from_tx, g.to_tx, g.expected, g.got);
            }
            for h in &s.report.sign_heals {
                println!("    heal: {h}");
            }
            println!("    summary {:?}", s.report.summary);
            for l in &s.report.skipped {
                println!("    skipped p{} {:.0}: {} ({}) {:?} {:?}", l.page, l.y, l.text, l.reason, l.date, l.amount);
            }
        }
    }
}
