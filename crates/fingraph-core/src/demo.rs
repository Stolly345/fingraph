//! Simulated statements for the five Cayman banks, rendered as positioned words exactly like
//! `pdftotext -bbox` would give them. The unit tests scan these, and "Load Demo Data" imports a year
//! of them (Personal + a property business), so every layout quirk and chart has data behind it.

use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::Decimal;

use crate::money::fmt_plain;
use crate::parser::Token;
use crate::{AccountType, Currency};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bank {
    Butterfield,
    Scotiabank,
    Cnb,
    Rbc,
    Cicsa,
}

#[derive(Debug, Clone)]
pub struct DemoTx {
    pub date: NaiveDate,
    pub description: String,
    pub memo: Vec<String>,
    /// Ledger sign: negative = money out of the holder's pocket.
    pub amount: Decimal,
}

impl DemoTx {
    pub fn new(date: NaiveDate, description: &str, amount: Decimal) -> Self {
        DemoTx { date, description: description.into(), memo: Vec::new(), amount }
    }
    pub fn memo(mut self, line: &str) -> Self {
        self.memo.push(line.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct StatementSpec {
    pub bank: Bank,
    pub account_type: AccountType,
    pub currency: Currency,
    pub last4: String,
    /// Printed opening balance (for cards and loans: what's owed).
    pub opening: Decimal,
    pub period: (NaiveDate, NaiveDate),
    pub txs: Vec<DemoTx>,
    /// Test hooks: print this transaction (by index) at the top of the next page without its date,
    /// as a page-break split row; and print this one with its amount drifted into the description.
    pub split_row: Option<usize>,
    pub drift_row: Option<usize>,
    /// And print this one's amount with an OCR-style typo (`1,204,00`) that no pass can read.
    pub garble_row: Option<usize>,
}

impl StatementSpec {
    pub fn new(bank: Bank, account_type: AccountType, currency: Currency, last4: &str, opening: Decimal, period: (NaiveDate, NaiveDate), txs: Vec<DemoTx>) -> Self {
        StatementSpec { bank, account_type, currency, last4: last4.into(), opening, period, txs, split_row: None, drift_row: None, garble_row: None }
    }
    /// Statement terms: positive raises the printed balance.
    fn stmt(&self, t: &DemoTx) -> Decimal {
        if self.account_type.is_liability() { -t.amount } else { t.amount }
    }
    pub fn closing(&self) -> Decimal {
        self.opening + self.txs.iter().map(|t| self.stmt(t)).sum::<Decimal>()
    }
}

const CW: f64 = 4.6; // glyph width at 8pt
const H: f64 = 8.0;
const LINE: f64 = 12.0;
const BOTTOM: f64 = 730.0;

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
            x += wd + CW * 0.55;
        }
    }
    fn right(&mut self, x_right: f64, text: &str) {
        let width: f64 = text.split_whitespace().map(|w| w.chars().count() as f64 * CW).sum::<f64>() + CW * 0.55 * (text.split_whitespace().count().saturating_sub(1)) as f64;
        self.put(x_right - width, text);
    }
    fn nl(&mut self, by: f64) {
        self.y += by;
    }
    fn new_page(&mut self) {
        self.page += 1;
        self.y = 40.0;
    }
}

fn mon(d: NaiveDate) -> &'static str {
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][d.month0() as usize]
}

fn month_name(d: NaiveDate) -> &'static str {
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"][d.month0() as usize]
}

struct Style {
    letterhead: Vec<String>,
    period: String,
    /// (role-ish header, right edge or left x, is_money)
    date_hdr: &'static str,
    post_hdr: Option<&'static str>,
    desc_hdr: &'static str,
    money: Vec<(&'static str, Col)>,
    date_fmt: fn(NaiveDate) -> String,
    /// RBC: date only on a day's first row, balance only on its last.
    grouped: bool,
    summary: [&'static str; 4],
    repeat_header: bool,
    /// CNB: the date on a description's first line, the amount and balance on its last.
    amount_last: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Debit,
    Credit,
    Amount,
    Balance,
}

fn style(spec: &StatementSpec) -> Style {
    let (s, e) = spec.period;
    let card = spec.account_type == AccountType::CreditCard;
    let ccy = spec.currency.code();
    match spec.bank {
        Bank::Butterfield if card => Style {
            letterhead: vec!["Bank of Butterfield (Cayman) Limited".into(), "butterfieldgroup.com  SWIFT BNTBKYKY".into(), format!("Butterfield Visa Gold {ccy}")],
            period: format!("Statement Period: {} {} {} to {} {} {}", s.format("%d"), mon(s), s.year(), e.format("%d"), mon(e), e.year()),
            date_hdr: "Transaction Date",
            post_hdr: Some("Posting Date"),
            desc_hdr: "Description",
            money: vec![("Amount", Col::Amount)],
            date_fmt: |d| format!("{} {} {}", d.format("%d"), mon(d), d.year()),
            grouped: false,
            summary: ["Previous Balance", "Purchases & Debits", "Payments & Credits", "New Balance"],
            repeat_header: true,
            amount_last: false,
        },
        Bank::Butterfield => Style {
            letterhead: vec!["Bank of Butterfield (Cayman) Limited".into(), "butterfieldgroup.com  SWIFT BNTBKYKY".into()],
            period: format!("Statement Period: {} to {}", s.format("%d/%m/%Y"), e.format("%d/%m/%Y")),
            date_hdr: "Date",
            post_hdr: None,
            desc_hdr: "Description",
            money: vec![("Debit", Col::Debit), ("Credit", Col::Credit), ("Balance", Col::Balance)],
            date_fmt: |d| d.format("%d/%m/%Y").to_string(),
            grouped: false,
            summary: ["Opening Balance", "Total Debits", "Total Credits", "Closing Balance"],
            repeat_header: true,
            amount_last: false,
        },
        Bank::Scotiabank if card => Style {
            letterhead: vec!["Scotiabank & Trust (Cayman) Ltd.".into(), "ky.scotiabank.com".into(), format!("Scotiabank Visa {ccy} Credit Card")],
            period: format!("Statement Period {} {} - {} {}, {}", mon(s), s.day(), mon(e), e.day(), e.year()),
            date_hdr: "Transaction Date",
            post_hdr: Some("Posting Date"),
            desc_hdr: "Transaction / Description",
            money: vec![("DEBIT/CREDIT AMOUNT", Col::Amount)],
            date_fmt: |d| format!("{}-{}-{}", d.format("%d"), mon(d), d.year()),
            grouped: false,
            summary: ["Previous Balance", "Purchases, Cash Advances, Other Debits", "Payments & Credits", "New Balance"],
            repeat_header: true,
            amount_last: false,
        },
        Bank::Scotiabank => Style {
            letterhead: vec!["Scotiabank & Trust (Cayman) Ltd.".into(), "ky.scotiabank.com".into()],
            period: format!("Statement Period: {}{}{} to {}{}{}", s.format("%d"), mon(s).to_uppercase(), s.format("%y"), e.format("%d"), mon(e).to_uppercase(), e.format("%y")),
            date_hdr: "Transaction Date",
            post_hdr: None,
            desc_hdr: "Description",
            money: vec![("DEBIT", Col::Debit), ("CREDIT", Col::Credit), ("Balance", Col::Balance)],
            date_fmt: |d| format!("{} {}", mon(d).to_uppercase(), d.format("%d")),
            grouped: false,
            summary: ["Opening Balance", "Withdrawals", "Deposits", "Closing Balance"],
            repeat_header: true,
            amount_last: false,
        },
        // the real layout (a Feb 2026 statement): one Amount column with a trailing minus, `DD/MM`
        Bank::Cnb => Style {
            letterhead: vec!["Cayman National Bank Ltd.".into(), "caymannational.com  SWIFT CNBCKYKY".into(), if card { format!("CNB Visa {ccy}") } else { format!("Business Current Account {ccy}") }],
            period: format!("Period: {} to {}", s.format("%d/%m/%Y"), e.format("%d/%m/%Y")),
            date_hdr: "Date",
            post_hdr: None,
            desc_hdr: "Description",
            money: vec![("Amount", Col::Amount), ("Balance", Col::Balance)],
            date_fmt: |d| d.format("%d/%m").to_string(),
            grouped: false,
            summary: ["Beginning Balance", "Cheques & Other Debit", "Deposits & Other Cred", "Ending Balance"],
            repeat_header: true,
            amount_last: true,
        },
        Bank::Rbc => Style {
            letterhead: vec!["RBC Royal Bank (Cayman) Limited".into(), "Royal Bank of Canada  rbcroyalbank.com  ROYCKYKY".into()],
            period: format!("Statement Period: {} {}, {} to {} {}, {}", month_name(s), s.day(), s.year(), month_name(e), e.day(), e.year()),
            date_hdr: "Date",
            post_hdr: None,
            desc_hdr: "Description",
            money: vec![("Withdrawals ($)", Col::Debit), ("Deposits ($)", Col::Credit), ("Balance ($)", Col::Balance)],
            date_fmt: |d| format!("{} {}", d.format("%d"), mon(d)),
            grouped: true,
            summary: ["Opening Balance", "Total Withdrawals", "Total Deposits", "Closing Balance"],
            repeat_header: false,
            amount_last: false,
        },
        Bank::Cicsa => Style {
            letterhead: vec!["CICSA Co-operative Credit Union Ltd.".into(), "Cayman Islands Civil Service Association  cicsacu.com.ky".into()],
            period: format!("Statement Period: {} to {}", s.format("%d/%m/%Y"), e.format("%d/%m/%Y")),
            date_hdr: "Date",
            post_hdr: None,
            desc_hdr: "Transaction/Reference",
            money: vec![("Withdrawal/Debit", Col::Debit), ("Deposit/Credit", Col::Credit), ("Balance", Col::Balance)],
            date_fmt: |d| d.format("%d/%m/%Y").to_string(),
            grouped: false,
            summary: ["Opening Balance", "Total Withdrawals", "Total Deposits", "Closing Balance"],
            repeat_header: true,
            amount_last: false,
        },
    }
}

fn money_text(bank: Bank, v: Decimal, col: Col) -> String {
    let neg = v.is_sign_negative() && !v.is_zero();
    let p = fmt_plain(v.abs());
    match (bank, col) {
        (Bank::Cnb, _) if neg => format!("{p} -"),
        (Bank::Butterfield, Col::Amount) if neg => format!("{p} CR"),
        (Bank::Scotiabank, Col::Amount) => format!("${}{p}", if neg { "-" } else { "" }),
        (_, _) if neg => format!("-{p}"),
        _ => p,
    }
}

/// Render one or more account sections (a credit-union statement holds several) as one document.
pub fn render(specs: &[StatementSpec]) -> Vec<Token> {
    let mut sh = Sheet { page: 1, y: 40.0, tokens: Vec::new() };
    let first = &specs[0];
    let st = style(first);
    let multi = specs.len() > 1;
    for l in &st.letterhead {
        sh.put(40.0, l);
        sh.nl(LINE);
    }
    sh.right(570.0, "Page 1");
    sh.nl(LINE);
    sh.put(40.0, &st.period);
    sh.nl(LINE);
    if !multi {
        if first.account_type != AccountType::CreditCard {
            sh.put(40.0, &format!("Product: {} Account {}", if first.account_type == AccountType::Savings { "Personal Savings" } else { "Current" }, first.currency.code()));
            sh.nl(LINE);
        }
        let mask = if first.account_type == AccountType::CreditCard { format!("Card Number: **** **** **** {}", first.last4) } else { format!("Account Number: ****{}", first.last4) };
        sh.put(40.0, &mask);
        sh.put(330.0, &format!("Currency: {}", first.currency.code()));
        sh.nl(LINE * 1.5);
    }
    for spec in specs {
        let st = style(spec);
        let liability = spec.account_type.is_liability();
        if multi {
            sh.nl(LINE);
            let title = match spec.account_type {
                AccountType::ShareAccount => "Share Savings Account",
                AccountType::Loan => "Loan Account",
                _ => "Chequing Account",
            };
            sh.put(40.0, &format!("{title} {}", spec.last4));
            sh.nl(LINE);
        }
        // summary box
        let (mut dr, mut cr) = (Decimal::ZERO, Decimal::ZERO);
        for t in &spec.txs {
            let s = spec.stmt(t);
            if (s > Decimal::ZERO) == liability { dr += s.abs() } else { cr += s.abs() }
        }
        for (label, v) in st.summary.iter().zip([spec.opening, dr, cr, spec.closing()]) {
            sh.put(40.0, label);
            sh.right(330.0, &format!("{} {}", spec.currency.symbol(), fmt_plain(v)));
            sh.nl(LINE);
        }
        sh.nl(LINE);

        let date_x = 40.0;
        let post_x = 128.0;
        // the description starts clear of the date heading ("Transaction Date" is wider than the dates)
        let desc_x = if st.post_hdr.is_some() { 206.0 } else { (date_x + st.date_hdr.len() as f64 * CW + 16.0).max(112.0) };
        let rights: Vec<f64> = match st.money.len() {
            1 => vec![565.0],
            _ => vec![405.0, 485.0, 565.0],
        };
        let header = |sh: &mut Sheet| {
            sh.put(date_x, st.date_hdr);
            if let Some(p) = st.post_hdr {
                sh.put(post_x, p);
            }
            sh.put(desc_x, st.desc_hdr);
            for ((h, _), r) in st.money.iter().zip(&rights) {
                sh.right(*r, h);
            }
            sh.nl(LINE * 1.4);
        };
        header(&mut sh);

        let mut bal = spec.opening;
        let mut pending_split: Option<(Decimal, Decimal, Col)> = None;
        for (i, t) in spec.txs.iter().enumerate() {
            let s = spec.stmt(t);
            bal += s;
            let lines = 1 + t.memo.len();
            if sh.y + LINE * lines as f64 > BOTTOM || pending_split.is_some() {
                sh.new_page();
                sh.right(570.0, &format!("Page {}", sh.page));
                sh.nl(LINE * 2.0);
                if st.repeat_header {
                    header(&mut sh);
                }
                if let Some((v, b, col)) = pending_split.take() {
                    // the split row's amount and balance, alone at the top of the page
                    let k = st.money.iter().position(|m| m.1 == col).unwrap_or(0);
                    sh.right(rights[k], &money_text(spec.bank, v, col));
                    if let Some(bk) = st.money.iter().position(|m| m.1 == Col::Balance) {
                        sh.right(rights[bk], &money_text(spec.bank, b, Col::Balance));
                    }
                    sh.nl(LINE);
                }
            }
            let same_day_before = i > 0 && spec.txs[i - 1].date == t.date;
            let same_day_after = spec.txs.get(i + 1).is_some_and(|n| n.date == t.date);
            if !(st.grouped && same_day_before) {
                sh.put(date_x, &(st.date_fmt)(t.date));
            }
            if st.post_hdr.is_some() {
                let posted = t.date + Duration::days(1);
                sh.put(post_x, &(st.date_fmt)(posted));
            }
            sh.put(desc_x, &t.description);
            if st.amount_last {
                for m in &t.memo {
                    sh.nl(LINE);
                    sh.put(desc_x, m);
                }
            }
            let col = match st.money.len() {
                1 => Col::Amount,
                _ if st.money[0].1 == Col::Amount => Col::Amount,
                _ if s.is_sign_negative() != liability => Col::Debit,
                _ => Col::Credit,
            };
            let shown = if col == Col::Amount { s } else { s.abs() };
            if Some(i) == spec.split_row {
                sh.nl(LINE);
                pending_split = Some((shown, bal, col));
                continue;
            }
            let k = st.money.iter().position(|m| m.1 == col).unwrap_or(0);
            if Some(i) == spec.garble_row {
                sh.right(rights[k], &money_text(spec.bank, shown, col).replacen('.', ",", 1));
            } else if Some(i) == spec.drift_row {
                // amount printed ~30pt left of its column: not in any locked column band
                sh.right(rights[k] - 34.0, &money_text(spec.bank, shown, col));
            } else {
                sh.right(rights[k], &money_text(spec.bank, shown, col));
            }
            // the garbled row prints no balance either, so the balances can't fill it in: it's left for the inspector
            if let Some(bk) = st.money.iter().position(|m| m.1 == Col::Balance)
                && !(st.grouped && same_day_after)
                && Some(i) != spec.garble_row
            {
                sh.right(rights[bk], &money_text(spec.bank, bal, Col::Balance));
            }
            sh.nl(LINE);
            for m in t.memo.iter().filter(|_| !st.amount_last) {
                sh.y -= 2.0;
                sh.put(desc_x, m);
                sh.nl(LINE);
            }
        }
        // CNB prints no closing row in the table (the summary box has it)
        if let Some(bk) = st.money.iter().position(|m| m.1 == Col::Balance).filter(|_| !st.amount_last) {
            sh.put(desc_x, "Closing Balance");
            sh.right(rights[bk], &money_text(spec.bank, spec.closing(), Col::Balance));
            sh.nl(LINE);
        }
        sh.nl(LINE);
        sh.put(40.0, "Continued on next page");
        sh.nl(LINE);
    }
    sh.tokens
}

// ---------------------------------------------------------------- a year of demo data

/// Deterministic xorshift, so the demo is identical on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
    fn cents(&mut self, lo: i64, hi: i64) -> Decimal {
        Decimal::new(self.range(lo * 100, hi * 100), 2)
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
}

pub struct DemoDoc {
    pub filename: String,
    pub workspace: &'static str,
    pub tokens: Vec<Token>,
    pub specs: Vec<StatementSpec>,
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day.min(28)).unwrap_or_default()
}

fn dec(s: &str) -> Decimal {
    s.parse().unwrap_or_default()
}

fn month_end(y: i32, m: u32) -> NaiveDate {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).unwrap_or_default() - Duration::days(1)
}

fn in_range(x: NaiveDate, p: (NaiveDate, NaiveDate)) -> bool {
    x >= p.0 && x <= p.1
}

/// Twelve months (Nov 2025 – Oct 2026) of statements: Personal across Scotiabank, Butterfield, RBC
/// and CICSA; a property business at Cayman National (KYD and USD).
pub fn demo_documents() -> Vec<DemoDoc> {
    let mut rng = Rng(0x5eed_f1a6_2026);
    let months: Vec<(i32, u32)> = (0..12).map(|i| if i < 2 { (2025, 11 + i) } else { (2026, i - 1) }).collect();
    let mut docs = Vec::new();

    let mut sb_bal = dec("4210.55");
    let mut bf_owed = dec("380.20");
    let mut rbc_bal = dec("2500.00");
    let mut shares = dec("6200.00");
    let mut loan = dec("18450.00");
    let mut cnb_kyd = dec("12840.00");
    let mut cnb_usd = dec("3100.00");

    for (mi, &(y, m)) in months.iter().enumerate() {
        let last = mi == months.len() - 1;
        let cal = (d(y, m, 1), if last { d(y, m, 26) } else { month_end(y, m) });
        // Scotiabank runs 16th → 15th, which puts a December/January rollover in the year
        let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        let scot = (NaiveDate::from_ymd_opt(py, pm, 16).unwrap_or_default(), d(y, m, 15));

        // the card is paid in full each month from Scotiabank, converted at the peg
        let pay_usd = bf_owed.max(Decimal::ZERO);
        let pay_kyd = crate::money::round2(pay_usd / dec("1.2"));

        // ---------------- Scotiabank chequing (KYD)
        let mut t = Vec::new();
        t.push(DemoTx::new(scot.0 + Duration::days(0), "ELECTRONIC CREDIT", dec("2480.15")).memo("HARBOUR FREIGHT DIR PAY CR SALARY"));
        let cuc_base = rng.cents(175, 235);
        let cuc = if last { (cuc_base * dec("1.34")).round_dp(2) } else { cuc_base };
        t.push(DemoTx::new(scot.0 + Duration::days(3), "CUC WEB PMT", -cuc).memo("CARIBBEAN UTILITIES CO"));
        t.push(DemoTx::new(scot.0 + Duration::days(4), "WATER AUTHORITY WEB PMT", -rng.cents(38, 55)));
        for w in 0..4 {
            let day = scot.0 + Duration::days(2 + w * 7 + rng.range(0, 2));
            let store = rng.pick(&["FOSTER'S FOOD FAIR #12 CAMANA BAY", "FOSTER'S FOOD FAIR #03 AIRPORT", "KIRK MARKET GEORGE TOWN", "HURLEY'S MARKETPLACE"]);
            t.push(DemoTx::new(day, "POS PURCHASE", -rng.cents(45, 190)).memo(store));
        }
        t.push(DemoTx::new(scot.0 + Duration::days(9), "POS PURCHASE", -rng.cents(40, 75)).memo("RUBIS RED BAY 0045"));
        if mi % 3 == 1 {
            t.push(DemoTx::new(scot.0 + Duration::days(11), "POS PURCHASE", -rng.cents(60, 420)).memo("KIRK HOME CENTRE 0021"));
        }
        t.push(DemoTx::new(scot.0 + Duration::days(12), "ABM WITHDRAWAL", -dec("100.00")).memo("*BNS CAMANA BAY"));
        t.push(DemoTx::new(scot.0 + Duration::days(14), "ELECTRONIC CREDIT", dec("2480.15")).memo("HARBOUR FREIGHT DIR PAY CR SALARY"));
        t.push(DemoTx::new(d(y, m, 1), "PC TRANSFER DEBIT", -pay_kyd).memo("TRANSFER TO BUTTERFIELD VISA 7710"));
        t.push(DemoTx::new(scot.0 + Duration::days(16), "PC TRANSFER DEBIT", -dec("500.00")).memo("TRANSFER TO RBC SAVINGS 3390"));
        t.push(DemoTx::new(scot.0 + Duration::days(17), "MOBILE OB TRF DR", -dec("450.00")).memo("TRF TO: CICSA CREDIT UNION LOAN 1003"));
        t.push(DemoTx::new(scot.0 + Duration::days(17), "SPECIAL SERVICE CHARGE", -dec("1.00")).memo("Third Party TRF Fee"));
        t.push(DemoTx::new(scot.0 + Duration::days(20), "POS PURCHASE", -rng.cents(85, 140)).memo("FLOW CAYMAN BILL PAY"));
        if mi == 7 {
            // a double charge two days apart
            t.push(DemoTx::new(scot.0 + Duration::days(21), "POS PURCHASE", -dec("64.20")).memo("CAYMAN CABS LTD"));
            t.push(DemoTx::new(scot.0 + Duration::days(22), "POS PURCHASE", -dec("64.20")).memo("CAYMAN CABS LTD"));
        }
        t.push(DemoTx::new(scot.1, "SERVICE CHARGE", -dec("13.00")));
        t.push(DemoTx::new(scot.1, "STAMP DUTY TAX", -dec("3.50")));
        t.sort_by_key(|x| x.date);
        let spec = StatementSpec::new(Bank::Scotiabank, AccountType::Checking, Currency::KYD, "2045", sb_bal, scot, t);
        sb_bal = spec.closing();
        docs.push(DemoDoc { filename: format!("Scotiabank_Chequing_2045_{}.pdf", scot.1), workspace: "Personal", tokens: render(std::slice::from_ref(&spec)), specs: vec![spec] });

        // ---------------- Butterfield Visa (USD)
        let mut t = Vec::new();
        let hbo = if (y, m) >= (2026, 10) { dec("17.99") } else { dec("15.99") };
        t.push(DemoTx::new(d(y, m, 3), "HBO MAX HBOMAX.COM 8442", -hbo));
        t.push(DemoTx::new(d(y, m, 5), "NETFLIX.COM LOS GATOS", -dec("15.49")));
        t.push(DemoTx::new(d(y, m, 8), "SPOTIFY P1A2B3C4 STOCKHOLM", -dec("11.99")));
        t.push(DemoTx::new(d(y, m, 10), "AMAZON MKTPL*2K4TY SEATTLE", -rng.cents(25, 180)));
        for _ in 0..3 {
            let place = rng.pick(&["CRACKED CONCH WEST BAY", "MICHAEL'S GENUINE CAMANA BAY", "BRASSERIE GEORGE TOWN", "TACO CUBANO SEVEN MILE"]);
            t.push(DemoTx::new(d(y, m, rng.range(6, 24) as u32), place, -rng.cents(30, 140)).memo(&format!("KYD {} @ 1.2000", fmt_plain(rng.cents(25, 110)))));
        }
        t.push(DemoTx::new(d(y, m, 2), "PAYMENT RECEIVED - THANK YOU", crate::money::round2(pay_kyd * dec("1.2"))));
        if mi == 4 {
            t.push(DemoTx::new(d(y, m, 27), "INTEREST CHARGE PURCHASES", -dec("12.84")));
        }
        t.push(DemoTx::new(d(y, m, 28), "FOREIGN TRANSACTION FEE", -rng.cents(2, 6)));
        t.sort_by_key(|x| x.date);
        let period = (d(y, m, 1), month_end(y, m));
        let mut spec = StatementSpec::new(Bank::Butterfield, AccountType::CreditCard, Currency::USD, "7710", bf_owed, period, t);
        spec.txs.retain(|x| in_range(x.date, period));
        bf_owed = spec.closing();
        docs.push(DemoDoc { filename: format!("Butterfield_Visa_7710_{y}-{m:02}.pdf"), workspace: "Personal", tokens: render(std::slice::from_ref(&spec)), specs: vec![spec] });

        // ---------------- RBC savings (KYD): grouped dates
        let mut t = vec![
            DemoTx::new(scot.0 + Duration::days(17), "TRANSFER FROM SCOTIABANK", dec("500.00")),
            DemoTx::new(scot.0 + Duration::days(17), "BRANCH DEPOSIT", rng.cents(20, 80)),
            DemoTx::new(cal.1 - Duration::days(1), "INTEREST PAID", Decimal::new(rng.range(150, 420), 2)),
            DemoTx::new(cal.1 - Duration::days(1), "MONTHLY FEE", -dec("5.00")),
        ];
        if mi % 4 == 2 {
            t.push(DemoTx::new(scot.0 + Duration::days(20), "BILL PAYMENT HOME GAS", -rng.cents(60, 110)));
        }
        t.sort_by_key(|x| x.date);
        let spec = StatementSpec::new(Bank::Rbc, AccountType::Savings, Currency::KYD, "3390", rbc_bal, cal, t);
        rbc_bal = spec.closing();
        docs.push(DemoDoc { filename: format!("RBC_Savings_3390_{y}-{m:02}.pdf"), workspace: "Personal", tokens: render(std::slice::from_ref(&spec)), specs: vec![spec] });

        // ---------------- CICSA credit union: shares + loan in one statement
        let mut st = vec![DemoTx::new(d(y, m, 15), "PAYROLL DEDUCTION SHARES", dec("300.00")).memo("PAYROLL SPLIT HARBOUR FREIGHT")];
        if m == 12 {
            st.push(DemoTx::new(d(y, m, 20), "DIVIDEND POSTING 2025", (shares * dec("0.045")).round_dp(2)));
        }
        let interest = (loan * dec("0.0075")).round_dp(2);
        let lt = vec![DemoTx::new(scot.0 + Duration::days(18), "LOAN PAYMENT - THANK YOU", dec("450.00")), DemoTx::new(cal.1 - Duration::days(2), "LOAN INTEREST", -interest)];
        let s1 = StatementSpec::new(Bank::Cicsa, AccountType::ShareAccount, Currency::KYD, "1002", shares, cal, st);
        let s2 = StatementSpec::new(Bank::Cicsa, AccountType::Loan, Currency::KYD, "1003", loan, cal, lt);
        shares = s1.closing();
        loan = s2.closing();
        docs.push(DemoDoc { filename: format!("CICSA_Statement_{y}-{m:02}.pdf"), workspace: "Personal", tokens: render(&[s1.clone(), s2.clone()]), specs: vec![s1, s2] });

        // ---------------- Cayman National business current account (KYD)
        let mut t = vec![
            DemoTx::new(d(y, m, 1), "RENT RECEIVED UNIT A SMITH", dec("2400.00")),
            DemoTx::new(d(y, m, 2), "RENT RECEIVED UNIT B JONES", dec("2150.00")),
            DemoTx::new(d(y, m, 6), "CUC WEB PMT PROPERTY", -rng.cents(140, 210)),
            DemoTx::new(d(y, m, 9), "A.L. THOMPSON'S GEORGE TOWN", -rng.cents(40, 380)),
            DemoTx::new(d(y, m, 12), "GENERAL LANDSCAPING LTD", -dec("275.00")),
            DemoTx::new(d(y, m, 20), &format!("CHEQUE {:06}", 1200 + mi), -dec("650.00")),
            DemoTx::new(d(y, m, 28), "MONTHLY ACCOUNT FEE", -dec("15.00")),
        ];
        if m == 3 {
            t.push(DemoTx::new(d(y, m, 15), "BRITISH CAYMANIAN INSURANCE", -dec("1840.00")));
        }
        if mi % 2 == 0 {
            t.push(DemoTx::new(d(y, m, 16), "TRANSFER FROM USD ACCT 6121", dec("1000.00")));
        }
        t.sort_by_key(|x| x.date);
        let mut spec = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::KYD, "6120", cnb_kyd, cal, t);
        if mi == 9 {
            spec.split_row = Some(3); // page-break split row → Pass 5 recovers it
        }
        cnb_kyd = spec.closing();
        docs.push(DemoDoc { filename: format!("CNB_Business_KYD_6120_{y}-{m:02}.pdf"), workspace: "Business", tokens: render(std::slice::from_ref(&spec)), specs: vec![spec] });

        // ---------------- Cayman National USD account
        let mut t = vec![DemoTx::new(d(y, m, 4), "INCOMING WIRE BLUE WATER CHARTERS", rng.cents(900, 1600)), DemoTx::new(d(y, m, 4), "INCOMING WIRE FEE", -dec("15.00"))];
        if mi % 2 == 0 {
            t.push(DemoTx::new(d(y, m, 16), "TRANSFER TO KYD ACCT 6120", -dec("1200.00")));
        }
        t.push(DemoTx::new(d(y, m, 22), "ADOBE CREATIVE CLOUD", -dec("59.99")));
        t.sort_by_key(|x| x.date);
        let mut spec = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::USD, "6121", cnb_usd, cal, t);
        if mi == 10 {
            spec.garble_row = Some(0); // left for you to fix in the Statement Inspector
        }
        cnb_usd = spec.closing();
        docs.push(DemoDoc { filename: format!("CNB_Business_USD_6121_{y}-{m:02}.pdf"), workspace: "Business", tokens: render(std::slice::from_ref(&spec)), specs: vec![spec] });
    }
    docs
}
