//! Budgeting methods: the well-known ways to divide what comes in (50/30/20, 70/20/10, 80/20 pay
//! yourself first, zero-based) and how each fills a budget from what you actually spend. Needs are
//! kept at what they usually cost; wants are scaled to fit what the method leaves; saving and paying
//! down debt get the method's share, and anything a method doesn't spend goes to savings.
//!
//! Percentages are of take-home plus your pension contribution: the pension is money saved for you,
//! so it counts toward the savings share and only the rest has to be put aside by hand.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Method {
    /// Your own numbers, no rule.
    #[default]
    Own,
    FiftyThirtyTwenty,
    SeventyTwentyTen,
    EightyTwenty,
    ZeroBased,
}

/// A method's split of the money, in percent. `needs`/`wants` only for 50/30/20; the others give
/// one figure for all spending.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    pub needs: Option<u8>,
    pub wants: Option<u8>,
    pub spend: u8,
    pub save: u8,
    pub debt: u8,
}

impl Method {
    pub const ALL: [Method; 5] = [Self::FiftyThirtyTwenty, Self::SeventyTwentyTen, Self::EightyTwenty, Self::ZeroBased, Self::Own];

    pub fn label(self) -> &'static str {
        match self {
            Self::Own => "My Own Numbers",
            Self::FiftyThirtyTwenty => "50/30/20",
            Self::SeventyTwentyTen => "70/20/10",
            Self::EightyTwenty => "80/20: Pay Yourself First",
            Self::ZeroBased => "Zero-Based",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Self::Own => "Set each category yourself, starting from what it usually costs. No rule to meet.",
            Self::FiftyThirtyTwenty => "Half on needs (housing, bills, insurance, groceries, transport), 30% on wants (eating out, shopping, trips), 20% saved. The balanced default, from Elizabeth Warren's All Your Worth.",
            Self::SeventyTwentyTen => "70% for all spending, needs and wants alike, 20% saved, and 10% to pay down debt faster until it's gone (then it's saved too). Good when there's a card balance to clear.",
            Self::EightyTwenty => "Move 20% to savings the day you're paid, then spend the other 80% without tracking categories closely. The easiest to keep: one weekly allowance instead of a limit per category.",
            Self::ZeroBased => "Give every dollar a job: take-home minus bills, spending, big bills, debt and savings comes to exactly zero. The most hands-on, and the tightest control.",
        }
    }

    pub fn split(self) -> Option<Split> {
        match self {
            Self::FiftyThirtyTwenty => Some(Split { needs: Some(50), wants: Some(30), spend: 80, save: 20, debt: 0 }),
            Self::SeventyTwentyTen => Some(Split { needs: None, wants: None, spend: 70, save: 20, debt: 10 }),
            Self::EightyTwenty => Some(Split { needs: None, wants: None, spend: 80, save: 20, debt: 0 }),
            Self::Own | Self::ZeroBased => None,
        }
    }

    /// "Your 50/30/20 Plan".
    pub fn plan_title(self) -> String {
        match self {
            Self::Own => "Budget Plan".into(),
            Self::EightyTwenty => "Your 80/20 Plan".into(),
            m => format!("Your {} Plan", m.label()),
        }
    }
}

/// One part of a method's split, as the Budget page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    pub name: &'static str,
    /// The method's target, in percent.
    pub target: u8,
    /// This plan's share of the base, in percent.
    pub share: Decimal,
    pub amount: Decimal,
    pub note: &'static str,
    /// On the right side of the target.
    pub ok: bool,
}

impl Method {
    /// How the Budget page groups categories. 80/20 has no category table: one weekly allowance.
    pub fn groups(self) -> &'static [&'static str] {
        match self {
            Self::Own | Self::ZeroBased => &["Bills", "Everyday"],
            Self::FiftyThirtyTwenty => &["Needs", "Wants"],
            Self::SeventyTwentyTen => &["Spending"],
            Self::EightyTwenty => &[],
        }
    }

    /// Which of `groups` a category belongs in.
    pub fn group_of(self, fixed: bool, need: bool) -> Option<usize> {
        match self {
            Self::Own | Self::ZeroBased => Some(if fixed { 0 } else { 1 }),
            Self::FiftyThirtyTwenty => Some(if need { 0 } else { 1 }),
            Self::SeventyTwentyTen => Some(0),
            Self::EightyTwenty => None,
        }
    }

    /// How a plan stands against the method's split. `needs` includes big bills and loan payments;
    /// what isn't spent or paid off counts as saved (the pension is in `base`, and unspent).
    pub fn shares(self, base: Decimal, needs: Decimal, wants: Decimal, debt: Decimal) -> Vec<Share> {
        let saved = base - needs - wants - debt;
        let of = |v: Decimal| share(v, base);
        let under = |name, target: u8, amount: Decimal| {
            let ok = of(amount) <= Decimal::from(target);
            Share { name, target, share: of(amount), amount, note: if ok { "within target" } else { "over target" }, ok }
        };
        let over = |name, target: u8, amount: Decimal, none: &'static str| {
            let ok = of(amount) >= Decimal::from(target);
            let note = if amount.is_zero() { none } else if of(amount) == Decimal::from(target) { "on target" } else if ok { "above target" } else { "below target" };
            Share { name, target, share: of(amount), amount, note, ok }
        };
        let Some(s) = self.split() else { return Vec::new() };
        match self {
            Self::FiftyThirtyTwenty => vec![under("Needs", s.needs.unwrap_or(50), needs), under("Wants", s.wants.unwrap_or(30), wants), over("Saving", s.save, saved, "nothing saved yet")],
            Self::SeventyTwentyTen => vec![under("Spending", s.spend, needs + wants), over("Saving", s.save, saved, "nothing saved yet"), over("Paying Off Debt", s.debt, debt, "no debt to pay down")],
            _ => Vec::new(),
        }
    }
}

/// What a zero-based budget hasn't given a job yet. Below zero: more assigned than comes in.
pub fn to_assign(income: Decimal, budgets: Decimal, big: Decimal, loans: Decimal, debt: Decimal, saving: Decimal) -> Decimal {
    income - budgets - big - loans - debt - saving
}

/// 80/20's weekly spending money: each variable budget's weekly share, added up the way the Budget
/// Plan PDF does, so screen and paper agree.
pub fn weekly_allowance(variable: &[Decimal]) -> Decimal {
    variable.iter().map(|b| (*b * Decimal::from(12) / Decimal::from(52)).round_dp(0)).sum()
}

/// Categories that are needs unless you say otherwise: what you'd have to pay even in a lean month.
pub const NEEDS: &[&str] = &[
    "Bank Fees & Interest",
    "Education",
    "Family Expense",
    "Government & Fees",
    "Groceries",
    "Health & Fitness",
    "Housing",
    "Insurance",
    "Professional Services",
    "Property Expense",
    "Repairs & Maintenance",
    "Transport",
    "Utilities",
];

impl Db {
    /// The categories you count as needs (the rest are wants); `NEEDS` until changed.
    pub fn need_categories(&self) -> Vec<String> {
        self.setting("budget_needs").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_else(|| NEEDS.iter().map(|s| s.to_string()).collect())
    }
    pub fn set_need(&self, category: &str, need: bool) -> Result<()> {
        let mut v = self.need_categories();
        v.retain(|c| c != category);
        if need {
            v.push(category.to_string());
            v.sort();
        }
        self.set_setting("budget_needs", &serde_json::to_string(&v).unwrap_or_default())
    }
}

/// One category going into a method: its name, need or want, and what it usually costs a month.
#[derive(Debug, Clone, PartialEq)]
pub struct CatIn {
    pub name: String,
    pub need: bool,
    pub usual: Decimal,
}

/// What a method sets, a month.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Alloc {
    /// Each category's budget (needs at their usual cost, wants scaled to fit).
    pub budgets: Vec<(String, Decimal)>,
    /// Put aside by hand (the savings share less the pension, plus anything unspent).
    pub savings: Decimal,
    /// Extra paid off debt.
    pub debt: Decimal,
    /// The method's targets: needs (with big bills and loan payments), wants, all spending, saving
    /// (pension included), debt.
    pub target_needs: Option<Decimal>,
    pub target_wants: Option<Decimal>,
    pub target_spend: Decimal,
    pub target_save: Decimal,
    pub target_debt: Decimal,
    /// What doesn't fit, in plain words.
    pub notes: Vec<String>,
}

fn pct(base: Decimal, p: u8) -> Decimal {
    (base * Decimal::from(p) / Decimal::ONE_HUNDRED).round_dp(0)
}

/// Down to the nearest 5.
fn fives(d: Decimal) -> Decimal {
    let five = Decimal::from(5);
    (d / five).floor() * five
}

/// Fill a budget by `method`. `base` = take-home a month plus the pension; `big` = big bills set
/// aside a month; `loans` = loan payments a month (both needs); `owed` = debt to pay down.
/// `None` for methods that don't fill anything (your own numbers, zero-based).
#[allow(clippy::too_many_arguments)]
pub fn allocate(method: Method, base: Decimal, pension: Decimal, big: Decimal, loans: Decimal, owed: Decimal, cats: &[CatIn], money: &dyn Fn(Decimal) -> String) -> Option<Alloc> {
    let s = method.split()?;
    let needs: Decimal = cats.iter().filter(|c| c.need).map(|c| c.usual.round_dp(0)).sum::<Decimal>() + big + loans;
    let wants_usual: Decimal = cats.iter().filter(|c| !c.need).map(|c| c.usual).sum();
    let spend_cap = pct(base, s.spend);
    let mut notes = Vec::new();
    // debt: the method's share while there's debt, never more than what's owed
    let debt_share = pct(base, s.debt);
    let debt = debt_share.min(owed.max(Decimal::ZERO));
    if s.debt > 0 && debt < debt_share {
        notes.push(if owed <= Decimal::ZERO {
            format!("No debt to pay down, so the {}% debt share goes to savings.", s.debt)
        } else {
            format!("Only {} is owed, so the rest of the debt share goes to savings.", money(owed))
        });
    }
    // what wants may have: their own share (50/30/20), or what's left of spending after needs
    let room = match s.wants {
        Some(w) => pct(base, w).min(spend_cap - needs),
        None => spend_cap - needs,
    }
    .max(Decimal::ZERO);
    if let Some(n) = s.needs
        && needs > pct(base, n)
    {
        notes.push(format!("Needs come to {} a month, {} over the {n}% for needs, so wants are trimmed to make up the difference.", money(needs), money(needs - pct(base, n))));
    }
    if needs > spend_cap {
        notes.push(format!("Needs alone ({}) are more than the {}% for spending ({}): there's nothing left for wants, and saving falls short until needs come down.", money(needs), s.spend, money(spend_cap)));
    }
    let k = if wants_usual > Decimal::ZERO { (room / wants_usual).min(Decimal::ONE) } else { Decimal::ONE };
    if k < Decimal::ONE {
        notes.push(format!("Wants usually cost {}; this method leaves {} for them, so each want is cut to {}% of what it usually costs.", money(wants_usual), money(room), (k * Decimal::ONE_HUNDRED).round_dp(0)));
    }
    let budgets: Vec<(String, Decimal)> = cats.iter().map(|c| (c.name.clone(), if c.need { c.usual.round_dp(0) } else { fives(c.usual * k) })).collect();
    let wants: Decimal = budgets.iter().zip(cats).filter(|(_, c)| !c.need).map(|(b, _)| b.1).sum();
    // savings by hand: everything not spent or paid off, the pension being saved already
    let savings = (base - pension - needs - wants - debt).max(Decimal::ZERO);
    let target_save = pct(base, s.save);
    if savings + pension < target_save {
        notes.push(format!("Saving comes to {} a month with your pension, short of the {}% target of {}.", money(savings + pension), s.save, money(target_save)));
    }
    Some(Alloc {
        budgets,
        savings,
        debt,
        target_needs: s.needs.map(|n| pct(base, n)),
        target_wants: s.wants.map(|w| pct(base, w)),
        target_spend: spend_cap,
        target_save,
        target_debt: debt_share,
        notes,
    })
}

/// Whole percent of `base`.
pub fn share(part: Decimal, base: Decimal) -> Decimal {
    if base.is_zero() { Decimal::ZERO } else { (part / base * Decimal::ONE_HUNDRED).round_dp(0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }
    fn m(x: Decimal) -> String {
        x.to_string()
    }
    fn cats() -> Vec<CatIn> {
        vec![
            CatIn { name: "Housing".into(), need: true, usual: d("1500") },
            CatIn { name: "Groceries".into(), need: true, usual: d("600") },
            CatIn { name: "Dining".into(), need: false, usual: d("1000") },
            CatIn { name: "Shopping".into(), need: false, usual: d("1000") },
        ]
    }

    #[test]
    fn fifty_thirty_twenty_trims_wants_and_saves_the_rest() {
        // base 6000: needs 2100 + big 300 = 2400 (under 3000); wants get 30% = 1800 of their usual 2000
        let a = allocate(Method::FiftyThirtyTwenty, d("6000"), d("250"), d("300"), d("0"), d("0"), &cats(), &m).unwrap();
        assert_eq!(a.budgets, vec![("Housing".into(), d("1500")), ("Groceries".into(), d("600")), ("Dining".into(), d("900")), ("Shopping".into(), d("900"))]);
        // 6000 − 250 pension − 2400 needs − 1800 wants
        assert_eq!(a.savings, d("1550"));
        assert_eq!((a.target_needs, a.target_wants, a.target_save), (Some(d("3000")), Some(d("1800")), d("1200")));
        assert!(a.savings + d("250") >= a.target_save);
    }

    #[test]
    fn seventy_twenty_ten_pays_debt_until_its_gone() {
        let a = allocate(Method::SeventyTwentyTen, d("6000"), d("0"), d("300"), d("0"), d("5000"), &cats(), &m).unwrap();
        assert_eq!(a.debt, d("600"));
        // spending 70% = 4200: needs 2400 leave 1800 for wants
        assert_eq!(a.budgets.iter().filter(|b| b.0 == "Dining").map(|b| b.1).sum::<Decimal>(), d("900"));
        assert_eq!(a.savings, d("6000") - d("2400") - d("1800") - d("600"));
        // a small balance: only that much, the rest saved
        let b = allocate(Method::SeventyTwentyTen, d("6000"), d("0"), d("300"), d("0"), d("200"), &cats(), &m).unwrap();
        assert_eq!(b.debt, d("200"));
        assert_eq!(b.savings, a.savings + d("400"));
        assert!(b.notes.iter().any(|n| n.contains("rest of the debt share")));
    }

    #[test]
    fn needs_over_the_line_leave_nothing_for_wants() {
        let a = allocate(Method::EightyTwenty, d("2500"), d("0"), d("300"), d("0"), d("0"), &cats(), &m).unwrap();
        assert!(a.budgets.iter().filter(|b| b.0 == "Dining" || b.0 == "Shopping").all(|b| b.1.is_zero()));
        // what's left after needs (2500 − 2400) is saved, short of the 20%
        assert_eq!(a.savings, d("100"));
        assert!(a.notes.iter().any(|n| n.contains("short of the 20%")));
        assert!(a.notes.iter().any(|n| n.contains("Needs alone")));
    }

    #[test]
    fn each_method_groups_its_categories_its_own_way() {
        assert_eq!(Method::Own.group_of(true, false), Some(0));
        assert_eq!(Method::ZeroBased.group_of(false, true), Some(1));
        assert_eq!(Method::FiftyThirtyTwenty.group_of(true, false), Some(1));
        assert_eq!(Method::SeventyTwentyTen.group_of(false, false), Some(0));
        assert!(Method::EightyTwenty.groups().is_empty());
        assert_eq!(Method::EightyTwenty.group_of(true, true), None);
    }

    #[test]
    fn shares_measure_a_plan_against_its_method() {
        let s = Method::FiftyThirtyTwenty.shares(d("6000"), d("2400"), d("2400"), d("0"));
        assert_eq!(s.iter().map(|x| x.share).collect::<Vec<_>>(), vec![d("40"), d("40"), d("20")]);
        assert!(s[0].ok);
        assert_eq!((s[1].ok, s[1].note), (false, "over target"));
        assert_eq!((s[2].ok, s[2].note), (true, "on target"));
        let s = Method::SeventyTwentyTen.shares(d("6000"), d("3000"), d("600"), d("0"));
        assert_eq!(s[0].amount, d("3600"));
        assert_eq!((s[1].amount, s[1].note), (d("2400"), "above target"));
        assert_eq!(s[2].note, "no debt to pay down");
        assert!(Method::EightyTwenty.shares(d("6000"), d("1"), d("1"), d("0")).is_empty());
    }

    #[test]
    fn zero_based_counts_what_has_no_job() {
        assert_eq!(to_assign(d("5374"), d("1608"), d("155"), d("420"), d("0"), d("3000")), d("191"));
        assert_eq!(to_assign(d("1000"), d("1200"), d("0"), d("0"), d("0"), d("0")), d("-200"));
    }

    #[test]
    fn the_weekly_allowance_matches_the_printed_plan() {
        // 460 → 106, 220 → 51, per category, as `CatPlan::weekly` does
        assert_eq!(weekly_allowance(&[d("460"), d("220")]), d("157"));
    }

    #[test]
    fn own_and_zero_based_fill_nothing() {
        assert!(allocate(Method::Own, d("6000"), d("0"), d("0"), d("0"), d("0"), &cats(), &m).is_none());
        assert!(allocate(Method::ZeroBased, d("6000"), d("0"), d("0"), d("0"), d("0"), &cats(), &m).is_none());
    }
}
