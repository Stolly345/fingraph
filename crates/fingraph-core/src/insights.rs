//! Dashboard numbers: KPIs, chart series and "What changed?" cards, all computed from ledger rows.
//! Transfers between your own accounts are never income or spending.

use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeMap;

use crate::db::{CategoryKind, DocumentRow, Entity, TxRow};
use crate::money::{fmt_money, round2};
use crate::rules::{self, merchant_stem};
use crate::{Currency, Fx, Reconciliation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    ThisMonth,
    LastMonth,
    Last90,
    Ytd,
    Year(i32),
    All,
    Custom(NaiveDate, NaiveDate),
}

pub fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

pub fn add_months(d: NaiveDate, n: i32) -> NaiveDate {
    let m0 = d.year() * 12 + d.month0() as i32 + n;
    NaiveDate::from_ymd_opt(m0.div_euclid(12), m0.rem_euclid(12) as u32 + 1, 1).unwrap_or(d)
}

impl Range {
    pub fn label(&self) -> String {
        match self {
            Self::ThisMonth => "This Month".into(),
            Self::LastMonth => "Last Month".into(),
            Self::Last90 => "Last 90 Days".into(),
            Self::Ytd => "Year to Date".into(),
            Self::Year(y) => y.to_string(),
            Self::All => "All Time".into(),
            Self::Custom(a, b) => format!("{a} – {b}"),
        }
    }

    /// Inclusive dates. `All` spans the data.
    pub fn resolve(&self, today: NaiveDate, data: Option<(NaiveDate, NaiveDate)>) -> (NaiveDate, NaiveDate) {
        match *self {
            Self::ThisMonth => (month_start(today), today),
            Self::LastMonth => (add_months(today, -1), month_start(today) - Duration::days(1)),
            Self::Last90 => (today - Duration::days(89), today),
            Self::Ytd => (NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today), today),
            Self::Year(y) => (NaiveDate::from_ymd_opt(y, 1, 1).unwrap_or(today), NaiveDate::from_ymd_opt(y, 12, 31).unwrap_or(today)),
            Self::All => data.unwrap_or((today, today)),
            Self::Custom(a, b) => (a, b),
        }
    }

    /// The same-length window just before.
    pub fn prior(from: NaiveDate, to: NaiveDate) -> (NaiveDate, NaiveDate) {
        let len = (to - from).num_days() + 1;
        (from - Duration::days(len), from - Duration::days(1))
    }
}

/// Money in the display currency: exact when it already is, the workspace figure (which honours
/// statement FX lines) when that matches, else the configured rate.
pub fn value(t: &TxRow, display: Currency, fx: &Fx) -> Decimal {
    if t.currency == display {
        t.amount
    } else if t.entity_currency == display {
        t.amount_entity
    } else {
        fx.convert(t.amount, t.currency, display)
    }
}

fn counts(t: &TxRow) -> bool {
    !t.is_transfer()
}

fn in_range(t: &TxRow, from: NaiveDate, to: NaiveDate) -> bool {
    t.posted >= from && t.posted <= to
}

pub fn data_span(rows: &[TxRow]) -> Option<(NaiveDate, NaiveDate)> {
    Some((rows.iter().map(|t| t.posted).min()?, rows.iter().map(|t| t.posted).max()?))
}

fn pct(a: Decimal, b: Decimal) -> Option<f64> {
    (!b.is_zero()).then(|| ((a - b) / b.abs() * Decimal::ONE_HUNDRED).to_f64().unwrap_or(0.0))
}

fn payee(t: &TxRow) -> String {
    if t.clean_payee.is_empty() { rules::title_case(&merchant_stem(&t.raw_description, &t.memo)) } else { t.clean_payee.clone() }
}

#[derive(Debug, Clone, Default)]
pub struct Kpis {
    pub income: Decimal,
    pub expenses: Decimal,
    pub net: Decimal,
    pub net_prior: Decimal,
    pub net_delta_pct: Option<f64>,
    pub savings_rate_pct: Option<f64>,
    pub subs_monthly: Decimal,
    pub subs_annual: Decimal,
    pub statements: usize,
    pub reconciled: usize,
    pub uncategorized: usize,
}

fn flows(rows: &[TxRow], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx) -> (Decimal, Decimal) {
    let (mut inc, mut exp) = (Decimal::ZERO, Decimal::ZERO);
    for t in rows.iter().filter(|t| in_range(t, from, to) && counts(t)) {
        let v = value(t, ccy, fx);
        if v.is_sign_positive() { inc += v } else { exp += -v }
    }
    (inc, exp)
}

pub fn kpis(rows: &[TxRow], docs: &[DocumentRow], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx, today: NaiveDate) -> Kpis {
    let (income, expenses) = flows(rows, from, to, ccy, fx);
    let (pf, pt) = Range::prior(from, to);
    let (pi, pe) = flows(rows, pf, pt, ccy, fx);
    let net = income - expenses;
    let net_prior = pi - pe;
    let subs = subscriptions(rows, ccy, fx, today);
    let subs_monthly: Decimal = subs.iter().filter(|s| s.active).map(|s| s.monthly).sum();
    Kpis {
        income,
        expenses,
        net,
        net_prior,
        net_delta_pct: pct(net, net_prior),
        savings_rate_pct: (!income.is_zero()).then(|| (net / income * Decimal::ONE_HUNDRED).to_f64().unwrap_or(0.0)),
        subs_monthly: round2(subs_monthly),
        subs_annual: round2(subs_monthly * Decimal::from(12)),
        statements: docs.len(),
        reconciled: docs.iter().filter(|d| d.reconciliation == Reconciliation::Reconciled).count(),
        uncategorized: rows.iter().filter(|t| t.category_id.is_none() && t.transfer_peer.is_none()).count(),
    }
}

#[derive(Debug, Clone)]
pub struct MonthBar {
    pub month: NaiveDate,
    pub income: Decimal,
    pub expenses: Decimal,
    pub net: Decimal,
}

/// `n` months ending with the month containing `end`.
pub fn monthly(rows: &[TxRow], end: NaiveDate, n: i32, ccy: Currency, fx: &Fx) -> Vec<MonthBar> {
    (0..n)
        .map(|i| {
            let m = add_months(end, i - n + 1);
            let (inc, exp) = flows(rows, m, add_months(m, 1) - Duration::days(1), ccy, fx);
            MonthBar { month: m, income: inc, expenses: exp, net: inc - exp }
        })
        .collect()
}

fn top_category(t: &TxRow, parents: &BTreeMap<i64, String>) -> String {
    match (t.category_id, &t.category_name) {
        (Some(id), Some(name)) => parents.get(&id).cloned().unwrap_or_else(|| name.clone()),
        _ => "Uncategorized".into(),
    }
}

/// Spending by top-level category, biggest first. `parents` maps a sub-category id to its parent's name.
pub fn by_category(rows: &[TxRow], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> Vec<(String, Decimal)> {
    let mut m: BTreeMap<String, Decimal> = BTreeMap::new();
    for t in rows.iter().filter(|t| in_range(t, from, to) && counts(t)) {
        let v = value(t, ccy, fx);
        if v.is_sign_negative() && t.category_kind != Some(CategoryKind::Income) {
            *m.entry(top_category(t, parents)).or_default() += -v;
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

/// Merchants inside one top-level category (the donut's drill-down).
pub fn merchants_in(rows: &[TxRow], category: &str, from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> Vec<(String, Decimal)> {
    let mut m: BTreeMap<String, Decimal> = BTreeMap::new();
    for t in rows.iter().filter(|t| in_range(t, from, to) && counts(t) && top_category(t, parents) == category) {
        let v = value(t, ccy, fx);
        if v.is_sign_negative() {
            *m.entry(payee(t)).or_default() += -v;
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

#[derive(Debug, Clone, Default)]
pub struct BurnCurve {
    pub month: NaiveDate,
    /// Cumulative spend for day 1..=today's day.
    pub current: Vec<Decimal>,
    /// Day 1..=31 across the previous six months: mean, lowest, highest.
    pub avg: Vec<Decimal>,
    pub lo: Vec<Decimal>,
    pub hi: Vec<Decimal>,
}

fn cumulative(rows: &[TxRow], month: NaiveDate, days: u32, ccy: Currency, fx: &Fx) -> Vec<Decimal> {
    let mut per = vec![Decimal::ZERO; days as usize];
    for t in rows.iter().filter(|t| month_start(t.posted) == month && counts(t)) {
        let v = value(t, ccy, fx);
        if v.is_sign_negative() {
            per[(t.posted.day() as usize - 1).min(days as usize - 1)] += -v;
        }
    }
    let mut acc = Decimal::ZERO;
    per.into_iter().map(|x| {
        acc += x;
        acc
    }).collect()
}

/// This month's cumulative spending against the band of the previous six.
pub fn burn_curve(rows: &[TxRow], as_of: NaiveDate, ccy: Currency, fx: &Fx) -> BurnCurve {
    let month = month_start(as_of);
    let current = cumulative(rows, month, as_of.day(), ccy, fx);
    let history: Vec<Vec<Decimal>> = (1..=6).map(|i| cumulative(rows, add_months(month, -i), 31, ccy, fx)).filter(|c| c.last().is_some_and(|x| !x.is_zero())).collect();
    let mut b = BurnCurve { month, current, ..Default::default() };
    if history.is_empty() {
        return b;
    }
    for d in 0..31 {
        let col: Vec<Decimal> = history.iter().map(|h| h[d]).collect();
        b.avg.push(round2(col.iter().copied().sum::<Decimal>() / Decimal::from(col.len())));
        b.lo.push(col.iter().copied().min().unwrap_or_default());
        b.hi.push(col.iter().copied().max().unwrap_or_default());
    }
    b
}

/// Top ten payees by outflow, with their cumulative share of all outflow (%).
pub fn pareto(rows: &[TxRow], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx) -> Vec<(String, Decimal, f64)> {
    let mut m: BTreeMap<String, Decimal> = BTreeMap::new();
    for t in rows.iter().filter(|t| in_range(t, from, to) && counts(t)) {
        let v = value(t, ccy, fx);
        if v.is_sign_negative() {
            *m.entry(payee(t)).or_default() += -v;
        }
    }
    let total: Decimal = m.values().copied().sum();
    let mut v: Vec<(String, Decimal)> = m.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    let mut acc = Decimal::ZERO;
    v.into_iter()
        .take(10)
        .map(|(k, x)| {
            acc += x;
            let share = if total.is_zero() { 0.0 } else { (acc / total * Decimal::ONE_HUNDRED).to_f64().unwrap_or(0.0) };
            (k, x, share)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct BalanceSeries {
    pub account: String,
    pub bank: String,
    pub points: Vec<(NaiveDate, Decimal)>,
}

/// Balance history per account, from the running balances Pass 4 verified (and each statement's
/// closing balance for accounts that print none). Cards and loans plot as negative: what you owe.
pub fn balances(rows: &[TxRow], docs: &[DocumentRow], ccy: Currency, fx: &Fx) -> Vec<BalanceSeries> {
    let mut by: BTreeMap<i64, BalanceSeries> = BTreeMap::new();
    let liab: BTreeMap<i64, bool> = rows.iter().map(|t| (t.account_id, t.account_type.is_liability())).collect();
    let acc_ccy: BTreeMap<i64, Currency> = rows.iter().map(|t| (t.account_id, t.currency)).collect();
    for t in rows {
        if let Some(b) = t.balance {
            let s = by.entry(t.account_id).or_insert_with(|| BalanceSeries { account: t.account_name.clone(), bank: t.bank_short.clone(), points: Vec::new() });
            let b = if t.account_type.is_liability() { -b } else { b };
            s.points.push((t.posted, fx.convert(b, t.currency, ccy)));
        }
    }
    for d in docs {
        if let (Some(c), Some((_, end))) = (d.closing, d.period) {
            let s = by.entry(d.account_id).or_insert_with(|| BalanceSeries { account: d.account_name.clone(), bank: d.bank_short.clone(), points: Vec::new() });
            if !s.points.iter().any(|p| p.0 == end) {
                let c = if liab.get(&d.account_id).copied().unwrap_or(false) { -c } else { c };
                s.points.push((end, fx.convert(c, acc_ccy.get(&d.account_id).copied().unwrap_or(d.currency), ccy)));
            }
        }
    }
    let mut out: Vec<BalanceSeries> = by.into_values().collect();
    for s in &mut out {
        s.points.sort_by_key(|p| p.0);
        // keep the last balance of each day
        let mut v: Vec<(NaiveDate, Decimal)> = Vec::new();
        for p in &s.points {
            match v.last_mut() {
                Some(l) if l.0 == p.0 => *l = *p,
                _ => v.push(*p),
            }
        }
        s.points = v;
    }
    out
}

#[derive(Debug, Clone)]
pub struct PnlRow {
    pub label: String,
    pub income: Decimal,
    pub expenses: Decimal,
    pub net: Decimal,
}

/// Net income per workspace, then per property/project tag within the selection.
pub fn pnl(rows: &[TxRow], entities: &[Entity], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx) -> Vec<PnlRow> {
    let mut m: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for t in rows.iter().filter(|t| in_range(t, from, to) && counts(t)) {
        let v = value(t, ccy, fx);
        let ent = entities.iter().find(|e| e.id == t.entity_id).map_or("?".to_string(), |e| e.name.clone());
        let mut keys = vec![ent.clone()];
        if !t.tag.is_empty() {
            keys.push(format!("{ent} › {}", t.tag));
        }
        for k in keys {
            let e = m.entry(k).or_default();
            if v.is_sign_positive() { e.0 += v } else { e.1 += -v }
        }
    }
    m.into_iter().map(|(label, (i, e))| PnlRow { label, income: i, expenses: e, net: i - e }).collect()
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub stem: String,
    pub payee: String,
    pub account: String,
    pub currency: Currency,
    pub latest: Decimal,
    pub previous: Option<Decimal>,
    /// The most recent price rise: (old, new, when).
    pub last_rise: Option<(Decimal, Decimal, NaiveDate)>,
    pub cadence_days: i64,
    pub last_charged: NaiveDate,
    pub charges: usize,
    /// In the display currency.
    pub monthly: Decimal,
    pub annual: Decimal,
    pub active: bool,
    pub tx_ids: Vec<i64>,
}

/// Charges flagged as subscriptions, one entry per merchant.
pub fn subscriptions(rows: &[TxRow], ccy: Currency, fx: &Fx, today: NaiveDate) -> Vec<Subscription> {
    let mut by: BTreeMap<String, Vec<&TxRow>> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.is_subscription && t.amount.is_sign_negative()) {
        by.entry(merchant_stem(&t.raw_description, &t.memo)).or_default().push(t);
    }
    let mut out: Vec<Subscription> = by
        .into_iter()
        .map(|(stem, mut v)| {
            v.sort_by_key(|t| t.posted);
            let last = v[v.len() - 1];
            let cad = rules::cadence(&v.iter().map(|t| t.posted).collect::<Vec<_>>()).unwrap_or(30);
            let latest_disp = -value(last, ccy, fx);
            let monthly = round2(latest_disp * Decimal::from(30) / Decimal::from(cad));
            Subscription {
                payee: payee(last),
                account: last.account_name.clone(),
                currency: last.currency,
                latest: -last.amount,
                previous: v.len().checked_sub(2).map(|i| -v[i].amount),
                last_rise: (1..v.len()).rev().find(|&i| -v[i].amount > -v[i - 1].amount && v[i].currency == v[i - 1].currency).map(|i| (-v[i - 1].amount, -v[i].amount, v[i].posted)),
                cadence_days: cad,
                last_charged: last.posted,
                charges: v.len(),
                monthly,
                annual: round2(latest_disp * Decimal::from(365) / Decimal::from(cad)),
                active: (today - last.posted).num_days() <= 2 * cad + 7,
                tx_ids: v.iter().map(|t| t.id).collect(),
                stem,
            }
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.monthly));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsightKind {
    PriceCreep,
    Spike,
    Duplicate,
    BankFees,
    CategoryDrift,
    TaxSummary,
}

impl InsightKind {
    pub const ALL: [InsightKind; 6] = [Self::PriceCreep, Self::Spike, Self::Duplicate, Self::BankFees, Self::CategoryDrift, Self::TaxSummary];
    pub fn label(self) -> &'static str {
        match self {
            Self::PriceCreep => "Subscription price creep",
            Self::Spike => "Utility & vendor spikes",
            Self::Duplicate => "Possible double charges",
            Self::BankFees => "Bank fees & interest",
            Self::CategoryDrift => "Category drift",
            Self::TaxSummary => "Tax / business P&L",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Self::PriceCreep => "go-up-symbolic",
            Self::Spike => "dialog-warning-symbolic",
            Self::Duplicate => "edit-copy-symbolic",
            Self::BankFees => "x-office-spreadsheet-symbolic",
            Self::CategoryDrift => "view-sort-descending-symbolic",
            Self::TaxSummary => "x-office-spreadsheet-symbolic",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Insight {
    pub kind: InsightKind,
    pub title: String,
    pub detail: String,
    pub tx_ids: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct FeeLine {
    pub bank: String,
    pub year: i32,
    pub total: Decimal,
    pub count: usize,
}

/// Every service charge, wire/FX fee and interest charge, per bank per year.
pub fn bank_fees(rows: &[TxRow], ccy: Currency, fx: &Fx) -> Vec<FeeLine> {
    let mut m: BTreeMap<(String, i32), (Decimal, usize)> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer()) {
        if t.category_name.as_deref() == Some("Bank Fees & Interest") || rules::is_bank_fee(&t.raw_description, &t.memo) {
            let e = m.entry((if t.bank_short.is_empty() { "Other".into() } else { t.bank_short.clone() }, t.posted.year())).or_default();
            e.0 += -value(t, ccy, fx);
            e.1 += 1;
        }
    }
    let mut v: Vec<FeeLine> = m.into_iter().map(|((bank, year), (total, count))| FeeLine { bank, year, total, count }).collect();
    v.sort_by(|a, b| b.year.cmp(&a.year).then(b.total.cmp(&a.total)));
    v
}

/// The "What changed?" cards for the window ending `to`.
pub fn insights(rows: &[TxRow], entities: &[Entity], from: NaiveDate, to: NaiveDate, ccy: Currency, fx: &Fx, parents: &BTreeMap<i64, String>) -> Vec<Insight> {
    let mut out = Vec::new();
    let money = |d: Decimal| fmt_money(d, ccy);

    // price creep: a subscription got dearer within the last 90 days
    for s in subscriptions(rows, ccy, fx, to) {
        if let Some((old, new, when)) = s.last_rise
            && s.active
            && (to - when).num_days() <= 90
        {
            let pc = pct(new, old).unwrap_or(0.0);
            let since = if month_start(when) == month_start(to) { "this month".to_string() } else { format!("in {}", when.format("%B")) };
            out.push(Insight {
                kind: InsightKind::PriceCreep,
                title: format!("{} increased by +{pc:.1}% {since}", s.payee),
                detail: format!("{old} → {new} {} per charge ({} a year)", s.currency, money(s.annual)),
                tx_ids: s.tx_ids.clone(),
            });
        }
    }

    // spikes: a bill's latest month vs its own six-month average
    type Months = BTreeMap<NaiveDate, (Decimal, Vec<i64>)>;
    let mut bills: BTreeMap<(i64, String), Months> = BTreeMap::new();
    for t in rows.iter().filter(|t| t.amount.is_sign_negative() && counts(t) && t.posted <= to) {
        let utility = t.category_id.is_some_and(|id| parents.get(&id).map_or(t.category_name.as_deref() == Some("Utilities"), |p| p == "Utilities"));
        if utility || t.is_recurring {
            let e = bills.entry((t.account_id, payee(t))).or_default().entry(month_start(t.posted)).or_default();
            e.0 += -value(t, ccy, fx);
            e.1.push(t.id);
        }
    }
    for ((_, name), months) in &bills {
        let Some((&last_m, (last_v, ids))) = months.iter().next_back() else { continue };
        // the latest bill may have landed late last month
        if last_m < add_months(month_start(from), -1) {
            continue;
        }
        let prior: Vec<Decimal> = (1..=6).filter_map(|i| months.get(&add_months(last_m, -i)).map(|x| x.0)).collect();
        let charges_per_month = months.values().map(|x| x.1.len()).sum::<usize>() as f64 / months.len() as f64;
        if prior.len() < 3 || charges_per_month > 2.0 {
            continue;
        }
        let avg = prior.iter().copied().sum::<Decimal>() / Decimal::from(prior.len());
        // a bill is steady: every prior month within 25% of the average (not a shopping habit)
        if prior.iter().any(|p| (*p - avg).abs() > avg / Decimal::from(4)) {
            continue;
        }
        if let Some(p) = pct(*last_v, avg)
            && p > 20.0
        {
            out.push(Insight {
                kind: InsightKind::Spike,
                title: format!("{name} up {p:.0}% vs 6-month average"),
                detail: format!("{} in {} against an average of {}", money(*last_v), last_m.format("%B"), money(round2(avg))),
                tx_ids: ids.clone(),
            });
        }
    }

    // duplicates: same account, merchant and amount within 72 hours
    let mut seen: Vec<&TxRow> = rows.iter().filter(|t| in_range(t, from, to) && t.amount.is_sign_negative() && counts(t)).collect();
    seen.sort_by_key(|t| (t.account_id, t.posted));
    for (i, a) in seen.iter().enumerate() {
        for b in seen[i + 1..].iter().take_while(|b| b.account_id == a.account_id && (b.posted - a.posted).num_days() <= 3) {
            if a.amount == b.amount && merchant_stem(&a.raw_description, &a.memo) == merchant_stem(&b.raw_description, &b.memo) && !a.is_subscription {
                out.push(Insight {
                    kind: InsightKind::Duplicate,
                    title: format!("{} charged {} twice", payee(a), fmt_money(-a.amount, a.currency)),
                    detail: format!("{} and {} on {}", a.posted.format("%d %b"), b.posted.format("%d %b"), a.account_name),
                    tx_ids: vec![a.id, b.id],
                });
            }
        }
    }

    // fees: this window, per bank
    let window: Vec<TxRow> = rows.iter().filter(|t| in_range(t, from, to)).cloned().collect();
    let fees = bank_fees(&window, ccy, fx);
    if !fees.is_empty() {
        let total: Decimal = fees.iter().map(|f| f.total).sum();
        let mut per: BTreeMap<String, Decimal> = BTreeMap::new();
        for f in &fees {
            *per.entry(f.bank.clone()).or_default() += f.total;
        }
        let mut per: Vec<_> = per.into_iter().collect();
        per.sort_by_key(|x| std::cmp::Reverse(x.1));
        out.push(Insight {
            kind: InsightKind::BankFees,
            title: format!("Banks charged you {}", money(total)),
            detail: per.iter().map(|(b, v)| format!("{b} {}", money(*v))).collect::<Vec<_>>().join(" · "),
            tx_ids: window.iter().filter(|t| t.amount.is_sign_negative() && (t.category_name.as_deref() == Some("Bank Fees & Interest") || rules::is_bank_fee(&t.raw_description, &t.memo))).map(|t| t.id).collect(),
        });
    }

    // drift: categories this month above their trailing three-month average
    let this = month_start(to);
    let cat_month = |m: NaiveDate| by_category(rows, m, add_months(m, 1) - Duration::days(1), ccy, fx, parents);
    let now: BTreeMap<String, Decimal> = cat_month(this).into_iter().collect();
    let prev: Vec<BTreeMap<String, Decimal>> = (1..=3).map(|i| cat_month(add_months(this, -i)).into_iter().collect()).collect();
    let mut drift: Vec<(String, f64, Decimal, Decimal)> = now
        .iter()
        .filter(|(k, _)| k.as_str() != "Uncategorized")
        .filter_map(|(k, v)| {
            let avg = prev.iter().map(|p| p.get(k).copied().unwrap_or_default()).sum::<Decimal>() / Decimal::from(3);
            let p = pct(*v, avg)?;
            (p > 10.0 && !avg.is_zero()).then(|| (k.clone(), p, *v, avg))
        })
        .collect();
    drift.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (k, p, v, avg) in drift.into_iter().take(3) {
        out.push(Insight {
            kind: InsightKind::CategoryDrift,
            title: format!("{k} is {p:.0}% above its 3-month average"),
            detail: format!("{} so far in {} vs {} a month", money(v), this.format("%B"), money(round2(avg))),
            tx_ids: Vec::new(),
        });
    }

    // YTD P&L per workspace and tag
    let ytd_from = NaiveDate::from_ymd_opt(to.year(), 1, 1).unwrap_or(to);
    for r in pnl(rows, entities, ytd_from, to, ccy, fx) {
        out.push(Insight {
            kind: InsightKind::TaxSummary,
            title: format!("{} — {} net YTD", r.label, money(r.net)),
            detail: format!("Income {} · Expenses {}", money(r.income), money(r.expenses)),
            tx_ids: Vec::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, TxFilter};

    #[test]
    fn demo_insights() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let fx = db.fx().unwrap();
        // answer the triage the way a user would: HBO is a subscription, CUC a utility
        let subs_cat = db.category_id("Subscriptions & Digital").unwrap();
        let util = db.category_id("Utilities").unwrap();
        for (pat, cat, sub) in [("HBO MAX", subs_cat, true), ("CUC", util, false)] {
            db.learn(&crate::rules::Rule { id: 0, kind: crate::rules::RuleKind::NormalizedTokens, pattern: pat.into(), clean_payee: None, category_id: cat, is_subscription: sub, is_transfer: false, expected_cadence: None, entity_id: None }).unwrap();
        }
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let (from, to) = Range::ThisMonth.resolve(today, None);
        let parents = BTreeMap::new();
        let ins = insights(&rows, &db.entities().unwrap(), from, to, Currency::KYD, &fx, &parents);
        let titles: Vec<&str> = ins.iter().map(|i| i.title.as_str()).collect();
        assert!(titles.iter().any(|t| t.starts_with("HBO") && t.contains("+12.5% this month")), "{titles:#?}");
        assert!(titles.iter().any(|t| t.contains("Caribbean Utilities") && t.contains("vs 6-month")), "{titles:#?}");
        assert!(ins.iter().any(|i| i.kind == InsightKind::BankFees));
        let all = Range::All.resolve(today, data_span(&rows));
        let dup = insights(&rows, &[], all.0, all.1, Currency::KYD, &fx, &parents);
        assert!(dup.iter().any(|i| i.kind == InsightKind::Duplicate && i.title.contains("Cayman Cabs")), "{:#?}", dup.iter().filter(|i| i.kind == InsightKind::Duplicate).collect::<Vec<_>>());
        let k = kpis(&rows, &db.documents(&[]).unwrap(), all.0, all.1, Currency::KYD, &fx, today);
        assert_eq!(k.statements, k.reconciled + 1, "all but the deliberately garbled demo statement");
        assert!(k.subs_monthly > Decimal::ZERO);
        // transfers never count: card payments don't show up as spending
        assert!(by_category(&rows, all.0, all.1, Currency::KYD, &fx, &parents).iter().all(|(k, _)| k != "Transfers"));
        let fees = bank_fees(&rows, Currency::KYD, &fx);
        assert!(fees.iter().any(|f| f.bank == "Scotiabank") && fees.iter().any(|f| f.bank == "Butterfield"));
    }
}
