//! Bank profiles: how to recognise an institution's statements and what its layout quirks are.
//! Five Cayman banks are built in; users can edit them or add their own (stored in `bank_profiles`).

use serde::{Deserialize, Serialize};

use super::dates::Order;
use super::Token;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutKind {
    SignedSingleAmount,
    SeparateDebitCredit,
    DebitCreditWithBalance,
    CreditCardDRCR,
}

impl LayoutKind {
    pub const ALL: [LayoutKind; 4] = [Self::SignedSingleAmount, Self::SeparateDebitCredit, Self::DebitCreditWithBalance, Self::CreditCardDRCR];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignedSingleAmount => "SignedSingleAmount",
            Self::SeparateDebitCredit => "SeparateDebitCredit",
            Self::DebitCreditWithBalance => "DebitCreditWithBalance",
            Self::CreditCardDRCR => "CreditCardDRCR",
        }
    }
    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|k| k.as_str() == s).unwrap_or(Self::DebitCreditWithBalance)
    }
}

/// Extra header words per column role, on top of the generic ones every scan knows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeaderAnchors {
    #[serde(default)]
    pub date: Vec<String>,
    #[serde(default)]
    pub posting_date: Vec<String>,
    #[serde(default)]
    pub description: Vec<String>,
    #[serde(default)]
    pub reference: Vec<String>,
    #[serde(default)]
    pub debit: Vec<String>,
    #[serde(default)]
    pub credit: Vec<String>,
    #[serde(default)]
    pub amount: Vec<String>,
    #[serde(default)]
    pub balance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BankProfile {
    pub id: i64,
    pub slug: String,
    pub display_name: String,
    /// Short label for badges (`Butterfield`, `CNB`).
    pub short_name: String,
    pub routing_identifiers: Vec<String>,
    pub default_date_format: String,
    pub column_layout_kind: LayoutKind,
    pub header_anchors: HeaderAnchors,
    pub noise_patterns: Vec<String>,
    /// Needs named groups `amt` and `ccy`; `rate` is optional.
    pub fx_note_regex: String,
    pub is_builtin: bool,
}

impl BankProfile {
    pub fn date_order(&self) -> Order {
        if self.default_date_format.to_ascii_uppercase().starts_with("MM") { Order::MonthFirst } else { Order::DayFirst }
    }
}

pub(crate) const GENERIC_FX: &str = r"(?i)(?P<ccy>USD|KYD|US\$|CI\$)\s*(?P<amt>\d[\d,]*\.\d{2})\s*(?:@|at)\s*(?P<rate>\d+\.\d+)";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

pub fn builtins() -> Vec<BankProfile> {
    vec![
        BankProfile {
            id: 0,
            slug: "butterfield_ky".into(),
            display_name: "Bank of Butterfield (Cayman) Limited".into(),
            short_name: "Butterfield".into(),
            routing_identifiers: s(&["Butterfield", "Bank of Butterfield (Cayman)", "BNTBKYKY", "butterfieldgroup.com"]),
            default_date_format: "DD/MM/YYYY".into(),
            column_layout_kind: LayoutKind::DebitCreditWithBalance,
            header_anchors: HeaderAnchors { posting_date: s(&["posting date"]), ..Default::default() },
            noise_patterns: s(&[r"(?i)butterfield is licensed", r"(?i)member of the butterfield group", r"(?i)deposit insurance"]),
            fx_note_regex: GENERIC_FX.into(),
            is_builtin: true,
        },
        BankProfile {
            id: 0,
            slug: "scotiabank_ky".into(),
            display_name: "Scotiabank & Trust (Cayman) Ltd.".into(),
            short_name: "Scotiabank".into(),
            routing_identifiers: s(&["Scotiabank", "Scotiabank & Trust (Cayman)", "NOSCKYKY", "ky.scotiabank.com", "scotiabank.com/ky", "Bank of Nova Scotia"]),
            default_date_format: "DDMMM".into(),
            column_layout_kind: LayoutKind::DebitCreditWithBalance,
            header_anchors: HeaderAnchors {
                debit: s(&["withdrawals (-)", "total minus"]),
                credit: s(&["deposits (+)", "total plus"]),
                amount: s(&["debit/credit amount"]),
                ..Default::default()
            },
            noise_patterns: s(&[r"(?i)trademark of the bank of nova scotia", r"(?i)help us protect you", r"(?i)the branch walk with your valid photo"]),
            fx_note_regex: r"\((?P<amt>\d[\d,]*\.\d{2})\s*(?P<ccy>KYD|USD)\)".into(),
            is_builtin: true,
        },
        BankProfile {
            id: 0,
            slug: "cnb_ky".into(),
            display_name: "Cayman National Bank Ltd.".into(),
            short_name: "CNB".into(),
            routing_identifiers: s(&["Cayman National", "Cayman National Bank", "CNB", "CNBCKYKY", "caymannational.com"]),
            // checked against a real statement (Feb 2026): `02/02` dates, one Amount column with a trailing
            // minus for money out, the date on a description's first line and the amount on its last
            default_date_format: "DD/MM".into(),
            column_layout_kind: LayoutKind::SignedSingleAmount,
            header_anchors: HeaderAnchors { description: s(&["description", "particulars"]), reference: s(&["cheque #"]), ..Default::default() },
            noise_patterns: s(&[
                r"(?i)cayman national bank ltd\. is licensed",
                r"(?i)please examine this statement",
                r"(?i)^\W*customer\s+copy\W*$",
                r"(?i)celebrating half a century",
            ]),
            fx_note_regex: GENERIC_FX.into(),
            is_builtin: true,
        },
        BankProfile {
            id: 0,
            slug: "rbc_ky".into(),
            display_name: "Royal Bank of Canada (RBC Royal Bank Cayman)".into(),
            short_name: "RBC".into(),
            routing_identifiers: s(&["RBC Royal Bank", "Royal Bank of Canada", "ROYCKYKY", "rbcroyalbank.com"]),
            default_date_format: "DD MMM".into(),
            column_layout_kind: LayoutKind::DebitCreditWithBalance,
            header_anchors: HeaderAnchors::default(),
            noise_patterns: s(&[r"(?i)registered trade-?marks? of royal bank of canada", r"(?i)^\s*rbc royal bank\s*$"]),
            fx_note_regex: GENERIC_FX.into(),
            is_builtin: true,
        },
        BankProfile {
            id: 0,
            slug: "cicsa_cu_ky".into(),
            display_name: "Cayman Islands Civil Service Association (CICSA) Co-operative Credit Union Ltd.".into(),
            short_name: "CICSA CU".into(),
            routing_identifiers: s(&["Credit Union", "CICSA", "Co-operative Credit Union", "cicsacu.com.ky"]),
            default_date_format: "DD/MM/YYYY".into(),
            column_layout_kind: LayoutKind::DebitCreditWithBalance,
            header_anchors: HeaderAnchors {
                description: s(&["transaction/reference", "transaction / reference"]),
                debit: s(&["withdrawal/debit"]),
                credit: s(&["deposit/credit"]),
                ..Default::default()
            },
            noise_patterns: s(&[r"(?i)owned by its members", r"(?i)co-operative societies law"]),
            fx_note_regex: GENERIC_FX.into(),
            is_builtin: true,
        },
    ]
}

/// A short identifier (`CNB`) must match as a whole word; longer ones as substrings.
fn id_matches(hay_lower: &str, id: &str) -> bool {
    let id = id.to_lowercase();
    if id.len() > 4 {
        return hay_lower.contains(&id);
    }
    hay_lower.match_indices(&id).any(|(i, _)| {
        let before = hay_lower[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = hay_lower[i + id.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before && after
    })
}

/// Score every profile against the document: each distinct identifier counts 3 when it appears
/// on page 1 (the letterhead) and 1 when only seen later, so a "TRF TO: BANK OF BUTTERFIELD" line in
/// a Scotiabank statement can't outvote Scotiabank's own letterhead.
pub fn fingerprint<'a>(profiles: &'a [BankProfile], tokens: &[Token]) -> (Option<&'a BankProfile>, Vec<(String, u32)>) {
    let page1: String = tokens.iter().filter(|t| t.page == 1).map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ").to_lowercase();
    let all: String = tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ").to_lowercase();
    let mut scores: Vec<(String, u32)> = profiles
        .iter()
        .map(|p| {
            let score = p
                .routing_identifiers
                .iter()
                .map(|id| if id_matches(&page1, id) { 3 } else if id_matches(&all, id) { 1 } else { 0 })
                .sum();
            (p.slug.clone(), score)
        })
        .collect();
    let best = scores.iter().enumerate().filter(|(_, (_, s))| *s > 0).max_by_key(|(i, (_, s))| (*s, usize::MAX - i)).map(|(i, _)| &profiles[i]);
    scores.sort_by_key(|x| std::cmp::Reverse(x.1));
    (best, scores)
}
