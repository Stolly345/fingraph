//! Monthly budgets per top-level category, suggested from what you actually spent. A budget belongs
//! to the view it was set in (one workspace, or a consolidated set), so Personal and the business
//! keep their own.

use chrono::{Datelike, NaiveDate};
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
    /// Category id → monthly budget: the latest amount set.
    pub fn budgets(&self, scope: &str) -> Result<BTreeMap<i64, (Decimal, Currency)>> {
        // dates are compared as text, so not NaiveDate::MAX ("+262142-…")
        self.budgets_in(scope, NaiveDate::from_ymd_opt(9999, 12, 1).unwrap_or_default())
    }

    /// `None` (or zero) removes the budget. It applies from this month on; earlier months keep the
    /// amount they had (see `budgets_in`).
    pub fn set_budget(&self, scope: &str, category_id: i64, amount: Option<(Decimal, Currency)>) -> Result<()> {
        self.set_budget_from(scope, category_id, amount, month_start(chrono::Local::now().date_naive()))
    }

    pub fn set_budget_from(&self, scope: &str, category_id: i64, amount: Option<(Decimal, Currency)>, month: NaiveDate) -> Result<()> {
        let (a, c) = amount.map_or((Decimal::ZERO, Currency::KYD), |(a, c)| (a.abs(), c));
        // a category's first budget covers the months before it too (so last month can be closed
        // against it); later changes apply from `month` on
        let first: bool = self.conn.query_row("SELECT COUNT(*) = 0 FROM budget_history WHERE scope = ?1 AND category_id = ?2", params![scope, category_id], |r| r.get(0))?;
        let month = if first { NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or(month) } else { month };
        self.conn.execute(
            "INSERT INTO budget_history VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope, category_id, month) DO UPDATE SET amount = excluded.amount, currency = excluded.currency",
            params![scope, category_id, month_start(month).to_string(), a.to_string(), c.code()],
        )?;
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

/// What each top-level category usually costs a month: the median over the `n` months before
/// `month` that the statements cover completely (a month with no spending in a category counts as
/// zero, so a one-off doesn't become a habit; months before the first statement or after the last
/// aren't zero, they're unknown, and are left out), rounded up to the next 10. With less than one
/// whole month of statements, the partial months are used. Categories with nothing are left out.
pub fn suggest(rows: &[TxRow], month: NaiveDate, n: i32, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> BTreeMap<String, Decimal> {
    let month = month_start(month);
    let Some((first, last)) = crate::insights::data_span(rows) else { return BTreeMap::new() };
    let months: Vec<(NaiveDate, NaiveDate)> = (1..=n).map(|i| add_months(month, -i)).map(|from| (from, add_months(from, 1).pred_opt().unwrap_or(from))).filter(|(f, t)| *t >= first && *f <= last).collect();
    // ponytail: a statement period that starts on the 2nd still makes its first month partial; fine for a median
    let whole: Vec<(NaiveDate, NaiveDate)> = months.iter().copied().filter(|(f, t)| *f >= month_start(first) && (first.day() <= 3 || *f > first) && *t <= last).collect();
    let months = if whole.is_empty() { months } else { whole };
    let mut per: BTreeMap<String, Vec<Decimal>> = BTreeMap::new();
    for (from, to) in &months {
        for (cat, v) in by_category(rows, *from, *to, ccy, fx, parents) {
            per.entry(cat).or_default().push(v);
        }
    }
    per.into_iter()
        .filter_map(|(cat, mut v)| {
            v.resize(months.len(), Decimal::ZERO);
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
/// last 120 days at a steady rhythm, at a steady amount — at least half of them within 15% of the
/// usual one, so a bonus, a refund or a bank's penny test under the same wording doesn't hide it. Salary-categorised or payroll-worded deposits win,
/// then the biggest per month. `today` should be the newest statement's date.
pub fn detect_salary(rows: &[TxRow], today: NaiveDate) -> Option<Salary> {
    let since = today - chrono::Duration::days(120);
    let mut by: BTreeMap<String, Vec<&TxRow>> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.posted >= since && t.amount > Decimal::ZERO && !t.is_transfer() && t.split_seq.is_none()) {
        by.entry(crate::rules::payer_key(&t.raw_description, &t.memo)).or_default().push(t);
    }
    let mut best: Option<((bool, Decimal), Salary)> = None;
    for v in by.values_mut().filter(|v| v.len() >= 3) {
        // the usual amount, and only the deposits near it
        let mut amounts: Vec<Decimal> = v.iter().map(|t| t.amount).collect();
        amounts.sort();
        let usual = amounts[amounts.len() / 2];
        let near = |t: &TxRow| (t.amount - usual).abs() <= usual * Decimal::new(15, 2);
        if v.iter().filter(|t| near(t)).count() * 2 < v.len() {
            continue;
        }
        v.retain(|t| near(t));
        if v.len() < 3 {
            continue;
        }
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

// ──────────────────────────────────────────────────────────────── rollovers & moving money

/// Money moved from one category to another in one month ("cover the Dining overspend from Entertainment").
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub id: i64,
    pub month: NaiveDate,
    pub from: i64,
    pub to: i64,
    pub amount: Decimal,
    pub currency: Currency,
}

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

impl Db {
    /// Category id → the budget that applied in `month` (a budget removed by then is left out).
    pub fn budgets_in(&self, scope: &str, month: NaiveDate) -> Result<BTreeMap<i64, (Decimal, Currency)>> {
        let mut st = self.conn.prepare(
            "SELECT h.category_id, h.amount, h.currency FROM budget_history h
             WHERE h.scope = ?1 AND h.month = (SELECT MAX(month) FROM budget_history WHERE scope = h.scope AND category_id = h.category_id AND month <= ?2)",
        )?;
        let rows = st.query_map(params![scope, month_start(month).to_string()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).map(|(c, a, k)| (c, (crate::db::dec(&a), crate::db::ccy(&k)))).filter(|(_, (a, _))| !a.is_zero()).collect())
    }

    pub fn moves(&self, scope: &str, month: NaiveDate) -> Result<Vec<Move>> {
        let mut st = self.conn.prepare("SELECT id, month, from_category, to_category, amount, currency FROM budget_moves WHERE scope = ?1 AND month = ?2 ORDER BY id")?;
        let rows = st.query_map(params![scope, month_start(month).to_string()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).map(|(id, m, from, to, a, c)| Move { id, month: date(&m), from, to, amount: crate::db::dec(&a), currency: crate::db::ccy(&c) }).collect())
    }

    pub fn add_move(&self, scope: &str, month: NaiveDate, from: i64, to: i64, amount: Decimal, currency: Currency) -> Result<()> {
        self.conn.execute(
            "INSERT INTO budget_moves (scope, month, from_category, to_category, amount, currency, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![scope, month_start(month).to_string(), from, to, amount.abs().to_string(), currency.code()],
        )?;
        Ok(())
    }

    pub fn delete_move(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM budget_moves WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Categories whose leftover (or overspending) carries into the next month → the month that started.
    pub fn rollovers(&self, scope: &str) -> BTreeMap<i64, NaiveDate> {
        self.setting(&format!("budget_rollover:{scope}")).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    }

    pub fn set_rollover(&self, scope: &str, category_id: i64, from: Option<NaiveDate>) -> Result<()> {
        let mut r = self.rollovers(scope);
        match from {
            Some(m) => r.insert(category_id, month_start(m)),
            None => r.remove(&category_id),
        };
        self.set_setting(&format!("budget_rollover:{scope}"), &serde_json::to_string(&r).unwrap_or_default())
    }
}

/// One category's month.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Envelope {
    pub budget: Decimal,
    /// Moved in (+) or out (−) this month.
    pub moved: Decimal,
    /// Left over from last month (negative: last month's overspending), when the category rolls over.
    pub carried: Decimal,
    pub spent: Decimal,
}

impl Envelope {
    pub fn available(&self) -> Decimal {
        self.budget + self.moved + self.carried - self.spent
    }
    /// What you had to spend this month.
    pub fn funded(&self) -> Decimal {
        self.budget + self.moved + self.carried
    }
}

/// Each top-level expense category's `month` (`cats` = id, name): the budget that applied then,
/// money moved, the rollover from the month before, and the spending.
#[allow(clippy::too_many_arguments)]
pub fn envelopes(db: &Db, scope: &str, rows: &[TxRow], cats: &[(i64, String)], month: NaiveDate, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> Result<BTreeMap<i64, Envelope>> {
    let month = month_start(month);
    let starts = db.rollovers(scope);
    // ponytail: walks back at most 24 months; a rollover older than that forgets its earliest leftovers
    let first = starts.values().copied().min().map_or(month, |s| s.max(add_months(month, -24)).min(month));
    let mut prev: BTreeMap<i64, Envelope> = BTreeMap::new();
    let mut m = first;
    loop {
        let budgets = db.budgets_in(scope, m)?;
        let moves = db.moves(scope, m)?;
        let spent: BTreeMap<String, Decimal> = by_category(rows, m, month_end(m), ccy, fx, parents).into_iter().collect();
        let mut cur = BTreeMap::new();
        for (id, name) in cats {
            let budget = budgets.get(id).map_or(Decimal::ZERO, |(a, c)| fx.convert(*a, *c, ccy));
            let moved: Decimal = moves
                .iter()
                .map(|mv| {
                    let v = fx.convert(mv.amount, mv.currency, ccy);
                    if mv.to == *id { v } else if mv.from == *id { -v } else { Decimal::ZERO }
                })
                .sum();
            let carried = match (starts.get(id), prev.get(id)) {
                (Some(s), Some(p)) if *s < m => p.available(),
                _ => Decimal::ZERO,
            };
            cur.insert(*id, Envelope { budget, moved, carried, spent: spent.get(name).copied().unwrap_or_default() });
        }
        if m >= month {
            return Ok(cur);
        }
        prev = cur;
        m = add_months(m, 1);
    }
}

// ──────────────────────────────────────────────────────────────── non-monthly bills

/// A bill that comes once a quarter or once a year, set aside for a little each month.
#[derive(Debug, Clone, PartialEq)]
pub struct Fund {
    pub id: i64,
    pub name: String,
    pub amount: Decimal,
    pub currency: Currency,
    pub every_months: i32,
    pub next_due: NaiveDate,
    /// The merchant it was suggested from, if any.
    pub stem: String,
}

fn months_between(a: NaiveDate, b: NaiveDate) -> i32 {
    use chrono::Datelike;
    (b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32
}

impl Fund {
    pub fn monthly(&self) -> Decimal {
        (self.amount / Decimal::from(self.every_months.max(1))).round_dp(2)
    }
    /// The next due date on or after `today`'s month (a past one moves on by the bill's cycle).
    pub fn due(&self, today: NaiveDate) -> NaiveDate {
        let mut d = self.next_due;
        let mut i = 0;
        while month_start(d) < month_start(today) && i < 100 {
            i += 1;
            d = day_in(add_months(month_start(self.next_due), self.every_months.max(1) * i), self.next_due);
        }
        d
    }
    /// What should be put aside by `today`: one share for each month of the cycle so far, the month
    /// after it was last due being the first and the due month the last.
    pub fn set_aside(&self, today: NaiveDate) -> Decimal {
        let due = self.due(today);
        let last = add_months(month_start(due), -self.every_months.max(1));
        let n = months_between(last, month_start(today)).clamp(1, self.every_months.max(1));
        if n == self.every_months { self.amount } else { self.monthly() * Decimal::from(n) }
    }
}

impl Db {
    pub fn funds(&self, scope: &str) -> Result<Vec<Fund>> {
        let mut st = self.conn.prepare("SELECT id, name, amount, currency, every_months, next_due, stem FROM sinking_funds WHERE scope = ?1 ORDER BY next_due, name")?;
        let rows = st.query_map([scope], |r| {
            Ok(Fund {
                id: r.get(0)?,
                name: r.get(1)?,
                amount: crate::db::dec(&r.get::<_, String>(2)?),
                currency: crate::db::ccy(&r.get::<_, String>(3)?),
                every_months: r.get(4)?,
                next_due: date(&r.get::<_, String>(5)?),
                stem: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// `f.id == 0` adds it.
    pub fn save_fund(&self, scope: &str, f: &Fund) -> Result<()> {
        let (a, due) = (f.amount.abs().to_string(), f.next_due.to_string());
        if f.id == 0 {
            self.conn.execute(
                "INSERT INTO sinking_funds (scope, name, amount, currency, every_months, next_due, stem) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![scope, f.name, a, f.currency.code(), f.every_months.max(1), due, f.stem],
            )?;
        } else {
            self.conn.execute(
                "UPDATE sinking_funds SET name = ?2, amount = ?3, currency = ?4, every_months = ?5, next_due = ?6, stem = ?7 WHERE id = ?1",
                params![f.id, f.name, a, f.currency.code(), f.every_months.max(1), due, f.stem],
            )?;
        }
        Ok(())
    }

    pub fn delete_fund(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM sinking_funds WHERE id = ?1", [id])?;
        Ok(())
    }
}

/// The 1st of `month` (1–12) the next time it comes round: this year if it's this month or later,
/// otherwise next year. For a bill picked by the month it's due.
pub fn due_in_month(month: u32, today: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    let year = if month >= today.month() { today.year() } else { today.year() + 1 };
    NaiveDate::from_ymd_opt(year, month.clamp(1, 12), 1).unwrap_or(today)
}

/// A quarterly or yearly bill seen in the statements, offered as a fund.
#[derive(Debug, Clone, PartialEq)]
pub struct FundIdea {
    pub stem: String,
    pub payee: String,
    /// The latest charge, in the display currency.
    pub amount: Decimal,
    pub every_months: i32,
    pub next_due: NaiveDate,
}

/// Bills charged every three or twelve months (two or more charges at that rhythm, amounts within
/// 25%), and subscriptions billed yearly or quarterly.
pub fn fund_ideas(rows: &[TxRow], ccy: Currency, fx: &Fx, today: NaiveDate) -> Vec<FundIdea> {
    let mut by: BTreeMap<String, Vec<&TxRow>> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer() && t.split_seq.is_none()) {
        by.entry(crate::rules::payer_key(&t.raw_description, &t.memo)).or_default().push(t);
    }
    let idea = |stem: &str, last: &TxRow, cadence: i64| {
        let every = if cadence >= 300 { 12 } else { 3 };
        let amount = -crate::insights::value(last, ccy, fx);
        let mut due = last.posted + chrono::Duration::days(cadence);
        while due < month_start(today) {
            due += chrono::Duration::days(cadence);
        }
        let payee = if last.clean_payee.is_empty() { crate::rules::title_case(stem) } else { last.clean_payee.clone() };
        FundIdea { stem: stem.to_string(), payee, amount: amount.round_dp(2), every_months: every, next_due: due }
    };
    let mut out = Vec::new();
    for (stem, v) in &mut by {
        v.sort_by_key(|t| t.posted);
        let last = v[v.len() - 1];
        let dates: Vec<NaiveDate> = v.iter().map(|t| t.posted).collect();
        let steady = v.iter().all(|t| (t.amount - last.amount).abs() <= last.amount.abs() / Decimal::from(4));
        match crate::rules::cadence(&dates) {
            Some(c @ (91 | 365)) if steady => out.push(idea(stem, last, c)),
            _ => {}
        }
    }
    for s in crate::insights::subscriptions(rows, ccy, fx, today).iter().filter(|s| s.active && s.cadence_days >= 85) {
        if out.iter().any(|o| o.stem == s.stem) {
            continue;
        }
        if let Some(last) = rows.iter().filter(|t| s.tx_ids.contains(&t.id)).max_by_key(|t| t.posted) {
            out.push(idea(&s.stem, last, s.cadence_days));
        }
    }
    out.sort_by_key(|o| o.next_due);
    out
}

// ──────────────────────────────────────────────────────────────── watchlists

/// A merchant (or any wording) with a monthly limit: "Amazon, CI$ 150".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub name: String,
    /// Matched against the statement wording and the payee, ignoring case.
    pub pattern: String,
    pub limit: Decimal,
    pub currency: Currency,
}

impl Db {
    pub fn watchlists(&self, scope: &str) -> Vec<Watch> {
        self.setting(&format!("watchlists:{scope}")).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    }
    pub fn save_watchlists(&self, scope: &str, w: &[Watch]) -> Result<()> {
        self.set_setting(&format!("watchlists:{scope}"), &serde_json::to_string(w).unwrap_or_default())
    }
}

/// What a watch caught between the dates (spending, not transfers) and those transactions.
pub fn watched(rows: &[TxRow], w: &Watch, from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx) -> (Decimal, Vec<i64>) {
    let p = w.pattern.trim().to_uppercase();
    if p.is_empty() {
        return (Decimal::ZERO, Vec::new());
    }
    let hits: Vec<&TxRow> = rows
        .iter()
        .filter(|t| t.posted >= from && t.posted <= to && t.amount.is_sign_negative() && !t.is_transfer())
        .filter(|t| format!("{} {} {}", t.raw_description, t.memo, t.clean_payee).to_uppercase().contains(&p))
        .collect();
    (hits.iter().map(|t| -crate::insights::value(t, ccy, fx)).sum(), hits.iter().map(|t| t.id).collect())
}

// ──────────────────────────────────────────────────────────────── monthly close

/// What a month came to, saved when you close it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CloseSummary {
    pub currency: Option<Currency>,
    pub income: Decimal,
    pub spent: Decimal,
    /// Income − spending.
    pub saved: Decimal,
    pub age_of_money: Option<i64>,
    /// Categories over their budget.
    pub over: usize,
    /// Regular bills that didn't come.
    pub missed: usize,
    pub net_worth: Option<Decimal>,
}

impl Db {
    /// Months you've been through in Monthly Close → (what it came to, when it was closed).
    pub fn closes(&self, scope: &str) -> Result<BTreeMap<NaiveDate, (CloseSummary, String)>> {
        let mut st = self.conn.prepare("SELECT month, summary, closed_at FROM month_closes WHERE scope = ?1")?;
        let rows = st.query_map([scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).map(|(m, s, c)| (date(&m), (serde_json::from_str(&s).unwrap_or_default(), c))).collect())
    }
    pub fn close_month(&self, scope: &str, month: NaiveDate, summary: &CloseSummary) -> Result<()> {
        self.conn.execute(
            "INSERT INTO month_closes VALUES (?1, ?2, ?3, datetime('now')) ON CONFLICT(scope, month) DO UPDATE SET summary = excluded.summary, closed_at = excluded.closed_at",
            params![scope, month_start(month).to_string(), serde_json::to_string(summary)?],
        )?;
        Ok(())
    }
    pub fn reopen_month(&self, scope: &str, month: NaiveDate) -> Result<()> {
        self.conn.execute("DELETE FROM month_closes WHERE scope = ?1 AND month = ?2", params![scope, month_start(month).to_string()])?;
        Ok(())
    }
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
    fn a_bonus_doesnt_hide_the_salary() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let mut t = db.transactions(&TxFilter::default()).unwrap().into_iter().find(|t| !t.is_transfer() && t.amount > Decimal::ZERO).unwrap();
        t.category_name = None;
        t.raw_description = "ELECTRONIC CREDIT".into();
        t.clean_payee = String::new();
        let pay = |day: u32, mo: u32, amt: &str| {
            let mut x = t.clone();
            x.posted = NaiveDate::from_ymd_opt(2026, mo, day).unwrap();
            x.amount = d(amt);
            x
        };
        // every two weeks, with one bonus payday
        // (the owner's real pattern: a bonus, two refunds and a penny test under the same wording)
        let rows = vec![
            pay(29, 5, "2720.64"), pay(11, 6, "2720.64"), pay(23, 6, "297.00"), pay(23, 6, "297.00"), pay(25, 6, "2720.64"), pay(9, 7, "2720.64"), pay(24, 7, "2720.64"),
            pay(7, 8, "4122.51"), pay(21, 8, "2720.64"), pay(4, 9, "0.01"), pay(4, 9, "2720.64"),
        ];
        let s = detect_salary(&rows, NaiveDate::from_ymd_opt(2026, 9, 15).unwrap()).expect("the salary");
        assert_eq!((s.amount, s.cycle), (d("2720.64"), PayCycle::Fortnightly));
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

    #[test]
    fn suggestions_use_only_months_the_statements_cover() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let fx = db.fx().unwrap();
        let all = db.categories().unwrap();
        let parents: BTreeMap<i64, String> = all.iter().filter_map(|c| Some((c.id, all.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect();
        // statements from 18 May to 15 Sep: only June, July and August are whole
        let (a, b) = (NaiveDate::from_ymd_opt(2026, 5, 18).unwrap(), NaiveDate::from_ymd_opt(2026, 9, 15).unwrap());
        let rows: Vec<TxRow> = db.transactions(&TxFilter { split_lines: true, ..Default::default() }).unwrap().into_iter().filter(|t| t.posted >= a && t.posted <= b).collect();
        let month = |m: u32| {
            let f = NaiveDate::from_ymd_opt(2026, m, 1).unwrap();
            by_category(&rows, f, month_end(f), Currency::KYD, &fx, &parents).into_iter().find(|c| c.0 == "Groceries").map(|c| c.1).unwrap_or_default()
        };
        let mut v = [month(6), month(7), month(8)];
        v.sort();
        let want = (v[1] / Decimal::TEN).ceil() * Decimal::TEN;
        let got = suggest(&rows, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), 6, Currency::KYD, &fx, &parents);
        assert_eq!(got["Groceries"], want);
    }

    #[test]
    fn rollovers_moves_and_budget_history() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter { split_lines: true, ..Default::default() }).unwrap();
        let fx = db.fx().unwrap();
        let all = db.categories().unwrap();
        let parents: BTreeMap<i64, String> = all.iter().filter_map(|c| Some((c.id, all.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect();
        let id = |n: &str| all.iter().find(|c| c.name == n).unwrap().id;
        let (g, e) = (id("Groceries"), id("Entertainment"));
        let cats = vec![(g, "Groceries".to_string()), (e, "Entertainment".to_string())];
        let m = |mo: u32| NaiveDate::from_ymd_opt(2026, mo, 1).unwrap();
        let sc = "1";
        db.set_budget_from(sc, g, Some((d("500"), Currency::KYD)), m(7)).unwrap();
        db.set_budget_from(sc, g, Some((d("700"), Currency::KYD)), m(9)).unwrap();
        assert_eq!(db.budgets_in(sc, m(8)).unwrap()[&g].0, d("500"));
        assert_eq!(db.budgets_in(sc, m(9)).unwrap()[&g].0, d("700"));
        // the first budget reaches back; removing it later stops it from then on
        assert_eq!(db.budgets_in(sc, m(6)).unwrap()[&g].0, d("500"));
        db.set_budget_from(sc, e, None, m(9)).unwrap();
        db.set_budget_from(sc, e, Some((d("80"), Currency::KYD)), m(10)).unwrap();
        assert!(!db.budgets_in(sc, m(9)).unwrap().contains_key(&e));

        let spent = |mo: u32| by_category(&rows, m(mo), month_end(m(mo)), Currency::KYD, &fx, &parents).into_iter().find(|c| c.0 == "Groceries").map(|c| c.1).unwrap_or_default();
        // no rollover: each month stands alone
        let env = envelopes(&db, sc, &rows, &cats, m(9), Currency::KYD, &fx, &parents).unwrap();
        assert_eq!(env[&g], Envelope { budget: d("700"), moved: Decimal::ZERO, carried: Decimal::ZERO, spent: spent(9) });
        // rolling over from July: August starts with July's leftover, September with August's
        db.set_rollover(sc, g, Some(m(7))).unwrap();
        let jul = d("500") - spent(7);
        let aug = d("500") + jul - spent(8);
        let env = envelopes(&db, sc, &rows, &cats, m(9), Currency::KYD, &fx, &parents).unwrap();
        assert_eq!(env[&g].carried, aug);
        assert_eq!(env[&g].available(), d("700") + aug - spent(9));
        // moving 50 from Entertainment into Groceries in September
        db.add_move(sc, m(9), e, g, d("50"), Currency::KYD).unwrap();
        let env = envelopes(&db, sc, &rows, &cats, m(9), Currency::KYD, &fx, &parents).unwrap();
        assert_eq!(env[&g].moved, d("50"));
        assert_eq!(env[&e].moved, d("-50"));
        let mv = db.moves(sc, m(9)).unwrap();
        db.delete_move(mv[0].id).unwrap();
        assert!(db.moves(sc, m(9)).unwrap().is_empty());
        db.set_rollover(sc, g, None).unwrap();
        assert!(db.rollovers(sc).is_empty());
    }

    #[test]
    fn a_bill_due_in_a_month_is_the_next_one() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(due_in_month(10, d(2026, 10, 28)), d(2026, 10, 1)); // this month still counts
        assert_eq!(due_in_month(12, d(2026, 10, 28)), d(2026, 12, 1));
        assert_eq!(due_in_month(3, d(2026, 10, 28)), d(2027, 3, 1));
    }

    #[test]
    fn funds_set_aside_a_share_a_month() {
        let f = Fund { id: 0, name: "Car insurance".into(), amount: d("1200"), currency: Currency::KYD, every_months: 12, next_due: NaiveDate::from_ymd_opt(2027, 3, 15).unwrap(), stem: String::new() };
        assert_eq!(f.monthly(), d("100"));
        // April to October 2026: seven shares
        assert_eq!(f.set_aside(NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()), d("700"));
        assert_eq!(f.set_aside(NaiveDate::from_ymd_opt(2027, 3, 2).unwrap()), d("1200"));
        // after it's due, the next year starts
        assert_eq!(f.due(NaiveDate::from_ymd_opt(2027, 4, 1).unwrap()), NaiveDate::from_ymd_opt(2028, 3, 15).unwrap());
        assert_eq!(f.set_aside(NaiveDate::from_ymd_opt(2027, 4, 1).unwrap()), d("100"));

        let db = Db::open_in_memory().unwrap();
        db.save_fund("1", &f).unwrap();
        let mut saved = db.funds("1").unwrap().remove(0);
        saved.amount = d("1300");
        db.save_fund("1", &saved).unwrap();
        assert_eq!(db.funds("1").unwrap()[0].amount, d("1300"));
        db.delete_fund(saved.id).unwrap();
        assert!(db.funds("1").unwrap().is_empty());
    }

    #[test]
    fn watchlists_and_closes() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let fx = db.fx().unwrap();
        let w = Watch { name: "Amazon".into(), pattern: "amazon".into(), limit: d("100"), currency: Currency::KYD };
        db.save_watchlists("1", std::slice::from_ref(&w)).unwrap();
        assert_eq!(db.watchlists("1"), vec![w.clone()]);
        let (spent, ids) = watched(&rows, &w, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(), Currency::KYD, &fx);
        assert!(spent > Decimal::ZERO && !ids.is_empty());

        let sep = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let sum = CloseSummary { income: d("5000"), spent: d("4200"), saved: d("800"), age_of_money: Some(21), ..Default::default() };
        db.close_month("1", sep, &sum).unwrap();
        assert_eq!(db.closes("1").unwrap()[&sep].0, sum);
        db.reopen_month("1", sep).unwrap();
        assert!(db.closes("1").unwrap().is_empty());
        let ideas = fund_ideas(&rows, Currency::KYD, &fx, NaiveDate::from_ymd_opt(2026, 10, 27).unwrap());
        println!("{ideas:?}");
    }
}
