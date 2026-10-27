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
use serde::{Deserialize, Serialize};

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

// ──────────────────────────────────────────────────────────────── alerts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    /// Spent more than the budget.
    Over,
    /// 85% gone while more than 15% of the month is still to come.
    Near,
}

/// `elapsed` = how far through the month (0–1); a past month is 1.
pub fn alert(spent: Decimal, budget: Decimal, elapsed: Decimal) -> Option<Alert> {
    if budget <= Decimal::ZERO {
        return None;
    }
    let used = spent / budget;
    let near = Decimal::new(85, 2);
    if used > Decimal::ONE {
        Some(Alert::Over)
    } else if used >= near && elapsed < near {
        Some(Alert::Near)
    } else {
        None
    }
}

// ──────────────────────────────────────────────────────────────── the plan

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PayCycle {
    Weekly,
    Fortnightly,
    /// Twice a month (the 15th and the last day, say).
    SemiMonthly,
    #[default]
    Monthly,
}

impl PayCycle {
    pub const ALL: [PayCycle; 4] = [Self::Weekly, Self::Fortnightly, Self::SemiMonthly, Self::Monthly];
    pub fn label(self) -> &'static str {
        match self {
            Self::Weekly => "Weekly",
            Self::Fortnightly => "Every two weeks",
            Self::SemiMonthly => "Twice a month",
            Self::Monthly => "Monthly",
        }
    }
    pub fn per_year(self) -> Decimal {
        Decimal::from(match self {
            Self::Weekly => 52,
            Self::Fortnightly => 26,
            Self::SemiMonthly => 24,
            Self::Monthly => 12,
        })
    }
    /// The payday after `d`.
    pub fn next(self, d: NaiveDate) -> NaiveDate {
        match self {
            Self::Weekly => d + chrono::Duration::days(7),
            Self::Fortnightly => d + chrono::Duration::days(14),
            // the 15th, then the month's last day
            Self::SemiMonthly => {
                let fifteenth = month_start(d) + chrono::Duration::days(14);
                if d < fifteenth { fifteenth } else if d < month_end(d) { month_end(d) } else { month_start(d + chrono::Duration::days(1)) + chrono::Duration::days(14) }
            }
            Self::Monthly => day_in(add_months(month_start(d), 1), d),
        }
    }
}

fn month_end(d: NaiveDate) -> NaiveDate {
    add_months(month_start(d), 1).pred_opt().unwrap_or(d)
}

/// `like`'s day of the month, in `month` (clamped to its last day).
fn day_in(month: NaiveDate, like: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    month.with_day(like.day()).unwrap_or_else(|| month_end(month))
}

/// Taken off each pay: an amount, or a percent of gross ("Pension 5%").
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Deduction {
    pub name: String,
    pub amount: Decimal,
    pub percent: bool,
}

/// What comes in and what you're aiming to keep: the basis for the budget's projections.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Plan {
    pub cycle: PayCycle,
    pub next_pay: Option<NaiveDate>,
    /// Gross pay per payday.
    pub gross: Decimal,
    pub deductions: Vec<Deduction>,
    /// Rent received, side work… per month, after any costs.
    pub other_income: Decimal,
    /// Loan and card instalments per month (transfers, so not in the spending budget).
    pub loan_payments: Decimal,
    /// Per month.
    pub savings_goal: Decimal,
    pub currency: Option<Currency>,
}

impl Plan {
    pub fn deduction(&self, d: &Deduction) -> Decimal {
        if d.percent { (self.gross * d.amount / Decimal::ONE_HUNDRED).round_dp(2) } else { d.amount }
    }
    pub fn net_pay(&self) -> Decimal {
        self.gross - self.deductions.iter().map(|d| self.deduction(d)).sum::<Decimal>()
    }
    /// Take-home per month, averaged over the year (26 fortnightly pays ÷ 12).
    pub fn monthly_income(&self) -> Decimal {
        (self.net_pay() * self.cycle.per_year() / Decimal::from(12)).round_dp(2) + self.other_income
    }
    /// What's left each month after the budget and loan payments.
    pub fn monthly_savings(&self, budgeted: Decimal) -> Decimal {
        self.monthly_income() - budgeted - self.loan_payments
    }
    /// The next `n` paydays from `today`.
    pub fn paydays(&self, today: NaiveDate, n: usize) -> Vec<NaiveDate> {
        let Some(anchor) = self.next_pay else { return Vec::new() };
        // monthly pay keeps its day (the 31st, or the month's last day when shorter)
        let nth = |i: i32, d: NaiveDate| if self.cycle == PayCycle::Monthly { day_in(add_months(month_start(anchor), i), anchor) } else { self.cycle.next(d) };
        let (mut d, mut i) = (anchor, 0);
        while d < today && i < 2000 {
            i += 1;
            d = nth(i, d);
        }
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(d);
            i += 1;
            d = nth(i, d);
        }
        out
    }
    /// The factor flexible budgets must be multiplied by to reach the savings goal:
    /// income − loans − fixed − k·flexible = goal  ⇒  k = (income − loans − fixed − goal) ÷ flexible.
    /// `None` when there's no flexible spending to cut, or even cutting all of it isn't enough.
    pub fn flexible_factor(&self, fixed: Decimal, flexible: Decimal) -> Option<Decimal> {
        if flexible <= Decimal::ZERO {
            return None;
        }
        let k = (self.monthly_income() - self.loan_payments - fixed - self.savings_goal) / flexible;
        (k >= Decimal::ZERO).then(|| k.min(Decimal::ONE).round_dp(4))
    }
}

impl Db {
    pub fn plan(&self, scope: &str) -> Option<Plan> {
        serde_json::from_str(&self.setting(&format!("budget_plan:{scope}"))?).ok()
    }
    pub fn save_plan(&self, scope: &str, plan: &Plan) -> Result<()> {
        self.set_setting(&format!("budget_plan:{scope}"), &serde_json::to_string(plan).unwrap_or_default())
    }
}

/// Bills that don't bend month to month; everything else is flexible when cutting to a goal.
pub const FIXED: &[&str] = &["Bank Fees & Interest", "Education", "Government & Fees", "Housing", "Insurance", "Property Expense", "Subscriptions & Digital", "Utilities"];

pub fn is_fixed(category: &str) -> bool {
    FIXED.contains(&category)
}

impl Db {
    /// The categories you count as fixed expenses (the rest are variable); `FIXED` until changed.
    pub fn fixed_categories(&self) -> Vec<String> {
        self.setting("budget_fixed").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_else(|| FIXED.iter().map(|s| s.to_string()).collect())
    }
    pub fn set_fixed(&self, category: &str, fixed: bool) -> Result<()> {
        let mut v = self.fixed_categories();
        v.retain(|c| c != category);
        if fixed {
            v.push(category.to_string());
            v.sort();
        }
        self.set_setting("budget_fixed", &serde_json::to_string(&v).unwrap_or_default())
    }
}

// ──────────────────────────────────────────────────────────────── from your statements

/// A regular pay found in the statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Salary {
    pub payee: String,
    /// The latest deposit (take-home).
    pub amount: Decimal,
    pub currency: Currency,
    pub cycle: PayCycle,
    pub last: NaiveDate,
}

/// The likeliest salary: money in, not a transfer, from the same payer at least three times in the
/// last 120 days at a steady rhythm and a steady amount (within 15%). Salary-categorised or
/// payroll-worded deposits win, then the biggest per month.
pub fn detect_salary(rows: &[TxRow], today: NaiveDate) -> Option<Salary> {
    let since = today - chrono::Duration::days(120);
    let mut by: BTreeMap<String, Vec<&TxRow>> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.posted >= since && t.amount > Decimal::ZERO && !t.is_transfer() && t.split_seq.is_none()) {
        by.entry(crate::rules::merchant_stem(&t.raw_description, &t.memo)).or_default().push(t);
    }
    let mut best: Option<((bool, Decimal), Salary)> = None;
    for v in by.values_mut().filter(|v| v.len() >= 3) {
        v.sort_by_key(|t| t.posted);
        let mut gaps: Vec<i64> = v.windows(2).map(|w| (w[1].posted - w[0].posted).num_days()).collect();
        gaps.sort_unstable();
        let gap = gaps[gaps.len() / 2];
        let cycle = match gap {
            5..=9 => PayCycle::Weekly,
            12..=14 => PayCycle::Fortnightly,
            15..=17 => PayCycle::SemiMonthly,
            26..=35 => PayCycle::Monthly,
            _ => continue,
        };
        let last = v[v.len() - 1];
        let amounts: Vec<Decimal> = v.iter().map(|t| t.amount).collect();
        let (lo, hi) = (amounts.iter().min().copied().unwrap_or_default(), amounts.iter().max().copied().unwrap_or_default());
        if hi > lo * Decimal::new(115, 2) {
            continue;
        }
        let words = format!("{} {} {}", last.raw_description, last.clean_payee, last.category_name.clone().unwrap_or_default()).to_uppercase();
        let salaryish = ["SALARY", "PAYROLL", "WAGES", "PAYCHEQUE", "PAYCHECK"].iter().any(|w| words.contains(w)) && !words.contains("SHARES");
        let monthly = last.amount * cycle.per_year() / Decimal::from(12);
        let s = Salary { payee: last.clean_payee.clone(), amount: last.amount, currency: last.currency, cycle, last: last.posted };
        if best.as_ref().is_none_or(|(k, _)| (salaryish, monthly) > *k) {
            best = Some(((salaryish, monthly), s));
        }
    }
    best.map(|(_, s)| s)
}

/// Monthly totals of one top-level category over the `n` whole months before `month`, oldest first.
pub fn history(rows: &[TxRow], category: &str, month: NaiveDate, n: i32, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> Vec<Decimal> {
    let month = month_start(month);
    (1..=n)
        .rev()
        .map(|i| {
            let from = add_months(month, -i);
            let to = add_months(from, 1).pred_opt().unwrap_or(from);
            by_category(rows, from, to, ccy, fx, parents).into_iter().find(|(c, _)| c == category).map(|x| x.1).unwrap_or_default()
        })
        .collect()
}

/// Least-squares straight line through `v` (x = 0, 1, 2…): (value at x = 0, change per step).
pub fn trend(v: &[Decimal]) -> (Decimal, Decimal) {
    let n = Decimal::from(v.len());
    if v.len() < 2 {
        return (v.first().copied().unwrap_or_default(), Decimal::ZERO);
    }
    let xs: Vec<Decimal> = (0..v.len()).map(Decimal::from).collect();
    let (sx, sy) = (xs.iter().sum::<Decimal>(), v.iter().sum::<Decimal>());
    let sxy: Decimal = xs.iter().zip(v).map(|(x, y)| x * y).sum();
    let sxx: Decimal = xs.iter().map(|x| x * x).sum();
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    ((sy - slope * sx) / n, slope)
}

/// The line `trend` drew, `ahead` steps after the last point; never below zero.
pub fn forecast(v: &[Decimal], ahead: usize) -> Decimal {
    let (a, b) = trend(v);
    (a + b * Decimal::from(v.len() - 1 + ahead)).max(Decimal::ZERO).round_dp(2)
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

        assert!(db.fixed_categories().contains(&"Utilities".to_string()));
        db.set_fixed("Utilities", false).unwrap();
        db.set_fixed("Groceries", true).unwrap();
        let f = db.fixed_categories();
        assert!(!f.contains(&"Utilities".to_string()) && f.contains(&"Groceries".to_string()));
    }

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn alerts_over_and_near() {
        assert_eq!(alert(d("110"), d("100"), d("0.5")), Some(Alert::Over));
        assert_eq!(alert(d("90"), d("100"), d("0.5")), Some(Alert::Near));
        // 90% used with 90% of the month gone is just on pace
        assert_eq!(alert(d("90"), d("100"), d("0.9")), None);
        assert_eq!(alert(d("50"), d("100"), d("0.5")), None);
        assert_eq!(alert(d("50"), d("0"), d("0.5")), None);
    }

    #[test]
    fn plan_math() {
        let p = Plan {
            cycle: PayCycle::Fortnightly,
            gross: d("2000"),
            deductions: vec![Deduction { name: "Pension".into(), amount: d("5"), percent: true }, Deduction { name: "Health".into(), amount: d("150"), percent: false }],
            other_income: d("100"),
            loan_payments: d("400"),
            savings_goal: d("1000"),
            ..Default::default()
        };
        assert_eq!(p.net_pay(), d("1750")); // 2000 − 100 − 150
        assert_eq!(p.monthly_income(), d("3891.67")); // 1750 × 26 ÷ 12 + 100
        assert_eq!(p.monthly_savings(d("2000")), d("1491.67"));
        // fixed 1500 + k × 1500 flexible: 3891.67 − 400 − 1500 − 1000 = 991.67 → k = 0.6611
        assert_eq!(p.flexible_factor(d("1500"), d("1500")), Some(d("0.6611")));
        // already under: no cut needed (capped at 1)
        assert_eq!(p.flexible_factor(d("100"), d("100")), Some(Decimal::ONE));
        // fixed alone breaks the goal
        assert_eq!(p.flexible_factor(d("3000"), d("100")), None);

        let p = Plan { cycle: PayCycle::SemiMonthly, next_pay: NaiveDate::from_ymd_opt(2026, 1, 15), ..Default::default() };
        let days: Vec<String> = p.paydays(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(), 3).iter().map(|d| d.to_string()).collect();
        assert_eq!(days, ["2026-02-15", "2026-02-28", "2026-03-15"]);
        let p = Plan { cycle: PayCycle::Monthly, next_pay: NaiveDate::from_ymd_opt(2026, 1, 31), ..Default::default() };
        assert_eq!(p.paydays(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(), 2), [NaiveDate::from_ymd_opt(2026, 2, 28).unwrap(), NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()]);
    }

    #[test]
    fn trend_line() {
        let v = [d("100"), d("110"), d("120"), d("130")];
        assert_eq!(trend(&v), (d("100"), d("10")));
        assert_eq!(forecast(&v, 1), d("140"));
        assert_eq!(forecast(&[d("50"), d("10")], 3), Decimal::ZERO);
    }

    #[test]
    fn finds_the_demo_salary() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let s = detect_salary(&rows, NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()).expect("a salary");
        println!("{s:?}");
        assert!(s.amount > Decimal::ZERO);
    }
}
