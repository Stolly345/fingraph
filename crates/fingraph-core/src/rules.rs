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
pub use crate::merchants::Direction;

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
                let (pk, sk) = (known_key(&pat), known_key(&stem));
                // the same well-known service however the bank printed it (HBO MAX / PAYPAL *HBOMAX)
                if FUZZY.load(Ordering::Relaxed) && pk.is_some() && pk.map(|f| f.merchant.name.as_str()) == sk.map(|f| f.merchant.name.as_str()) {
                    return true;
                }
                // a rule for one business never catches another the directory knows apart: FLOW isn't
                // FLOW TAXI, AMAZON isn't AMAZON DIGITAL (a Contains-text or pattern rule matches broadly)
                // (a rule written for a payment platform, PADDLE.NET, is about whatever it bills: left as it was)
                let platform = crate::merchants::find(&pat).is_some_and(|f| f.merchant.fallback);
                if let Some(s) = sk.filter(|s| !platform && pk.map(|f| f.merchant.name.as_str()) != Some(s.merchant.name.as_str())) {
                    let pat_words = crate::merchants::tokens(&pat);
                    if pk.is_some() || !s.alias.iter().all(|a| pat_words.contains(a)) {
                        return false;
                    }
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
    if let (Some(x), Some(y)) = (known_key(a), known_key(b))
        && x.merchant.name == y.merchant.name
    {
        return true;
    }
    let (wa, wb): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
    let n = wa.len().min(wb.len());
    // one word never swallows a longer name: BODDEN isn't BODDEN OFFICE SUPPLY (the directory still joins known ones)
    let lone = n == 1 && wa.len() != wb.len();
    n > 0 && !lone && wa[0].len() >= 3 && (0..n).all(|i| word_match(wa[i], wb[i], i == 0))
}

/// Which well-known merchant a stem names, if any. Short keys must be whole words ("FLOW" ≠ "FLOWERS"),
/// and a lone brand word the rest of the name contradicts (PARAMOUNT PLUMBING) names nobody.
fn known_key(stem: &str) -> Option<crate::merchants::Found> {
    crate::merchants::find(stem).filter(|f| f.conflict.is_none() && !f.merchant.fallback)
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

/// The paying bank's name printed after a transfer's payee ("TRF TO: ACME BANK OF BUTTERFIELD INTL"):
/// only when something comes before it, so a payment *to* the bank keeps its name.
static BANK_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s(bank of butterfield|cayman national bank|royal bank of canada|scotiabank)\b.*$").unwrap());

/// A short number that is part of a name ("7 ELEVEN", "345 VETS"), kept where a reference would be dropped.
static LEADING_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,3})\s+[A-Z]").unwrap());

/// How names were cleaned: version 1 (up to 0.2.28) kept four words and dropped every number; version 2
/// keeps six words, a leading number that is part of the name, and the bank's own name when it is the payee.
/// Saved choices keyed by the old cleaning are moved over once (`Db::migrate_merchant_keys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemVersion {
    V1,
    V2,
}

/// Who a payment is from or to, ignoring reference numbers: a bank that prints a new reference on
/// every deposit ("100004512, HARBOUR FREIGHT DIR PAY") still reads as one payer. For spotting rhythms
/// (pay, bills); rules keep matching on `merchant_stem`.
pub fn payer_key(raw: &str, memo: &str) -> String {
    payer_key_v(raw, memo, StemVersion::V2)
}

pub fn payer_key_v(raw: &str, memo: &str, v: StemVersion) -> String {
    let stem = merchant_stem_v(raw, memo, v);
    let kept: Vec<&str> = stem.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty() && !w.chars().any(|c| c.is_ascii_digit())).collect();
    if kept.is_empty() { stem } else { kept.join(" ") }
}

/// `POS PURCHASE` / `FOSTER'S FOOD FAIR #12 CAMANA BAY` → `FOSTER'S FOOD FAIR`. Upper-case words, single-spaced.
pub fn merchant_stem(raw: &str, memo: &str) -> String {
    merchant_stem_v(raw, memo, StemVersion::V2)
}

pub fn merchant_stem_v(raw: &str, memo: &str, v: StemVersion) -> String {
    let raw = raw.trim();
    let first_memo = memo.split(" | ").next().unwrap_or("").trim();
    let base = if (GENERIC.is_match(raw) || raw.is_empty()) && !first_memo.is_empty() && !first_memo.starts_with("Ref ") { first_memo } else { raw };
    let mut s = base.to_uppercase();
    for _ in 0..2 {
        s = PREFIX.replace(&s, "").to_string();
    }
    // BODDEN MOTORS1USD: the card's currency glued to the name, which the reference filter would take with it
    s = GLUED_CCY.replace_all(&s, "$1").to_string();
    let mut lead = None;
    if v == StemVersion::V2 {
        if let Some(c) = LEADING_NUMBER.captures(s.trim_start()) {
            let n = c[1].to_string();
            s = s.trim_start()[n.len()..].to_string();
            lead = Some(n);
        }
        s = BANK_SUFFIX.replace(&s, "").to_string();
    }
    for (i, re) in NOISE.iter().enumerate() {
        if i == 2 && v == StemVersion::V2 {
            continue;
        }
        s = re.replace_all(&s, " ").to_string();
    }
    // trailing location words can repeat ("CAMANA BAY GRAND CAYMAN")
    for _ in 0..3 {
        s = NOISE[9].replace_all(s.trim(), "").to_string();
    }
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == '*' || c == '/' || c == '-').map(|w| w.trim_matches(|c: char| c == '.' || c == ',' || c == ':')).filter(|w| w.chars().any(char::is_alphanumeric)).collect();
    let limit = if v == StemVersion::V1 { 4 } else { 6 };
    let words: Vec<&str> = match &lead {
        Some(n) if !words.is_empty() => std::iter::once(n.as_str()).chain(words).collect(),
        _ => words,
    };
    let stem = words.iter().take(limit).copied().collect::<Vec<_>>().join(" ");
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

static REFUND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(refunds?|refunded|rfnd|reversal|reversed|returns?|returned|chargeback|credit voucher|merchandise credit)\b").unwrap());

/// Wording that says money came back from a purchase.
pub fn is_refund(raw: &str, memo: &str) -> bool {
    REFUND.is_match(raw) || REFUND.is_match(memo)
}

/// A friendlier payee and a category suggestion (by name) for a stem, whichever way the money went.
pub fn known_merchant(stem: &str, raw: &str) -> (String, Option<&'static str>) {
    let hay = format!("{stem} {raw}");
    // the directory names it (a confident match) ...
    if let Some(f) = crate::merchants::find(&hay) {
        // ... unless the rest of the name says otherwise: PARAMOUNT PLUMBING is a plumber, not Paramount+
        if let Some(c) = f.conflict {
            return (title_case(stem), Some(c));
        }
        return (f.merchant.name.clone(), Some(f.merchant.category.as_str()));
    }
    if FEE_HINT.is_match(&hay) {
        return (title_case(stem), Some("Bank Fees & Interest"));
    }
    // ... or a word in its name suggests one (MARKET → Groceries, DIGITAL → Subscriptions & Digital)
    (title_case(stem), crate::merchants::keyword_category(stem))
}

/// `known_merchant` for money going one way: a deposit is only ever pay, interest or rent from the
/// directory, or a refund from a shop (which takes back from that shop's category); a payment never
/// lands in an income category.
pub fn known_merchant_for(stem: &str, raw: &str, memo: &str, dir: Direction) -> (String, Option<&'static str>) {
    let hay = format!("{stem} {raw}");
    match dir {
        Direction::Out => match crate::merchants::find_for(&hay, Direction::Out) {
            Some(f) if f.conflict.is_none() => (f.merchant.name.clone(), Some(f.merchant.category.as_str())),
            Some(f) => (title_case(stem), f.conflict),
            None if FEE_HINT.is_match(&hay) => (title_case(stem), Some("Bank Fees & Interest")),
            None => (title_case(stem), crate::merchants::keyword_category(stem)),
        },
        Direction::In => {
            if let Some(f) = crate::merchants::find_for(&hay, Direction::In) {
                return (f.merchant.name.clone(), Some(f.merchant.category.as_str()));
            }
            match crate::merchants::find_for(&hay, Direction::Out) {
                Some(f) if f.conflict.is_none() => (f.merchant.name.clone(), is_refund(raw, memo).then_some(f.merchant.category.as_str())),
                _ => (title_case(stem), None),
            }
        }
    }
}

/// A category FinGraph is sure enough of to file on its own: a directory match that fits the money's
/// direction and that nothing in the name contradicts, or (when allowed) an uncontested keyword guess
/// on a payment. Returns the category name and the merchant's proper name when known. Refunds and
/// other money in from shops are left for you.
pub fn auto_category(raw: &str, memo: &str, amount: Decimal, keywords_too: bool) -> Option<(&'static str, Option<&'static str>)> {
    let stem = merchant_stem(raw, memo);
    let hay = format!("{stem} {raw}");
    let dir = Direction::of(amount);
    if let Some(f) = crate::merchants::find_for(&hay, dir) {
        return f.conflict.is_none().then_some((f.merchant.category.as_str(), Some(f.merchant.name.as_str())));
    }
    if dir == Direction::In || crate::merchants::find_for(&hay, Direction::In).is_some() {
        return None;
    }
    if !keywords_too {
        return None;
    }
    crate::merchants::keyword_match(&stem).filter(|h| h.conflict.is_none()).map(|h| (h.category, None))
}

/// Why a transaction was (or would be) filed where it is, in a sentence: your rule, the directory
/// entry and the words that matched, a keyword, or what stopped FinGraph from deciding.
pub fn explain(rules: &[Rule], entity_id: i64, raw: &str, memo: &str, amount: Decimal) -> Option<String> {
    if let Some(r) = first_match(rules, entity_id, raw, memo, amount) {
        let how = match r.kind {
            RuleKind::NormalizedTokens => "the words",
            RuleKind::Substring => "text containing",
            RuleKind::Regex => "the pattern",
            RuleKind::Exact => "exactly",
        };
        return Some(format!("Your rule for {how} \u{201c}{}\u{201d}", r.pattern));
    }
    let stem = merchant_stem(raw, memo);
    let hay = format!("{stem} {raw}");
    let dir = Direction::of(amount);
    if let Some(f) = crate::merchants::find_for(&hay, dir) {
        let m = f.merchant;
        return Some(match f.conflict {
            Some(c) => format!("\u{201c}{}\u{201d} is {} ({}) in the directory, but the rest of the name suggests {c}, so it\u{2019}s left for you", f.alias_text(), m.name, m.category),
            None => format!("Directory: {} ({}), matched \u{201c}{}\u{201d}", m.name, m.category, f.alias_text()),
        });
    }
    if dir == Direction::In {
        return crate::merchants::find_for(&hay, Direction::Out).map(|f| {
            if is_refund(raw, memo) {
                format!("A refund from {}: it would take back from {}, but money in is left for you", f.merchant.name, f.merchant.category)
            } else {
                format!("Money in from {}, a business you pay: pay or a refund, so it\u{2019}s left for you", f.merchant.name)
            }
        });
    }
    if crate::merchants::find_for(&hay, Direction::In).is_some() {
        return Some("Its name is in the directory as income, but this is money out, so it\u{2019}s left for you".into());
    }
    if FEE_HINT.is_match(&hay) {
        return Some("Worded like a bank fee or interest charge".into());
    }
    crate::merchants::keyword_match(&stem).map(|h| match h.conflict {
        Some(c) => format!("Keyword \u{201c}{}\u{201d} suggests {}, but another word suggests {c}, so it\u{2019}s left for you", h.key, h.category),
        None => format!("Keyword \u{201c}{}\u{201d} suggests {}", h.key, h.category),
    })
}

/// What saving a rule would do, before it's saved: the transactions it matches, how many would move
/// to its category, and how many you confirmed by hand (left as they are). Same test as applying it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RulePreview {
    pub matched: Vec<i64>,
    pub would_change: usize,
    pub kept_confirmed: usize,
    /// Where the matched transactions are filed now, most first: (category, count).
    pub now_in: Vec<(String, usize)>,
}

pub fn preview(rule: &Rule, rows: &[crate::db::TxRow]) -> RulePreview {
    let mut p = RulePreview::default();
    let mut now: BTreeMap<String, usize> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.split_seq.is_none() && rule.entity_id.is_none_or(|e| e == t.account_entity_id || e == t.entity_id)) {
        if !rule.applies(&t.raw_description, &t.memo, t.amount) {
            continue;
        }
        p.matched.push(t.id);
        *now.entry(t.category_name.clone().unwrap_or_else(|| "Uncategorized".into())).or_default() += 1;
        if t.review == crate::ReviewStatus::Confirmed && t.category_id.is_some() {
            p.kept_confirmed += 1;
        } else if rule.category_id.is_some() && rule.category_id != t.category_id {
            p.would_change += 1;
        }
    }
    p.now_in = now.into_iter().collect();
    p.now_in.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    p
}

impl RulePreview {
    pub fn label(&self) -> String {
        if self.matched.is_empty() {
            return "Matches nothing in your ledger yet; it will apply to new statements".into();
        }
        let n = self.matched.len();
        let mut s = format!("Matches {n} transaction{}", if n == 1 { "" } else { "s" });
        if self.would_change > 0 {
            s.push_str(&format!(" · {} would change category", self.would_change));
        }
        if self.kept_confirmed > 0 {
            s.push_str(&format!(" · {} you confirmed stay as they are", self.kept_confirmed));
        }
        let now: Vec<String> = self.now_in.iter().take(3).map(|(c, k)| format!("{c} {k}")).collect();
        s.push_str(&format!("\nNow in: {}", now.join(", ")));
        s
    }
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
    /// Why FinGraph suggests what it does (or nothing), in a sentence.
    pub why: Option<String>,
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
            let all_out = v.iter().all(|t| t.amount.is_sign_negative());
            let dir = if all_out { Direction::Out } else { Direction::In };
            let (payee, cat) = known_merchant_for(&stem, &v[0].raw, &v[0].memo, dir);
            let why = explain(&[], 0, &v[0].raw, &v[0].memo, v[0].amount);
            let dates: Vec<NaiveDate> = v.iter().map(|t| t.posted).collect();
            let cad = cadence(&dates);
            let mut totals: BTreeMap<Currency, Decimal> = BTreeMap::new();
            for t in &v {
                *totals.entry(t.currency).or_default() += t.amount;
            }
            let latest = v.iter().max_by_key(|t| t.posted).unwrap();
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
                why,
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
        // a family name is half of Cayman's shops: one word never swallows a longer name
        assert!(!same_merchant("BODDEN", "BODDEN OFFICE SUPPLY"));
        // a card statement can glue its currency onto the name: BODDEN MOTORS1USD isn't Bodden Office Supply
        assert_eq!(merchant_stem("BODDEN MOTORS1USD, GEORGE TOWN", "Ref 1000000001"), "BODDEN MOTORS");
        assert!(!same_merchant(&merchant_stem("BODDEN MOTORS1USD, GEORGE TOWN", "Ref 1000000001"), &merchant_stem("BODDEN OFFICE SUPPLY, GRAND CAYMAN", "Ref 1000000002")));
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
            t(1, d(10, 14), "THIRD PARTY TRF BNS", "Transfer to ANNA RIVERS 0193", "-640.12"),
            t(2, d(7, 29), "THIRD PARTY TRF BNS", "Transfer to SEAVIEW DENTAL LTD. 4", "-6.40"),
            t(3, d(7, 29), "THIRD PARTY TRF BNS", "Transfer to SEAVIEW DENTAL LTD. 4", "-11.25"),
            t(4, d(7, 3), "THIRD PARTY TRF BNS", "Transfer to JOHN BAKER 2290", "-25.00"),
            t(5, d(5, 15), "THIRD PARTY TRF BNS #Error", "Transfer to ANNA RIVERS 0193", "-640.00"),
            t(6, d(3, 2), "THIRD PARTY TRF BNS", "Transfer to ANNA RIVERS 0193", "-640.00"),
        ]);
        let mut stems: Vec<(String, usize)> = g.iter().map(|g| (g.stem.clone(), g.count)).collect();
        stems.sort();
        assert_eq!(stems, vec![("ANNA RIVERS".to_string(), 3), ("JOHN BAKER".to_string(), 1), ("SEAVIEW DENTAL".to_string(), 2)]);
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
    /// Probes from the 0.2.29 matching review, all made-up names: each pair must match or stay apart.
    #[test]
    fn a_rule_for_one_business_never_catches_another() {
        let flow = Rule { pattern: "FLOW".into(), category_id: Some(1), ..Default::default() };
        // (the directory check comes before, and doesn't depend on, the truncated-spelling preference;
        // that switch is process-wide, so tests leave it alone)
        assert!(flow.matches("FLOW WEB PMT", ""));
        assert!(!flow.matches("FLOW TAXI, GEORGE TOWN", ""), "Flow Taxi is its own business");
        // a rule from a stem the directory doesn't know never swallows a business it does
        let seven = Rule { pattern: "SEVEN MILE".into(), ..Default::default() };
        assert!(!seven.matches("SEVEN MILE DENTAL", ""));
        assert!(seven.matches("SEVEN MILE TRADING", ""));
        // matching every business with the text is still there, asked for by name
        // a rule you wrote for a payment platform still covers what it billed
        let paddle = Rule { pattern: "PADDLE.NET".into(), ..Default::default() };
        assert!(paddle.matches("PADDLE.NET* NORDVPN", ""));
        let broad = Rule { kind: RuleKind::Substring, pattern: "FLOW".into(), ..Default::default() };
        assert!(broad.matches("FLOW TAXI", ""));
    }

    #[test]
    fn a_brand_word_the_rest_of_the_name_contradicts_is_left_for_you() {
        let out: Decimal = "-80.00".parse().unwrap();
        assert_eq!(auto_category("PANDORA JEWELRY MIAMI", "", out, true), None);
        assert_eq!(known_merchant(&merchant_stem("PANDORA JEWELRY MIAMI", ""), "PANDORA JEWELRY MIAMI"), ("Pandora Jewelry Miami".into(), Some("Shopping")));
        assert_eq!(auto_category("PARAMOUNT PLUMBING LTD", "", out, true), None);
        assert_eq!(known_merchant_for("PARAMOUNT PLUMBING", "PARAMOUNT PLUMBING LTD", "", Direction::Out).1, Some("Repairs & Maintenance"));
        assert!(explain(&[], 1, "PARAMOUNT PLUMBING LTD", "", out).unwrap().contains("left for you"));
        // the services themselves still file
        assert_eq!(auto_category("PARAMOUNTPLUS.COM", "", out, false).map(|c| c.0), Some("Subscriptions & Digital"));
        assert_eq!(auto_category("PANDORA MEDIA", "", out, false).map(|c| c.0), Some("Subscriptions & Digital"));
        // a related word isn't a contradiction: a grocer's FOOD FAIR, a jerk centre's JERK
        assert_eq!(auto_category("POS PURCHASE", "FOSTER'S FOOD FAIR #12 CAMANA BAY", out, false).map(|c| c.0), Some("Groceries"));
        assert_eq!(auto_category("MARLEY JERK CENTRE", "", out, false).map(|c| c.0), Some("Takeaway & Delivery"));
    }

    #[test]
    fn names_keep_what_tells_them_apart() {
        assert!(merchant_stem("7 ELEVEN 12345, MIAMI", "").starts_with("7 ELEVEN"));
        let a = merchant_stem("MOBILE OB TRF DR", "TRF TO: JOHN A SMITH JR LANDSCAPING BANK OF BUTTERFIELD INTL");
        let b = merchant_stem("MOBILE OB TRF DR", "TRF TO: JOHN A SMITH JR CLEANING BANK OF BUTTERFIELD INTL");
        assert_eq!(a, "JOHN A SMITH JR LANDSCAPING");
        assert_ne!(a, b);
        assert!(!same_merchant(&a, &b));
        // the old cleaning, kept for moving saved choices over, merged them
        assert_eq!(merchant_stem_v("MOBILE OB TRF DR", "TRF TO: JOHN A SMITH JR CLEANING", StemVersion::V1), "JOHN A SMITH JR");
        // a payment to the bank itself keeps the bank's name
        assert_eq!(merchant_stem("CAYMAN NATIONAL BANK LOAN", ""), "CAYMAN NATIONAL BANK LOAN");
    }

    #[test]
    fn money_in_and_money_out_are_matched_their_own_way() {
        let (inn, out): (Decimal, Decimal) = ("1200.00".parse().unwrap(), "-1200.00".parse().unwrap());
        assert_eq!(auto_category("INTEREST PAID", "", inn, false).map(|c| c.0), Some("Interest & Dividends"));
        assert_ne!(auto_category("INTEREST PAID", "", out, true).map(|c| c.0), Some("Interest & Dividends"));
        assert_eq!(auto_category("ELECTRONIC CREDIT", "ACME HOLDINGS DIR PAY", inn, false).map(|c| c.0), Some("Salary"));
        // an airline's deposit: pay or a refund, never a flight, and never filed on its own
        assert_eq!(auto_category("ELECTRONIC CREDIT, CAYMAN AIRWAYS REFUND", "", inn, true), None);
        assert_eq!(known_merchant_for("CAYMAN AIRWAYS REFUND", "CAYMAN AIRWAYS REFUND", "", Direction::In).1, Some("Flights"), "a refund takes back from Flights");
        assert_eq!(known_merchant_for("CAYMAN AIRWAYS", "CAYMAN AIRWAYS", "", Direction::In).1, None);
        assert!(explain(&[], 1, "CAYMAN AIRWAYS REFUND", "", inn).unwrap().starts_with("A refund from"));
        // keywords are about spending: a deposit with MARKET in it isn't groceries
        assert_eq!(auto_category("ISLAND MARKET LTD", "", inn, true), None);
    }

    #[test]
    fn explanations_name_what_matched() {
        let out: Decimal = "-9.99".parse().unwrap();
        assert_eq!(explain(&[], 1, "NETFLIX.COM", "", out).as_deref(), Some("Directory: Netflix (Subscriptions & Digital), matched \u{201c}NETFLIX\u{201d}"));
        assert_eq!(explain(&[], 1, "HARBOUR EYE CLINIC", "", out).as_deref(), Some("Keyword \u{201c}EYE CLINIC\u{201d} suggests Optical"));
        let r = Rule { pattern: "NETFLIX".into(), category_id: Some(1), ..Default::default() };
        assert!(explain(&[r], 1, "NETFLIX.COM", "", out).unwrap().starts_with("Your rule"));
    }

    #[test]
    fn a_rule_preview_counts_what_saving_would_change() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let mut confirmed = crate::insights::tests::row(1, d, "NETFLIX.COM", "-15.99", "Entertainment", "");
        confirmed.review = crate::ReviewStatus::Confirmed;
        let mut auto = crate::insights::tests::row(2, d, "NETFLIX.COM", "-15.99", "Entertainment", "");
        auto.review = crate::ReviewStatus::AutoCategorized;
        let other = crate::insights::tests::row(3, d, "KIRK MARKET", "-50", "Groceries", "");
        let r = Rule { pattern: "NETFLIX".into(), category_id: Some(17), ..Default::default() };
        let p = preview(&r, &[confirmed, auto, other]);
        assert_eq!(p.matched, vec![1, 2]);
        assert_eq!((p.would_change, p.kept_confirmed), (1, 1));
        assert_eq!(p.now_in, vec![("Entertainment".to_string(), 2)]);
        assert!(p.label().contains("1 would change category"));
        assert!(preview(&Rule { pattern: "NOBODY".into(), ..Default::default() }, &[]).label().starts_with("Matches nothing"));
    }
}
