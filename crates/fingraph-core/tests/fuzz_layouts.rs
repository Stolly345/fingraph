//! Statement generator: thousands of statements with random layouts and known answers, rendered as positioned
//! words the way `pdftotext -bbox` gives them, then scanned with no bank profile (every one is an unknown bank).
//!
//! `cargo test -p fingraph-core --test fuzz_layouts -- --nocapture` scores 400 (`FINGRAPH_FUZZ_N` for more,
//! `FINGRAPH_FUZZ_SEED` for another set, `FINGRAPH_FUZZ_SHOW=<n>` prints statement n). The one result that
//! must never happen — rows that are wrong, yet marked Reconciled — fails the test.
use chrono::{Datelike, Duration, NaiveDate};
use fingraph_core::parser::{MultiPassScanner, Token};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

const CW: f64 = 5.2;
const H: f64 = 8.0;
const LINE: f64 = 11.0;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn cents(&mut self, lo: i64, hi: i64) -> Decimal {
        Decimal::new(lo * 100 + (self.next() % ((hi - lo) * 100) as u64) as i64, 2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cols {
    DebitCreditBalance,
    CreditDebitBalance,
    SignedBalance,
    MarkedBalance,
    CardAmount,
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum Heads {
    English,
    Unusual,
    French,
    Spanish,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum DateFmt {
    DmySlash,
    MdySlash,
    DMon,
    MonD,
    DMonYyDash,
    Iso,
    French,
    Spanish,
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum Num {
    Dot,
    Comma,
    Space,
    Plain,
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum Neg {
    Minus,
    Trailing,
    Parens,
}

#[derive(Debug, Clone)]
struct Layout {
    cols: Cols,
    heads: Heads,
    date: DateFmt,
    num: Num,
    neg: Neg,
    currency: bool,
    newest_first: bool,
    wrap: Option<bool>, // Some(true): amount on the first line, Some(false): on the last
    opening_row: bool,
    summary_box: bool,
    watermark: bool,
    margin: bool,
    relisting: bool,
    per_page: usize,
}

impl Layout {
    fn features(&self) -> Vec<String> {
        let mut v = vec![format!("{:?}", self.cols), format!("heads:{:?}", self.heads), format!("date:{:?}", self.date), format!("num:{:?}", self.num)];
        if matches!(self.cols, Cols::SignedBalance) {
            v.push(format!("neg:{:?}", self.neg));
        }
        for (on, name) in [
            (self.currency, "currency"),
            (self.newest_first, "newest_first"),
            (self.wrap == Some(true), "wrap_amount_first"),
            (self.wrap == Some(false), "wrap_amount_last"),
            (self.opening_row, "opening_row"),
            (self.summary_box, "summary_box"),
            (self.watermark, "watermark"),
            (self.margin, "margin"),
            (self.relisting, "relisting"),
            (!self.opening_row && !self.summary_box, "no_opening_printed"),
        ] {
            if on {
                v.push(name.into());
            }
        }
        v
    }
}

struct Tx {
    date: NaiveDate,
    desc: String,
    memo: String,
    /// Ledger sign: negative = money out of the holder's pocket (a card charge too).
    amount: Decimal,
}

const MERCHANTS: &[&str] = &["GROCERY MART", "CITY POWER CO", "WATER AUTHORITY", "COFFEE HOUSE", "FUEL STATION 12", "ONLINE STORE", "PHARMACY PLUS", "HARDWARE DEPOT", "STREAMING SVC", "RESTAURANT BAY"];

fn fr_month(m: u32) -> &'static str {
    ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."][m as usize - 1]
}
fn es_month(m: u32) -> &'static str {
    ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic"][m as usize - 1]
}
fn en_month(m: u32) -> &'static str {
    ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"][m as usize - 1]
}

fn fmt_date(d: NaiveDate, f: DateFmt, with_year: bool) -> String {
    let (dd, mm, y) = (d.day(), d.month(), d.year());
    match f {
        DateFmt::DmySlash => format!("{dd:02}/{mm:02}/{y}"),
        DateFmt::MdySlash => format!("{mm:02}/{dd:02}/{y}"),
        DateFmt::DMon if with_year => format!("{dd:02} {} {y}", en_month(mm)),
        DateFmt::DMon => format!("{dd:02} {}", en_month(mm)),
        DateFmt::MonD if with_year => format!("{} {dd}, {y}", en_month(mm)),
        DateFmt::MonD => format!("{} {dd:02}", en_month(mm)),
        DateFmt::DMonYyDash => format!("{dd:02}-{}-{:02}", en_month(mm), y % 100),
        DateFmt::Iso => format!("{y}-{mm:02}-{dd:02}"),
        DateFmt::French => format!("{dd} {} {y}", fr_month(mm)),
        DateFmt::Spanish => format!("{dd} {} {y}", es_month(mm)),
    }
}

fn fmt_num(v: Decimal, n: Num) -> String {
    let s = format!("{:.2}", v.abs());
    let (int, frac) = s.split_once('.').unwrap();
    let group = |sep: &str| {
        let b = int.as_bytes();
        let mut out = String::new();
        for (i, c) in b.iter().enumerate() {
            if i > 0 && (b.len() - i) % 3 == 0 {
                out.push_str(sep);
            }
            out.push(*c as char);
        }
        out
    };
    match n {
        Num::Dot => format!("{}.{frac}", group(",")),
        Num::Comma => format!("{},{frac}", group(".")),
        Num::Space => format!("{},{frac}", group(" ")),
        Num::Plain => format!("{int}.{frac}"),
    }
}

/// An amount as printed: currency sign, then the sign style when it's signed.
fn money(v: Decimal, l: &Layout, signed: bool) -> String {
    let body = format!("{}{}", if l.currency { "$" } else { "" }, fmt_num(v, l.num));
    if !signed || v >= Decimal::ZERO {
        return body;
    }
    match l.neg {
        Neg::Minus => format!("-{body}"),
        Neg::Trailing => format!("{body}-"),
        Neg::Parens => format!("({body})"),
    }
}

struct Sheet {
    page: u32,
    y: f64,
    tokens: Vec<Token>,
}
impl Sheet {
    fn put(&mut self, x: f64, text: &str) {
        let mut x = x;
        for w in text.split_whitespace() {
            let wd = w.chars().count() as f64 * CW;
            self.tokens.push(Token::new(self.page, x, self.y, x + wd, self.y + H, w));
            x += wd + CW * 0.6;
        }
    }
    fn right(&mut self, xr: f64, text: &str) {
        let n = text.split_whitespace().count();
        let w: f64 = text.split_whitespace().map(|w| w.chars().count() as f64 * CW).sum::<f64>() + CW * 0.6 * n.saturating_sub(1) as f64;
        self.put(xr - w, text);
    }
}

struct Statement {
    layout: Layout,
    tokens: Vec<Token>,
    txs: Vec<(NaiveDate, Decimal)>,
}

fn generate(seed: u64) -> Statement {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for _ in 0..4 {
        r.next();
    }
    let cols = [Cols::DebitCreditBalance, Cols::CreditDebitBalance, Cols::SignedBalance, Cols::MarkedBalance, Cols::CardAmount][r.below(5)];
    let date = [DateFmt::DmySlash, DateFmt::MdySlash, DateFmt::DMon, DateFmt::MonD, DateFmt::DMonYyDash, DateFmt::Iso, DateFmt::French, DateFmt::Spanish][r.below(8)];
    let heads = match date {
        DateFmt::French => Heads::French,
        DateFmt::Spanish => Heads::Spanish,
        _ => [Heads::English, Heads::English, Heads::Unusual, Heads::None][r.below(4)],
    };
    let num = match heads {
        Heads::French | Heads::Spanish => [Num::Comma, Num::Space][r.below(2)],
        _ => [Num::Dot, Num::Dot, Num::Plain, Num::Comma, Num::Space][r.below(5)],
    };
    let layout = Layout {
        cols,
        heads,
        date,
        num,
        neg: [Neg::Minus, Neg::Trailing, Neg::Parens][r.below(3)],
        currency: r.chance(30),
        newest_first: r.chance(15),
        wrap: if r.chance(35) { Some(r.chance(70)) } else { None },
        opening_row: cols != Cols::CardAmount && r.chance(50),
        summary_box: cols == Cols::CardAmount || r.chance(60),
        watermark: r.chance(15),
        margin: r.chance(15),
        relisting: r.chance(10),
        per_page: 12 + r.below(20),
    };
    let l = &layout;
    // the period: a month, in 2024 or 2025
    let start = NaiveDate::from_ymd_opt(2024 + r.below(2) as i32, 1 + r.below(12) as u32, 1).unwrap();
    let end = (start + Duration::days(32)).with_day(1).unwrap() - Duration::days(1);
    let n = 6 + r.below(30);
    let mut days: Vec<i64> = (0..n).map(|_| r.below((end - start).num_days() as usize + 1) as i64).collect();
    days.sort();
    let card = cols == Cols::CardAmount;
    let mut txs: Vec<Tx> = days
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let deposit = if card { r.chance(10) } else { r.chance(25) };
            let amt = if deposit { r.cents(50, 3000) } else { -r.cents(2, 900) };
            let desc = if deposit { if card { "PAYMENT RECEIVED".to_string() } else { format!("DEPOSIT REF {}", 1000 + i) } } else { MERCHANTS[r.below(MERCHANTS.len())].to_string() };
            Tx { date: start + Duration::days(*d), desc, memo: format!("REF {} AUTH {}", 400_000 + r.below(99_999), r.below(9_999)), amount: amt }
        })
        .collect();
    let opening = if card { r.cents(0, 2000) } else { r.cents(1000, 20_000) };
    // statement terms: a card's balance is what's owed (a charge raises it)
    let stmt = |t: &Tx| if card { -t.amount } else { t.amount };
    let mut bal = opening;
    let mut bals = Vec::new();
    for t in &txs {
        bal += stmt(t);
        bals.push(bal);
    }
    let closing = bal;
    let truth: Vec<(NaiveDate, Decimal)> = txs.iter().map(|t| (t.date, t.amount)).collect();

    let mut sh = Sheet { page: 1, y: 40.0, tokens: Vec::new() };
    let bank = ["Meadowlark Savings Bank", "Coral Reef Trust Ltd.", "First Harbour Bank", "Union Pacific Credit Union"][r.below(4)];
    sh.put(40.0, bank);
    sh.y += LINE;
    sh.put(40.0, "P.O. Box 1234, Grand Town");
    sh.y += LINE * 1.5;
    let (from, to) = match heads {
        Heads::French => ("Période du", "au"),
        Heads::Spanish => ("Período del", "al"),
        _ => ("Statement Period:", "to"),
    };
    sh.put(40.0, &format!("{from} {} {to} {}", fmt_date(start, date, true), fmt_date(end, date, true)));
    sh.y += LINE;
    sh.put(40.0, &format!("Account Number: ****{}", 1000 + r.below(8999)));
    sh.y += LINE * 1.5;
    // card statements always say so (credit limit, minimum payment): it's how a charge's sign is known
    if card {
        sh.put(40.0, ["Credit Limit:", "Minimum Payment Due:", "Visa Credit Card"][r.below(3)]);
        sh.right(260.0, &money(r.cents(500, 9000), l, false));
        sh.y += LINE * 1.5;
    }
    let (lab_open, lab_close) = match heads {
        Heads::French => ("Solde précédent", "Nouveau solde"),
        Heads::Spanish => ("Saldo anterior", "Saldo final"),
        _ => [("Opening Balance", "Closing Balance"), ("Previous Balance", "New Balance"), ("Beginning Balance", "Ending Balance"), ("Balance Brought Forward", "Balance Carried Forward")][r.below(4)],
    };
    if l.summary_box {
        sh.put(40.0, lab_open);
        sh.right(260.0, &money(opening, l, false));
        sh.y += LINE;
        sh.put(40.0, lab_close);
        sh.right(260.0, &money(closing, l, true));
        sh.y += LINE * 2.0;
    }

    // columns: date at 40, description at 120, money right edges at 420 / 490 / 560
    let (h_date, h_desc, h_out, h_in, h_bal, h_amt) = match heads {
        Heads::English => ("Date", "Description", "Withdrawals", "Deposits", "Balance", "Amount"),
        Heads::Unusual => ("Posted", "Narrative", "Money Out", "Money In", "Running Total", "Value"),
        Heads::French => ("Date", "Libellé", "Débit", "Crédit", "Solde", "Montant"),
        Heads::Spanish => ("Fecha", "Concepto", "Cargo", "Abono", "Saldo", "Importe"),
        Heads::None => ("", "", "", "", "", ""),
    };
    let money_heads: Vec<(f64, &str)> = match cols {
        Cols::DebitCreditBalance => vec![(420.0, h_out), (490.0, h_in), (560.0, h_bal)],
        Cols::CreditDebitBalance => vec![(420.0, h_in), (490.0, h_out), (560.0, h_bal)],
        Cols::SignedBalance | Cols::MarkedBalance => vec![(490.0, h_amt), (560.0, h_bal)],
        Cols::CardAmount => vec![(560.0, h_amt)],
    };
    let header = |sh: &mut Sheet| {
        if heads == Heads::None {
            return;
        }
        sh.put(40.0, h_date);
        sh.put(120.0, h_desc);
        for (x, h) in &money_heads {
            sh.right(*x, h);
        }
        sh.y += LINE * 1.4;
    };
    let margin = |sh: &mut Sheet, r: &mut Rng| {
        if l.margin && r.chance(40) {
            sh.put(12.0, ["AC", "CT", "RO", "NI"][r.below(4)]);
        }
    };
    header(&mut sh);
    // balance lines in the table: opening above the oldest row, closing below the newest (so a newest-first
    // list prints the closing at the top and the opening at the bottom)
    let open_label = match heads {
        Heads::French => "Solde reporté",
        Heads::Spanish => "Saldo anterior",
        _ => "Opening Balance",
    };
    let close_label = match heads {
        Heads::French => "Nouveau solde",
        Heads::Spanish => "Saldo final",
        _ => "Closing Balance",
    };
    let balance_line = |sh: &mut Sheet, label: &str, v: Decimal| {
        sh.put(120.0, label);
        sh.right(560.0, &money(v, l, true));
        sh.y += LINE;
    };
    let (first_line, last_line) = if l.newest_first { ((close_label, closing), (open_label, opening)) } else { ((open_label, opening), (close_label, closing)) };
    if l.opening_row {
        balance_line(&mut sh, first_line.0, first_line.1);
    }
    let order: Vec<usize> = if l.newest_first { (0..txs.len()).rev().collect() } else { (0..txs.len()).collect() };
    let mut on_page = 0;
    for &i in &order {
        if on_page == l.per_page {
            sh.page += 1;
            sh.y = 40.0;
            sh.put(40.0, bank);
            sh.y += LINE * 2.0;
            header(&mut sh);
            on_page = 0;
        }
        on_page += 1;
        let t = &txs[i];
        let amounts = |sh: &mut Sheet| {
            let v = stmt(t);
            match cols {
                Cols::DebitCreditBalance | Cols::CreditDebitBalance => {
                    let out_x = if cols == Cols::DebitCreditBalance { 420.0 } else { 490.0 };
                    let in_x = if cols == Cols::DebitCreditBalance { 490.0 } else { 420.0 };
                    sh.right(if v < Decimal::ZERO { out_x } else { in_x }, &money(v.abs(), l, false));
                }
                Cols::SignedBalance => sh.right(490.0, &money(v, l, true)),
                Cols::MarkedBalance => sh.right(490.0, &format!("{} {}", money(v.abs(), l, false), if v < Decimal::ZERO { "DR" } else { "CR" })),
                Cols::CardAmount => sh.right(560.0, &money(v, l, true)),
            }
            if cols != Cols::CardAmount {
                // an overdrawn balance carries the statement's minus sign
                sh.right(560.0, &money(bals[i], l, true));
            }
        };
        margin(&mut sh, &mut r);
        sh.put(40.0, &fmt_date(t.date, date, false));
        sh.put(120.0, &t.desc);
        match l.wrap {
            Some(false) => {
                sh.y += LINE;
                sh.put(120.0, &t.memo);
                amounts(&mut sh);
            }
            Some(true) => {
                amounts(&mut sh);
                sh.y += LINE;
                sh.put(120.0, &t.memo);
            }
            None => amounts(&mut sh),
        }
        sh.y += LINE;
    }
    if l.opening_row {
        balance_line(&mut sh, last_line.0, last_line.1);
    }
    if l.relisting {
        sh.y += LINE * 2.0;
        sh.put(40.0, "Transactions by type");
        sh.y += LINE * 1.4;
        for t in txs.iter_mut().filter(|t| t.amount < Decimal::ZERO) {
            sh.put(40.0, &fmt_date(t.date, date, false));
            sh.put(120.0, &t.desc);
            sh.right(560.0, &money(t.amount.abs(), l, false));
            sh.y += LINE;
        }
    }
    if l.watermark {
        let pages = sh.page;
        for p in 1..=pages {
            for (k, c) in "SAMPLE".chars().enumerate() {
                let (x, y) = (180.0 + k as f64 * 40.0, 400.0 - k as f64 * 40.0);
                sh.tokens.push(Token::new(p, x, y, x + 24.0, y + 34.0, &c.to_string()));
            }
        }
    }
    Statement { layout, tokens: sh.tokens, txs: truth }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Outcome {
    /// Every row right, and proven.
    Exact,
    /// Every row right, but not proven (held).
    RightHeld,
    /// Not right, and held: a safe miss.
    WrongHeld,
    /// Not right, yet marked Reconciled: must never happen.
    WrongReconciled,
}

fn judge(s: &Statement) -> (Outcome, String) {
    let r = MultiPassScanner::new(&[]).scan_tokens(s.tokens.clone()).unwrap();
    let mut got: Vec<(NaiveDate, Decimal)> = r.sections.iter().flat_map(|s| &s.transactions).map(|t| (t.posted, t.amount)).collect();
    let mut want = s.txs.clone();
    got.sort();
    want.sort();
    let right = got == want;
    let proven = r.is_reconciled();
    let outcome = match (right, proven) {
        (true, true) => Outcome::Exact,
        (true, false) => Outcome::RightHeld,
        (false, false) => Outcome::WrongHeld,
        (false, true) => Outcome::WrongReconciled,
    };
    let missing = want.iter().filter(|w| !got.contains(w)).count();
    let extra = got.iter().filter(|g| !want.contains(g)).count();
    (outcome, format!("{} rows read of {}, {missing} missing, {extra} wrong, {:?}", got.len(), want.len(), r.sections.iter().map(|s| &s.reconciliation).collect::<Vec<_>>()))
}

#[test]
fn generated_statements() {
    let n: u64 = std::env::var("FINGRAPH_FUZZ_N").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
    let seed: u64 = std::env::var("FINGRAPH_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    if let Some(k) = std::env::var("FINGRAPH_FUZZ_SHOW").ok().and_then(|v| v.parse::<u64>().ok()) {
        let s = generate(seed * 1_000_003 + k);
        println!("{:?}", s.layout);
        let mut toks = s.tokens.clone();
        toks.sort_by_key(|t| (t.page, t.y0 as i64, t.x0 as i64));
        let mut line = (0, -1.0);
        for t in &toks {
            if (t.page, t.y0) != line {
                print!("\n{:>2} {:>5.0} ", t.page, t.y0);
                line = (t.page, t.y0);
            }
            print!("{:>4.0}:{} ", t.x0, t.text);
        }
        println!("\n{:?}", s.txs);
        let r = MultiPassScanner::new(&[]).scan_tokens(s.tokens.clone()).unwrap();
        for sec in &r.sections {
            println!("{:?} open {:?} close {:?} read_as {:?}", sec.reconciliation, sec.opening, sec.closing, r.diagnostics.pass3.read_as);
            for t in &sec.transactions {
                println!("  {} {} bal {:?} {}", t.posted, t.amount, t.balance, t.description);
            }
        }
        println!("{}", judge(&s).1);
        return;
    }
    let mut tally: BTreeMap<Outcome, usize> = BTreeMap::new();
    // per feature: (statements with it, of those not read exactly)
    let mut by_feature: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut bad = Vec::new();
    for k in 0..n {
        let s = generate(seed * 1_000_003 + k);
        let (o, why) = judge(&s);
        *tally.entry(o).or_default() += 1;
        for f in s.layout.features() {
            let e = by_feature.entry(f).or_default();
            e.0 += 1;
            e.1 += (o != Outcome::Exact) as usize;
        }
        if std::env::var("FINGRAPH_FUZZ_LIST").is_ok() && o != Outcome::Exact {
            println!("#{k} {o:?}: {why} {:?}", s.layout.features());
        }
        if o == Outcome::WrongReconciled {
            bad.push(format!("#{k}: {why} {:?}", s.layout.features()));
        }
    }
    let exact = tally.get(&Outcome::Exact).copied().unwrap_or(0);
    println!("{n} generated statements: {:.1}% read exactly and proven; {tally:?}", 100.0 * exact as f64 / n as f64);
    println!("not exact, by feature (share of statements with that feature):");
    let mut rows: Vec<_> = by_feature.into_iter().collect();
    rows.sort_by(|a, b| (b.1.1 as f64 / b.1.0 as f64).total_cmp(&(a.1.1 as f64 / a.1.0 as f64)));
    for (f, (all, miss)) in rows {
        println!("  {f:<24} {miss:>4}/{all:<4} {:>5.1}%", 100.0 * miss as f64 / all as f64);
    }
    assert!(bad.is_empty(), "wrong rows marked Reconciled:\n{}", bad.join("\n"));
    // the default set (seed 1, 400 statements) read 96.8% exactly when the engine was finished (2026-10-30): a
    // change that loses more than a couple of points shows here
    if n == 400 && seed == 1 {
        assert!(exact * 1000 >= 940 * n as usize, "exact reads fell to {exact}/{n}");
    }
}
