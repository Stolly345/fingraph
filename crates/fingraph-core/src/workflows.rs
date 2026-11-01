//! How money really moves on-island, beyond reading statements: split transactions, business spending
//! you paid personally and its reimbursement, receipts attached to transactions, and whether every
//! statement is there (coverage and balance continuity between statements).

use chrono::{Duration, NaiveDate};
use rusqlite::{OptionalExtension, params};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::db::{self, Db, TxFilter, TxRow};
use crate::insights::{add_months, month_start};
use crate::money::{round2, to_minor};
use crate::{Currency, Error, Reconciliation, Result};

// ──────────────────────────────────────────────────────────────── splits

/// One line of a split: part of a charge, in its own category, workspace and project.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SplitLine {
    pub amount: Decimal,
    pub category_id: Option<i64>,
    pub business_entity_id: Option<i64>,
    pub tag: String,
    pub memo: String,
}

impl Db {
    pub fn splits(&self, tx_id: i64) -> Result<Vec<SplitLine>> {
        let mut st = self.conn.prepare("SELECT amount, category_id, business_entity_id, tag, memo FROM splits WHERE transaction_id = ?1 ORDER BY seq")?;
        let rows = st.query_map([tx_id], |r| {
            Ok(SplitLine { amount: db::dec(&r.get::<_, String>(0)?), category_id: r.get(1)?, business_entity_id: r.get(2)?, tag: r.get(3)?, memo: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replace a transaction's split (empty = no split). The lines must add up to the charge exactly,
    /// and money moving between your own accounts can't be split.
    pub fn set_splits(&self, tx_id: i64, lines: &[SplitLine]) -> Result<()> {
        let t = self.transaction(tx_id)?;
        if !lines.is_empty() {
            if t.transfer_peer.is_some() {
                return Err(Error("a transfer between your own accounts can't be split".into()));
            }
            let sum: Decimal = lines.iter().map(|l| l.amount).sum();
            if sum != t.amount {
                return Err(Error(format!("the lines add up to {sum}, the charge is {}", t.amount)));
            }
            if lines.iter().any(|l| l.amount.is_zero()) {
                return Err(Error("a split line can't be zero".into()));
            }
        }
        let tx = self.sp()?;
        tx.execute("DELETE FROM splits WHERE transaction_id = ?1", [tx_id])?;
        for (i, l) in lines.iter().enumerate() {
            tx.execute(
                "INSERT INTO splits (transaction_id, seq, amount, amount_minor, category_id, business_entity_id, tag, memo) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![tx_id, i as i64, l.amount.to_string(), to_minor(l.amount), l.category_id, l.business_entity_id.filter(|b| *b != t.account_entity_id), l.tag.trim(), l.memo.trim()],
            )?;
        }
        if !lines.is_empty() {
            tx.execute("UPDATE transactions SET review_status = 'Confirmed' WHERE id = ?1", [tx_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `transactions()` for reports: each split row becomes its lines, each counted in its own
    /// category and workspace (the workspace filter applies to the lines, not the statement row).
    pub(crate) fn with_split_lines(&self, f: &TxFilter) -> Result<Vec<TxRow>> {
        let wanted: HashSet<i64> = f.entity_ids.iter().copied().collect();
        let rows = self.transactions(&TxFilter { entity_ids: Vec::new(), split_lines: false, ..f.clone() })?;
        let fx = self.fx()?;
        let bases: HashMap<i64, Currency> = self.entities()?.into_iter().map(|e| (e.id, e.base_currency)).collect();
        let cats: HashMap<i64, (String, db::CategoryKind)> = self.categories()?.into_iter().map(|c| (c.id, (c.name, c.kind))).collect();
        let mut lines: HashMap<i64, Vec<(i64, SplitLine)>> = HashMap::new();
        {
            let mut st = self.conn.prepare("SELECT transaction_id, seq, amount, category_id, business_entity_id, tag, memo FROM splits ORDER BY transaction_id, seq")?;
            let it = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, SplitLine { amount: db::dec(&r.get::<_, String>(2)?), category_id: r.get(3)?, business_entity_id: r.get(4)?, tag: r.get(5)?, memo: r.get(6)? }))
            })?;
            for x in it {
                let (t, seq, l) = x?;
                lines.entry(t).or_default().push((seq, l));
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for t in rows {
            let Some(ls) = lines.get(&t.id).filter(|_| t.splits > 0) else {
                if wanted.is_empty() || wanted.contains(&t.entity_id) {
                    out.push(t);
                }
                continue;
            };
            // same-workspace lines share the statement's figure by running total, so they add up to it to the cent
            let mut cum = (Decimal::ZERO, Decimal::ZERO);
            for (seq, l) in ls {
                let mut r = t.clone();
                r.split_seq = Some(*seq);
                r.amount = l.amount;
                r.business_entity_id = l.business_entity_id;
                r.entity_id = l.business_entity_id.unwrap_or(t.account_entity_id);
                let share = (r.entity_id == t.entity_id && !t.amount.is_zero()).then(|| {
                    let before = cum.1;
                    cum.0 += l.amount;
                    cum.1 = round2(t.amount_entity * cum.0 / t.amount);
                    cum.1 - before
                });
                if !wanted.is_empty() && !wanted.contains(&r.entity_id) {
                    continue;
                }
                r.entity_currency = bases.get(&r.entity_id).copied().unwrap_or(t.entity_currency);
                // same workspace: keep the statement's own exchange rate; another: convert
                r.amount_entity = share.unwrap_or_else(|| db::entity_amount(&fx, l.amount, t.currency, r.entity_currency, None, t.posted));
                r.category_id = l.category_id;
                (r.category_name, r.category_kind) = match l.category_id.and_then(|c| cats.get(&c)) {
                    Some((n, k)) => (Some(n.clone()), Some(*k)),
                    None => (None, None),
                };
                if !l.tag.is_empty() {
                    r.tag = l.tag.clone();
                }
                if !l.memo.is_empty() {
                    r.memo = format!("{} | {}", l.memo, t.memo);
                }
                // the printed balance belongs to the statement row: keep it once
                if *seq != 0 {
                    r.balance = None;
                }
                out.push(r);
            }
        }
        Ok(out)
    }
}

// ──────────────────────────────────────────────────────────────── reimbursements

/// Business spending paid from a personal account (a whole charge or one split line).
#[derive(Debug, Clone)]
pub struct OwedItem {
    pub tx_id: i64,
    pub account_id: i64,
    pub fitid: String,
    /// -1 = the whole transaction.
    pub split_seq: i64,
    pub posted: NaiveDate,
    pub payee: String,
    pub account: String,
    /// What the business owes back, in the business's currency (positive).
    pub owed: Decimal,
    pub currency: Currency,
    pub reimbursed: bool,
}

impl Db {
    /// Everything a business workspace spent through your personal accounts, newest first.
    pub fn owed_items(&self, business: i64) -> Result<Vec<OwedItem>> {
        let paid: HashSet<(i64, String, i64)> = {
            let mut st = self.conn.prepare("SELECT account_id, fitid, split_seq FROM reimbursements")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let rows = self.transactions(&TxFilter { entity_ids: vec![business], split_lines: true, ..Default::default() })?;
        Ok(rows
            .into_iter()
            .filter(|t| t.account_entity_id != business && t.amount.is_sign_negative() && !t.is_transfer())
            .map(|t| {
                let seq = t.split_seq.unwrap_or(-1);
                OwedItem {
                    reimbursed: paid.contains(&(t.account_id, t.fitid.clone(), seq)),
                    tx_id: t.id,
                    account_id: t.account_id,
                    fitid: t.fitid.clone(),
                    split_seq: seq,
                    posted: t.posted,
                    payee: t.clean_payee.clone(),
                    account: t.account_name.clone(),
                    owed: -t.amount_entity,
                    currency: t.entity_currency,
                }
            })
            .collect())
    }

    /// Money that came in and could be the business paying you back: credits to accounts outside
    /// the business, nearest to `amount` first.
    pub fn reimbursement_candidates(&self, business: i64, amount: Decimal) -> Result<Vec<TxRow>> {
        let used: HashSet<(i64, String)> = {
            let mut st = self.conn.prepare("SELECT DISTINCT paid_account_id, paid_fitid FROM reimbursements")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut v: Vec<TxRow> = self
            .transactions(&TxFilter::default())?
            .into_iter()
            .filter(|t| t.amount.is_sign_positive() && t.account_entity_id != business && !used.contains(&(t.account_id, t.fitid.clone())))
            .collect();
        v.sort_by_key(|t| ((t.amount_entity - amount).abs(), std::cmp::Reverse(t.posted)));
        v.truncate(25);
        Ok(v)
    }

    /// Mark these items as paid back by `paid_tx` (a transfer in, now filed under Transfers).
    pub fn record_reimbursement(&self, items: &[OwedItem], paid_tx: i64) -> Result<()> {
        let p = self.transaction(paid_tx)?;
        let tx = self.sp()?;
        for i in items {
            tx.execute(
                "INSERT OR REPLACE INTO reimbursements (account_id, fitid, split_seq, paid_account_id, paid_fitid, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![i.account_id, i.fitid, i.split_seq, p.account_id, p.fitid, db::now()],
            )?;
        }
        if p.category_id.is_none() {
            let transfers = self.category_id("Transfers")?;
            tx.execute("UPDATE transactions SET category_id = ?2, review_status = 'Confirmed' WHERE id = ?1", params![paid_tx, transfers])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn undo_reimbursement(&self, item: &OwedItem) -> Result<()> {
        self.conn.execute("DELETE FROM reimbursements WHERE account_id = ?1 AND fitid = ?2 AND split_seq = ?3", params![item.account_id, item.fitid, item.split_seq])?;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────── attachments

#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: i64,
    pub filename: String,
    pub path: PathBuf,
}

impl Db {
    pub fn attachments_dir(&self) -> PathBuf {
        self.path().and_then(|p| p.parent().map(|d| d.join("attachments"))).unwrap_or_else(|| std::env::temp_dir().join("fingraph-attachments"))
    }

    /// Keep a copy of a receipt or invoice with this transaction (by its fingerprint, so it survives rescans).
    pub fn attach(&self, tx_id: i64, src: &Path) -> Result<()> {
        let t = self.transaction(tx_id)?;
        let sha = db::file_sha256(src)?;
        let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
        let stored = format!("{sha}{ext}");
        let dir = self.attachments_dir();
        std::fs::create_dir_all(&dir)?;
        db::private(&dir, 0o700);
        let dst = dir.join(&stored);
        if !dst.exists() {
            std::fs::copy(src, &dst)?;
            db::private(&dst, 0o600);
        }
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| stored.clone());
        self.conn.execute(
            "INSERT INTO attachments (account_id, fitid, filename, stored, added_at) VALUES (?1,?2,?3,?4,?5)",
            params![t.account_id, t.fitid, name, stored, db::now()],
        )?;
        Ok(())
    }

    pub fn attachments(&self, tx_id: i64) -> Result<Vec<Attachment>> {
        let t = self.transaction(tx_id)?;
        let dir = self.attachments_dir();
        let mut st = self.conn.prepare("SELECT id, filename, stored FROM attachments WHERE account_id = ?1 AND fitid = ?2 ORDER BY id")?;
        let rows = st.query_map(params![t.account_id, t.fitid], |r| Ok(Attachment { id: r.get(0)?, filename: r.get(1)?, path: dir.join(r.get::<_, String>(2)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Its file goes too once nothing else is attached to it.
    pub fn detach(&self, attachment_id: i64) -> Result<()> {
        let stored: Option<String> = self.conn.query_row("SELECT stored FROM attachments WHERE id = ?1", [attachment_id], |r| r.get(0)).optional()?;
        self.conn.execute("DELETE FROM attachments WHERE id = ?1", [attachment_id])?;
        if let Some(stored) = stored
            && self.conn.query_row("SELECT COUNT(*) FROM attachments WHERE stored = ?1", [&stored], |r| r.get::<_, i64>(0))? == 0
        {
            let _ = std::fs::remove_file(self.attachments_dir().join(stored));
        }
        Ok(())
    }

    /// Copy the receipts behind these transactions into `dir` as `2026-06-12 Cooling Services 485.00.pdf`.
    /// Returns how many files were written.
    pub fn export_receipts(&self, rows: &[TxRow], dir: &Path) -> Result<usize> {
        std::fs::create_dir_all(dir)?;
        let mut n = 0;
        let mut seen = HashSet::new();
        let mut written = HashSet::new();
        for t in rows.iter().filter(|t| t.attachments > 0) {
            if !seen.insert(t.id) {
                continue;
            }
            for (k, a) in self.attachments(t.id)?.into_iter().enumerate() {
                let ext = a.path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                let safe: String = t.clean_payee.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
                // two receipts that would share a name (same day, payee and amount) both get written
                let base = format!("{} {} {}", t.posted, safe.trim(), t.amount.abs());
                let name = (k + 1..).map(|i| if i > 1 { format!("{base} ({i}){ext}") } else { format!("{base}{ext}") }).find(|n| !written.contains(n)).unwrap_or_default();
                if a.path.exists() {
                    written.insert(name.clone());
                    std::fs::copy(&a.path, dir.join(name))?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

// ──────────────────────────────────────────────────────────────── statement coverage

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonthState {
    /// Before this account's first statement.
    Before,
    /// A statement covers it and reconciled.
    Verified,
    /// A statement covers it but didn't balance.
    Discrepancy,
    /// No statement, though the month is over.
    Missing,
    /// This month: the statement isn't due yet.
    Pending,
}

#[derive(Debug, Clone)]
pub struct Coverage {
    pub account_id: i64,
    pub account: String,
    pub months: Vec<(NaiveDate, MonthState)>,
    /// Statement-to-statement problems: a gap in dates, or a closing balance that isn't the next opening.
    pub breaks: Vec<String>,
}

impl Db {
    /// For each visible account: which of the `n` months up to `today` have a statement, and whether
    /// its statements chain (closing of one = opening of the next).
    pub fn coverage(&self, entity_ids: &[i64], n: i32, today: NaiveDate) -> Result<Vec<Coverage>> {
        let docs = self.documents_all(entity_ids)?;
        let accounts = self.accounts()?;
        let first = add_months(month_start(today), -(n - 1));
        let mut out = Vec::new();
        for a in accounts.iter().filter(|a| !a.hidden && (entity_ids.is_empty() || entity_ids.contains(&a.entity_id))) {
            let mut mine: Vec<&db::DocumentRow> = docs.iter().filter(|d| d.account_id == a.id && d.period.is_some()).collect();
            if mine.is_empty() {
                continue;
            }
            mine.sort_by_key(|d| d.period.map(|p| p.0));
            // the same statement downloaded twice (two file names, same period and balances) counts once
            mine.dedup_by(|q, p| q.period == p.period && q.opening == p.opening && q.closing == p.closing);
            let start = mine[0].period.map(|p| p.0).unwrap_or(first);
            let months = (0..n)
                .map(|i| {
                    let m = add_months(first, i);
                    let end = add_months(m, 1) - Duration::days(1);
                    let over: Vec<_> = mine.iter().filter(|d| d.period.is_some_and(|(s, e)| s <= end && e >= m)).collect();
                    let state = if !over.is_empty() {
                        if over.iter().all(|d| d.reconciliation == Reconciliation::Reconciled) { MonthState::Verified } else { MonthState::Discrepancy }
                    } else if end < start {
                        MonthState::Before
                    } else if m >= month_start(today) {
                        MonthState::Pending
                    } else {
                        MonthState::Missing
                    };
                    (m, state)
                })
                .collect();
            let mut breaks = Vec::new();
            for w in mine.windows(2) {
                let (p, q) = (w[0], w[1]);
                let (Some((_, pe)), Some((qs, _))) = (p.period, q.period) else { continue };
                // overlapping statements (re-issued, or two formats of one month) aren't a chain to check
                if (pe - qs).num_days() > 3 {
                    continue;
                }
                if (qs - pe).num_days() > 3 {
                    breaks.push(format!("No statement between {} and {}", pe.format("%d %b %Y"), qs.format("%d %b %Y")));
                } else if let (Some(c), Some(o)) = (p.closing, q.opening)
                    && c != o
                {
                    breaks.push(format!(
                        "Closing {} on {} ≠ opening {} on {} — a statement may be missing or overlapping",
                        crate::money::fmt_money(c, p.currency),
                        pe.format("%d %b"),
                        crate::money::fmt_money(o, q.currency),
                        qs.format("%d %b %Y")
                    ));
                }
            }
            out.push(Coverage { account_id: a.id, account: a.name.clone(), months, breaks });
        }
        Ok(out)
    }
}

/// Is every account that has statements in `[from, to]` also covered for most (80%) of
/// `[pfrom, pto]`? If not, a "vs prior period" comparison would compare against missing data.
pub fn prior_covered(docs: &[db::DocumentRow], (from, to): (NaiveDate, NaiveDate), (pfrom, pto): (NaiveDate, NaiveDate)) -> bool {
    let days = (pto - pfrom).num_days() + 1;
    let mut by: BTreeMap<i64, Vec<(NaiveDate, NaiveDate)>> = BTreeMap::new();
    for d in docs {
        if let Some(p) = d.period {
            by.entry(d.account_id).or_default().push(p);
        }
    }
    by.values().filter(|ps| ps.iter().any(|(s, e)| *s <= to && *e >= from)).all(|ps| {
        let covered = (0..days).filter(|i| {
            let day = pfrom + Duration::days(*i);
            ps.iter().any(|(s, e)| *s <= day && day <= *e)
        });
        covered.count() as f64 >= 0.8 * days as f64
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rule;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn splits_report_as_lines_in_their_own_workspaces() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let biz = db.entity_by_name("Business — Property").unwrap().unwrap();
        let personal = db.entity_by_name("Personal").unwrap().unwrap();
        let t = db.transactions(&TxFilter { search: "KIRK HOME CENTRE".into(), ..Default::default() }).unwrap()[0].clone();
        let repairs = db.category_id("Repairs & Maintenance").unwrap();
        let home = db.category_id("Household Supplies").unwrap();
        let rental = (t.amount * Decimal::new(2, 0) / Decimal::new(3, 0)).round_dp(2);
        // the lines must add up to the charge
        assert!(db.set_splits(t.id, &[SplitLine { amount: rental, ..Default::default() }]).is_err());
        db.set_splits(
            t.id,
            &[SplitLine { amount: rental, category_id: repairs, business_entity_id: Some(biz), tag: "Unit A".into(), memo: String::new() }, SplitLine { amount: t.amount - rental, category_id: home, ..Default::default() }],
        )
        .unwrap();
        let in_biz = db.transactions(&TxFilter { entity_ids: vec![biz], split_lines: true, search: "KIRK HOME CENTRE".into(), ..Default::default() }).unwrap();
        let in_me = db.transactions(&TxFilter { entity_ids: vec![personal], split_lines: true, search: "KIRK HOME CENTRE".into(), ..Default::default() }).unwrap();
        assert!(in_biz.iter().any(|r| r.id == t.id && r.amount == rental && r.category_id == repairs && r.tag == "Unit A"));
        assert!(in_me.iter().any(|r| r.id == t.id && r.amount == t.amount - rental && r.category_id == home));
        // the statement row itself is untouched (reconciliation still works on it)
        assert_eq!(db.transaction(t.id).unwrap().amount, t.amount);
        // and the business owes you the rental part
        let owed = db.owed_items(biz).unwrap();
        let item = owed.iter().find(|o| o.tx_id == t.id).unwrap().clone();
        assert_eq!(item.owed, -rental);
        assert!(!item.reimbursed);
        let pay = db.reimbursement_candidates(biz, item.owed).unwrap()[0].clone();
        db.record_reimbursement(std::slice::from_ref(&item), pay.id).unwrap();
        assert!(db.owed_items(biz).unwrap().iter().find(|o| o.tx_id == t.id).unwrap().reimbursed);
    }

    #[test]
    fn amount_rules_and_reference_rules() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let fam = db.category_id("Childcare & Nanny").unwrap();
        // the RBC bill payment varies; only the CI$ 100.00 ATM withdrawals count, say, as an allowance
        let rule = Rule { pattern: "BNS".into(), category_id: fam, amount_min: Some(Decimal::new(10000, 2)), amount_max: Some(Decimal::new(10000, 2)), ..Default::default() };
        assert!(rule.applies("ABM WITHDRAWAL", "*BNS CAMANA BAY", Decimal::new(-10000, 2)));
        assert!(!rule.applies("ABM WITHDRAWAL", "*BNS CAMANA BAY", Decimal::new(-25000, 2)));
        db.learn(&rule).unwrap();
        assert!(db.transactions(&TxFilter { search: "BNS CAMANA".into(), ..Default::default() }).unwrap().iter().all(|t| t.category_id == fam));
        // a standing order known only by its reference
        let r = Rule { kind: crate::rules::RuleKind::Substring, pattern: "0527629".into(), ..Default::default() };
        assert!(r.matches("ABM WITHDRAWAL", "*BNS 18 FORUM LANE CAMANA BAY KY 0527629"));
    }

    #[test]
    fn attachments_follow_the_transaction() {
        let dir = std::env::temp_dir().join(format!("fingraph-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("ledger.db")).unwrap();
        db.load_demo().unwrap();
        let t = db.transactions(&TxFilter { search: "A.L. THOMPSON".into(), ..Default::default() }).unwrap()[0].clone();
        let receipt = dir.join("receipt.pdf");
        std::fs::write(&receipt, b"%PDF-1.4 receipt").unwrap();
        db.attach(t.id, &receipt).unwrap();
        assert_eq!(db.transaction(t.id).unwrap().attachments, 1);
        let out = dir.join("export");
        assert_eq!(db.export_receipts(&[db.transaction(t.id).unwrap()], &out).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_finds_a_missing_statement() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let today = d(2026, 10, 27);
        let all = db.coverage(&[], 12, today).unwrap();
        let rbc = all.iter().find(|c| c.account.contains("3390")).unwrap();
        assert!(rbc.breaks.is_empty(), "{:?}", rbc.breaks);
        assert!(rbc.months.iter().all(|(_, s)| matches!(s, MonthState::Verified | MonthState::Pending)), "{:?}", rbc.months);
        // drop June's RBC statement: June is missing and the chain breaks around it
        let june = db.documents(&[]).unwrap().into_iter().find(|x| x.account_name.contains("3390") && x.period.is_some_and(|p| p.0 == d(2026, 6, 1))).unwrap();
        db.delete_document(june.id).unwrap();
        let all = db.coverage(&[], 12, today).unwrap();
        let rbc = all.iter().find(|c| c.account.contains("3390")).unwrap();
        assert!(rbc.months.iter().any(|(m, s)| *m == d(2026, 6, 1) && *s == MonthState::Missing));
        assert_eq!(rbc.breaks.len(), 1, "{:?}", rbc.breaks);
        // and "vs prior period" knows July can't be compared with June
        let docs = db.documents(&[]).unwrap();
        assert!(!prior_covered(&docs, (d(2026, 7, 1), d(2026, 7, 31)), (d(2026, 6, 1), d(2026, 6, 30))));
        assert!(prior_covered(&docs, (d(2026, 9, 1), d(2026, 9, 30)), (d(2026, 8, 1), d(2026, 8, 31))));
    }

    #[test]
    fn split_lines_add_up_to_the_cent() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let t = db.transactions(&TxFilter::default()).unwrap().into_iter().find(|t| t.currency != t.entity_currency && t.transfer_peer.is_none() && t.amount < Decimal::from(-3)).unwrap();
        let third = (t.amount / Decimal::from(3)).round_dp(2);
        let lines: Vec<SplitLine> = [third, third, t.amount - third - third].into_iter().map(|amount| SplitLine { amount, ..Default::default() }).collect();
        db.set_splits(t.id, &lines).unwrap();
        let got: Decimal = db.transactions(&TxFilter { split_lines: true, ..Default::default() }).unwrap().iter().filter(|r| r.id == t.id).map(|r| r.amount_entity).sum();
        assert_eq!(got, t.amount_entity);
    }
}
