//! Monthly budgets per top-level category, suggested from what you actually spent. A budget belongs
//! to the view it was set in (one workspace, or a consolidated set), so Personal and the business
//! keep their own.

use chrono::NaiveDate;
use rusqlite::params;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

use crate::db::{Db, TxRow};
use crate::insights::{add_months, by_category, month_start};
use crate::money::Fx;
use crate::{Currency, Result};

/// The key for a set of workspaces: `"1,3"`.
pub fn scope(entity_ids: &[i64]) -> String {
    let mut ids = entity_ids.to_vec();
    ids.sort_unstable();
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

impl Db {
    /// Category id → monthly budget, as entered.
    pub fn budgets(&self, scope: &str) -> Result<BTreeMap<i64, (Decimal, Currency)>> {
        let mut st = self.conn.prepare("SELECT category_id, amount, currency FROM budgets WHERE scope = ?1")?;
        let rows = st.query_map([scope], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).map(|(c, a, k)| (c, (crate::db::dec(&a), crate::db::ccy(&k)))).collect())
    }

    /// `None` (or zero) removes the budget.
    pub fn set_budget(&self, scope: &str, category_id: i64, amount: Option<(Decimal, Currency)>) -> Result<()> {
        match amount.filter(|(a, _)| !a.is_zero()) {
            Some((a, c)) => self.conn.execute(
                "INSERT INTO budgets (scope, category_id, amount, currency) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(scope, category_id) DO UPDATE SET amount = excluded.amount, currency = excluded.currency",
                params![scope, category_id, a.abs().to_string(), c.code()],
            )?,
            None => self.conn.execute("DELETE FROM budgets WHERE scope = ?1 AND category_id = ?2", params![scope, category_id])?,
        };
        Ok(())
    }
}

/// What each top-level category usually costs a month: the median over the `n` whole months before
/// `month` (months with no spending count as zero, so a one-off doesn't become a habit), rounded up
/// to the next 10. Categories with nothing in those months are left out.
pub fn suggest(rows: &[TxRow], month: NaiveDate, n: i32, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> BTreeMap<String, Decimal> {
    let month = month_start(month);
    let mut per: BTreeMap<String, Vec<Decimal>> = BTreeMap::new();
    for i in 1..=n {
        let from = add_months(month, -i);
        let to = add_months(from, 1).pred_opt().unwrap_or(from);
        for (cat, v) in by_category(rows, from, to, ccy, fx, parents) {
            per.entry(cat).or_default().push(v);
        }
    }
    per.into_iter()
        .filter_map(|(cat, mut v)| {
            v.resize(n as usize, Decimal::ZERO);
            v.sort();
            let mid = v.len() / 2;
            let m = if v.len() % 2 == 0 { (v[mid - 1] + v[mid]) / Decimal::TWO } else { v[mid] };
            let ten = Decimal::TEN;
            let r = (m / ten).ceil() * ten;
            (!r.is_zero()).then_some((cat, r))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TxFilter;

    #[test]
    fn suggests_the_typical_month_and_keeps_budgets() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter { split_lines: true, ..Default::default() }).unwrap();
        let fx = db.fx().unwrap();
        let cats = db.categories().unwrap();
        let parents: BTreeMap<i64, String> = cats.iter().filter_map(|c| Some((c.id, cats.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect();
        let s = suggest(&rows, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), 6, Currency::KYD, &fx, &parents);
        assert!(!s.is_empty());
        assert!(s.values().all(|v| (*v % Decimal::TEN).is_zero() && *v > Decimal::ZERO));
        let groceries = s.get("Groceries").copied().unwrap_or_default();
        assert!(groceries > Decimal::ZERO);

        let sc = scope(&[3, 1]);
        assert_eq!(sc, "1,3");
        let g = cats.iter().find(|c| c.name == "Groceries").unwrap().id;
        db.set_budget(&sc, g, Some((Decimal::new(600, 0), Currency::KYD))).unwrap();
        db.set_budget(&sc, g, Some((Decimal::new(650, 0), Currency::KYD))).unwrap();
        assert_eq!(db.budgets(&sc).unwrap()[&g], (Decimal::new(650, 0), Currency::KYD));
        assert!(db.budgets("1").unwrap().is_empty());
        db.set_budget(&sc, g, None).unwrap();
        assert!(db.budgets(&sc).unwrap().is_empty());
    }
}
