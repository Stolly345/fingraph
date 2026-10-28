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
    pub currency: Option<Currency>,
    pub currency_columns: Vec<Currency>,
    pub summary_lines: Vec<String>,
    /// Read by OCR from pictures of the pages: amounts are proven by the balances, names may have typos.
    #[serde(default)]
    pub ocr: bool,
    /// The first lines of page 1 (letterhead), for building a new profile.
    #[serde(default)]
    pub letterhead: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pass3Report {
    pub anchors: usize,
    pub stitched_lines: usize,
    pub inherited_dates: usize,
    pub sections: usize,
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
    pub rescans: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub pass1: Pass1Report,
    pub pages: Vec<PageGeometry>,
    pub pass3: Pass3Report,
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

#[cfg(test)]
mod tests {
    use super::*;

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
