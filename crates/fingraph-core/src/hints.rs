//! Merchant hints: what a user may choose to send back so FinGraph's built-in merchant directory can
//! learn, as a merchant name and the category they gave it. Nothing here is sent anywhere: the app
//! writes a file the user reads, edits and sends themselves.
//!
//! Only shop and service names leave, never people: no amounts, dates, accounts or balances; no
//! income (it names employers and tenants); no transfers or payments made by bank transfer, cheque or
//! deposit (they name people); and nothing the directory already gets right, since it adds nothing.

use std::collections::BTreeMap;

use crate::db::{Category, CategoryKind, TxRow};
use crate::rules::{known_merchant, looks_like_transfer, merchant_stem, varied_payee};

#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    /// The merchant as the statement names it, reference numbers and places stripped (`KIRK MOTORS`).
    pub merchant: String,
    pub category: String,
    /// The category's parent, when it's a sub-category (`Vehicle Maintenance` → `Transport`).
    pub parent: Option<String>,
    /// How many of your transactions say so (the most common category wins).
    pub times: usize,
}

/// The hints in `rows`, most-used first.
pub fn merchant_hints(rows: &[TxRow], cats: &[Category]) -> Vec<Hint> {
    let mut by: BTreeMap<String, (String, BTreeMap<i64, usize>)> = BTreeMap::new();
    for t in rows {
        let Some(cat) = t.category_id else { continue };
        // money out to a shop, not income, not your own money moving, not a person paid by transfer
        if !t.amount.is_sign_negative() || t.is_transfer() || matches!(t.category_kind, Some(CategoryKind::Income | CategoryKind::Transfer)) || looks_like_transfer(&t.raw_description, &t.memo) {
            continue;
        }
        let stem = merchant_stem(&t.raw_description, &t.memo);
        if stem.len() < 3 || varied_payee(&stem) {
            continue;
        }
        let e = by.entry(stem).or_insert_with(|| (t.raw_description.clone(), BTreeMap::new()));
        *e.1.entry(cat).or_default() += 1;
    }
    let mut out: Vec<Hint> = by
        .into_iter()
        .filter_map(|(stem, (raw, counts))| {
            let (&cat, _) = counts.iter().max_by_key(|(id, n)| (**n, std::cmp::Reverse(**id)))?;
            let c = cats.iter().find(|c| c.id == cat)?;
            // the directory already says the same: nothing to teach
            if known_merchant(&stem, &raw).1 == Some(c.name.as_str()) {
                return None;
            }
            let parent = c.parent_id.and_then(|p| cats.iter().find(|x| x.id == p)).map(|p| p.name.clone());
            Some(Hint { merchant: stem, category: c.name.clone(), parent, times: counts.values().sum() })
        })
        .collect();
    out.sort_by(|a, b| b.times.cmp(&a.times).then(a.merchant.cmp(&b.merchant)));
    out
}

/// The file the user sends: CSV with a header, one merchant a line, and the FinGraph version that made it.
pub fn to_csv(hints: &[Hint], version: &str) -> String {
    let q = |s: &str| if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut s = format!("# FinGraph {version} merchant hints: merchant names and categories only\nmerchant,category,parent,times\n");
    for h in hints {
        s.push_str(&format!("{},{},{},{}\n", q(&h.merchant), q(&h.category), q(h.parent.as_deref().unwrap_or("")), h.times));
    }
    s
}
