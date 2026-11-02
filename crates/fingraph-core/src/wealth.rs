//! Net worth (statement balances plus things you own or owe that no statement shows), goals (save
//! up for something, or pay something down) and a debt payoff planner (avalanche vs snowball).

use chrono::{Datelike, NaiveDate};
use rusqlite::params;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

use crate::db::{Db, DocumentRow, TxRow};
use crate::insights::{add_months, month_start};
use crate::{Currency, Fx, Result};

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

fn month_end(m: NaiveDate) -> NaiveDate {
    add_months(month_start(m), 1).pred_opt().unwrap_or(m)
}

fn months_between(a: NaiveDate, b: NaiveDate) -> i32 {
    (b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32
}

/// The last value on or before `d`.
pub fn value_at(points: &[(NaiveDate, Decimal)], d: NaiveDate) -> Option<Decimal> {
    points.iter().rev().find(|p| p.0 <= d).map(|p| p.1)
}

// ──────────────────────────────────────────────────────────────── things not on a statement

/// The house, the car, a pension, a family loan: valued by hand from time to time.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualItem {
    pub id: i64,
    pub entity_id: i64,
    pub name: String,
    pub liability: bool,
    pub currency: Currency,
    /// Interest a year, in percent (debts).
    pub apr: Option<Decimal>,
    /// (as of, value), oldest first; a debt's value is what's owed (positive).
    pub values: Vec<(NaiveDate, Decimal)>,
}

impl Db {
    pub fn manual_items(&self, entity_ids: &[i64]) -> Result<Vec<ManualItem>> {
        let mut st = self.conn.prepare("SELECT id, entity_id, name, liability, currency, apr FROM manual_items ORDER BY liability, name")?;
        let rows = st.query_map([], |r| {
            Ok(ManualItem {
                id: r.get(0)?,
                entity_id: r.get(1)?,
                name: r.get(2)?,
                liability: r.get::<_, i64>(3)? != 0,
                currency: crate::db::ccy(&r.get::<_, String>(4)?),
                apr: r.get::<_, Option<String>>(5)?.map(|s| crate::db::dec(&s)),
                values: Vec::new(),
            })
        })?;
        let mut out: Vec<ManualItem> = rows.filter_map(|r| r.ok()).filter(|i| entity_ids.is_empty() || entity_ids.contains(&i.entity_id)).collect();
        let mut vs = self.conn.prepare("SELECT as_of, value FROM manual_values WHERE item_id = ?1 ORDER BY as_of")?;
        for i in &mut out {
            i.values = vs.query_map([i.id], |r| Ok((date(&r.get::<_, String>(0)?), crate::db::dec(&r.get::<_, String>(1)?))))?.filter_map(|r| r.ok()).collect();
        }
        Ok(out)
    }

    /// `i.id == 0` adds it; returns the id. Values are kept separately (`set_manual_value`).
    pub fn save_manual_item(&self, i: &ManualItem) -> Result<i64> {
        let apr = i.apr.map(|a| a.to_string());
        if i.id == 0 {
            self.conn.execute(
                "INSERT INTO manual_items (entity_id, name, liability, currency, apr) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![i.entity_id, i.name, i.liability as i64, i.currency.code(), apr],
            )?;
            Ok(self.conn.last_insert_rowid())
        } else {
            self.conn.execute("UPDATE manual_items SET entity_id = ?2, name = ?3, liability = ?4, currency = ?5, apr = ?6 WHERE id = ?1", params![i.id, i.entity_id, i.name, i.liability as i64, i.currency.code(), apr])?;
            Ok(i.id)
        }
    }

    pub fn delete_manual_item(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM manual_values WHERE item_id = ?1", [id])?;
        self.conn.execute("DELETE FROM manual_items WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn set_manual_value(&self, id: i64, as_of: NaiveDate, value: Decimal) -> Result<()> {
        self.conn.execute(
            "INSERT INTO manual_values VALUES (?1, ?2, ?3) ON CONFLICT(item_id, as_of) DO UPDATE SET value = excluded.value",
            params![id, as_of.to_string(), value.abs().to_string()],
        )?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────── net worth

/// One account's verified balances over time, in its own currency (cards and loans negative).
#[derive(Debug, Clone)]
pub struct AccountBalances {
    pub name: String,
    pub currency: Currency,
    pub liability: bool,
    pub points: Vec<(NaiveDate, Decimal)>,
}

/// Every account's verified balances: running balances, then statement closings.
pub fn account_balances(rows: &[TxRow], docs: &[DocumentRow]) -> BTreeMap<i64, AccountBalances> {
    let mut by: BTreeMap<i64, AccountBalances> = crate::insights::own_accounts(rows)
        .into_iter()
        .map(|(a, t)| (a, AccountBalances { name: t.account_name.clone(), currency: t.currency, liability: t.account_type.is_liability(), points: Vec::new() }))
        .collect();
    for t in rows {
        let Some(e) = by.get_mut(&t.account_id) else { continue };
        if let Some(b) = t.balance {
            e.points.push((t.posted, if e.liability { -b } else { b }));
        }
    }
    for d in docs {
        if let (Some(c), Some((_, end)), Some(e)) = (d.closing, d.period, by.get_mut(&d.account_id)) {
            // a closing balance covers its whole day
            e.points.retain(|p| p.0 != end);
            e.points.push((end, if e.liability { -c } else { c }));
        }
    }
    for e in by.values_mut() {
        e.points.sort_by_key(|p| p.0);
        let mut v: Vec<(NaiveDate, Decimal)> = Vec::new();
        for p in &e.points {
            match v.last_mut() {
                Some(l) if l.0 == p.0 => *l = *p,
                _ => v.push(*p),
            }
        }
        e.points = v;
    }
    by
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetWorth {
    pub date: NaiveDate,
    pub assets: Decimal,
    /// What's owed, positive.
    pub debts: Decimal,
}

impl NetWorth {
    pub fn net(&self) -> Decimal {
        self.assets - self.debts
    }
}

/// Net worth on `d`: each account's last verified balance on or before it, and each item's last value.
pub fn net_worth_on(accounts: &BTreeMap<i64, AccountBalances>, items: &[ManualItem], d: NaiveDate, ccy: Currency, fx: &Fx) -> NetWorth {
    let (mut assets, mut debts) = (Decimal::ZERO, Decimal::ZERO);
    for a in accounts.values() {
        // at that day's rate; an account in a currency with no rate yet is left out, not counted 1:1
        if let Some(v) = value_at(&a.points, d).and_then(|v| crate::insights::known(fx, v, a.currency, ccy, d)) {
            if v.is_sign_negative() { debts -= v } else { assets += v }
        }
    }
    for i in items {
        if let Some(v) = value_at(&i.values, d).and_then(|v| crate::insights::known(fx, v, i.currency, ccy, d)) {
            if i.liability { debts += v } else { assets += v }
        }
    }
    NetWorth { date: d, assets: assets.round_dp(2), debts: debts.round_dp(2) }
}

/// Month ends for the `n` months up to `end` (the last one is `end` itself).
pub fn net_worth_series(accounts: &BTreeMap<i64, AccountBalances>, items: &[ManualItem], end: NaiveDate, n: i32, ccy: Currency, fx: &Fx) -> Vec<NetWorth> {
    (0..n).rev().map(|i| if i == 0 { end } else { month_end(add_months(month_start(end), -i)) }).map(|d| net_worth_on(accounts, items, d, ccy, fx)).collect()
}

// ──────────────────────────────────────────────────────────────── goals

#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    pub id: i64,
    pub name: String,
    /// Pay something down (a card, a loan) rather than save up.
    pub paydown: bool,
    /// Save: the amount to reach. Pay down: the amount to pay off.
    pub target: Decimal,
    pub currency: Currency,
    pub target_date: Option<NaiveDate>,
    /// Planned each month.
    pub monthly: Decimal,
    /// Track a statement account instead of recording contributions.
    pub account_id: Option<i64>,
    /// Save: already saved (or the account's balance) when the goal was made. Pay down: owed then.
    pub start_value: Decimal,
    pub created: NaiveDate,
}

impl Db {
    pub fn goals(&self, scope: &str) -> Result<Vec<Goal>> {
        let mut st = self.conn.prepare("SELECT id, name, paydown, target, currency, target_date, monthly, account_id, start_value, created FROM goals WHERE scope = ?1 ORDER BY id")?;
        let rows = st.query_map([scope], |r| {
            Ok(Goal {
                id: r.get(0)?,
                name: r.get(1)?,
                paydown: r.get::<_, i64>(2)? != 0,
                target: crate::db::dec(&r.get::<_, String>(3)?),
                currency: crate::db::ccy(&r.get::<_, String>(4)?),
                target_date: r.get::<_, Option<String>>(5)?.map(|s| date(&s)),
                monthly: crate::db::dec(&r.get::<_, String>(6)?),
                account_id: r.get(7)?,
                start_value: crate::db::dec(&r.get::<_, String>(8)?),
                created: date(&r.get::<_, String>(9)?),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// `g.id == 0` adds it.
    pub fn save_goal(&self, scope: &str, g: &Goal) -> Result<i64> {
        let (t, m, sv, td) = (g.target.abs().to_string(), g.monthly.abs().to_string(), g.start_value.to_string(), g.target_date.map(|d| d.to_string()));
        if g.id == 0 {
            self.conn.execute(
                "INSERT INTO goals (scope, name, paydown, target, currency, target_date, monthly, account_id, start_value, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![scope, g.name, g.paydown as i64, t, g.currency.code(), td, m, g.account_id, sv, g.created.to_string()],
            )?;
            Ok(self.conn.last_insert_rowid())
        } else {
            self.conn.execute(
                "UPDATE goals SET name = ?2, paydown = ?3, target = ?4, currency = ?5, target_date = ?6, monthly = ?7, account_id = ?8, start_value = ?9 WHERE id = ?1",
                params![g.id, g.name, g.paydown as i64, t, g.currency.code(), td, m, g.account_id, sv],
            )?;
            Ok(g.id)
        }
    }

    pub fn delete_goal(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM goal_entries WHERE goal_id = ?1", [id])?;
        self.conn.execute("DELETE FROM goals WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Month → put towards the goal (goals without an account).
    pub fn goal_entries(&self, id: i64) -> Result<BTreeMap<NaiveDate, Decimal>> {
        let mut st = self.conn.prepare("SELECT month, amount FROM goal_entries WHERE goal_id = ?1")?;
        let rows = st.query_map([id], |r| Ok((date(&r.get::<_, String>(0)?), crate::db::dec(&r.get::<_, String>(1)?))))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// `None` (or zero) clears the month.
    pub fn set_goal_entry(&self, id: i64, month: NaiveDate, amount: Option<Decimal>) -> Result<()> {
        let m = month_start(month).to_string();
        match amount.filter(|a| !a.is_zero()) {
            Some(a) => self.conn.execute("INSERT INTO goal_entries VALUES (?1, ?2, ?3) ON CONFLICT(goal_id, month) DO UPDATE SET amount = excluded.amount", params![id, m, a.to_string()])?,
            None => self.conn.execute("DELETE FROM goal_entries WHERE goal_id = ?1 AND month = ?2", params![id, m])?,
        };
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Done,
    Ahead,
    OnTrack,
    AtRisk,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Ahead => "Ahead",
            Self::OnTrack => "On track",
            Self::AtRisk => "At risk",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Saved so far, or paid off so far.
    pub done: Decimal,
    pub remaining: Decimal,
    pub status: GoalStatus,
    /// At the planned monthly amount.
    pub finish: Option<NaiveDate>,
    /// A month, to make the target date (when there is one).
    pub needed: Option<Decimal>,
}

/// `balance` = the linked account's latest balance in the goal's currency (cards and loans
/// negative); `entries` = contributions recorded.
pub fn progress(g: &Goal, balance: Option<Decimal>, entries: Decimal, today: NaiveDate) -> Progress {
    let done = match (g.paydown, balance.filter(|_| g.account_id.is_some())) {
        (false, Some(b)) => b,
        (false, None) => g.start_value + entries,
        (true, Some(b)) => g.start_value - (-b).max(Decimal::ZERO),
        (true, None) => entries,
    };
    let remaining = (g.target - done).max(Decimal::ZERO);
    // what's come in since the goal was made, against the plan's pace
    let gained = if g.paydown { done } else { done - g.start_value };
    let elapsed = months_between(g.created, today).max(0);
    // more than a century away (or no plan) is never
    let finish = (g.monthly > Decimal::ZERO).then(|| (remaining / g.monthly).ceil()).and_then(|m| i32::try_from(m).ok()).filter(|m| *m <= 1200).map(|m| add_months(month_start(today), m));
    let needed = g.target_date.map(|t| {
        let left = months_between(today, t).max(1);
        (remaining / Decimal::from(left)).round_dp(2)
    });
    let pace = Decimal::from(elapsed) * g.monthly;
    let status = if remaining.is_zero() {
        GoalStatus::Done
    } else if let Some(t) = g.target_date {
        // with a date, it's about getting there in time at the planned amount
        if finish.is_none_or(|f| month_start(f) > month_start(t)) {
            GoalStatus::AtRisk
        } else if needed.is_some_and(|n| n <= g.monthly * Decimal::new(8, 1)) {
            GoalStatus::Ahead
        } else {
            GoalStatus::OnTrack
        }
    } else if elapsed == 0 || pace.is_zero() {
        GoalStatus::OnTrack
    } else if gained >= pace * Decimal::new(11, 1) {
        GoalStatus::Ahead
    } else if gained >= pace * Decimal::new(9, 1) {
        GoalStatus::OnTrack
    } else {
        GoalStatus::AtRisk
    };
    Progress { done, remaining, status, finish: finish.filter(|_| !remaining.is_zero()), needed }
}

// ──────────────────────────────────────────────────────────────── paying off debt

#[derive(Debug, Clone, PartialEq)]
pub struct Debt {
    pub name: String,
    /// Owed, positive.
    pub balance: Decimal,
    /// Percent a year.
    pub apr: Decimal,
}

// ponytail: one rule for every card and loan (3% of the balance, at least CI$ 25); a loan's real
// instalment is usually higher, so its payoff here is the slowest it could be
pub const MIN_PERCENT: Decimal = Decimal::from_parts(3, 0, 0, false, 0);
pub const MIN_FLOOR: Decimal = Decimal::from_parts(25, 0, 0, false, 0);

pub fn min_payment(balance: Decimal) -> Decimal {
    (balance * MIN_PERCENT / Decimal::ONE_HUNDRED).round_dp(2).max(MIN_FLOOR).min(balance)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Highest interest first: the least interest paid.
    Avalanche,
    /// Smallest balance first: the quickest wins.
    Snowball,
    /// Only each debt's minimum, nothing extra: the baseline the others are measured against.
    Minimums,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Payoff {
    pub months: u32,
    pub interest: Decimal,
    /// Each debt and the month (1 = next month) it's paid off, in that order.
    pub order: Vec<(String, u32)>,
}

/// Pay `monthly` in all every month: each debt's minimum, and everything left over to the target
/// debt (highest rate or smallest balance); a paid-off debt's minimum then rolls into the next.
/// `None` when `monthly` doesn't cover the minimums, or it would take more than 50 years.
pub fn payoff(debts: &[Debt], monthly: Decimal, strategy: Strategy) -> Option<Payoff> {
    let mut bal: Vec<Decimal> = debts.iter().map(|d| d.balance.max(Decimal::ZERO)).collect();
    if strategy != Strategy::Minimums && monthly < bal.iter().map(|b| min_payment(*b)).sum::<Decimal>() {
        return None;
    }
    let mut order: Vec<(String, u32)> = Vec::new();
    let mut interest = Decimal::ZERO;
    let twelve_hundred = Decimal::from(1200);
    let mut before: Decimal = bal.iter().sum();
    for month in 1..=600u32 {
        if bal.iter().all(|b| b.is_zero()) {
            return Some(Payoff { months: month - 1, interest: interest.round_dp(2), order });
        }
        for (i, d) in debts.iter().enumerate() {
            let x = (bal[i] * d.apr / twelve_hundred).round_dp(2);
            interest += x;
            bal[i] += x;
        }
        let mut left = if strategy == Strategy::Minimums { Decimal::MAX } else { monthly };
        for b in bal.iter_mut() {
            let p = min_payment(*b).min(left);
            *b -= p;
            left -= p;
        }
        let mut open: Vec<usize> = (0..debts.len()).filter(|i| !bal[*i].is_zero()).collect();
        match strategy {
            Strategy::Avalanche => open.sort_by(|a, b| debts[*b].apr.cmp(&debts[*a].apr).then(bal[*a].cmp(&bal[*b]))),
            Strategy::Snowball => open.sort_by(|a, b| bal[*a].cmp(&bal[*b]).then(debts[*b].apr.cmp(&debts[*a].apr))),
            Strategy::Minimums => open.clear(),
        }
        for i in open {
            let p = left.min(bal[i]);
            bal[i] -= p;
            left -= p;
        }
        // owing as much as last month (interest outruns the payments) means it never ends, and
        // compounding it for 50 years would overflow Decimal
        let owed: Decimal = bal.iter().sum();
        if owed >= before {
            return None;
        }
        before = owed;
        for (i, d) in debts.iter().enumerate() {
            if bal[i].is_zero() && !order.iter().any(|o| o.0 == d.name) && d.balance > Decimal::ZERO {
                order.push((d.name.clone(), month));
            }
        }
    }
    None
}

#[test]
fn minimums_only_takes_longest_and_costs_most() {
    let d = |v: &str| v.parse::<Decimal>().unwrap();
    let debts = [Debt { name: "Card".into(), balance: d("3000"), apr: d("24") }, Debt { name: "Loan".into(), balance: d("2000"), apr: d("9") }];
    let a = payoff(&debts, d("400"), Strategy::Avalanche).unwrap();
    let m = payoff(&debts, Decimal::ZERO, Strategy::Minimums).unwrap();
    assert!(m.months > a.months && m.interest > a.interest);
}

#[test]
fn payoff_gives_up_when_interest_outruns_payments() {
    let d = [Debt { name: "Card".into(), balance: Decimal::from(5000), apr: Decimal::from(9000) }];
    assert_eq!(payoff(&d, Decimal::from(200), Strategy::Avalanche), None);
}

static INTEREST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\binterest\b").unwrap());

/// A card or loan's interest rate from its statements: the interest charged over its last three
/// months of statements (fewer when there are fewer), as a yearly percentage of what's owed now. `None` without interest charges.
pub fn estimate_apr(rows: &[TxRow], account_id: i64, owed: Decimal) -> Option<Decimal> {
    if owed <= Decimal::ZERO {
        return None;
    }
    let mine: Vec<&TxRow> = rows.iter().filter(|t| t.account_id == account_id).collect();
    let last = mine.iter().map(|t| t.posted).max()?;
    let since = add_months(month_start(last), -2);
    let window: Vec<&&TxRow> = mine.iter().filter(|t| t.posted >= since).collect();
    // interest only: its own sub-categories or the word, not the fees filed beside it in "Bank Fees & Interest"
    let interest: Decimal = window
        .iter()
        .filter(|t| t.amount.is_sign_negative())
        .filter(|t| matches!(t.category_name.as_deref(), Some("Credit Card Interest" | "Loan Interest")) || INTEREST.is_match(&format!("{} {}", t.raw_description, t.memo)))
        .map(|t| -t.amount)
        .sum();
    // a month's worth per statement read (at most the three), else per calendar month seen
    let docs: std::collections::BTreeSet<i64> = window.iter().filter_map(|t| t.document_id).collect();
    let months = if docs.is_empty() { window.iter().map(|t| month_start(t.posted)).collect::<std::collections::BTreeSet<_>>().len() } else { docs.len() };
    let months = Decimal::from(months.clamp(1, 3));
    (!interest.is_zero()).then(|| (interest / months / owed * Decimal::from(1200)).round_dp(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TxFilter;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }
    fn ymd(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    #[test]
    fn net_worth_from_statements_and_items() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let docs = db.documents(&[]).unwrap();
        let fx = db.fx().unwrap();
        let acc = account_balances(&rows, &docs);
        assert!(acc.values().any(|a| a.liability && a.points.iter().all(|p| p.1 <= Decimal::ZERO)));
        let end = ymd(2026, 10, 31);
        let before = net_worth_on(&acc, &[], end, Currency::KYD, &fx);
        assert!(before.assets > Decimal::ZERO && before.debts > Decimal::ZERO);

        let id = db.save_manual_item(&ManualItem { id: 0, entity_id: 1, name: "Car".into(), liability: false, currency: Currency::KYD, apr: None, values: Vec::new() }).unwrap();
        db.set_manual_value(id, ymd(2026, 1, 1), d("20000")).unwrap();
        db.set_manual_value(id, ymd(2026, 9, 1), d("18000")).unwrap();
        let items = db.manual_items(&[]).unwrap();
        assert_eq!(items[0].values.len(), 2);
        let after = net_worth_on(&acc, &items, end, Currency::KYD, &fx);
        assert_eq!(after.assets - before.assets, d("18000"));
        assert_eq!(net_worth_on(&acc, &items, ymd(2026, 3, 31), Currency::KYD, &fx).assets - net_worth_on(&acc, &[], ymd(2026, 3, 31), Currency::KYD, &fx).assets, d("20000"));
        let s = net_worth_series(&acc, &items, end, 12, Currency::KYD, &fx);
        assert_eq!(s.len(), 12);
        assert_eq!(s.last().unwrap().date, end);
        db.delete_manual_item(id).unwrap();
        assert!(db.manual_items(&[]).unwrap().is_empty());
    }

    #[test]
    fn goal_progress_and_status() {
        let today = ymd(2026, 10, 27);
        // no date: judged on pace. 4 months × 500 planned
        let g = Goal { id: 0, name: "Emergency fund".into(), paydown: false, target: d("6000"), currency: Currency::KYD, target_date: None, monthly: d("500"), account_id: None, start_value: d("1000"), created: ymd(2026, 6, 1) };
        let p = progress(&g, None, d("2000"), today);
        assert_eq!((p.done, p.remaining, p.status), (d("3000"), d("3000"), GoalStatus::OnTrack));
        // 3,000 to go at 500 a month: April 2027
        assert_eq!(p.finish, Some(ymd(2027, 4, 1)));
        assert_eq!(progress(&g, None, d("2500"), today).status, GoalStatus::Ahead);
        assert_eq!(progress(&g, None, d("500"), today).status, GoalStatus::AtRisk);
        assert_eq!(progress(&g, None, d("5000"), today).status, GoalStatus::Done);
        // with a date: getting there in time. October 2027 needs 3,000 ÷ 12 = 250 a month, half the plan
        let dated = Goal { target_date: Some(ymd(2027, 10, 1)), ..g.clone() };
        let p = progress(&dated, None, d("2000"), today);
        assert_eq!((p.needed, p.status), (Some(d("250")), GoalStatus::Ahead));
        // May 2027 needs 3,000 ÷ 7 = 428.57 of the 500: on track
        assert_eq!(progress(&Goal { target_date: Some(ymd(2027, 5, 1)), ..g.clone() }, None, d("2000"), today).status, GoalStatus::OnTrack);
        // January 2027 is before April
        assert_eq!(progress(&Goal { target_date: Some(ymd(2027, 1, 1)), ..g.clone() }, None, d("2000"), today).status, GoalStatus::AtRisk);
        // paying down a card linked to its account: owed 4,000 at the start, 2,500 now
        let card = Goal { paydown: true, target: d("4000"), start_value: d("4000"), account_id: Some(7), target_date: None, ..g.clone() };
        let p = progress(&card, Some(d("-2500")), Decimal::ZERO, today);
        assert_eq!((p.done, p.remaining), (d("1500"), d("2500")));

        let db = Db::open_in_memory().unwrap();
        let id = db.save_goal("1", &g).unwrap();
        db.set_goal_entry(id, ymd(2026, 9, 1), Some(d("500"))).unwrap();
        db.set_goal_entry(id, ymd(2026, 10, 1), Some(d("400"))).unwrap();
        assert_eq!(db.goal_entries(id).unwrap().values().copied().sum::<Decimal>(), d("900"));
        assert_eq!(db.goals("1").unwrap()[0].name, "Emergency fund");
        db.delete_goal(id).unwrap();
        assert!(db.goals("1").unwrap().is_empty());
    }

    #[test]
    fn avalanche_pays_less_interest_than_snowball() {
        let debts = vec![
            Debt { name: "Card".into(), balance: d("5000"), apr: d("21.99") },
            Debt { name: "Store card".into(), balance: d("800"), apr: d("12") },
            Debt { name: "Car loan".into(), balance: d("9000"), apr: d("7.5") },
        ];
        let a = payoff(&debts, d("800"), Strategy::Avalanche).unwrap();
        let s = payoff(&debts, d("800"), Strategy::Snowball).unwrap();
        println!("{a:?}\n{s:?}");
        assert!(a.interest < s.interest);
        assert_eq!(a.order[0].0, "Card");
        assert_eq!(s.order[0].0, "Store card");
        assert_eq!(a.order.len(), 3);
        // not enough for the minimums
        assert!(payoff(&debts, d("100"), Strategy::Avalanche).is_none());
        assert_eq!(min_payment(d("500")), d("25"));
        assert_eq!(min_payment(d("10000")), d("300"));
    }

    #[test]
    fn apr_from_interest_charges() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let docs = db.documents(&[]).unwrap();
        let acc = account_balances(&rows, &docs);
        // the demo loan charges 0.75% a month on what's owed: about 9% a year
        let (id, a) = acc.iter().find(|(id, _)| rows.iter().any(|t| t.account_id == **id && t.raw_description == "LOAN INTEREST")).expect("the loan");
        let owed = -a.points.last().unwrap().1;
        let apr = estimate_apr(&rows, *id, owed).unwrap();
        println!("{} owed {owed}: {apr}%", a.name);
        assert!(apr > d("8") && apr < d("10"), "{apr}");
        // a card that charged interest once, months ago, has no rate to go on
        let card = acc.iter().find(|(_, a)| a.liability && a.name.contains("Card")).unwrap();
        assert_eq!(estimate_apr(&rows, *card.0, d("1000")), None);
    }

    #[test]
    fn interest_is_interest_and_one_statement_is_one_month() {
        let mut fee = crate::insights::tests::row(1, ymd(2026, 9, 10), "ANNUAL FEE", "-95.00", "Bank Fees & Interest", "");
        fee.account_id = 7;
        assert_eq!(estimate_apr(&[fee.clone()], 7, d("2000")), None);
        let mut int = crate::insights::tests::row(2, ymd(2026, 10, 14), "INTEREST CHARGE", "-50.00", "Credit Card Interest", "");
        (int.account_id, int.document_id) = (7, Some(1));
        assert_eq!(estimate_apr(&[int.clone()], 7, d("3000")), Some(d("20.0")));
        // a fee beside it doesn't count, and three statements are three months
        let more: Vec<TxRow> = [(3, 8, 14, 2), (4, 9, 14, 3)].into_iter().map(|(id, m, dd, doc)| TxRow { id, posted: ymd(2026, m, dd), document_id: Some(doc), ..int.clone() }).collect();
        assert_eq!(estimate_apr(&[vec![fee, int], more].concat(), 7, d("3000")), Some(d("20.0")));
    }

    #[test]
    fn a_goal_a_lifetime_away_is_at_risk() {
        let g = Goal { id: 0, name: "x".into(), paydown: false, target: d("50000"), currency: Currency::KYD, target_date: Some(ymd(2027, 1, 1)), monthly: d("0.01"), account_id: None, start_value: Decimal::ZERO, created: ymd(2026, 1, 1) };
        let p = progress(&g, None, Decimal::ZERO, ymd(2026, 10, 1));
        assert_eq!((p.status, p.finish), (GoalStatus::AtRisk, None));
    }
}
