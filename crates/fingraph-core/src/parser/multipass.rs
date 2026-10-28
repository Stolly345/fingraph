//! Passes 1–3: from positioned words to candidate transactions.

use chrono::{Datelike, NaiveDate};
use regex::Regex;
use rust_decimal::Decimal;
use std::path::Path;
use std::sync::LazyLock;

use super::dates::{self, DateCtx, Order};
use super::profiles::{self, BankProfile, LayoutKind};
use super::{reconcile, ColumnBand, ColumnRole, Diagnostics, PageGeometry, ScanResult, ScannedTx, Section, SectionReport, SkippedLine, Token};
use crate::money::{self, Printed};
use crate::{AccountType, Currency, Error, Reconciliation, Result};

pub struct MultiPassScanner<'a> {
    pub profiles: &'a [BankProfile],
    /// Year for year-less dates when no statement period can be found.
    pub fallback_year: i32,
}

impl<'a> MultiPassScanner<'a> {
    pub fn new(profiles: &'a [BankProfile]) -> Self {
        MultiPassScanner { profiles, fallback_year: chrono::Local::now().year() }
    }

    pub fn scan_pdf(&self, path: &Path) -> Result<ScanResult> {
        let tokens = super::tokens_from_pdf(path)?;
        if !tokens.is_empty() {
            return self.scan_tokens(tokens);
        }
        let mut r = self.scan_tokens(super::ocr_tokens(path)?)?;
        r.diagnostics.pass1.ocr = true;
        // never fully trusted: OCR can misread a name even when every balance checks out
        for s in &mut r.sections {
            s.confidence = s.confidence.min(0.9);
        }
        Ok(r)
    }

    pub fn scan_tokens(&self, tokens: Vec<Token>) -> Result<ScanResult> {
        if tokens.is_empty() {
            return Err(Error("no text found in the statement".into()));
        }
        let mut diag = Diagnostics::default();
        let rows = build_rows(tokens);
        let meta = pass1(self, &rows, &mut diag);
        let layout = pass2(&meta, &rows, &mut diag);
        let mut sections = pass3(&meta, &rows, &layout, &mut diag);
        for s in &mut sections {
            reconcile::verify_and_heal(s);
        }
        Ok(ScanResult { profile_slug: meta.profile.map(|p| p.slug.clone()), period: meta.ctx.period, sections, diagnostics: diag })
    }
}

// ---------------------------------------------------------------- rows & cells

#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub page: u32,
    pub y0: f64,
    pub y1: f64,
    pub words: Vec<Token>,
}

impl Row {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
    pub fn yc(&self) -> f64 {
        (self.y0 + self.y1) / 2.0
    }
    pub fn h(&self) -> f64 {
        self.words.iter().map(Token::h).fold(0.0, f64::max).max(4.0)
    }
    /// Indexes of words that start a cell (a gap wider than ~2/3 of the text height before them).
    fn cell_starts(&self) -> Vec<usize> {
        let gap = 0.6 * self.h();
        (0..self.words.len()).filter(|&i| i == 0 || self.words[i].x0 - self.words[i - 1].x1 > gap).collect()
    }
    fn cells(&self) -> Vec<(f64, f64, String)> {
        let starts = self.cell_starts();
        starts
            .iter()
            .enumerate()
            .map(|(k, &s)| {
                let e = starts.get(k + 1).copied().unwrap_or(self.words.len());
                let ws = &self.words[s..e];
                (ws[0].x0, ws[ws.len() - 1].x1, ws.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "))
            })
            .collect()
    }
}

/// Words → rows: same page, vertical centres within half a line of each other.
pub(crate) fn build_rows(mut tokens: Vec<Token>) -> Vec<Row> {
    // rotated runs (account numbers printed up the margin) are taller than wide: not part of any row
    tokens.retain(|t| t.text.chars().count() < 3 || t.y1 - t.y0 < 1.5 * (t.x1 - t.x0));
    tokens.sort_by(|a, b| (a.page, a.yc()).partial_cmp(&(b.page, b.yc())).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<Row> = Vec::new();
    for t in tokens {
        if let Some(r) = rows.last_mut()
            && r.page == t.page
            && (t.yc() - r.yc()).abs() <= 0.45 * t.h().min(r.h())
        {
            r.y0 = r.y0.min(t.y0);
            r.y1 = r.y1.max(t.y1);
            r.words.push(t);
            continue;
        }
        rows.push(Row { page: t.page, y0: t.y0, y1: t.y1, words: vec![t] });
    }
    for r in &mut rows {
        r.words.sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows
}

/// An amount found in a row: its words `lo..hi`, the core word, what it said.
#[derive(Debug, Clone, Copy)]
struct MoneyItem {
    lo: usize,
    hi: usize,
    x0: f64,
    x1: f64,
    printed: Printed,
}

fn money_items(words: &[Token]) -> Vec<MoneyItem> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if !money::is_amount_core(&words[i].text) {
            i += 1;
            continue;
        }
        let near = |a: &Token, b: &Token| b.x0 - a.x1 < 1.2 * a.h();
        let mut lo = i;
        let floor = out.last().map_or(0, |m: &MoneyItem| m.hi);
        while lo > floor && money::is_amount_affix(&words[lo - 1].text) && near(&words[lo - 1], &words[lo]) {
            lo -= 1;
        }
        let mut hi = i + 1;
        while hi < words.len()
            && money::is_amount_affix(&words[hi].text)
            && Currency::parse(&words[hi].text).is_none()
            && near(&words[hi - 1], &words[hi])
            && !money::is_amount_core(&words[hi].text)
        {
            hi += 1;
        }
        let text = words[lo..hi].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        if let Some(printed) = money::parse_printed(&text) {
            out.push(MoneyItem { lo, hi, x0: words[lo].x0, x1: words[hi - 1].x1, printed });
        }
        i = hi;
    }
    out
}

// ---------------------------------------------------------------- pass 1

pub(crate) struct Meta<'p> {
    pub profile: Option<&'p BankProfile>,
    pub ctx: DateCtx,
    pub account_last4: Option<String>,
    pub account_type: AccountType,
    pub currency: Currency,
    pub currency_columns: Vec<(Currency, f64)>,
    pub noise: Vec<Regex>,
    pub fx: Option<Regex>,
    pub layout: LayoutKind,
}

static LAST4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:[*xX•]{3,}[\s-]*(\d{4})\b|account\s*(?:number|no\.?|#)\s*:?\s*[\d\s*xX-]*?(\d{4})\b)").unwrap()
});

pub(crate) static OPENING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:opening|previous|beginning|starting)\s+balance|balance\s+(?:brought\s+)?forward|brought\s+forward)").unwrap());
pub(crate) static CLOSING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:closing|new|ending)\s+balance|balance\s+carried\s+forward|carried\s+forward)").unwrap());
static DEBITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:total\s+)?(?:withdrawals|debits|minus)\b|^purchases\b|^total\s+purchases|^cheques\s*(?:&|and)\s*other\s+deb").unwrap());
static CREDITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:total\s+)?(?:deposits|credits|plus)\b|^payments\s*(?:&|and)\s*credits").unwrap());
/// Words that may precede a summary amount on the line under its label (CNB: `as of 01/03/26   1,000.00`).
static AS_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:as|of|\d{1,2}/\d{1,2}/\d{2,4})$").unwrap());
static GENERIC_NOISE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^\s*page\s+\d+\s*(?:of\s+\d+)?\s*$",
        r"(?i)continued\s+(?:on|from)\s+(?:the\s+)?(?:next|previous|reverse)\s+page",
        r"(?i)^\s*\(?continued\)?\s*$",
        r"(?i)^\s*\*?\s*trademarks?\s+of\b",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

pub(crate) fn summary_value(rows: &[&Row], label: &Regex, cols: &[(Currency, f64)], want: Currency) -> Option<(Decimal, String)> {
    for (ri, r) in rows.iter().enumerate() {
        for s in r.cell_starts() {
            let rest = r.words[s..].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            if !label.is_match(&rest) {
                continue;
            }
            // the amount must open the next cell ("Purchases & Debits    US$ 15.99"), so a count like
            // "No. of Deposits   3" never lends its neighbour's amount
            // a typewriter font spaces the label out ("Deposits  &  Other Cred"): skip its words, stop at the first number
            let starts = r.cell_starts();
            let k = starts.iter().copied().find(|&c| c > s && r.words[c].text.chars().any(|ch| ch.is_ascii_digit() || ch == '$')).unwrap_or(r.words.len());
            let after: Vec<Token> = r.words[k..].to_vec();
            // `as_of`: the line under the label may start `as of 01/02/26` (never the line above: that's another label's)
            let first_money = |ws: &[Token], as_of: bool| -> Option<Vec<MoneyItem>> {
                let items = money_items(ws);
                let lead = ws.iter().take_while(|w| Currency::parse(&w.text).is_some() || w.text == "$" || (as_of && AS_OF.is_match(&w.text))).count();
                (items.first()?.lo <= lead).then_some(items)
            };
            let near = |o: Option<&&Row>, as_of: bool| o.filter(|n| n.page == r.page && (n.yc() - r.yc()).abs() < 2.6 * r.h()).and_then(|n| first_money(&n.words, as_of));
            let Some(items) = first_money(&after, false).or_else(|| near(rows.get(ri + 1), true)).or_else(|| near(ri.checked_sub(1).and_then(|i| rows.get(i)), false)) else {
                continue;
            };
            let pick = if cols.len() > 1 {
                let target = cols.iter().find(|(c, _)| *c == want)?.1;
                items.iter().min_by(|a, b| ((a.x0 + a.x1) / 2.0 - target).abs().total_cmp(&((b.x0 + b.x1) / 2.0 - target).abs()))?
            } else {
                &items[0]
            };
            let p = pick.printed;
            let v = if p.negative == Some(true) { -p.magnitude } else { p.magnitude };
            return Some((v, r.text()));
        }
    }
    None
}

fn detect_currency(page1: &str) -> Currency {
    let no_parens = Regex::new(r"\([^)]*\)").unwrap().replace_all(page1, "");
    let up = no_parens.to_ascii_uppercase();
    let kyd = up.matches("KYD").count() + up.matches("CI$").count() * 2;
    let usd = up.matches("USD").count() + up.matches("US$").count() * 2;
    if usd > kyd { Currency::USD } else { Currency::KYD }
}

/// CNB prints the account kind as a code: `ACCOUNT #: SV - 1234567`.
static SAVINGS_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"account\s*#\s*:?\s*sv\b").unwrap());

fn detect_type(page1: &str) -> AccountType {
    let t = page1.to_lowercase();
    if ["credit limit", "minimum payment", "credit card", "card number", "mastercard", "visa", "american express"].iter().any(|k| t.contains(k)) {
        AccountType::CreditCard
    } else if t.contains("loan account") || t.contains("loan balance") || t.contains("principal") {
        AccountType::Loan
    } else if t.contains("share savings") || t.contains("share account") {
        AccountType::ShareAccount
    } else if t.contains("savings") || SAVINGS_CODE.is_match(&t) {
        AccountType::Savings
    } else {
        AccountType::Checking
    }
}

fn pass1<'p>(scanner: &MultiPassScanner<'p>, rows: &[Row], diag: &mut Diagnostics) -> Meta<'p> {
    let tokens: Vec<Token> = rows.iter().flat_map(|r| r.words.iter().cloned()).collect();
    let (profile, scores) = profiles::fingerprint(scanner.profiles, &tokens);
    let order = profile.map_or(Order::DayFirst, BankProfile::date_order);

    // the period: a row that says "period"/"from", or the row right under it, or any row with two dated ends
    let mut period = None;
    for (i, r) in rows.iter().enumerate() {
        let t = r.text();
        let lower = t.to_lowercase();
        if lower.contains("period") || lower.contains("statement from") || lower.contains("statement date") {
            let next = rows.get(i + 1).filter(|n| n.page == r.page).map(Row::text).unwrap_or_default();
            period = dates::parse_period(&t, order).or_else(|| dates::parse_period(&next, order)).or_else(|| dates::parse_period(&format!("{t} {next}"), order));
            if period.is_some() {
                break;
            }
        }
    }
    if period.is_none() {
        period = rows.iter().find_map(|r| dates::parse_period(&r.text(), order));
    }

    let page1: String = rows.iter().filter(|r| r.page == 1).map(Row::text).collect::<Vec<_>>().join("\n");
    // the letterhead: page 1 up to the first row that looks like a transaction (a date and an amount)
    let ctx0 = DateCtx { order, period, fallback_year: scanner.fallback_year };
    let head: String = rows
        .iter()
        .filter(|r| r.page == 1)
        .take_while(|r| dates::parse_first(&r.text(), &ctx0).is_none() || money_items(&r.words).is_empty() || dates::parse_period(&r.text(), order).is_some())
        .map(Row::text)
        .collect::<Vec<_>>()
        .join("\n");
    let account_last4 = LAST4.captures(&page1).and_then(|c| c.get(1).or(c.get(2))).map(|m| m.as_str().to_string());
    let mut account_type = detect_type(&head);
    let layout = profile.map_or(LayoutKind::DebitCreditWithBalance, |p| p.column_layout_kind);
    if layout == LayoutKind::CreditCardDRCR {
        account_type = AccountType::CreditCard;
    }
    let currency = detect_currency(&page1);
    let currency_columns: Vec<(Currency, f64)> = rows
        .iter()
        .find_map(|r| {
            let cols: Vec<(Currency, f64)> = r
                .cells()
                .iter()
                .filter(|(_, _, t)| t == "KYD" || t == "USD")
                .filter_map(|(a, b, t)| Some((Currency::parse(t)?, (a + b) / 2.0)))
                .collect();
            (cols.len() == 2 && cols[0].0 != cols[1].0).then_some(cols)
        })
        .unwrap_or_default();

    let mut noise: Vec<Regex> = GENERIC_NOISE.clone();
    if let Some(p) = profile {
        noise.extend(p.noise_patterns.iter().filter_map(|n| Regex::new(n).ok()));
    }
    let fx = profile.and_then(|p| Regex::new(&p.fx_note_regex).ok());

    diag.pass1.letterhead = rows.iter().filter(|r| r.page == 1).take(6).map(Row::text).collect();
    diag.pass1.profile = profile.map(|p| p.slug.clone());
    diag.pass1.scores = scores;
    diag.pass1.period = period;
    diag.pass1.account_last4 = account_last4.clone();
    diag.pass1.account_type = Some(account_type);
    diag.pass1.currency = Some(currency);
    diag.pass1.currency_columns = currency_columns.iter().map(|c| c.0).collect();

    Meta {
        profile,
        ctx: DateCtx { order, period, fallback_year: period.map_or(scanner.fallback_year, |(_, e)| e.year()) },
        account_last4,
        account_type,
        currency,
        currency_columns,
        noise,
        fx,
        layout,
    }
}

// ---------------------------------------------------------------- pass 2

#[derive(Debug, Clone)]
pub(crate) struct Geometry {
    /// Text columns by centre: (role, lo, hi).
    text: Vec<(ColumnRole, f64, f64)>,
    /// Money columns by right edge: (role, lo, hi).
    money: Vec<(ColumnRole, f64, f64)>,
    /// Everything at or right of this is the amounts zone.
    money_lo: f64,
    left: f64,
    right: f64,
}

impl Geometry {
    fn text_role(&self, x: f64) -> Option<ColumnRole> {
        if x < self.left || x > self.right {
            return None;
        }
        if x >= self.money_lo {
            return Some(ColumnRole::Description);
        }
        self.text.iter().find(|(_, lo, hi)| x >= *lo && x < *hi).map(|c| c.0)
    }
    fn money_role(&self, right_edge: f64) -> Option<ColumnRole> {
        if right_edge < self.money_lo || right_edge > self.right {
            return None;
        }
        self.money.iter().find(|(_, lo, hi)| right_edge > *lo && right_edge <= *hi).map(|c| c.0)
    }
    fn bands(&self) -> Vec<ColumnBand> {
        self.text.iter().chain(&self.money).map(|&(role, x0, x1)| ColumnBand { role, x0: x0.max(self.left), x1: x1.min(self.right) }).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RowKind {
    Noise,
    Header,
    Outside,
    Table(usize),
}

pub(crate) struct Layout {
    pub kinds: Vec<RowKind>,
    pub geos: Vec<Geometry>,
}

fn classify_header(text: &str, profile: Option<&BankProfile>) -> Option<ColumnRole> {
    let t = text.to_lowercase();
    if let Some(p) = profile {
        let a = &p.header_anchors;
        for (list, role) in [
            (&a.posting_date, ColumnRole::PostDate),
            (&a.date, ColumnRole::Date),
            (&a.amount, ColumnRole::Amount),
            (&a.debit, ColumnRole::Debit),
            (&a.credit, ColumnRole::Credit),
            (&a.balance, ColumnRole::Balance),
            (&a.description, ColumnRole::Description),
            (&a.reference, ColumnRole::Reference),
        ] {
            if list.iter().any(|k| t == k.to_lowercase()) {
                return Some(role);
            }
        }
    }
    let has = |ks: &[&str]| ks.iter().any(|k| t.contains(k));
    if t.len() > 40 {
        return None;
    }
    if has(&["balance"]) {
        Some(ColumnRole::Balance)
    } else if has(&["posting date", "post date", "posted"]) {
        Some(ColumnRole::PostDate)
    } else if t == "date" || has(&["date"]) {
        Some(ColumnRole::Date)
    } else if has(&["debit/credit", "amount"]) {
        Some(ColumnRole::Amount)
    } else if has(&["withdrawal", "debit", "minus", "money out", "paid out"]) {
        Some(ColumnRole::Debit)
    } else if has(&["deposit", "credit", "plus", "money in", "paid in"]) {
        Some(ColumnRole::Credit)
    } else if has(&["description", "particulars", "details", "narrative", "payee", "transaction/reference"]) {
        Some(ColumnRole::Description)
    } else if has(&["reference", "ref no", "ref.", "cheque no", "check no"]) {
        Some(ColumnRole::Reference)
    } else {
        None
    }
}

/// A table header starting at `rows[i]` (possibly spanning the next two rows): its columns and last row.
type Columns = Vec<(ColumnRole, f64, f64)>;
/// Columns with the header words that named them.
type NamedColumns = Vec<(ColumnRole, f64, f64, String)>;

fn detect_header(rows: &[Row], i: usize, kinds: &[RowKind], profile: Option<&BankProfile>) -> Option<(NamedColumns, usize)> {
    let r = &rows[i];
    let first = r.cells();
    if first.iter().filter(|c| classify_header(&c.2, profile).is_some()).count() < 2 || !money_items(&r.words).is_empty() {
        return None;
    }
    let mut cells: Vec<(f64, f64, String)> = first;
    let mut last = i;
    for j in i + 1..(i + 5).min(rows.len()) {
        let n = &rows[j];
        if n.page != r.page || kinds[j] == RowKind::Noise || n.yc() - rows[last].yc() > 2.6 * r.h() {
            break;
        }
        let nc = n.cells();
        // margin furniture (vertical lettering, sort codes) between header lines
        if nc.iter().all(|c| c.2.len() <= 3 && classify_header(&c.2, profile).is_none()) {
            continue;
        }
        if !money_items(&n.words).is_empty() || nc.iter().any(|c| classify_header(&c.2, profile).is_none() && c.2.len() > 2) {
            break;
        }
        // merge into an overlapping role-less cell above ("Transaction" + "Date"), else add as a sub-column
        for c in nc {
            let role = classify_header(&c.2, profile);
            match cells.iter_mut().find(|u| u.0 < c.1 + 2.0 && c.0 < u.1 + 2.0 && (classify_header(&u.2, profile).is_none() || role.is_none())) {
                Some(u) => {
                    u.0 = u.0.min(c.0);
                    u.1 = u.1.max(c.1);
                    u.2 = format!("{} {}", u.2, c.2);
                }
                None => cells.push(c),
            }
        }
        last = j;
    }
    let mut cols: NamedColumns = cells.iter().filter_map(|(a, b, t)| Some((classify_header(t, profile)?, *a, *b, t.clone()))).collect();
    let roles: Vec<ColumnRole> = cols.iter().map(|c| c.0).collect();
    if roles.contains(&ColumnRole::Debit) && roles.contains(&ColumnRole::Credit) {
        cols.retain(|c| c.0 != ColumnRole::Amount);
    }
    // two "date" columns: the second one is the posting date
    if !roles.contains(&ColumnRole::PostDate) && roles.iter().filter(|r| **r == ColumnRole::Date).count() == 2 {
        if let Some(c) = cols.iter_mut().filter(|c| c.0 == ColumnRole::Date).nth(1) {
            c.0 = ColumnRole::PostDate;
        }
    }
    let roles: Vec<ColumnRole> = cols.iter().map(|c| c.0).collect();
    let has_date = roles.contains(&ColumnRole::Date) || roles.contains(&ColumnRole::PostDate);
    let has_money = roles.iter().any(|r| matches!(r, ColumnRole::Debit | ColumnRole::Credit | ColumnRole::Amount));
    (has_date && has_money && roles.contains(&ColumnRole::Description)).then(|| {
        cols.sort_by(|a, b| a.1.total_cmp(&b.1));
        (cols, last)
    })
}

/// A detected header's columns without their words.
fn bare(cols: &[(ColumnRole, f64, f64, String)]) -> Columns {
    cols.iter().map(|c| (c.0, c.1, c.2)).collect()
}

fn initial_geometry(cols: &[(ColumnRole, f64, f64)]) -> Geometry {
    let text: Vec<_> = cols.iter().filter(|c| !c.0.is_money()).collect();
    let money: Vec<_> = cols.iter().filter(|c| c.0.is_money()).collect();
    let centre = |c: &(ColumnRole, f64, f64)| (c.1 + c.2) / 2.0;
    let desc_centre = text.iter().find(|c| c.0 == ColumnRole::Description).map_or(0.0, |c| centre(c));
    let money_lo = money.first().map_or(f64::MAX, |m| (desc_centre + centre(m)) / 2.0).max(text.last().map_or(0.0, |t| centre(t) + 1.0));
    let mut tg = Vec::new();
    for (k, c) in text.iter().enumerate() {
        let lo = if k == 0 { f64::MIN } else { (centre(text[k - 1]) + centre(c)) / 2.0 };
        let hi = text.get(k + 1).map_or(money_lo, |n| (centre(c) + centre(n)) / 2.0);
        tg.push((c.0, lo, hi));
    }
    let mut mg = Vec::new();
    for (k, c) in money.iter().enumerate() {
        let lo = if k == 0 { money_lo } else { (money[k - 1].2 + c.2) / 2.0 };
        let hi = money.get(k + 1).map_or(f64::MAX, |n| (c.2 + n.2) / 2.0);
        mg.push((c.0, lo, hi));
    }
    let left = cols.first().map_or(0.0, |c| c.1) - 18.0;
    let right = cols.last().map_or(0.0, |c| c.2) + 45.0;
    Geometry { text: tg, money: mg, money_lo, left, right }
}

/// Lock the money columns to where amounts actually sit. Amounts are right-aligned, so each column's
/// right edges pile up on one x; the band becomes that x ± a tolerance, and a number whose right edge
/// is nowhere near it (an FX note like `(154.88 KYD)` inside a description) stays description text.
fn refine(geo: &mut Geometry, rows: &[&Row]) {
    let mut rights: Vec<Vec<f64>> = vec![Vec::new(); geo.money.len()];
    for r in rows {
        for it in money_items(&r.words) {
            if let Some(k) = geo.money.iter().position(|(_, lo, hi)| it.x1 > *lo && it.x1 <= *hi)
                && it.x1 >= geo.money_lo
                && it.x1 <= geo.right
            {
                rights[k].push(it.x1);
            }
        }
    }
    let medians: Vec<Option<f64>> = rights
        .iter_mut()
        .map(|v| {
            v.sort_by(f64::total_cmp);
            (!v.is_empty()).then(|| v[v.len() / 2])
        })
        .collect();
    if medians.iter().any(Option::is_none) {
        return refine_extents(geo, rows);
    }
    let m: Vec<f64> = medians.into_iter().flatten().collect();
    let n = m.len();
    for k in 0..n {
        let left = if k == 0 { 30.0 } else { (m[k] - m[k - 1]) / 2.0 }.min(30.0);
        let right = if k + 1 == n { 30.0 } else { (m[k + 1] - m[k]) / 2.0 }.min(30.0);
        geo.money[k].1 = m[k] - left;
        geo.money[k].2 = m[k] + right;
    }
    let lo = geo.money[0].1;
    let floor = geo.text.last().map_or(f64::MIN, |t| t.1 + 1.0);
    geo.money_lo = lo.max(floor);
    if let Some(t) = geo.text.last_mut() {
        t.2 = geo.money_lo;
    }
    geo.right = geo.money[n - 1].2;
}

/// Fallback when a column never shows an amount: boundaries half-way between neighbours' extremes.
fn refine_extents(geo: &mut Geometry, rows: &[&Row]) {
    let mut edges: Vec<(f64, f64)> = vec![(f64::MAX, f64::MIN); geo.money.len()];
    let mut seen = vec![0usize; geo.money.len()];
    for r in rows {
        for it in money_items(&r.words) {
            if let Some(k) = geo.money.iter().position(|(_, lo, hi)| it.x1 > *lo && it.x1 <= *hi)
                && it.x1 >= geo.money_lo
                && it.x1 <= geo.right
            {
                edges[k].0 = edges[k].0.min(it.x0);
                edges[k].1 = edges[k].1.max(it.x1);
                seen[k] += 1;
            }
        }
    }
    for k in 1..geo.money.len() {
        if seen[k - 1] > 0 && seen[k] > 0 && edges[k - 1].1 < edges[k].1 {
            // boundary between the right edges, but never left of where column k's amounts end minus their width
            let b = ((edges[k - 1].1 + edges[k].1) / 2.0).max(edges[k - 1].1 + 0.5);
            geo.money[k - 1].2 = b;
            geo.money[k].1 = b;
        }
    }
    if seen.first().copied().unwrap_or(0) > 0 {
        let first_x0 = edges[0].0 - 3.0;
        let floor = geo.text.last().map_or(f64::MIN, |t| t.1 + 1.0);
        if first_x0 > floor {
            geo.money_lo = first_x0.min(geo.money_lo.max(first_x0));
            geo.money[0].1 = geo.money_lo;
            if let Some(t) = geo.text.last_mut() {
                t.2 = geo.money_lo;
            }
        }
    }
    if let Some(&(_, hi)) = edges.iter().zip(&seen).rev().find(|(_, s)| **s > 0).map(|(e, _)| e) {
        geo.right = geo.right.max(hi + 6.0);
    }
}

fn pass2(meta: &Meta, rows: &[Row], diag: &mut Diagnostics) -> Layout {
    let mut kinds: Vec<RowKind> = rows.iter().map(|r| if meta.noise.iter().any(|n| n.is_match(&r.text())) { RowKind::Noise } else { RowKind::Outside }).collect();
    let mut geos: Vec<Geometry> = Vec::new();
    let mut current: Option<usize> = None;
    let mut page = 0;
    let mut pages: Vec<PageGeometry> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let r = &rows[i];
        if r.page != page {
            page = r.page;
            pages.push(PageGeometry { page, inherited: current.is_some(), ..Default::default() });
        }
        let pg = pages.last_mut().unwrap();
        if kinds[i] == RowKind::Noise {
            pg.noise_stripped.push(r.text());
            i += 1;
            continue;
        }
        if let Some((cols, last)) = detect_header(rows, i, &kinds, meta.profile) {
            if pg.header_words.is_empty() {
                pg.header_words = cols.iter().map(|c| (c.0, c.3.clone())).collect();
            }
            geos.push(initial_geometry(&bare(&cols)));
            current = Some(geos.len() - 1);
            for k in kinds.iter_mut().take(last + 1).skip(i) {
                *k = RowKind::Header;
            }
            pg.header_y.get_or_insert(r.y0);
            pg.inherited = false;
            i = last + 1;
            continue;
        }
        if let Some(g) = current {
            kinds[i] = RowKind::Table(g);
            pg.rows += 1;
            // the table ends with its closing balance; what follows is fee breakdowns and fine print
            if TABLE_END.is_match(&r.text()) {
                current = None;
            }
        }
        i += 1;
    }
    for (g, geo) in geos.iter_mut().enumerate() {
        let mine: Vec<&Row> = rows.iter().zip(&kinds).filter(|(_, k)| **k == RowKind::Table(g)).map(|(r, _)| r).collect();
        refine(geo, &mine);
    }
    for pg in &mut pages {
        let g = rows.iter().zip(&kinds).filter(|(r, _)| r.page == pg.page).find_map(|(_, k)| if let RowKind::Table(g) = k { Some(*g) } else { None });
        if let Some(g) = g {
            pg.columns = geos[g].bands();
        }
    }
    diag.pages = pages;
    Layout { kinds, geos }
}

// ---------------------------------------------------------------- pass 3

#[derive(Debug, Default)]
struct Parts {
    date: String,
    post: String,
    reference: String,
    desc: String,
    monies: Vec<(ColumnRole, Printed)>,
    /// Amounts that sat in no column (a drifted or misaligned print).
    loose: Vec<Printed>,
    /// Any word outside the table's columns (left/right margins).
    outside: bool,
}

fn split_row(r: &Row, g: &Geometry) -> Parts {
    let items = money_items(&r.words);
    let mut p = Parts::default();
    let mut in_money = vec![false; r.words.len()];
    for it in items.iter().rev() {
        if let Some(role) = g.money_role(it.x1)
            && !p.monies.iter().any(|(r, _)| *r == role)
        {
            p.monies.push((role, it.printed));
            in_money[it.lo..it.hi].iter_mut().for_each(|m| *m = true);
        } else {
            p.loose.push(it.printed);
        }
    }
    let push = |s: &mut String, w: &str| {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(w);
    };
    for (w, used) in r.words.iter().zip(&in_money) {
        if *used {
            continue;
        }
        match g.text_role(w.xc()) {
            Some(ColumnRole::Date) => push(&mut p.date, &w.text),
            Some(ColumnRole::PostDate) => push(&mut p.post, &w.text),
            Some(ColumnRole::Reference) => push(&mut p.reference, &w.text),
            Some(_) => push(&mut p.desc, &w.text),
            None => p.outside |= w.text.len() > 3,
        }
    }
    // a stray letter or two in the date column is margin lettering, not a date
    for s in [&mut p.date, &mut p.post] {
        if s.len() <= 3 && !s.bytes().any(|b| b.is_ascii_digit()) {
            s.clear();
        }
    }
    p
}

static GENERIC_FX: LazyLock<Regex> = LazyLock::new(|| Regex::new(profiles::GENERIC_FX).unwrap());
static DIGITS4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{4,}\b").unwrap());
static TABLE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:closing|ending)\s+balance\b|\bbalance\s+carried\s+forward\b").unwrap());
static SUMMARY_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:sub-?)?totals?\b|^(?:opening|closing|previous|new|beginning|ending)\s+balance|balance\s+(?:brought|carried)?\s*forward|^brought\s+forward|^carried\s+forward").unwrap()
});
static SECTION_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\b(?P<ccy>KYD|USD)\s+transactions?\b|^\W*(?P<kind>share\s+savings|shares?|chequing|checking|savings|loan)\s+(?:account|statement)\b)").unwrap()
});

/// Sign in statement terms: positive moves the printed balance up.
fn stmt_sign(role: ColumnRole, p: &Printed, liability: bool, layout: LayoutKind) -> (Decimal, bool) {
    let m = p.magnitude;
    match role {
        ColumnRole::Debit => (if liability { m } else { -m }, false),
        ColumnRole::Credit => (if liability { -m } else { m }, false),
        _ => {
            if let Some(cr) = p.credit_marker {
                let up = if liability { !cr } else { cr };
                (if up { m } else { -m }, false)
            } else if let Some(neg) = p.negative {
                (if neg { -m } else { m }, false)
            } else {
                // unsigned in a single amount column: a card charge, or (for deposits) ambiguous
                let flexible = !liability && layout != LayoutKind::SignedSingleAmount;
                (m, flexible || !liability)
            }
        }
    }
}

fn balance_value(p: &Printed, liability: bool) -> Decimal {
    let neg = p.negative == Some(true) || if liability { p.credit_marker == Some(true) } else { p.credit_marker == Some(false) };
    if neg { -p.magnitude } else { p.magnitude }
}

struct Building {
    section: Section,
    /// (page, y, is_anchor, index into txs or into conts)
    events: Vec<Event>,
    conts: Vec<(u32, f64, f64, String)>,
    anchor_desc_empty: Vec<bool>,
    anchor_y: Vec<(u32, f64, f64)>,
    refs: Vec<String>,
}

enum Event {
    Anchor(usize),
    Cont(usize),
}

fn new_section(currency: Currency, account_type: AccountType, last4: Option<String>, label: Option<String>) -> Building {
    Building {
        section: Section {
            currency,
            account_type,
            account_last4: last4,
            account_label: label,
            opening: None,
            closing: None,
            total_debits: None,
            total_credits: None,
            transactions: Vec::new(),
            reconciliation: Reconciliation::Unverified,
            confidence: 0.0,
            report: SectionReport::default(),
            checkpoints: Vec::new(),
        },
        events: Vec::new(),
        conts: Vec::new(),
        anchor_desc_empty: Vec::new(),
        anchor_y: Vec::new(),
        refs: Vec::new(),
    }
}

/// A dated line with no amount yet (CNB): its description runs on below it, the amount comes on the last line.
struct Head {
    page: u32,
    y: f64,
    date: NaiveDate,
    effective: Option<NaiveDate>,
    lines: Vec<String>,
    text: String,
}

fn orphan_head(b: &mut Building, h: Head) {
    b.section.report.skipped.push(SkippedLine { page: h.page, y: h.y, text: h.text, date: Some(h.date), amount: None, reason: "dated line without an amount".into() });
}

fn pass3(meta: &Meta, rows: &[Row], layout: &Layout, diag: &mut Diagnostics) -> Vec<Section> {
    let mut built: Vec<(Building, usize)> = vec![(new_section(meta.currency, meta.account_type, meta.account_last4.clone(), None), 0)];
    let mut last_date: Option<(u32, NaiveDate, Option<NaiveDate>)> = None;
    let mut pending: Option<Head> = None;

    for (ri, r) in rows.iter().enumerate() {
        let text = r.text();
        if layout.kinds[ri] == RowKind::Header {
            // a transaction split by a page break carries on under the next page's header
            if let Some(h) = pending.as_mut() {
                (h.page, h.y) = (r.page, r.yc());
            }
            continue;
        }
        if layout.kinds[ri] == RowKind::Noise {
            continue;
        }
        if let Some(c) = SECTION_ROW.captures(&text)
            && money_items(&r.words).is_empty()
        {
            let cur = &built.last().unwrap().0.section;
            let (ccy, ty) = match (c.name("ccy"), c.name("kind")) {
                (Some(ccy), _) => (Currency::parse(ccy.as_str()).unwrap_or(cur.currency), cur.account_type),
                (_, Some(k)) => {
                    let k = k.as_str().to_lowercase();
                    let ty = if k.starts_with("share") {
                        AccountType::ShareAccount
                    } else if k.starts_with("loan") {
                        AccountType::Loan
                    } else if k.starts_with("sav") {
                        AccountType::Savings
                    } else {
                        AccountType::Checking
                    };
                    (cur.currency, ty)
                }
                _ => (cur.currency, cur.account_type),
            };
            let last4 = DIGITS4.find_iter(&text).last().map(|m| m.as_str()[m.as_str().len() - 4..].to_string()).or(cur.account_last4.clone());
            if (ccy, ty, &last4) != (cur.currency, cur.account_type, &cur.account_last4) {
                let label = Some(text.trim().to_string());
                if built.last().unwrap().0.section.transactions.is_empty() && built.last().unwrap().0.section.checkpoints.is_empty() {
                    built.pop();
                }
                built.push((new_section(ccy, ty, last4, label), ri));
            }
            continue;
        }
        let RowKind::Table(g) = layout.kinds[ri] else { continue };
        let geo = &layout.geos[g];
        let b = &mut built.last_mut().unwrap().0;
        let liability = b.section.account_type.is_liability();
        let p = split_row(r, geo);
        let txn_money: Vec<(ColumnRole, Printed)> = p.monies.iter().filter(|(role, _)| *role != ColumnRole::Balance).copied().collect();
        let bal = p.monies.iter().find(|(role, _)| *role == ColumnRole::Balance).map(|(_, pr)| balance_value(pr, liability));
        let date = dates::parse_first(&p.date, &meta.ctx);
        let post = dates::parse_first(&p.post, &meta.ctx);
        let desc = p.desc.trim().to_string();
        let skip = |b: &mut Building, reason: &str| {
            let amount = txn_money.first().or(p.loose.first().map(|l| (ColumnRole::Amount, *l)).as_ref()).map(|(_, pr)| pr.magnitude).or_else(|| (bal.is_none()).then(|| money::find_amount(&text).map(|pr| pr.magnitude)).flatten());
            let d = date.or(post).or_else(|| dates::parse_first(&text, &meta.ctx));
            if amount.is_some() || d.is_some() {
                b.section.report.skipped.push(SkippedLine { page: r.page, y: r.yc(), text: text.clone(), date: d, amount, reason: reason.into() });
            }
        };

        if SUMMARY_ROW.is_match(&desc) || (desc.is_empty() && SUMMARY_ROW.is_match(&text)) {
            match bal {
                Some(v) => b.section.checkpoints.push((r.page, r.yc(), v)),
                None => skip(b, "summary/total line"),
            }
            continue;
        }

        let near = |h: &Head| h.page == r.page && r.yc() - h.y < 2.6 * r.h();
        let undated = date.is_none() && post.is_none() && p.date.trim().is_empty() && p.post.trim().is_empty();

        if !txn_money.is_empty() {
            let head = if undated { pending.take_if(|h| near(h)) } else { None };
            if let Some(h) = pending.take() {
                orphan_head(b, h);
            }
            // the posting date is the ledger date when both are printed
            let (posted, effective) = match (&head, post, date) {
                (Some(h), _, _) => (Some(h.date), h.effective),
                (None, Some(po), d) => (Some(po), d.filter(|d| *d != po)),
                (None, None, d) => (d, None),
            };
            let desc = match head {
                Some(h) => h.lines.into_iter().chain((!desc.is_empty()).then(|| desc.clone())).collect::<Vec<_>>().join(" "),
                None => desc.clone(),
            };
            let posted = match posted {
                Some(d) => Some(d),
                None => match last_date {
                    Some((pg, d, _)) if pg == r.page && p.date.trim().is_empty() && p.post.trim().is_empty() => {
                        diag.pass3.inherited_dates += 1;
                        Some(d)
                    }
                    _ => None,
                },
            };
            let Some(posted) = posted else {
                skip(b, "amount without a date");
                continue;
            };
            last_date = Some((r.page, posted, effective));
            let mut stmt = Decimal::ZERO;
            let mut flexible = false;
            for (role, pr) in &txn_money {
                let (v, f) = stmt_sign(*role, pr, liability, meta.layout);
                stmt += v;
                flexible |= f;
            }
            let idx = b.section.transactions.len();
            b.section.transactions.push(ScannedTx {
                posted,
                effective,
                description: desc.clone(),
                memo: String::new(),
                amount: if liability { -stmt } else { stmt },
                stmt_amount: stmt,
                balance: bal,
                fx_original: None,
                page: r.page,
                y: r.yc(),
                recovered: false,
                sign_flexible: flexible && txn_money.len() == 1,
            });
            b.anchor_desc_empty.push(desc.is_empty());
            b.anchor_y.push((r.page, r.yc(), r.h()));
            b.refs.push(p.reference.trim().to_string());
            b.events.push(Event::Anchor(idx));
            diag.pass3.anchors += 1;
            continue;
        }
        if let Some(v) = bal {
            // a dated row with only a balance lost its amount (printed outside the columns): Pass 5's job
            if date.is_some() || post.is_some() {
                skip(b, "amount outside the locked columns");
            } else {
                b.section.checkpoints.push((r.page, r.yc(), v));
            }
            continue;
        }
        if !desc.is_empty() && undated && !p.outside {
            let mut line = desc;
            if !p.reference.is_empty() {
                line = format!("{line} {}", p.reference.trim());
            }
            if let Some(h) = pending.as_mut().filter(|h| near(h)) {
                h.lines.push(line);
                h.y = r.yc();
                continue;
            }
            b.conts.push((r.page, r.yc(), r.h(), line));
            b.events.push(Event::Cont(b.conts.len() - 1));
            continue;
        }
        if let Some(d) = post.or(date)
            && !desc.is_empty()
            && !p.outside
        {
            if let Some(h) = pending.take() {
                orphan_head(b, h);
            }
            pending = Some(Head { page: r.page, y: r.yc(), date: d, effective: post.and(date).filter(|e| Some(*e) != post), lines: vec![desc], text: text.clone() });
            continue;
        }
        skip(b, "unrecognised line in the table");
    }
    if let Some(h) = pending.take() {
        orphan_head(&mut built.last_mut().unwrap().0, h);
    }

    let mut out = Vec::new();
    let outside: Vec<usize> = (0..rows.len()).filter(|&i| layout.kinds[i] == RowKind::Outside).collect();
    let all: Vec<&Row> = outside.iter().map(|&i| &rows[i]).collect();
    let starts: Vec<usize> = built.iter().map(|b| b.1).collect();
    let n_sections = built.len();
    for (k, (b, start_row)) in built.into_iter().enumerate() {
        let mut sec = stitch(b, diag);
        if sec.transactions.is_empty() && n_sections > 1 {
            continue;
        }
        // summary box: this section's own rows first, else the whole document's (picked by currency column)
        let end_row = starts.get(k + 1).copied().unwrap_or(rows.len());
        let own: Vec<&Row> = outside.iter().filter(|&&i| i >= start_row && i < end_row).map(|&i| &rows[i]).collect();
        for (label, slot) in [(&*OPENING, 0), (&*CLOSING, 1), (&*DEBITS, 2), (&*CREDITS, 3)] {
            let found = summary_value(&own, label, &meta.currency_columns, sec.currency).or_else(|| {
                (n_sections == 1 || !meta.currency_columns.is_empty()).then(|| summary_value(&all, label, &meta.currency_columns, sec.currency)).flatten()
            });
            if let Some((v, line)) = found {
                diag.pass1.summary_lines.push(line);
                match slot {
                    0 => sec.opening = Some(v),
                    1 => sec.closing = Some(v),
                    2 => sec.total_debits = Some(v.abs()),
                    _ => sec.total_credits = Some(v.abs()),
                }
            }
        }
        for fx in meta.fx.iter().chain(std::iter::once(&*GENERIC_FX)) {
            for t in sec.transactions.iter_mut().filter(|t| t.fx_original.is_none()) {
                let hay = format!("{} {}", t.description, t.memo);
                if let Some(c) = fx.captures(&hay)
                    && let (Some(ccy), Some(amt)) = (c.name("ccy").and_then(|m| Currency::parse(m.as_str())), c.name("amt"))
                    && let Some(a) = money::parse_user(amt.as_str())
                    && ccy != sec.currency
                {
                    t.fx_original = Some((ccy, a.abs()));
                }
            }
        }
        out.push(sec);
    }
    diag.pass3.sections = out.len();
    out
}

/// Pass 3b: give each description-only line to its transaction. Lines go to the transaction above,
/// except when the transaction below printed no description on its own row (a vertically centred
/// multi-line description) and is nearer.
fn stitch(mut b: Building, diag: &mut Diagnostics) -> Section {
    let mut memo: Vec<Vec<String>> = vec![Vec::new(); b.section.transactions.len()];
    let mut prev: Option<usize> = None;
    let mut last_y: Option<(u32, f64)> = None;
    for (ei, e) in b.events.iter().enumerate() {
        match e {
            Event::Anchor(i) => {
                prev = Some(*i);
                last_y = Some((b.anchor_y[*i].0, b.anchor_y[*i].1));
            }
            Event::Cont(c) => {
                let (page, y, h, ref line) = b.conts[*c];
                let next = b.events[ei + 1..].iter().find_map(|e| if let Event::Anchor(i) = e { Some(*i) } else { None }).filter(|&i| b.anchor_y[i].0 == page);
                let up = prev.filter(|&i| b.anchor_y[i].0 == page && last_y.is_some_and(|(p, ly)| p == page && y - ly < 2.6 * h));
                let dist = |i: usize| (b.anchor_y[i].1 - y).abs();
                let target = match (up, next) {
                    (Some(u), Some(n)) if b.anchor_desc_empty[n] && dist(n) < dist(u) && dist(n) < 2.6 * h => Some(n),
                    (None, Some(n)) if b.anchor_desc_empty[n] && dist(n) < 2.6 * h => Some(n),
                    (Some(u), _) => Some(u),
                    _ => None,
                };
                match target {
                    Some(t) => {
                        memo[t].push(line.clone());
                        diag.pass3.stitched_lines += 1;
                        if Some(t) == prev {
                            last_y = Some((page, y));
                        }
                    }
                    None => b.section.report.skipped.push(SkippedLine { page, y, text: line.clone(), date: None, amount: None, reason: "orphan description line".into() }),
                }
            }
        }
    }
    for (i, t) in b.section.transactions.iter_mut().enumerate() {
        let mut lines = std::mem::take(&mut memo[i]);
        // keep the vertical order of the lines
        if t.description.is_empty() {
            t.description = lines.join(" ");
            lines.clear();
        }
        if !b.refs[i].is_empty() {
            lines.push(format!("Ref {}", b.refs[i]));
        }
        t.memo = lines.join(" | ");
        if t.description.is_empty() {
            t.description = "(no description)".into();
        }
    }
    b.section
}
