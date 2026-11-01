//! A category guesser that learns from your own ledger: naive Bayes over the words of each
//! transaction's merchant, its amount band, the day of the week, the account and the direction of
//! the money. Local and small; it only suggests (To Review shows its best two), never files.

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeMap;

use crate::db::TxRow;
use crate::rules::merchant_stem;

/// Fewer categorised transactions than this and the guesses aren't worth showing.
pub const MIN_TRAINED: usize = 30;

pub fn features(raw: &str, memo: &str, amount: Decimal, posted: NaiveDate, account_id: i64) -> Vec<String> {
    let mut f: Vec<String> = merchant_stem(raw, memo).split_whitespace().filter(|w| w.len() >= 2 && !w.chars().all(|c| c.is_ascii_digit())).map(|w| format!("W:{w}")).collect();
    let a = amount.abs().to_f64().unwrap_or(0.0);
    // amount bands double: 1–2, 2–4 … 512–1024
    f.push(format!("A:{}", if a < 1.0 { 0 } else { a.log2().floor() as i64 + 1 }));
    f.push(format!("D:{}", posted.weekday()));
    f.push(format!("C:{account_id}"));
    f.push(if amount.is_sign_negative() { "S:out".into() } else { "S:in".into() });
    f
}

#[derive(Debug, Default)]
pub struct Model {
    /// category → (transactions, feature → count, all features)
    by: BTreeMap<i64, (u32, BTreeMap<String, u32>, u32)>,
    vocab: usize,
    total: u32,
}

impl Model {
    /// From every categorised transaction. `None` below `MIN_TRAINED`.
    pub fn train(rows: &[TxRow]) -> Option<Model> {
        let mut m = Model::default();
        let mut vocab: std::collections::BTreeSet<String> = Default::default();
        for t in rows.iter().filter(|t| t.split_seq.is_none()) {
            let Some(c) = t.category_id else { continue };
            let e = m.by.entry(c).or_default();
            e.0 += 1;
            for x in features(&t.raw_description, &t.memo, t.amount, t.posted, t.account_id) {
                *e.1.entry(x.clone()).or_default() += 1;
                e.2 += 1;
                vocab.insert(x);
            }
            m.total += 1;
        }
        m.vocab = vocab.len();
        (m.total as usize >= MIN_TRAINED).then_some(m)
    }

    /// Categories with their probability, likeliest first.
    pub fn guess(&self, feats: &[String]) -> Vec<(i64, f64)> {
        let v = self.vocab as f64 + 1.0;
        let scores: Vec<(i64, f64)> = self
            .by
            .iter()
            .map(|(c, (n, counts, all))| {
                let prior = (*n as f64 / self.total as f64).ln();
                let like: f64 = feats.iter().map(|x| ((*counts.get(x).unwrap_or(&0) as f64 + 1.0) / (*all as f64 + v)).ln()).sum();
                (*c, prior + like)
            })
            .collect();
        let top = scores.iter().map(|s| s.1).fold(f64::MIN, f64::max);
        let z: f64 = scores.iter().map(|s| (s.1 - top).exp()).sum();
        let mut out: Vec<(i64, f64)> = scores.into_iter().map(|(c, s)| (c, (s - top).exp() / z)).collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, TxFilter};

    #[test]
    fn learns_the_demo_ledger() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let known: Vec<&TxRow> = rows.iter().filter(|t| t.category_id.is_some()).collect();
        // hold out every fifth categorised row, train on the rest
        let train: Vec<TxRow> = known.iter().enumerate().filter(|(i, _)| i % 5 != 0).map(|(_, t)| (*t).clone()).collect();
        let test: Vec<&TxRow> = known.iter().enumerate().filter(|(i, _)| i % 5 == 0).map(|(_, t)| *t).collect();
        let m = Model::train(&train).expect("enough to learn from");
        let right = test.iter().filter(|t| m.guess(&features(&t.raw_description, &t.memo, t.amount, t.posted, t.account_id)).iter().take(2).any(|g| Some(g.0) == t.category_id)).count();
        println!("{right} of {} held-out rows in the top two", test.len());
        assert!(right * 10 >= test.len() * 8, "{right}/{}", test.len());
        assert!(Model::train(&train[..10]).is_none());
    }
}
