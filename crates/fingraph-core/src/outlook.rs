//! Reading forward from the last statement: how old the money you spend is (Age of Money), which
//! regular bills a month should have had and whether they came, and where each account's balance
//! is probably heading until the next statement arrives. Pass whole statement rows, not split
//! lines (`TxFilter::split_lines` off), so a split charge still reads as one payment. Statements come a month at a time, so all
//! of it is worked out from verified history, and says how stale it is.

use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, VecDeque};

use crate::db::{DocumentRow, TxRow};
use crate::insights::{add_months, latest_balances, month_start, own_accounts, value};
use crate::rules::{cadence, merchant_stem, payer_key, title_case};
use crate::{Currency, Fx};

fn month_end(m: NaiveDate) -> NaiveDate {
    add_months(month_start(m), 1).pred_opt().unwrap_or(m)
}

/// The `k`-th due date after a charge on `prev`: monthly, quarterly and yearly cadences step by calendar months
/// (the 31st becomes the 28th in February, and stays the 31st where there is one); weekly ones by days.
pub fn nth_due(prev: NaiveDate, cadence: i64, k: i64) -> NaiveDate {
    use chrono::Datelike;
    let months = match cadence {
        30 => k,
        91 => 3 * k,
        365 => 12 * k,
        _ => return prev + Duration::days(cadence * k),
    };
    let m = add_months(month_start(prev), months as i32);
    m.with_day(prev.day()).unwrap_or_else(|| month_end(m))
}

fn payee(t: &TxRow) -> String {
    if t.clean_payee.is_empty() { title_case(&merchant_stem(&t.raw_description, &t.memo)) } else { t.clean_payee.clone() }
}

// ──────────────────────────────────────────────────────────────── Age of Money

#[derive(Debug, Clone, PartialEq)]
pub struct AgeOfMoney {
    /// Days, today (as of the latest statement).
    pub days: i64,
    pub as_of: NaiveDate,
    /// At each month end.
    pub by_month: Vec<(NaiveDate, i64)>,
}

/// How long money sat between arriving and being spent, YNAB-style: income into your bank accounts
/// (not cards or loans; not transfers between your own accounts) is stacked oldest first, and each
/// purchase, from any account, spends the oldest money still there. A statement's opening balance
/// counts as money that arrived on its first day. The age is the average over the last 10
/// purchases; spending beyond what has arrived counts as zero days old.
pub fn age_of_money(rows: &[TxRow], docs: &[DocumentRow], ccy: Currency, fx: &Fx) -> Option<AgeOfMoney> {
    let liability: BTreeMap<i64, bool> = rows.iter().map(|t| (t.account_id, t.account_type.is_liability())).collect();
    // (date, +in / −out); inflows sort first on a day, so a payday's money can be spent that day
    let mut events: Vec<(NaiveDate, bool, Decimal)> = Vec::new();
    let mut opened: BTreeMap<i64, ()> = BTreeMap::new();
    let mut docs: Vec<&DocumentRow> = docs.iter().filter(|d| d.period.is_some()).collect();
    docs.sort_by_key(|d| d.period.map(|p| p.0));
    for d in docs {
        if liability.get(&d.account_id).copied().unwrap_or(true) || opened.insert(d.account_id, ()).is_some() {
            continue;
        }
        if let (Some(o), Some((start, _))) = (d.opening, d.period)
            && o > Decimal::ZERO
        {
            events.push((start, false, crate::insights::known(fx, o, d.currency, ccy, start).unwrap_or_default()));
        }
    }
    for t in rows.iter().filter(|t| !t.is_transfer()) {
        let v = value(t, ccy, fx);
        if v.is_sign_positive() && !t.account_type.is_liability() {
            events.push((t.posted, false, v));
        } else if v.is_sign_negative() {
            events.push((t.posted, true, -v));
        }
    }
    if !events.iter().any(|e| e.1) {
        return None;
    }
    events.sort_by_key(|e| (e.0, e.1));
    let mut layers: VecDeque<(NaiveDate, Decimal)> = VecDeque::new();
    let mut ages: Vec<(NaiveDate, i64)> = Vec::new();
    for (d, out, mut v) in events {
        if !out {
            layers.push_back((d, v));
            continue;
        }
        let (total, mut weighted) = (v, Decimal::ZERO);
        while v > Decimal::ZERO {
            let Some(front) = layers.front_mut() else { break };
            let take = v.min(front.1);
            weighted += take * Decimal::from((d - front.0).num_days());
            front.1 -= take;
            v -= take;
            if front.1.is_zero() {
                layers.pop_front();
            }
        }
        let age = if total.is_zero() { 0 } else { (weighted / total).round().try_into().unwrap_or(0) };
        ages.push((d, age));
    }
    let at = |when: NaiveDate| -> Option<i64> {
        let upto: Vec<i64> = ages.iter().filter(|a| a.0 <= when).map(|a| a.1).collect();
        let last = &upto[upto.len().saturating_sub(10)..];
        (!last.is_empty()).then(|| last.iter().sum::<i64>() / last.len() as i64)
    };
    let as_of = ages.last()?.0;
    let first = month_start(ages[0].0);
    let mut by_month = Vec::new();
    let mut m = first;
    while m <= as_of {
        let end = month_end(m).min(as_of);
        if let Some(a) = at(end) {
            by_month.push((end, a));
        }
        m = add_months(m, 1);
    }
    Some(AgeOfMoney { days: at(as_of)?, as_of, by_month })
}

// ──────────────────────────────────────────────────────────────── regular bills

/// A regular payment: same merchant on one account at a steady rhythm.
#[derive(Debug, Clone, PartialEq)]
pub struct Regular {
    pub stem: String,
    pub payee: String,
    pub account_id: i64,
    pub account: String,
    pub currency: Currency,
    /// Days between charges: 7, 14, 30, 91 or 365.
    pub cadence: i64,
    /// Typical amount (median of the last three), signed: money in is positive.
    pub amount: Decimal,
    pub last: NaiveDate,
    pub first: NaiveDate,
    pub dates: Vec<NaiveDate>,
    pub is_transfer: bool,
}

fn median(mut v: Vec<Decimal>) -> Decimal {
    v.sort();
    if v.is_empty() { Decimal::ZERO } else { v[v.len() / 2] }
}

/// Every regular payment (in or out, transfers included) seen up to `upto`: flagged recurring or a
/// subscription, or three or more charges at a steady rhythm whose amounts stay within 25% of the
/// typical one, so a weekly supermarket run with a different total each time isn't a bill.
pub fn regulars(rows: &[TxRow], upto: NaiveDate) -> Vec<Regular> {
    let mut by: BTreeMap<(i64, String, bool), Vec<&TxRow>> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.posted <= upto) {
        by.entry((t.account_id, payer_key(&t.raw_description, &t.memo), t.amount.is_sign_positive())).or_default().push(t);
    }
    let mut out = Vec::new();
    for ((account_id, stem, _), mut v) in by {
        v.sort_by_key(|t| t.posted);
        let dates: Vec<NaiveDate> = v.iter().map(|t| t.posted).collect();
        let Some(cad) = cadence(&dates) else { continue };
        let typical = median(v.iter().rev().take(3).map(|t| t.amount).collect());
        let flagged = v.iter().any(|t| t.is_recurring || t.is_subscription);
        let steady = v.len() >= 3 && v.iter().rev().take(6).all(|t| (t.amount - typical).abs() <= typical.abs() / Decimal::from(4));
        if !(flagged || steady) {
            continue;
        }
        let last = v[v.len() - 1];
        out.push(Regular {
            payee: payee(last),
            account: last.account_name.clone(),
            currency: last.currency,
            cadence: cad,
            amount: typical,
            last: last.posted,
            first: v[0].posted,
            dates,
            is_transfer: v.iter().any(|t| t.is_transfer()),
            account_id,
            stem,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum BillState {
    /// Charged on these dates.
    Paid(Vec<NaiveDate>),
    /// Due, and the account's statement for the month is in, but no charge.
    Missed,
    /// Due, but the statement covering it hasn't been imported yet.
    NotYetSeen,
    /// First seen this month.
    New,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedBill {
    pub payee: String,
    pub account: String,
    pub account_id: i64,
    /// Typical, in the display currency (positive).
    pub amount: Decimal,
    pub cadence: i64,
    pub due: NaiveDate,
    pub state: BillState,
}

/// The regular bills (money out, not transfers) `month` should have had, and what happened to each.
/// `covered` = each account's latest statement end date.
pub fn bills_for(rows: &[TxRow], covered: &BTreeMap<i64, NaiveDate>, month: NaiveDate, ccy: Currency, fx: &Fx) -> Vec<ExpectedBill> {
    let (m0, m1) = (month_start(month), month_end(month));
    let slack = Duration::days(4);
    let mut out = Vec::new();
    for r in regulars(rows, m1 + slack).into_iter().filter(|r| r.amount.is_sign_negative() && !r.is_transfer) {
        let Some(amount) = fx.convert(-r.amount, r.currency, ccy) else { continue };
        let amount = amount.round_dp(2);
        let new = month_start(r.first) == m0;
        let anchor = r.dates.iter().copied().rfind(|d| *d < m0).unwrap_or(r.first);
        let mut k = if anchor < m0 { 1 } else { 0 };
        let mut due = nth_due(anchor, r.cadence, k);
        let mut skipped = 0;
        while due < m0 {
            k += 1;
            skipped += 1;
            due = nth_due(anchor, r.cadence, k);
        }
        if skipped >= 3 { continue; }
        let mut unmatched: Vec<_> = r.dates.iter().copied()
            .filter(|d| *d >= m0 - slack && *d <= m1 + slack).collect();
        while due <= m1 {
            // Each actual charge can settle only one occurrence. Prefer the closest date so
            // a weekly payment cannot also mark its neighbour paid.
            let matched = unmatched.iter().enumerate()
                .filter(|(_, d)| (**d - due).num_days().abs() <= 4)
                .min_by_key(|(_, d)| (**d - due).num_days().abs()).map(|(i, _)| i);
            let state = if let Some(i) = matched {
                let paid = unmatched.remove(i);
                if new && k == 0 { BillState::New } else { BillState::Paid(vec![paid]) }
            } else if covered.get(&r.account_id).is_some_and(|end| *end >= due) {
                BillState::Missed
            } else { BillState::NotYetSeen };
            out.push(ExpectedBill { payee: r.payee.clone(), account: r.account.clone(), account_id: r.account_id, amount, cadence: r.cadence, due, state });
            k += 1;
            due = nth_due(anchor, r.cadence, k);
        }
    }
    out.sort_by(|a, b| a.due.cmp(&b.due).then(a.payee.cmp(&b.payee)));
    out
}

/// Each account's latest statement end date.
pub fn covered(docs: &[DocumentRow]) -> BTreeMap<i64, NaiveDate> {
    let mut m: BTreeMap<i64, NaiveDate> = BTreeMap::new();
    for d in docs {
        if let Some((_, end)) = d.period {
            let e = m.entry(d.account_id).or_insert(end);
            *e = (*e).max(end);
        }
    }
    m
}

// ──────────────────────────────────────────────────────────────── balance forecast

#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub account_id: i64,
    pub account: String,
    pub currency: Currency,
    /// The last verified balance and its date.
    pub start: (NaiveDate, Decimal),
    /// One point a day from the start to the horizon, in the account's currency.
    pub points: Vec<(NaiveDate, Decimal)>,
    /// Regular payments expected along the way: (date, payee, amount).
    pub events: Vec<(NaiveDate, String, Decimal)>,
    /// Everything else, per day (the last 90 days' average, usually negative).
    pub daily: Decimal,
}

impl Forecast {
    pub fn at(&self, d: NaiveDate) -> Option<Decimal> {
        self.points.iter().rev().find(|p| p.0 <= d).map(|p| p.1)
    }
    /// The lowest point between the dates.
    pub fn low(&self, from: NaiveDate, to: NaiveDate) -> Option<(NaiveDate, Decimal)> {
        self.points.iter().filter(|p| p.0 >= from && p.0 <= to).min_by_key(|p| p.1).copied()
    }
}

/// For each bank account (not cards or loans): start from its latest verified balance, add every
/// regular payment in or out (pay, bills, card payments, standing orders) on its rhythm, and spread
/// the rest of its money in and out evenly, at the last 90 days' rate, until `until`.
pub fn forecast(rows: &[TxRow], docs: &[DocumentRow], until: NaiveDate) -> Vec<Forecast> {
    let balances = latest_balances(rows, docs);
    let mut accounts: BTreeMap<i64, (&str, Currency)> = BTreeMap::new();
    for (id, t) in own_accounts(rows).into_iter().filter(|(_, t)| !t.account_type.is_liability()) {
        accounts.insert(id, (&t.account_name, t.currency));
    }
    let mut out = Vec::new();
    for (id, (name, ccy)) in accounts {
        let Some(&(start, bal)) = balances.get(&id) else { continue };
        let mine: Vec<TxRow> = rows.iter().filter(|t| t.account_id == id).cloned().collect();
        let regs = regulars(&mine, start);
        // the rest: what wasn't a regular payment over the 90 days before the balance
        let since = start - Duration::days(89);
        let regular_ids: Vec<(String, bool)> = regs.iter().map(|r| (r.stem.clone(), r.amount.is_sign_positive())).collect();
        let other: Decimal = mine
            .iter()
            .filter(|t| t.posted >= since && t.posted <= start)
            .filter(|t| !regular_ids.contains(&(payer_key(&t.raw_description, &t.memo), t.amount.is_sign_positive())))
            .map(|t| t.amount)
            .sum();
        let span = (start - mine.iter().map(|t| t.posted).filter(|d| *d >= since).min().unwrap_or(start)).num_days().max(30);
        let daily = (other / Decimal::from(span)).round_dp(2);
        let mut events: Vec<(NaiveDate, String, Decimal)> = Vec::new();
        for r in &regs {
            // one that stopped (three cycles without a payment) isn't coming back
            if (start - r.last).num_days() > 3 * r.cadence + 5 {
                continue;
            }
            let mut k = 1;
            let mut d = nth_due(r.last, r.cadence, k);
            while d <= until {
                if d > start {
                    events.push((d, r.payee.clone(), r.amount));
                }
                k += 1;
                d = nth_due(r.last, r.cadence, k);
            }
        }
        events.sort_by_key(|e| e.0);
        let mut points = vec![(start, bal)];
        let mut b = bal;
        let mut d = start;
        while d < until {
            d += Duration::days(1);
            b += daily + events.iter().filter(|e| e.0 == d).map(|e| e.2).sum::<Decimal>();
            points.push((d, b));
        }
        out.push(Forecast { account_id: id, account: name.to_string(), currency: ccy, start: (start, bal), points, events, daily });
    }
    out
}

/// A forecast made at Monthly Close, kept to compare with the statement when it arrives:
/// account → (the date it was for, the estimated balance, when it was made).
pub type Checks = BTreeMap<i64, (NaiveDate, Decimal, NaiveDate)>;

impl crate::db::Db {
    pub fn forecast_checks(&self) -> Checks {
        self.setting("forecast_checks").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    }
    pub fn save_forecast_checks(&self, c: &Checks) -> crate::Result<()> {
        self.set_setting("forecast_checks", &serde_json::to_string(c).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, TxFilter};

    fn demo() -> (Db, Vec<TxRow>, Vec<DocumentRow>) {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let docs = db.documents(&[]).unwrap();
        (db, rows, docs)
    }

    #[test]
    fn age_of_money_on_the_demo_year() {
        let (db, rows, docs) = demo();
        let a = age_of_money(&rows, &docs, Currency::KYD, &db.fx().unwrap()).expect("an age");
        println!("{} days as of {}, {:?}", a.days, a.as_of, a.by_month);
        assert!(a.days > 0 && a.days < 400);
        assert!(a.by_month.len() >= 10);
    }

    #[test]
    fn oldest_money_is_spent_first() {
        // 100 on the 1st, 100 on the 11th; spending 150 on the 21st takes 100 that's 20 days old
        // and 50 that's 10 days old: (100×20 + 50×10) ÷ 150 = 16.67 → 17 days
        let (db, rows, _) = demo();
        let mut t = rows.iter().find(|t| !t.account_type.is_liability()).unwrap().clone();
        t.transfer_peer = None;
        t.category_kind = None;
        t.split_seq = None;
        let at = |d: u32, a: &str| {
            let mut x = t.clone();
            x.posted = NaiveDate::from_ymd_opt(2026, 1, d).unwrap();
            x.amount = a.parse().unwrap();
            x.currency = Currency::KYD;
            x
        };
        let rows = vec![at(1, "100"), at(11, "100"), at(21, "-150")];
        let a = age_of_money(&rows, &[], Currency::KYD, &db.fx().unwrap()).unwrap();
        assert_eq!(a.days, 17);
    }

    #[test]
    fn bills_paid_missed_and_not_yet_seen() {
        let (db, rows, docs) = demo();
        let fx = db.fx().unwrap();
        let cov = covered(&docs);
        let sep = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let bills = bills_for(&rows, &cov, sep, Currency::KYD, &fx);
        for b in &bills {
            println!("{} {} {:?}", b.payee, b.amount, b.state);
        }
        assert!(bills.iter().any(|b| matches!(b.state, BillState::Paid(_))));
        // November's statements aren't in (the demo ends in October)
        let nov = bills_for(&rows, &cov, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), Currency::KYD, &fx);
        assert!(!nov.is_empty() && nov.iter().all(|b| b.state == BillState::NotYetSeen));
        // pretend September's statement is in but the bill isn't on it
        let first = bills.iter().find(|b| matches!(b.state, BillState::Paid(_)) && b.cadence == 30).unwrap();
        let without: Vec<TxRow> = rows.iter().filter(|t| !(t.account_id == first.account_id && payee(t) == first.payee && month_start(t.posted) == sep)).cloned().collect();
        let again = bills_for(&without, &cov, sep, Currency::KYD, &fx);
        assert!(again.iter().any(|b| b.payee == first.payee && b.state == BillState::Missed), "{again:?}");
    }

    #[test]
    fn weekly_and_fortnightly_bills_count_every_occurrence_and_match_once() {
        let d = |s: &str| s.parse::<NaiveDate>().unwrap();
        for (cadence, dates, count) in [
            (7, vec!["2026-01-01", "2026-01-08", "2026-01-15", "2026-01-22", "2026-01-29", "2026-02-05"], 4),
            (14, vec!["2026-01-01", "2026-01-15", "2026-01-29", "2026-02-12"], 2),
        ] {
            let mut rows: Vec<_> = dates.iter().enumerate().map(|(i, day)| crate::insights::tests::row(i as i64, d(day), "REGULAR BILL", "-100", "Insurance", "")).collect();
            for t in &mut rows { t.is_subscription = true; }
            let cov = BTreeMap::from([(rows[0].account_id, d("2026-02-28"))]);
            let bills = bills_for(&rows, &cov, d("2026-02-01"), Currency::KYD, &Fx::default());
            assert_eq!(bills.len(), count, "cadence {cadence}");
            assert_eq!(bills.iter().map(|b| b.amount).sum::<Decimal>(), Decimal::from(100 * count));
            assert_eq!(bills.iter().filter(|b| matches!(b.state, BillState::Paid(_))).count(), 1);
            assert_eq!(bills.iter().filter(|b| b.state == BillState::Missed).count(), count - 1);
        }
    }

    #[test]
    fn forecast_carries_the_balance_forward() {
        let (_, rows, docs) = demo();
        let until = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let f = forecast(&rows, &docs, until);
        assert!(!f.is_empty());
        for a in &f {
            println!("{} from {:?}: {} events, {}/day, ends {:?}", a.account, a.start, a.events.len(), a.daily, a.points.last());
            assert_eq!(a.points[0], a.start);
            assert_eq!(a.points.last().unwrap().0, until);
        }
        // the chequing account has a salary coming in
        assert!(f.iter().any(|a| a.events.iter().any(|e| e.2 > Decimal::ZERO)));
        let db = Db::open_in_memory().unwrap();
        let c: Checks = f.iter().map(|a| (a.account_id, (until, a.at(until).unwrap(), a.start.0))).collect();
        db.save_forecast_checks(&c).unwrap();
        assert_eq!(db.forecast_checks(), c);
    }

    #[test]
    fn february_still_has_its_monthly_bills() {
        let ymd = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        let mut rows = Vec::new();
        // paid on the 1st, and on the last day of the month
        for (i, (y, m)) in [(2025, 8), (2025, 9), (2025, 10), (2025, 11), (2025, 12), (2026, 1), (2026, 2), (2026, 3)].into_iter().enumerate() {
            rows.push(crate::insights::tests::row(100 + i as i64, ymd(y, m, 1), "ACME INSURANCE", "-100.00", "Insurance", ""));
            rows.push(crate::insights::tests::row(200 + i as i64, month_end(ymd(y, m, 1)), "ZED GYM", "-50.00", "Fitness", ""));
        }
        let cov: BTreeMap<i64, NaiveDate> = [(1, ymd(2026, 3, 31))].into();
        let fx = Fx::default();
        let feb = bills_for(&rows, &cov, ymd(2026, 2, 1), Currency::KYD, &fx);
        assert_eq!(feb.iter().map(|b| (b.payee.as_str(), b.due, b.state.clone())).collect::<Vec<_>>(), vec![("ACME INSURANCE", ymd(2026, 2, 1), BillState::Paid(vec![ymd(2026, 2, 1)])), ("ZED GYM", ymd(2026, 2, 28), BillState::Paid(vec![ymd(2026, 2, 28)]))]);
        // without February's charges, both are missed
        let without: Vec<TxRow> = rows.iter().filter(|t| month_start(t.posted) != ymd(2026, 2, 1)).cloned().collect();
        assert!(bills_for(&without, &cov, ymd(2026, 2, 1), Currency::KYD, &fx).iter().filter(|b| b.state == BillState::Missed).count() == 2);
        assert_eq!(nth_due(ymd(2026, 1, 31), 30, 2), ymd(2026, 3, 31));
    }

    #[test]
    fn a_charge_flagged_to_the_business_leaves_its_account_at_home() {
        let (db, _, _) = demo();
        let ents = db.entities().unwrap();
        let (personal, biz) = (ents[0].id, ents[1].id);
        let t = db.transactions(&TxFilter { entity_ids: vec![personal], ..Default::default() }).unwrap().into_iter().find(|t| t.balance.is_some() && t.amount.is_sign_negative() && !t.is_transfer() && !t.account_type.is_liability()).unwrap();
        let before = forecast(&db.transactions(&TxFilter { entity_ids: vec![biz], ..Default::default() }).unwrap(), &db.documents(&[biz]).unwrap(), t.posted + Duration::days(45));
        db.set_business(&[t.id], Some(biz)).unwrap();
        let brows = db.transactions(&TxFilter { entity_ids: vec![biz], ..Default::default() }).unwrap();
        let bdocs = db.documents(&[biz]).unwrap();
        let after = forecast(&brows, &bdocs, t.posted + Duration::days(45));
        assert!(!after.iter().any(|f| f.account_id == t.account_id));
        assert_eq!(before.len(), after.len());
        let fx = db.fx().unwrap();
        let nw = |rows: &[TxRow]| crate::wealth::net_worth_on(&crate::wealth::account_balances(rows, &bdocs), &[], t.posted, Currency::KYD, &fx).net();
        let without: Vec<TxRow> = brows.iter().filter(|x| x.id != t.id).cloned().collect();
        assert_eq!(nw(&brows), nw(&without));
        // picked by account, the personal view still sees the flagged row in its own account
        let mine = db.transactions(&TxFilter { entity_ids: vec![personal], by_account: true, ..Default::default() }).unwrap();
        assert!(mine.iter().any(|x| x.id == t.id));
        assert!(!db.transactions(&TxFilter { entity_ids: vec![biz], by_account: true, ..Default::default() }).unwrap().iter().any(|x| x.id == t.id));
    }
}
