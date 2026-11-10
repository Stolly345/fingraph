//! Refunds linked to the purchases they give back: a credit from a shop finds the purchase it
//! returns (same merchant, earlier, at least as much), takes that purchase's category so it offsets
//! the spending, and says which purchase it was. Only on good evidence: the very same amount, or
//! refund wording. Anything else stays in To Review, as money in from a business always has.

use chrono::Duration;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};

use crate::db::{CategoryKind, Db, TxRow};
use crate::rules::{is_refund, merchant_stem, same_merchant};
use crate::{Result, ReviewStatus};

/// How far back a refund looks for its purchase.
pub const LOOK_BACK_DAYS: i64 = 120;

#[derive(Debug, Clone, PartialEq)]
pub struct RefundLink {
    pub refund: i64,
    pub purchase: i64,
    /// The refund is the whole purchase, to the cent.
    pub whole: bool,
}

/// The merchant a line names, with refund wording taken out so `KIRK MARKET REFUND` meets `KIRK MARKET`.
fn who(t: &TxRow) -> (String, Option<&'static str>) {
    let strip = |s: &str| s.split_whitespace().filter(|w| !is_refund(w, "")).collect::<Vec<_>>().join(" ");
    let (raw, memo) = (strip(&t.raw_description), strip(&t.memo));
    let stem = merchant_stem(&raw, &memo);
    let dir = crate::merchants::find(&format!("{stem} {raw}")).filter(|f| f.conflict.is_none() && !f.merchant.fallback).map(|f| f.merchant.name.as_str());
    (stem, dir)
}

fn same(a: &(String, Option<&'static str>), b: &(String, Option<&'static str>)) -> bool {
    match (a.1, b.1) {
        (Some(x), Some(y)) => x == y,
        (None, None) => same_merchant(&a.0, &b.0),
        _ => false,
    }
}

/// Every refund in `rows` that can be tied to its purchase. Each purchase gives back at most its own
/// amount across all its refunds (two partial refunds can share one purchase). `skip` = refunds you
/// unlinked, never linked again. Refunds you filed yourself as income are left alone.
pub fn find_links(rows: &[TxRow], skip: &BTreeSet<i64>) -> Vec<RefundLink> {
    let mut credits: Vec<&TxRow> = rows
        .iter()
        .filter(|t| t.amount.is_sign_positive() && !t.amount.is_zero() && !t.is_transfer() && t.split_seq.is_none() && !skip.contains(&t.id))
        .filter(|t| !(t.review == ReviewStatus::Confirmed && t.category_kind == Some(CategoryKind::Income)))
        .collect();
    credits.sort_by_key(|t| (t.posted, t.id));
    let purchases: Vec<&TxRow> = rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer() && t.split_seq.is_none()).collect();
    let keys: BTreeMap<i64, (String, Option<&'static str>)> = credits.iter().chain(purchases.iter()).map(|t| (t.id, who(t))).collect();
    let mut left: BTreeMap<i64, Decimal> = purchases.iter().map(|p| (p.id, -p.amount)).collect();
    let mut out = Vec::new();
    for c in credits {
        let worded = is_refund(&c.raw_description, &c.memo);
        let earliest = c.posted - Duration::days(LOOK_BACK_DAYS);
        let candidates: Vec<&&TxRow> = purchases
            .iter()
            .filter(|p| p.posted <= c.posted && p.posted >= earliest && p.entity_id == c.entity_id && p.currency == c.currency)
            .filter(|p| left.get(&p.id).is_some_and(|l| *l >= c.amount))
            .filter(|p| same(&keys[&c.id], &keys[&p.id]))
            .collect();
        // the same amount, nearest first; else (only with refund wording) the nearest purchase it fits in
        let exact = candidates.iter().filter(|p| -p.amount == c.amount).max_by_key(|p| (p.posted, p.id));
        let pick = exact.or_else(|| if worded { candidates.iter().max_by_key(|p| (p.posted, p.id)) } else { None });
        if let Some(p) = pick {
            *left.get_mut(&p.id).expect("candidate") -= c.amount;
            out.push(RefundLink { refund: c.id, purchase: p.id, whole: -p.amount == c.amount });
        }
    }
    out
}

impl Db {
    fn refund_skips(&self) -> BTreeSet<i64> {
        self.setting("refund_unlinked").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    }

    /// The links in force: refund id → purchase id (worked out from the ledger, so a rescan or an
    /// edit keeps them right).
    pub fn refund_links(&self) -> Result<BTreeMap<i64, i64>> {
        Ok(find_links(&self.transactions(&Default::default())?, &self.refund_skips()).into_iter().map(|l| (l.refund, l.purchase)).collect())
    }

    /// File each linked refund you haven't filed yourself under its purchase's category, with its
    /// payee. Returns how many changed.
    pub fn link_refunds(&self) -> Result<usize> {
        let rows = self.transactions(&Default::default())?;
        let by: BTreeMap<i64, &TxRow> = rows.iter().map(|t| (t.id, t)).collect();
        let tx = self.sp()?;
        let mut n = 0;
        for l in find_links(&rows, &self.refund_skips()) {
            let (r, p) = (by[&l.refund], by[&l.purchase]);
            if r.review == ReviewStatus::Confirmed || p.category_id.is_none() || r.category_id == p.category_id {
                continue;
            }
            tx.execute(
                "UPDATE transactions SET category_id = ?2, clean_payee = ?3, review_status = 'AutoCategorized' WHERE id = ?1",
                rusqlite::params![r.id, p.category_id, p.clean_payee],
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// The purchase a refund gives back, if it's linked.
    pub fn refund_of(&self, refund_id: i64) -> Result<Option<TxRow>> {
        match self.refund_links()?.get(&refund_id) {
            Some(p) => Ok(Some(self.transaction(*p)?)),
            None => Ok(None),
        }
    }

    /// Not a refund of that purchase after all: unlinked for good, and back to To Review unless you filed it.
    pub fn unlink_refund(&self, refund_id: i64) -> Result<()> {
        let mut s = self.refund_skips();
        s.insert(refund_id);
        self.set_setting("refund_unlinked", &serde_json::to_string(&s)?)?;
        self.conn.execute("UPDATE transactions SET category_id = NULL, review_status = 'NeedsReview' WHERE id = ?1 AND review_status = 'AutoCategorized'", [refund_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insights::tests::row;
    use chrono::NaiveDate;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    #[test]
    fn a_refund_finds_its_purchase() {
        let rows = vec![
            row(1, d(9, 2), "ISLAND SHOES", "-120.00", "Clothing & Shoes", ""),
            row(2, d(9, 20), "ISLAND SHOES", "-80.00", "Clothing & Shoes", ""),
            row(3, d(9, 25), "ISLAND SHOES REFUND", "50.00", "", ""),
            row(4, d(9, 26), "ISLAND SHOES", "120.00", "", ""),
            row(5, d(9, 27), "KIRK MARKET", "35.00", "", ""),
            row(6, d(9, 28), "ISLAND SHOES", "33.00", "", ""),
        ];
        let links = find_links(&rows, &BTreeSet::new());
        // worded, partial: the nearest purchase it fits in
        assert!(links.contains(&RefundLink { refund: 3, purchase: 2, whole: false }), "{links:?}");
        // the very same amount, no wording needed
        assert!(links.contains(&RefundLink { refund: 4, purchase: 1, whole: true }), "{links:?}");
        // no purchase at that merchant; and an odd amount without refund wording: left alone
        assert!(!links.iter().any(|l| l.refund == 5 || l.refund == 6), "{links:?}");
        // unlinked by you: stays unlinked
        assert!(!find_links(&rows, &[4].into_iter().collect()).iter().any(|l| l.refund == 4));
    }

    #[test]
    fn a_purchase_gives_back_no_more_than_it_cost() {
        let rows = vec![
            row(1, d(9, 2), "ISLAND SHOES", "-100.00", "Clothing & Shoes", ""),
            row(2, d(9, 10), "ISLAND SHOES REFUND", "60.00", "", ""),
            row(3, d(9, 12), "ISLAND SHOES REFUND", "60.00", "", ""),
        ];
        let links = find_links(&rows, &BTreeSet::new());
        assert_eq!(links.len(), 1, "{links:?}");
        // and pay from an airline never meets a flight it didn't refund
        let pay = vec![row(1, d(9, 2), "CAYMAN AIRWAYS", "-450.00", "Flights", ""), row(2, d(9, 15), "CAYMAN AIRWAYS DIR PAY", "2720.64", "", "")];
        assert!(find_links(&pay, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn linking_files_the_refund_with_its_purchase() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&Default::default()).unwrap();
        let buy = rows.iter().find(|t| t.amount.is_sign_negative() && t.category_id.is_some() && !t.is_transfer() && t.split_seq.is_none() && format!("{} {}", t.raw_description, t.memo).contains("KIRK")).unwrap().clone();
        // any deposit, moved onto the purchase's account three days later and worded as a refund
        let credit = rows.iter().find(|t| t.amount.is_sign_positive() && t.transfer_peer.is_none() && t.split_seq.is_none()).unwrap().clone();
        db.conn
            .execute(
                "UPDATE transactions SET account_id = ?2, posted_date = ?3, currency = ?4, raw_description = 'KIRK MARKET REFUND', memo = '', amount = '5.00', category_id = NULL, review_status = 'NeedsReview' WHERE id = ?1",
                rusqlite::params![credit.id, buy.account_id, (buy.posted + Duration::days(3)).to_string(), buy.currency.code()],
            )
            .unwrap();
        assert!(db.link_refunds().unwrap() >= 1);
        let after = db.transaction(credit.id).unwrap();
        assert_eq!(after.category_id, buy.category_id);
        assert!(db.refund_of(credit.id).unwrap().is_some_and(|p| p.clean_payee.contains("Kirk") || p.raw_description.contains("KIRK")));
        db.unlink_refund(credit.id).unwrap();
        assert_eq!(db.transaction(credit.id).unwrap().category_id, None);
        assert!(db.refund_of(credit.id).unwrap().is_none());
        assert_eq!(db.link_refunds().unwrap(), 0, "unlinked stays unlinked");
    }
}
