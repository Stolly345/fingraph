//! Persist a complete budget setup as one transaction.
use crate::{Currency, Decimal, Result};
use crate::budget::{Fund, Plan};
use crate::db::Db;
use crate::prefs::Prefs;

pub type BudgetAmount = (i64, Option<(Decimal, Currency)>);

pub struct BudgetSetup {
    pub plan: Plan,
    pub needs: Vec<(String, bool)>,
    pub budgets: Vec<BudgetAmount>,
    pub funds: Vec<Fund>,
    pub deleted_funds: Vec<i64>,
    /// Merchant and whether it belongs among non-monthly bills.
    pub big_bill_choices: Vec<(String, bool)>,
    pub preferences: Option<Prefs>,
}

impl Db {
    pub fn save_budget_setup(&self, scope: &str, setup: &BudgetSetup) -> Result<()> {
        let tx = self.sp()?;
        for (name, need) in &setup.needs { self.set_need(name, *need)?; }
        self.save_plan(scope, &setup.plan)?;
        for (id, amount) in &setup.budgets { self.set_budget(scope, *id, *amount)?; }
        for fund in &setup.funds { self.save_fund(scope, fund)?; }
        for id in &setup.deleted_funds { self.delete_fund(*id)?; }
        for (stem, big) in &setup.big_bill_choices {
            if *big { self.keep_big_bill(stem)?; } else { self.mark_not_big_bill(stem)?; }
        }
        // Global preferences must not change until their database transaction commits.
        if let Some(p) = &setup.preferences { self.set_setting("preferences", &serde_json::to_string(p)?)?; }
        tx.commit()?;
        if let Some(p) = &setup.preferences { p.apply_globals(); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_late_failure_rolls_back_plan_categories_and_budget() {
        let db = Db::open_in_memory().unwrap();
        let category = db.category_id("Groceries").unwrap().unwrap();
        let before = db.need_categories();
        db.conn.execute_batch("CREATE TRIGGER reject_fund BEFORE INSERT ON sinking_funds BEGIN SELECT RAISE(ABORT, 'simulated disk/write failure'); END;").unwrap();
        let setup = BudgetSetup {
            plan: Plan::default(), needs: vec![("Groceries".into(), false)],
            budgets: vec![(category, Some((500.into(), Currency::KYD)))],
            funds: vec![Fund { id: 0, name: "Insurance".into(), amount: 1200.into(), currency: Currency::KYD, every_months: 12, next_due: "2026-12-01".parse().unwrap(), stem: String::new() }],
            deleted_funds: vec![], big_bill_choices: vec![], preferences: None,
        };
        assert!(db.save_budget_setup("1", &setup).is_err());
        assert!(db.plan("1").is_none());
        assert!(db.budgets("1").unwrap().is_empty());
        assert_eq!(db.need_categories(), before);
        db.conn.execute_batch("DROP TRIGGER reject_fund").unwrap();
        db.save_budget_setup("1", &setup).unwrap();
        assert!(db.plan("1").is_some());
        assert_eq!(db.budgets("1").unwrap()[&category].0, Decimal::from(500));
    }
}
