//! Pass 4 (prove the extraction with the statement's own arithmetic) and Pass 5 (re-scan exactly
//! where it failed). All in statement terms: `stmt_amount` moves the printed balance.

use rust_decimal::Decimal;

use super::{ScannedTx, Section, SegmentCheck, SummaryCheck};
use crate::Reconciliation;

type Key = (u32, f64);

fn key(t: &ScannedTx) -> Key {
    (t.page, t.y)
}

fn before(a: Key, b: Key) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
}

struct Gap {
    /// Lines strictly between these two positions may hold the missing row.
    lo: Option<Key>,
    hi: Key,
    missing: Decimal,
}

/// Balance points: (number of transactions before it, balance, where it was printed).
fn points(s: &Section) -> Vec<(usize, Decimal, Key)> {
    let mut pts: Vec<(usize, Decimal, Key)> = s.checkpoints.iter().map(|&(p, y, b)| (s.transactions.iter().filter(|t| before(key(t), (p, y))).count(), b, (p, y))).collect();
    for (i, t) in s.transactions.iter().enumerate() {
        if let Some(b) = t.balance {
            pts.push((i + 1, b, key(t)));
        }
    }
    pts.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.0.cmp(&b.2.0)).then(a.2.1.total_cmp(&b.2.1)));
    pts.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    pts
}

/// Pass 4. Checks every stretch between two printed balances, flips a sign when exactly one row's
/// flip makes the stretch add up, and checks Opening + Σ = Closing.
fn verify(s: &mut Section) -> Vec<Gap> {
    let pts = points(s);
    // a checkpoint before any transaction is the opening balance; one after all of them, the closing
    let pos_of = |c: &(u32, f64, Decimal)| s.transactions.iter().filter(|t| before(key(t), (c.0, c.1))).count();
    let opening = s.checkpoints.iter().find(|c| pos_of(c) == 0).map(|c| c.2).or(s.opening);
    let closing = s.checkpoints.iter().rev().find(|c| pos_of(c) == s.transactions.len()).map(|c| c.2).or(s.closing);
    s.opening = opening;
    s.closing = closing;

    let mut segments = Vec::new();
    let mut gaps = Vec::new();
    let mut prev: Option<(usize, Decimal, Option<Key>)> = opening.map(|o| (0, o, None));
    let mut misread: Option<usize> = None;
    for (k, &(pos, bal, at)) in pts.iter().enumerate() {
        if misread == Some(k) {
            continue;
        }
        let Some((ppos, pbal, pkey)) = prev else {
            prev = Some((pos, bal, Some(at)));
            continue;
        };
        if pos < ppos || (pos == ppos && bal == pbal) {
            prev = Some((pos, bal, Some(at)));
            continue;
        }
        let expected = bal - pbal;
        let mut got: Decimal = s.transactions[ppos..pos].iter().map(|t| t.stmt_amount).sum();
        if got != expected {
            // self-healing sign: exactly one row whose flip fixes the stretch (prefer unsigned ones)
            let fixes: Vec<usize> = (ppos..pos).filter(|&i| !s.transactions[i].stmt_amount.is_zero() && got - s.transactions[i].stmt_amount * Decimal::TWO == expected).collect();
            let flexible: Vec<usize> = fixes.iter().copied().filter(|&i| s.transactions[i].sign_flexible).collect();
            let pick = if flexible.len() == 1 { Some(flexible[0]) } else if fixes.len() == 1 { Some(fixes[0]) } else { None };
            if let Some(i) = pick {
                let t = &mut s.transactions[i];
                t.stmt_amount = -t.stmt_amount;
                t.amount = -t.amount;
                t.sign_flexible = false;
                s.report.sign_heals.push(format!("{} {}: sign set from the balance change ({})", t.posted, t.description, t.stmt_amount));
                got = expected;
            }
        }
        // one balance off while its neighbours agree with every row between them: that balance was
        // misprinted or (OCR) misread — skip it and check the stretch to the next one instead
        if got != expected
            && let Some(&(npos, nbal, _)) = pts.get(k + 1)
            && npos >= pos
            && s.transactions[ppos..npos].iter().map(|t| t.stmt_amount).sum::<Decimal>() == nbal - pbal
        {
            let read = pos.checked_sub(1).map(|i| &s.transactions[i]);
            s.report.misread_balances.push(format!(
                "{}: balance read as {} but the rows around it say {}",
                read.map_or(String::new(), |t| format!("{} {}", t.posted, t.description)),
                bal,
                pbal + got
            ));
            misread = Some(k + 1);
            let expected = nbal - pbal;
            segments.push(SegmentCheck { from_tx: ppos.checked_sub(1), to_tx: npos.checked_sub(1), expected, got: expected, ok: true });
            prev = Some((npos, nbal, Some(pts[k + 1].2)));
            continue;
        }
        let ok = got == expected;
        segments.push(SegmentCheck { from_tx: ppos.checked_sub(1), to_tx: pos.checked_sub(1), expected, got, ok });
        if !ok {
            gaps.push(Gap { lo: pkey.or_else(|| ppos.checked_sub(1).map(|i| key(&s.transactions[i]))), hi: at, missing: expected - got });
        }
        prev = Some((pos, bal, Some(at)));
    }

    let liability = s.account_type.is_liability();
    let mut sum = SummaryCheck { opening, closing, total_debits: s.total_debits, total_credits: s.total_credits, ..Default::default() };
    for t in &s.transactions {
        let goes_up = t.stmt_amount.is_sign_positive();
        // a debit lowers a deposit balance and raises a card balance
        if goes_up == liability {
            sum.extracted_debits += t.stmt_amount.abs();
        } else {
            sum.extracted_credits += t.stmt_amount.abs();
        }
    }
    let total: Decimal = s.transactions.iter().map(|t| t.stmt_amount).sum();
    sum.difference = match (opening, closing) {
        (Some(o), Some(c)) => Some(c - (o + total)),
        _ => match (s.total_debits, s.total_credits) {
            (Some(d), Some(c)) => {
                let printed = if liability { d - c } else { c - d };
                Some(printed - total)
            }
            _ => None,
        },
    };
    s.report.summary = sum;
    s.report.segments = segments;
    gaps
}

/// Description left on a recovered line once its date and amount are taken out.
fn leftover_text(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().filter(|w| !crate::money::is_amount_core(w) && !crate::money::is_amount_affix(w)).collect();
    let s = words.join(" ");
    let found = super::dates::find_dates(&s);
    let s = match found.first() {
        Some(&(a, b, _)) if a < 4 => format!("{}{}", &s[..a], &s[b..]),
        _ => s,
    };
    let s = s.trim().to_string();
    if s.is_empty() { "(recovered line)".into() } else { s }
}

/// Move skipped line `li` into the ledger as a transaction of `stmt` (statement terms).
fn recover(s: &mut Section, li: usize, stmt: Decimal) {
    let mut line = s.report.skipped.remove(li);
    let at = (line.page, line.y);
    let idx = s.transactions.iter().position(|t| before(at, key(t))).unwrap_or(s.transactions.len());
    // a row split across a page break: its dated half is the last skipped line since the previous transaction
    if line.date.is_none() {
        let floor = idx.checked_sub(1).map(|i| key(&s.transactions[i]));
        let half = s.report.skipped.iter().rposition(|l| l.amount.is_none() && l.date.is_some() && before((l.page, l.y), at) && floor.is_none_or(|f| before(f, (l.page, l.y))));
        if let Some(h) = half {
            let h = s.report.skipped.remove(h);
            line.date = h.date;
            line.text = format!("{} {}", h.text, line.text);
            line.reason = "page-break split row".into();
        }
    }
    let posted = line.date.or_else(|| idx.checked_sub(1).map(|i| s.transactions[i].posted)).or_else(|| s.transactions.get(idx).map(|t| t.posted));
    let Some(posted) = posted else { return };
    let liability = s.account_type.is_liability();
    let description = leftover_text(&line.text);
    s.report.recovered.push(format!("p{} \u{201c}{}\u{201d} → {} ({})", line.page, line.text, stmt, line.reason));
    s.transactions.insert(
        idx,
        ScannedTx {
            posted,
            effective: None,
            description,
            memo: "Recovered by re-scan".into(),
            amount: if liability { -stmt } else { stmt },
            stmt_amount: stmt,
            balance: None,
            fx_original: None,
            page: line.page,
            y: line.y,
            recovered: true,
            sign_flexible: false,
        },
    );
}

/// Passes 4 and 5, then the verdict and a confidence score.
pub fn verify_and_heal(s: &mut Section) {
    let mut gaps = verify(s);
    for _ in 0..4 {
        let mut fixed = false;
        // a failing stretch: the missing row must be among the skipped lines between its two balances
        for g in &gaps {
            let m = g.missing.abs();
            let hit = s.report.skipped.iter().position(|l| l.amount == Some(m) && g.lo.is_none_or(|lo| before(lo, (l.page, l.y))) && before((l.page, l.y), g.hi));
            if let Some(li) = hit {
                recover(s, li, g.missing);
                fixed = true;
                break;
            }
        }
        // no running balance to narrow it down: a single skipped line worth exactly the difference
        if !fixed
            && gaps.is_empty()
            && let Some(d) = s.report.summary.difference.filter(|d| !d.is_zero())
        {
            let hits: Vec<usize> = (0..s.report.skipped.len()).filter(|&i| s.report.skipped[i].amount == Some(d.abs())).collect();
            if hits.len() == 1 {
                recover(s, hits[0], d);
                fixed = true;
            }
        }
        if !fixed {
            break;
        }
        s.report.rescans += 1;
        gaps = verify(s);
    }

    let diff = s.report.summary.difference;
    let failing: Vec<&SegmentCheck> = s.report.segments.iter().filter(|g| !g.ok).collect();
    s.reconciliation = match diff {
        Some(d) if d.is_zero() && failing.is_empty() => Reconciliation::Reconciled,
        Some(d) if !d.is_zero() => Reconciliation::Discrepancy(d),
        Some(_) => Reconciliation::Discrepancy(failing[0].expected - failing[0].got),
        None if s.report.segments.is_empty() => Reconciliation::Unverified,
        None if failing.is_empty() => Reconciliation::Reconciled,
        None => Reconciliation::Discrepancy(failing.iter().map(|g| g.expected - g.got).sum()),
    };
    let n = s.report.segments.len();
    let ok = n - failing.len();
    s.confidence = match s.reconciliation {
        Reconciliation::Reconciled => 1.0,
        Reconciliation::Unverified => 0.7,
        Reconciliation::Discrepancy(_) => 0.4 + if n > 0 { 0.5 * ok as f64 / n as f64 } else { 0.0 },
    };
    // only lines that looked like transactions are worth showing in the inspector
    s.report.skipped.retain(|l| l.amount.is_some() || l.date.is_some());
}
