//! Passes 1–3: from positioned words to candidate transactions.

use chrono::{Datelike, NaiveDate};
use regex::Regex;
use rust_decimal::Decimal;
use std::path::Path;
use std::sync::LazyLock;

use super::dates::{self, DateCtx, Order};
use super::profiles::{self, BankProfile, LayoutKind, RowShape};
use super::{reconcile, ColumnBand, ColumnRole, Diagnostics, ObservedLayout, PageGeometry, ScanResult, ScannedTx, Section, SectionReport, SkippedLine, Token};
use crate::money::{self, Printed};
use crate::{AccountType, Currency, Error, Reconciliation, Result};

pub struct MultiPassScanner<'a> {
    pub profiles: &'a [BankProfile],
    /// Year for year-less dates when no statement period can be found.
    pub fallback_year: i32,
}

impl<'a> MultiPassScanner<'a> {
    pub fn new(profiles: &'a [BankProfile]) -> Self {
        MultiPassScanner { profiles, fallback_year: chrono::Local::now().year() }
    }

    /// A statement file: a PDF, or a bank's CSV / OFX / QFX download.
    pub fn scan_file(&self, path: &Path) -> Result<ScanResult> {
        if let Some(r) = super::exports::read(path, self.profiles) {
            return r;
        }
        let (tokens, ocr) = super::page_tokens(path)?;
        let decimals = money::infer_decimals(tokens.iter().map(|t| t.text.as_str()));
        let mut r = self.scan_tokens(tokens)?;
        if !ocr {
            return Ok(r);
        }
        r.diagnostics.pass1.ocr = true;
        money::with_decimals(decimals, || reread_scanned_amounts(path, &mut r));
        // never fully trusted: OCR can misread a name even when every balance checks out
        for s in &mut r.sections {
            s.confidence = s.confidence.min(0.9);
        }
        Ok(r)
    }

    pub fn scan_tokens(&self, tokens: Vec<Token>) -> Result<ScanResult> {
        if tokens.is_empty() {
            return Err(Error("no text found in the statement".into()));
        }
        // the document's own way of writing amounts (`1.234,56` or `1,234.56`) holds for the whole read
        let decimals = money::infer_decimals(tokens.iter().map(|t| t.text.as_str()));
        money::with_decimals(decimals, || self.scan_rows(build_rows(join_thousands(tokens))))
    }

    fn scan_rows(&self, rows: Vec<Row>) -> Result<ScanResult> {
        let first = self.read(&rows, &Variant::default()).expect("the saved layout always reads");
        let rows_read = first.sections.iter().any(|s| !s.transactions.is_empty());
        if first.is_reconciled() && first.diagnostics.pass3.layout_notes.is_empty() && rows_read {
            return Ok(first);
        }
        // try, then prove: re-read other ways and keep the one the statement's own arithmetic agrees with
        // (a re-read that doesn't add up never replaces the saved layout's read: that's the inspector's job)
        let saved_notes = first.diagnostics.pass3.layout_notes.clone();
        let mut best = first;
        for v in Variant::alternatives() {
            let Some(r) = self.read(&rows, &v) else { continue };
            // unless the saved layout read nothing at all (rows to check by hand beat none): then, while
            // nothing adds up, the read with the most balance checks passing is the nearest to right (a
            // column of balances read as amounts passes none)
            let better = if score(&r).0 > 0 { score(&r) > score(&best) } else { score(&best).0 == 0 && !rows_read && partial(&r) > partial(&best) };
            if better {
                best = r;
            }
        }
        if let Some(how) = best.diagnostics.pass3.read_as.clone()
            && let Some(p) = best.profile_slug.as_ref().and_then(|s| self.profiles.iter().find(|p| &p.slug == s))
        {
            // what the saved layout got wrong comes first: it says which rule the statement broke
            let notes = &mut best.diagnostics.pass3.layout_notes;
            for n in saved_notes.into_iter().rev() {
                if !notes.contains(&n) {
                    notes.insert(0, n);
                }
            }
            notes.insert(
                0,
                format!("This statement didn't read with {}'s saved layout; it was read {how} and checked against its balances. Approving it saves the new layout", p.short_name),
            );
        }
        Ok(best)
    }

    /// One read of the statement, the way `v` says. `None` when that way doesn't apply (no columns to find).
    fn read(&self, rows: &[Row], v: &Variant) -> Option<ScanResult> {
        let mut diag = Diagnostics::default();
        let meta = pass1(self, rows, &mut diag, v);
        let layout = match v.money {
            Some(h) => data_layout(&meta, rows, &mut diag, h)?,
            None => pass2(&meta, rows, &mut diag),
        };
        let mut sections = pass3(&meta, rows, &layout, &mut diag);
        let none_read = sections.iter().all(|s| s.transactions.is_empty());
        diag.pass3.outside_dated_lines = outside_dated_lines(&meta, rows, &layout, none_read);
        for s in &mut sections {
            // newest first: the balances run backwards down the page, so read the page bottom-up
            if transaction_and_posting_swapped(s) {
                for t in &mut s.transactions {
                    if let Some(e) = t.effective {
                        (t.posted, t.effective) = (e, Some(t.posted));
                    }
                }
                diag.pass3.date_columns_swapped = true;
            }
            let flipped = newest_first(s);
            if flipped {
                mirror(s);
                diag.pass3.newest_first = true;
            }
            reconcile::verify_and_heal(s);
            // positions back to the page's own (the inspector points at them); the rows stay oldest first
            if flipped {
                mirror_positions(s);
            }
        }
        layout_notes(&meta, &sections, &mut diag);
        diag.pass3.read_as = v.describe();
        diag.observed = observe(&meta, rows, &sections, &diag);
        Some(ScanResult { profile_slug: meta.profile.map(|p| p.slug.clone()), period: meta.ctx.period, sections, diagnostics: diag })
    }
}

static FEE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:fees?|service\s+charges?|finance\s+charges?|overdraft\s+charges?|interest\s+charged?|purchase\s+interest|cash\s+advance|atm\s+withdrawal)\b").unwrap()
});
static PAID_WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bpayment\s*(?:-\s*)?(?:received|thank\s*you)|\bthank\s+you\s+for\s+(?:your\s+)?payment|\bautopay\s+payment").unwrap());
static PAY_WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:salary|payroll|direct\s+deposit)\b").unwrap());

/// Whether the transactions' own wording agrees with the account type the statement was read as. On a card
/// a fee raises the balance and a "payment received" lowers it; on a bank account it's the other way round,
/// and salary raises it. Reading a card as a bank account flips every sign and still adds up, so this is the
/// only check that can see it: two or more lines that clearly disagree hold the statement (the type isn't
/// changed — a person looks).
fn polarity_note(s: &Section) -> Option<String> {
    let (card, bank) = polarity_votes(s);
    let liability = s.account_type.is_liability();
    let (against, with) = if liability { (bank, card) } else { (card, bank) };
    (against >= 2 && against > 2 * with).then(|| {
        let (was, looks) = (s.account_type.label(), if liability { "a bank account" } else { "a credit card" });
        format!("Read as {was}, but {against} of its lines (fees, payments, salary) move the balance the way {looks}'s would: if it is one, every amount's sign is the wrong way round")
    })
}

/// Whether the lines' own wording confirms the account type read: two or more agree, none disagree.
fn polarity_confirms(s: &Section) -> bool {
    let (card, bank) = polarity_votes(s);
    let (against, with) = if s.account_type.is_liability() { (bank, card) } else { (card, bank) };
    with >= 2 && against == 0
}

/// Lines whose wording says which way they move a card's balance and a bank account's: (card, bank) votes. A
/// charge, interest, a cash advance or an ATM withdrawal raises what a card owes and lowers a bank balance; a
/// payment received does the opposite; salary raises a bank balance. The direction is the printed balance's, so
/// it doesn't depend on which type the statement was read as.
fn polarity_votes(s: &Section) -> (usize, usize) {
    let (mut card, mut bank) = (0, 0);
    for t in &s.transactions {
        let text = format!("{} {}", t.description, t.memo);
        let up = t.stmt_amount > Decimal::ZERO;
        if FEE_WORDS.is_match(&text) {
            if up { card += 1 } else { bank += 1 }
        } else if PAID_WORDS.is_match(&text) {
            if up { bank += 1 } else { card += 1 }
        } else if PAY_WORDS.is_match(&text) && up {
            bank += 1;
        }
    }
    (card, bank)
}

/// Lines that open with a date and carry an amount, between the table's first and last rows, that no table
/// took in: a page whose header wasn't found, say. Their transactions are missing, and if they net to zero the
/// balances can't tell — so they're counted, and a note holds the statement.
fn outside_dated_lines(meta: &Meta, rows: &[Row], layout: &Layout, none_read: bool) -> usize {
    let table = |k: &RowKind| matches!(k, RowKind::Table(_));
    // from the top (a whole section before the table read can go missing: its lines open with dates, a summary box's
    // don't) to the table's last row — after it come listings set aside on purpose; with no table, or nothing read
    // from it, every such line in the statement is missing
    let first = 0;
    let last = match layout.kinds.iter().rposition(table) {
        Some(b) if !none_read => b,
        _ => rows.len().saturating_sub(1),
    };
    (first..=last.min(rows.len().saturating_sub(1)))
        .filter(|&i| layout.kinds[i] == RowKind::Outside)
        .filter(|&i| {
            let r = &rows[i];
            let text = r.text();
            // the amount on the line itself, or on the next (a description that runs on to its amount)
            let next_has_money = rows.get(i + 1).is_some_and(|n| n.page == r.page && n.yc() - r.yc() < 2.6 * r.h() && !money_items(&n.words).is_empty() && leading_date(n, &meta.ctx).is_none());
            (!money_items(&r.words).is_empty() || next_has_money)
                && leading_date(r, &meta.ctx).is_some()
                // (the statement period's line: two dates weeks apart, not a transaction's two dates days apart)
                && dates::parse_period(&text, meta.ctx.order).is_none_or(|(a, b)| (b - a).num_days() < 7)
                && !SUMMARY_ROW.is_match(&text)
        })
        .count()
}

/// Two dates on a line: the transaction's and the day it posted, which is never earlier. When most lines
/// have the "posting" date before the "transaction" date, the two columns were taken the wrong way round
/// (headings unknown, or misleading): the ledger date is the later one.
fn transaction_and_posting_swapped(s: &Section) -> bool {
    let (mut fine, mut wrong) = (0, 0);
    for t in &s.transactions {
        if let Some(e) = t.effective {
            let days = (t.posted - e).num_days();
            // within a few weeks either way (a December/January pair can look a year apart otherwise)
            if days.abs() <= 40 {
                if days >= 0 { fine += 1 } else { wrong += 1 }
            }
        }
    }
    wrong >= 2 && wrong > 3 * fine
}

/// A section listed newest first: its dates go down the page far more often than up.
fn newest_first(s: &Section) -> bool {
    let (mut down, mut up) = (0, 0);
    for w in s.transactions.windows(2) {
        match w[1].posted.cmp(&w[0].posted) {
            std::cmp::Ordering::Less => down += 1,
            std::cmp::Ordering::Greater => up += 1,
            _ => {}
        }
    }
    down >= 2 && down > 3 * up
}

/// Turn a section upside down (last page first, bottom line first), so the balance checks run oldest to
/// newest: every position is mirrored, and the rows put back in that order.
fn mirror(s: &mut Section) {
    mirror_positions(s);
    s.transactions.reverse();
    s.checkpoints.reverse();
}

/// Mirror every position of a section: the last page becomes the first, the bottom line the top. Twice is
/// the identity.
fn mirror_positions(s: &mut Section) {
    // a fixed far page, so mirroring twice gives back every page whatever the section's first one was
    let pages = 100_000;
    let flip = |p: &mut u32, y: &mut f64| {
        *p = pages - *p;
        *y = -*y;
    };
    for t in &mut s.transactions {
        flip(&mut t.page, &mut t.y);
    }
    for c in &mut s.checkpoints {
        flip(&mut c.0, &mut c.1);
    }
    for l in &mut s.report.skipped {
        flip(&mut l.page, &mut l.y);
    }
    for d in &mut s.report.derived {
        flip(&mut d.0, &mut d.1);
    }
}

/// A scanned statement's doubtful amounts, read again one cell at a time (`reread_cell`): an amount worked out
/// from the balances is confirmed when its cell reads as exactly that; a gap the balances couldn't fill is
/// filled when an unreadable dated line's cell now reads as an amount and the statement then adds up.
fn reread_scanned_amounts(path: &Path, r: &mut ScanResult) {
    // the amount columns of each page (not the balance: it's printed on the same line)
    let zones: Vec<(u32, f64, f64)> = r
        .diagnostics
        .pages
        .iter()
        .filter_map(|pg| {
            let bands: Vec<&ColumnBand> = pg.columns.iter().filter(|b| b.role.is_money() && b.role != ColumnRole::Balance).collect();
            Some((pg.page, bands.iter().map(|b| b.x0).reduce(f64::min)? - 4.0, bands.iter().map(|b| b.x1).reduce(f64::max)? + 4.0))
        })
        .collect();
    // the line's own height, about: a taller crop catches the next line's amount
    let cell = |page: u32, y: f64| zones.iter().find(|z| z.0 == page).map(|z| super::reread_cell(path, page, z.1, y - 5.5, z.2, y + 5.5)).unwrap_or_default();
    for s in &mut r.sections {
        let confirmed: Vec<(u32, f64)> = s
            .report
            .derived
            .iter()
            .copied()
            .filter(|&(pg, y)| s.transactions.iter().find(|t| t.page == pg && t.y == y).is_some_and(|t| super::cell_shows_digits(&cell(pg, y), t.stmt_amount)))
            .collect();
        for &(pg, _) in &confirmed {
            s.report.recovered.push(format!("p{pg}: the worked-out amount was read again from its column and matches"));
        }
        s.report.derived.retain(|d| !confirmed.contains(d));
        if s.reconciliation == Reconciliation::Reconciled {
            continue;
        }
        let mut t = s.clone();
        let mut changed = false;
        for l in t.report.skipped.iter_mut().filter(|l| l.date.is_some() && l.amount.is_none()) {
            if let Some(v) = super::cell_amounts(&cell(l.page, l.y)).into_iter().find(|v| Some(*v) != l.balance.map(|b| b.abs())) {
                l.amount = Some(v);
                changed = true;
            }
        }
        if changed {
            reconcile::verify_and_heal(&mut t);
            if t.reconciliation == Reconciliation::Reconciled {
                *s = t;
            }
        }
    }
}

/// A way of reading a statement other than its bank's saved layout.
#[derive(Debug, Clone, Default)]
struct Variant {
    /// Ignore the profile's headings, summary labels and row shape (the generic words only).
    generic: bool,
    /// Day and month the other way round from the profile.
    swap_dates: bool,
    /// Find the columns from where the amounts line up, named by role assignment `n` (see `money_roles`).
    money: Option<usize>,
}

impl Variant {
    fn alternatives() -> Vec<Variant> {
        let mut v = vec![Variant { generic: true, ..Default::default() }, Variant { swap_dates: true, ..Default::default() }];
        v.extend((0..=4).map(|n| Variant { generic: true, money: Some(n), ..Default::default() }));
        v
    }
    fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.money.is_some() {
            parts.push("with its columns found from where the amounts line up");
        } else if self.generic {
            parts.push("with generic column headings");
        }
        if self.swap_dates {
            parts.push("with day and month swapped");
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// For reads that don't add up: balance checks passed, then rows.
fn partial(r: &ScanResult) -> (usize, usize) {
    let passed = r.sections.iter().flat_map(|s| &s.report.segments).filter(|g| g.ok).count();
    (passed, r.sections.iter().map(|s| s.transactions.len()).sum())
}

/// Better reads first: more sections that add up, then more rows, then fewer signs the arithmetic had to
/// correct (a balance on every row lets swapped debit/credit columns add up, one flipped sign at a time),
/// then fewer lines left over.
fn score(r: &ScanResult) -> (usize, usize, std::cmp::Reverse<usize>, std::cmp::Reverse<usize>, bool) {
    use std::cmp::Reverse;
    let ok = r.sections.iter().filter(|s| s.reconciliation == Reconciliation::Reconciled).count();
    let n = r.sections.iter().map(|s| s.transactions.len()).sum();
    let heals = r.sections.iter().map(|s| s.report.sign_heals.len()).sum();
    let skipped = r.sections.iter().map(|s| s.report.skipped.len()).sum();
    (ok, n, Reverse(heals), Reverse(skipped), r.diagnostics.pass3.layout_notes.is_empty())
}

/// Where the statement departed from what its bank profile declares, so a failed read names the rule.
fn layout_notes(meta: &Meta, sections: &[Section], diag: &mut Diagnostics) {
    // whatever the bank: things a person should look at even when the statement adds up
    if diag.pass3.outside_dated_lines > 0 {
        let n = diag.pass3.outside_dated_lines;
        diag.pass3.layout_notes.push(format!(
            "{n} dated line(s) with amounts sit among the transactions but outside the table that was read: if they're transactions, they're missing (they could cancel out, so the balances wouldn't show it)"
        ));
    }
    if diag.pass1.currency_unstated {
        diag.pass3.layout_notes.push(format!(
            "The statement doesn't name its currency (only `$` or nothing): it was read as {}. If it's another (US$, C$…), set the account's currency in Preferences → Workspaces & Accounts",
            sections.first().map_or(Currency::KYD, |s| s.currency)
        ));
    }
    if diag.pass1.date_order_unproven {
        let read = if meta.ctx.order == Order::MonthFirst { "month first" } else { "day first" };
        diag.pass3.layout_notes.push(format!("Every date could be day/month or month/day (none is past the 12th): they were read {read}, check them against the PDF"));
    }
    if meta.ctx.period.is_none() && diag.pass3.yearless_dates > 0 {
        let y = meta.ctx.fallback_year;
        diag.pass3.layout_notes.push(format!("The dates have no year and no statement period was found, so {y} was assumed: check the year"));
    }
    for s in sections {
        if let Some(note) = polarity_note(s) {
            diag.pass3.layout_notes.push(note);
        } else if diag.pass1.type_unstated && !s.transactions.is_empty() && s.transactions.iter().all(|t| t.balance.is_none()) && !polarity_confirms(s) {
            // no running balance and no word on what the account is: a card and a bank account read the same
            // amounts in opposite directions, and both add up
            diag.pass3.layout_notes.push(format!(
                "The statement doesn't say whether it's a card or a bank account, and prints no running balance, so which way its amounts go can't be checked: it was read as {}",
                s.account_type.label()
            ));
        }
    }
    if diag.pass3.debit_and_credit_lines > 0 {
        let n = diag.pass3.debit_and_credit_lines;
        diag.pass3.layout_notes.push(format!("{n} line(s) printed both a debit and a credit: each was read as its own transaction, check their dates"));
    }
    if diag.pass1.currency_guessed {
        let c = sections.first().map_or(Currency::KYD, |s| s.currency);
        diag.pass3.layout_notes.push(format!("The currency was read as {} ({}) from the statement's wording, with no bank profile to confirm it: check it", c.code(), c.name()));
    }
    for n in sections.iter().flat_map(|s| &s.report.notes) {
        if !diag.pass3.layout_notes.contains(n) {
            diag.pass3.layout_notes.push(n.clone());
        }
    }
    let dated_without_amount = |reason: &str| sections.iter().flat_map(|s| &s.report.skipped).filter(|l| l.reason == reason && l.date.is_some() && l.amount.is_none()).count();
    let notes = &mut diag.pass3.layout_notes;
    match meta.shape {
        RowShape::AmountFirst => {
            let n = dated_without_amount("unrecognised line in the table");
            if n > 0 {
                let bank = meta.profile.map_or("the layout", |p| p.short_name.as_str());
                notes.push(format!("{n} dated line(s) had no amount; {bank} is set to print the amount on a transaction's first line"));
            }
        }
        _ => {
            let n = dated_without_amount("dated line without an amount");
            if n > 0 {
                notes.push(format!("{n} dated line(s) never got an amount on the lines below them"));
            }
        }
    }
    let Some(p) = meta.profile else { return };
    let a = &p.header_anchors;
    let declared: Vec<String> = [&a.date, &a.posting_date, &a.description, &a.reference, &a.debit, &a.credit, &a.amount, &a.balance].into_iter().flatten().map(|w| w.to_lowercase()).collect();
    let seen: Vec<String> = diag.pages.iter().flat_map(|pg| pg.header_words.iter().map(|(_, w)| w.to_lowercase())).collect();
    if !declared.is_empty() && !seen.is_empty() && !seen.iter().any(|w| declared.contains(w)) {
        notes.push(format!("None of {}'s column headings ({}) were on the statement; the columns were found from generic words", p.short_name, declared.join(", ")));
    }
    if seen.is_empty() {
        notes.push("No table header was found on any page".into());
    }
}

// ---------------------------------------------------------------- rows & cells

#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub page: u32,
    pub y0: f64,
    pub y1: f64,
    pub words: Vec<Token>,
}

impl Row {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
    pub fn yc(&self) -> f64 {
        (self.y0 + self.y1) / 2.0
    }
    pub fn h(&self) -> f64 {
        self.words.iter().map(Token::h).fold(0.0, f64::max).max(4.0)
    }
    /// Indexes of words that start a cell (a gap wider than ~2/3 of the text height before them).
    fn cell_starts(&self) -> Vec<usize> {
        let gap = 0.6 * self.h();
        (0..self.words.len()).filter(|&i| i == 0 || self.words[i].x0 - self.words[i - 1].x1 > gap).collect()
    }
    fn cells(&self) -> Vec<(f64, f64, String)> {
        let starts = self.cell_starts();
        starts
            .iter()
            .enumerate()
            .map(|(k, &s)| {
                let e = starts.get(k + 1).copied().unwrap_or(self.words.len());
                let ws = &self.words[s..e];
                (ws[0].x0, ws[ws.len() - 1].x1, ws.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "))
            })
            .collect()
    }
}

static GROUP_HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-(]?[$€£]?\d{1,3}$").unwrap());
static GROUP_MID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{3}$").unwrap());
static GROUP_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{3}[.,]\d{2}[-)]?[$€£]?$").unwrap());

/// `10` `750,00` → `10750,00`: thousands written with spaces arrive as separate words. Joined only when the
/// pieces sit one space apart on one line and the run ends with decimals (`REF 12 345` stays as it is).
fn join_thousands(mut tokens: Vec<Token>) -> Vec<Token> {
    tokens.sort_by(|a, b| (a.page, a.yc(), a.x0).partial_cmp(&(b.page, b.yc(), b.x0)).unwrap_or(std::cmp::Ordering::Equal));
    // one space apart (up to ~0.9 of the text height: some fonts space wide), and the first group starts its own
    // cell, so a number ending a description (`FUEL STATION 12`) never joins the amount after it
    let near = |a: &Token, b: &Token| a.page == b.page && (a.yc() - b.yc()).abs() < 0.3 * a.h() && b.x0 > a.x1 && b.x0 - a.x1 <= 0.9 * a.h();
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let starts_cell = i == 0 || !near(&tokens[i - 1], &tokens[i]);
        if starts_cell && GROUP_HEAD.is_match(&tokens[i].text) {
            let mut j = i;
            while j + 1 < tokens.len() && near(&tokens[j], &tokens[j + 1]) && GROUP_MID.is_match(&tokens[j + 1].text) {
                j += 1;
            }
            if j + 1 < tokens.len() && near(&tokens[j], &tokens[j + 1]) && GROUP_TAIL.is_match(&tokens[j + 1].text) {
                let text: String = tokens[i..=j + 1].iter().map(|t| t.text.as_str()).collect();
                out.push(Token { x1: tokens[j + 1].x1, y1: tokens[i].y1.max(tokens[j + 1].y1), text, ..tokens[i].clone() });
                i = j + 2;
                continue;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// Words → rows: same page, vertical centres within half a line of each other.
pub(crate) fn build_rows(mut tokens: Vec<Token>) -> Vec<Row> {
    // rotated runs (account numbers printed up the margin) are taller than wide: not part of any row
    tokens.retain(|t| t.text.chars().count() < 3 || t.y1 - t.y0 < 1.5 * (t.x1 - t.x0));
    // watermark lettering ("SAMPLE" stamped across the page): a letter or two far taller than the text
    // around it, which would otherwise stretch one line over the next
    let mut heights: std::collections::HashMap<u32, Vec<f64>> = Default::default();
    for t in &tokens {
        heights.entry(t.page).or_default().push(t.h());
    }
    let median: std::collections::HashMap<u32, f64> = heights
        .into_iter()
        .map(|(p, mut v)| {
            v.sort_by(f64::total_cmp);
            (p, v[v.len() / 2])
        })
        .collect();
    tokens.retain(|t| t.text.chars().count() > 3 || t.h() <= 2.5 * median[&t.page]);
    tokens.sort_by(|a, b| (a.page, a.yc()).partial_cmp(&(b.page, b.yc())).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<Row> = Vec::new();
    for t in tokens {
        if let Some(r) = rows.last_mut()
            && r.page == t.page
            && (t.yc() - r.yc()).abs() <= 0.45 * t.h().min(r.h())
        {
            r.y0 = r.y0.min(t.y0);
            r.y1 = r.y1.max(t.y1);
            r.words.push(t);
            continue;
        }
        rows.push(Row { page: t.page, y0: t.y0, y1: t.y1, words: vec![t] });
    }
    for r in &mut rows {
        r.words.sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows
}

/// An amount found in a row: its words `lo..hi`, the core word, what it said.
#[derive(Debug, Clone, Copy)]
struct MoneyItem {
    lo: usize,
    hi: usize,
    x0: f64,
    x1: f64,
    printed: Printed,
}

fn money_items(words: &[Token]) -> Vec<MoneyItem> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if !money::is_amount_core(&words[i].text) {
            i += 1;
            continue;
        }
        let near = |a: &Token, b: &Token| b.x0 - a.x1 < 1.2 * a.h();
        let mut lo = i;
        let floor = out.last().map_or(0, |m: &MoneyItem| m.hi);
        while lo > floor && money::is_amount_affix(&words[lo - 1].text) && near(&words[lo - 1], &words[lo]) {
            lo -= 1;
        }
        let mut hi = i + 1;
        while hi < words.len()
            && money::is_amount_affix(&words[hi].text)
            && Currency::parse(&words[hi].text).is_none()
            && near(&words[hi - 1], &words[hi])
            && !money::is_amount_core(&words[hi].text)
        {
            hi += 1;
        }
        let text = words[lo..hi].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        if let Some(printed) = money::parse_printed(&text) {
            out.push(MoneyItem { lo, hi, x0: words[lo].x0, x1: words[hi - 1].x1, printed });
        }
        i = hi;
    }
    out
}

// ---------------------------------------------------------------- pass 1

pub(crate) struct Meta<'p> {
    pub profile: Option<&'p BankProfile>,
    pub ctx: DateCtx,
    pub account_last4: Option<String>,
    pub account_type: AccountType,
    pub currency: Currency,
    pub currency_columns: Vec<(Currency, f64)>,
    pub noise: Vec<Regex>,
    pub fx: Option<Regex>,
    pub layout: LayoutKind,
    pub shape: RowShape,
    /// The bank's own summary labels (opening, closing, debits, credits), tried before the generic ones.
    pub labels: [Option<Regex>; 4],
    /// The bank's closing labels anywhere in a line: they end a table like "closing balance" does.
    pub table_end: Option<Regex>,
    /// Headings from the generic words only (a re-read that ignores the profile's).
    pub generic: bool,
}

/// `["beginning balance"]` → `(?i)^(?:beginning\s+balance)`: a declared label, any spacing.
fn label_regex(labels: &[String]) -> Option<Regex> {
    label_pattern(labels, "^(?:", ")")
}

/// The same labels anywhere in a line, as whole words.
fn label_anywhere(labels: &[String]) -> Option<Regex> {
    label_pattern(labels, r"\b(?:", r")\b")
}

fn label_pattern(labels: &[String], open: &str, close: &str) -> Option<Regex> {
    let alts: Vec<String> = labels.iter().map(|l| l.split_whitespace().map(regex::escape).collect::<Vec<_>>().join(r"\s*")).filter(|a| !a.is_empty()).collect();
    (!alts.is_empty()).then(|| Regex::new(&format!("(?i){open}{}{close}", alts.join("|"))).ok()).flatten()
}

static LAST4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:[*xX•]{3,}[\s-]*(\d{4})\b|account\s*(?:number|no\.?|#)\s*:?\s*[\d\s*xX-]*?(\d{4})\b)").unwrap()
});

pub(crate) static OPENING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:opening|previous|beginning|starting)\s+balance|balance\s+(?:brought\s+)?forward|brought\s+forward|solde\s+(?:précédent|precedent|d'ouverture|initial|reporté|reporte)|saldo\s+(?:anterior|inicial))").unwrap());
/// Closing labels that can't mean anything else, tried before the generic ones: "carried forward" can also
/// caption an opening balance ("Carried forward from the prior cycle").
static CLOSING_PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:(?:closing|new|ending)\s+balance|nouveau\s+solde|solde\s+(?:final|de\s+fermeture)|saldo\s+final)").unwrap());
pub(crate) static CLOSING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:closing|new|ending)\s+balance|balance\s+carried\s+forward|carried\s+forward|nouveau\s+solde|solde\s+(?:final|de\s+fermeture|de\s+clôture)|saldo\s+(?:final|actual))").unwrap());
static DEBITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:total\s+)?(?:withdrawals|debits|minus)\b|^purchases\b|^total\s+purchases|^total\s+(?:withdrawal|debit)s?\s+amount").unwrap()
});
static CREDITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:total\s+)?(?:deposits|credits|plus)\b|^payments\s*(?:&|and)\s*credits|^total\s+(?:deposit|credit)s?\s+amount").unwrap());
/// Words that may precede a summary amount on the line under its label (CNB: `as of 01/03/26   1,000.00`).
static AS_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:as|of|\d{1,2}/\d{1,2}/\d{2,4})$").unwrap());
static GENERIC_NOISE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^\s*page\s+\d+\s*(?:of\s+\d+)?\s*$",
        r"(?i)continued\s+(?:on|from)\s+(?:the\s+)?(?:next|previous|reverse)\s+page",
        r"(?i)^\s*\(?continued\)?\s*$",
        r"(?i)^\s*\*?\s*trademarks?\s+of\b",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// The amount under a summary label: (amount, the line, the label as printed).
/// `liability`: a card's `1,234.56 CR` is a credit (nothing owed, below zero); a bank account's `DR` is overdrawn.
pub(crate) fn summary_value(rows: &[&Row], label: &Regex, cols: &[(Currency, f64)], want: Currency, liability: bool) -> Option<(Decimal, String, String)> {
    for (ri, r) in rows.iter().enumerate() {
        for s in r.cell_starts() {
            let rest = r.words[s..].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            if !label.is_match(&rest) {
                continue;
            }
            // the amount must open the next cell ("Purchases & Debits    US$ 15.99"), so a count like
            // "No. of Deposits   3" never lends its neighbour's amount
            // a typewriter font spaces the label out ("Deposits  &  Other Cred"): skip its words, stop at the first number
            let starts = r.cell_starts();
            let k = starts.iter().copied().find(|&c| c > s && r.words[c].text.chars().any(|ch| ch.is_ascii_digit() || ch == '$')).unwrap_or(r.words.len());
            let after: Vec<Token> = r.words[k..].to_vec();
            // `as_of`: the line under the label may start `as of 01/02/26` (never the line above: that's another label's)
            // a currency code leads the amount, even one FinGraph doesn't keep accounts in (`INR 84,913.55`)
            let code = |w: &str| Currency::parse(w).is_some() || w == "$" || (w.len() == 3 && w.bytes().all(|b| b.is_ascii_uppercase()));
            let first_money = |ws: &[Token], as_of: bool| -> Option<Vec<MoneyItem>> {
                let items = money_items(ws);
                let lead = ws.iter().take_while(|w| code(&w.text) || (as_of && AS_OF.is_match(&w.text))).count();
                (items.first()?.lo <= lead).then_some(items)
            };
            // the line above or below, from the label's own column on (a summary box beside the address)
            let label_x = r.words[s].x0 - 4.0;
            let near = |o: Option<&&Row>, as_of: bool| {
                o.filter(|n| n.page == r.page && (n.yc() - r.yc()).abs() < 2.6 * r.h()).and_then(|n| first_money(&n.words.iter().filter(|w| w.x0 >= label_x).cloned().collect::<Vec<_>>(), as_of))
            };
            let Some(items) = first_money(&after, false).or_else(|| near(rows.get(ri + 1), true)).or_else(|| near(ri.checked_sub(1).and_then(|i| rows.get(i)), false)) else {
                continue;
            };
            let pick = if cols.len() > 1 {
                let target = cols.iter().find(|(c, _)| *c == want)?.1;
                items.iter().min_by(|a, b| ((a.x0 + a.x1) / 2.0 - target).abs().total_cmp(&((b.x0 + b.x1) / 2.0 - target).abs()))?
            } else {
                &items[0]
            };
            let p = pick.printed;
            let v = balance_value(&p, liability);
            let printed = label.find(&rest).map_or(String::new(), |m| m.as_str().split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase());
            return Some((v, r.text(), printed));
        }
    }
    None
}

/// The statement's currency from what page 1 names, and whether it names one at all (else it's KYD by default).
/// Other currencies count only outside the transactions (`outside`: a card's `EUR 45.00` purchase isn't the
/// card's currency) and only written as codes are (`Dear Ron,` isn't the Romanian leu).
fn detect_currency(page1: &str, outside: &str) -> (Currency, bool) {
    let no_parens = Regex::new(r"\([^)]*\)").unwrap().replace_all(page1, "");
    let up = no_parens.to_uppercase();
    let kyd = up.matches("KYD").count() + up.matches("CI$").count() * 2;
    let usd = up.matches("USD").count() + up.matches("US$").count() * 2;
    let outside = Regex::new(r"\([^)]*\)").unwrap().replace_all(outside, "").into_owned();
    let up = outside.to_uppercase();
    // a statement from a bank back home names its own currency more than either (`GBP`, `£`, `C$`)
    let named = |c: Currency| -> usize {
        let code = Regex::new(&format!(r"\b{}\b", c.code())).map_or(0, |r| r.find_iter(&outside).count());
        let sym = c.symbol().to_uppercase();
        let symbol = if sym == c.code() {
            0
        } else {
            // a `$` symbol counts only on its own (`S$` inside `US$` is the US dollar's)
            Regex::new(&format!(r"(?:^|[^A-Z]){}", regex::escape(&sym))).map_or(0, |r| r.find_iter(&up).count())
        };
        code + 2 * symbol
    };
    let best = Currency::all().filter(|c| !c.pegged()).map(|c| (c, named(c))).max_by_key(|x| x.1);
    if let Some((c, n)) = best
        && n >= 2
        && n > kyd.max(usd)
    {
        return (c, true);
    }
    let said = kyd + usd + best.map_or(0, |b| b.1) > 0;
    (if usd > kyd { Currency::USD } else { Currency::KYD }, said)
}

/// Words that state a bank account's kind (cards are told by their own wording in `detect_type`).
static TYPE_STATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:chequing|checking|current|savings|share|deposit|loan|business|personal)\s+(?:account|acct|a/c)\b|\baccount\s+type\b|\b(?:compte\s+(?:chèques|courant|d'épargne)|cuenta\s+(?:corriente|de\s+ahorros?))\b").unwrap());

/// CNB prints the account kind as a code: `ACCOUNT #: SV - 1234567`.
static SAVINGS_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"account\s*#\s*:?\s*sv\b").unwrap());

/// Card and loan wording, as whole words (`advisable` isn't Visa, a `Principal Office` isn't a loan).
static CARD_WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:credit\s+limit|minimum\s+payment|credit\s+card|card\s+number|mastercard|visa|american\s+express)\b").unwrap());
static LOAN_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:loan\s+(?:account|balance)|principal\s+(?:balance|outstanding|amount|due|paid|repaid|and\s+interest|&\s+interest))\b").unwrap());

fn detect_type(page1: &str) -> AccountType {
    let t = page1.to_lowercase();
    if CARD_WORDS.is_match(&t) {
        AccountType::CreditCard
    } else if LOAN_WORDS.is_match(&t) {
        AccountType::Loan
    } else if t.contains("share savings") || t.contains("share account") {
        AccountType::ShareAccount
    } else if t.contains("savings") || SAVINGS_CODE.is_match(&t) {
        AccountType::Savings
    } else {
        AccountType::Checking
    }
}

// `03/04`, `03-04-2025`, `03.04.2025` — a dotted pair only with its year, or every `5.20` would count
static NUMERIC_DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{1,2})[/-](\d{1,2})\b|\b(\d{1,2})\.(\d{1,2})\.\d{2,4}\b").unwrap());

/// Day or month first, for a bank with no profile: whichever the statement's own numeric dates allow
/// (`09/23` can only be month first). Day first when nothing tells.
fn infer_order(rows: &[Row]) -> Order {
    let (day, month, _) = order_evidence(rows);
    if month > day { Order::MonthFirst } else { Order::DayFirst }
}

/// The one day/month order under which the statement is a statement: its period (if printed) spans at most 100
/// days (a quarter) and every line's leading date falls inside it, give or take a week — or, with no period, the
/// dates span at most 100 days. `03/01 – 12/01` with lines on `05/01` is January; read the other way it would run
/// March to December. `None` when both readings fit, or neither: then it stays a guess.
fn plausible_order(rows: &[Row], fallback_year: i32) -> Option<Order> {
    let fits = |order: Order| -> bool {
        let period = rows.iter().find_map(|r| dates::parse_period(&r.text(), order).filter(|(a, b)| (*b - *a).num_days() >= 7));
        // read the lines without the period, so a date can't quietly flip to fit it
        let ctx = DateCtx { order, period: None, fallback_year: period.map_or(fallback_year, |p| p.1.year()) };
        let ds: Vec<NaiveDate> = rows.iter().filter(|r| leading_date(r, &ctx).is_some()).filter_map(|r| dates::parse_first(&r.text(), &ctx)).collect();
        let (Some(lo), Some(hi)) = (ds.iter().min(), ds.iter().max()) else { return false };
        match period {
            Some((a, b)) => (b - a).num_days() <= 100 && *lo >= a - chrono::Duration::days(7) && *hi <= b + chrono::Duration::days(7),
            None => (*hi - *lo).num_days() <= 100,
        }
    };
    match (fits(Order::DayFirst), fits(Order::MonthFirst)) {
        (true, false) => Some(Order::DayFirst),
        (false, true) => Some(Order::MonthFirst),
        _ => None,
    }
}

/// Numeric dates that can only be day-first, only month-first, and that could be either (`03/04`).
fn order_evidence(rows: &[Row]) -> (usize, usize, usize) {
    let (mut day, mut month, mut either) = (0, 0, 0);
    for r in rows {
        let text = r.text();
        for c in NUMERIC_DATE.captures_iter(&text) {
            // the month and day of an ISO date (`2025-03-04`) say nothing about order
            let start = c.get(0).map_or(0, |m| m.start());
            if text[..start].ends_with(|ch: char| ch == '-' || ch.is_ascii_digit()) {
                continue;
            }
            let num = |i: usize, j: usize| c.get(i).or(c.get(j)).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
            let (a, b) = (num(1, 3), num(2, 4));
            day += (a > 12 && b <= 12) as usize;
            month += (b > 12 && a <= 12) as usize;
            either += ((1..=12).contains(&a) && (1..=12).contains(&b) && a != b) as usize;
        }
    }
    (day, month, either)
}

fn pass1<'p>(scanner: &MultiPassScanner<'p>, rows: &[Row], diag: &mut Diagnostics, v: &Variant) -> Meta<'p> {
    let tokens: Vec<Token> = rows.iter().flat_map(|r| r.words.iter().cloned()).collect();
    // page 1 outside its transactions: rows before the first and after the last that has a date and an
    // amount (either date order: the bank, and so its order, isn't known yet)
    let either = |r: &Row| {
        [Order::DayFirst, Order::MonthFirst].into_iter().any(|o| dates::parse_first(&r.text(), &DateCtx { order: o, period: None, fallback_year: scanner.fallback_year }).is_some()) && !money_items(&r.words).is_empty()
    };
    let p1: Vec<&Row> = rows.iter().filter(|r| r.page == 1).collect();
    let (first, last) = (p1.iter().position(|r| either(r)), p1.iter().rposition(|r| either(r)));
    let outside: String = p1.iter().enumerate().filter(|(i, _)| first.is_none_or(|f| *i < f) || last.is_some_and(|l| *i > l)).map(|(_, r)| r.text()).collect::<Vec<_>>().join("\n");
    let (profile, scores) = profiles::fingerprint(scanner.profiles, &outside, &tokens);
    let mut order = profile.map_or_else(|| infer_order(rows), BankProfile::date_order);
    // no bank to say how it writes dates, and no date past the 12th to tell: the reading that keeps the statement
    // a statement (its dates inside a period of a quarter at most) wins; if both or neither do, it's a guess
    let mut order_unproven = false;
    if profile.is_none() {
        let (day, month, either) = order_evidence(rows);
        if day == 0 && month == 0 && either > 0 {
            match plausible_order(rows, scanner.fallback_year) {
                Some(o) => order = o,
                None => order_unproven = true,
            }
        }
    }
    if v.swap_dates {
        order = if order == Order::DayFirst { Order::MonthFirst } else { Order::DayFirst };
    }

    // the period: a row that says "period"/"from", or the row right under it, or any row with two dated ends
    let mut period = None;
    for (i, r) in rows.iter().enumerate() {
        let t = r.text();
        let lower = t.to_lowercase();
        if lower.contains("period") || lower.contains("statement from") || lower.contains("statement date") {
            let next = rows.get(i + 1).filter(|n| n.page == r.page).map(Row::text).unwrap_or_default();
            period = dates::parse_period(&t, order).or_else(|| dates::parse_period(&next, order)).or_else(|| dates::parse_period(&format!("{t} {next}"), order));
            if period.is_some() {
                break;
            }
        }
    }
    if period.is_none() {
        period = rows.iter().find_map(|r| dates::parse_period(&r.text(), order));
    }

    let page1: String = rows.iter().filter(|r| r.page == 1).map(Row::text).collect::<Vec<_>>().join("\n");
    // the letterhead: page 1 up to the first row that looks like a transaction (a date and an amount)
    let ctx0 = DateCtx { order, period, fallback_year: scanner.fallback_year };
    let head: String = rows
        .iter()
        .filter(|r| r.page == 1)
        .take_while(|r| dates::parse_first(&r.text(), &ctx0).is_none() || money_items(&r.words).is_empty() || dates::parse_period(&r.text(), order).is_some())
        .map(Row::text)
        .collect::<Vec<_>>()
        .join("\n");
    let account_last4 = LAST4.captures(&page1).and_then(|c| c.get(1).or(c.get(2))).map(|m| m.as_str().to_string());
    let mut account_type = detect_type(&head);
    let layout = profile.map_or(LayoutKind::DebitCreditWithBalance, |p| p.column_layout_kind);
    if layout == LayoutKind::CreditCardDRCR {
        account_type = AccountType::CreditCard;
    }
    diag.pass1.date_order_unproven = order_unproven;
    // said in so many words? ("Savings Bank" in a name doesn't say what this account is)
    diag.pass1.type_unstated = profile.is_none() && account_type != AccountType::CreditCard && !TYPE_STATED.is_match(&page1);
    let (currency, said) = detect_currency(&page1, &outside);
    diag.pass1.currency_unstated = !said && profile.is_none();
    diag.pass1.currency_guessed = !currency.pegged() && profile.is_none();
    let currency_columns: Vec<(Currency, f64)> = rows
        .iter()
        .find_map(|r| {
            let cols: Vec<(Currency, f64)> = r
                .cells()
                .iter()
                .filter(|(_, _, t)| t == "KYD" || t == "USD")
                .filter_map(|(a, b, t)| Some((Currency::parse(t)?, (a + b) / 2.0)))
                .collect();
            (cols.len() == 2 && cols[0].0 != cols[1].0).then_some(cols)
        })
        .unwrap_or_default();

    let mut noise: Vec<Regex> = GENERIC_NOISE.clone();
    if let Some(p) = profile {
        noise.extend(p.noise_patterns.iter().filter_map(|n| Regex::new(n).ok()));
    }
    let fx = profile.and_then(|p| Regex::new(&p.fx_note_regex).ok());

    diag.pass1.letterhead = rows.iter().filter(|r| r.page == 1).take(6).map(Row::text).collect();
    // cells kept apart (two spaces) so a label and the name beside it aren't one phrase
    // the letterhead only: page 1 above the first transaction (a merchant's name or web address in a
    // transaction line is no fingerprint)
    let page1_cells: String = rows
        .iter()
        .filter(|r| r.page == 1)
        .take_while(|r| dates::parse_first(&r.text(), &ctx0).is_none() || money_items(&r.words).is_empty() || dates::parse_period(&r.text(), order).is_some())
        .map(|r| r.cells().into_iter().map(|c| c.2).collect::<Vec<_>>().join("  "))
        .collect::<Vec<_>>()
        .join("\n");
    diag.pass1.identifiers = profiles::bank_identifiers(&page1_cells);
    diag.pass1.profile = profile.map(|p| p.slug.clone());
    diag.pass1.scores = scores;
    diag.pass1.period = period;
    diag.pass1.account_last4 = account_last4.clone();
    diag.pass1.account_type = Some(account_type);
    diag.pass1.currency = Some(currency);
    diag.pass1.currency_columns = currency_columns.iter().map(|c| c.0).collect();

    Meta {
        profile,
        ctx: DateCtx { order, period, fallback_year: period.map_or(scanner.fallback_year, |(_, e)| e.year()) },
        account_last4,
        account_type,
        currency,
        currency_columns,
        noise,
        fx,
        layout,
        shape: profile.filter(|_| !v.generic).map_or(RowShape::Auto, |p| p.layout.row_shape),
        labels: profile.filter(|_| !v.generic).map_or([None, None, None, None], |p| {
            let l = &p.layout;
            [label_regex(&l.opening), label_regex(&l.closing), label_regex(&l.debits), label_regex(&l.credits)]
        }),
        table_end: profile.and_then(|p| label_anywhere(&p.layout.closing)),
        generic: v.generic,
    }
}

// ---------------------------------------------------------------- pass 2

#[derive(Debug, Clone)]
pub(crate) struct Geometry {
    /// Text columns by centre: (role, lo, hi).
    text: Vec<(ColumnRole, f64, f64)>,
    /// Money columns by right edge: (role, lo, hi).
    money: Vec<(ColumnRole, f64, f64)>,
    /// Everything at or right of this is the amounts zone.
    money_lo: f64,
    left: f64,
    right: f64,
}

impl Geometry {
    fn text_role(&self, x: f64) -> Option<ColumnRole> {
        if x < self.left || x > self.right {
            return None;
        }
        if x >= self.money_lo {
            return Some(ColumnRole::Description);
        }
        self.text.iter().find(|(_, lo, hi)| x >= *lo && x < *hi).map(|c| c.0)
    }
    fn money_role(&self, right_edge: f64) -> Option<ColumnRole> {
        self.money_band(right_edge).map(|k| self.money[k].0)
    }
    /// Which money column (by position) an amount ending at `right_edge` sits in.
    fn money_band(&self, right_edge: f64) -> Option<usize> {
        if right_edge < self.money_lo || right_edge > self.right {
            return None;
        }
        self.money.iter().position(|(_, lo, hi)| right_edge > *lo && right_edge <= *hi)
    }
    fn bands(&self) -> Vec<ColumnBand> {
        self.text.iter().chain(&self.money).map(|&(role, x0, x1)| ColumnBand { role, x0: x0.max(self.left), x1: x1.min(self.right) }).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RowKind {
    Noise,
    Header,
    Outside,
    Table(usize),
}

pub(crate) struct Layout {
    pub kinds: Vec<RowKind>,
    pub geos: Vec<Geometry>,
}

pub(crate) fn classify_header(text: &str, profile: Option<&BankProfile>) -> Option<ColumnRole> {
    let t = text.to_lowercase();
    if let Some(p) = profile {
        let a = &p.header_anchors;
        for (list, role) in [
            (&a.posting_date, ColumnRole::PostDate),
            (&a.date, ColumnRole::Date),
            (&a.amount, ColumnRole::Amount),
            (&a.debit, ColumnRole::Debit),
            (&a.credit, ColumnRole::Credit),
            (&a.balance, ColumnRole::Balance),
            (&a.description, ColumnRole::Description),
            (&a.reference, ColumnRole::Reference),
        ] {
            if list.iter().any(|k| t == k.to_lowercase()) {
                return Some(role);
            }
        }
    }
    let has = |ks: &[&str]| ks.iter().any(|k| t.contains(k));
    if t.len() > 40 {
        return None;
    }
    if has(&["balance", "solde", "saldo"]) {
        Some(ColumnRole::Balance)
    } else if has(&["posting date", "post date", "posted"]) {
        Some(ColumnRole::PostDate)
    } else if t == "date" || has(&["date", "fecha", "datum"]) {
        Some(ColumnRole::Date)
    } else if has(&["debit/credit", "amount", "montant", "importe", "bedrag", "betrag"]) {
        Some(ColumnRole::Amount)
    } else if has(&["withdrawal", "debit", "débit", "cargo", "retiro", "minus", "money out", "paid out", "fee", "charges", "frais", "comisión"]) {
        Some(ColumnRole::Debit)
    } else if has(&["deposit", "credit", "crédit", "abono", "depósito", "plus", "money in", "paid in"]) {
        Some(ColumnRole::Credit)
    } else if has(&["description", "particulars", "detail", "narrative", "payee", "transaction/reference", "libellé", "libelle", "concepto", "descripción", "détail", "omschrijving", "verwendungszweck"]) {
        Some(ColumnRole::Description)
    } else if has(&["reference", "ref no", "ref.", "cheque no", "check no"]) {
        Some(ColumnRole::Reference)
    } else {
        None
    }
}

/// A table header starting at `rows[i]` (possibly spanning the next two rows): its columns and last row.
type Columns = Vec<(ColumnRole, f64, f64)>;
/// Columns with the header words that named them.
type NamedColumns = Vec<(ColumnRole, f64, f64, String)>;

fn detect_header(rows: &[Row], i: usize, kinds: &[RowKind], profile: Option<&BankProfile>) -> Option<(NamedColumns, usize)> {
    let r = &rows[i];
    let first = r.cells();
    if first.iter().filter(|c| classify_header(&c.2, profile).is_some()).count() < 2 || !money_items(&r.words).is_empty() {
        return None;
    }
    let mut cells: Vec<(f64, f64, String)> = first;
    let mut last = i;
    for j in i + 1..(i + 5).min(rows.len()) {
        let n = &rows[j];
        if n.page != r.page || kinds[j] == RowKind::Noise || n.yc() - rows[last].yc() > 2.6 * r.h() {
            break;
        }
        let nc = n.cells();
        // margin furniture (vertical lettering, sort codes) between header lines
        if nc.iter().all(|c| c.2.len() <= 3 && classify_header(&c.2, profile).is_none()) {
            continue;
        }
        if !money_items(&n.words).is_empty() || nc.iter().any(|c| classify_header(&c.2, profile).is_none() && c.2.len() > 2) {
            break;
        }
        // merge into an overlapping role-less cell above ("Transaction" + "Date"), else add as a sub-column
        for c in nc {
            let role = classify_header(&c.2, profile);
            match cells.iter_mut().find(|u| u.0 < c.1 + 2.0 && c.0 < u.1 + 2.0 && (classify_header(&u.2, profile).is_none() || role.is_none())) {
                Some(u) => {
                    u.0 = u.0.min(c.0);
                    u.1 = u.1.max(c.1);
                    u.2 = format!("{} {}", u.2, c.2);
                }
                None => cells.push(c),
            }
        }
        last = j;
    }
    let mut cols: NamedColumns = cells.iter().filter_map(|(a, b, t)| Some((classify_header(t, profile)?, *a, *b, t.clone()))).collect();
    let roles: Vec<ColumnRole> = cols.iter().map(|c| c.0).collect();
    if roles.contains(&ColumnRole::Debit) && roles.contains(&ColumnRole::Credit) {
        cols.retain(|c| c.0 != ColumnRole::Amount);
    }
    // two "date" columns: the second one is the posting date
    if !roles.contains(&ColumnRole::PostDate) && roles.iter().filter(|r| **r == ColumnRole::Date).count() == 2 {
        if let Some(c) = cols.iter_mut().filter(|c| c.0 == ColumnRole::Date).nth(1) {
            c.0 = ColumnRole::PostDate;
        }
    }
    let roles: Vec<ColumnRole> = cols.iter().map(|c| c.0).collect();
    let has_date = roles.contains(&ColumnRole::Date) || roles.contains(&ColumnRole::PostDate);
    let has_money = roles.iter().any(|r| matches!(r, ColumnRole::Debit | ColumnRole::Credit | ColumnRole::Amount));
    (has_date && has_money && roles.contains(&ColumnRole::Description)).then(|| {
        cols.sort_by(|a, b| a.1.total_cmp(&b.1));
        (cols, last)
    })
}

/// A detected header's columns without their words.
fn bare(cols: &[(ColumnRole, f64, f64, String)]) -> Columns {
    cols.iter().map(|c| (c.0, c.1, c.2)).collect()
}

fn initial_geometry(cols: &[(ColumnRole, f64, f64)]) -> Geometry {
    let text: Vec<_> = cols.iter().filter(|c| !c.0.is_money()).collect();
    let money: Vec<_> = cols.iter().filter(|c| c.0.is_money()).collect();
    let centre = |c: &(ColumnRole, f64, f64)| (c.1 + c.2) / 2.0;
    let desc_centre = text.iter().find(|c| c.0 == ColumnRole::Description).map_or(0.0, |c| centre(c));
    let money_lo = money.first().map_or(f64::MAX, |m| (desc_centre + centre(m)) / 2.0).max(text.last().map_or(0.0, |t| centre(t) + 1.0));
    let mut tg = Vec::new();
    for (k, c) in text.iter().enumerate() {
        let lo = if k == 0 { f64::MIN } else { (centre(text[k - 1]) + centre(c)) / 2.0 };
        let hi = text.get(k + 1).map_or(money_lo, |n| (centre(c) + centre(n)) / 2.0);
        tg.push((c.0, lo, hi));
    }
    let mut mg = Vec::new();
    for (k, c) in money.iter().enumerate() {
        let lo = if k == 0 { money_lo } else { (money[k - 1].2 + c.2) / 2.0 };
        let hi = money.get(k + 1).map_or(f64::MAX, |n| (c.2 + n.2) / 2.0);
        mg.push((c.0, lo, hi));
    }
    let left = cols.first().map_or(0.0, |c| c.1) - 18.0;
    let right = cols.last().map_or(0.0, |c| c.2) + 45.0;
    Geometry { text: tg, money: mg, money_lo, left, right }
}

/// Lock the money columns to where amounts actually sit. Amounts are right-aligned, so each column's
/// right edges pile up on one x; the band becomes that x ± a tolerance, and a number whose right edge
/// is nowhere near it (an FX note like `(154.88 KYD)` inside a description) stays description text.
fn refine(geo: &mut Geometry, rows: &[&Row]) {
    let mut rights: Vec<Vec<f64>> = vec![Vec::new(); geo.money.len()];
    for r in rows {
        for it in money_items(&r.words) {
            if let Some(k) = geo.money.iter().position(|(_, lo, hi)| it.x1 > *lo && it.x1 <= *hi)
                && it.x1 >= geo.money_lo
                && it.x1 <= geo.right
            {
                rights[k].push(it.x1);
            }
        }
    }
    let medians: Vec<Option<f64>> = rights
        .iter_mut()
        .map(|v| {
            v.sort_by(f64::total_cmp);
            (!v.is_empty()).then(|| v[v.len() / 2])
        })
        .collect();
    if medians.iter().any(Option::is_none) {
        return refine_extents(geo, rows);
    }
    let m: Vec<f64> = medians.into_iter().flatten().collect();
    let n = m.len();
    for k in 0..n {
        let left = if k == 0 { 30.0 } else { (m[k] - m[k - 1]) / 2.0 }.min(30.0);
        let right = if k + 1 == n { 30.0 } else { (m[k + 1] - m[k]) / 2.0 }.min(30.0);
        geo.money[k].1 = m[k] - left;
        geo.money[k].2 = m[k] + right;
    }
    let lo = geo.money[0].1;
    let floor = geo.text.last().map_or(f64::MIN, |t| t.1 + 1.0);
    geo.money_lo = lo.max(floor);
    if let Some(t) = geo.text.last_mut() {
        t.2 = geo.money_lo;
    }
    geo.right = geo.money[n - 1].2;
}

/// Fallback when a column never shows an amount: boundaries half-way between neighbours' extremes.
fn refine_extents(geo: &mut Geometry, rows: &[&Row]) {
    let mut edges: Vec<(f64, f64)> = vec![(f64::MAX, f64::MIN); geo.money.len()];
    let mut seen = vec![0usize; geo.money.len()];
    for r in rows {
        for it in money_items(&r.words) {
            if let Some(k) = geo.money.iter().position(|(_, lo, hi)| it.x1 > *lo && it.x1 <= *hi)
                && it.x1 >= geo.money_lo
                && it.x1 <= geo.right
            {
                edges[k].0 = edges[k].0.min(it.x0);
                edges[k].1 = edges[k].1.max(it.x1);
                seen[k] += 1;
            }
        }
    }
    for k in 1..geo.money.len() {
        if seen[k - 1] > 0 && seen[k] > 0 && edges[k - 1].1 < edges[k].1 {
            // boundary between the right edges, but never left of where column k's amounts end minus their width
            let b = ((edges[k - 1].1 + edges[k].1) / 2.0).max(edges[k - 1].1 + 0.5);
            geo.money[k - 1].2 = b;
            geo.money[k].1 = b;
        }
    }
    if seen.first().copied().unwrap_or(0) > 0 {
        let first_x0 = edges[0].0 - 3.0;
        let floor = geo.text.last().map_or(f64::MIN, |t| t.1 + 1.0);
        if first_x0 > floor {
            geo.money_lo = first_x0.min(geo.money_lo.max(first_x0));
            geo.money[0].1 = geo.money_lo;
            if let Some(t) = geo.text.last_mut() {
                t.2 = geo.money_lo;
            }
        }
    }
    if let Some(&(_, hi)) = edges.iter().zip(&seen).rev().find(|(_, s)| **s > 0).map(|(e, _)| e) {
        geo.right = geo.right.max(hi + 6.0);
    }
}

fn pass2(meta: &Meta, rows: &[Row], diag: &mut Diagnostics) -> Layout {
    let mut kinds: Vec<RowKind> = rows.iter().map(|r| if meta.noise.iter().any(|n| n.is_match(&r.text())) { RowKind::Noise } else { RowKind::Outside }).collect();
    let mut geos: Vec<Geometry> = Vec::new();
    let mut current: Option<usize> = None;
    // the table that last ended (at a closing line), for a section that follows it
    let mut ended: Option<usize> = None;
    let mut resume: Option<usize> = None;
    let mut page = 0;
    let mut pages: Vec<PageGeometry> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let r = &rows[i];
        if r.page != page {
            page = r.page;
            pages.push(PageGeometry { page, inherited: current.is_some(), ..Default::default() });
        }
        let pg = pages.last_mut().unwrap();
        if kinds[i] == RowKind::Noise {
            pg.noise_stripped.push(r.text());
            i += 1;
            continue;
        }
        if let Some((cols, last)) = detect_header(rows, i, &kinds, meta.profile.filter(|_| !meta.generic)) {
            if pg.header_words.is_empty() {
                pg.header_words = cols.iter().map(|c| (c.0, c.3.clone())).collect();
            }
            geos.push(initial_geometry(&bare(&cols)));
            current = Some(geos.len() - 1);
            resume = None;
            for k in kinds.iter_mut().take(last + 1).skip(i) {
                *k = RowKind::Header;
            }
            pg.header_y.get_or_insert(r.y0);
            pg.inherited = false;
            i = last + 1;
            continue;
        }
        // another account's section after a closing line (`USD Current Account — 4411`): its table goes on at
        // its first dated line — the section's own summary box (opening, totals) in between stays outside it
        if current.is_none() && ended.is_some() && SECTION_ROW.is_match(&r.text()) && money_items(&r.words).is_empty() {
            resume = ended;
        }
        if current.is_none() && resume.is_some() && leading_date(r, &meta.ctx).is_some() {
            current = resume.take();
        }
        if let Some(g) = current {
            kinds[i] = RowKind::Table(g);
            pg.rows += 1;
            // the table ends with its closing balance; what follows is fee breakdowns and fine print
            if TABLE_END.is_match(&r.text()) || meta.table_end.as_ref().is_some_and(|e| e.is_match(&r.text())) {
                ended = current.take();
            }
        }
        i += 1;
    }
    for (g, geo) in geos.iter_mut().enumerate() {
        let mine: Vec<&Row> = rows.iter().zip(&kinds).filter(|(_, k)| **k == RowKind::Table(g)).map(|(r, _)| r).collect();
        refine(geo, &mine);
    }
    for pg in &mut pages {
        let g = rows.iter().zip(&kinds).filter(|(r, _)| r.page == pg.page).find_map(|(_, k)| if let RowKind::Table(g) = k { Some(*g) } else { None });
        if let Some(g) = g {
            pg.columns = geos[g].bands();
        }
    }
    diag.pages = pages;
    Layout { kinds, geos }
}

// ---------------------------------------------------------------- pass 2, from the amounts

/// Money roles tried for the columns found from the amounts, left to right (the rightmost columns when
/// there are more). Which one is the balance is left to the arithmetic: the assignment that reconciles wins.
fn money_roles(k: usize) -> &'static [&'static [ColumnRole]] {
    use ColumnRole::*;
    match k {
        0 => &[],
        1 => &[&[Amount]],
        2 => &[&[Amount, Balance], &[Debit, Credit], &[Credit, Debit]],
        3 => &[&[Debit, Credit, Balance], &[Credit, Debit, Balance], &[Amount, Balance]],
        // a fee column beside the money out (the balance moves by both)
        _ => &[&[Debit, Debit, Credit, Balance], &[Debit, Credit, Balance], &[Credit, Debit, Balance], &[Amount, Balance]],
    }
}

/// Where a row's leading date (and a second one right after it, the posting date) sits: x0, x1 of each.
type LeadDate = ((f64, f64), Option<(f64, f64)>);

/// A row that opens with a date, after at most two bits of margin lettering (`SS 16JUN …`).
fn leading_date(r: &Row, ctx: &DateCtx) -> Option<LeadDate> {
    let w = &r.words;
    let date_at = |s: usize| -> Option<(usize, (f64, f64))> {
        // a date is at most three words (`Jul 15 2026`): the longest run from `s` that reads as one
        (1..=3.min(w.len().saturating_sub(s))).rev().find_map(|n| {
            let text = w[s..s + n].iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ");
            let found = dates::find_dates(&text);
            let (a, b, _) = found.first()?;
            (*a == 0 && *b == text.len() && dates::parse_first(&text, ctx).is_some()).then(|| (s + n, (w[s].x0, w[s + n - 1].x1)))
        })
    };
    // margin lettering is capitals (`SS`, `AC`): `as of 01/02/26` under a summary label is a summary line, not a row
    let margin = w.iter().take_while(|t| t.text.chars().count() <= 3 && !t.text.chars().any(|c| c.is_ascii_digit() || c.is_lowercase())).count().min(2);
    let (next, first) = (0..=margin).find_map(date_at)?;
    Some((first, date_at(next).map(|d| d.1)))
}

/// Right edges that pile up: (median, count) of each run of edges within 4 pt of each other.
fn clusters(mut xs: Vec<f64>) -> Vec<(f64, usize)> {
    xs.sort_by(f64::total_cmp);
    let mut out: Vec<Vec<f64>> = Vec::new();
    for x in xs {
        match out.last_mut() {
            Some(g) if x - g[g.len() - 1] <= 4.0 => g.push(x),
            _ => out.push(vec![x]),
        }
    }
    out.into_iter().map(|g| (g[g.len() / 2], g.len())).collect()
}

/// Pass 2 without a header: the table is the rows that open with a date and the amounts around them, its
/// money columns are where those amounts' right edges line up, named by `money_roles(k)[n]`. The words
/// over the columns, whatever they say, are kept as the headings (and learned when the read is approved).
fn data_layout(meta: &Meta, rows: &[Row], diag: &mut Diagnostics, n: usize) -> Option<Layout> {
    let noise: Vec<bool> = rows.iter().map(|r| meta.noise.iter().any(|x| x.is_match(&r.text()))).collect();
    let mut dated: Vec<Option<LeadDate>> = rows.iter().zip(&noise).map(|(r, &nz)| if nz { None } else { leading_date(r, &meta.ctx) }).collect();
    // the date column is where most dates start: a memo line opening with a date (`16JUN26 fee`) isn't in it
    let mut starts: Vec<f64> = dated.iter().flatten().map(|d| d.0.0).collect();
    starts.sort_by(f64::total_cmp);
    let col = *starts.get(starts.len() / 2)?;
    for d in &mut dated {
        if d.is_some_and(|d| (d.0.0 - col).abs() > 8.0) {
            *d = None;
        }
    }
    let n_dated = dated.iter().flatten().count();
    if n_dated < 2 {
        return None;
    }
    let dated_in = |i: usize, js: std::ops::Range<usize>| js.into_iter().any(|j| j < rows.len() && rows[j].page == rows[i].page && dated[j].is_some());
    let date_lo = dated.iter().flatten().map(|d| d.0.0).fold(f64::MAX, f64::min);
    let date_hi = dated.iter().flatten().map(|d| d.0.1).fold(f64::MIN, f64::max);
    let seconds: Vec<(f64, f64)> = dated.iter().flatten().filter_map(|d| d.1).collect();
    // a second date column: on enough lines to be a column (a re-listing or a second section may print one date)
    let post_hi = (seconds.len() >= 3 && seconds.len() * 4 >= n_dated).then(|| seconds.iter().map(|d| d.1).fold(f64::MIN, f64::max));
    let text_hi = post_hi.unwrap_or(date_hi);

    let mut items: Vec<(f64, f64)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        // amounts on a dated line or the few lines after it (not a summary box above the table)
        if !noise[i] && dated_in(i, i.saturating_sub(4)..i + 1) {
            items.extend(money_items(&r.words).iter().filter(|m| m.x0 > text_hi).map(|m| (m.x0, m.x1)));
        }
    }
    // ponytail: two amounts make a column (a month with two deposits); a stray pair is outvoted by the arithmetic
    let found: Vec<(f64, usize)> = clusters(items.iter().map(|m| m.1).collect()).into_iter().filter(|c| c.1 >= 2).collect();
    // the last assignment is a lone amount column, the one most amounts are in (a card with no balances,
    // whose page also has a few amounts lined up elsewhere)
    let (roles, cols): (&[ColumnRole], Vec<f64>) = match money_roles(found.len()).get(n) {
        Some(roles) => (roles, found[found.len() - roles.len()..].iter().map(|c| c.0).collect()),
        None if n == money_roles(found.len()).len() && !found.is_empty() => (&[ColumnRole::Amount], vec![found.iter().max_by_key(|c| c.1)?.0]),
        None => return None,
    };

    let k = cols.len();
    let mut money = Vec::new();
    for (i, &m) in cols.iter().enumerate() {
        let left = if i == 0 { 30.0 } else { (m - cols[i - 1]) / 2.0 }.min(30.0);
        let right = if i + 1 == k { 30.0 } else { (cols[i + 1] - m) / 2.0 }.min(30.0);
        money.push((roles[i], m - left, m + right));
    }
    // the amounts zone starts where the first money column's amounts start
    let first_x0 = items.iter().filter(|it| (it.1 - cols[0]).abs() <= 4.0).map(|it| it.0).fold(f64::MAX, f64::min);
    let money_lo = (first_x0 - 2.0).min(money[0].1).max(text_hi + 2.0);
    money[0].1 = money_lo;
    let mut text = vec![(ColumnRole::Date, f64::MIN, date_hi + 1.0)];
    if let Some(ph) = post_hi {
        text.push((ColumnRole::PostDate, date_hi + 1.0, ph + 1.0));
    }
    text.push((ColumnRole::Description, text_hi + 1.0, money_lo));
    let right = money[k - 1].2;
    let geo = Geometry { text, money, money_lo, left: date_lo - 18.0, right };

    let in_money = |r: &Row| money_items(&r.words).iter().any(|m| geo.money_role(m.x1).is_some());
    let only_balance = |r: &Row| money_items(&r.words).iter().all(|m| geo.money_role(m.x1) == Some(ColumnRole::Balance));
    let mut kinds: Vec<RowKind> = noise.iter().map(|&nz| if nz { RowKind::Noise } else { RowKind::Outside }).collect();
    let mut pages: Vec<PageGeometry> = Vec::new();
    let mut page_nums: Vec<u32> = rows.iter().map(|r| r.page).collect();
    page_nums.dedup();
    for page in page_nums {
        let on: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].page == page).collect();
        // a line with amounts belongs to the table when it follows a dated line (a description running on to
        // its amount), or, above the first one, when its only amount is a balance (`OPENING BALANCE`)
        let tableish: Vec<usize> = on
            .iter()
            .copied()
            .filter(|&i| !noise[i] && (dated[i].is_some() || (in_money(&rows[i]) && (dated_in(i, i.saturating_sub(4)..i) || (only_balance(&rows[i]) && dated_in(i, i + 1..i + 5))))))
            .collect();
        let mut pg = PageGeometry { page, ..Default::default() };
        if let (Some(&a), Some(&b)) = (tableish.first(), tableish.last()) {
            let mut end = b;
            // description lines under the last transaction
            while end + 1 < rows.len() && rows[end + 1].page == page && !noise[end + 1] && money_items(&rows[end + 1].words).is_empty() && rows[end + 1].yc() - rows[end].yc() < 2.6 * rows[end].h() {
                end += 1;
            }
            for i in a..=end {
                if !noise[i] {
                    kinds[i] = RowKind::Table(0);
                    pg.rows += 1;
                }
            }
            pg.columns = geo.bands();
            pg.header_words = headings_over(rows, a, &noise, &geo, date_lo);
            pg.header_y = pg.header_words.first().map(|_| rows[a].y0);
        }
        pages.push(pg);
    }
    diag.pages = pages;
    Some(Layout { kinds, geos: vec![geo] })
}

/// The words printed over the columns, up to three lines above the table's first row: the line nearest
/// the table names a column first (a sub-heading `CREDIT` beats the `Amount` spanning it).
fn headings_over(rows: &[Row], first: usize, noise: &[bool], geo: &Geometry, date_lo: f64) -> Vec<(ColumnRole, String)> {
    let mut out: Vec<(ColumnRole, String)> = Vec::new();
    let mut below = first;
    for i in (first.saturating_sub(3)..first).rev() {
        let (r, b) = (&rows[i], &rows[below]);
        if r.page != b.page || noise[i] || !money_items(&r.words).is_empty() || b.yc() - r.yc() > 3.0 * r.h() {
            break;
        }
        for (x0, x1, text) in r.cells() {
            if x1 < date_lo - 1.0 {
                continue; // margin lettering
            }
            // money headings line up with their amounts on the right, text headings on the left
            let role = geo.money.iter().find(|(_, lo, hi)| x1 > *lo && x1 <= *hi).map(|m| m.0).or_else(|| (x0 < geo.money_lo).then(|| geo.text_role(x0 + 1.0)).flatten());
            if let Some(role) = role
                && !out.iter().any(|o| o.0 == role)
            {
                out.push((role, text.to_lowercase()));
            }
        }
        below = i;
    }
    out
}

/// What this read found out about the layout (headings and labels are filled in passes 2 and 3).
fn observe(meta: &Meta, rows: &[Row], sections: &[Section], diag: &Diagnostics) -> ObservedLayout {
    let mut o = diag.observed.clone();
    if let Some(pg) = diag.pages.iter().find(|p| !p.header_words.is_empty()) {
        o.headings = pg.header_words.iter().map(|(r, w)| (*r, w.to_lowercase())).collect();
        o.money_columns = pg.columns.iter().filter(|c| c.role.is_money()).map(|c| c.role).collect();
    }
    let p3 = &diag.pass3;
    o.row_shape = if p3.joined_rows > 0 {
        RowShape::AmountLast
    } else if p3.stitched_lines > 0 {
        RowShape::AmountFirst
    } else {
        RowShape::Auto
    };
    o.month_first = meta.ctx.order == Order::MonthFirst;
    // balance lines inside the table (`BALANCE BROUGHT FORWARD   1,000.00`): their words are the label
    let label_of = |c: &(u32, f64, Decimal)| {
        let r = rows.iter().find(|r| r.page == c.0 && r.yc() == c.1)?;
        let left = diag.pages.iter().find(|p| p.page == r.page).and_then(|p| p.columns.first()).map_or(f64::MIN, |b| b.x0);
        let words: Vec<String> = r.words.iter().filter(|w| w.x0 >= left - 2.0 && w.text.len() >= 3 && w.text.chars().all(char::is_alphabetic) && Currency::parse(&w.text).is_none()).map(|w| w.text.to_lowercase()).collect();
        (!words.is_empty()).then(|| words.join(" "))
    };
    if let Some(s) = sections.first() {
        let before = |c: &&(u32, f64, Decimal)| s.transactions.first().is_none_or(|t| (c.0, c.1) < (t.page, t.y));
        let after = |c: &&(u32, f64, Decimal)| s.transactions.last().is_none_or(|t| (c.0, c.1) > (t.page, t.y));
        if o.opening.is_none() {
            o.opening = s.checkpoints.iter().find(before).and_then(label_of);
        }
        if o.closing.is_none() {
            o.closing = s.checkpoints.iter().rev().find(after).and_then(label_of);
        }
    }
    o
}

// ---------------------------------------------------------------- pass 3

#[derive(Debug, Default)]
struct Parts {
    date: String,
    post: String,
    reference: String,
    desc: String,
    monies: Vec<(ColumnRole, Printed)>,
    /// Amounts that sat in no column (a drifted or misaligned print).
    loose: Vec<Printed>,
    /// Any word outside the table's columns (left/right margins).
    outside: bool,
    /// Where the description starts on the line (its first word's left edge).
    desc_x0: Option<f64>,
}

fn split_row(r: &Row, g: &Geometry) -> Parts {
    let items = money_items(&r.words);
    let mut p = Parts::default();
    let mut in_money = vec![false; r.words.len()];
    // one amount per column; two columns may share a role (a payment and its fee, both money out)
    let mut used: Vec<usize> = Vec::new();
    for it in items.iter().rev() {
        if let Some(k) = g.money_band(it.x1)
            && !used.contains(&k)
        {
            used.push(k);
            p.monies.push((g.money[k].0, it.printed));
            in_money[it.lo..it.hi].iter_mut().for_each(|m| *m = true);
        } else {
            p.loose.push(it.printed);
        }
    }
    let push = |s: &mut String, w: &str| {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(w);
    };
    for (w, used) in r.words.iter().zip(&in_money) {
        // a dash standing in for an empty amount (`-`, `–`) is no description
        if *used || (w.x0 >= g.money_lo && matches!(w.text.as_str(), "-" | "–" | "—")) {
            continue;
        }
        match g.text_role(w.xc()) {
            Some(ColumnRole::Date) => push(&mut p.date, &w.text),
            Some(ColumnRole::PostDate) => push(&mut p.post, &w.text),
            Some(ColumnRole::Reference) => push(&mut p.reference, &w.text),
            Some(_) => {
                p.desc_x0.get_or_insert(w.x0);
                push(&mut p.desc, &w.text)
            }
            None => p.outside |= w.text.len() > 3,
        }
    }
    // a description that starts left of its centred heading spills into the date column: give it back
    if let Some(&(_, end, _)) = dates::find_dates(&p.date).first() {
        let rest = p.date[end..].trim().to_string();
        if !rest.is_empty() && dates::find_dates(&rest).is_empty() {
            p.date.truncate(end);
            p.desc = if p.desc.is_empty() { rest } else { format!("{rest} {}", p.desc) };
        }
    }
    // a stray letter or two in the date column is margin lettering, not a date
    for s in [&mut p.date, &mut p.post] {
        if s.len() <= 3 && !s.bytes().any(|b| b.is_ascii_digit()) {
            s.clear();
        }
    }
    p
}

static GENERIC_FX: LazyLock<Regex> = LazyLock::new(|| Regex::new(profiles::GENERIC_FX).unwrap());
static DIGITS4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)[0-9]{4,}(?-u:\b)").unwrap());
// (not "balance carried forward": that's printed at the foot of a page, and the table carries on overleaf)
static TABLE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:closing|ending)\s+balance\b|\bnouveau\s+solde\b|\bsaldo\s+final\b").unwrap());
static SUMMARY_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:sub-?)?totals?\b|^(?:opening|closing|previous|new|beginning|ending)\s+balance|balance\s+(?:brought|carried)?\s*forward|^brought\s+forward|^carried\s+forward|^nouveau\s+solde|^solde\s+(?:précédent|reporté|final|d'ouverture)|^saldo\s+(?:anterior|inicial|final)").unwrap()
});
static SECTION_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\b(?P<ccy>KYD|USD)\s+transactions?\b|^\W*(?:(?-i:(?P<code>[A-Z]{3}))\s+)?(?P<kind>share\s+savings|shares?|current|chequing|checking|savings|loan)\s+(?:account|statement)\b)").unwrap()
});

/// Sign in statement terms: positive moves the printed balance up.
fn stmt_sign(role: ColumnRole, p: &Printed, liability: bool, layout: LayoutKind) -> (Decimal, bool) {
    let m = p.magnitude;
    match role {
        ColumnRole::Debit => (if liability { m } else { -m }, false),
        ColumnRole::Credit => (if liability { -m } else { m }, false),
        _ => {
            if let Some(cr) = p.credit_marker {
                let up = if liability { !cr } else { cr };
                (if up { m } else { -m }, false)
            } else if let Some(neg) = p.negative {
                (if neg { -m } else { m }, false)
            } else {
                // unsigned in a single amount column: a card charge, or (for deposits) ambiguous
                let flexible = !liability && layout != LayoutKind::SignedSingleAmount;
                (m, flexible || !liability)
            }
        }
    }
}

fn balance_value(p: &Printed, liability: bool) -> Decimal {
    let neg = p.negative == Some(true) || if liability { p.credit_marker == Some(true) } else { p.credit_marker == Some(false) };
    if neg { -p.magnitude } else { p.magnitude }
}

struct Building {
    section: Section,
    /// (page, y, is_anchor, index into txs or into conts)
    events: Vec<Event>,
    conts: Vec<(u32, f64, f64, String)>,
    anchor_desc_empty: Vec<bool>,
    anchor_y: Vec<(u32, f64, f64)>,
    refs: Vec<String>,
    /// Where each transaction's description starts (page, x): the description column's left edge, by example.
    desc_starts: Vec<(u32, f64)>,
}

enum Event {
    Anchor(usize),
    Cont(usize),
}

fn new_section(currency: Currency, account_type: AccountType, last4: Option<String>, label: Option<String>) -> Building {
    Building {
        section: Section {
            currency,
            account_type,
            account_last4: last4,
            account_label: label,
            opening: None,
            closing: None,
            total_debits: None,
            total_credits: None,
            transactions: Vec::new(),
            reconciliation: Reconciliation::Unverified,
            confidence: 0.0,
            report: SectionReport::default(),
            checkpoints: Vec::new(),
        },
        events: Vec::new(),
        conts: Vec::new(),
        anchor_desc_empty: Vec::new(),
        anchor_y: Vec::new(),
        refs: Vec::new(),
        desc_starts: Vec::new(),
    }
}

/// A dated line with no amount yet (CNB): its description runs on below it, the amount comes on the last line.
struct Head {
    page: u32,
    y: f64,
    date: NaiveDate,
    effective: Option<NaiveDate>,
    lines: Vec<String>,
    text: String,
}

fn orphan_head(b: &mut Building, h: Head) {
    b.section.report.skipped.push(SkippedLine { page: h.page, y: h.y, text: h.text, date: Some(h.date), amount: None, reason: "dated line without an amount".into(), balance: None });
}

fn pass3(meta: &Meta, rows: &[Row], layout: &Layout, diag: &mut Diagnostics) -> Vec<Section> {
    let mut built: Vec<(Building, usize)> = vec![(new_section(meta.currency, meta.account_type, meta.account_last4.clone(), None), 0)];
    let mut last_date: Option<(u32, NaiveDate, Option<NaiveDate>)> = None;
    let mut pending: Option<Head> = None;

    for (ri, r) in rows.iter().enumerate() {
        let text = r.text();
        if layout.kinds[ri] == RowKind::Header {
            // a transaction split by a page break carries on under the next page's header
            if let Some(h) = pending.as_mut() {
                (h.page, h.y) = (r.page, r.yc());
            }
            continue;
        }
        if layout.kinds[ri] == RowKind::Noise {
            continue;
        }
        if let Some(c) = SECTION_ROW.captures(&text)
            && money_items(&r.words).is_empty()
        {
            let cur = &built.last().unwrap().0.section;
            let (ccy, ty) = match (c.name("ccy"), c.name("kind")) {
                (Some(ccy), _) => (Currency::parse(ccy.as_str()).unwrap_or(cur.currency), cur.account_type),
                (_, Some(k)) => {
                    let k = k.as_str().to_lowercase();
                    let ty = if k.starts_with("share") {
                        AccountType::ShareAccount
                    } else if k.starts_with("loan") {
                        AccountType::Loan
                    } else if k.starts_with("sav") {
                        AccountType::Savings
                    } else {
                        AccountType::Checking
                    };
                    (c.name("code").and_then(|m| Currency::parse(m.as_str())).unwrap_or(cur.currency), ty)
                }
                _ => (cur.currency, cur.account_type),
            };
            let last4 = DIGITS4.find_iter(&text).last().map(|m| m.as_str()[m.as_str().len() - 4..].to_string()).or(cur.account_last4.clone());
            if (ccy, ty, &last4) != (cur.currency, cur.account_type, &cur.account_last4) {
                let label = Some(text.trim().to_string());
                if built.last().unwrap().0.section.transactions.is_empty() && built.last().unwrap().0.section.checkpoints.is_empty() {
                    built.pop();
                }
                built.push((new_section(ccy, ty, last4, label), ri));
            }
            continue;
        }
        let RowKind::Table(g) = layout.kinds[ri] else { continue };
        let geo = &layout.geos[g];
        let b = &mut built.last_mut().unwrap().0;
        let liability = b.section.account_type.is_liability();
        let p = split_row(r, geo);
        let txn_money: Vec<(ColumnRole, Printed)> = p.monies.iter().filter(|(role, _)| *role != ColumnRole::Balance).copied().collect();
        let bal = p.monies.iter().find(|(role, _)| *role == ColumnRole::Balance).map(|(_, pr)| balance_value(pr, liability));
        let date = dates::parse_first(&p.date, &meta.ctx);
        let post = dates::parse_first(&p.post, &meta.ctx);
        let desc = p.desc.trim().to_string();
        let skip = |b: &mut Building, reason: &str| {
            let amount = txn_money.first().or(p.loose.first().map(|l| (ColumnRole::Amount, *l)).as_ref()).map(|(_, pr)| pr.magnitude).or_else(|| (bal.is_none()).then(|| money::find_amount(&text).map(|pr| pr.magnitude)).flatten());
            // the posting date when both are printed, like every other row
            let d = post.or(date).or_else(|| dates::parse_first(&text, &meta.ctx));
            if amount.is_some() || d.is_some() {
                // a dated line whose amount couldn't be read (`-26` for `.26` by OCR) still says its balance
                let balance = bal.filter(|_| amount.is_none() && d.is_some());
                b.section.report.skipped.push(SkippedLine { page: r.page, y: r.yc(), text: text.clone(), date: d, amount, reason: reason.into(), balance });
            }
        };

        // an undated `Deposits & Other Credits  1,243.92` is a summary total that strayed into the table
        let total_line = date.is_none() && post.is_none() && !money_items(&r.words).is_empty() && [&*DEBITS, &*CREDITS].into_iter().chain(meta.labels[2..].iter().flatten()).any(|l| l.is_match(&desc));
        if total_line {
            // a section's own totals, printed under its table: they check its rows when no balance does
            let cur = b.section.currency;
            if let Some((v, _, _)) = summary_value(&[r], &DEBITS, &[], cur, false) {
                b.section.total_debits = Some(v.abs());
            }
            if let Some((v, _, _)) = summary_value(&[r], &CREDITS, &[], cur, false) {
                b.section.total_credits = Some(v.abs());
            }
        }
        if SUMMARY_ROW.is_match(&desc) || (desc.is_empty() && SUMMARY_ROW.is_match(&text)) || total_line {
            match bal.filter(|_| !total_line) {
                Some(v) => {
                    b.section.checkpoints.push((r.page, r.yc(), v));
                    // `Ending Balance  848.99` in the table: what's listed after it isn't this account's activity
                    if TABLE_END.is_match(&text) {
                        b.section.closing = Some(v);
                    }
                }
                None => skip(b, reconcile::SUMMARY_LINE),
            }
            continue;
        }

        let near = |h: &Head| h.page == r.page && r.yc() - h.y < 2.6 * r.h();
        let undated = date.is_none() && post.is_none() && p.date.trim().is_empty() && p.post.trim().is_empty();

        // an undated `0.00` line is information under a transaction (`Chevron station #0441   0.00`), not one
        if undated && bal.is_none() && !txn_money.is_empty() && txn_money.iter().all(|(_, p)| p.magnitude.is_zero()) {
            if !desc.is_empty() {
                b.conts.push((r.page, r.yc(), r.h(), desc));
                b.events.push(Event::Cont(b.conts.len() - 1));
            }
            continue;
        }
        if !txn_money.is_empty() {
            let head = if undated { pending.take_if(|h| near(h)) } else { None };
            if let Some(h) = pending.take() {
                orphan_head(b, h);
            }
            // the posting date is the ledger date when both are printed
            let (posted, effective) = match (&head, post, date) {
                (Some(h), _, _) => (Some(h.date), h.effective),
                (None, Some(po), d) => (Some(po), d.filter(|d| *d != po)),
                (None, None, d) => (d, None),
            };
            diag.pass3.joined_rows += head.is_some() as usize;
            let desc = match head {
                Some(h) => h.lines.into_iter().chain((!desc.is_empty()).then(|| desc.clone())).collect::<Vec<_>>().join(" "),
                None => desc.clone(),
            };
            let posted = match posted {
                Some(d) => Some(d),
                None => match last_date {
                    Some((pg, d, _)) if pg == r.page && p.date.trim().is_empty() && p.post.trim().is_empty() => {
                        diag.pass3.inherited_dates += 1;
                        Some(d)
                    }
                    _ => None,
                },
            };
            let Some(posted) = posted else {
                skip(b, "amount without a date");
                continue;
            };
            last_date = Some((r.page, posted, effective));
            if dates::find_dates(&format!("{} {}", p.date, p.post)).first().is_some_and(|d| !d.2.has_year()) {
                diag.pass3.yearless_dates += 1;
            }
            // a line with both a debit and a credit is two transactions (netting them would still add up to the
            // totals and hide both); the balance printed on the line follows the second
            let nonzero = |role: ColumnRole| txn_money.iter().any(|(r, pr)| *r == role && !pr.magnitude.is_zero());
            let parts: Vec<Vec<(ColumnRole, Printed)>> = if nonzero(ColumnRole::Debit) && nonzero(ColumnRole::Credit) {
                diag.pass3.debit_and_credit_lines += 1;
                txn_money.iter().map(|m| vec![*m]).collect()
            } else {
                vec![txn_money.clone()]
            };
            let n_parts = parts.len();
            for (k, part) in parts.into_iter().enumerate() {
                let mut stmt = Decimal::ZERO;
                let mut flexible = false;
                for (role, pr) in &part {
                    let (v, f) = stmt_sign(*role, pr, liability, meta.layout);
                    stmt += v;
                    flexible |= f;
                }
                let idx = b.section.transactions.len();
                // the second of a split line sits a hair lower, so the two keep their order
                let y = r.yc() + k as f64 * 0.01;
                b.section.transactions.push(ScannedTx {
                    posted,
                    effective,
                    description: desc.clone(),
                    memo: String::new(),
                    amount: if liability { -stmt } else { stmt },
                    stmt_amount: stmt,
                    balance: if k + 1 == n_parts { bal } else { None },
                    fx_original: None,
                    page: r.page,
                    y,
                    recovered: false,
                    sign_flexible: flexible && part.len() == 1,
                });
                b.anchor_desc_empty.push(desc.is_empty());
                if let Some(x) = p.desc_x0 {
                    b.desc_starts.push((r.page, x));
                }
                b.anchor_y.push((r.page, y, r.h()));
                b.refs.push(p.reference.trim().to_string());
                b.events.push(Event::Anchor(idx));
                diag.pass3.anchors += 1;
            }
            continue;
        }
        if let Some(v) = bal {
            // a dated row with only a balance: an opening line (`07/01 Opening Balance  5,000.00`), or a row
            // that lost its amount (printed outside the columns, or unreadable): its balance counts either way,
            // and Pass 5 turns it into a transaction if the balances say one is missing there
            // (a dated row that does show an amount somewhere is a transaction whose amount strayed: Pass 5
            // places it from the skipped line; its balance isn't a free-standing point)
            let dated = date.is_some() || post.is_some();
            if dated {
                skip(b, "amount outside the locked columns");
            }
            if !dated || p.loose.is_empty() {
                b.section.checkpoints.push((r.page, r.yc(), v));
            }
            continue;
        }
        // a description's next line starts in the description column (or a little indented): a line starting in
        // the margin or centred on the page (a notice, a page footer) belongs to no transaction
        // (judged on this page's own examples first: a page can sit a little to one side of the others)
        let off_column = p.desc_x0.is_some_and(|x| {
            let here: Vec<f64> = b.desc_starts.iter().filter(|s| s.0 == r.page).map(|s| s.1).collect();
            let mut v = if here.is_empty() { b.desc_starts.iter().map(|s| s.1).collect() } else { here };
            v.sort_by(f64::total_cmp);
            !v.is_empty() && (x < v[v.len() / 2] - 2.0 * r.h() || x > v[v.len() / 2] + 12.0 * r.h())
        });
        // (it carries no amount — those were dealt with above — so there's nothing to recover from it)
        if !desc.is_empty() && undated && !p.outside && off_column {
            continue;
        }
        if !desc.is_empty() && undated && !p.outside {
            let mut line = desc;
            if !p.reference.is_empty() {
                line = format!("{line} {}", p.reference.trim());
            }
            if let Some(h) = pending.as_mut().filter(|h| near(h)) {
                h.lines.push(line);
                h.y = r.yc();
                continue;
            }
            b.conts.push((r.page, r.yc(), r.h(), line));
            b.events.push(Event::Cont(b.conts.len() - 1));
            continue;
        }
        if let Some(d) = post.or(date)
            && !desc.is_empty()
            && !p.outside
            && meta.shape != RowShape::AmountFirst
        {
            if let Some(h) = pending.take() {
                orphan_head(b, h);
            }
            pending = Some(Head { page: r.page, y: r.yc(), date: d, effective: post.and(date).filter(|e| Some(*e) != post), lines: vec![desc], text: text.clone() });
            continue;
        }
        skip(b, "unrecognised line in the table");
    }
    if let Some(h) = pending.take() {
        orphan_head(&mut built.last_mut().unwrap().0, h);
    }

    let mut out = Vec::new();
    let outside: Vec<usize> = (0..rows.len()).filter(|&i| layout.kinds[i] == RowKind::Outside).collect();
    let all: Vec<&Row> = outside.iter().map(|&i| &rows[i]).collect();
    let starts: Vec<usize> = built.iter().map(|b| b.1).collect();
    let n_sections = built.len();
    for (k, (b, start_row)) in built.into_iter().enumerate() {
        let mut sec = stitch(b, diag);
        if sec.transactions.is_empty() && n_sections > 1 {
            continue;
        }
        // summary box: this section's own rows first, else the whole document's (picked by currency column)
        let end_row = starts.get(k + 1).copied().unwrap_or(rows.len());
        let own: Vec<&Row> = outside.iter().filter(|&&i| i >= start_row && i < end_row).map(|&i| &rows[i]).collect();
        for (label, slot) in [(&*OPENING, 0), (&*CLOSING, 1), (&*DEBITS, 2), (&*CREDITS, 3)] {
            let find = |label: &Regex| {
                let liability = sec.account_type.is_liability();
                summary_value(&own, label, &meta.currency_columns, sec.currency, liability).or_else(|| {
                    (n_sections == 1 || !meta.currency_columns.is_empty()).then(|| summary_value(&all, label, &meta.currency_columns, sec.currency, liability)).flatten()
                })
            };
            // the bank's declared label first; the generic words are the fallback, and using them is noted
            let declared = meta.labels[slot].as_ref();
            let found = declared.and_then(find).or_else(|| {
                let v = if slot == 1 { find(&CLOSING_PLAIN).or_else(|| find(label)) } else { find(label) };
                if declared.is_some() && v.is_some() && slot < 2 {
                    let what = if slot == 0 { "opening" } else { "closing" };
                    diag.pass3.layout_notes.push(format!("The {what} balance wasn't under any of this bank's own labels; a generic one was used"));
                }
                v
            });
            if let Some((v, line, printed)) = found {
                diag.pass1.summary_lines.push(line);
                match slot {
                    0 => diag.observed.opening = Some(printed),
                    1 => diag.observed.closing = Some(printed),
                    _ => {}
                }
                match slot {
                    0 => sec.opening = Some(v),
                    1 => sec.closing = Some(v),
                    2 => sec.total_debits = Some(v.abs()),
                    _ => sec.total_credits = Some(v.abs()),
                }
            }
        }
        for fx in meta.fx.iter().chain(std::iter::once(&*GENERIC_FX)) {
            for t in sec.transactions.iter_mut().filter(|t| t.fx_original.is_none()) {
                let hay = format!("{} {}", t.description, t.memo);
                if let Some(c) = fx.captures(&hay)
                    && let (Some(ccy), Some(amt)) = (c.name("ccy").and_then(|m| Currency::parse(m.as_str())), c.name("amt"))
                    && let Some(a) = money::parse_user(amt.as_str())
                    && ccy != sec.currency
                {
                    t.fx_original = Some((ccy, a.abs()));
                }
            }
        }
        out.push(sec);
    }
    diag.pass3.sections = out.len();
    out
}

/// Pass 3b: give each description-only line to its transaction. Lines go to the transaction above,
/// except when the transaction below printed no description on its own row (a vertically centred
/// multi-line description) and is nearer.
fn stitch(mut b: Building, diag: &mut Diagnostics) -> Section {
    let mut memo: Vec<Vec<String>> = vec![Vec::new(); b.section.transactions.len()];
    let mut prev: Option<usize> = None;
    let mut last_y: Option<(u32, f64)> = None;
    for (ei, e) in b.events.iter().enumerate() {
        match e {
            Event::Anchor(i) => {
                prev = Some(*i);
                last_y = Some((b.anchor_y[*i].0, b.anchor_y[*i].1));
            }
            Event::Cont(c) => {
                let (page, y, h, ref line) = b.conts[*c];
                let next = b.events[ei + 1..].iter().find_map(|e| if let Event::Anchor(i) = e { Some(*i) } else { None }).filter(|&i| b.anchor_y[i].0 == page);
                let up = prev.filter(|&i| b.anchor_y[i].0 == page && last_y.is_some_and(|(p, ly)| p == page && y - ly < 2.6 * h));
                let dist = |i: usize| (b.anchor_y[i].1 - y).abs();
                let target = match (up, next) {
                    (Some(u), Some(n)) if b.anchor_desc_empty[n] && dist(n) < dist(u) && dist(n) < 2.6 * h => Some(n),
                    (None, Some(n)) if b.anchor_desc_empty[n] && dist(n) < 2.6 * h => Some(n),
                    (Some(u), _) => Some(u),
                    _ => None,
                };
                match target {
                    Some(t) => {
                        memo[t].push(line.clone());
                        diag.pass3.stitched_lines += 1;
                        if Some(t) == prev {
                            last_y = Some((page, y));
                        }
                    }
                    None => b.section.report.skipped.push(SkippedLine { page, y, text: line.clone(), date: None, amount: None, reason: "orphan description line".into(), balance: None }),
                }
            }
        }
    }
    for (i, t) in b.section.transactions.iter_mut().enumerate() {
        let mut lines = std::mem::take(&mut memo[i]);
        // keep the vertical order of the lines
        if t.description.is_empty() {
            t.description = lines.join(" ");
            lines.clear();
        }
        if !b.refs[i].is_empty() {
            lines.push(format!("Ref {}", b.refs[i]));
        }
        t.memo = lines.join(" | ");
        if t.description.is_empty() {
            t.description = "(no description)".into();
        }
    }
    b.section
}
