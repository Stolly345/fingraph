//! Statement coverage shared by budgets, reports and exported plans.
use chrono::NaiveDate;
use crate::db::{Account, Db, DocumentRow};
use crate::{Reconciliation, Result};

#[derive(Debug, Clone)]
pub struct AccountCoverage {
    pub id: i64,
    pub name: String,
    pub verified_end: Option<NaiveDate>,
}

#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub accounts: Vec<AccountCoverage>,
}

impl Coverage {
    pub fn new(accounts: &[Account], docs: &[DocumentRow]) -> Self {
        Self { accounts: accounts.iter().filter(|a| !a.hidden).map(|a| AccountCoverage {
            id: a.id, name: a.name.clone(),
            verified_end: docs.iter().filter(|d| d.account_id == a.id && verified(d))
                .filter_map(|d| d.period.map(|p| p.1)).max(),
        }).collect() }
    }

    /// Least recent verified statement end. None if any visible account is unverified.
    /// This is a freshness measure, not a claim that every intervening date is covered.
    pub fn common_end(&self) -> Option<NaiveDate> {
        self.accounts.iter().map(|a| a.verified_end).collect::<Option<Vec<_>>>()?.into_iter().min()
    }

    pub fn label(&self) -> String {
        let ends: Vec<_> = self.accounts.iter().filter_map(|a| a.verified_end).collect();
        let missing = self.accounts.len() - ends.len();
        let Some(oldest) = ends.iter().min() else { return "No verified statements yet".into() };
        let newest = ends.iter().max().unwrap_or(oldest);
        if missing > 0 {
            format!("{missing} accounts have no verified statement · newest {}", newest.format("%d %b %Y"))
        } else if oldest == newest {
            format!("Verified statement ends: {}", oldest.format("%d %b %Y"))
        } else {
            format!("Verified statement ends: {} – {}", oldest.format("%d %b %Y"), newest.format("%d %b %Y"))
        }
    }

    pub fn details(&self) -> String {
        self.accounts.iter().map(|a| format!("{}: {}", a.name,
            a.verified_end.map(|d| d.format("%d %b %Y").to_string()).unwrap_or_else(|| "no verified statement".into())))
            .collect::<Vec<_>>().join("\n")
    }
}

pub fn verified(d: &DocumentRow) -> bool {
    d.review.is_empty() && d.reconciliation == Reconciliation::Reconciled
}

/// Require every day, including the edges of a calendar month, to have verified coverage.
pub fn covers_period(docs: &[DocumentRow], account: i64, from: NaiveDate, to: NaiveDate) -> bool {
    (0..=(to - from).num_days()).all(|i| {
        let day = from + chrono::Duration::days(i);
        docs.iter().any(|d| d.account_id == account && verified(d) && d.period.is_some_and(|(s, e)| s <= day && day <= e))
    })
}

pub fn complete_months(accounts: &[Account], docs: &[DocumentRow], end: NaiveDate, n: i32) -> Vec<NaiveDate> {
    use crate::insights::{add_months, month_start};
    (0..n).map(|i| add_months(month_start(end), i - n + 1)).filter(|m| {
        let last = add_months(*m, 1).pred_opt().unwrap_or(*m);
        if last > end { return false; }
        let mut relevant = 0;
        for a in accounts {
            let first = docs.iter().filter(|d| d.account_id == a.id).filter_map(|d| d.period.map(|p| p.0)).min();
            if first.is_some_and(|start| start > last) { continue; }
            relevant += 1;
            if !covers_period(docs, a.id, *m, last) { return false; }
        }
        relevant > 0
    }).collect()
}

impl Db {
    pub fn statement_coverage(&self, entity_ids: &[i64]) -> Result<Coverage> {
        let accounts: Vec<_> = self.accounts()?.into_iter()
            .filter(|a| entity_ids.is_empty() || entity_ids.contains(&a.entity_id)).collect();
        Ok(Coverage::new(&accounts, &self.documents_all(entity_ids)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freshness_requires_every_visible_account_and_only_verified_documents() {
        let db = Db::open_in_memory().unwrap(); db.load_demo().unwrap();
        let accounts: Vec<_> = db.accounts().unwrap().into_iter().take(2).collect();
        let mut docs = db.documents(&[]).unwrap();
        docs.retain(|d| accounts.iter().any(|a| a.id == d.account_id));
        for d in &mut docs { d.period = Some(("2026-10-01".parse().unwrap(),
            if d.account_id == accounts[0].id { "2026-10-15" } else { "2026-10-31" }.parse().unwrap())); }
        let c = Coverage::new(&accounts, &docs);
        assert_eq!(c.common_end(), Some("2026-10-15".parse().unwrap()));
        assert!(c.label().contains("15 Oct 2026 – 31 Oct 2026"));
        for d in &mut docs { if d.account_id == accounts[0].id { d.reconciliation = Reconciliation::Unverified; } }
        assert_eq!(Coverage::new(&accounts, &docs).common_end(), None);
    }
}
