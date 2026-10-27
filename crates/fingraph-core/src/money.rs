//! Currencies, the KYD/USD peg, and reading printed amounts (`CI$ 1,685.00 -`, `$-3,000.00`,
//! `(50.00)`, `1,234.56 CR`).

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

static CODES: AtomicBool = AtomicBool::new(false);
static BRACKETS: AtomicBool = AtomicBool::new(false);
static STATEMENT_FX: AtomicBool = AtomicBool::new(true);

/// Display preferences (`KYD 1.00` vs `CI$ 1.00`, brackets for negatives) and whether a rate
/// printed on a statement beats the configured one. Set from `Prefs::apply_globals`.
pub fn set_style(codes: bool, brackets: bool, statement_fx_wins: bool) {
    CODES.store(codes, Ordering::Relaxed);
    BRACKETS.store(brackets, Ordering::Relaxed);
    STATEMENT_FX.store(statement_fx_wins, Ordering::Relaxed);
}

pub fn statement_fx_wins() -> bool {
    STATEMENT_FX.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Currency {
    KYD,
    USD,
}

impl Currency {
    pub const ALL: [Currency; 2] = [Currency::KYD, Currency::USD];

    pub fn code(self) -> &'static str {
        match self {
            Self::KYD => "KYD",
            Self::USD => "USD",
        }
    }
    pub fn symbol(self) -> &'static str {
        match self {
            Self::KYD => "CI$",
            Self::USD => "US$",
        }
    }
    /// `KYD`, `CI$`, `CI`, `KY$`, `USD`, `US$`, `US` (any case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().trim_end_matches(['(', ')', ',', ':']).to_ascii_uppercase().as_str() {
            "KYD" | "CI$" | "CI" | "KY$" | "CID" => Some(Self::KYD),
            "USD" | "US$" | "US" => Some(Self::USD),
            _ => None,
        }
    }
}

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Exchange rates, seeded with the Cayman peg (1 KYD = 1.20 USD).
#[derive(Debug, Clone)]
pub struct Fx {
    pub kyd_to_usd: Decimal,
    pub usd_to_kyd: Decimal,
}

impl Default for Fx {
    fn default() -> Self {
        Fx {
            kyd_to_usd: Decimal::new(120, 2),
            usd_to_kyd: Decimal::new(83333333, 8),
        }
    }
}

impl Fx {
    pub fn rate(&self, from: Currency, to: Currency) -> Decimal {
        match (from, to) {
            (Currency::KYD, Currency::USD) => self.kyd_to_usd,
            (Currency::USD, Currency::KYD) => self.usd_to_kyd,
            _ => Decimal::ONE,
        }
    }
    /// Converted and rounded to cents (banker's rounding is *not* used: statements round half up).
    pub fn convert(&self, amount: Decimal, from: Currency, to: Currency) -> Decimal {
        if from == to {
            return amount;
        }
        round2(amount * self.rate(from, to))
    }
}

pub fn round2(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
}

/// Integer cents, for the `*_minor` columns.
pub fn to_minor(d: Decimal) -> i64 {
    (round2(d) * Decimal::ONE_HUNDRED).to_i64().unwrap_or(0)
}

pub fn from_minor(m: i64) -> Decimal {
    Decimal::new(m, 2)
}

/// `1,234.50` with a leading minus when negative.
pub fn fmt_plain(d: Decimal) -> String {
    let d = round2(d);
    let neg = d.is_sign_negative() && !d.is_zero();
    let s = format!("{:.2}", d.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}{out}.{frac}", if neg { "-" } else { "" })
}

/// `-CI$ 1,234.50` / `US$ 15.99`.
pub fn fmt_money(d: Decimal, ccy: Currency) -> String {
    let p = fmt_plain(d);
    let sym = if CODES.load(Ordering::Relaxed) { ccy.code() } else { ccy.symbol() };
    match p.strip_prefix('-') {
        Some(rest) if BRACKETS.load(Ordering::Relaxed) => format!("({sym} {rest})"),
        Some(rest) => format!("-{sym} {rest}"),
        None => format!("{sym} {p}"),
    }
}

/// A printed amount: always-positive magnitude plus whatever told us its direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Printed {
    pub magnitude: Decimal,
    /// `-`, `(…)` → Some(true); `+` → Some(false).
    pub negative: Option<bool>,
    /// `CR` → Some(true), `DR` → Some(false).
    pub credit_marker: Option<bool>,
    pub currency: Option<Currency>,
}

/// Is this one word the numeric core of an amount (`1,234.56`, `$-3,000.00`, `(12.00)`, `5.00CR`)?
pub fn is_amount_core(word: &str) -> bool {
    let w = word.trim_start_matches(['(', '-', '+']);
    let w = w.strip_prefix("CI$").or_else(|| w.strip_prefix("US$")).or_else(|| w.strip_prefix('$')).unwrap_or(w);
    let w = w.trim_start_matches(['-', '+']);
    let w = w.trim_end_matches(')');
    let w = w.strip_suffix("CR").or_else(|| w.strip_suffix("DR")).or_else(|| w.strip_suffix("Cr")).or_else(|| w.strip_suffix("Dr")).unwrap_or(w);
    let w = w.trim_end_matches(['-', '+', ')']);
    let Some((int, frac)) = w.split_once('.') else { return false };
    if frac.len() != 2 || !frac.bytes().all(|b| b.is_ascii_digit()) || int.is_empty() {
        return false;
    }
    // thousands groups must be 3 digits
    let mut groups = int.split(',');
    let first = groups.next().unwrap_or("");
    let max_first = if int.contains(',') { 3 } else { 12 };
    !first.is_empty() && first.len() <= max_first && first.bytes().all(|b| b.is_ascii_digit()) && groups.all(|g| g.len() == 3 && g.bytes().all(|b| b.is_ascii_digit()))
}

/// Words that may sit next to an amount's core and belong to it.
pub fn is_amount_affix(word: &str) -> bool {
    matches!(word, "-" | "+" | "(" | ")" | "CR" | "DR" | "Cr" | "Dr" | "cr" | "dr" | "$") || Currency::parse(word).is_some()
}

/// Parse an amount from its words joined by spaces (`CI$ 1,685.00 -`).
pub fn parse_printed(text: &str) -> Option<Printed> {
    let t = text.trim();
    let mut negative = None;
    let mut credit_marker = None;
    let mut currency = None;
    let upper = t.to_ascii_uppercase();
    if upper.contains("CI$") || upper.contains("KYD") {
        currency = Some(Currency::KYD);
    } else if upper.contains("US$") || upper.contains("USD") {
        currency = Some(Currency::USD);
    }
    if t.contains('(') && t.contains(')') {
        negative = Some(true);
    }
    let digits_start = t.find(|c: char| c.is_ascii_digit())?;
    let digits_end = t.rfind(|c: char| c.is_ascii_digit())? + 1;
    let before = &t[..digits_start];
    let after = t[digits_end..].trim();
    if before.contains('-') {
        negative = Some(true);
    } else if before.contains('+') {
        negative = Some(false);
    }
    let after_up = after.to_ascii_uppercase();
    if after_up.starts_with('-') || after_up.ends_with('-') {
        negative = Some(true);
    } else if after_up.starts_with('+') {
        negative = Some(false);
    }
    if after_up.contains("CR") {
        credit_marker = Some(true);
    } else if after_up.contains("DR") {
        credit_marker = Some(false);
    }
    let num: String = t[digits_start..digits_end].chars().filter(|c| *c != ',').collect();
    let magnitude = Decimal::from_str(&num).ok()?;
    Some(Printed { magnitude, negative, credit_marker, currency })
}

/// Lenient amount from free text (Pass 5 / user input): first thing that looks like money.
pub fn find_amount(text: &str) -> Option<Printed> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let i = words.iter().position(|w| is_amount_core(w))?;
    let mut lo = i;
    while lo > 0 && is_amount_affix(words[lo - 1]) {
        lo -= 1;
    }
    let mut hi = i + 1;
    while hi < words.len() && is_amount_affix(words[hi]) && Currency::parse(words[hi]).is_none() {
        hi += 1;
    }
    parse_printed(&words[lo..hi].join(" "))
}

/// User-typed or stored amount (`-12.50`, `1,200`, `(40.00)`).
pub fn parse_user(text: &str) -> Option<Decimal> {
    let t = text.trim();
    let neg = t.starts_with('-') || (t.starts_with('(') && t.ends_with(')'));
    let num: String = t.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let d = Decimal::from_str(&num).ok()?;
    Some(if neg { -d } else { d })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn printed_forms() {
        let p = parse_printed("CI$ 1,685.00 -").unwrap();
        assert_eq!((p.magnitude, p.negative, p.currency), (d("1685.00"), Some(true), Some(Currency::KYD)));
        let p = parse_printed("$-3,000.00").unwrap();
        assert_eq!((p.magnitude, p.negative), (d("3000.00"), Some(true)));
        let p = parse_printed("(50.00)").unwrap();
        assert_eq!(p.negative, Some(true));
        let p = parse_printed("1,234.56 CR").unwrap();
        assert_eq!(p.credit_marker, Some(true));
        let p = parse_printed("2,720.64 +").unwrap();
        assert_eq!(p.negative, Some(false));
    }

    #[test]
    fn amount_cores() {
        for ok in ["1,685.00", "$-3,000.00", "(50.00)", "5.00CR", "12.34-", "$10.38", "1234567.00"] {
            assert!(is_amount_core(ok), "{ok}");
        }
        for bad in ["1", "0.84000", "1361684850032", "15AUG26", "12,34.00", "1.5", "7005311"] {
            assert!(!is_amount_core(bad), "{bad}");
        }
    }

    #[test]
    fn peg_and_minor() {
        let fx = Fx::default();
        assert_eq!(fx.convert(d("100.00"), Currency::KYD, Currency::USD), d("120.00"));
        assert_eq!(fx.convert(d("120.00"), Currency::USD, Currency::KYD), d("100.00"));
        assert_eq!(to_minor(d("-12.345")), -1235);
        assert_eq!(fmt_plain(d("-1234567.5")), "-1,234,567.50");
        assert_eq!(fmt_money(d("-15.99"), Currency::USD), "-US$ 15.99");
    }
}
