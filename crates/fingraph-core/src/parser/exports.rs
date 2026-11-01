//! Bank downloads — CSV and OFX/QFX — read into the same `ScanResult` a PDF gives, so account matching,
//! review holds and duplicate checks work the same. One file can hold several accounts (RBC's CSV
//! download lists every account in one file): each account+currency becomes its own section.
//!
//! A download proves less than a statement: a CSV usually has no balances at all (held as NoBalances,
//! unless the account's previous statement's closing balance makes the rows add up), and an OFX has only
//! the closing balance.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDate;
use regex::Regex;
use rust_decimal::Decimal;

use super::{BankProfile, Diagnostics, ScanResult, ScannedTx, Section, SectionReport};
use crate::{AccountType, Currency, Error, Reconciliation, Result};

/// `None` when the file is a PDF (or anything else that isn't a CSV/OFX text download).
pub fn read(path: &Path, profiles: &[BankProfile]) -> Option<Result<ScanResult>> {
    let bytes = std::fs::read(path).ok()?;
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(4).any(|w| w == b"%PDF") {
        return None;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let text = decode(&bytes);
    if text.chars().take(1024).any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n' | '\x0c')) {
        // binary: a .pdf goes to the PDF scanner (which says what's wrong with it); anything else isn't a download
        return (ext != "pdf").then(|| Err(Error("this isn't a PDF statement or a CSV / OFX download".into())));
    }
    let text = text.trim_start_matches('\u{feff}');
    let upper_head: String = text.chars().take(4096).collect::<String>().to_uppercase();
    if upper_head.contains("OFXHEADER") || upper_head.contains("<OFX>") {
        return Some(ofx(text, profiles));
    }
    // ponytail: kept copies are all named <sha>.pdf, so anything text-like that isn't OFX is tried as CSV
    if ext == "csv" || ext == "pdf" || text.lines().next().is_some_and(|l| l.contains(',')) {
        return Some(csv(text, profiles));
    }
    None
}

/// UTF-8, else Windows-1252 (what Excel and older bank sites save).
fn decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // ponytail: Latin-1 plus cp1252's common 0x80–0x9F punctuation; the rest of that block becomes '?'
    bytes
        .iter()
        .map(|&b| match b {
            0x80 => '€',
            0x91 => '‘',
            0x92 => '’',
            0x93 => '“',
            0x94 => '”',
            0x96 => '–',
            0x97 => '—',
            0x85 => '…',
            0x81..=0x9f => '?',
            _ => b as char,
        })
        .collect()
}

fn profile_slug(text: &str, profiles: &[BankProfile]) -> Option<String> {
    let t = text.to_lowercase();
    profiles.iter().find(|p| p.routing_identifiers.iter().any(|id| id.len() >= 3 && t.contains(&id.to_lowercase()))).map(|p| p.slug.clone())
}

fn account_type(text: &str) -> Option<AccountType> {
    let t = text.to_lowercase();
    if ["visa", "mastercard", "master card", "amex", "credit card", "creditcard", "card"].iter().any(|w| t.contains(w)) {
        Some(AccountType::CreditCard)
    } else if ["loan", "mortgage", "creditline", "line of credit"].iter().any(|w| t.contains(w)) {
        Some(AccountType::Loan)
    } else if t.contains("saving") {
        Some(AccountType::Savings)
    } else if ["chequing", "checking", "current", "moneymrkt"].iter().any(|w| t.contains(w)) {
        Some(AccountType::Checking)
    } else {
        None
    }
}

fn last4(number: &str) -> Option<String> {
    let d: String = number.chars().filter(char::is_ascii_digit).collect();
    (d.len() >= 4).then(|| d[d.len() - 4..].to_string())
}

fn amount(cell: &str) -> Option<Decimal> {
    let c = cell.trim();
    if c.is_empty() {
        return None;
    }
    let p = crate::money::parse_printed(c)?;
    let neg = p.negative == Some(true) || p.credit_marker == Some(false);
    Some(if neg { -p.magnitude } else { p.magnitude })
}

/// One account's rows, in file order, before they're signed for the ledger.
struct Rows {
    ty: Option<AccountType>,
    label: Option<String>,
    number: Option<String>,
    currency: Option<Currency>,
    /// (date, description, memo, amount with money-out negative, printed balance)
    rows: Vec<(NaiveDate, String, String, Decimal, Option<Decimal>)>,
    closing: Option<Decimal>,
}

/// Rows → a section. Amounts arrive signed the account holder's way (money out negative), which is how
/// CSV and OFX downloads both print them, cards included.
fn section(mut r: Rows, notes: &mut Vec<String>, default_ccy: Currency) -> Section {
    // newest-first downloads are read oldest-first, keeping the order within a day
    if r.rows.first().map(|x| x.0) > r.rows.last().map(|x| x.0) {
        r.rows.reverse();
    }
    let ty = r.ty.unwrap_or(AccountType::Checking);
    let name = r.number.as_deref().and_then(last4).map_or_else(|| "the account".to_string(), |l| format!("••{l}"));
    if r.ty.is_none() {
        notes.push(format!("The download doesn't say what kind of account {name} is: Checking was assumed"));
    }
    let currency = r.currency.unwrap_or_else(|| {
        notes.push(format!("The download names no currency for {name}: {} was assumed", default_ccy.code()));
        default_ccy
    });
    // most downloads print card purchases as money out (negative); some (Amex-style) print them positive.
    // With no balances to say which, a card whose rows are mostly positive is read the other way, and held.
    if ty.is_liability() && r.rows.iter().all(|x| x.4.is_none()) && r.rows.iter().filter(|x| x.3 > Decimal::ZERO).count() > r.rows.iter().filter(|x| x.3 < Decimal::ZERO).count() {
        for x in &mut r.rows {
            x.3 = -x.3;
        }
        notes.push(format!("Most of {name}'s rows are positive, so they were read as purchases (money out) — check one you know"));
    }
    let stmt = |a: Decimal| if ty.is_liability() { -a } else { a };
    // a balance column: which way it runs (a card's can print what you owe as negative) is whichever the rows agree with
    let flip = {
        let deltas: Vec<(Decimal, Decimal)> = r.rows.windows(2).filter_map(|w| Some((w[1].4? - w[0].4?, stmt(w[1].3)))).collect();
        deltas.iter().filter(|(d, a)| *d == -*a).count() > deltas.iter().filter(|(d, a)| d == a).count()
    };
    let bal = |b: Decimal| if flip { -b } else { b };
    let transactions: Vec<ScannedTx> = r
        .rows
        .iter()
        .enumerate()
        .map(|(i, (date, desc, memo, amt, balance))| ScannedTx {
            posted: *date,
            effective: None,
            description: desc.clone(),
            memo: memo.clone(),
            amount: *amt,
            stmt_amount: stmt(*amt),
            balance: balance.map(bal),
            fx_original: None,
            page: 1,
            y: i as f64,
            recovered: false,
            sign_flexible: false,
        })
        .collect();
    let all_balances = !transactions.is_empty() && transactions.iter().all(|t| t.balance.is_some());
    // ponytail: with a balance on every row the first row's own balance is the opening, so rows 2..n are proven, row 1 isn't
    let opening = all_balances.then(|| transactions[0].balance.unwrap() - transactions[0].stmt_amount);
    let closing = if all_balances { transactions.last().and_then(|t| t.balance) } else { r.closing.map(|c| if ty.is_liability() { -c } else { c }) };
    let mut s = Section {
        currency,
        account_type: ty,
        account_last4: r.number.as_deref().and_then(last4),
        account_label: r.label,
        opening,
        closing,
        total_debits: None,
        total_credits: None,
        transactions,
        reconciliation: Reconciliation::Unverified,
        confidence: 0.0,
        report: SectionReport::default(),
        checkpoints: Vec::new(),
    };
    super::reconcile::verify_and_heal(&mut s);
    s
}

fn finish(groups: Vec<Rows>, slug: Option<String>, period: Option<(NaiveDate, NaiveDate)>, mut notes: Vec<String>, default_ccy: Currency) -> Result<ScanResult> {
    let sections: Vec<Section> = groups.into_iter().filter(|g| !g.rows.is_empty() || g.closing.is_some()).map(|g| section(g, &mut notes, default_ccy)).collect();
    if sections.iter().all(|s| s.transactions.is_empty()) {
        return Err(Error("no transactions were found in this download".into()));
    }
    let dates = sections.iter().flat_map(|s| &s.transactions).map(|t| t.posted);
    let period = period.or_else(|| Some((dates.clone().min()?, dates.max()?)));
    let mut diagnostics = Diagnostics::default();
    diagnostics.pass1.profile = slug.clone();
    diagnostics.pass1.period = period;
    if let [s] = sections.as_slice() {
        diagnostics.pass1.account_last4 = s.account_last4.clone();
        diagnostics.pass1.account_type = Some(s.account_type);
        diagnostics.pass1.currency = Some(s.currency);
    }
    diagnostics.pass3.layout_notes = notes;
    Ok(ScanResult { profile_slug: slug, period, sections, diagnostics })
}

// ---------------------------------------------------------------- OFX / QFX

fn ofx(text: &str, profiles: &[BankProfile]) -> Result<ScanResult> {
    let tag = |block: &str, name: &str| -> Option<String> {
        // a value runs to the next tag or line end, or is wrapped in CDATA (`<NAME><![CDATA[ALDI]]>`)
        let re = Regex::new(&format!(r"(?is)<{name}>\s*(?:<!\[CDATA\[(.*?)\]\]>|([^<\r\n]*))")).ok()?;
        let c = re.captures(block)?;
        let v = c.get(1).or(c.get(2))?.as_str().split_whitespace().collect::<Vec<_>>().join(" ");
        let v = v.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&nbsp;", " ").replace("&amp;", "&");
        Some(v).filter(|v| !v.is_empty())
    };
    let date = |v: Option<String>| v.and_then(|v| NaiveDate::parse_from_str(v.get(..8)?, "%Y%m%d").ok());
    let splitter = Regex::new(r"(?i)<(CC)?STMTRS>").expect("static regex");
    let starts: Vec<(usize, bool)> = splitter.captures_iter(text).map(|c| (c.get(0).unwrap().start(), c.get(1).is_some())).collect();
    if starts.is_empty() {
        return Err(Error("this OFX file holds no bank or card statement".into()));
    }
    let (mut groups, mut from, mut to) = (Vec::new(), None::<NaiveDate>, None::<NaiveDate>);
    for (k, &(at, card)) in starts.iter().enumerate() {
        let block = &text[at..starts.get(k + 1).map_or(text.len(), |n| n.0)];
        let (head, trns) = block.split_once("<STMTTRN>").map_or((block, ""), |(h, _)| (h, &block[h.len()..]));
        let ty = if card { Some(AccountType::CreditCard) } else { tag(head, "ACCTTYPE").and_then(|t| account_type(&t)) };
        if let Some(d) = date(tag(head, "DTSTART")) {
            from = Some(from.map_or(d, |f| f.min(d)));
        }
        if let Some(d) = date(tag(head, "DTEND")) {
            to = Some(to.map_or(d, |t| t.max(d)));
        }
        let mut rows = Vec::new();
        for t in trns.split("<STMTTRN>").skip(1) {
            let (Some(d), Some(a)) = (date(tag(t, "DTPOSTED")), tag(t, "TRNAMT").and_then(|a| amount(&a))) else { continue };
            let name = tag(t, "NAME").or_else(|| tag(t, "PAYEE")).unwrap_or_default();
            let memo = tag(t, "MEMO").unwrap_or_default();
            let (desc, memo) = if name.is_empty() { (memo, String::new()) } else { (name, memo) };
            rows.push((d, desc, memo, a, None));
        }
        let ledger = block.split_once("<LEDGERBAL>").and_then(|(_, b)| tag(b, "BALAMT")).and_then(|a| amount(&a));
        groups.push(Rows { ty, label: None, number: tag(head, "ACCTID"), currency: tag(head, "CURDEF").and_then(|c| Currency::parse(&c)), rows, closing: ledger });
    }
    let period = from.zip(to);
    finish(groups, profile_slug(text, profiles), period, Vec::new(), Currency::KYD)
}

// ---------------------------------------------------------------- CSV

/// RFC 4180: quoted fields, `""` escapes, commas and line breaks inside quotes.
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let (mut out, mut rec, mut field, mut quoted) = (Vec::new(), Vec::new(), String::new(), false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => rec.push(std::mem::take(&mut field)),
            ('\n', false) => {
                rec.push(std::mem::take(&mut field));
                out.push(std::mem::take(&mut rec));
            }
            ('\r', false) => {}
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !rec.is_empty() {
        rec.push(field);
        out.push(rec);
    }
    out.into_iter().filter(|r| r.iter().any(|f| !f.trim().is_empty())).collect()
}

#[derive(Default)]
struct Cols {
    date: Option<usize>,
    desc: Vec<usize>,
    amount: Vec<(usize, Option<Currency>)>,
    debit: Option<usize>,
    credit: Option<usize>,
    balance: Option<usize>,
    number: Option<usize>,
    kind: Option<usize>,
    currency: Option<usize>,
}

fn columns(header: &[String]) -> Cols {
    let mut c = Cols::default();
    for (i, h) in header.iter().enumerate() {
        let h = h.trim().to_lowercase();
        // `CAD$`, `USD$`, `KYD`: an amount column in that currency
        let code = h.trim_end_matches('$').trim();
        if code.len() == 3
            && let Some(ccy) = Currency::parse(code)
        {
            c.amount.push((i, Some(ccy)));
        } else if h.contains("date") {
            // the first date column is the transaction date; a later `posting date` doesn't replace it
            c.date.get_or_insert(i);
        } else if h.contains("account type") || h == "type" && c.kind.is_none() {
            c.kind = Some(i);
        } else if h.contains("account") && (h.contains("number") || h.contains("no") || h == "account") {
            c.number = Some(i);
        } else if h.contains("balance") {
            c.balance = Some(i);
        } else if h.contains("debit") || h.contains("withdrawal") || h == "money out" || h == "paid out" {
            c.debit = Some(i);
        } else if h.contains("credit") || h.contains("deposit") || h == "money in" || h == "paid in" {
            c.credit = Some(i);
        } else if h.contains("amount") {
            c.amount.push((i, None));
        } else if h == "currency" || h == "ccy" {
            c.currency = Some(i);
        } else if ["description", "payee", "details", "narrative", "memo", "merchant", "particulars", "name", "transaction"].iter().any(|w| h.contains(w)) {
            c.desc.push(i);
        }
    }
    c
}

/// Day/month order for numeric dates: proven by any day past the 12th, else month-first (with a note).
fn numeric_parts(s: &str) -> Option<(u32, u32, i32, bool)> {
    let re = Regex::new(r"^(\d{1,4})[/.-](\d{1,2})[/.-](\d{2,4})").ok()?;
    let c = re.captures(s.trim())?;
    let (a, b, y): (u32, u32, i32) = (c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?);
    if c[1].len() == 4 {
        // ISO: year first, then month, day
        return Some((y as u32, b, a as i32, true));
    }
    let y = if y < 100 { 2000 + y } else { y };
    Some((a, b, y, false))
}

fn parse_date(s: &str, month_first: bool) -> Option<NaiveDate> {
    let s = s.trim();
    if let Some((a, b, y, iso)) = numeric_parts(s) {
        let (m, d) = if iso || !month_first { (b, a) } else { (a, b) };
        return NaiveDate::from_ymd_opt(y, m, d);
    }
    ["%d %b %Y", "%d-%b-%Y", "%d-%b-%y", "%b %d, %Y", "%B %d, %Y", "%d %B %Y", "%b %d %Y"].iter().find_map(|f| NaiveDate::parse_from_str(s, f).ok())
}

fn csv(text: &str, profiles: &[BankProfile]) -> Result<ScanResult> {
    let recs = csv_records(text);
    // the header is the first line naming a date and an amount (some banks print a few lines above it)
    let Some((hi, cols)) = recs.iter().enumerate().take(30).map(|(i, r)| (i, columns(r))).find(|(_, c)| c.date.is_some() && (!c.amount.is_empty() || c.debit.is_some() || c.credit.is_some()))
    else {
        return Err(Error("this CSV has no date and amount columns FinGraph recognises".into()));
    };
    let header = recs[hi].iter().map(|h| h.trim().to_lowercase()).collect::<Vec<_>>().join(",");
    if header.contains("amount (workspace)") && header.ends_with("fitid") {
        return Err(Error("this is a FinGraph export, not a bank download: importing it would count every row twice".into()));
    }
    let rbc = header.contains("account type,account number,transaction date,cheque number,description 1,description 2");
    let slug = if rbc { profiles.iter().find(|p| p.slug == "rbc_ky").map(|p| p.slug.clone()) } else { profile_slug(&recs[..hi].iter().flatten().cloned().collect::<Vec<_>>().join(" "), profiles) };
    let body = &recs[hi + 1..];
    let cell = |r: &Vec<String>, i: Option<usize>| i.and_then(|i| r.get(i)).map(|s| s.trim().to_string()).unwrap_or_default();

    let numeric: Vec<(u32, u32, i32, bool)> = body.iter().filter_map(|r| numeric_parts(&cell(r, cols.date))).filter(|p| !p.3).collect();
    let month_first = !numeric.iter().any(|p| p.0 > 12);
    let mut notes = Vec::new();
    if !numeric.is_empty() && !numeric.iter().any(|p| p.0 > 12 || p.1 > 12) {
        notes.push("No date in the download has a day past the 12th: dates were read month first (MM/DD/YYYY) — check one you know".into());
    }

    let mut groups: BTreeMap<(String, String, Option<Currency>), Rows> = BTreeMap::new();
    let mut unread = 0;
    for r in body {
        let Some(date) = parse_date(&cell(r, cols.date), month_first) else {
            unread += 1;
            continue;
        };
        let descs: Vec<String> = cols.desc.iter().map(|&i| cell(r, Some(i))).filter(|d| !d.is_empty()).collect();
        let (desc, memo) = match descs.split_first() {
            Some((d, rest)) => (d.clone(), rest.join(" ")),
            None => (String::new(), String::new()),
        };
        let row_ccy = cols.currency.and_then(|i| Currency::parse(&cell(r, Some(i))));
        let amt: Option<(Decimal, Option<Currency>)> = cols.amount.iter().find_map(|&(i, c)| Some((amount(&cell(r, Some(i)))?, c.or(row_ccy)))).or_else(|| {
            // separate debit/credit columns hold sizes, not signs
            let d = amount(&cell(r, cols.debit)).map(|d| -d.abs());
            let c = amount(&cell(r, cols.credit)).map(|c| c.abs());
            Some((d.or(c)?, row_ccy))
        });
        let Some((amt, ccy)) = amt else {
            unread += 1;
            continue;
        };
        let number = cell(r, cols.number);
        let kind = cell(r, cols.kind);
        let g = groups.entry((number.clone(), kind.clone(), ccy)).or_insert_with(|| Rows {
            ty: account_type(&kind),
            label: (!kind.is_empty()).then(|| kind.clone()),
            number: (!number.is_empty()).then_some(number),
            currency: ccy,
            rows: Vec::new(),
            closing: None,
        });
        g.rows.push((date, desc, memo, amt, amount(&cell(r, cols.balance))));
    }
    if unread > 0 {
        notes.push(format!("{unread} line(s) of the download had no date or amount FinGraph could read and were left out"));
    }
    // lines above the header can name the account, its type or its currency (`Currency:,USD`)
    let pre = &recs[..hi];
    let pre_text = pre.iter().flatten().map(|c| c.trim()).collect::<Vec<_>>().join(" ");
    let pre_ccy = pre_text.split(|c: char| !c.is_ascii_alphabetic()).filter(|w| w.len() == 3).find_map(Currency::parse);
    let pre_number = pre.iter().find(|r| r.first().is_some_and(|c| c.to_lowercase().contains("account"))).and_then(|r| r[1..].iter().find(|c| last4(c).is_some())).cloned();
    let mut groups: Vec<Rows> = groups.into_values().collect();
    for g in &mut groups {
        g.currency = g.currency.or(pre_ccy);
        g.ty = g.ty.or_else(|| account_type(&pre_text));
        if g.number.is_none() {
            g.number.clone_from(&pre_number);
        }
    }
    finish(groups, slug, None, notes, Currency::KYD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::profiles::builtins;

    fn write(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fingraph-exports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn rbc_csv_keeps_accounts_currencies_and_card_type_apart() {
        let p = write(
            "rbc.csv",
            "\"Account Type\",\"Account Number\",\"Transaction Date\",\"Cheque Number\",\"Description 1\",\"Description 2\",\"CAD$\",\"USD$\"\n\
             Savings,01234-5550001,10/2/2026,,\"PAYROLL\",\"HARBOUR FREIGHT\",,2480.15\n\
             Savings,01234-5550001,10/14/2026,,\"TRANSFER\",\"TO VISA\",,-300.00\n\
             Chequing,01234-5550002,10/3/2026,,\"GROCERY, KAYMAN\",\"\",-82.10,\n\
             Visa,4510********7788,10/5/2026,,\"FUEL\",\"\",-45.00,\n\
             Visa,4510********7788,10/20/2026,,\"PAYMENT - THANK YOU\",\"\",300.00,\n",
        );
        let r = read(&p, &builtins()).unwrap().unwrap();
        assert_eq!(r.profile_slug.as_deref(), Some("rbc_ky"));
        assert_eq!(r.sections.len(), 3, "{:?}", r.sections.iter().map(|s| &s.account_last4).collect::<Vec<_>>());
        let by = |l: &str| r.sections.iter().find(|s| s.account_last4.as_deref() == Some(l)).unwrap();
        let sav = by("0001");
        assert_eq!((sav.currency, sav.account_type, sav.transactions.len()), (Currency::USD, AccountType::Savings, 2));
        assert_eq!(by("0002").transactions[0].description, "GROCERY, KAYMAN");
        assert_eq!(by("0002").currency, Currency::parse("CAD").unwrap());
        let visa = by("7788");
        assert_eq!(visa.account_type, AccountType::CreditCard);
        // a card purchase is money out for the ledger, and moves the card's balance up
        assert_eq!((visa.transactions[0].amount, visa.transactions[0].stmt_amount), (Decimal::new(-4500, 2), Decimal::new(4500, 2)));
        assert_eq!(r.period, NaiveDate::from_ymd_opt(2026, 10, 2).zip(NaiveDate::from_ymd_opt(2026, 10, 20)));
        assert!(r.sections.iter().all(|s| s.reconciliation == Reconciliation::Unverified));
    }

    #[test]
    fn csv_with_balances_reconciles_newest_first_and_day_first() {
        let p = write(
            "bal.csv",
            "Date,Description,Debit,Credit,Balance,Currency\n\
             28/10/2026,Coffee,4.50,,995.50,KYD\n\
             15/10/2026,Salary,,500.00,1000.00,KYD\n",
        );
        let r = read(&p, &builtins()).unwrap().unwrap();
        let s = &r.sections[0];
        assert_eq!(s.transactions[0].posted, NaiveDate::from_ymd_opt(2026, 10, 15).unwrap());
        assert_eq!((s.opening, s.closing), (Some(Decimal::new(50000, 2)), Some(Decimal::new(99550, 2))));
        assert_eq!(s.reconciliation, Reconciliation::Reconciled);
        // and a wrong balance is caught
        let p = write("bad.csv", "Date,Description,Amount,Balance\n2026-10-01,A,-10.00,90.00\n2026-10-02,B,-10.00,75.00\n2026-10-03,C,-5.00,70.00\n");
        let r = read(&p, &builtins()).unwrap().unwrap();
        assert_ne!(r.sections[0].reconciliation, Reconciliation::Reconciled);
    }

    #[test]
    fn ofx_reads_each_statement_with_its_closing_balance() {
        let p = write(
            "rbc.ofx",
            "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX><SIGNONMSGSRSV1><SONRS><FI><ORG>Royal Bank of Canada</ORG></FI></SONRS></SIGNONMSGSRSV1>\
             <BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>KYD<BANKACCTFROM><BANKID>12345<ACCTID>5550002<ACCTTYPE>CHECKING</BANKACCTFROM>\
             <BANKTRANLIST><DTSTART>20261001<DTEND>20261031\
             <STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20261003120000[-5:EST]<TRNAMT>-82.10<FITID>1<NAME>GROCERY<MEMO>POS</STMTTRN>\
             <STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20261015<TRNAMT>1000.00<FITID>2<NAME>PAYROLL</STMTTRN>\
             </BANKTRANLIST><LEDGERBAL><BALAMT>1917.90<DTASOF>20261031</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1>\
             <CREDITCARDMSGSRSV1><CCSTMTTRNRS><CCSTMTRS><CURDEF>USD<CCACCTFROM><ACCTID>4510000000007788</CCACCTFROM>\
             <BANKTRANLIST><DTSTART>20261001<DTEND>20261031<STMTTRN><DTPOSTED>20261005<TRNAMT>-45.00<NAME>FUEL</STMTTRN></BANKTRANLIST>\
             <LEDGERBAL><BALAMT>-45.00<DTASOF>20261031</LEDGERBAL></CCSTMTRS></CCSTMTTRNRS></CREDITCARDMSGSRSV1></OFX>",
        );
        let r = read(&p, &builtins()).unwrap().unwrap();
        assert_eq!(r.profile_slug.as_deref(), Some("rbc_ky"));
        assert_eq!(r.sections.len(), 2);
        let (chq, visa) = (&r.sections[0], &r.sections[1]);
        assert_eq!((chq.currency, chq.account_type, chq.account_last4.as_deref()), (Currency::KYD, AccountType::Checking, Some("0002")));
        assert_eq!((chq.transactions[0].description.as_str(), chq.transactions[0].memo.as_str()), ("GROCERY", "POS"));
        assert_eq!(chq.closing, Some(Decimal::new(191790, 2)));
        assert_eq!((visa.currency, visa.account_type, visa.closing), (Currency::USD, AccountType::CreditCard, Some(Decimal::new(4500, 2))));
        assert_eq!(r.period, NaiveDate::from_ymd_opt(2026, 10, 1).zip(NaiveDate::from_ymd_opt(2026, 10, 31)));
    }

    #[test]
    fn awkward_files_from_real_banks_and_spreadsheets() {
        let p = builtins();
        // OFX values in CDATA, with entities
        let r = read(&write("cdata.ofx", "OFXHEADER:100\n<OFX><STMTRS><CURDEF>AUD<BANKACCTFROM><ACCTID>123456789</BANKACCTFROM><BANKTRANLIST><STMTTRN><DTPOSTED>20131215<TRNAMT>-16.85<NAME><![CDATA[EFTPOS  ALDI   ]]></NAME><MEMO>AT&amp;T</MEMO></STMTTRN></BANKTRANLIST></STMTRS></OFX>"), &p).unwrap().unwrap();
        assert_eq!((r.sections[0].transactions[0].description.as_str(), r.sections[0].transactions[0].memo.as_str()), ("EFTPOS ALDI", "AT&T"));
        // two accounts with no account numbers are still two accounts
        let r = read(&write("types.csv", "Account Type,Date,Description,Amount\nSavings,2026-10-01,A,10.00\nChequing,2026-10-01,B,-10.00\n"), &p).unwrap().unwrap();
        assert_eq!(r.sections.len(), 2);
        // a card that prints purchases positive: read as money out, and held
        let r = read(&write("amex.csv", "Date,Description,Amount,Account Type\n10/01/2026,Store,45.00,Credit Card\n10/02/2026,Store2,20.00,Credit Card\n10/09/2026,Payment,-65.00,Credit Card\n"), &p).unwrap().unwrap();
        assert_eq!(r.sections[0].transactions[0].amount, Decimal::new(-4500, 2));
        assert!(r.diagnostics.pass3.layout_notes.iter().any(|n| n.contains("purchases")));
        // Windows-1252 with CRLF and a BOM-less header; the account and currency named above the header
        let mut b = b"Account:,Chequing ****9988\r\nCurrency:,USD\r\n\r\nDate,Description,Amount\r\n2026-10-01,CAF".to_vec();
        b.extend_from_slice(b"\xc9 \x93X\x94,-5.00\r\n");
        let path = write("cp1252.csv", "");
        std::fs::write(&path, b).unwrap();
        let r = read(&path, &p).unwrap().unwrap();
        let s = &r.sections[0];
        assert_eq!((s.transactions[0].description.as_str(), s.currency, s.account_last4.as_deref()), ("CAF\u{c9} \u{201c}X\u{201d}", Currency::USD, Some("9988")));
        // binary, a contacts list, FinGraph's own export: refused with a reason
        let path = write("bin.csv", "");
        std::fs::write(&path, [0x13u8, 0x37, 0x00, 0xff, 0x02]).unwrap();
        assert!(read(&path, &p).unwrap().is_err());
        assert!(read(&write("contacts.csv", "Name,Email\nBob,b@x.com\n"), &p).unwrap().is_err());
        let own = crate::export::csv(&[]) + "2026-10-01,A,B,C,-1.00,KYD,-1.00,KYD,,,,x\n";
        assert!(read(&write("own.csv", &own), &p).unwrap().unwrap_err().to_string().contains("FinGraph export"));
    }

    #[test]
    fn pdfs_are_left_to_the_scanner() {
        assert!(read(&write("x.pdf", "%PDF-1.7\n..."), &[]).is_none());
    }
}
