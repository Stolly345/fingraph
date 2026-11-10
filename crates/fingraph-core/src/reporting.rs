//! One definition of a monthly average for reports, Summary and runway.
use chrono::NaiveDate;
use crate::{Currency, Decimal, Error, Result};
use crate::db::{Db, TxRow};
use crate::insights::{add_months, flows, missing_rates};

#[derive(Debug, Default)]
pub struct FlowAverage {
    pub months: Vec<NaiveDate>,
    pub income: Decimal,
    pub expenses: Decimal,
}

impl FlowAverage {
    pub fn monthly(&self) -> Option<(Decimal, Decimal)> {
        let n = Decimal::from(self.months.len());
        (!n.is_zero()).then(|| ((self.income / n).round_dp(2), (self.expenses / n).round_dp(2)))
    }
    pub fn label(&self) -> String {
        match (self.months.first(), self.months.last()) {
            (Some(first), Some(last)) => format!("{} complete months between {} and {}; partial or missing months excluded", self.months.len(), first.format("%b %Y"), last.format("%b %Y")),
            _ => "Average unavailable: no complete months across these accounts".into(),
        }
    }
}

impl Db {
    pub fn cash_flow_average(&self, rows: &[TxRow], entity_ids: &[i64], end: NaiveDate, n: i32, ccy: Currency) -> Result<FlowAverage> {
        let fx = self.fx()?;
        if !missing_rates(rows, ccy, &fx).is_empty() { return Err(Error("Exchange rates are needed for this average".into())); }
        let accounts: Vec<_> = self.accounts()?.into_iter().filter(|a| entity_ids.is_empty() || entity_ids.contains(&a.entity_id) || rows.iter().any(|t| t.account_id == a.id)).collect();
        let months = crate::coverage::complete_months(&accounts, &self.documents_all(&[])?, end, n);
        let (mut income, mut expenses) = (Decimal::ZERO, Decimal::ZERO);
        for m in &months {
            let (i, e) = flows(rows, *m, add_months(*m, 1).pred_opt().unwrap_or(*m), ccy, &fx);
            income += i; expenses += e;
        }
        Ok(FlowAverage { months, income, expenses })
    }
}

/// Financial health, each figure with what it was worked out from (no combined score).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Health {
    /// Cash, and the essential (fixed-category) spending of an average complete month.
    pub emergency: Option<(Decimal, Decimal)>,
    /// Owed on cards and loans, and the income of an average complete month.
    pub debt: Option<(Decimal, Decimal)>,
    /// Bank fees and interest charged over the last 12 months, and how many charges.
    pub fees_12m: (Decimal, usize),
}

impl Health {
    /// Months of essential spending the cash would cover.
    pub fn emergency_months(&self) -> Option<Decimal> {
        self.emergency.filter(|e| !e.1.is_zero()).map(|(c, e)| (c / e).round_dp(1))
    }
    /// Debt as months of income.
    pub fn debt_months(&self) -> Option<Decimal> {
        self.debt.filter(|d| !d.1.is_zero()).map(|(o, i)| (o / i).round_dp(1))
    }
}

/// `(cash, owed)` from account balances; the rest from rows over the complete months in `avg`.
pub fn health(rows: &[TxRow], avg: &FlowAverage, (cash, owed): (Decimal, Decimal), fixed: &[String], parents: &std::collections::BTreeMap<i64, String>, (ccy, fx): (Currency, &crate::Fx), today: NaiveDate) -> Health {
    let n = Decimal::from(avg.months.len());
    let essential = (!n.is_zero()).then(|| {
        let total: Decimal = avg.months.iter().map(|m| crate::insights::by_category(rows, *m, add_months(*m, 1).pred_opt().unwrap_or(*m), ccy, fx, parents).into_iter().filter(|(c, _)| fixed.contains(c)).map(|x| x.1).sum::<Decimal>()).sum();
        (total / n).round_dp(2)
    });
    let year_ago = today - chrono::Duration::days(365);
    let fees: Vec<&TxRow> = rows.iter().filter(|t| t.posted > year_ago && t.posted <= today && t.amount.is_sign_negative() && !t.is_transfer() && (t.category_name.as_deref() == Some("Bank Fees & Interest") || crate::rules::is_bank_fee(&t.raw_description, &t.memo))).collect();
    Health {
        emergency: essential.filter(|e| !e.is_zero() && !cash.is_sign_negative()).map(|e| (cash, e)),
        debt: avg.monthly().filter(|m| !m.0.is_zero()).map(|m| (owed.max(Decimal::ZERO), m.0)),
        fees_12m: (fees.iter().map(|t| -crate::insights::value(t, ccy, fx)).sum::<Decimal>().round_dp(2), fees.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn averages_exclude_a_partial_account_month_instead_of_counting_it_as_zero() {
        let db = Db::open_in_memory().unwrap(); db.load_demo().unwrap();
        let accounts: Vec<_> = db.accounts().unwrap().into_iter().take(2).collect();
        let ids: Vec<_> = accounts.iter().map(|a| a.id).collect();
        let mut docs = db.documents(&[]).unwrap(); docs.retain(|d| ids.contains(&d.account_id));
        let mut selected = Vec::new();
        for a in &accounts {
            let mut mine = docs.iter().filter(|d| d.account_id == a.id);
            let mut d = mine.next().unwrap().clone();
            d.review.clear(); d.reconciliation = crate::Reconciliation::Reconciled;
            d.period = Some(("2026-01-01".parse().unwrap(), "2026-01-31".parse().unwrap())); selected.push(d.clone());
            d.id = mine.next().unwrap().id;
            d.period = Some(("2026-02-01".parse().unwrap(), if a.id == ids[0] { "2026-02-15" } else { "2026-02-28" }.parse().unwrap())); selected.push(d);
        }
        let months = crate::coverage::complete_months(&accounts, &selected, "2026-02-28".parse().unwrap(), 2);
        assert_eq!(months, vec!["2026-01-01".parse::<NaiveDate>().unwrap()]);
        db.conn.execute(&format!("DELETE FROM accounts WHERE id NOT IN ({},{})", ids[0], ids[1]), []).unwrap();
        let keep = selected.iter().map(|d| d.id.to_string()).collect::<Vec<_>>().join(",");
        db.conn.execute(&format!("DELETE FROM documents WHERE id NOT IN ({keep})"), []).unwrap();
        for d in &selected {
            let (start, end) = d.period.unwrap();
            db.conn.execute("UPDATE documents SET period_start=?1,period_end=?2,reconciliation_status='Reconciled',review_hold=0 WHERE id=?3", rusqlite::params![start.to_string(),end.to_string(),d.id]).unwrap();
        }
        let row = crate::insights::tests::row;
        let expense = row(1, "2026-01-15".parse().unwrap(), "Shopping", "-100", "Shopping", "");
        let mut income = row(2, "2026-01-15".parse().unwrap(), "Salary", "300", "Income", "");
        income.category_kind = Some(crate::db::CategoryKind::Income);
        let mut partial = income.clone(); partial.posted = "2026-02-10".parse().unwrap(); partial.amount = 9000.into();
        let mut rows = vec![expense, income, partial];
        for t in &mut rows { t.account_id = ids[0]; }
        let actual = db.cash_flow_average(&rows, &[], "2026-02-28".parse().unwrap(), 2, Currency::KYD).unwrap();
        assert_eq!(actual.monthly(), Some((300.into(),100.into())), "February must not dilute or inflate January");
        for d in &mut selected { d.period = Some(("2026-02-01".parse().unwrap(), "2026-02-15".parse().unwrap())); }
        assert!(crate::coverage::complete_months(&accounts, &selected, "2026-02-28".parse().unwrap(), 1).is_empty());
        assert!(FlowAverage::default().monthly().is_none());
        let average = FlowAverage { months, income: 300.into(), expenses: 100.into() };
        assert_eq!(average.monthly(), Some((300.into(), 100.into())));
    }

    #[test]
    fn health_shows_each_figure_with_its_parts() {
        use crate::insights::tests::row;
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let rows = vec![
            row(1, d(8, 9), "CUC", "-200", "Utilities", ""),
            row(2, d(9, 9), "CUC", "-210", "Utilities", ""),
            row(3, d(9, 12), "FOSTERS", "-400", "Groceries", ""),
            row(4, d(9, 30), "MONTHLY FEE", "-5", "Bank Fees & Interest", ""),
        ];
        let avg = FlowAverage { months: vec![d(8, 1), d(9, 1)], income: Decimal::from(6000), expenses: Decimal::from(815) };
        let v = |s: &str| s.parse::<Decimal>().unwrap();
        let h = health(&rows, &avg, (v("2050"), v("3000")), &["Utilities".to_string()], &Default::default(), (Currency::KYD, &crate::Fx::default()), d(10, 1));
        assert_eq!(h.emergency, Some((v("2050"), v("205"))));
        assert_eq!(h.emergency_months(), Some(v("10.0")));
        assert_eq!(h.debt_months(), Some(v("1.0")));
        assert_eq!(h.fees_12m, (v("5"), 1));
        // no complete months: no emergency or debt figure, never a made-up zero
        let none = health(&rows, &FlowAverage::default(), (v("2050"), v("3000")), &[], &Default::default(), (Currency::KYD, &crate::Fx::default()), d(10, 1));
        assert_eq!((none.emergency, none.debt), (None, None));
    }
}
