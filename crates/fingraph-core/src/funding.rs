//! Explicit allocations are separate from a bill's time-based savings target.
use chrono::NaiveDate;
use crate::{Decimal, Error, Result};
use crate::db::Db;

impl Db {
    /// A user-recorded allocation for one bill cycle, never inferred from elapsed time.
    pub fn fund_allocation(&self, fund_id: i64, due: NaiveDate) -> Option<Decimal> {
        self.setting(&format!("fund_allocation:{fund_id}:{due}")).and_then(|v| v.parse().ok())
    }

    pub fn set_fund_allocation(&self, fund_id: i64, due: NaiveDate, amount: Option<Decimal>) -> Result<()> {
        let exists: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM sinking_funds WHERE id=?1)", [fund_id], |r| r.get(0))?;
        if !exists || fund_id <= 0 || amount.is_some_and(|v| v < Decimal::ZERO) {
            return Err(Error("Choose a saved bill and a non-negative allocation".into()));
        }
        self.set_setting(&format!("fund_allocation:{fund_id}:{due}"), &amount.map(|a| a.to_string()).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_allocation_is_explicit_and_does_not_carry_into_the_next_bill_cycle() {
        let db = Db::open_in_memory().unwrap();
        let due = "2026-12-01".parse().unwrap();
        db.save_fund("1", &crate::budget::Fund { id: 0, name: "Insurance".into(), amount: 1200.into(), currency: crate::Currency::KYD, every_months: 12, next_due: due, stem: String::new() }).unwrap();
        assert_eq!(db.fund_allocation(1, due), None);
        db.set_fund_allocation(1, due, Some(Decimal::from(300))).unwrap();
        assert_eq!(db.fund_allocation(1, due), Some(Decimal::from(300)));
        assert_eq!(db.fund_allocation(1, "2027-12-01".parse().unwrap()), None);
        assert!(db.set_fund_allocation(1, due, Some(Decimal::NEGATIVE_ONE)).is_err());
        db.set_fund_allocation(1, due, None).unwrap();
        assert_eq!(db.fund_allocation(1, due), None);
        db.set_fund_allocation(1, due, Some(300.into())).unwrap();
        db.delete_fund(1).unwrap();
        assert_eq!(db.fund_allocation(1, due), None);
        assert!(db.set_fund_allocation(1, due, Some(300.into())).is_err());
    }
}
