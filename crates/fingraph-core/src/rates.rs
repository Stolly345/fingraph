//! Exchange-rate downloads, for currencies other than the pegged KYD and USD. Frankfurter first (central-bank
//! and official rates, decades of history); the fawazahmed0 currency API as the fallback (static files on
//! jsDelivr with a Cloudflare mirror, daily from 2 March 2024). Both answer in units per US dollar. Requests carry
//! only currency codes and dates. Off unless switched on in Preferences.

use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use std::process::Command;
use std::str::FromStr;

use crate::{Currency, Error, Result};

/// The public Frankfurter server.
pub const FRANKFURTER: &str = "https://api.frankfurter.dev";
/// The fallback's first day.
pub const FALLBACK_FROM: (i32, u32, u32) = (2024, 3, 2);
/// At most this many days are fetched one by one from the fallback in one go.
const FALLBACK_MAX_DAYS: i64 = 120;
/// The fallback gives up after this many days in a row it couldn't reach (offline: seconds, not an hour).
const FALLBACK_MAX_MISSES: usize = 3;

/// Rates downloaded for a stretch of days: (day, currency, units per US dollar), and where they came from.
#[derive(Debug, Clone, Default)]
pub struct Downloaded {
    pub rates: Vec<(NaiveDate, Currency, Decimal)>,
    pub source: &'static str,
}

fn get(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-q", "-sS", "--fail", "--max-time", "25", "-A", "FinGraph (personal finance; exchange rates)", url])
        .output()
        .map_err(|e| Error(format!("couldn't run curl: {e}")))?;
    if !out.status.success() {
        return Err(Error(format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn decimal(v: &serde_json::Value) -> Option<Decimal> {
    let d = Decimal::from_str(&v.to_string()).ok().or_else(|| v.as_f64().and_then(|f| Decimal::from_str(&format!("{f:.10}")).ok()))?;
    (d > Decimal::ZERO).then_some(d)
}

/// Frankfurter's `/v2/rates?base=USD&quotes=…&from=…&to=…`: `[{"date","base","quote","rate"}, …]`.
pub fn parse_frankfurter(json: &str, want: &[Currency]) -> Vec<(NaiveDate, Currency, Decimal)> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    items
        .iter()
        .filter(|i| i["base"].as_str() == Some("USD"))
        .filter_map(|i| {
            let day = NaiveDate::parse_from_str(i["date"].as_str()?, "%Y-%m-%d").ok()?;
            let c = Currency::parse(i["quote"].as_str()?).filter(|c| want.contains(c))?;
            Some((day, c, decimal(&i["rate"])?))
        })
        .collect()
}

/// The fallback's `usd.json` for one day: `{"date": "…", "usd": {"gbp": 0.77, …}}`.
pub fn parse_fallback(json: &str, want: &[Currency]) -> Vec<(NaiveDate, Currency, Decimal)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let Some(day) = v["date"].as_str().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else { return Vec::new() };
    want.iter().filter_map(|c| Some((day, *c, decimal(&v["usd"][c.code().to_lowercase()])?))).collect()
}

/// Rates for `want` (pegged ones are skipped) on every day from `from` to `to`, from Frankfurter, else from the
/// fallback (from 2 March 2024, at most 120 days at a time). Fails only when neither answers.
pub fn download(want: &[Currency], from: NaiveDate, to: NaiveDate) -> Result<Downloaded> {
    let want: Vec<Currency> = want.iter().copied().filter(|c| !c.pegged()).collect();
    if want.is_empty() || from > to {
        return Ok(Downloaded::default());
    }
    let codes = want.iter().map(|c| c.code()).collect::<Vec<_>>().join(",");
    let first = get(&format!("{FRANKFURTER}/v2/rates?base=USD&quotes={codes}&from={from}&to={to}"));
    if let Ok(json) = &first {
        let rates = parse_frankfurter(json, &want);
        if !rates.is_empty() {
            return Ok(Downloaded { rates, source: "Frankfurter" });
        }
    }
    let (y, m, d) = FALLBACK_FROM;
    let start = from.max(NaiveDate::from_ymd_opt(y, m, d).unwrap_or(from));
    let mut rates = Vec::new();
    let mut day = start;
    let mut misses = 0;
    while day <= to && (day - start).num_days() < FALLBACK_MAX_DAYS && misses < FALLBACK_MAX_MISSES {
        let urls = [
            format!("https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@{day}/v1/currencies/usd.json"),
            format!("https://{day}.currency-api.pages.dev/v1/currencies/usd.json"),
        ];
        match urls.iter().find_map(|u| get(u).ok()) {
            Some(json) => {
                misses = 0;
                rates.extend(parse_fallback(&json, &want));
            }
            None => misses += 1,
        }
        day += Duration::days(1);
    }
    if rates.is_empty() {
        return Err(Error(format!("no exchange rates could be downloaded ({})", first.err().map_or_else(|| "Frankfurter had none".into(), |e| e.0))));
    }
    Ok(Downloaded { rates, source: "fawazahmed0 currency API" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// The two services' answers as they were on 30 Oct 2026 (trimmed), read the same way: units per US dollar.
    #[test]
    fn both_answers_read_as_units_per_dollar() {
        let gbp = Currency::parse("GBP").unwrap();
        let jmd = Currency::parse("JMD").unwrap();
        let frankfurter = r#"[{"date":"2025-03-10","base":"USD","quote":"GBP","rate":0.77447},{"date":"2025-03-10","base":"USD","quote":"JMD","rate":156.43},{"date":"2025-03-11","base":"USD","quote":"GBP","rate":0.77418},{"date":"2025-03-11","base":"EUR","quote":"GBP","rate":0.8}]"#;
        assert_eq!(
            parse_frankfurter(frankfurter, &[gbp, jmd]),
            vec![(d(2025, 3, 10), gbp, Decimal::from_str("0.77447").unwrap()), (d(2025, 3, 10), jmd, Decimal::from_str("156.43").unwrap()), (d(2025, 3, 11), gbp, Decimal::from_str("0.77418").unwrap())]
        );
        let fallback = r#"{"date":"2025-03-14","usd":{"gbp":0.77230773,"jmd":157.05438003,"kyd":0.83152056}}"#;
        assert_eq!(parse_fallback(fallback, &[gbp]), vec![(d(2025, 3, 14), gbp, Decimal::from_str("0.77230773").unwrap())]);
        assert!(parse_frankfurter("error code: 522", &[gbp]).is_empty());
    }

    /// The real services (network; run by hand: `cargo test -p fingraph-core -- --ignored live_rates`): a past week
    /// of GBP and JMD from Frankfurter, and one day from the fallback.
    #[test]
    #[ignore]
    fn live_rates() {
        let (gbp, jmd) = (Currency::parse("GBP").unwrap(), Currency::parse("JMD").unwrap());
        let got = download(&[gbp, jmd, Currency::KYD], d(2025, 3, 10), d(2025, 3, 14)).unwrap();
        println!("{}: {} rates, e.g. {:?}", got.source, got.rates.len(), got.rates.first());
        assert!(got.rates.iter().any(|r| r.1 == gbp) && got.rates.iter().any(|r| r.1 == jmd) && got.rates.iter().all(|r| r.1 != Currency::KYD));
        let one = get("https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@2025-03-14/v1/currencies/usd.json").unwrap();
        let fb = parse_fallback(&one, &[gbp, jmd]);
        println!("fallback: {fb:?}");
        assert_eq!(fb.len(), 2);
    }
}
