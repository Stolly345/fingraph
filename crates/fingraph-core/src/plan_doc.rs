//! The budget you walk away with: the Budget Plan (what to do each payday, which bills come when, a
//! weekly limit for each variable category, the savings it adds up to), the monthly report card
//! (the plan against what happened), and a calendar file of paydays and bills. Pure data: the app
//! lays it out on paper; nothing here draws.

use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::budget::{Fund, Plan};
use crate::db::{CategoryKind, TxRow};
use crate::insights::{add_months, month_start, value};
use crate::money::{Fx, fmt_money};
use crate::outlook::{nth_due, regulars};
use crate::Currency;

/// Which of your accounts does what: where pay lands, what the bills and everyday spending come
/// out of, and where the big-bill money and the savings are kept. `None` = the pay account.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Roles {
    pub pay: Option<i64>,
    pub bills: Option<i64>,
    pub spending: Option<i64>,
    pub big_bills: Option<i64>,
    pub savings: Option<i64>,
    /// The card or loan extra debt payments go to.
    pub debt: Option<i64>,
}

/// One budget category on the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct CatPlan {
    pub name: String,
    pub fixed: bool,
    /// A need (housing, groceries) rather than a want (eating out).
    pub need: bool,
    /// Per month.
    pub budget: Decimal,
    /// What it usually costs a month (the median of recent months), when known.
    pub usual: Option<Decimal>,
}

impl CatPlan {
    pub fn weekly(&self) -> Decimal {
        (self.budget * Decimal::from(12) / Decimal::from(52)).round_dp(0)
    }
    /// Usually costs more than its budget: where the plan is likeliest to break.
    pub fn at_risk(&self) -> bool {
        self.usual.is_some_and(|u| u > self.budget * Decimal::new(105, 2))
    }
}

/// A regular bill (monthly, fortnightly or weekly) seen in your statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Bill {
    pub name: String,
    pub account: String,
    /// What it usually costs (positive).
    pub amount: Decimal,
    /// Days apart: 7, 14 or 30.
    pub cadence: i64,
    /// The next one on or after the plan's date.
    pub next: NaiveDate,
}

impl Bill {
    pub fn every(&self) -> &'static str {
        match self.cadence {
            7 => "weekly",
            14 => "fortnightly",
            _ => "monthly",
        }
    }
    /// Its dates from `next` up to `until`.
    pub fn dates(&self, until: NaiveDate) -> Vec<NaiveDate> {
        (0..400).map(|k| nth_due(self.next, self.cadence, k)).take_while(|d| *d <= until).collect()
    }
}

/// One yearly or quarterly bill and where its money should be.
#[derive(Debug, Clone, PartialEq)]
pub struct BigBill {
    pub name: String,
    pub amount: Decimal,
    pub every_months: i32,
    pub monthly: Decimal,
    pub due: NaiveDate,
    /// What should be put aside for it by now.
    pub saved_by_now: Decimal,
}

/// One line of the payday checklist.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Per payday.
    pub amount: Decimal,
    /// "Move to Butterfield Savings" (from the pay account), or "Leave in Butterfield Chequing".
    pub action: String,
    /// What it's for: "savings", "big yearly bills".
    pub why: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanDoc {
    pub method: crate::method::Method,
    pub made: NaiveDate,
    /// The newest statement the figures come from.
    pub data_through: NaiveDate,
    pub ccy: Currency,
    pub cycle: String,
    pub per_year: Decimal,
    /// Take-home per payday.
    pub take_home: Decimal,
    pub next_paydays: Vec<NaiveDate>,
    pub pay_account: String,
    pub checklist: Vec<Step>,
    /// Per payday: positive = spare after the checklist, negative = the plan spends more than comes in.
    pub spare: Decimal,
    // per month
    pub income: Decimal,
    pub other_income: Decimal,
    pub fixed: Decimal,
    pub variable: Decimal,
    pub big: Decimal,
    pub loans: Decimal,
    /// Paid off debt a month on top of the minimums, the account it goes to, and what's owed.
    pub debt: Decimal,
    pub debt_account: Option<String>,
    pub owed: Decimal,
    /// Pension a month (counts as saving), and what a method divides (take-home + pension).
    pub pension: Decimal,
    pub base: Decimal,
    pub goal: Decimal,
    /// income − fixed − variable − big − loans − debt.
    pub left_to_save: Decimal,
    pub cats: Vec<CatPlan>,
    pub bills: Vec<Bill>,
    pub big_bills: Vec<BigBill>,
    /// The big-bills account's latest balance (name, date, amount), when it has one of its own.
    pub big_account: Option<(String, NaiveDate, Decimal)>,
    /// Cancelled bills and subscriptions: name, what that saves a year.
    pub cancelled: Vec<(String, Decimal)>,
}

impl PlanDoc {
    pub fn money(&self, d: Decimal) -> String {
        fmt_money(d, self.ccy)
    }
    /// Per payday, from a monthly figure.
    pub fn per_pay(&self, monthly: Decimal) -> Decimal {
        (monthly * Decimal::from(12) / self.per_year).round_dp(0)
    }
    /// What's saved a month: the goal (or as much of it as the budget leaves), or with no goal, what's left.
    pub fn saving(&self) -> Decimal {
        let left = self.left_to_save.max(Decimal::ZERO);
        if self.goal > Decimal::ZERO { self.goal.min(left) } else { left }
    }
    /// Needs a month: need categories, big bills and loan minimums.
    pub fn needs(&self) -> Decimal {
        self.cats.iter().filter(|c| c.need).map(|c| c.budget).sum::<Decimal>() + self.big + self.loans
    }
    pub fn wants(&self) -> Decimal {
        self.cats.iter().filter(|c| !c.need).map(|c| c.budget).sum()
    }
    /// Months to clear the debt at the extra payment (None: no debt, or nothing extra paid).
    pub fn months_to_clear(&self) -> Option<i64> {
        (self.owed > Decimal::ZERO && self.debt > Decimal::ZERO).then(|| (self.owed / self.debt).ceil().to_string().parse().unwrap_or(0))
    }
    pub fn saved_by_now(&self) -> Decimal {
        self.big_bills.iter().map(|b| b.saved_by_now).sum()
    }
    /// What the savings come to at this rate: (by the end of this year, in a year, in five years).
    pub fn projection(&self) -> (Decimal, Decimal, Decimal) {
        let save = self.saving();
        let months_left = Decimal::from(12 - self.made.month());
        (save * months_left, save * Decimal::from(12), save * Decimal::from(60))
    }
}

/// Everything the plan is made from, in the display currency.
pub struct PlanInput<'a> {
    pub plan: &'a Plan,
    pub roles: &'a Roles,
    pub ccy: Currency,
    pub fx: &'a Fx,
    pub cats: Vec<CatPlan>,
    pub funds: &'a [Fund],
    pub rows: &'a [TxRow],
    /// Account id → its name as you'd say it ("Butterfield Chequing ••1234").
    pub accounts: &'a BTreeMap<i64, String>,
    /// Latest balance per account, in the display currency.
    pub balances: &'a BTreeMap<i64, (NaiveDate, Decimal)>,
    /// Owed on cards and loans, in the display currency.
    pub owed: Decimal,
    pub cancelled: Vec<(String, Decimal)>,
    /// Fixed-expense categories, and each category's top-level name: what counts as a bill.
    pub fixed: Vec<String>,
    pub parents: &'a BTreeMap<i64, String>,
    pub today: NaiveDate,
    pub data_through: NaiveDate,
}

/// The regular bills still running: monthly or more often, money out, not a transfer, seen recently
/// and not cancelled, and a bill rather than a habit: a subscription, or filed in a fixed-expense
/// category (`fixed`, by top-level name), so a fortnightly lunch at the same place isn't one. Each
/// with its next date on or after `today`.
pub fn running_bills(rows: &[TxRow], fixed: &[String], parents: &BTreeMap<i64, String>, ccy: Currency, fx: &Fx, today: NaiveDate, data_through: NaiveDate) -> Vec<Bill> {
    let top = |t: &TxRow| t.category_id.and_then(|c| parents.get(&c).cloned()).or_else(|| t.category_name.clone()).unwrap_or_default();
    // judged by its latest charge: cancelled (under either spelling of the merchant), or not a bill
    let is_bill = |r: &crate::outlook::Regular| {
        let last: Vec<&TxRow> = rows.iter().filter(|t| t.account_id == r.account_id && t.posted == r.last && crate::rules::payer_key(&t.raw_description, &t.memo) == r.stem).collect();
        !last.iter().any(|t| crate::insights::is_cancelled(t)) && last.iter().any(|t| t.is_subscription || fixed.contains(&top(t)))
    };
    let mut out: Vec<Bill> = regulars(rows, data_through)
        .into_iter()
        .filter(|r| !r.is_transfer && r.amount.is_sign_negative() && matches!(r.cadence, 7 | 14 | 30))
        .filter(is_bill)
        .filter(|r| (data_through - r.last).num_days() <= 2 * r.cadence + 7)
        .filter_map(|r| {
            let mut k = 1;
            let mut next = nth_due(r.last, r.cadence, k);
            while next < today && k < 2000 {
                k += 1;
                next = nth_due(r.last, r.cadence, k);
            }
            Some(Bill { name: r.payee, account: r.account, amount: fx.convert(-r.amount, r.currency, ccy)?.round_dp(2), cadence: r.cadence, next })
        })
        .collect();
    out.sort_by(|a, b| a.next.cmp(&b.next).then(a.name.cmp(&b.name)));
    out
}

pub fn plan_doc(i: PlanInput) -> Option<PlanDoc> {
    if !crate::insights::missing_rates(i.rows, i.ccy, i.fx).is_empty() { return None; }
    let p = i.plan;
    let name = |id: Option<i64>| id.and_then(|id| i.accounts.get(&id).cloned());
    let pay_account = name(i.roles.pay).unwrap_or_else(|| "your pay account".into());
    let fixed: Decimal = i.cats.iter().filter(|c| c.fixed).map(|c| c.budget).sum();
    let variable: Decimal = i.cats.iter().filter(|c| !c.fixed).map(|c| c.budget).sum();
    let big: Decimal = i.funds.iter().map(|f| i.fx.convert(f.monthly(), f.currency, i.ccy)).collect::<Option<Vec<_>>>()?.into_iter().sum::<Decimal>().round_dp(2);
    let income = p.monthly_income();
    let left_to_save = income - fixed - variable - big - p.loan_payments - p.debt_extra;
    let per_year = p.cycle.per_year();
    let per_pay = |m: Decimal| (m * Decimal::from(12) / per_year).round_dp(0);
    // what to save: the goal, or what's left when there's no goal
    let save = if p.savings_goal > Decimal::ZERO { p.savings_goal } else { left_to_save.max(Decimal::ZERO) };

    // the checklist: each pot from the pay account to its own, pots sharing an account on one line
    let pots: [(Decimal, Option<i64>, &str); 5] = [
        (fixed + p.loan_payments, i.roles.bills, if p.loan_payments > Decimal::ZERO { "bills and loans" } else { "bills" }),
        (big, i.roles.big_bills, "big bills"),
        (save, i.roles.savings, "savings"),
        (p.debt_extra, i.roles.debt, "debt payoff"),
        (variable, i.roles.spending, "spending"),
    ];
    let mut by_target: Vec<(Option<i64>, Decimal, Vec<&str>)> = Vec::new();
    for (monthly, acct, why) in pots {
        if monthly <= Decimal::ZERO {
            continue;
        }
        // an account the same as the pay account is "leave it there"
        let target = acct.filter(|a| Some(*a) != i.roles.pay);
        match by_target.iter_mut().find(|t| t.0 == target) {
            Some(t) => {
                t.1 += per_pay(monthly);
                t.2.push(why);
            }
            None => by_target.push((target, per_pay(monthly), vec![why])),
        }
    }
    // moves first, then what stays
    by_target.sort_by_key(|t| t.0.is_none());
    let checklist: Vec<Step> = by_target
        .iter()
        .map(|(target, amount, why)| Step {
            amount: *amount,
            action: match target.and_then(|t| i.accounts.get(&t)) {
                Some(to) => format!("Move to {to}"),
                None => format!("Leave in {pay_account}"),
            },
            why: join_and(why),
        })
        .collect();
    let other_per_pay = per_pay(p.other_income);
    let spare = p.net_pay() + other_per_pay - checklist.iter().map(|s| s.amount).sum::<Decimal>();

    let big_bills: Vec<BigBill> = i
        .funds
        .iter()
        .map(|f| Some(BigBill {
            name: f.name.clone(),
            amount: i.fx.convert(f.amount, f.currency, i.ccy)?,
            every_months: f.every_months,
            monthly: i.fx.convert(f.monthly(), f.currency, i.ccy)?,
            due: f.due(i.today),
            saved_by_now: i.fx.convert(f.set_aside(i.today), f.currency, i.ccy)?,
        }))
        .collect::<Option<Vec<_>>>()?;
    let big_account = i.roles.big_bills.filter(|a| Some(*a) != i.roles.pay).and_then(|a| Some((i.accounts.get(&a)?.clone(), i.balances.get(&a)?.0, i.balances.get(&a)?.1)));
    let mut cats = i.cats;
    cats.sort_by(|a, b| b.fixed.cmp(&a.fixed).then(b.budget.cmp(&a.budget)));

    Some(PlanDoc {
        method: p.method,
        debt: p.debt_extra,
        debt_account: name(i.roles.debt),
        owed: i.owed,
        pension: p.pension_monthly(),
        base: p.method_base(),
        made: i.today,
        data_through: i.data_through,
        ccy: i.ccy,
        cycle: p.cycle.label().to_string(),
        per_year,
        take_home: p.net_pay(),
        next_paydays: p.paydays(i.today, 3),
        pay_account,
        checklist,
        spare,
        income,
        other_income: p.other_income,
        fixed,
        variable,
        big,
        loans: p.loan_payments,
        goal: p.savings_goal,
        left_to_save,
        cats,
        // not one already set aside for as a big bill, nor a service you cancelled under another spelling
        bills: running_bills(i.rows, &i.fixed, i.parents, i.ccy, i.fx, i.today, i.data_through)
            .into_iter()
            .filter(|b| !big_bills.iter().any(|f| f.name.eq_ignore_ascii_case(&b.name)) && !i.cancelled.iter().any(|c| c.0.eq_ignore_ascii_case(&b.name)))
            .collect(),
        big_bills,
        big_account,
        cancelled: i.cancelled,
    })
}

fn join_and(v: &[&str]) -> String {
    match v {
        [] => String::new(),
        [a] => a.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

// ──────────────────────────────────────────────────────────────── the monthly report card

#[derive(Debug, Clone, PartialEq)]
pub struct CardLine {
    pub name: String,
    pub fixed: bool,
    pub budget: Decimal,
    pub spent: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportCard {
    pub method: crate::method::Method,
    pub month: NaiveDate,
    /// The month as the method sees it: take-home + pension, and the needs, wants, saving (pension
    /// included) and extra debt payments actually made.
    pub base: Decimal,
    pub pension: Decimal,
    pub needs_actual: Decimal,
    pub wants_actual: Decimal,
    pub debt_plan: Decimal,
    pub ccy: Currency,
    /// Categories with a budget, and what was spent in them.
    pub lines: Vec<CardLine>,
    /// Spending in categories without a budget.
    pub unbudgeted: Vec<(String, Decimal)>,
    pub income_plan: Decimal,
    pub income_actual: Decimal,
    pub spent_plan: Decimal,
    pub spent_actual: Decimal,
    pub big_plan: Decimal,
    pub loans_plan: Decimal,
    pub loans_actual: Decimal,
    pub save_plan: Decimal,
    /// Income − spending − loan payments.
    pub saved_actual: Decimal,
    /// What the big-bill money should come to by the month's end, and the account's balance then.
    pub big_should_have: Decimal,
    pub big_account: Option<(String, Decimal)>,
}

impl ReportCard {
    pub fn money(&self, d: Decimal) -> String {
        fmt_money(d, self.ccy)
    }
    pub fn over(&self) -> Vec<&CardLine> {
        self.lines.iter().filter(|l| l.spent > l.budget).collect()
    }
}

/// `month` against the plan. `spent` is each top-level category's spending that month (as on the
/// Budget page), `budgets` each category's budget then.
#[allow(clippy::too_many_arguments)]
pub fn report_card(
    month: NaiveDate,
    plan: &Plan,
    budgets: &[(String, bool, Decimal)],
    spent: &BTreeMap<String, Decimal>,
    funds: &[Fund],
    rows: &[TxRow],
    parents: &BTreeMap<i64, String>,
    ccy: Currency,
    fx: &Fx,
    big_account: Option<(String, Decimal)>,
    needs: &[String],
) -> Option<ReportCard> {
    if !crate::insights::missing_rates(rows, ccy, fx).is_empty() { return None; }
    let month = month_start(month);
    let end = add_months(month, 1) - Duration::days(1);
    let in_month = |t: &&TxRow| t.posted >= month && t.posted <= end && !t.is_transfer();
    let income_actual: Decimal = rows.iter().filter(in_month).filter(|t| t.category_kind == Some(CategoryKind::Income)).map(|t| value(t, ccy, fx)).filter(|v| v.is_sign_positive()).sum();
    let top = |t: &TxRow| t.category_id.and_then(|c| parents.get(&c).cloned()).or_else(|| t.category_name.clone()).unwrap_or_default();
    let loans_actual: Decimal = rows.iter().filter(|t| t.posted >= month && t.posted <= end && t.amount.is_sign_negative() && top(t) == "Loan Payments").map(|t| -value(t, ccy, fx)).sum();
    let lines: Vec<CardLine> = budgets.iter().map(|(n, fixed, b)| CardLine { name: n.clone(), fixed: *fixed, budget: *b, spent: spent.get(n).copied().unwrap_or_default() }).collect();
    let mut unbudgeted: Vec<(String, Decimal)> = spent.iter().filter(|(n, v)| **v > Decimal::ZERO && *n != "Loan Payments" && !budgets.iter().any(|b| &b.0 == *n)).map(|(n, v)| (n.clone(), *v)).collect();
    unbudgeted.sort_by_key(|x| std::cmp::Reverse(x.1));
    let spent_actual: Decimal = spent.iter().filter(|(n, _)| *n != "Loan Payments").map(|(_, v)| *v).sum();
    let big_plan: Decimal = funds.iter().map(|f| fx.convert(f.monthly(), f.currency, ccy)).collect::<Option<Vec<_>>>()?.into_iter().sum::<Decimal>().round_dp(2);
    let spent_plan: Decimal = budgets.iter().map(|b| b.2).sum();
    let needs_actual: Decimal = spent.iter().filter(|(n, _)| needs.contains(n)).map(|(_, v)| *v).sum::<Decimal>() + loans_actual;
    let wants_actual: Decimal = spent.iter().filter(|(n, _)| !needs.contains(n) && *n != "Loan Payments").map(|(_, v)| *v).sum();
    let pension = plan.pension_monthly();
    Some(ReportCard {
        method: plan.method,
        base: income_actual.round_dp(2) + pension,
        pension,
        needs_actual: needs_actual.round_dp(2),
        wants_actual: wants_actual.round_dp(2),
        debt_plan: plan.debt_extra,
        month,
        ccy,
        lines,
        unbudgeted,
        income_plan: plan.monthly_income(),
        income_actual: income_actual.round_dp(2),
        spent_plan,
        spent_actual: spent_actual.round_dp(2),
        big_plan,
        loans_plan: plan.loan_payments,
        loans_actual: loans_actual.round_dp(2),
        save_plan: if plan.savings_goal > Decimal::ZERO { plan.savings_goal } else { (plan.monthly_income() - spent_plan - big_plan - plan.loan_payments - plan.debt_extra).max(Decimal::ZERO) },
        saved_actual: (income_actual - spent_actual - loans_actual).round_dp(2),
        big_should_have: funds.iter().map(|f| fx.convert(f.set_aside(end), f.currency, ccy)).collect::<Option<Vec<_>>>()?.into_iter().sum(),
        big_account,
    })
}

// ──────────────────────────────────────────────────────────────── the calendar file

fn ics_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

/// Lines longer than 75 bytes go on as continuation lines starting with a space (RFC 5545).
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in line.chars() {
        if n + c.len_utf8() > 74 {
            out.push_str("\r\n ");
            n = 1;
        }
        out.push(c);
        n += c.len_utf8();
    }
    out.push_str("\r\n");
    out
}

/// All-day events with a reminder (`alarm`: an ISO duration such as `-P3D` or `PT9H`).
struct Event {
    uid: String,
    day: NaiveDate,
    summary: String,
    description: String,
    alarm: &'static str,
}

/// An iCalendar file of the next `months` months: each payday (with its checklist and a reminder
/// that morning), each regular bill (reminder three days before) and each big bill (two weeks
/// before). Stable ids, so importing a newer file updates the events instead of doubling them.
pub fn calendar(doc: &PlanDoc, plan: &Plan, months: i32, stamp: chrono::NaiveDateTime) -> String {
    let until = add_months(month_start(doc.made), months + 1) - Duration::days(1);
    let mut ev: Vec<Event> = Vec::new();
    let steps: Vec<String> = doc.checklist.iter().map(|s| format!("• {}: {} ({})", doc.money(s.amount), s.action, s.why)).collect();
    for d in plan.paydays(doc.made, 60).into_iter().take_while(|d| *d <= until) {
        ev.push(Event {
            uid: format!("payday-{d}"),
            day: d,
            summary: format!("Payday: move money ({} take-home)", doc.money(doc.take_home)),
            description: format!("FinGraph budget plan of {}:\n{}", doc.made.format("%d %b %Y"), steps.join("\n")),
            alarm: "PT9H",
        });
    }
    for b in &doc.bills {
        for d in b.dates(until) {
            ev.push(Event {
                uid: format!("bill-{}-{d}", slug(&b.name)),
                day: d,
                summary: format!("{} due: {}", b.name, doc.money(b.amount)),
                description: format!("Usually {} {}, from {}.", doc.money(b.amount), b.every(), b.account),
                alarm: "-P3D",
            });
        }
    }
    for b in &doc.big_bills {
        let mut d = b.due;
        while d <= until {
            ev.push(Event {
                uid: format!("big-{}-{d}", slug(&b.name)),
                day: d,
                summary: format!("{} due: {}", b.name, doc.money(b.amount)),
                description: format!("A big bill: {} set aside a month covers it.", doc.money(b.monthly)),
                alarm: "-P14D",
            });
            d = add_months(month_start(d), b.every_months.max(1)).with_day(d.day()).unwrap_or_else(|| add_months(month_start(d), b.every_months.max(1) + 1) - Duration::days(1));
        }
    }
    ev.sort_by(|a, b| a.day.cmp(&b.day).then(a.summary.cmp(&b.summary)));
    let mut s = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//FinGraph//Budget Plan//EN\r\nCALSCALE:GREGORIAN\r\nX-WR-CALNAME:FinGraph Budget\r\n");
    for e in ev {
        s.push_str("BEGIN:VEVENT\r\n");
        s.push_str(&fold(&format!("UID:{}@fingraph", e.uid)));
        s.push_str(&format!("DTSTAMP:{}\r\n", stamp.format("%Y%m%dT%H%M%SZ")));
        s.push_str(&format!("DTSTART;VALUE=DATE:{}\r\n", e.day.format("%Y%m%d")));
        s.push_str(&format!("DTEND;VALUE=DATE:{}\r\n", (e.day + Duration::days(1)).format("%Y%m%d")));
        s.push_str(&fold(&format!("SUMMARY:{}", ics_text(&e.summary))));
        s.push_str(&fold(&format!("DESCRIPTION:{}", ics_text(&e.description))));
        s.push_str("TRANSP:TRANSPARENT\r\n");
        s.push_str(&format!("BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:{}\r\n", e.alarm));
        s.push_str(&fold(&format!("DESCRIPTION:{}", ics_text(&e.summary))));
        s.push_str("END:VALARM\r\nEND:VEVENT\r\n");
    }
    s.push_str("END:VCALENDAR\r\n");
    s
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::PayCycle;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }
    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn sample() -> (Plan, Roles, BTreeMap<i64, String>, Vec<Fund>) {
        let plan = Plan { cycle: PayCycle::Monthly, next_pay: Some(d(2026, 11, 30)), gross: dec("5000"), savings_goal: dec("500"), loan_payments: dec("300"), currency: Some(Currency::KYD), ..Default::default() };
        let roles = Roles { pay: Some(1), bills: Some(1), spending: Some(2), big_bills: Some(3), savings: Some(3), debt: None };
        let accounts: BTreeMap<i64, String> = [(1, "Chequing".to_string()), (2, "Visa".to_string()), (3, "Savings".to_string())].into_iter().collect();
        let funds = vec![Fund { id: 1, name: "Car insurance".into(), amount: dec("1200"), currency: Currency::KYD, every_months: 12, next_due: d(2027, 3, 15), stem: String::new() }];
        (plan, roles, accounts, funds)
    }

    #[test]
    fn a_plan_with_an_unpriced_bill_is_unavailable() {
        let (plan, roles, accounts, mut funds) = sample();
        funds[0].currency = Currency::parse("GBP").unwrap();
        let doc = plan_doc(PlanInput { plan: &plan, roles: &roles, ccy: Currency::KYD, fx: &Fx::default(), cats: vec![], funds: &funds, rows: &[], accounts: &accounts, balances: &BTreeMap::new(), owed: Decimal::ZERO, cancelled: vec![], fixed: vec![], parents: &BTreeMap::new(), today: d(2026, 11, 6), data_through: d(2026, 10, 31) });
        assert!(doc.is_none(), "an unknown bill cost must not improve the savings projection");
    }

    #[test]
    fn checklist_moves_each_pot_to_its_account() {
        let (plan, roles, accounts, funds) = sample();
        let cats = vec![
            CatPlan { name: "Housing".into(), fixed: true, need: true, budget: dec("2000"), usual: Some(dec("2000")) },
            CatPlan { name: "Groceries".into(), fixed: false, need: true, budget: dec("1040"), usual: Some(dec("1200")) },
        ];
        let doc = plan_doc(PlanInput { plan: &plan, roles: &roles, ccy: Currency::KYD, fx: &Fx::default(), cats, funds: &funds, rows: &[], accounts: &accounts, balances: &BTreeMap::new(), owed: Decimal::ZERO, cancelled: vec![], fixed: vec![], parents: &BTreeMap::new(), today: d(2026, 11, 6), data_through: d(2026, 10, 31) }).unwrap();
        let lines: Vec<(&str, Decimal)> = doc.checklist.iter().map(|s| (s.action.as_str(), s.amount)).collect();
        // big bills (100) and savings (500) share the savings account; spending goes to the card; bills stay
        assert_eq!(lines.len(), 3);
        assert_eq!(lines, vec![("Move to Savings", dec("600")), ("Move to Visa", dec("1040")), ("Leave in Chequing", dec("2300"))]);
        // 5000 − 600 − 1040 − 2300
        assert_eq!(doc.spare, dec("1060"));
        assert_eq!(doc.left_to_save, dec("5000") - dec("2000") - dec("1040") - dec("100") - dec("300"));
        assert_eq!(doc.cats[0].name, "Housing", "fixed first");
        assert!(doc.cats[1].at_risk(), "usually 1200 against 1040");
        assert_eq!(doc.cats[1].weekly(), dec("240"));
    }

    #[test]
    fn calendar_has_paydays_bills_and_reminders() {
        let (plan, roles, accounts, funds) = sample();
        let mut doc = plan_doc(PlanInput { plan: &plan, roles: &roles, ccy: Currency::KYD, fx: &Fx::default(), cats: vec![], funds: &funds, rows: &[], accounts: &accounts, balances: &BTreeMap::new(), owed: Decimal::ZERO, cancelled: vec![], fixed: vec![], parents: &BTreeMap::new(), today: d(2026, 11, 6), data_through: d(2026, 10, 31) }).unwrap();
        doc.bills.push(Bill { name: "CUC".into(), account: "Chequing".into(), amount: dec("250"), cadence: 30, next: d(2026, 11, 20) });
        let ics = calendar(&doc, &plan, 12, d(2026, 11, 6).and_hms_opt(12, 0, 0).unwrap());
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n") && ics.ends_with("END:VCALENDAR\r\n"));
        assert_eq!(ics.matches("SUMMARY:Payday").count(), 13, "Nov 2026 to Nov 2027");
        assert_eq!(ics.matches("SUMMARY:CUC due").count(), 13);
        assert_eq!(ics.matches("SUMMARY:Car insurance due").count(), 1);
        assert!(ics.contains("UID:bill-cuc-2026-12-20@fingraph"));
        assert!(ics.contains("TRIGGER:-P3D") && ics.contains("TRIGGER:-P14D") && ics.contains("TRIGGER:PT9H"));
        assert!(ics.lines().all(|l| l.len() <= 75), "folded");
    }

    #[test]
    fn report_card_sums() {
        let (plan, ..) = sample();
        let spent: BTreeMap<String, Decimal> = [("Groceries".to_string(), dec("1100")), ("Shopping".to_string(), dec("90"))].into_iter().collect();
        let c = report_card(d(2026, 10, 1), &plan, &[("Groceries".into(), false, dec("1040"))], &spent, &[], &[], &BTreeMap::new(), Currency::KYD, &Fx::default(), None, &["Groceries".to_string()]).unwrap();
        assert_eq!(c.over().len(), 1);
        assert_eq!(c.unbudgeted, vec![("Shopping".to_string(), dec("90"))]);
        assert_eq!(c.spent_actual, dec("1190"));
        assert_eq!(c.saved_actual, dec("-1190"), "no income rows");
        assert_eq!((c.needs_actual, c.wants_actual), (dec("1100"), dec("90")));
    }
}
