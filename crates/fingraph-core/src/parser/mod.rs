//! The 5-pass statement scanner.
//!
//! 1. `multipass::pass1` — spatial tokens, institution fingerprint, period/account/currency, summary box.
//! 2. `multipass::pass2` — rows, column geometry per page (header words, then money right-edge histograms), noise strip.
//! 3. `multipass::pass3` — transaction rows by locked columns, date inheritance, multi-line stitching, sections.
//! 4. `reconcile::verify` — running balance `Bal[i-1] + Amt[i] = Bal[i]`, sign self-healing, summary equation.
//! 5. `reconcile::heal` — re-scan the skipped lines between a failing pair of rows, then report what's left.

pub mod dates;
pub mod multipass;
pub mod profiles;
pub mod reconcile;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

use std::process::Command;

use crate::{AccountType, Currency, Error, Reconciliation, Result};
pub use multipass::MultiPassScanner;
pub use profiles::{BankProfile, LayoutKind};

/// One word on a page with its box, in PDF points from the top-left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub page: u32,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub text: String,
}

impl Token {
    pub fn new(page: u32, x0: f64, y0: f64, x1: f64, y1: f64, text: &str) -> Self {
        Token { page, x0, y0, x1, y1, text: text.to_string() }
    }
    pub fn xc(&self) -> f64 {
        (self.x0 + self.x1) / 2.0
    }
    pub fn yc(&self) -> f64 {
        (self.y0 + self.y1) / 2.0
    }
    pub fn h(&self) -> f64 {
        (self.y1 - self.y0).max(1.0)
    }
}

/// Every word of a PDF with its box, via poppler's `pdftotext -bbox` (font names aren't exposed there,
/// so geometry does all the work). Image-only PDFs come back empty: see [`ocr_tokens`].
pub fn tokens_from_pdf(path: &Path) -> Result<Vec<Token>> {
    let out = Command::new("pdftotext")
        .arg("-bbox")
        .arg(path)
        .arg("-")
        .output()
        .map_err(|e| Error(format!("couldn't run pdftotext (install poppler-utils): {e}")))?;
    if !out.status.success() {
        return Err(Error(format!("pdftotext failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(parse_bbox(&String::from_utf8_lossy(&out.stdout)))
}

const OCR_DPI: f64 = 300.0;
static OCR_SPLIT_HEAD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\d{1,3},$").unwrap());
static OCR_COMMA_CENTS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^(\d{1,3}(?:,\d{3})*),(\d{2})(\D*)$").unwrap());

/// Words of an image-only PDF (a scan, or a redaction that flattened the pages) by OCR: each page is
/// rendered in grey, anything lighter than the ink is blanked (so a pale "CUSTOMER COPY" watermark
/// doesn't garble the amounts under it), and tesseract's word boxes are scaled back to points.
pub fn ocr_tokens(path: &Path) -> Result<Vec<Token>> {
    let dir = std::env::temp_dir().join(format!("fingraph-ocr-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    std::fs::create_dir_all(&dir)?;
    let r = ocr_in(path, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn ocr_in(path: &Path, dir: &Path) -> Result<Vec<Token>> {
    // ponytail: first 20 pages only (~3 s each), so a big scanned document in the watched folder can't
    // tie the machine up for minutes; raise it if a real statement ever runs longer
    let out = Command::new("pdftoppm").args(["-r", &OCR_DPI.to_string(), "-gray", "-l", "20"]).arg(path).arg(dir.join("p")).output()?;
    if !out.status.success() {
        return Err(Error(format!("pdftoppm failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let mut pages: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "pgm")).collect();
    pages.sort(); // p-01.pgm, p-02.pgm… zero-padded, so this is page order
    let scale = 72.0 / OCR_DPI;
    let mut tokens: Vec<Token> = Vec::new();
    for (i, img) in pages.iter().enumerate() {
        threshold_pgm(img)?;
        let out = Command::new("tesseract")
            .arg(img)
            .args(["stdout", "--psm", "6", "tsv"])
            .output()
            .map_err(|e| Error(format!("this PDF is only pictures of pages; reading it needs tesseract (sudo apt install tesseract-ocr): {e}")))?;
        if !out.status.success() {
            return Err(Error(format!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        // level page block par line word left top width height conf text
        let mut line_of: Option<String> = None;
        for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 12 || f[0] != "5" || f[11].trim().is_empty() {
                continue;
            }
            let n = |k: usize| f[k].parse::<f64>().unwrap_or(0.0) * scale;
            let (x, y) = (n(6), n(7));
            let mut t = Token::new(i as u32 + 1, x, y, x + n(8), y + n(9), f[11].trim());
            let id = format!("{}.{}.{}", f[2], f[3], f[4]);
            // `3,` `748.78` → `3,748.78`: OCR split one amount in two
            if let Some(prev) = tokens.last_mut().filter(|_| line_of.as_deref() == Some(id.as_str()))
                && OCR_SPLIT_HEAD.is_match(&prev.text)
                && t.text.starts_with(|c: char| c.is_ascii_digit())
                && t.x0 - prev.x1 < prev.h()
            {
                prev.text.push_str(&t.text);
                prev.x1 = t.x1;
                prev.y1 = prev.y1.max(t.y1);
                t = prev.clone();
                tokens.pop();
            }
            // `49,48` is `49.48` with the point misread
            t.text = OCR_COMMA_CENTS.replace(&t.text, "${1}.${2}${3}").into_owned();
            tokens.push(t);
            line_of = Some(id);
        }
    }
    Ok(tokens)
}

/// Binary greyscale PGM in place: ink stays black, everything paler goes white.
fn threshold_pgm(img: &Path) -> Result<()> {
    let mut b = std::fs::read(img)?;
    // header: "P5", width, height, maxval, each followed by whitespace
    let mut fields = 0;
    let mut i = 0;
    while fields < 4 && i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        fields += 1;
    }
    for px in b.iter_mut().skip(i + 1) {
        // 180 read a real CNB statement best (120 and 210 both misread more digits)
        *px = if *px > 180 { 255 } else { 0 };
    }
    std::fs::write(img, b)?;
    Ok(())
}

fn attr(line: &str, name: &str) -> f64 {
    line.find(&format!("{name}=\""))
        .and_then(|i| {
            let rest = &line[i + name.len() + 2..];
            rest[..rest.find('"')?].parse().ok()
        })
        .unwrap_or(0.0)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#39;", "'").replace("&amp;", "&")
}

pub fn parse_bbox(xhtml: &str) -> Vec<Token> {
    let mut page = 0;
    let mut out = Vec::new();
    for line in xhtml.lines() {
        let l = line.trim();
        if l.starts_with("<page") {
            page += 1;
        } else if l.starts_with("<word") {
            let (Some(a), Some(b)) = (l.find('>'), l.rfind("</word>")) else { continue };
            let text = unescape(&l[a + 1..b]);
            if !text.trim().is_empty() {
                out.push(Token { page, x0: attr(l, "xMin"), y0: attr(l, "yMin"), x1: attr(l, "xMax"), y1: attr(l, "yMax"), text });
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnRole {
    Date,
    PostDate,
    Description,
    Reference,
    Debit,
    Credit,
    Amount,
    Balance,
}

impl ColumnRole {
    pub fn is_money(self) -> bool {
        matches!(self, Self::Debit | Self::Credit | Self::Amount | Self::Balance)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnBand {
    pub role: ColumnRole,
    pub x0: f64,
    pub x1: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageGeometry {
    pub page: u32,
    pub header_y: Option<f64>,
    pub columns: Vec<ColumnBand>,
    /// No header on this page: the previous page's columns were reused.
    pub inherited: bool,
    pub rows: usize,
    pub noise_stripped: Vec<String>,
    /// The header words that named each column (for the column-mapping helper).
    #[serde(default)]
    pub header_words: Vec<(ColumnRole, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pass1Report {
    pub profile: Option<String>,
    pub scores: Vec<(String, u32)>,
    pub period: Option<(NaiveDate, NaiveDate)>,
    pub account_last4: Option<String>,
    pub account_type: Option<AccountType>,
    /// The statement doesn't say what kind of account it is (no card wording, no "savings account" and the
    /// like, no bank profile that says): the type above is a default.
    #[serde(default)]
    pub type_unstated: bool,
    pub currency: Option<Currency>,
    pub currency_columns: Vec<Currency>,
    pub summary_lines: Vec<String>,
    /// Read by OCR from pictures of the pages: amounts are proven by the balances, names may have typos.
    #[serde(default)]
    pub ocr: bool,
    /// The first lines of page 1 (letterhead), for building a new profile.
    #[serde(default)]
    pub letterhead: Vec<String>,
    /// What on page 1 could recognise this bank's statements: its name and web/email domain.
    #[serde(default)]
    pub identifiers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pass3Report {
    pub anchors: usize,
    pub stitched_lines: usize,
    pub inherited_dates: usize,
    pub sections: usize,
    /// Dated lines whose amount came on a later line (CNB's row shape).
    #[serde(default)]
    pub joined_rows: usize,
    /// Where this statement didn't match its bank's declared layout, in plain words.
    #[serde(default)]
    pub layout_notes: Vec<String>,
    /// Not read with the saved layout: how it was read instead (`with generic column headings`).
    #[serde(default)]
    pub read_as: Option<String>,
    /// Transactions whose date has no year (taken from the statement period, else assumed).
    #[serde(default)]
    pub yearless_dates: usize,
    /// Lines that printed both a debit and a credit (read as two transactions).
    #[serde(default)]
    pub debit_and_credit_lines: usize,
    /// Listed newest first (read bottom-up; transaction pages and positions are mirrored).
    #[serde(default)]
    pub newest_first: bool,
    /// The transaction and posting date columns were taken the wrong way round, and swapped back.
    #[serde(default)]
    pub date_columns_swapped: bool,
}

/// What a read found out about a statement's layout. Approving a held statement teaches its bank profile
/// these (`Db::approve_document`), and each import is compared with the account's last approved one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservedLayout {
    /// The heading over each column, lower case.
    pub headings: Vec<(ColumnRole, String)>,
    /// The money columns, left to right.
    pub money_columns: Vec<ColumnRole>,
    pub row_shape: profiles::RowShape,
    pub month_first: bool,
    /// The words on the opening and closing balance lines (`balance brought forward`).
    pub opening: Option<String>,
    pub closing: Option<String>,
}

impl ObservedLayout {
    pub fn is_empty(&self) -> bool {
        self.headings.is_empty() && self.money_columns.is_empty()
    }

    /// How `self` (a new statement) looks different from `before` (the account's last approved one), in
    /// plain words; empty when either wasn't recorded.
    pub fn changes_from(&self, before: &ObservedLayout) -> Vec<String> {
        if self.is_empty() || before.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let words = |o: &ObservedLayout| {
            let mut v: Vec<String> = o.headings.iter().map(|h| h.1.clone()).collect();
            v.sort();
            v.dedup();
            v
        };
        let (now, was) = (words(self), words(before));
        if now != was {
            out.push(format!("column headings are now {} (were {})", now.join(", "), was.join(", ")));
        }
        if self.money_columns != before.money_columns {
            let names = |v: &[ColumnRole]| v.iter().map(|r| format!("{r:?}").to_lowercase()).collect::<Vec<_>>().join(", ");
            out.push(format!("amount columns are now {} (were {})", names(&self.money_columns), names(&before.money_columns)));
        }
        if self.row_shape != before.row_shape && self.row_shape != profiles::RowShape::Auto && before.row_shape != profiles::RowShape::Auto {
            out.push("amounts moved to a different line of each transaction".into());
        }
        if self.month_first != before.month_first {
            out.push("dates are written the other way round (day and month)".into());
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentCheck {
    /// Transaction indexes (within the section) bounding the check; `None` = the opening balance.
    pub from_tx: Option<usize>,
    pub to_tx: Option<usize>,
    pub expected: Decimal,
    pub got: Decimal,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedLine {
    pub page: u32,
    pub y: f64,
    pub text: String,
    pub date: Option<NaiveDate>,
    pub amount: Option<Decimal>,
    pub reason: String,
    /// The balance printed on the line, when its amount couldn't be read (Pass 5 works the amount out from it).
    #[serde(default)]
    pub balance: Option<Decimal>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SummaryCheck {
    pub opening: Option<Decimal>,
    pub closing: Option<Decimal>,
    pub total_debits: Option<Decimal>,
    pub total_credits: Option<Decimal>,
    pub extracted_debits: Decimal,
    pub extracted_credits: Decimal,
    /// closing − (opening + extracted), in statement terms. `None` when there's nothing to compare.
    pub difference: Option<Decimal>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SectionReport {
    pub segments: Vec<SegmentCheck>,
    pub sign_heals: Vec<String>,
    pub summary: SummaryCheck,
    pub recovered: Vec<String>,
    pub skipped: Vec<SkippedLine>,
    /// Printed balances that disagree with both neighbours (a misprint, or an OCR misread), left out of the checks.
    #[serde(default)]
    pub misread_balances: Vec<String>,
    /// Where amounts were worked out from the balances rather than read (page, y): held until someone looks,
    /// unless reading the cell again confirms them.
    #[serde(default)]
    pub derived: Vec<(u32, f64)>,
    pub rescans: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub pass1: Pass1Report,
    pub pages: Vec<PageGeometry>,
    pub pass3: Pass3Report,
    #[serde(default)]
    pub observed: ObservedLayout,
}

/// One extracted transaction, signed for the ledger (negative = money out of the account holder's pocket).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScannedTx {
    pub posted: NaiveDate,
    pub effective: Option<NaiveDate>,
    pub description: String,
    pub memo: String,
    pub amount: Decimal,
    /// Signed the statement's way: positive moves the printed balance up (a deposit, or a card charge).
    pub stmt_amount: Decimal,
    pub balance: Option<Decimal>,
    /// The original amount from an FX line (`USD 145.00 @ 0.84`, `(59.14 KYD)`).
    pub fx_original: Option<(Currency, Decimal)>,
    pub page: u32,
    pub y: f64,
    pub recovered: bool,
    #[serde(skip)]
    pub(crate) sign_flexible: bool,
}

/// One account's worth of a statement (a dual-currency card or a credit-union statement has several).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub currency: Currency,
    pub account_type: AccountType,
    pub account_last4: Option<String>,
    pub account_label: Option<String>,
    pub opening: Option<Decimal>,
    pub closing: Option<Decimal>,
    pub total_debits: Option<Decimal>,
    pub total_credits: Option<Decimal>,
    pub transactions: Vec<ScannedTx>,
    pub reconciliation: Reconciliation,
    pub confidence: f64,
    pub report: SectionReport,
    /// Balance-only rows (`OPENING BALANCE`, `BALANCE FORWARD`): page, y, balance.
    #[serde(default)]
    pub checkpoints: Vec<(u32, f64, Decimal)>,
}

impl Section {
    pub fn extracted_sum(&self) -> Decimal {
        self.transactions.iter().map(|t| t.amount).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub profile_slug: Option<String>,
    pub period: Option<(NaiveDate, NaiveDate)>,
    pub sections: Vec<Section>,
    pub diagnostics: Diagnostics,
}

impl ScanResult {
    pub fn is_reconciled(&self) -> bool {
        !self.sections.is_empty() && self.sections.iter().all(|s| s.reconciliation == Reconciliation::Reconciled)
    }
}

/// Words a layout sample keeps as they are: the vocabulary of headings, summary boxes and periods.
const LAYOUT_WORDS: &[&str] = &[
    "date", "dates", "transaction", "posting", "posted", "value", "description", "details", "particulars", "reference", "ref", "no.", "cheque", "#", "debit", "debits",
    "credit", "credits", "cred", "amount", "balance", "withdrawals", "withdrawal", "deposits", "deposit", "opening", "closing", "beginning", "ending", "previous", "new",
    "total", "totals", "page", "of", "statement", "period", "periodic", "to", "from", "as", "account", "currency", "kyd", "usd", "ci$", "us$", "$", "dr", "cr", "&",
    "and", "other", "cheques", "payments", "purchases", "cash", "advances", "minimum", "payment", "due", "limit", "share", "shares", "savings", "loan", "chequing",
    "checking", "interest", "brought", "carried", "forward", "-", "/", "(+)", "(-)", "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    "january", "february", "march", "april", "june", "july", "august", "september", "october", "november", "december", "visa", "mastercard", "amex",
    "american", "express", "card", "number", "business", "current", "personal", "product",
];

/// A statement's layout without its private contents, to share or keep as a test: every word stays where
/// it was, and dates, amounts, headings, summary labels and the bank's own identifying words stay as
/// printed; every other word has its letters turned to x and its digits to 0 (names, merchants,
/// account and reference numbers). `keep` adds words, e.g. a bank profile's identifiers and labels.
pub fn layout_sample(tokens: &[Token], keep: &[String]) -> Vec<Token> {
    let keep: Vec<String> = keep.iter().flat_map(|k| k.split_whitespace()).map(|w| w.to_lowercase()).collect();
    tokens
        .iter()
        .map(|t| {
            let w = t.text.as_str();
            let lower = w.to_lowercase();
            let bare = lower.trim_matches(|c: char| matches!(c, ':' | ',' | '.' | '(' | ')'));
            let dated = dates::find_dates(w).first().is_some_and(|d| d.0 == 0 && d.1 == w.len())
                || (bare.len() == 4 && (bare.starts_with("19") || bare.starts_with("20")) && bare.bytes().all(|b| b.is_ascii_digit()));
            if crate::money::is_amount_core(w) || dated || LAYOUT_WORDS.contains(&bare) || keep.iter().any(|k| k == &lower || k == bare) || w.chars().count() <= 2 {
                return t.clone();
            }
            let text: String = w.chars().map(|c| if c.is_ascii_digit() { '0' } else if c.is_uppercase() { 'X' } else if c.is_alphabetic() { 'x' } else { c }).collect();
            Token { text, ..t.clone() }
        })
        .collect()
}

/// A layout sample on disk: which bank it's from and its anonymised words. Samples of real statements go in
/// `tests/fixtures/private/` (not committed), where `private_layout_samples` checks every one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutSample {
    pub bank: Option<String>,
    pub ocr: bool,
    pub tokens: Vec<Token>,
}

/// One amount cell of a scanned page read again: just that rectangle (points), rendered at 400 dpi with pale ink
/// blanked, OCR'd as a single line allowed only digits and amount signs. Returns the words read. Nothing is
/// believed on its own: callers only accept what the balances already asked for.
pub(crate) fn reread_cell(path: &Path, page: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<String> {
    const DPI: f64 = 400.0;
    let px = |v: f64| ((v.max(0.0)) * DPI / 72.0).round() as i64;
    let dir = std::env::temp_dir().join(format!("fingraph-cell-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let img = dir.join("cell");
    let out = (|| -> Option<String> {
        let ok = Command::new("pdftoppm")
            .args(["-r", "400", "-gray", "-singlefile", "-f", &page.to_string(), "-l", &page.to_string()])
            .args(["-x", &px(x0).to_string(), "-y", &px(y0).to_string(), "-W", &(px(x1) - px(x0)).max(1).to_string(), "-H", &(px(y1) - px(y0)).max(1).to_string()])
            .arg(path)
            .arg(&img)
            .status()
            .ok()?
            .success();
        let pgm = img.with_extension("pgm");
        if !ok || threshold_pgm(&pgm).is_err() {
            return None;
        }
        let o = Command::new("tesseract").arg(&pgm).args(["stdout", "--psm", "7", "-c", "tessedit_char_whitelist=0123456789.,-()"]).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    out.unwrap_or_default().split_whitespace().map(str::to_string).collect()
}

/// A re-read cell's words as amounts (`.26` counts as 0.26).
pub(crate) fn cell_amounts(words: &[String]) -> Vec<Decimal> {
    words
        .iter()
        .filter_map(|w| {
            let w = w.trim_matches(['(', ')']);
            let w = if let Some(rest) = w.strip_prefix('.') { format!("0.{rest}") } else if let Some(rest) = w.strip_prefix("-.") { format!("-0.{rest}") } else { w.to_string() };
            crate::money::is_amount_core(&w).then(|| crate::money::parse_printed(&w)).flatten().map(|p| p.magnitude)
        })
        .collect()
}

/// Whether a re-read cell shows exactly the digits of `amount` (`26` or `-26` for 0.26): OCR loses a faint point
/// or reads it as a dash, but rarely invents digits — and `amount` came from the balances, not from this read.
pub(crate) fn cell_shows_digits(words: &[String], amount: Decimal) -> bool {
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>().trim_start_matches('0').to_string();
    let want = digits(&format!("{:.2}", amount.abs()));
    !want.is_empty() && words.iter().any(|w| digits(w) == want)
}

/// The words of a PDF: its text layer, or OCR when it's only pictures of pages (true).
pub fn page_tokens(path: &Path) -> Result<(Vec<Token>, bool)> {
    let tokens = tokens_from_pdf(path)?;
    if !tokens.is_empty() {
        return Ok((tokens, false));
    }
    Ok((ocr_tokens(path)?, true))
}

/// A statement as a shareable layout sample (JSON), keeping its bank's own words.
pub fn layout_sample_json(path: &Path, profiles: &[BankProfile]) -> Result<String> {
    let (tokens, ocr) = page_tokens(path)?;
    let scan = MultiPassScanner::new(profiles).scan_tokens(tokens.clone())?;
    let p = scan.profile_slug.as_ref().and_then(|s| profiles.iter().find(|p| &p.slug == s));
    let keep: Vec<String> = p
        .map(|p| {
            let a = &p.header_anchors;
            let l = &p.layout;
            [&p.routing_identifiers, &a.date, &a.posting_date, &a.description, &a.reference, &a.debit, &a.credit, &a.amount, &a.balance, &l.opening, &l.closing, &l.debits, &l.credits]
                .into_iter()
                .flatten()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Ok(serde_json::to_string_pretty(&LayoutSample { bank: scan.profile_slug, ocr, tokens: layout_sample(&tokens, &keep) })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_sample_reads_the_same_without_the_names() {
        let p = profiles::builtins();
        let doc = &crate::demo::demo_documents()[1];
        let keep: Vec<String> = p.iter().flat_map(|p| p.routing_identifiers.clone()).collect();
        let sample = layout_sample(&doc.tokens, &keep);
        let (a, b) = (MultiPassScanner::new(&p).scan_tokens(doc.tokens.clone()).unwrap(), MultiPassScanner::new(&p).scan_tokens(sample.clone()).unwrap());
        assert_eq!(a.profile_slug, b.profile_slug);
        let money = |r: &ScanResult| r.sections.iter().map(|s| (s.reconciliation.clone(), s.transactions.iter().map(|t| (t.posted, t.amount)).collect::<Vec<_>>())).collect::<Vec<_>>();
        assert_eq!(money(&a), money(&b));
        // the merchants are gone (heading words like PAYMENT may also appear in a description, and stay)
        let words: Vec<&str> = sample.iter().map(|t| t.text.as_str()).collect();
        let private: Vec<&str> = a.sections[0]
            .transactions
            .iter()
            .flat_map(|t| t.description.split_whitespace())
            .filter(|w| w.len() > 3 && !w.chars().any(|c| c.is_ascii_digit()) && !LAYOUT_WORDS.contains(&w.to_lowercase().as_str()))
            .collect();
        assert!(!private.is_empty() && private.iter().all(|w| !words.contains(w)), "{private:?}");
    }

    /// A re-read amount cell confirms a worked-out amount only by its exact digits (OCR loses or garbles the point,
    /// it rarely invents digits), and only well-formed amounts can fill a gap.
    #[test]
    fn a_reread_cell_confirms_by_digits_and_fills_only_with_amounts() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let d = |s: &str| Decimal::from_str_exact(s).unwrap();
        assert!(cell_shows_digits(&w(&["-26"]), d("0.26")));
        assert!(cell_shows_digits(&w(&["1,243,92"]), d("1243.92")));
        assert!(!cell_shows_digits(&w(&["126"]), d("0.26")));
        assert!(!cell_shows_digits(&w(&["26"]), d("2.60")), "2.60 is 260");
        assert_eq!(cell_amounts(&w(&[".26", "598.71", "26"])), vec![d("0.26"), d("598.71")]);
    }

    #[test]
    fn ocr_misread_decimal_commas_are_fixed_and_real_thousands_kept() {
        let fix = |t: &str| OCR_COMMA_CENTS.replace(t, "${1}.${2}${3}").into_owned();
        assert_eq!(fix("49,48"), "49.48");
        assert_eq!(fix("1,946,30"), "1,946.30");
        assert_eq!(fix("1,946.30"), "1,946.30");
        assert_eq!(fix("1,946"), "1,946");
        assert!(OCR_SPLIT_HEAD.is_match("3,") && !OCR_SPLIT_HEAD.is_match("3,748.78"));
    }
}
