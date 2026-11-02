//! Bank profiles: how to recognise an institution's statements and what its layout quirks are.
//! Five Cayman banks are built in; users can edit them or add their own (stored in `bank_profiles`).

use serde::{Deserialize, Serialize};

use super::dates::Order;
use regex::Regex;
use std::sync::LazyLock;

use super::{ColumnRole, Diagnostics, ObservedLayout, Token};

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

/// Where a transaction's amount sits when its description runs over several lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowShape {
    /// Not checked against a real statement yet: either is accepted.
    #[default]
    Auto,
    /// Date, description and amount on the first line, detail lines under it (Scotiabank).
    AmountFirst,
    /// Date on the first line, amount and balance on the last (Cayman National).
    AmountLast,
}

/// The parts of a bank's layout that are written down rather than guessed at every scan. A scan that
/// doesn't match them says which one (Scan Report → layout notes).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutRules {
    pub row_shape: RowShape,
    /// Summary-box labels, matched at the start of a line before the generic ones.
    pub opening: Vec<String>,
    pub closing: Vec<String>,
    pub debits: Vec<String>,
    pub credits: Vec<String>,
    /// Account types read correctly from a real statement (`AccountType::as_str`). A statement of any
    /// other type from this bank waits for review, and approving it adds its type here.
    pub verified: Vec<String>,
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
    #[serde(default)]
    pub layout: LayoutRules,
}

impl BankProfile {
    pub fn date_order(&self) -> Order {
        if self.default_date_format.to_ascii_uppercase().starts_with("MM") { Order::MonthFirst } else { Order::DayFirst }
    }

    /// A new profile from one statement of a bank nobody set up: named and recognised by what page 1 says
    /// (`Pass1Report::identifiers`; empty when it names no bank), laid out the way the statement was read.
    pub fn from_statement(d: &Diagnostics) -> BankProfile {
        let p1 = &d.pass1;
        let ids = p1.identifiers.clone();
        let display_name = ids.first().cloned().or_else(|| p1.letterhead.first().cloned()).unwrap_or_else(|| "My Bank".into());
        // the badge: the name without its legal tail, or the domain without its ending
        let short_name = if display_name.contains('.') && !display_name.contains(' ') {
            display_name.split('.').next().unwrap_or("Bank").to_string()
        } else {
            let keep: Vec<&str> = display_name.split_whitespace().filter(|w| !matches!(w.to_lowercase().trim_matches('.'), "ltd" | "limited" | "inc" | "plc" | "n.a" | "na" | "co" | "corp")).take(3).collect();
            keep.join(" ")
        };
        let headings: Vec<ColumnRole> = d.observed.money_columns.clone();
        let column_layout_kind = if headings.contains(&ColumnRole::Debit) && headings.contains(&ColumnRole::Balance) {
            LayoutKind::DebitCreditWithBalance
        } else if headings.contains(&ColumnRole::Debit) {
            LayoutKind::SeparateDebitCredit
        } else if p1.account_type == Some(crate::AccountType::CreditCard) {
            LayoutKind::CreditCardDRCR
        } else {
            LayoutKind::SignedSingleAmount
        };
        let mut p = BankProfile {
            id: 0,
            slug: display_name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>().trim_matches('_').chars().take(24).collect(),
            short_name,
            display_name,
            routing_identifiers: ids,
            default_date_format: "DD/MM/YYYY".into(),
            column_layout_kind,
            header_anchors: HeaderAnchors::default(),
            noise_patterns: Vec::new(),
            fx_note_regex: GENERIC_FX.into(),
            is_builtin: false,
            layout: LayoutRules::default(),
        };
        p.learn(&d.observed);
        p
    }

    /// Learn what an approved statement showed about this bank's layout: its column headings, balance
    /// labels, row shape and date order. Only adds headings and labels (older statements keep reading);
    /// returns whether anything changed.
    pub fn learn(&mut self, o: &ObservedLayout) -> bool {
        let before = self.clone();
        for (role, word) in &o.headings {
            let w = word.trim().to_lowercase();
            // the generic words already name it: nothing to learn
            if w.is_empty() || super::multipass::classify_header(&w, None) == Some(*role) {
                continue;
            }
            let a = &mut self.header_anchors;
            let list = match role {
                ColumnRole::Date => &mut a.date,
                ColumnRole::PostDate => &mut a.posting_date,
                ColumnRole::Description => &mut a.description,
                ColumnRole::Reference => &mut a.reference,
                ColumnRole::Debit => &mut a.debit,
                ColumnRole::Credit => &mut a.credit,
                ColumnRole::Amount => &mut a.amount,
                ColumnRole::Balance => &mut a.balance,
            };
            if !list.contains(&w) {
                list.push(w);
            }
        }
        for (label, list) in [(&o.opening, &mut self.layout.opening), (&o.closing, &mut self.layout.closing)] {
            if let Some(l) = label.as_ref().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty() && l.split_whitespace().count() <= 5)
                && !list.contains(&l)
            {
                list.push(l);
            }
        }
        if o.row_shape != RowShape::Auto {
            self.layout.row_shape = o.row_shape;
        }
        if o.month_first != (self.date_order() == Order::MonthFirst) {
            self.default_date_format = if o.month_first { "MM/DD/YYYY" } else { "DD/MM/YYYY" }.into();
        }
        *self != before
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
            layout: LayoutRules::default(),
        },
        BankProfile {
            id: 0,
            slug: "scotiabank_ky".into(),
            display_name: "Scotiabank & Trust (Cayman) Ltd.".into(),
            short_name: "Scotiabank".into(),
            routing_identifiers: s(&["Scotiabank", "Scotiabank & Trust (Cayman)", "NOSCKYKY", "ky.scotiabank.com", "scotiabank.com/ky", "Bank of Nova Scotia"]),
            default_date_format: "DDMMM".into(),
            column_layout_kind: LayoutKind::DebitCreditWithBalance,
            // the real headings: chequing `Transaction Date | Description | DEBIT | CREDIT | Balance`, cards
            // `TRANSACTION DATE | POSTING DATE | REFERENCE NO. | TRANSACTION / DESCRIPTION | DEBIT/CREDIT AMOUNT`
            header_anchors: HeaderAnchors {
                date: s(&["transaction date"]),
                posting_date: s(&["posting date"]),
                description: s(&["description", "transaction / description"]),
                reference: s(&["reference no."]),
                debit: s(&["debit"]),
                credit: s(&["credit"]),
                amount: s(&["debit/credit amount"]),
                balance: s(&["balance"]),
            },
            noise_patterns: s(&[r"(?i)trademark of the bank of nova scotia", r"(?i)help us protect you", r"(?i)the branch walk with your valid photo"]),
            fx_note_regex: r"\((?P<amt>\d[\d,]*\.\d{2})\s*(?P<ccy>KYD|USD)\)".into(),
            is_builtin: true,
            // checked against 12 months of real chequing and card statements (2026)
            layout: LayoutRules {
                row_shape: RowShape::AmountFirst,
                opening: s(&["opening balance", "previous balance"]),
                closing: s(&["closing balance", "new balance"]),
                debits: s(&["withdrawals", "purchases, cash advances"]),
                credits: s(&["deposits", "payments & credits"]),
                verified: s(&["Checking", "CreditCard"]),
            },
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
            layout: LayoutRules {
                row_shape: RowShape::AmountLast,
                opening: s(&["beginning balance"]),
                closing: s(&["ending balance"]),
                debits: s(&["cheques & other debit"]),
                credits: s(&["deposits & other cred"]),
                verified: s(&["Savings"]),
            },
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
            layout: LayoutRules::default(),
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
            layout: LayoutRules::default(),
        },
    ]
}

static DOMAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:www\.)?([a-z0-9][a-z0-9-]*(?:\.[a-z0-9-]+)*\.(?:com|net|org|bank|ky|co|uk|ca|us|biz|info|io)(?:\.[a-z]{2})?)\b").unwrap());
static BANK_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:bank|trust|credit union|building society|bancorp|banco|banque)\b").unwrap());
/// Domains that say nothing about the bank: mail providers and the placeholders templates use.
const NOT_A_BANK: &[&str] = &["gmail.com", "yahoo.com", "hotmail.com", "outlook.com", "icloud.com", "aol.com", "mail.com", "email.com", "domain.com", "example.com", "company.com", "website.com", "yourwebsite.com", "sampletemplates.com", "templatelab.com", "tidyforms.com"];
/// Words that make a phrase about banking rather than a bank's name.
const NOT_A_NAME: &[&str] = &[
    "statement", "account", "acct", "name", "address", "branch", "transfer", "deposit", "payment", "balance", "information", "reconciliation", "fund",
    "online", "telephone", "mobile", "internet", "thank", "call", "contact", "details", "division", "savings", "checking", "your", "our", "int",
];

/// What could recognise a bank's statements later, from its letterhead (page 1 above the first transaction,
/// cells two spaces apart): its name — a proper name with Bank, Trust, Credit Union… in it, without a label
/// before it (`Bank Name: People's Trust Bank`) — and its web address. Never an email address (the
/// customer's, or a template's placeholder) or a merchant's domain from a transaction line.
pub fn bank_identifiers(letterhead: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in letterhead.lines() {
        for cell in line.split("  ").map(str::trim).filter(|c| !c.is_empty()) {
            let cell = cell.rsplit_once(':').map_or(cell, |(label, rest)| if rest.trim().is_empty() { label } else { rest }).trim();
            let name = cell.trim_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != '.');
            let words: Vec<&str> = name.split_whitespace().collect();
            // Title Case or capitals throughout, small joining words aside: `Bank of Butterfield`, `JPMORGAN CHASE BANK, N.A.`
            let proper = words.iter().all(|w| {
                let w = w.trim_matches(|c: char| !c.is_alphanumeric());
                w.is_empty() || matches!(w, "of" | "and" | "the" | "de" | "du" | "la" | "&") || w.chars().next().is_some_and(|c| c.is_uppercase() || c.is_ascii_digit())
            });
            let lower = name.to_lowercase();
            if BANK_WORD.is_match(name)
                && (2..=7).contains(&words.len())
                && proper
                && !name.chars().any(|c| c.is_ascii_digit() || matches!(c, '@' | '[' | '<' | '/' | '?'))
                && !lower.split(|c: char| !c.is_alphanumeric()).any(|w| NOT_A_NAME.contains(&w) || w.strip_suffix('s').is_some_and(|w| NOT_A_NAME.contains(&w)))
                && !names.iter().any(|n| n.eq_ignore_ascii_case(name))
            {
                names.push(name.to_string());
            }
        }
    }
    let mut domains: Vec<String> = DOMAIN
        .captures_iter(letterhead)
        .filter(|c| c.get(0).is_some_and(|m| !letterhead[..m.start()].ends_with('@')) && !c[1].contains('@'))
        .map(|c| c[1].to_lowercase())
        .filter(|d| !NOT_A_BANK.contains(&d.as_str()))
        .collect();
    // an email's domain isn't a web address: drop domains that only appear after an `@`
    domains.retain(|d| !letterhead.to_lowercase().contains(&format!("@{d}")) || letterhead.to_lowercase().contains(&format!("www.{d}")));
    // a bank's own address: written as one (`www.`), or about banking, or it is the name or its initials
    // (`lnb.com` for Liberty National Bank); a shop's domain in the letterhead text is none of these
    let lower = letterhead.to_lowercase();
    domains.retain(|d| {
        // the name without its ending (`ky.harbourbank` of `ky.harbourbank.com`)
        let stem = d.rsplit_once('.').map_or(d.as_str(), |(s, _)| s);
        lower.contains(&format!("www.{d}"))
            || ["bank", "trust", "credit", "cu"].iter().any(|w| stem.contains(w))
            || names.iter().any(|n| {
                let words: Vec<String> = n.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).filter(|w| !w.is_empty()).collect();
                let initials: String = words.iter().filter_map(|w| w.chars().next()).collect();
                words.iter().any(|w| w.len() >= 4 && stem.contains(w.as_str())) || stem.split('.').any(|label| label.len() >= 3 && initials.starts_with(label))
            })
    });
    domains.dedup();
    names.into_iter().take(2).chain(domains.into_iter().take(2)).collect()
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
/// a Scotiabank statement can't outvote Scotiabank's own letterhead. A bank is only picked when it's named
/// on page 1: a statement whose letterhead names no known bank (a rebrand, a new bank) is from an unknown
/// bank, not from whichever bank one of its transfer lines mentions.
///
/// `head` is page 1 outside its transactions (letterhead and footer): a transfer line on page 1 that says
/// "Credit Union" is part of the transactions, and counts only once.
pub fn fingerprint<'a>(profiles: &'a [BankProfile], head: &str, tokens: &[Token]) -> (Option<&'a BankProfile>, Vec<(String, u32)>) {
    let page1 = head.to_lowercase();
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
    let best = scores.iter().enumerate().filter(|(_, (_, s))| *s >= 3).max_by_key(|(i, (_, s))| (*s, usize::MAX - i)).map(|(i, _)| &profiles[i]);
    scores.sort_by_key(|x| std::cmp::Reverse(x.1));
    (best, scores)
}
