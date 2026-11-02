//! Merchant stems, learned rules and Smart Triage grouping. Deterministic: the same ledger always
//! gives the same groups, suggestions and rule matches.

use chrono::NaiveDate;
use regex::Regex;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static FUZZY: AtomicBool = AtomicBool::new(true);

/// Whether truncated spellings count as one merchant (a preference; on by default).
pub fn set_fuzzy(on: bool) {
    FUZZY.store(on, Ordering::Relaxed);
}

use crate::Currency;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleKind {
    /// Every word of the pattern appears in the merchant stem (the wizard's default).
    #[default]
    NormalizedTokens,
    Substring,
    Regex,
    Exact,
}

impl RuleKind {
    pub const ALL: [RuleKind; 4] = [Self::NormalizedTokens, Self::Substring, Self::Regex, Self::Exact];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NormalizedTokens => "NormalizedTokens",
            Self::Substring => "Substring",
            Self::Regex => "Regex",
            Self::Exact => "Exact",
        }
    }
    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|k| k.as_str() == s).unwrap_or(Self::NormalizedTokens)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub kind: RuleKind,
    pub pattern: String,
    pub clean_payee: Option<String>,
    pub category_id: Option<i64>,
    pub is_subscription: bool,
    pub is_transfer: bool,
    /// Days between charges (30 = monthly), for subscriptions.
    pub expected_cadence: Option<i64>,
    /// `None` = every workspace.
    pub entity_id: Option<i64>,
    /// Matching payments are business expenses of this workspace.
    #[serde(default)]
    pub business_entity_id: Option<i64>,
    /// Only when the amount (either sign) is at least / at most this: "Adriana, CI$ 125.00 only".
    #[serde(default)]
    pub amount_min: Option<Decimal>,
    #[serde(default)]
    pub amount_max: Option<Decimal>,
}

impl Rule {
    /// Inside the rule's amount range (no range = any amount).
    pub fn amount_ok(&self, amount: Decimal) -> bool {
        let a = amount.abs();
        self.amount_min.is_none_or(|m| a >= m) && self.amount_max.is_none_or(|m| a <= m)
    }

    /// Text and amount both match.
    pub fn applies(&self, raw: &str, memo: &str, amount: Decimal) -> bool {
        self.amount_ok(amount) && self.matches(raw, memo)
    }

    pub fn matches(&self, raw: &str, memo: &str) -> bool {
        match self.kind {
            RuleKind::NormalizedTokens => {
                let stem = merchant_stem(raw, memo);
                let pat = merchant_stem(&self.pattern, "");
                if pat.is_empty() {
                    return false;
                }
                // the same well-known service however the bank printed it (HBO MAX / PAYPAL *HBOMAX)
                if FUZZY.load(Ordering::Relaxed) && known_key(&pat).is_some() && known_key(&pat) == known_key(&stem) {
                    return true;
                }
                // every pattern word appears, allowing for names the bank cut short (HELP / HEL / HE)
                let words: Vec<&str> = stem.split(' ').collect();
                pat.split(' ').enumerate().all(|(i, p)| words.iter().any(|w| word_match(p, w, i == 0)))
            }
            RuleKind::Substring => {
                let hay = format!("{raw} {memo}").to_uppercase();
                hay.contains(&self.pattern.to_uppercase())
            }
            RuleKind::Regex => Regex::new(&self.pattern).is_ok_and(|r| r.is_match(&format!("{raw} {memo}"))),
            RuleKind::Exact => raw.trim().eq_ignore_ascii_case(self.pattern.trim()),
        }
    }
}

/// Two words name the same thing: equal, or one is the other cut short. The first word of a name
/// must share at least 5 letters, so "KIRK MARKET" never matches "KIRK HOME CENTRE" this way.
fn word_match(a: &str, b: &str, first: bool) -> bool {
    if a == b {
        return true;
    }
    if !FUZZY.load(Ordering::Relaxed) {
        return false;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= if first { 5 } else { 2 } && long.starts_with(short)
}

/// Same merchant, as printed on two different statements: word by word, allowing truncation
/// (`DISCOVERYDI HELP` = `DISCOVERYDI HEL` = `DISCOVERYDI HE`), or the same well-known service.
pub fn same_merchant(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if !FUZZY.load(Ordering::Relaxed) {
        return false;
    }
    if known_key(a).is_some() && known_key(a) == known_key(b) {
        return true;
    }
    let (wa, wb): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
    let n = wa.len().min(wb.len());
    // one word never swallows a longer name: KIRK isn't KIRK OFFICE EQUIPMEN (the directory still joins known ones)
    let lone = n == 1 && wa.len() != wb.len();
    n > 0 && !lone && wa[0].len() >= 3 && (0..n).all(|i| word_match(wa[i], wb[i], i == 0))
}

/// Which well-known merchant a stem names, if any. Short keys must be whole words ("FLOW" ≠ "FLOWERS").
fn known_key(stem: &str) -> Option<&'static str> {
    crate::merchants::lookup(stem).map(|m| m.name.as_str())
}

pub fn regex_ok(pattern: &str) -> bool {
    Regex::new(pattern).is_ok()
}

/// First matching rule; workspace-specific rules beat global ones, newer beat older.
pub fn first_match<'a>(rules: &'a [Rule], entity_id: i64, raw: &str, memo: &str, amount: Decimal) -> Option<&'a Rule> {
    let scoped = rules.iter().rev().filter(|r| r.entity_id == Some(entity_id)).find(|r| r.applies(raw, memo, amount));
    scoped.or_else(|| rules.iter().rev().filter(|r| r.entity_id.is_none()).find(|r| r.applies(raw, memo, amount)))
}

/// Bank transaction-type labels that say nothing about the merchant (Scotiabank prints the merchant
/// on the next line).
static GENERIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(pos (purchase|debit)|visa (purchase|debit)|debit card purchase|electronic (credit|debit)|mobile ob trf (dr|cr)|pc transfer (debit|credit)|abm withdrawal|atm withdrawal|bill payment|internet banking (payment|transfer)|point of sale|third party (trf|transfer)( bns)?)(\s*#\s*error)?$").unwrap()
});

/// What a bank prints for every cheque, deposit or cash withdrawal, whoever it was to or from: one
/// answer for the whole stem would file every future cheque under the same category.
static VARIED_PAYEE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(cheques?|checks?|cheque paid|cheque deposit|branch deposit|cash deposit|counter deposit|mobile deposit|deposits?|cash withdrawal|counter withdrawal|withdrawal|abm withdrawal|atm withdrawal|atm|abm)$").unwrap()
});

/// A stem that stands for many different payees (`CHEQUE`, `BRANCH DEPOSIT`), so each charge needs its own answer.
pub fn varied_payee(stem: &str) -> bool {
    VARIED_PAYEE.is_match(stem.trim())
}

static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(pos (purchase|debit)|visa (purchase|debit)|debit card purchase|point of sale|ath|sq \*|sq\*|tst\*|tst \*|pp\*|paypal \*|paypal\*|sp |trf to:|trf from:|transfer to|transfer from|bill payment|web pmt|purchase)\s*").unwrap()
});

static GLUED_CCY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([A-Z]{3,})\d(USD|KYD)\b").unwrap());

static NOISE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\([^)]*\)",                                              // FX notes "(59.14 KYD)"
        r"(?i)\b(usd|kyd|us\$|ci\$)\s*\d[\d,]*\.\d{2}(\s*@\s*\d+\.\d+)?", // "USD 145.00 @ 0.84"
        r"(?i)\bbank of butterfield.*|cayman national bank.*|royal bank of canada.*|scotiabank.*$",
        r"(?i),.*$",                                                // ", GRAND CAYMAN" location suffix
        r"#\s*\d+",                                                 // store numbers
        r"\*[A-Z0-9]{5,}",                                          // AMAZON MKTPL*2K4TY
        r"\b\d{2}[/-]\d{2}([/-]\d{2,4})?\b",                         // dates
        r"\b[A-Z]*\d[A-Z0-9]{3,}\b",                                // refs, card numbers, phone numbers
        r"\b\d+\b",
        r"(?i)\b(grand cayman|george town|georgetown|west bay|camana bay|ky|us|gb|cayman islands?|islnds)\b\s*$",
        r"(?i)\b(web pmt|thank you|ltd|limited|inc|llc|co)\b",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// Who a payment is from or to, ignoring reference numbers: a bank that prints a new reference on
/// every deposit ("100004512, HARBOUR FREIGHT DIR PAY") still reads as one payer. For spotting rhythms
/// (pay, bills); rules keep matching on `merchant_stem`.
pub fn payer_key(raw: &str, memo: &str) -> String {
    let stem = merchant_stem(raw, memo);
    let kept: Vec<&str> = stem.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty() && !w.chars().any(|c| c.is_ascii_digit())).collect();
    if kept.is_empty() { stem } else { kept.join(" ") }
}

/// `POS PURCHASE` / `FOSTER'S FOOD FAIR #12 CAMANA BAY` → `FOSTER'S FOOD FAIR`. Upper-case words, single-spaced.
pub fn merchant_stem(raw: &str, memo: &str) -> String {
    let raw = raw.trim();
    let first_memo = memo.split(" | ").next().unwrap_or("").trim();
    let base = if (GENERIC.is_match(raw) || raw.is_empty()) && !first_memo.is_empty() && !first_memo.starts_with("Ref ") { first_memo } else { raw };
    let mut s = base.to_uppercase();
    for _ in 0..2 {
        s = PREFIX.replace(&s, "").to_string();
    }
    // KIRK MOTORS1USD: the card's currency glued to the name, which the reference filter would take with it
    s = GLUED_CCY.replace_all(&s, "$1").to_string();
    for re in NOISE.iter() {
        s = re.replace_all(&s, " ").to_string();
    }
    // trailing location words can repeat ("CAMANA BAY GRAND CAYMAN")
    for _ in 0..3 {
        s = NOISE[9].replace_all(s.trim(), "").to_string();
    }
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == '*' || c == '/' || c == '-').map(|w| w.trim_matches(|c: char| c == '.' || c == ',' || c == ':')).filter(|w| w.chars().any(char::is_alphanumeric)).collect();
    let stem = words.iter().take(4).copied().collect::<Vec<_>>().join(" ");
    if stem.is_empty() { base.to_uppercase() } else { stem }
}

/// `FOSTER'S FOOD FAIR` → `Foster's Food Fair`; short all-caps tokens (CUC, HBO, RBC) stay caps.
pub fn title_case(stem: &str) -> String {
    stem.split(' ')
        .map(|w| {
            let acronym = !w.contains(['A', 'E', 'I', 'O', 'U']) || matches!(w, "CUC" | "HBO" | "USD" | "KYD" | "ABM" | "ATM" | "POS" | "AIG" | "UK" | "US");
            if w.len() <= 3 && w.chars().all(|c| c.is_ascii_uppercase()) && acronym {
                return w.to_string();
            }
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}


static SUBSCRIPTION_HINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(hbo|netflix|spotify|youtube|google one|expressvpn|adobe|audible|hulu|disney|apple\.com|icloud|prime video|anthropic|openai|dropbox|microsoft|office 365|signal|yahoo mail)").unwrap());

static TRANSFER_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(transfer (to|from)|trf (to|from)|pc transfer|card payment|payment received|visa pmt|mastercard pmt|loan payment|thank you|own account|internal transfer)\b").unwrap()
});

/// Regular, but never a subscription.
static NOT_SUBSCRIPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(cheque|check|abm|atm|withdrawal|cash|interest|loan|rent|salary|payroll|deposit|dividend)\b").unwrap());

static FEE_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(service charge|monthly fee|account fee|wire fee|trf fee|transaction fee|foreign transaction fee|fx fee|overlimit fee|late payment fee|stamp duty|debit interest|interest charge|loan interest|record keeping)\b").unwrap()
});

pub fn is_bank_fee(raw: &str, memo: &str) -> bool {
    FEE_HINT.is_match(raw) || FEE_HINT.is_match(memo)
}

static CARD_PAYMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(card payment|card pmt|credit card (payment|pmt)|visa (payment|pmt)|mastercard (payment|pmt)|amex (payment|pmt)|american express (payment|pmt)|cc (payment|pmt)|payment received|payment - thank you|payment thank you|autopay payment|online payment thank you)\b").unwrap()
});

/// Wording that says a credit card is being paid (or received a payment).
pub fn is_card_payment(text: &str) -> bool {
    CARD_PAYMENT.is_match(text)
}

pub fn looks_like_transfer(raw: &str, memo: &str) -> bool {
    TRANSFER_HINT.is_match(raw) || TRANSFER_HINT.is_match(memo)
}

/// A friendlier payee and a category suggestion (by name) for a stem.
pub fn known_merchant(stem: &str, raw: &str) -> (String, Option<&'static str>) {
    let hay = format!("{stem} {raw}");
    // the directory names it (a confident match) ...
    if let Some(m) = crate::merchants::lookup(&hay) {
        return (m.name.clone(), Some(m.category.as_str()));
    }
    if FEE_HINT.is_match(&hay) {
        return (title_case(stem), Some("Bank Fees & Interest"));
    }
    // ... or a word in its name suggests one (MARKET → Groceries, DIGITAL → Subscriptions & Digital)
    (title_case(stem), crate::merchants::keyword_category(stem))
}

/// A category FinGraph is sure enough of to file on its own: a directory match, or (when allowed)
/// a keyword guess. Returns the category name and the merchant's proper name when known.
pub fn auto_category(raw: &str, memo: &str, keywords_too: bool) -> Option<(&'static str, Option<&'static str>)> {
    let stem = merchant_stem(raw, memo);
    let hay = format!("{stem} {raw}");
    if let Some(m) = crate::merchants::lookup(&hay) {
        return Some((m.category.as_str(), Some(m.name.as_str())));
    }
    keywords_too.then(|| crate::merchants::keyword_category(&stem)).flatten().map(|c| (c, None))
}

/// Charges this regular (median gap 6–8, 13–16, 26–35 or 350–380 days, amounts within 25%) are recurring.
pub fn cadence(dates: &[NaiveDate]) -> Option<i64> {
    if dates.len() < 2 {
        return None;
    }
    let mut ds = dates.to_vec();
    ds.sort();
    let mut gaps: Vec<i64> = ds.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
    gaps.sort();
    let med = gaps[gaps.len() / 2];
    [(6, 8, 7), (13, 16, 14), (26, 35, 30), (85, 95, 91), (350, 380, 365)].iter().find(|(lo, hi, _)| med >= *lo && med <= *hi).map(|x| x.2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriageKind {
    Subscription,
    Transfer,
    Merchant,
}

/// What the wizard needs about one uncategorised transaction.
#[derive(Debug, Clone)]
pub struct TriageInput {
    pub id: i64,
    pub entity_id: i64,
    pub posted: NaiveDate,
    pub raw: String,
    pub memo: String,
    pub amount: Decimal,
    pub currency: Currency,
    pub account: String,
}

#[derive(Debug, Clone)]
pub struct TriageGroup {
    pub stem: String,
    pub suggested_payee: String,
    pub suggested_category: Option<&'static str>,
    /// What your own ledger suggests (category names, likeliest first; see `learn`), besides
    /// `suggested_category`.
    pub guesses: Vec<String>,
    pub kind: TriageKind,
    pub tx_ids: Vec<i64>,
    pub entity_ids: Vec<i64>,
    pub count: usize,
    /// Per currency, so a KYD and a USD charge are never added together.
    pub totals: BTreeMap<Currency, Decimal>,
    pub typical_amount: Decimal,
    /// Every charge was for exactly the same amount.
    pub same_amount: bool,
    pub currency: Currency,
    pub cadence: Option<i64>,
    pub sample: String,
    /// Every charge in the group, newest first, so you can tell which payment is which.
    pub charges: Vec<TriageInput>,
}

/// Group uncategorised transactions by merchant stem; the biggest groups first.
pub fn triage_groups(items: &[TriageInput]) -> Vec<TriageGroup> {
    let mut exact: BTreeMap<String, Vec<&TriageInput>> = BTreeMap::new();
    for it in items {
        exact.entry(merchant_stem(&it.raw, &it.memo)).or_default().push(it);
    }
    // one card per merchant, however each statement spelled it; the commonest spelling names it
    let mut clusters: Vec<Vec<(String, Vec<&TriageInput>)>> = Vec::new();
    for (stem, v) in exact {
        match clusters.iter_mut().find(|c| c.iter().any(|(s, _)| same_merchant(s, &stem))) {
            Some(c) => c.push((stem, v)),
            None => clusters.push(vec![(stem, v)]),
        }
    }
    let by: BTreeMap<String, Vec<&TriageInput>> = clusters
        .into_iter()
        .map(|mut c| {
            c.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(b.0.len().cmp(&a.0.len())));
            let name = c[0].0.clone();
            (name, c.into_iter().flat_map(|(_, v)| v).collect())
        })
        .collect();
    let mut out: Vec<TriageGroup> = by
        .into_iter()
        .map(|(stem, v)| {
            let (payee, cat) = known_merchant(&stem, &v[0].raw);
            let dates: Vec<NaiveDate> = v.iter().map(|t| t.posted).collect();
            let cad = cadence(&dates);
            let mut totals: BTreeMap<Currency, Decimal> = BTreeMap::new();
            for t in &v {
                *totals.entry(t.currency).or_default() += t.amount;
            }
            let latest = v.iter().max_by_key(|t| t.posted).unwrap();
            let all_out = v.iter().all(|t| t.amount.is_sign_negative());
            // a known service, or the very same price on a steady schedule (bills and fees vary or say so)
            let same_price = v.windows(2).all(|w| w[0].amount == w[1].amount);
            let excluded = v.iter().any(|t| NOT_SUBSCRIPTION.is_match(&format!("{} {}", t.raw, t.memo)) || is_bank_fee(&t.raw, &t.memo));
            let kind = if v.iter().any(|t| looks_like_transfer(&t.raw, &t.memo)) {
                TriageKind::Transfer
            } else if all_out && !excluded && (SUBSCRIPTION_HINT.is_match(&stem) || cat == Some("Subscriptions & Digital") || (cad.is_some() && same_price && v.len() >= 3 && cat != Some("Utilities"))) {
                TriageKind::Subscription
            } else {
                TriageKind::Merchant
            };
            let mut entity_ids: Vec<i64> = v.iter().map(|t| t.entity_id).collect();
            entity_ids.sort();
            entity_ids.dedup();
            TriageGroup {
                suggested_payee: payee,
                // recurring isn't a category: a monthly landscaper is still Repairs & Maintenance, so only
                // a known digital service defaults to Subscriptions & Digital
                suggested_category: cat.or_else(|| SUBSCRIPTION_HINT.is_match(&stem).then_some("Subscriptions & Digital")),
                guesses: Vec::new(),
                kind,
                tx_ids: v.iter().map(|t| t.id).collect(),
                entity_ids,
                count: v.len(),
                totals,
                typical_amount: latest.amount,
                same_amount: v.windows(2).all(|w| w[0].amount == w[1].amount && w[0].currency == w[1].currency),
                currency: latest.currency,
                cadence: cad,
                sample: if latest.memo.is_empty() { latest.raw.clone() } else { format!("{} — {}", latest.raw, latest.memo) },
                charges: {
                    let mut c: Vec<TriageInput> = v.iter().map(|t| (*t).clone()).collect();
                    c.sort_by(|a, b| b.posted.cmp(&a.posted).then(b.id.cmp(&a.id)));
                    c
                },
                stem,
            }
        })
        .collect();
    // the questions that matter most first: subscriptions, then transfers, then everything by size
    let rank = |k: TriageKind| match k {
        TriageKind::Subscription => 0,
        TriageKind::Transfer => 1,
        TriageKind::Merchant => 2,
    };
    out.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then(b.count.cmp(&a.count)).then(a.stem.cmp(&b.stem)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems() {
        assert_eq!(merchant_stem("POS PURCHASE", "FOSTER'S FOOD FAIR #12 CAMANA BAY"), "FOSTER'S FOOD FAIR");
        assert_eq!(merchant_stem("POS PURCHASE", "FOSTER'S FOOD FAIR #03 AIRPORT"), "FOSTER'S FOOD FAIR AIRPORT");
        assert_eq!(merchant_stem("HBO MAX HBOMAX.COM 8442", ""), "HBO MAX HBOMAX.COM");
        assert_eq!(merchant_stem("PAYPAL *HBOMAX HELP.HBOMA4029357733 US", ""), "HBOMAX HELP");
        assert_eq!(merchant_stem("SQ *CRACKED CONCH WEST BAY", ""), "CRACKED CONCH");
        assert_eq!(merchant_stem("CUC WEB PMT", "CARIBBEAN UTILITIES CO"), "CUC");
        assert_eq!(merchant_stem("MOBILE OB TRF DR", "TRF TO: ACME APPLIANCE SERVIC BANK OF BUTTERFIELD INTL"), "ACME APPLIANCE SERVIC");
        assert_eq!(merchant_stem("KIRK MARKET/BACK-UP 1, GEORGE TOWN", "Ref 1234567890"), "KIRK MARKET BACK UP");
        assert_eq!(title_case("FOSTER'S FOOD FAIR"), "Foster's Food Fair");
        assert_eq!(known_merchant("CUC", "CUC WEB PMT").0, "Caribbean Utilities Company");
    }

    #[test]
    fn truncated_names_are_one_merchant() {
        let stem = |m: &str| merchant_stem("POS PURCHASE", m);
        let (a, b, c) = (stem("PAYPAL *DISCOVERYDI HELP.4029357733 US"), stem("PAYPAL *DISCOVERYDI HEL 4029357733 US"), merchant_stem("PAYPAL *DISCOVERYDI HE, 4029357733 (9.90 KYD)", ""));
        assert!(same_merchant(&a, &b) && same_merchant(&a, &c) && same_merchant(&b, &c), "{a} / {b} / {c}");
        assert!(same_merchant("HBO MAX HBOMAX.COM", "HBOMAX HELP"));
        assert!(!same_merchant("KIRK MARKET", "KIRK HOME CENTRE"));
        assert!(!same_merchant("FLOW", "FLOWERS BY JANE"));
        // "KIRK" is half of Cayman's shops: one word never swallows a longer name
        assert!(!same_merchant("KIRK", "KIRK OFFICE EQUIPMEN"));
        // Scotiabank glues the card currency onto the name: KIRK MOTORS1USD (Napa) isn't Kirk Office Equipment
        assert_eq!(merchant_stem("KIRK MOTORS1USD, GEORGE TOWN", "Ref 1738069811"), "KIRK MOTORS");
        assert!(!same_merchant(&merchant_stem("KIRK MOTORS1USD, GEORGE TOWN", "Ref 1738069811"), &merchant_stem("KIRK OFFICE EQUIPMEN, GRAND CAYMAN", "Ref 1654528427")));
        // a rule learned from one spelling covers the others
        let r = Rule { pattern: c.clone(), category_id: Some(17), ..Default::default() };
        assert!(r.matches("POS PURCHASE", "PAYPAL *DISCOVERYDI HELP.4029357733 US"));
        assert!(r.matches("POS PURCHASE", "PAYPAL *DISCOVERYDI HEL 4029357733 US"));
        // a rule for one Amazon service doesn't capture the others
        let pv = Rule { pattern: "PRIME VIDEO CHANNELS".into(), ..Default::default() };
        assert!(!pv.matches("AMAZON DIGITAL SVCS, AMZN.COM/BILL", ""));
        assert!(!pv.matches("AUDIBLE*A12B34567, AMZN.COM/BILL", ""));
        assert!(!r.matches("POS PURCHASE", "KIRK MARKET"));
        // and triage asks once
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let t = |id, date, memo: &str| TriageInput { id, entity_id: 1, posted: date, raw: "POS PURCHASE".into(), memo: memo.into(), amount: "-9.99".parse().unwrap(), currency: Currency::KYD, account: "Chq".into() };
        let g = triage_groups(&[t(1, d(5, 28), "PAYPAL *DISCOVERYDI HELP.4029357733 US"), t(2, d(7, 28), "PAYPAL *DISCOVERYDI HELP.4029357733 US"), t(3, d(8, 31), "PAYPAL *DISCOVERYDI HEL 4029357733 US")]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].count, 3);
        assert_eq!(g[0].suggested_payee, "Discovery+");
        assert_eq!(payer_key("ELECTRONIC CREDIT", "100004512, Harbour Freight Dir Pay Cr"), payer_key("ELECTRONIC CREDIT", "100004513, Harbour Freight Dir Pay Cr"));
        assert_eq!(g[0].kind, TriageKind::Subscription);
    }

    #[test]
    fn rules_match_on_stems() {
        let r = Rule { id: 1, kind: RuleKind::NormalizedTokens, pattern: "FOSTER'S FOOD FAIR".into(), clean_payee: None, category_id: Some(3), is_subscription: false, is_transfer: false, expected_cadence: None, entity_id: None, business_entity_id: None, ..Default::default() };
        assert!(r.matches("POS PURCHASE", "FOSTER'S FOOD FAIR #12 CAMANA BAY"));
        assert!(!r.matches("POS PURCHASE", "KIRK MARKET"));
    }

    #[test]
    fn triage_finds_subscriptions_and_transfers() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let t = |id, date, raw: &str, amt: &str| TriageInput { id, entity_id: 1, posted: date, raw: raw.into(), memo: String::new(), amount: amt.parse().unwrap(), currency: Currency::USD, account: "Visa".into() };
        let g = triage_groups(&[
            t(5, d(6, 1), "ABM WITHDRAWAL", "-100.00"),
            t(6, d(7, 1), "ABM WITHDRAWAL", "-100.00"),
            t(7, d(8, 1), "ABM WITHDRAWAL", "-100.00"),
            t(1, d(6, 3), "HBO MAX HBOMAX.COM 8442", "-15.99"),
            t(2, d(7, 3), "HBO MAX HBOMAX.COM 8442", "-15.99"),
            t(3, d(8, 3), "HBO MAX HBOMAX.COM 8442", "-17.99"),
            t(4, d(8, 9), "PAYMENT RECEIVED - THANK YOU", "500.00"),
        ]);
        assert_eq!(g[0].kind, TriageKind::Subscription);
        assert_eq!(g[0].count, 3);
        assert_eq!(g[0].cadence, Some(30));
        assert_eq!(g[0].suggested_payee, "HBO Max");
        assert_eq!(g[0].charges.iter().map(|c| c.posted).collect::<Vec<_>>(), vec![d(8, 3), d(7, 3), d(6, 3)], "newest first");
        assert_eq!(g[1].kind, TriageKind::Transfer);
        assert_eq!(g[2].kind, TriageKind::Merchant, "a monthly cash withdrawal is not a subscription");
        assert_eq!(g[0].suggested_category, Some("Subscriptions & Digital"));
    }

    #[test]
    fn recurring_is_not_a_category() {
        let d = |m| NaiveDate::from_ymd_opt(2026, m, 12).unwrap();
        let t = |id, m| TriageInput { id, entity_id: 1, posted: d(m), raw: "GENERAL LANDSCAPING LTD".into(), memo: String::new(), amount: "-275.00".parse().unwrap(), currency: Currency::KYD, account: "Chq".into() };
        let g = triage_groups(&[t(1, 6), t(2, 7), t(3, 8), t(4, 9)]);
        assert_eq!(g[0].kind, TriageKind::Subscription, "same price every month still asks");
        assert_eq!(g[0].suggested_category, Some("Landscaping"), "the directory's answer, not Subscriptions & Digital");
    }

    /// Scotiabank prints "THIRD PARTY TRF BNS" for every transfer out to someone else; who it went to
    /// is on the next line. One card per payee, not one for all of them (found in Getting Started).
    #[test]
    fn third_party_transfers_group_by_payee() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let t = |id, date, raw: &str, memo: &str, amt: &str| TriageInput { id, entity_id: 1, posted: date, raw: raw.into(), memo: memo.into(), amount: amt.parse().unwrap(), currency: Currency::KYD, account: "Chq".into() };
        let g = triage_groups(&[
            t(1, d(10, 14), "THIRD PARTY TRF BNS", "Transfer to DOUGHTARTY MOLL 0477", "-850.77"),
            t(2, d(7, 29), "THIRD PARTY TRF BNS", "Transfer to GRAND HARBOUR MEDICAL LTD. 8", "-8.03"),
            t(3, d(7, 29), "THIRD PARTY TRF BNS", "Transfer to GRAND HARBOUR MEDICAL LTD. 8", "-13.87"),
            t(4, d(7, 3), "THIRD PARTY TRF BNS", "Transfer to PAUL STOLL 7563", "-20.00"),
            t(5, d(5, 15), "THIRD PARTY TRF BNS #Error", "Transfer to DOUGHTARTY MOLL 0477", "-850.00"),
            t(6, d(3, 2), "THIRD PARTY TRF BNS", "Transfer to DOUGHTARTY MOLL 0477", "-850.00"),
        ]);
        let mut stems: Vec<(String, usize)> = g.iter().map(|g| (g.stem.clone(), g.count)).collect();
        stems.sort();
        assert_eq!(stems, vec![("DOUGHTARTY MOLL".to_string(), 3), ("GRAND HARBOUR MEDICAL".to_string(), 2), ("PAUL STOLL".to_string(), 1)]);
    }

    #[test]
    fn varied_payees() {
        for s in ["CHEQUE", "Cheque", "BRANCH DEPOSIT", "CASH WITHDRAWAL", "ATM"] {
            assert!(varied_payee(s), "{s}");
        }
        for s in ["CHEQUE BOOK FEE", "INCOMING WIRE BLUE WATER", "MONTHLY FEE", "KIRK MARKET"] {
            assert!(!varied_payee(s), "{s}");
        }
    }
}
