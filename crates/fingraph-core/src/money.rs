//! Currencies, the KYD/USD peg, and reading printed amounts (`CI$ 1,685.00 -`, `$-3,000.00`,
//! `(50.00)`, `1,234.56 CR`).

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use chrono::NaiveDate;
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

/// A currency FinGraph keeps accounts in: one of `CURRENCIES` (an index into it, so it's `Copy` and its code
/// and symbol are `'static`). KYD and USD come first; stored and serialised as the ISO code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Currency(u8);

/// (ISO code, symbol, name). Codes that are also everyday English words are left out (ALL, TOP, CUP, PEN, TRY,
/// GEL, MAD, BOB…): next to an amount in a description they'd be read as its currency.
pub const CURRENCIES: &[(&str, &str, &str)] = &[
    ("KYD", "CI$", "Cayman Islands dollar"),
    ("USD", "US$", "US dollar"),
    ("EUR", "€", "Euro"),
    ("GBP", "£", "Pound sterling"),
    ("CAD", "C$", "Canadian dollar"),
    ("JMD", "J$", "Jamaican dollar"),
    ("TTD", "TT$", "Trinidad and Tobago dollar"),
    ("BBD", "Bds$", "Barbadian dollar"),
    ("BSD", "B$", "Bahamian dollar"),
    ("BMD", "BD$", "Bermudian dollar"),
    ("XCD", "EC$", "East Caribbean dollar"),
    ("BZD", "BZ$", "Belize dollar"),
    ("GYD", "G$", "Guyanese dollar"),
    ("HNL", "HNL", "Honduran lempira"),
    ("GTQ", "GTQ", "Guatemalan quetzal"),
    ("NIO", "C$N", "Nicaraguan córdoba"),
    ("CRC", "₡", "Costa Rican colón"),
    ("PAB", "PAB", "Panamanian balboa"),
    ("DOP", "RD$", "Dominican peso"),
    ("HTG", "HTG", "Haitian gourde"),
    ("MXN", "MX$", "Mexican peso"),
    ("COP", "COP", "Colombian peso"),
    ("BRL", "R$", "Brazilian real"),
    ("ARS", "ARS", "Argentine peso"),
    ("CLP", "CLP", "Chilean peso"),
    ("UYU", "UYU", "Uruguayan peso"),
    ("AUD", "A$", "Australian dollar"),
    ("NZD", "NZ$", "New Zealand dollar"),
    ("CHF", "CHF", "Swiss franc"),
    ("SEK", "SEK", "Swedish krona"),
    ("NOK", "NOK", "Norwegian krone"),
    ("DKK", "DKK", "Danish krone"),
    ("ISK", "ISK", "Icelandic króna"),
    ("PLN", "PLN", "Polish złoty"),
    ("CZK", "CZK", "Czech koruna"),
    ("HUF", "HUF", "Hungarian forint"),
    ("RON", "RON", "Romanian leu"),
    ("BGN", "BGN", "Bulgarian lev"),
    ("UAH", "UAH", "Ukrainian hryvnia"),
    ("JPY", "JPY", "Japanese yen"),
    ("CNY", "CNY", "Chinese yuan"),
    ("HKD", "HK$", "Hong Kong dollar"),
    ("SGD", "S$", "Singapore dollar"),
    ("KRW", "₩", "South Korean won"),
    ("THB", "฿", "Thai baht"),
    ("MYR", "RM", "Malaysian ringgit"),
    ("IDR", "IDR", "Indonesian rupiah"),
    ("PHP", "₱", "Philippine peso"),
    ("VND", "₫", "Vietnamese đồng"),
    ("INR", "₹", "Indian rupee"),
    ("PKR", "PKR", "Pakistani rupee"),
    ("LKR", "LKR", "Sri Lankan rupee"),
    ("BDT", "৳", "Bangladeshi taka"),
    ("NPR", "NPR", "Nepalese rupee"),
    ("AED", "AED", "UAE dirham"),
    ("SAR", "SAR", "Saudi riyal"),
    ("QAR", "QAR", "Qatari riyal"),
    ("ILS", "₪", "Israeli shekel"),
    ("EGP", "EGP", "Egyptian pound"),
    ("NGN", "₦", "Nigerian naira"),
    ("GHS", "GH₵", "Ghanaian cedi"),
    ("KES", "KSh", "Kenyan shilling"),
    ("ZAR", "ZAR", "South African rand"),
    ("ZWG", "ZWG", "Zimbabwe gold"),
];

/// Symbols and old spellings that name a currency (codes are matched as they are).
const ALIASES: &[(&str, &str)] = &[
    ("CI$", "KYD"), ("CI", "KYD"), ("KY$", "KYD"), ("CID", "KYD"),
    ("US$", "USD"), ("US", "USD"),
    ("€", "EUR"), ("£", "GBP"), ("C$", "CAD"), ("CA$", "CAD"), ("J$", "JMD"), ("TT$", "TTD"), ("BDS$", "BBD"),
    ("B$", "BSD"), ("BD$", "BMD"), ("EC$", "XCD"), ("BZ$", "BZD"), ("G$", "GYD"), ("RD$", "DOP"), ("MX$", "MXN"),
    ("R$", "BRL"), ("A$", "AUD"), ("AU$", "AUD"), ("NZ$", "NZD"), ("HK$", "HKD"), ("S$", "SGD"), ("₱", "PHP"),
    ("₹", "INR"), ("₦", "NGN"), ("₪", "ILS"), ("₩", "KRW"), ("฿", "THB"), ("₡", "CRC"), ("₫", "VND"), ("৳", "BDT"),
    ("GH₵", "GHS"), ("KSH", "KES"),
];

impl Currency {
    pub const KYD: Currency = Currency(0);
    pub const USD: Currency = Currency(1);

    /// Every currency FinGraph knows, KYD and USD first.
    pub fn all() -> impl Iterator<Item = Currency> {
        (0..CURRENCIES.len() as u8).map(Currency)
    }
    pub fn code(self) -> &'static str {
        CURRENCIES[self.0 as usize].0
    }
    pub fn symbol(self) -> &'static str {
        CURRENCIES[self.0 as usize].1
    }
    pub fn name(self) -> &'static str {
        CURRENCIES[self.0 as usize].2
    }
    /// `KYD — Cayman Islands dollar`, for pickers.
    pub fn label(self) -> String {
        format!("{} — {}", self.code(), self.name())
    }
    /// A currency's code or symbol (`GBP`, `£`, `CI$`, `US`, any case).
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().trim_end_matches(['(', ')', ',', ':']).to_uppercase();
        let code = ALIASES.iter().find(|(a, _)| *a == t).map_or(t.as_str(), |(_, c)| c);
        CURRENCIES.iter().position(|(c, _, _)| *c == code).map(|i| Currency(i as u8))
    }
    /// Pegged to the US dollar at a fixed rate (no market rate needed).
    pub fn pegged(self) -> bool {
        self == Currency::KYD || self == Currency::USD
    }
}

impl Serialize for Currency {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Currency::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown currency {s}")))
    }
}

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Exchange rates: the Cayman peg (1 KYD = 1.20 USD) and, for other currencies, how many units of each buy one
/// US dollar on each day (downloaded and kept in the ledger). Every conversion goes through the US dollar.
#[derive(Debug, Clone)]
pub struct Fx {
    pub kyd_to_usd: Decimal,
    pub usd_to_kyd: Decimal,
    /// Units of a currency per US dollar, by day.
    pub daily: std::collections::BTreeMap<Currency, std::collections::BTreeMap<NaiveDate, Decimal>>,
}

impl Default for Fx {
    fn default() -> Self {
        Fx { kyd_to_usd: Decimal::new(120, 2), usd_to_kyd: Decimal::new(83333333, 8), daily: Default::default() }
    }
}

impl Fx {
    /// Units of `c` per US dollar on `day` (the latest known on or before it; or the earliest known, for a day
    /// before any), or today (`None`: the latest). `None` when nothing is known for `c`.
    pub fn per_usd(&self, c: Currency, day: Option<NaiveDate>) -> Option<Decimal> {
        match c {
            Currency::USD => Some(Decimal::ONE),
            Currency::KYD => Some(self.usd_to_kyd),
            _ => {
                let rates = self.daily.get(&c)?;
                match day {
                    Some(d) => rates.range(..=d).next_back().or_else(|| rates.iter().next()).map(|(_, r)| *r),
                    None => rates.values().next_back().copied(),
                }
            }
        }
    }
    /// Whether any rate is known for `c` (always, for the pegged ones).
    pub fn knows(&self, c: Currency) -> bool {
        self.per_usd(c, None).is_some()
    }
    /// How many units of `to` one unit of `from` buys on `day` (`None`: today). `None` when either is unknown.
    pub fn rate_on(&self, from: Currency, to: Currency, day: Option<NaiveDate>) -> Option<Decimal> {
        if from == to {
            return Some(Decimal::ONE);
        }
        match (from, to) {
            (Currency::KYD, Currency::USD) => Some(self.kyd_to_usd),
            (Currency::USD, Currency::KYD) => Some(self.usd_to_kyd),
            _ => {
                let (f, t) = (self.per_usd(from, day)?, self.per_usd(to, day)?);
                (!f.is_zero()).then(|| t / f)
            }
        }
    }
    /// Today's rate; 1 when a currency's rate isn't known yet (the amount stays as printed until it is).
    pub fn rate(&self, from: Currency, to: Currency) -> Decimal {
        self.rate_on(from, to, None).unwrap_or(Decimal::ONE)
    }
    /// Converted at today's rate and rounded to cents (half up, as statements round).
    pub fn convert(&self, amount: Decimal, from: Currency, to: Currency) -> Decimal {
        self.convert_on(amount, from, to, None)
    }
    /// Converted at `day`'s rate (the latest known on or before it) and rounded to cents.
    pub fn convert_on(&self, amount: Decimal, from: Currency, to: Currency, day: Option<NaiveDate>) -> Decimal {
        if from == to {
            return amount;
        }
        round2(amount * self.rate_on(from, to, day).unwrap_or(Decimal::ONE))
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

/// How a document writes its decimals: `1,234.56` (point) or `1.234,56` (comma).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Decimals {
    #[default]
    Point,
    Comma,
}

thread_local! {
    // ponytail: the document being scanned sets this for its amounts (`with_decimals`) instead of every
    // amount helper taking a format argument; it's per thread and put back afterwards
    static DECIMALS: std::cell::Cell<Decimals> = const { std::cell::Cell::new(Decimals::Point) };
}

/// Run `f` reading amounts the way `d` says.
pub fn with_decimals<T>(d: Decimals, f: impl FnOnce() -> T) -> T {
    let old = DECIMALS.replace(d);
    let out = f();
    DECIMALS.set(old);
    out
}

/// `1.234,56` → `1,234.56` when the document uses decimal commas: then one parser serves both.
fn as_point(s: &str) -> std::borrow::Cow<'_, str> {
    if DECIMALS.get() == Decimals::Comma {
        s.chars().map(|c| match c { '.' => ',', ',' => '.', c => c }).collect::<String>().into()
    } else {
        s.into()
    }
}

/// Which decimal style a document's amounts use, from the ones that can't be anything else: three or more
/// digits before the separator, or a thousands group (`15.320,00`, `1,200.00`).
pub fn infer_decimals<'a>(words: impl Iterator<Item = &'a str>) -> Decimals {
    let (mut point, mut comma) = (0, 0);
    for w in words {
        let w = w.trim_matches(|c: char| !c.is_ascii_digit());
        let Some(i) = w.rfind(['.', ',']) else { continue };
        let (int, frac) = (&w[..i], &w[i + 1..]);
        if frac.len() != 2 || !frac.bytes().all(|b| b.is_ascii_digit()) || !int.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b',') {
            continue;
        }
        let digits = int.bytes().filter(u8::is_ascii_digit).count();
        let grouped = int.contains(['.', ',']);
        if digits < 3 && !grouped {
            continue;
        }
        if w.as_bytes()[i] == b',' { comma += 1 } else { point += 1 }
    }
    if comma > point { Decimals::Comma } else { Decimals::Point }
}

/// Is this one word the numeric core of an amount (`1,234.56`, `$-3,000.00`, `(12.00)`, `5.00CR`)?
pub fn is_amount_core(word: &str) -> bool {
    let word = as_point(word);
    let w = word.trim_start_matches(['(', '-', '+']);
    let w = w.strip_prefix("CI$").or_else(|| w.strip_prefix("US$")).or_else(|| w.strip_prefix('$')).or_else(|| w.strip_prefix('€')).or_else(|| w.strip_prefix('£')).unwrap_or(w);
    let w = w.trim_start_matches(['-', '+']);
    let w = w.trim_end_matches(')');
    let w = w.strip_suffix("CR").or_else(|| w.strip_suffix("DR")).or_else(|| w.strip_suffix("Cr")).or_else(|| w.strip_suffix("Dr")).unwrap_or(w);
    let w = w.trim_end_matches(['-', '+', ')', '€', '£', '$']);
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
    matches!(word, "-" | "+" | "(" | ")" | "CR" | "DR" | "Cr" | "Dr" | "cr" | "dr" | "$" | "€" | "£") || Currency::parse(word).is_some()
}

/// Parse an amount from its words joined by spaces (`CI$ 1,685.00 -`).
pub fn parse_printed(text: &str) -> Option<Printed> {
    let t = as_point(text);
    let t = t.trim();
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
    // nothing anyone types is near a trillion; bigger would overflow Decimal once multiplied
    if d > Decimal::from(1_000_000_000_000i64) {
        return None;
    }
    Some(if neg { -d } else { d })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes, symbols and old spellings all name their currency; words that happen to be currency codes elsewhere
    /// (TOP, ALL) aren't currencies here; the code is what's stored, as it always was.
    #[test]
    fn currencies_by_code_and_symbol() {
        let c = |s: &str| Currency::parse(s).map(Currency::code);
        assert_eq!((c("KYD"), c("CI$"), c("us$"), c("USD")), (Some("KYD"), Some("KYD"), Some("USD"), Some("USD")));
        assert_eq!((c("GBP"), c("£"), c("C$"), c("J$"), c("₱"), c("gbp")), (Some("GBP"), Some("GBP"), Some("CAD"), Some("JMD"), Some("PHP"), Some("GBP")));
        assert_eq!((c("TOP"), c("ALL"), c("PEN"), c("XYZ")), (None, None, None, None));
        assert_eq!(serde_json::to_string(&Currency::KYD).unwrap(), "\"KYD\"");
        assert_eq!(serde_json::from_str::<Currency>("\"GBP\"").unwrap().code(), "GBP");
        assert!(Currency::KYD < Currency::USD && Currency::all().next() == Some(Currency::KYD));
    }

    /// Other currencies go through the US dollar at the day's rate (the latest kept on or before it); KYD↔USD stays
    /// on the peg; a currency with no rate converts one for one.
    #[test]
    fn dated_rates_through_the_dollar() {
        let gbp = Currency::parse("GBP").unwrap();
        let day = |m: u32, d: u32| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        let mut fx = Fx::default();
        fx.daily.entry(gbp).or_default().insert(day(3, 2), d("0.80"));
        fx.daily.entry(gbp).or_default().insert(day(3, 9), d("0.75"));
        assert_eq!(fx.convert_on(d("100.00"), gbp, Currency::USD, Some(day(3, 5))), d("125.00"), "the 2nd's rate, on the 5th");
        assert_eq!(fx.convert_on(d("100.00"), gbp, Currency::USD, Some(day(3, 9))), d("133.33"));
        assert_eq!(fx.convert_on(d("100.00"), gbp, Currency::KYD, Some(day(3, 5))), d("104.17"), "via the dollar, then the peg");
        assert_eq!(fx.convert_on(d("100.00"), gbp, Currency::USD, Some(day(1, 1))), d("125.00"), "before any rate: the earliest");
        assert_eq!(fx.convert(d("120.00"), Currency::USD, Currency::KYD), d("100.00"));
        let cad = Currency::parse("CAD").unwrap();
        assert!(!fx.knows(cad) && fx.convert(d("50.00"), cad, Currency::KYD) == d("50.00"));
    }

    #[test]
    fn typed_amounts_are_bounded() {
        assert_eq!(parse_user("1,234.50"), Some(Decimal::new(123450, 2)));
        assert_eq!(parse_user("99999999999999999999"), None);
    }

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
