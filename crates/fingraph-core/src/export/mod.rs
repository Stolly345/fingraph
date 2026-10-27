//! CSV, QIF, OFX 1.0.2 (SGML) and QuickBooks QBO. OFX/QBO are emitted as the SGML dialect Quicken and
//! QuickBooks actually import; QBO adds `<INTU.BID>`, which QuickBooks requires.

use chrono::{Local, NaiveDate};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::db::{Account, TxRow};
use crate::insights::month_start;
use crate::{AccountType, Currency, Fx};

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// `Date,Account,Payee,Description,Amount,Currency,Amount (workspace),Category,Tag,Memo,FITID`
pub fn csv(rows: &[TxRow]) -> String {
    let mut out = String::from("Date,Account,Payee,Description,Amount,Currency,Amount (workspace),Workspace currency,Category,Tag,Memo,FITID\n");
    for t in rows {
        let cells = [
            t.posted.to_string(),
            t.account_name.clone(),
            t.clean_payee.clone(),
            t.raw_description.clone(),
            t.amount.to_string(),
            t.currency.code().into(),
            t.amount_entity.to_string(),
            t.entity_currency.code().into(),
            t.category_name.clone().unwrap_or_default(),
            t.tag.clone(),
            t.memo.clone(),
            t.fitid.clone(),
        ];
        out += &cells.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out
}

/// Quicken Interchange Format, one `!Account` block per account.
pub fn qif(rows: &[TxRow]) -> String {
    let mut by: BTreeMap<(String, AccountType), Vec<&TxRow>> = BTreeMap::new();
    for t in rows {
        by.entry((t.account_name.clone(), t.account_type)).or_default().push(t);
    }
    let mut out = String::new();
    for ((name, ty), mut v) in by {
        let qty = if ty == AccountType::CreditCard { "CCard" } else { "Bank" };
        let _ = write!(out, "!Option:AutoSwitch\n!Account\nN{name}\nT{qty}\n^\n!Clear:AutoSwitch\n!Type:{qty}\n");
        v.sort_by_key(|t| t.posted);
        for t in v {
            let _ = write!(out, "D{}\nT{}\nP{}\n", t.posted.format("%m/%d/%Y"), t.amount, t.clean_payee);
            if !t.memo.is_empty() {
                let _ = writeln!(out, "M{}", t.memo.replace('\n', " "));
            }
            if let Some(c) = &t.category_name {
                let _ = writeln!(out, "L{c}");
            }
            out += "^\n";
        }
    }
    out
}

fn sgml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn trntype(t: &TxRow) -> &'static str {
    let up = t.raw_description.to_uppercase();
    if up.contains("FEE") || up.contains("SERVICE CHARGE") {
        "FEE"
    } else if up.contains("INTEREST") {
        if t.amount.is_sign_positive() { "INT" } else { "DEBIT" }
    } else if up.contains("ATM") || up.contains("ABM") {
        "ATM"
    } else if up.contains("CHEQUE") || up.contains("CHECK") {
        "CHECK"
    } else if t.transfer_peer.is_some() {
        "XFER"
    } else if t.amount.is_sign_positive() {
        "CREDIT"
    } else {
        "DEBIT"
    }
}

/// One account's statement. `intu_bid`: `Some` writes a QBO.
fn ofx_doc(acct: &Account, rows: &[&TxRow], intu_bid: Option<&str>, balance: Option<(Decimal, NaiveDate)>) -> String {
    let now = Local::now().format("%Y%m%d%H%M%S").to_string();
    let from = rows.iter().map(|t| t.posted).min().map(|d| d.format("%Y%m%d").to_string()).unwrap_or_else(|| now[..8].to_string());
    let to = rows.iter().map(|t| t.posted).max().map(|d| d.format("%Y%m%d").to_string()).unwrap_or_else(|| now[..8].to_string());
    let card = acct.account_type == AccountType::CreditCard;
    let acctid = acct.account_number_masked.trim_start_matches('•').to_string();
    let acctid = if acctid.is_empty() { format!("FG{}", acct.id) } else { acctid };
    let bankid = if acct.ofx_bank_id.is_empty() { "000000000" } else { &acct.ofx_bank_id };
    let mut s = String::from("OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n");
    let _ = write!(s, "<OFX>\n<SIGNONMSGSRSV1>\n<SONRS>\n<STATUS>\n<CODE>0\n<SEVERITY>INFO\n</STATUS>\n<DTSERVER>{now}\n<LANGUAGE>ENG\n");
    if let Some(bid) = intu_bid {
        let _ = write!(s, "<FI>\n<ORG>{}\n<FID>{bid}\n</FI>\n<INTU.BID>{bid}\n", sgml(&acct.bank_short));
    }
    s += "</SONRS>\n</SIGNONMSGSRSV1>\n";
    let (msgs, trnrs, stmtrs) = if card { ("CREDITCARDMSGSRSV1", "CCSTMTTRNRS", "CCSTMTRS") } else { ("BANKMSGSRSV1", "STMTTRNRS", "STMTRS") };
    let _ = write!(s, "<{msgs}>\n<{trnrs}>\n<TRNUID>1\n<STATUS>\n<CODE>0\n<SEVERITY>INFO\n</STATUS>\n<{stmtrs}>\n<CURDEF>{}\n", acct.currency.code());
    if card {
        let _ = write!(s, "<CCACCTFROM>\n<ACCTID>{acctid}\n</CCACCTFROM>\n");
    } else {
        let accttype = if matches!(acct.account_type, AccountType::Savings | AccountType::ShareAccount) { "SAVINGS" } else { "CHECKING" };
        let _ = write!(s, "<BANKACCTFROM>\n<BANKID>{bankid}\n<ACCTID>{acctid}\n<ACCTTYPE>{accttype}\n</BANKACCTFROM>\n");
    }
    let _ = write!(s, "<BANKTRANLIST>\n<DTSTART>{from}\n<DTEND>{to}\n");
    for t in rows {
        let name: String = sgml(if t.clean_payee.is_empty() { &t.raw_description } else { &t.clean_payee }).chars().take(32).collect();
        let _ = write!(s, "<STMTTRN>\n<TRNTYPE>{}\n<DTPOSTED>{}\n<TRNAMT>{}\n<FITID>{}\n<NAME>{name}\n", trntype(t), t.posted.format("%Y%m%d"), t.amount, &t.fitid[..t.fitid.len().min(32)]);
        let memo = if t.memo.is_empty() { t.raw_description.clone() } else { format!("{} {}", t.raw_description, t.memo) };
        let _ = write!(s, "<MEMO>{}\n</STMTTRN>\n", sgml(&memo).chars().take(255).collect::<String>());
    }
    s += "</BANKTRANLIST>\n";
    let (bal, asof) = balance.map(|(b, d)| (b, d.format("%Y%m%d").to_string())).unwrap_or((Decimal::ZERO, to));
    let _ = write!(s, "<LEDGERBAL>\n<BALAMT>{bal}\n<DTASOF>{asof}\n</LEDGERBAL>\n</{stmtrs}>\n</{trnrs}>\n</{msgs}>\n</OFX>\n");
    s
}

/// Latest verified running balance for the account (cards: negative, what's owed).
fn last_balance(rows: &[&TxRow]) -> Option<(Decimal, NaiveDate)> {
    rows.iter().filter(|t| t.balance.is_some()).max_by_key(|t| (t.posted, t.id)).map(|t| {
        let b = t.balance.unwrap_or_default();
        (if t.account_type.is_liability() { -b } else { b }, t.posted)
    })
}

/// OFX for one account.
pub fn ofx(acct: &Account, rows: &[TxRow]) -> String {
    let v: Vec<&TxRow> = rows.iter().filter(|t| t.account_id == acct.id).collect();
    ofx_doc(acct, &v, None, last_balance(&v))
}

/// QBO (QuickBooks Web Connect) for one account. QuickBooks refuses a file without a bank id.
pub fn qbo(acct: &Account, rows: &[TxRow]) -> String {
    let v: Vec<&TxRow> = rows.iter().filter(|t| t.account_id == acct.id).collect();
    let bid = if acct.ofx_intu_bid.is_empty() { "3000" } else { &acct.ofx_intu_bid };
    ofx_doc(acct, &v, Some(bid), last_balance(&v))
}

/// Month × category P&L as CSV (income positive, expenses negative, transfers left out).
pub fn pnl_csv(rows: &[TxRow], ccy: Currency, fx: &Fx) -> String {
    let mut months: Vec<NaiveDate> = rows.iter().map(|t| month_start(t.posted)).collect();
    months.sort();
    months.dedup();
    let mut grid: BTreeMap<String, BTreeMap<NaiveDate, Decimal>> = BTreeMap::new();
    for t in rows.iter().filter(|t| !t.is_transfer()) {
        let cat = t.category_name.clone().unwrap_or_else(|| "Uncategorized".into());
        *grid.entry(cat).or_default().entry(month_start(t.posted)).or_default() += crate::insights::value(t, ccy, fx);
    }
    let mut out = format!("Category ({}),{},Total\n", ccy.code(), months.iter().map(|m| m.format("%Y-%m").to_string()).collect::<Vec<_>>().join(","));
    let mut totals: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
    for (cat, m) in &grid {
        let vals: Vec<Decimal> = months.iter().map(|mo| m.get(mo).copied().unwrap_or_default()).collect();
        for (mo, v) in months.iter().zip(&vals) {
            *totals.entry(*mo).or_default() += v;
        }
        let _ = writeln!(out, "{},{},{}", csv_field(cat), vals.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","), vals.iter().copied().sum::<Decimal>());
    }
    let tv: Vec<Decimal> = months.iter().map(|m| totals.get(m).copied().unwrap_or_default()).collect();
    let _ = writeln!(out, "Net,{},{}", tv.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","), tv.iter().copied().sum::<Decimal>());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, TxFilter};

    #[test]
    fn exports_are_well_formed() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let accts = db.accounts().unwrap();
        let c = csv(&rows);
        assert_eq!(c.lines().count(), rows.len() + 1);
        let q = qif(&rows);
        assert_eq!(q.matches("\n^\n").count(), rows.len() + accts.len());
        let card = accts.iter().find(|a| a.account_type == AccountType::CreditCard).unwrap();
        let o = qbo(card, &rows);
        assert!(o.starts_with("OFXHEADER:100") && o.contains("<INTU.BID>3000") && o.contains("<CCSTMTRS>") && o.contains("<CURDEF>USD"));
        let n = rows.iter().filter(|t| t.account_id == card.id).count();
        assert_eq!(o.matches("<STMTTRN>").count(), n);
        assert_eq!(o.matches("<STMTTRN>").count(), o.matches("</STMTTRN>").count());
        let chq = accts.iter().find(|a| a.account_type == AccountType::Checking).unwrap();
        assert!(ofx(chq, &rows).contains("<ACCTTYPE>CHECKING"));
        let p = pnl_csv(&rows, Currency::KYD, &db.fx().unwrap());
        assert!(p.lines().last().unwrap().starts_with("Net,"));
    }
}
