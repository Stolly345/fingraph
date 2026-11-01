//! `cargo run -p fingraph-core --example ledger -- reset|rescan|reapply|cards`: the app's Reset Learning and
//! Rescan All Statements, on the default ledger, from a terminal.

use fingraph_core::db::Db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open_default()?;
    for cmd in std::env::args().skip(1) {
        match cmd.as_str() {
            "reset" => println!("reset: {} transactions back in triage", db.reset_learning(&[])?),
            "rescan" => {
                let (n, txs, missing) = db.rescan_all()?;
                println!("rescan: {n} statements, {txs} transactions, missing {missing:?}");
            }
            "reapply" => println!("reapply: {} transactions updated", db.reapply_rules()?),
            "cards" => {
                let pairs = db.match_transfers()?;
                println!("cards: {pairs} new transfer pairs, {} credit card payments recognised", db.mark_card_payments()?);
            }
            other => eprintln!("unknown command {other} (reset, rescan, reapply, cards)"),
        }
    }
    Ok(())
}
