//! Statement dates in every form the five banks print: `20/08/2026`, `20 Aug 2026`, `25-Aug-2026`,
//! `15AUG26`, `20AUG`, `Aug 21`, `2026-08-20`. Year-less dates take their year from the statement
//! period (with Dec→Jan rollover); `NN/NN` order is settled by the profile, then by the period.

use chrono::{Datelike, Duration, NaiveDate};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Partial {
    Iso(NaiveDate),
    Named { day: u32, month: u32, year: Option<i32> },
    Numeric { a: u32, b: u32, year: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    DayFirst,
    MonthFirst,
}

#[derive(Debug, Clone, Copy)]
pub struct DateCtx {
    pub order: Order,
    pub period: Option<(NaiveDate, NaiveDate)>,
    pub fallback_year: i32,
}

const MON: &str = r"jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?";

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?ix)
        \b(?P<iy>\d{{4}})-(?P<im>\d{{2}})-(?P<id>\d{{2}})\b
      | \b(?P<bm>{MON})\.?\s+(?P<bd>\d{{1,2}})(?:st|nd|rd|th)?\b(?:,?\s+(?P<by>\d{{4}})\b)?
      | \b(?P<ad>\d{{1,2}})(?:st|nd|rd|th)?[\s\-]?(?P<am>{MON})\.?(?:[\s\-,]+(?P<ay4>\d{{4}})\b|-?(?P<ay2>\d{{2}})\b|\b)
      | \b(?P<na>\d{{1,2}})[/.\-](?P<nb>\d{{1,2}})[/.\-](?P<ny>\d{{4}}|\d{{2}})\b
      | \b(?P<sa>\d{{1,2}})/(?P<sb>\d{{1,2}})\b"
    ))
    .expect("date regex")
});

fn month_num(s: &str) -> u32 {
    match &s.to_ascii_lowercase()[..3] {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        _ => 12,
    }
}

fn year_of(s: &str) -> i32 {
    let y: i32 = s.parse().unwrap_or(0);
    if s.len() == 2 { 2000 + y } else { y }
}

/// Every date-looking thing in `text`, left to right, with where it was.
pub fn find_dates(text: &str) -> Vec<(usize, usize, Partial)> {
    let mut out = Vec::new();
    for c in DATE_RE.captures_iter(text) {
        let m = c.get(0).unwrap();
        let g = |n: &str| c.name(n).map(|m| m.as_str());
        let p = if let (Some(y), Some(mo), Some(d)) = (g("iy"), g("im"), g("id")) {
            match NaiveDate::from_ymd_opt(year_of(y), mo.parse().unwrap_or(0), d.parse().unwrap_or(0)) {
                Some(d) => Partial::Iso(d),
                None => continue,
            }
        } else if let (Some(mo), Some(d)) = (g("bm"), g("bd")) {
            Partial::Named { day: d.parse().unwrap_or(0), month: month_num(mo), year: g("by").map(year_of) }
        } else if let (Some(d), Some(mo)) = (g("ad"), g("am")) {
            Partial::Named { day: d.parse().unwrap_or(0), month: month_num(mo), year: g("ay4").or(g("ay2")).map(year_of) }
        } else if let (Some(a), Some(b), Some(y)) = (g("na"), g("nb"), g("ny")) {
            Partial::Numeric { a: a.parse().unwrap_or(0), b: b.parse().unwrap_or(0), year: Some(year_of(y)) }
        } else if let (Some(a), Some(b)) = (g("sa"), g("sb")) {
            Partial::Numeric { a: a.parse().unwrap_or(0), b: b.parse().unwrap_or(0), year: None }
        } else {
            continue;
        };
        out.push((m.start(), m.end(), p));
    }
    out
}

/// The year a year-less month/day belongs to: the period's end year, or the year before if that would
/// put it well after the period (a December row on a statement ending in January).
fn infer_year(month: u32, day: u32, ctx: &DateCtx) -> Option<NaiveDate> {
    let Some((_, end)) = ctx.period else {
        return NaiveDate::from_ymd_opt(ctx.fallback_year, month, day);
    };
    let d = NaiveDate::from_ymd_opt(end.year(), month, day).or_else(|| NaiveDate::from_ymd_opt(end.year() - 1, month, day))?;
    if d > end + Duration::days(10) { NaiveDate::from_ymd_opt(end.year() - 1, month, day) } else { Some(d) }
}

fn in_period(d: NaiveDate, ctx: &DateCtx) -> bool {
    ctx.period.is_none_or(|(s, e)| d >= s - Duration::days(40) && d <= e + Duration::days(10))
}

pub fn resolve(p: Partial, ctx: &DateCtx) -> Option<NaiveDate> {
    match p {
        Partial::Iso(d) => Some(d),
        Partial::Named { day, month, year: Some(y) } => NaiveDate::from_ymd_opt(y, month, day),
        Partial::Named { day, month, year: None } => infer_year(month, day, ctx),
        Partial::Numeric { a, b, year } => {
            let (d1, m1, d2, m2) = match ctx.order {
                Order::DayFirst => (a, b, b, a),
                Order::MonthFirst => (b, a, a, b),
            };
            let make = |d, m| match year {
                Some(y) => NaiveDate::from_ymd_opt(y, m, d),
                None => infer_year(m, d, ctx),
            };
            match (make(d1, m1), make(d2, m2)) {
                (Some(x), Some(y)) if !in_period(x, ctx) && in_period(y, ctx) => Some(y),
                (Some(x), _) => Some(x),
                (None, y) => y,
            }
        }
    }
}

/// First date in `text`.
pub fn parse_first(text: &str, ctx: &DateCtx) -> Option<NaiveDate> {
    find_dates(text).into_iter().find_map(|(_, _, p)| resolve(p, ctx))
}

/// A statement period from a line like `Statement Period: 15AUG26 to 15SEP26`,
/// `Aug 17 - Sep 15, 2026` or `01/08/2026 to 31/08/2026`. The end must carry a year.
pub fn parse_period(text: &str, order: Order) -> Option<(NaiveDate, NaiveDate)> {
    let found = find_dates(text);
    if found.len() < 2 {
        return None;
    }
    let ctx0 = DateCtx { order, period: None, fallback_year: 2000 };
    let has_year = |p: &Partial| !matches!(p, Partial::Named { year: None, .. } | Partial::Numeric { year: None, .. });
    // the pair written as a range ("May 16 - Jun 15, 2026", "15AUG26 to 15SEP26") beats whatever date
    // comes after it on the line (a "pay by" date)
    let joined = |i: usize| {
        let between = text[found[i].1..found[i + 1].0].trim().to_lowercase();
        matches!(between.as_str(), "-" | "–" | "—" | "to" | "through" | "thru" | "until")
    };
    let i = (0..found.len() - 1).rev().find(|&i| joined(i) && has_year(&found[i + 1].2)).unwrap_or(found.len() - 2);
    let (start_p, end_p) = (found[i].2, found[i + 1].2);
    if !has_year(&end_p) {
        return None;
    }
    let end = resolve(end_p, &ctx0)?;
    let ctx = DateCtx { order, period: Some((end - Duration::days(400), end)), fallback_year: end.year() };
    let start = resolve(start_p, &ctx)?;
    (start <= end && (end - start).num_days() <= 370).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn forms() {
        let ctx = DateCtx { order: Order::DayFirst, period: Some((ymd(2026, 8, 15), ymd(2026, 9, 15))), fallback_year: 2026 };
        for (s, want) in [
            ("20/08/2026", ymd(2026, 8, 20)),
            ("20 Aug 2026", ymd(2026, 8, 20)),
            ("25-Aug-2026", ymd(2026, 8, 25)),
            ("15AUG26", ymd(2026, 8, 15)),
            ("20AUG", ymd(2026, 8, 20)),
            ("SS 20AUG", ymd(2026, 8, 20)),
            ("Aug 21", ymd(2026, 8, 21)),
            ("2026-08-20", ymd(2026, 8, 20)),
            ("09/03/26", ymd(2026, 9, 3)),
        ] {
            assert_eq!(parse_first(s, &ctx), Some(want), "{s}");
        }
    }

    #[test]
    fn rollover_and_order() {
        let ctx = DateCtx { order: Order::DayFirst, period: Some((ymd(2025, 12, 16), ymd(2026, 1, 15))), fallback_year: 2026 };
        assert_eq!(parse_first("DEC 28", &ctx), Some(ymd(2025, 12, 28)));
        assert_eq!(parse_first("JAN 03", &ctx), Some(ymd(2026, 1, 3)));
        // 01/12 day-first is 1 Dec (in period); month-first reading would be 12 Jan, also in period: profile wins
        assert_eq!(parse_first("01/12/2025", &ctx), Some(ymd(2025, 12, 1)));
        // 12/28 can only be month-first
        assert_eq!(parse_first("12/28/2025", &ctx), Some(ymd(2025, 12, 28)));
    }

    #[test]
    fn periods() {
        assert_eq!(parse_period("Statement Period: 15AUG26 to 15SEP26", Order::DayFirst), Some((ymd(2026, 8, 15), ymd(2026, 9, 15))));
        assert_eq!(parse_period("Aug 17 - Sep 15, 2026", Order::DayFirst), Some((ymd(2026, 8, 17), ymd(2026, 9, 15))));
        assert_eq!(parse_period("Dec 16 - Jan 15, 2026", Order::DayFirst), Some((ymd(2025, 12, 16), ymd(2026, 1, 15))));
        assert_eq!(parse_period("01/08/2026 to 31/08/2026", Order::DayFirst), Some((ymd(2026, 8, 1), ymd(2026, 8, 31))));
        // the pay-by date after the period isn't its end
        assert_eq!(parse_period("***********3575 May 16 - Jun 15, 2026 July 10, 2026 2 of 5", Order::DayFirst), Some((ymd(2026, 5, 16), ymd(2026, 6, 15))));
    }
}
