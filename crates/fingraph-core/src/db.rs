//! The ledger: SQLite in WAL mode at `~/.local/share/fingraph/ledger.db`. Money is stored twice,
//! as the exact decimal string and as integer minor units, always next to its ISO currency.

use chrono::{Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::str::FromStr;

use crate::money::{to_minor, Fx};
use crate::prefs::Prefs;
use crate::parser::profiles::{self, BankProfile, HeaderAnchors, LayoutKind, LayoutRules};
use crate::parser::{Diagnostics, ScanResult, Section, SectionReport};
use crate::rules::{self, Rule, RuleKind};
use crate::{AccountType, Currency, EntityKind, Error, Reconciliation, Result, ReviewStatus};

mod schema;
use schema::{MIGRATIONS, CATEGORIES};

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub kind: EntityKind,
    pub base_currency: Currency,
    pub icon_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: i64,
    pub entity_id: i64,
    pub bank_profile_id: Option<i64>,
    pub bank_short: String,
    pub name: String,
    pub account_number_masked: String,
    pub account_type: AccountType,
    pub currency: Currency,
    pub ofx_bank_id: String,
    pub ofx_intu_bid: String,
    /// Closed: left out of account lists and the balance chart.
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryKind {
    Income,
    Expense,
    Transfer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub kind: CategoryKind,
    /// Your own choice of icon and colour; see `looks` for the defaults.
    pub icon: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub id: i64,
    pub account_id: i64,
    pub account_name: String,
    pub entity_id: i64,
    pub bank_short: String,
    pub filename: String,
    pub period: Option<(NaiveDate, NaiveDate)>,
    pub opening: Option<Decimal>,
    pub closing: Option<Decimal>,
    pub total_debits: Option<Decimal>,
    pub total_credits: Option<Decimal>,
    pub extracted_sum: Decimal,
    pub currency: Currency,
    pub reconciliation: Reconciliation,
    pub confidence: f64,
    pub tx_count: i64,
    pub imported_at: String,
    /// Waiting for review: why (empty once approved).
    pub review: Vec<Review>,
}

/// One ledger row with everything the views need joined in.
#[derive(Debug, Clone, PartialEq)]
pub struct TxRow {
    pub id: i64,
    /// The workspace the row counts in: its account's, or the business it was flagged for.
    pub document_id: Option<i64>,
    pub account_id: i64,
    pub account_name: String,
    pub account_type: AccountType,
    pub bank_short: String,
    pub entity_id: i64,
    pub entity_currency: Currency,
    pub posted: NaiveDate,
    pub effective: Option<NaiveDate>,
    pub raw_description: String,
    pub clean_payee: String,
    pub amount: Decimal,
    pub currency: Currency,
    pub amount_entity: Option<Decimal>,
    pub balance: Option<Decimal>,
    pub category_id: Option<i64>,
    pub category_name: Option<String>,
    pub category_kind: Option<CategoryKind>,
    pub tag: String,
    pub memo: String,
    pub fitid: String,
    pub is_recurring: bool,
    pub is_subscription: bool,
    pub transfer_peer: Option<i64>,
    pub review: ReviewStatus,
    /// Flagged as a business expense of this workspace (paid from another workspace's account).
    pub business_entity_id: Option<i64>,
    /// The workspace of the account it was paid from.
    pub account_entity_id: i64,
    /// How many split lines the statement row has (0 = not split).
    pub splits: i64,
    /// Set on a reporting row that is one line of a split (see `TxFilter::split_lines`).
    pub split_seq: Option<i64>,
    pub attachments: i64,
}

impl TxRow {
    pub fn is_transfer(&self) -> bool {
        self.transfer_peer.is_some() || self.category_kind == Some(CategoryKind::Transfer)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TxFilter {
    /// Empty = all workspaces.
    pub entity_ids: Vec<i64>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub account_id: Option<i64>,
    pub category_id: Option<i64>,
    pub uncategorized_only: bool,
    pub search: String,
    /// For reports: a split row becomes its lines, each in its own category and workspace.
    pub split_lines: bool,
    /// Pick rows by their account's own workspace, ignoring business flags (balances, forecasts, exports).
    pub by_account: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub document_ids: Vec<i64>,
    pub inserted: usize,
    pub duplicates: usize,
    pub already_imported: bool,
    pub auto_categorized: usize,
    /// Sections waiting for review (see [`Review`]).
    pub held: usize,
}

/// Where one section of a scanned statement goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Existing(i64),
    New,
    Skip,
}

/// One section of a statement, before it's imported.
#[derive(Debug, Clone)]
pub struct SectionPlan {
    /// The known account with the same bank, last 4 digits and currency.
    pub matched: Option<i64>,
    /// What a new account would be called (`CNB Savings KYD ••1234`).
    pub new_name: String,
    /// The statement printed an account number (last 4 digits) the scanner could read.
    pub number_read: bool,
    /// No account number could be read: the same bank's accounts in this currency, to pick from.
    pub candidates: Vec<(i64, String)>,
}

/// What importing a scanned statement would do, so anything needing a person can be asked first.
#[derive(Debug, Clone, Default)]
pub struct ImportPlan {
    pub sections: Vec<SectionPlan>,
    pub already_imported: bool,
    /// Statements already in the ledger for the same account and period (another copy or another read).
    pub same_period: Vec<(i64, String)>,
    /// Neither a transaction nor a balance could be read (a stray summary page, say).
    pub no_transactions: bool,
}

/// A quiet month (no transactions, balances printed) is still a statement; this is neither.
fn nothing_read(scan: &ScanResult) -> bool {
    scan.sections.iter().all(|s| s.transactions.is_empty() && s.opening.is_none() && s.closing.is_none()) || undated_only(scan)
}

/// No transactions, but lines with amounts and no date: a blank template (`mm/dd/yyyy`), not a statement.
fn undated_only(scan: &ScanResult) -> bool {
    scan.sections.iter().all(|s| s.transactions.is_empty()) && scan.sections.iter().flat_map(|s| &s.report.skipped).any(|l| l.amount.is_some() && l.date.is_none() && l.reason != crate::parser::reconcile::SUMMARY_LINE)
}

fn nothing_read_reason(scan: &ScanResult) -> &'static str {
    if undated_only(scan) {
        "its transaction lines have amounts but no dates FinGraph can read (a blank template?)"
    } else {
        "no transactions or balances were found in this statement"
    }
}

impl ImportPlan {
    /// Every section belongs to a known account and nothing overlaps: import without asking.
    pub fn automatic(&self) -> Option<Vec<Target>> {
        if self.no_transactions || self.already_imported || !self.same_period.is_empty() {
            return None;
        }
        self.sections.iter().map(|s| s.matched.map(Target::Existing)).collect()
    }
    /// Known accounts where they match, new ones otherwise (demo data, tests, the terminal).
    pub fn default_targets(&self) -> Vec<Target> {
        self.sections.iter().map(|s| s.matched.map_or(Target::New, Target::Existing)).collect()
    }
}

/// Why a statement waits for review before its transactions count anywhere. Stored as codes.
/// A floating currency in use, and its latest kept rate's day and source (`None`: none yet).
pub type RateStatus = (Currency, Option<(NaiveDate, String)>);

/// What approving a held statement did besides letting it count.
#[derive(Debug, Clone, Default)]
pub struct Approval {
    /// The bank whose saved layout learned from it (short name).
    pub learned: Option<String>,
    /// Other held statements from that bank read again with what was learned.
    pub rereads: usize,
    /// The anonymised layout sample kept of it.
    pub sample: Option<std::path::PathBuf>,
    /// A bank nobody had set up got its profile from this statement (short name).
    pub created: Option<String>,
    /// A bank nobody had set up, whose statement doesn't say who it is on page 1: no profile was made.
    pub unnamed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    /// The rows don't add up to the printed balances.
    Mismatch,
    /// No balances printed to check against.
    NoBalances,
    /// Read by OCR: names may have typos.
    Ocr,
    /// The first statement of this kind from this bank: the layout hasn't been confirmed on a real one.
    NewLayout,
    /// It departs from the bank's declared layout: it can add up and still have wrong names or dates.
    OffLayout,
    /// It looks different from the account's previous statement: the bank changed its layout.
    LayoutChanged,
    /// An amount couldn't be read and was worked out from the balances around it.
    Derived,
    /// It doesn't start where the account's previous statement ended: a statement or rows may be missing.
    Continuity,
}

impl Review {
    pub const ALL: [Review; 8] = [Review::Mismatch, Review::NoBalances, Review::Ocr, Review::NewLayout, Review::OffLayout, Review::LayoutChanged, Review::Derived, Review::Continuity];
    pub fn code(self) -> &'static str {
        match self {
            Review::Mismatch => "mismatch",
            Review::NoBalances => "nobalances",
            Review::Ocr => "ocr",
            Review::NewLayout => "newlayout",
            Review::OffLayout => "offlayout",
            Review::LayoutChanged => "layoutchanged",
            Review::Derived => "derived",
            Review::Continuity => "continuity",
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            Review::Mismatch => "the rows don't add up to the statement's balances",
            Review::NoBalances => "there are no balances to check the rows against",
            Review::Ocr => "it was read by OCR, so names may have typos",
            Review::NewLayout => "it's the first statement of this kind from this bank: check the dates, amounts and descriptions once",
            Review::OffLayout => "it doesn't match this bank's declared layout (see the notes in its Scan Report), so names or dates may be off",
            Review::LayoutChanged => "the bank changed how its statements look since this account's last one (see its Scan Report): check it once, and approving it teaches FinGraph the new layout",
            Review::Derived => "an amount on it couldn't be read, so it was worked out from the balances before and after it (see Recovered in its Scan Report): check that line against the PDF",
            Review::Continuity => "it doesn't start where this account's previous statement ended (see its Scan Report): a statement, or some of its rows, may be missing",
        }
    }
    pub fn parse_list(s: &str) -> Vec<Review> {
        Review::ALL.into_iter().filter(|r| s.split(',').any(|c| c == r.code())).collect()
    }
    pub fn join(v: &[Review]) -> String {
        v.iter().map(|r| r.code()).collect::<Vec<_>>().join(",")
    }
}

pub struct Db {
    pub conn: Connection,
}

/// A transaction that nests: a SAVEPOINT, so one inside another (a rescan's import inside the rescan)
/// commits or rolls back with the outer one. Dropped without `commit` = rolled back.
pub struct Sp<'a> {
    conn: &'a Connection,
    done: bool,
}

impl Sp<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.conn.execute_batch("RELEASE fg")?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Sp<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK TO fg; RELEASE fg");
        }
    }
}

impl std::ops::Deref for Sp<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
    }
}

pub(crate) fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or_default()
}
fn opt_dec(s: Option<String>) -> Option<Decimal> {
    s.and_then(|s| Decimal::from_str(&s).ok())
}
pub(crate) fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}
pub(crate) fn ccy(s: &str) -> Currency {
    Currency::parse(s).unwrap_or(Currency::KYD)
}
pub(crate) fn cat_kind(s: &str) -> CategoryKind {
    match s {
        "Income" => CategoryKind::Income,
        "Transfer" => CategoryKind::Transfer,
        _ => CategoryKind::Expense,
    }
}
pub(crate) fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Deterministic SHA-256 FITID over account, date, amount, normalised description and the row's
/// position among identical rows that day (two identical coffees are two transactions).
/// A transaction's fingerprint: account, date, amount and which of that day's same-amount rows it is.
/// Not the description, so the same statement read twice (a re-download, an OCR copy, a rescan that
/// fixed a misread name) can't add its rows a second time.
pub fn fitid(account_id: i64, posted: NaiveDate, amount: Decimal, seq: usize) -> String {
    let mut h = Sha256::new();
    h.update(format!("v2|{account_id}|{posted}|{}|{seq}", crate::money::round2(amount)).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut h = Sha256::new();
    h.update(std::fs::read(path)?);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

const TX_SELECT: &str = "SELECT t.id, t.document_id, t.account_id, a.name, a.account_type, COALESCE(bp.short_name, ''), COALESCE(t.business_entity_id, a.entity_id), e.base_currency,
    t.posted_date, t.effective_date, t.raw_description, t.clean_payee, t.amount, t.currency, t.amount_in_entity_currency, t.running_balance_reported,
    t.category_id, c.name, c.kind, t.sub_entity_tag, t.memo, t.fitid, t.is_recurring, t.is_subscription, t.transfer_peer_tx_id, t.review_status, t.business_entity_id, a.entity_id,
    (SELECT COUNT(*) FROM splits s WHERE s.transaction_id = t.id), (SELECT COUNT(*) FROM attachments x WHERE x.account_id = t.account_id AND x.fitid = t.fitid)
    FROM transactions t JOIN accounts a ON a.id = t.account_id JOIN entities e ON e.id = COALESCE(t.business_entity_id, a.entity_id)
    LEFT JOIN bank_profiles bp ON bp.id = a.bank_profile_id LEFT JOIN categories c ON c.id = t.category_id";

fn tx_row(r: &Row) -> rusqlite::Result<TxRow> {
    Ok(TxRow {
        id: r.get(0)?,
        document_id: r.get(1)?,
        account_id: r.get(2)?,
        account_name: r.get(3)?,
        account_type: AccountType::parse(&r.get::<_, String>(4)?),
        bank_short: r.get(5)?,
        entity_id: r.get(6)?,
        entity_currency: ccy(&r.get::<_, String>(7)?),
        posted: date(&r.get::<_, String>(8)?),
        effective: r.get::<_, Option<String>>(9)?.map(|s| date(&s)),
        raw_description: r.get(10)?,
        clean_payee: r.get(11)?,
        amount: dec(&r.get::<_, String>(12)?),
        currency: ccy(&r.get::<_, String>(13)?),
        amount_entity: opt_dec(r.get(14)?),
        balance: opt_dec(r.get(15)?),
        category_id: r.get(16)?,
        category_name: r.get(17)?,
        category_kind: r.get::<_, Option<String>>(18)?.map(|s| cat_kind(&s)),
        tag: r.get(19)?,
        memo: r.get(20)?,
        fitid: r.get(21)?,
        is_recurring: r.get(22)?,
        is_subscription: r.get(23)?,
        transfer_peer: r.get(24)?,
        review: ReviewStatus::parse(&r.get::<_, String>(25)?),
        business_entity_id: r.get(26)?,
        account_entity_id: r.get(27)?,
        splits: r.get(28)?,
        split_seq: None,
        attachments: r.get(29)?,
    })
}

fn profile_row(r: &Row) -> rusqlite::Result<BankProfile> {
    let j = |i: usize| -> rusqlite::Result<String> { r.get(i) };
    Ok(BankProfile {
        id: r.get(0)?,
        slug: r.get(1)?,
        display_name: r.get(2)?,
        short_name: r.get(3)?,
        routing_identifiers: serde_json::from_str(&j(4)?).unwrap_or_default(),
        default_date_format: r.get(5)?,
        column_layout_kind: LayoutKind::parse(&j(6)?),
        header_anchors: serde_json::from_str::<HeaderAnchors>(&j(7)?).unwrap_or_default(),
        noise_patterns: serde_json::from_str(&j(8)?).unwrap_or_default(),
        fx_note_regex: r.get(9)?,
        is_builtin: r.get(10)?,
        layout: {
            let stored: LayoutRules = serde_json::from_str(&j(11)?).unwrap_or_default();
            // a built-in nobody has declared a layout for yet takes the shipped one
            if stored == LayoutRules::default() && r.get::<_, bool>(10)? {
                profiles::builtins().into_iter().find(|p| p.slug == r.get::<_, String>(1).unwrap_or_default()).map(|p| p.layout).unwrap_or_default()
            } else {
                stored
            }
        },
    })
}

/// `chmod`, best effort (a folder on a drive without Unix permissions just keeps its own).
pub(crate) fn private(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

/// One of FinGraph's kept statement copies: a file named by its SHA-256.
fn is_kept_pdf(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".pdf")).is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn kept_pdfs(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| is_kept_pdf(p)).collect()
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            // the data folder is yours alone; a ledger kept somewhere you chose keeps that folder's permissions
            if dir == crate::data_dir() {
                private(dir, 0o700);
            }
        }
        let conn = Connection::open(path)?;
        // SQLite gives the -wal and -shm files the ledger's own permissions
        for f in [path.to_path_buf(), path.with_extension("db-wal"), path.with_extension("db-shm")] {
            private(&f, 0o600);
        }
        Self::init(conn)
    }

    pub fn open_default() -> Result<Db> {
        Self::open(&crate::data_dir().join("ledger.db"))
    }

    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // with WAL, NORMAL is still crash-safe (a power cut can lose only the last commit) and
        // spares an fsync per write
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // another handle on the same ledger (the `ledger` example, a backup) waits instead of failing
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Db { conn };
        db.migrate()?;
        db.seed()?;
        db.prefs().apply_globals();
        Ok(db)
    }

    /// Begin a (nestable) transaction.
    pub fn sp(&self) -> Result<Sp<'_>> {
        self.conn.execute_batch("SAVEPOINT fg")?;
        Ok(Sp { conn: &self.conn, done: false })
    }

    // ---------------------------------------------------------------- undo

    /// One step of undo for edits to transactions and what's been learned (categories, payees, tags,
    /// business and subscription flags, transfer pairs, rules): a copy taken just before the edit, in
    /// temp tables on this connection. A new checkpoint replaces the last; rows deleted or imported
    /// since aren't brought back or removed.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS temp.undo_tx; DROP TABLE IF EXISTS temp.undo_rules; DROP TABLE IF EXISTS temp.undo_unpaired;
             CREATE TEMP TABLE undo_tx AS SELECT * FROM main.transactions;
             CREATE TEMP TABLE undo_rules AS SELECT * FROM main.learned_rules;
             CREATE TEMP TABLE undo_unpaired AS SELECT * FROM main.unpaired;",
        )?;
        Ok(())
    }

    /// Put transactions and rules back as they were at the last `checkpoint`. False when there is none.
    pub fn undo_to_checkpoint(&self) -> Result<bool> {
        let have: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_temp_master WHERE name = 'undo_tx')", [], |r| r.get(0))?;
        if !have {
            return Ok(false);
        }
        let cols: Vec<String> = {
            let mut st = self.conn.prepare("SELECT name FROM pragma_table_info('transactions', 'main') WHERE name <> 'id'")?;
            st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        let list = cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
        let tx = self.sp()?;
        tx.execute_batch(&format!(
            "UPDATE main.transactions SET ({list}) = (SELECT {list} FROM temp.undo_tx u WHERE u.id = main.transactions.id)
                 WHERE id IN (SELECT id FROM temp.undo_tx);
             DELETE FROM main.learned_rules; INSERT INTO main.learned_rules SELECT * FROM temp.undo_rules;
             DELETE FROM main.unpaired; INSERT INTO main.unpaired SELECT * FROM temp.undo_unpaired;"
        ))?;
        tx.commit()?;
        self.conn.execute_batch("DROP TABLE temp.undo_tx; DROP TABLE temp.undo_rules; DROP TABLE temp.undo_unpaired;")?;
        Ok(true)
    }

    // ---------------------------------------------------------------- preferences

    pub fn prefs(&self) -> Prefs {
        self.setting("preferences").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    pub fn save_prefs(&self, p: &Prefs) -> Result<()> {
        self.set_setting("preferences", &serde_json::to_string(p)?)?;
        p.apply_globals();
        Ok(())
    }

    /// The ledger file, when it is one (not in memory).
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.conn.path().filter(|p| !p.is_empty()).map(std::path::PathBuf::from)
    }

    // ---------------------------------------------------------------- backups

    pub(crate) fn validate_backup_database(src: &Path) -> Result<()> {
        let db = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if integrity != "ok" || version < 0 || version as usize > MIGRATIONS.len() {
            return Err(Error("Backup is damaged or needs a newer FinGraph version".into()));
        }
        db.query_row("SELECT COUNT(*) FROM entities", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    /// Everything you put in goes: workspaces, accounts, statements, transactions, rules and
    /// settings. Bank profiles, categories and preferences stay.
    pub fn erase_all(&self) -> Result<()> {
        let tx = self.sp()?;
        for t in [
            "transactions", "documents", "accounts", "learned_rules", "entities", "splits", "reimbursements", "attachments", "budgets", "budget_history", "budget_moves", "sinking_funds",
            "month_closes", "manual_values", "manual_items", "goal_entries", "goals", "deleted_rows", "unpaired",
        ] {
            tx.execute(&format!("DELETE FROM {t}"), [])?;
        }
        // preferences, and the bookkeeping that says the bank profiles are already up to date (otherwise the
        // next open resets the built-ins and their learned layouts)
        tx.execute("DELETE FROM settings WHERE key != 'preferences' AND key != 'fitid_v2' AND key NOT LIKE 'profile_rev:%' AND key NOT LIKE 'last_id:%'", [])?;
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- kept statement PDFs

    /// The ledger's own folder (a scratch or demo ledger has its own, so it never touches your real files).
    fn home(&self) -> std::path::PathBuf {
        self.path().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| std::env::temp_dir().join(format!("fingraph-memory-{}", std::process::id())))
    }

    /// Where kept statement PDFs live: the folder chosen in Preferences, else `statements/` beside the ledger.
    pub fn statements_dir(&self) -> std::path::PathBuf {
        self.prefs().statements_dir.unwrap_or_else(|| self.home().join("statements"))
    }

    /// Keep a copy of an imported PDF, named by its SHA-256, so it can be rescanned later.
    pub fn keep_pdf(&self, src: &Path, sha256: &str) -> Result<()> {
        let dir = self.statements_dir();
        std::fs::create_dir_all(&dir)?;
        private(&dir, 0o700);
        let dst = dir.join(format!("{sha256}.pdf"));
        if !dst.exists() {
            std::fs::copy(src, &dst)?;
            private(&dst, 0o600);
        }
        Ok(())
    }

    /// A statement's PDF: the kept copy, else a file with the same name and contents in the data folder.
    pub fn stored_pdf(&self, sha256: &str, filename: &str) -> Option<std::path::PathBuf> {
        let kept = self.statements_dir().join(format!("{sha256}.pdf"));
        if kept.exists() {
            return Some(kept);
        }
        let old = self.home().join("documents").join(filename);
        (file_sha256(&old).ok().as_deref() == Some(sha256)).then_some(old)
    }

    /// Move the kept PDFs to a new folder and remember it. Only FinGraph's own copies (`<sha256>.pdf`) ever
    /// move; a folder that already holds other things gets a `FinGraph Statements` folder inside it.
    /// Returns how many moved and the folder they're in now.
    pub fn move_statements_dir(&self, to: &Path) -> Result<(usize, std::path::PathBuf)> {
        let from = self.statements_dir();
        let others = std::fs::read_dir(to).map(|d| d.flatten().any(|e| !is_kept_pdf(&e.path()))).unwrap_or(false);
        let to = if others { to.join("FinGraph Statements") } else { to.to_path_buf() };
        let mut n = 0;
        if to != from {
            std::fs::create_dir_all(&to)?;
            private(&to, 0o700);
            for f in kept_pdfs(&from) {
                let dst = to.join(f.file_name().unwrap_or_default());
                if std::fs::rename(&f, &dst).is_err() {
                    std::fs::copy(&f, &dst)?;
                    std::fs::remove_file(&f)?;
                }
                n += 1;
            }
            // only if nothing else is left in it
            let _ = std::fs::remove_dir(&from);
        }
        let mut p = self.prefs();
        p.statements_dir = Some(to.clone());
        self.save_prefs(&p)?;
        Ok((n, to))
    }

    /// Erase's "also delete the kept statement PDFs": FinGraph's own copies only, never anything else in the folder.
    pub fn delete_kept_pdfs(&self) -> Result<usize> {
        let dir = self.statements_dir();
        let files = kept_pdfs(&dir);
        for f in &files {
            std::fs::remove_file(f)?;
        }
        let _ = std::fs::remove_dir(&dir);
        Ok(files.len())
    }

    /// A kept copy no statement uses any more (its statement was deleted) goes too.
    fn drop_unused_pdf(&self, sha256: &str) -> Result<()> {
        let used: i64 = self.conn.query_row("SELECT COUNT(*) FROM documents WHERE sha256_hash = ?1", [sha256], |r| r.get(0))?;
        let f = self.statements_dir().join(format!("{sha256}.pdf"));
        if used == 0 && is_kept_pdf(&f) {
            let _ = std::fs::remove_file(f);
        }
        Ok(())
    }

    pub(crate) fn migrate(&self) -> Result<()> {
        let v: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(v as usize) {
            let tx = self.sp()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub(crate) fn seed(&self) -> Result<()> {
        for p in profiles::builtins() {
            let exists: bool = self.conn.query_row("SELECT COUNT(*) FROM bank_profiles WHERE slug = ?1", [&p.slug], |r| r.get::<_, i64>(0))? > 0;
            if !exists {
                self.save_profile(&p)?;
            }
        }
        // a built-in rewritten from a real statement replaces the stored guess, once (CNB 0.2.3,
        // Scotiabank's real headings 0.2.4); the next bank checked against a real PDF is added here
        if self.setting("fitid_v2").is_none() {
            self.rekey_fitids()?;
            self.set_setting("fitid_v2", "1")?;
        }
        for (slug, rev) in [("cnb_ky", "1"), ("scotiabank_ky", "2")] {
            let key = format!("profile_rev:{slug}");
            if self.setting(&key).as_deref() != Some(rev) {
                if let Some(id) = self.conn.query_row("SELECT id FROM bank_profiles WHERE slug = ?1 AND is_builtin = 1", [slug], |r| r.get::<_, i64>(0)).optional()? {
                    self.delete_or_reset_profile(id)?;
                }
                self.set_setting(&key, rev)?;
            }
        }
        let fx = Fx::default();
        self.conn.execute("INSERT OR IGNORE INTO fx_rates VALUES ('KYD', 'USD', ?1)", [fx.kyd_to_usd.to_string()])?;
        self.conn.execute("INSERT OR IGNORE INTO fx_rates VALUES ('USD', 'KYD', ?1)", [fx.usd_to_kyd.to_string()])?;
        // add whatever's missing, so older ledgers pick up new categories; nothing is renamed or removed
        let find = |name: &str, parent: Option<i64>| -> Result<Option<i64>> {
            Ok(self.conn.query_row("SELECT id FROM categories WHERE name = ?1 AND parent_id IS ?2", params![name, parent], |r| r.get(0)).optional()?)
        };
        for (name, kind, kids) in CATEGORIES {
            let pid = match find(name, None)? {
                Some(id) => id,
                None => {
                    self.conn.execute("INSERT INTO categories (name, parent_id, kind) VALUES (?1, NULL, ?2)", params![name, kind])?;
                    self.conn.last_insert_rowid()
                }
            };
            for k in *kids {
                if find(k, Some(pid))?.is_none() {
                    self.conn.execute("INSERT INTO categories (name, parent_id, kind) VALUES (?1, ?2, ?3)", params![k, pid, kind])?;
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- settings

    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO settings VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
        Ok(())
    }

    // ---------------------------------------------------------------- entities

    pub fn entities(&self) -> Result<Vec<Entity>> {
        let mut st = self.conn.prepare("SELECT id, name, kind, base_currency, icon_name FROM entities ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(Entity { id: r.get(0)?, name: r.get(1)?, kind: EntityKind::parse(&r.get::<_, String>(2)?), base_currency: ccy(&r.get::<_, String>(3)?), icon_name: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn create_entity(&self, name: &str, kind: EntityKind, base: Currency) -> Result<i64> {
        let icon = match kind {
            EntityKind::Personal => "avatar-default-symbolic",
            EntityKind::Business => "folder-documents-symbolic",
            EntityKind::RentalProperty => "go-home-symbolic",
        };
        let id = self.next_id("entities")?;
        self.conn.execute("INSERT INTO entities (id, name, kind, base_currency, icon_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, name, kind.as_str(), base.code(), icon, now()])?;
        Ok(id)
    }

    /// Workspace and account ids are never handed out twice, so what's keyed by one without a foreign key
    /// (budgets, goals, receipts, APRs) can't turn up in a new workspace after a delete.
    fn next_id(&self, table: &str) -> Result<i64> {
        let key = format!("last_id:{table}");
        let max: i64 = self.conn.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM {table}"), [], |r| r.get(0))?;
        let n = max.max(self.setting(&key).and_then(|v| v.parse().ok()).unwrap_or(0)) + 1;
        self.set_setting(&key, &n.to_string())?;
        Ok(n)
    }

    pub fn update_entity(&self, id: i64, name: &str, base: Currency) -> Result<()> {
        self.conn.execute("UPDATE entities SET name = ?2, base_currency = ?3 WHERE id = ?1", params![id, name, base.code()])?;
        self.revalue(Some(id))
    }

    pub fn entity_by_name(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM entities WHERE name = ?1", [name], |r| r.get(0)).optional()?)
    }

    // ---------------------------------------------------------------- bank profiles

    /// 0.2.4 changed the transaction fingerprint (no description in it); give every stored transaction
    /// its new one, and carry repayments and receipts that point at the old one across.
    fn rekey_fitids(&self) -> Result<()> {
        type Old = (i64, i64, Option<i64>, String, String, String, String);
        let rows: Vec<Old> = {
            let mut st = self.conn.prepare("SELECT id, account_id, document_id, posted_date, amount, fitid, memo FROM transactions ORDER BY COALESCE(document_id, 0), id")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let tx = self.sp()?;
        // numbered within each statement, the way an import numbers them
        let mut seq: std::collections::HashMap<(i64, Option<i64>, String, Decimal, bool), usize> = Default::default();
        for (id, account, doc, posted, amount, old, memo) in rows {
            let amount = dec(&amount);
            // lines converted by hand keep their own numbering, clear of the scanned rows'
            let manual = memo.starts_with("Converted from skipped line");
            let n = seq.entry((account, doc, posted.clone(), crate::money::round2(amount), manual)).or_insert(0);
            let new = fitid(account, date(&posted), amount, *n + if manual { 1000 } else { 0 });
            *n += 1;
            // a clash means the ledger already held this row twice (read two ways); leave that one alone
            if tx.execute("UPDATE OR IGNORE transactions SET fitid = ?2 WHERE id = ?1", params![id, new])? == 1 {
                tx.execute("UPDATE reimbursements SET fitid = ?3 WHERE account_id = ?1 AND fitid = ?2", params![account, old, new])?;
                tx.execute("UPDATE reimbursements SET paid_fitid = ?3 WHERE paid_account_id = ?1 AND paid_fitid = ?2", params![account, old, new])?;
                tx.execute("UPDATE attachments SET fitid = ?3 WHERE account_id = ?1 AND fitid = ?2", params![account, old, new])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn profiles(&self) -> Result<Vec<BankProfile>> {
        let mut st = self.conn.prepare("SELECT id, slug, display_name, short_name, routing_identifiers, default_date_format, column_layout_kind, header_anchors, noise_patterns, fx_note_regex, is_builtin, layout FROM bank_profiles ORDER BY is_builtin DESC, display_name")?;
        let rows = st.query_map([], profile_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert (id 0) or update a profile; returns its id.
    pub fn save_profile(&self, p: &BankProfile) -> Result<i64> {
        let ids = serde_json::to_string(&p.routing_identifiers)?;
        let anchors = serde_json::to_string(&p.header_anchors)?;
        let noise = serde_json::to_string(&p.noise_patterns)?;
        let layout = serde_json::to_string(&p.layout)?;
        if p.id == 0 {
            self.conn.execute(
                "INSERT INTO bank_profiles (slug, display_name, short_name, routing_identifiers, default_date_format, column_layout_kind, header_anchors, noise_patterns, fx_note_regex, is_builtin, layout) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![p.slug, p.display_name, p.short_name, ids, p.default_date_format, p.column_layout_kind.as_str(), anchors, noise, p.fx_note_regex, p.is_builtin, layout],
            )?;
            Ok(self.conn.last_insert_rowid())
        } else {
            self.conn.execute(
                "UPDATE bank_profiles SET display_name=?2, short_name=?3, routing_identifiers=?4, default_date_format=?5, column_layout_kind=?6, header_anchors=?7, noise_patterns=?8, fx_note_regex=?9, layout=?10 WHERE id=?1",
                params![p.id, p.display_name, p.short_name, ids, p.default_date_format, p.column_layout_kind.as_str(), anchors, noise, p.fx_note_regex, layout],
            )?;
            Ok(p.id)
        }
    }

    /// Custom profiles can be deleted; built-ins are put back to their shipped settings instead.
    pub fn delete_or_reset_profile(&self, id: i64) -> Result<()> {
        let (slug, builtin): (String, bool) = self.conn.query_row("SELECT slug, is_builtin FROM bank_profiles WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if builtin {
            if let Some(mut p) = profiles::builtins().into_iter().find(|p| p.slug == slug) {
                p.id = id;
                self.save_profile(&p)?;
            }
        } else {
            self.conn.execute("DELETE FROM bank_profiles WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- fx

    pub fn fx(&self) -> Result<Fx> {
        let get = |a: &str, b: &str| -> Result<Option<Decimal>> {
            Ok(self.conn.query_row("SELECT rate FROM fx_rates WHERE from_currency = ?1 AND to_currency = ?2", [a, b], |r| r.get::<_, String>(0)).optional()?.map(|s| dec(&s)))
        };
        let d = Fx::default();
        let mut daily: std::collections::BTreeMap<Currency, std::collections::BTreeMap<NaiveDate, Decimal>> = Default::default();
        let mut st = self.conn.prepare("SELECT day, currency, per_usd FROM fx_daily")?;
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
            let (day, c, rate) = row?;
            if let (Ok(day), Some(c)) = (NaiveDate::parse_from_str(&day, "%Y-%m-%d"), Currency::parse(&c)) {
                daily.entry(c).or_default().insert(day, dec(&rate));
            }
        }
        Ok(Fx { kyd_to_usd: get("KYD", "USD")?.unwrap_or(d.kyd_to_usd), usd_to_kyd: get("USD", "KYD")?.unwrap_or(d.usd_to_kyd), daily })
    }

    /// Every currency used by recorded or planned money that needs downloaded rates.
    pub fn floating_currencies(&self) -> Result<Vec<Currency>> {
        let mut st = self.conn.prepare("SELECT currency FROM accounts UNION SELECT base_currency FROM entities
            UNION SELECT currency FROM transactions UNION SELECT currency FROM budget_history
            UNION SELECT currency FROM budget_moves UNION SELECT currency FROM sinking_funds
            UNION SELECT currency FROM manual_items UNION SELECT currency FROM goals")?;
        let mut v: Vec<Currency> = st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|c| c.ok().and_then(|c| Currency::parse(&c))).filter(|c| !c.pegged()).collect();
        let mut plans = self.conn.prepare("SELECT value FROM settings WHERE key LIKE 'budget_plan:%'")?;
        for json in plans.query_map([], |r| r.get::<_, String>(0))? {
            if let Some(c) = serde_json::from_str::<crate::budget::Plan>(&json?).ok().and_then(|p| p.currency).filter(|c| !c.pegged()) { v.push(c); }
        }
        if let Some(c) = self.setting("display_currency").and_then(|c| Currency::parse(&c)).filter(|c| !c.pegged()) { v.push(c); }
        v.sort();
        v.dedup();
        Ok(v)
    }

    /// What to download: the floating currencies over each statement period that has no rate stored yet (a
    /// workspace in a floating currency needs its rate for every statement, an account only for its own), and
    /// from the day after the latest stored rate up to `today`. Ranges come back merged per currency set.
    pub fn rates_needed(&self, today: NaiveDate) -> Result<Vec<(Vec<Currency>, NaiveDate, NaiveDate)>> {
        let floating = self.floating_currencies()?;
        if floating.is_empty() {
            return Ok(Vec::new());
        }
        let fx = self.fx()?;
        let mut want: Vec<(Vec<Currency>, NaiveDate, NaiveDate)> = Vec::new();
        let periods: Vec<(String, String, String, String)> = {
            let mut st = self.conn.prepare(
                "SELECT d.period_start, d.period_end, a.currency, e.base_currency FROM documents d JOIN accounts a ON a.id = d.account_id
                 JOIN entities e ON e.id = a.entity_id WHERE d.period_start IS NOT NULL AND d.period_end IS NOT NULL",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (s0, e0, acct, base) in periods {
            let (start, end) = (date(&s0), date(&e0));
            let cs: Vec<Currency> = [ccy(&acct), ccy(&base)].into_iter().filter(|c| !c.pegged()).collect();
            // covered: a rate stored within the period (or the four days before it, for a weekend start)
            let covered = |c: &Currency| fx.daily.get(c).is_some_and(|m| m.range(start - chrono::Duration::days(4)..=end).next().is_some());
            let missing: Vec<Currency> = cs.into_iter().filter(|c| !covered(c)).collect();
            if !missing.is_empty() {
                want.push((missing, start - chrono::Duration::days(4), end.min(today)));
            }
        }
        // and today's: from the day after the latest stored (or the last 7 days, for a currency never fetched)
        for c in &floating {
            let from = fx.daily.get(c).and_then(|m| m.keys().next_back().copied()).map_or(today - chrono::Duration::days(7), |d| d + chrono::Duration::days(1));
            if from <= today - chrono::Duration::days(1) {
                want.push((vec![*c], from, today));
            }
        }
        // merge overlapping requests for the same currencies
        want.sort_by_key(|w| (w.0.clone(), w.1));
        let mut merged: Vec<(Vec<Currency>, NaiveDate, NaiveDate)> = Vec::new();
        for w in want {
            match merged.last_mut() {
                Some(m) if m.0 == w.0 && w.1 <= m.2 + chrono::Duration::days(1) => m.2 = m.2.max(w.2),
                _ => merged.push(w),
            }
        }
        Ok(merged)
    }

    /// Keep downloaded rates (units per US dollar), then convert every transaction again at its own day's rate.
    /// A rate that can't be right is dropped, so one bad answer from a server can't turn 50 GBP into millions:
    /// a pegged currency, a day still to come, a rate outside any currency's range, or one more than half off
    /// the currency's stored rate within a month of it (else this download's middle rate). Returns how many were kept.
    pub fn store_rates(&self, rates: &[(NaiveDate, Currency, Decimal)], source: &str) -> Result<usize> {
        let fx = self.fx()?;
        let today = Local::now().date_naive();
        let mut batch: std::collections::BTreeMap<Currency, Vec<Decimal>> = Default::default();
        for (_, c, r) in rates {
            batch.entry(*c).or_default().push(*r);
        }
        for v in batch.values_mut() {
            v.sort();
        }
        let month = chrono::Duration::days(31);
        let sane = |day: &NaiveDate, c: &Currency, r: &Decimal| {
            if c.pegged() || *day > today || *r < Decimal::new(1, 6) || *r > Decimal::from(10_000_000) {
                return false;
            }
            let stored = fx.daily.get(c).and_then(|m| {
                let near = [m.range(..=*day).next_back(), m.range(*day..).next()];
                near.into_iter().flatten().filter(|(d, _)| (**d - *day).abs() <= month).min_by_key(|(d, _)| (**d - *day).abs()).map(|(_, v)| *v)
            });
            let middle = batch.get(c).map(|v| v[v.len() / 2]);
            stored.or(middle).is_some_and(|m| (*r - m).abs() <= m / Decimal::TWO)
        };
        let keep: Vec<_> = rates.iter().filter(|(d, c, r)| sane(d, c, r)).collect();
        let tx = self.sp()?;
        for (day, c, r) in &keep {
            tx.execute("INSERT OR REPLACE INTO fx_daily VALUES (?1, ?2, ?3, ?4)", params![day.to_string(), c.code(), r.to_string(), source])?;
        }
        tx.commit()?;
        if !keep.is_empty() {
            self.revalue(None)?;
        }
        Ok(keep.len())
    }

    /// For Preferences: each floating currency in use, its latest stored rate's day and source (`None`: none yet).
    pub fn rates_status(&self) -> Result<Vec<RateStatus>> {
        let mut out = Vec::new();
        for c in self.floating_currencies()? {
            let last: Option<(String, String)> = self.conn.query_row("SELECT day, source FROM fx_daily WHERE currency = ?1 ORDER BY day DESC LIMIT 1", [c.code()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            out.push((c, last.map(|(d, s)| (date(&d), s))));
        }
        Ok(out)
    }

    /// Set the KYD→USD rate (USD→KYD follows) and revalue every cross-currency row.
    pub fn set_fx(&self, kyd_to_usd: Decimal) -> Result<()> {
        if kyd_to_usd <= Decimal::ZERO {
            return Err(Error("the rate must be positive".into()));
        }
        let inv = (Decimal::ONE / kyd_to_usd).round_dp(8);
        self.conn.execute("INSERT OR REPLACE INTO fx_rates VALUES ('KYD', 'USD', ?1)", [kyd_to_usd.to_string()])?;
        self.conn.execute("INSERT OR REPLACE INTO fx_rates VALUES ('USD', 'KYD', ?1)", [inv.to_string()])?;
        self.revalue(None)
    }

    /// Recompute `amount_in_entity_currency` (keeping statement-quoted FX originals).
    pub fn revalue_all(&self) -> Result<()> {
        self.revalue(None)
    }

    fn revalue(&self, entity: Option<i64>) -> Result<()> {
        let fx = self.fx()?;
        let rows: Vec<(i64, String, String, String, String, String)> = {
            let mut st = self.conn.prepare(
                "SELECT t.id, t.amount, t.currency, e.base_currency, t.memo, t.posted_date FROM transactions t JOIN accounts a ON a.id = t.account_id
                 JOIN entities e ON e.id = COALESCE(t.business_entity_id, a.entity_id) WHERE (?1 IS NULL OR e.id = ?1 OR a.entity_id = ?1)",
            )?;
            st.query_map([entity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let tx = self.sp()?;
        for (id, amt, c, base, memo, day) in rows {
            let v = entity_amount(&fx, dec(&amt), ccy(&c), ccy(&base), parse_fx_memo(&memo), date(&day));
            tx.execute("UPDATE transactions SET amount_in_entity_currency = ?2, amount_in_entity_minor = ?3 WHERE id = ?1", params![id, v.map(|v| v.to_string()).unwrap_or_default(), v.map(to_minor).unwrap_or_default()])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- accounts

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut st = self.conn.prepare(
            "SELECT a.id, a.entity_id, a.bank_profile_id, COALESCE(bp.short_name, ''), a.name, a.account_number_masked, a.account_type, a.currency, a.ofx_bank_id, a.ofx_intu_bid, a.hidden
             FROM accounts a LEFT JOIN bank_profiles bp ON bp.id = a.bank_profile_id ORDER BY a.entity_id, a.name",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                entity_id: r.get(1)?,
                bank_profile_id: r.get(2)?,
                bank_short: r.get(3)?,
                name: r.get(4)?,
                account_number_masked: r.get(5)?,
                account_type: AccountType::parse(&r.get::<_, String>(6)?),
                currency: ccy(&r.get::<_, String>(7)?),
                ofx_bank_id: r.get(8)?,
                ofx_intu_bid: r.get(9)?,
                hidden: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_account(&self, a: &Account) -> Result<()> {
        let tx = self.sp()?;
        tx.execute(
            "UPDATE accounts SET name = ?2, ofx_bank_id = ?3, ofx_intu_bid = ?4, entity_id = ?5, hidden = ?6, currency = ?7 WHERE id = ?1",
            params![a.id, a.name, a.ofx_bank_id, a.ofx_intu_bid, a.entity_id, a.hidden, a.currency.code()],
        )?;
        // a statement whose currency was a guess, put right: its statements and rows are in that currency too
        tx.execute("UPDATE transactions SET currency = ?2 WHERE account_id = ?1", params![a.id, a.currency.code()])?;
        tx.execute("UPDATE documents SET currency = ?2 WHERE account_id = ?1", params![a.id, a.currency.code()])?;
        tx.commit()?;
        self.revalue(Some(a.entity_id))
    }

    /// The account a statement section belongs to: same bank, last four digits, currency and type.
    /// The known account a section belongs to: same bank, same last 4 digits, same currency. The account
    /// type is a tie-breaker only (it's detected, and an OCR read can get it wrong); with no account
    /// number there's no match, and the person picks.
    fn find_account(&self, profile: Option<&BankProfile>, s: &Section) -> Result<Option<i64>> {
        let Some(last4) = &s.account_last4 else { return Ok(None) };
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM accounts WHERE bank_profile_id IS ?1 AND account_number_masked = ?2 AND currency = ?3 ORDER BY account_type = ?4 DESC, id LIMIT 1",
                params![profile.map(|p| p.id), format!("••{last4}"), s.currency.code(), s.account_type.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn new_account_name(profile: Option<&BankProfile>, s: &Section) -> String {
        let bank = profile.map_or("Bank", |p| p.short_name.as_str());
        format!("{bank} {} {}{}", s.account_type.label(), s.currency.code(), s.account_last4.as_ref().map(|l| format!(" ••{l}")).unwrap_or_default())
    }

    fn create_account(&self, entity_id: i64, profile: Option<&BankProfile>, s: &Section) -> Result<i64> {
        let masked = s.account_last4.as_ref().map(|l| format!("••{l}")).unwrap_or_default();
        let pid = profile.map(|p| p.id);
        let name = Self::new_account_name(profile, s);
        let id = self.next_id("accounts")?;
        self.conn.execute(
            "INSERT INTO accounts (id, entity_id, bank_profile_id, name, account_number_masked, account_type, currency, opening_balance, opening_balance_minor, opening_date) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)",
            params![id, entity_id, pid, name, masked, s.account_type.as_str(), s.currency.code(), s.opening.unwrap_or_default().to_string(), to_minor(s.opening.unwrap_or_default())],
        )?;
        Ok(id)
    }

    // ---------------------------------------------------------------- import

    /// Store a scanned statement: one document per section, transactions deduplicated by FITID,
    /// learned rules applied, transfers matched.
    /// What importing this scan would do: which accounts it matches, and anything that needs asking.
    pub fn plan_import(&self, sha256: &str, scan: &ScanResult) -> Result<ImportPlan> {
        let profiles = self.profiles()?;
        let profile = scan.profile_slug.as_ref().and_then(|s| profiles.iter().find(|p| &p.slug == s));
        let mut plan = ImportPlan {
            already_imported: self.conn.query_row("SELECT COUNT(*) FROM documents WHERE sha256_hash = ?1", [sha256], |r| r.get::<_, i64>(0))? > 0,
            no_transactions: nothing_read(scan),
            ..Default::default()
        };
        for s in &scan.sections {
            let matched = self.find_account(profile, s)?;
            let candidates = if s.account_last4.is_none() {
                let mut st = self.conn.prepare("SELECT id, name FROM accounts WHERE bank_profile_id IS ?1 AND currency = ?2 ORDER BY name")?;
                st.query_map(params![profile.map(|p| p.id), s.currency.code()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            } else {
                Vec::new()
            };
            if let (Some(a), Some((from, to))) = (matched, scan.period) {
                let mut st = self.conn.prepare("SELECT id, filename FROM documents WHERE account_id = ?1 AND period_start = ?2 AND period_end = ?3 AND sha256_hash != ?4")?;
                let dups: Vec<(i64, String)> = st.query_map(params![a, from.to_string(), to.to_string(), sha256], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                plan.same_period.extend(dups);
            }
            plan.sections.push(SectionPlan { matched, new_name: Self::new_account_name(profile, s), number_read: s.account_last4.is_some(), candidates });
        }
        Ok(plan)
    }

    /// Import into known accounts where they match and new ones otherwise, without asking.
    pub fn import_auto(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult) -> Result<ImportReport> {
        let targets = self.plan_import(sha256, scan)?.default_targets();
        self.import_scan(entity_id, filename, sha256, scan, &targets)
    }

    /// Why a freshly read section should wait for review.
    fn review_reasons(profile: Option<&BankProfile>, scan: &ScanResult, s: &Section) -> Vec<Review> {
        let mut v = Vec::new();
        match s.reconciliation {
            Reconciliation::Reconciled => {}
            Reconciliation::Discrepancy(_) => v.push(Review::Mismatch),
            Reconciliation::Unverified => v.push(Review::NoBalances),
        }
        if scan.diagnostics.pass1.ocr {
            v.push(Review::Ocr);
        }
        if !profile.is_some_and(|p| p.layout.verified.iter().any(|t| t == s.account_type.as_str())) {
            v.push(Review::NewLayout);
        }
        if !scan.diagnostics.pass3.layout_notes.is_empty() {
            v.push(Review::OffLayout);
        }
        if !s.report.derived.is_empty() {
            v.push(Review::Derived);
        }
        v
    }

    /// Store a scanned statement, each section into the account `targets` names. A section that doesn't
    /// reconcile, was read by OCR, or is the first of its kind from its bank is held for review.
    pub fn import_scan(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult, targets: &[Target]) -> Result<ImportReport> {
        self.import_scan_reviewed(entity_id, filename, sha256, scan, targets, &[])
    }

    /// `approved`: sections (by index) already approved before a rescan, which only a new mismatch holds again.
    fn import_scan_reviewed(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult, targets: &[Target], approved: &[(usize, bool)]) -> Result<ImportReport> {
        if nothing_read(scan) {
            return Err(Error(nothing_read_reason(scan).into()));
        }
        let mut rep = ImportReport::default();
        // OCR'd statements say so wherever their name shows, so nobody takes the names as exact
        let ocr_name = format!("{filename} (read by OCR)");
        let filename = if scan.diagnostics.pass1.ocr { ocr_name.as_str() } else { filename };
        let known: i64 = self.conn.query_row("SELECT COUNT(*) FROM documents WHERE sha256_hash = ?1", [sha256], |r| r.get(0))?;
        if known > 0 {
            rep.already_imported = true;
            return Ok(rep);
        }
        let profiles = self.profiles()?;
        let profile = scan.profile_slug.as_ref().and_then(|s| profiles.iter().find(|p| &p.slug == s));
        let fx = self.fx()?;
        let rules = self.rules()?;
        let tx = self.sp()?;
        for (si, s) in scan.sections.iter().enumerate() {
            let account_id = match targets.get(si).copied().unwrap_or(Target::New) {
                Target::Skip => continue,
                Target::Existing(id) => id,
                Target::New => self.create_account(entity_id, profile, s)?,
            };
            // an account found in another workspace counts in that one's currency, under that one's rules
            let (entity_id, base): (i64, Currency) =
                tx.query_row("SELECT a.entity_id, e.base_currency FROM accounts a JOIN entities e ON e.id = a.entity_id WHERE a.id = ?1", [account_id], |r| Ok((r.get(0)?, ccy(&r.get::<_, String>(1)?))))?;
            // the account's previous statement: where this one has to start
            let mut s = std::borrow::Cow::Borrowed(s);
            let mut continuity: Vec<String> = Vec::new();
            let mut broken = false;
            if let Some((start, _)) = scan.period
                && let Some((prev, name)) = self.previous_closing(account_id, start)?
            {
                match s.opening {
                    Some(o) if o != prev => {
                        broken = true;
                        continuity.push(format!("Opens at {o}, but the account's previous statement ({name}) closed at {prev}: a statement, or some of its rows, may be missing"));
                    }
                    Some(_) => {}
                    None if s.reconciliation != Reconciliation::Reconciled => {
                        // no opening printed: last month's closing is this month's opening; kept only if the rows then add up
                        let mut t = s.clone().into_owned();
                        t.opening = Some(prev);
                        crate::parser::reconcile::verify_and_heal(&mut t);
                        if t.reconciliation == Reconciliation::Reconciled {
                            continuity.push(format!("No opening balance is printed: the previous statement's closing balance ({prev}, {name}) was used, and every row adds up from it"));
                            s = std::borrow::Cow::Owned(t);
                        }
                    }
                    None => {}
                }
            }
            let s = s.as_ref();
            let mut why = Self::review_reasons(profile, scan, s);
            let was = approved.iter().find(|a| a.0 == si);
            if broken && was.is_none() {
                why.push(Review::Continuity);
            }
            match was {
                // approved before and reads the same: stays approved
                Some((_, true)) => why.clear(),
                // approved before: only what this read found wrong holds it again
                Some((_, false)) => why.retain(|r| matches!(r, Review::Mismatch | Review::NoBalances | Review::OffLayout)),
                None => {}
            }
            // the bank changed its layout: this one looks different from the account's statement before it
            let mut diagnostics = std::borrow::Cow::Borrowed(&scan.diagnostics);
            if !continuity.is_empty() {
                diagnostics.to_mut().pass3.layout_notes.extend(continuity);
            }
            let changes = self.previous_layout(account_id, scan.period.map(|p| p.1))?.map(|o| scan.diagnostics.observed.changes_from(&o)).unwrap_or_default();
            if !changes.is_empty() && was.is_none() {
                why.push(Review::LayoutChanged);
                let notes = &mut diagnostics.to_mut().pass3.layout_notes;
                notes.extend(changes.iter().map(|c| format!("Since this account's previous statement: {c}")));
            }
            rep.held += !why.is_empty() as usize;
            #[derive(serde::Serialize)]
            struct Stored<'a> {
                diagnostics: &'a Diagnostics,
                report: &'a SectionReport,
            }
            let diag = serde_json::to_string(&Stored { diagnostics: &diagnostics, report: &s.report })?;
            let sum = s.extracted_sum();
            tx.execute(
                "INSERT INTO documents (account_id, bank_profile_id, filename, sha256_hash, section_index, period_start, period_end, statement_opening_balance, statement_closing_balance,
                 statement_total_debits, statement_total_credits, extracted_tx_sum, extracted_tx_sum_minor, currency, reconciliation_status, scan_confidence_score, scan_diagnostics_json, imported_at,
                 review_hold, review_note)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                params![
                    account_id,
                    profile.map(|p| p.id),
                    filename,
                    sha256,
                    si as i64,
                    scan.period.map(|p| p.0.to_string()),
                    scan.period.map(|p| p.1.to_string()),
                    s.opening.map(|d| d.to_string()),
                    s.closing.map(|d| d.to_string()),
                    s.total_debits.map(|d| d.to_string()),
                    s.total_credits.map(|d| d.to_string()),
                    sum.to_string(),
                    to_minor(sum),
                    s.currency.code(),
                    s.reconciliation.to_db(),
                    s.confidence,
                    diag,
                    now(),
                    !why.is_empty(),
                    Review::join(&why)
                ],
            )?;
            let doc_id = tx.last_insert_rowid();
            rep.document_ids.push(doc_id);
            let mut seq: std::collections::HashMap<(NaiveDate, Decimal), usize> = Default::default();
            for t in &s.transactions {
                let n = seq.entry((t.posted, crate::money::round2(t.amount))).or_insert(0);
                let fid = fitid(account_id, t.posted, t.amount, *n);
                *n += 1;
                // you deleted it: a rescan or another copy of the statement doesn't bring it back
                if tx.query_row("SELECT COUNT(*) FROM deleted_rows WHERE account_id = ?1 AND fitid = ?2", params![account_id, fid], |r| r.get::<_, i64>(0))? > 0 {
                    continue;
                }
                let stem = rules::merchant_stem(&t.description, &t.memo);
                let (payee, _) = rules::known_merchant(&stem, &t.description);
                let ent = entity_amount(&fx, t.amount, s.currency, base, t.fx_original, t.posted);
                let mut memo = t.memo.clone();
                if let Some((oc, oa)) = t.fx_original
                    && !memo.contains(&format!("FX original {oc} {oa}"))
                {
                    memo = format!("{memo}{}FX original {oc} {oa}", if memo.is_empty() { "" } else { " | " });
                }
                let rule = rules::first_match(&rules, entity_id, &t.description, &memo, t.amount);
                let n = tx.execute(
                    "INSERT OR IGNORE INTO transactions (document_id, account_id, posted_date, effective_date, raw_description, clean_payee, amount, amount_minor, currency,
                     amount_in_entity_currency, amount_in_entity_minor, running_balance_reported, category_id, memo, fitid, is_subscription, review_status)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                    params![
                        doc_id,
                        account_id,
                        t.posted.to_string(),
                        t.effective.map(|d| d.to_string()),
                        t.description,
                        rule.and_then(|r| r.clean_payee.clone()).unwrap_or(payee),
                        t.amount.to_string(),
                        to_minor(t.amount),
                        s.currency.code(),
                        ent.map(|v| v.to_string()).unwrap_or_default(),
                        ent.map(to_minor).unwrap_or_default(),
                        t.balance.map(|b| b.to_string()),
                        rule.and_then(|r| r.category_id),
                        memo,
                        fid,
                        rule.is_some_and(|r| r.is_subscription),
                        if rule.is_some_and(|r| r.category_id.is_some()) { ReviewStatus::AutoCategorized } else { ReviewStatus::NeedsReview }.as_str()
                    ],
                )?;
                if n > 0
                    && let Some(b) = rule.and_then(|r| r.business_entity_id).filter(|b| *b != entity_id)
                {
                    let id = tx.last_insert_rowid();
                    let base: String = tx.query_row("SELECT base_currency FROM entities WHERE id = ?1", [b], |r| r.get(0))?;
                    let v = entity_amount(&fx, t.amount, s.currency, ccy(&base), t.fx_original, t.posted);
                    tx.execute("UPDATE transactions SET business_entity_id = ?2, amount_in_entity_currency = ?3, amount_in_entity_minor = ?4 WHERE id = ?1", params![id, b, v.map(|v| v.to_string()).unwrap_or_default(), v.map(to_minor).unwrap_or_default()])?;
                }
                if n == 0 {
                    rep.duplicates += 1;
                    tx.execute("INSERT OR IGNORE INTO shared_rows (document_id, transaction_id) SELECT ?1, id FROM transactions WHERE account_id = ?2 AND fitid = ?3", params![doc_id, account_id, fid])?;
                } else {
                    rep.inserted += 1;
                    rep.auto_categorized += rule.is_some_and(|r| r.category_id.is_some()) as usize;
                }
            }
            if why.is_empty() {
                self.claim_rows(doc_id)?;
            }
        }
        tx.commit()?;
        self.match_transfers()?;
        self.mark_card_payments()?;
        self.mark_recurring()?;
        rep.auto_categorized += self.apply_directory()?;
        Ok(rep)
    }

    // ---------------------------------------------------------------- documents

    /// Statements that count: those waiting for review are left out, like their transactions.
    pub fn documents(&self, entity_ids: &[i64]) -> Result<Vec<DocumentRow>> {
        Ok(self.documents_all(entity_ids)?.into_iter().filter(|d| d.review.is_empty()).collect())
    }

    /// Every statement, including those waiting for review (the Statements page, coverage).
    pub fn documents_all(&self, entity_ids: &[i64]) -> Result<Vec<DocumentRow>> {
        let mut st = self.conn.prepare(
            "SELECT d.id, d.account_id, a.name, a.entity_id, COALESCE(bp.short_name, ''), d.filename, d.period_start, d.period_end, d.statement_opening_balance, d.statement_closing_balance,
             d.statement_total_debits, d.statement_total_credits, d.extracted_tx_sum, d.currency, d.reconciliation_status, d.scan_confidence_score, d.imported_at,
             (SELECT COUNT(*) FROM transactions t WHERE t.document_id = d.id), CASE WHEN d.review_hold = 1 THEN d.review_note ELSE '' END
             FROM documents d JOIN accounts a ON a.id = d.account_id LEFT JOIN bank_profiles bp ON bp.id = d.bank_profile_id ORDER BY d.period_end DESC, d.id DESC",
        )?;
        let rows = st.query_map([], |r| {
            let ps: Option<String> = r.get(6)?;
            let pe: Option<String> = r.get(7)?;
            Ok(DocumentRow {
                id: r.get(0)?,
                account_id: r.get(1)?,
                account_name: r.get(2)?,
                entity_id: r.get(3)?,
                bank_short: r.get(4)?,
                filename: r.get(5)?,
                period: ps.zip(pe).map(|(a, b)| (date(&a), date(&b))),
                opening: opt_dec(r.get(8)?),
                closing: opt_dec(r.get(9)?),
                total_debits: opt_dec(r.get(10)?),
                total_credits: opt_dec(r.get(11)?),
                extracted_sum: dec(&r.get::<_, String>(12)?),
                currency: ccy(&r.get::<_, String>(13)?),
                reconciliation: Reconciliation::from_db(&r.get::<_, String>(14)?),
                confidence: r.get(15)?,
                imported_at: r.get(16)?,
                tx_count: r.get(17)?,
                review: Review::parse_list(&r.get::<_, String>(18)?),
            })
        })?;
        let all: Vec<DocumentRow> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(all.into_iter().filter(|d| entity_ids.is_empty() || entity_ids.contains(&d.entity_id)).collect())
    }

    pub fn document_report(&self, id: i64) -> Result<(Diagnostics, SectionReport)> {
        let json: String = self.conn.query_row("SELECT scan_diagnostics_json FROM documents WHERE id = ?1", [id], |r| r.get(0))?;
        #[derive(serde::Deserialize)]
        struct Stored {
            diagnostics: Diagnostics,
            report: SectionReport,
        }
        let s: Stored = serde_json::from_str(&json)?;
        Ok((s.diagnostics, s.report))
    }

    fn save_report(&self, id: i64, diag: &Diagnostics, report: &SectionReport) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Stored<'a> {
            diagnostics: &'a Diagnostics,
            report: &'a SectionReport,
        }
        self.conn.execute("UPDATE documents SET scan_diagnostics_json = ?2 WHERE id = ?1", params![id, serde_json::to_string(&Stored { diagnostics: diag, report })?])?;
        Ok(())
    }

    /// Deletes the whole statement (every section of the same file), its transactions and its kept PDF.
    /// Rows another statement also lists (an overlapping period) stay, with that statement.
    pub fn delete_document(&self, id: i64) -> Result<()> {
        let sha: String = self.conn.query_row("SELECT sha256_hash FROM documents WHERE id = ?1", [id], |r| r.get(0))?;
        let tx = self.sp()?;
        self.drop_documents(&sha)?;
        tx.commit()?;
        self.drop_unused_pdf(&sha)
    }

    /// Every section of one file goes; rows another statement also lists move to it (one that counts first).
    fn drop_documents(&self, sha256: &str) -> Result<()> {
        let moves: Vec<(i64, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT t.id, (SELECT s.document_id FROM shared_rows s JOIN documents o ON o.id = s.document_id WHERE s.transaction_id = t.id AND o.sha256_hash != ?1 ORDER BY o.review_hold, o.id LIMIT 1) AS other
                 FROM transactions t JOIN documents d ON d.id = t.document_id WHERE d.sha256_hash = ?1 AND other IS NOT NULL",
            )?;
            st.query_map([sha256], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (t, other) in moves {
            self.conn.execute("UPDATE transactions SET document_id = ?2 WHERE id = ?1", params![t, other])?;
            self.conn.execute("DELETE FROM shared_rows WHERE document_id = ?2 AND transaction_id = ?1", params![t, other])?;
        }
        self.conn.execute("DELETE FROM documents WHERE sha256_hash = ?1", [sha256])?;
        Ok(())
    }

    /// A statement that counts takes over the rows it lists from statements still waiting for review, so a
    /// held copy (an OCR read of the same month) can't keep them out of view. The held one still lists them.
    fn claim_rows(&self, doc_id: i64) -> Result<()> {
        let rows: Vec<(i64, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT t.id, t.document_id FROM shared_rows s JOIN transactions t ON t.id = s.transaction_id JOIN documents d ON d.id = t.document_id WHERE s.document_id = ?1 AND d.review_hold = 1",
            )?;
            st.query_map([doc_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (t, owner) in rows {
            self.conn.execute("UPDATE transactions SET document_id = ?2 WHERE id = ?1", params![t, doc_id])?;
            self.conn.execute("DELETE FROM shared_rows WHERE document_id = ?2 AND transaction_id = ?1", params![t, doc_id])?;
            self.conn.execute("INSERT OR IGNORE INTO shared_rows (document_id, transaction_id) VALUES (?1, ?2)", params![owner, t])?;
        }
        Ok(())
    }

    /// Every imported statement file: (sha256, filename, workspace). Demo statements are left out.
    pub fn statement_files(&self) -> Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT d.sha256_hash, d.filename, a.entity_id FROM documents d JOIN accounts a ON a.id = d.account_id
             WHERE d.section_index = 0 AND d.sha256_hash NOT LIKE 'demo-%' ORDER BY d.period_end, d.id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Swap a statement for a fresh scan of it: its old sections and transactions go, the new ones
    /// come in through the normal import (rules applied, transfers re-matched). Each section goes back to
    /// the account it was in (matched by account number and currency, not position), and stays approved
    /// if it was, unless it stops adding up.
    pub fn replace_statement(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult) -> Result<ImportReport> {
        // deleted while a Rescan All was running: not brought back
        let old: Vec<(i64, bool, String, String, i64, String)> = {
            let mut st = self.conn.prepare(
                "SELECT d.account_id, d.review_hold, a.account_number_masked, a.currency, d.id, d.reconciliation_status FROM documents d JOIN accounts a ON a.id = d.account_id WHERE d.sha256_hash = ?1 ORDER BY d.section_index",
            )?;
            st.query_map([sha256], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
        };
        if old.is_empty() {
            return Err(Error("that statement is no longer in FinGraph".into()));
        }
        let mut targets = self.plan_import(sha256, scan)?.default_targets();
        let mut approved = Vec::new();
        let mut used = vec![false; old.len()];
        for (si, s) in scan.sections.iter().enumerate() {
            let masked = s.account_last4.as_ref().map(|l| format!("••{l}")).unwrap_or_default();
            let free = |i: usize, o: &(i64, bool, String, String, i64, String)| !used[i] && o.3 == s.currency.code();
            // an unnumbered section you put in an account by hand: the same place in the statement
            let hit = old.iter().enumerate().position(|(i, o)| free(i, o) && o.2 == masked).or_else(|| (s.account_last4.is_none() && old.get(si).is_some_and(|o| free(si, o))).then_some(si));
            if let Some(i) = hit {
                used[i] = true;
                if let Some(t) = targets.get_mut(si) {
                    *t = Target::Existing(old[i].0);
                }
                if !old[i].1 {
                    // approved as it read then: a re-read that finds nothing new stays approved (a download never has balances)
                    let notes = self.document_report(old[i].4).map(|d| d.0.pass3.layout_notes).unwrap_or_default();
                    let same = Reconciliation::from_db(&old[i].5) == s.reconciliation && scan.diagnostics.pass3.layout_notes.iter().all(|n| notes.contains(n));
                    approved.push((si, same));
                }
            }
        }
        // the OCR mark comes back from the scan itself; strip it so the name doesn't grow
        let filename = filename.trim_end_matches(" (read by OCR)");
        self.swap(&[sha256.to_string()], entity_id, filename, sha256, scan, &targets, &approved)
    }

    /// The import's "Replace": the statements in `old` (the same account and period, from another file) go,
    /// this one comes in where `targets` say, with what you changed by hand carried across.
    pub fn replace_with(&self, old: &[i64], entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult, targets: &[Target]) -> Result<ImportReport> {
        let mut shas = Vec::new();
        for id in old {
            let sha: String = self.conn.query_row("SELECT sha256_hash FROM documents WHERE id = ?1", [id], |r| r.get(0))?;
            if !shas.contains(&sha) && sha != sha256 {
                shas.push(sha);
            }
        }
        let rep = self.swap(&shas, entity_id, filename, sha256, scan, targets, &[])?;
        for sha in &shas {
            self.drop_unused_pdf(sha)?;
        }
        Ok(rep)
    }

    /// One transaction: the `old` statements go, `scan` comes in, hand edits are put back. Any error
    /// leaves the ledger as it was.
    #[allow(clippy::too_many_arguments)]
    fn swap(&self, old: &[String], entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult, targets: &[Target], approved: &[(usize, bool)]) -> Result<ImportReport> {
        // what you changed by hand, by transaction fingerprint, so a rescan doesn't lose it
        struct Kept {
            account: i64,
            fitid: String,
            category: Option<i64>,
            payee: String,
            tag: String,
            business: Option<i64>,
            subscription: bool,
            confirmed: bool,
            memo: Option<String>,
            manual: Option<(NaiveDate, String, Decimal, String)>,
            splits: Vec<crate::workflows::SplitLine>,
        }
        // a read that found nothing never replaces the statement you have
        if nothing_read(scan) {
            return Err(Error("the new read found no transactions or balances; the statement was left as it was".into()));
        }
        let mut kept: Vec<Kept> = Vec::new();
        if self.prefs().rescan_keeps_edits {
            let mut st = self.conn.prepare(
                "SELECT t.account_id, t.fitid, t.category_id, t.clean_payee, t.sub_entity_tag, t.business_entity_id, t.is_subscription, t.review_status, t.posted_date, t.raw_description, t.amount, t.memo, t.memo_edited, t.id
                 FROM transactions t JOIN documents d ON d.id = t.document_id WHERE d.sha256_hash = ?1",
            )?;
            for sha in old {
                let rows = st.query_map([sha], |r| {
                    let memo: String = r.get(11)?;
                    Ok((
                        r.get::<_, i64>(13)?,
                        Kept {
                            account: r.get(0)?,
                            fitid: r.get(1)?,
                            category: r.get(2)?,
                            payee: r.get(3)?,
                            tag: r.get(4)?,
                            business: r.get(5)?,
                            subscription: r.get(6)?,
                            confirmed: r.get::<_, String>(7)? == "Confirmed",
                            memo: r.get::<_, bool>(12)?.then(|| memo.clone()),
                            manual: memo.starts_with("Converted from skipped line").then(|| (date(&r.get::<_, String>(8).unwrap_or_default()), r.get(9).unwrap_or_default(), dec(&r.get::<_, String>(10).unwrap_or_default()), memo.clone())),
                            splits: Vec::new(),
                        },
                    ))
                })?;
                for row in rows {
                    let (id, mut k) = row?;
                    k.splits = self.splits(id)?;
                    kept.push(k);
                }
            }
        }
        let sp = self.sp()?;
        for sha in old {
            self.drop_documents(sha)?;
        }
        let rep = self.import_scan_reviewed(entity_id, filename, sha256, scan, targets, approved)?;
        for k in kept {
            let id: Option<i64> = self.conn.query_row("SELECT id FROM transactions WHERE account_id = ?1 AND fitid = ?2", params![k.account, k.fitid], |r| r.get(0)).optional()?;
            let id = match (id, &k.manual) {
                (Some(id), _) => Some(id),
                // a line you converted in the inspector: put it back unless the scanner now reads it itself
                (None, Some((day, desc, amt, memo))) => {
                    let found: i64 = self.conn.query_row(
                        "SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND posted_date = ?2 AND amount_minor = ?3",
                        params![k.account, day.to_string(), to_minor(*amt)],
                        |r| r.get(0),
                    )?;
                    let doc: Option<i64> = self.conn.query_row("SELECT id FROM documents WHERE sha256_hash = ?1 AND account_id = ?2", params![sha256, k.account], |r| r.get(0)).optional()?;
                    if found == 0 { Some(self.add_manual(k.account, doc, *day, desc, *amt, memo)?) } else { None }
                }
                _ => None,
            };
            let Some(id) = id else { continue };
            if k.confirmed {
                self.conn.execute(
                    "UPDATE transactions SET category_id = ?2, clean_payee = ?3, is_subscription = ?4, review_status = 'Confirmed' WHERE id = ?1",
                    params![id, k.category, k.payee, k.subscription],
                )?;
            }
            // "Mark Subscription" is a hand edit even on a row a rule filed
            if k.subscription {
                self.set_subscription(&[id], true)?;
            }
            if !k.tag.is_empty() {
                self.set_tag(&[id], &k.tag)?;
            }
            if let Some(m) = &k.memo {
                self.set_memo(id, m)?;
            }
            if k.business.is_some() {
                self.set_business(&[id], k.business)?;
            }
            if !k.splits.is_empty() {
                self.set_splits(id, &k.splits)?;
            }
        }
        self.recheck_documents(sha256)?;
        sp.commit()?;
        Ok(rep)
    }

    fn recheck_documents(&self, sha256: &str) -> Result<()> {
        let ids: Vec<(i64, String)> = {
            let mut st = self.conn.prepare("SELECT id, currency FROM documents WHERE sha256_hash = ?1")?;
            st.query_map([sha256], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (id, c) in ids {
            // only a statement fixed by hand changes verdict; a clean rescan keeps the scanner's own
            let has_manual: i64 = self.conn.query_row("SELECT COUNT(*) FROM transactions WHERE document_id = ?1 AND memo LIKE 'Converted from skipped line%'", [id], |r| r.get(0))?;
            if has_manual > 0 {
                self.recheck_document(id, ccy(&c))?;
            }
        }
        Ok(())
    }

    /// Remove a workspace and everything in it: accounts, statements, transactions, its rules.
    pub fn delete_entity(&self, id: i64) -> Result<()> {
        let shas: Vec<String> = {
            let mut st = self.conn.prepare("SELECT DISTINCT d.sha256_hash FROM documents d JOIN accounts a ON a.id = d.account_id WHERE a.entity_id = ?1")?;
            st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let tx = self.sp()?;
        for sha in &shas {
            self.drop_documents(sha)?;
        }
        // kept by workspace id without a foreign key
        tx.execute("DELETE FROM manual_values WHERE item_id IN (SELECT id FROM manual_items WHERE entity_id = ?1)", [id])?;
        tx.execute("DELETE FROM manual_items WHERE entity_id = ?1", [id])?;
        tx.execute("DELETE FROM attachments WHERE account_id IN (SELECT id FROM accounts WHERE entity_id = ?1)", [id])?;
        tx.execute("DELETE FROM reimbursements WHERE account_id IN (SELECT id FROM accounts WHERE entity_id = ?1)", [id])?;
        tx.execute("DELETE FROM settings WHERE key LIKE 'apr:%' AND CAST(substr(key, 5) AS INTEGER) IN (SELECT id FROM accounts WHERE entity_id = ?1)", [id])?;
        tx.execute("DELETE FROM entities WHERE id = ?1", [id])?;
        tx.commit()?;
        for sha in &shas {
            self.drop_unused_pdf(sha)?;
        }
        Ok(())
    }

    /// Re-read every statement whose PDF can still be found with the current scanner and profiles.
    /// Returns (statements rescanned, transactions imported, files that couldn't be found).
    pub fn rescan_all(&self) -> Result<(usize, usize, Vec<String>)> {
        let profiles = self.profiles()?;
        let scanner = crate::parser::MultiPassScanner::new(&profiles);
        let (mut n, mut txs, mut missing) = (0, 0, Vec::new());
        for (sha, name, entity) in self.statement_files()? {
            let Some(path) = self.stored_pdf(&sha, &name) else {
                missing.push(name);
                continue;
            };
            let scan = scanner.scan_file(&path)?;
            txs += self.replace_statement(entity, &name, &sha, &scan)?.inserted;
            n += 1;
        }
        Ok((n, txs, missing))
    }

    /// Forget what triage and manual edits taught: rules, categories, payee names and subscription
    /// flags in these workspaces (all of them when empty). Transfers matched between your own
    /// accounts stay matched. Returns how many transactions are back in triage.
    pub fn reset_learning(&self, entity_ids: &[i64]) -> Result<usize> {
        let all: Vec<i64> = self.entities()?.iter().map(|e| e.id).collect();
        let scope: Vec<i64> = if entity_ids.is_empty() { all.clone() } else { entity_ids.to_vec() };
        let everything = all.iter().all(|e| scope.contains(e));
        let rows = self.transactions(&TxFilter { entity_ids: scope.clone(), ..Default::default() })?;
        let tx = self.sp()?;
        for e in &scope {
            tx.execute("DELETE FROM learned_rules WHERE entity_id = ?1", [e])?;
        }
        if everything {
            tx.execute("DELETE FROM learned_rules", [])?;
        }
        let mut n = 0;
        for t in &rows {
            let stem = rules::merchant_stem(&t.raw_description, &t.memo);
            let payee = rules::known_merchant(&stem, &t.raw_description).0;
            if t.transfer_peer.is_some() {
                tx.execute("UPDATE transactions SET clean_payee = ?2, is_subscription = 0 WHERE id = ?1", params![t.id, payee])?;
            } else {
                tx.execute(
                    "UPDATE transactions SET category_id = NULL, clean_payee = ?2, is_subscription = 0, business_entity_id = NULL, review_status = 'NeedsReview' WHERE id = ?1",
                    params![t.id, payee],
                )?;
                // no longer a business expense: nothing owed back for it
                if t.business_entity_id.is_some() {
                    tx.execute("DELETE FROM reimbursements WHERE account_id = ?1 AND fitid = ?2 AND split_seq = -1", params![t.account_id, t.fitid])?;
                }
                n += 1;
            }
        }
        // a row that left a business workspace counts in its own workspace's currency again
        self.revalue(None)?;
        tx.commit()?;
        Ok(n)
    }

    /// The Inspector's 1-click fix: turn a skipped line into a transaction, then re-check the statement.
    pub fn convert_skipped_line(&self, doc_id: i64, line_index: usize, posted: NaiveDate, amount: Decimal, description: &str) -> Result<i64> {
        let (diag, mut report) = self.document_report(doc_id)?;
        if line_index >= report.skipped.len() {
            return Err(Error("that line is no longer listed".into()));
        }
        let line = report.skipped.remove(line_index);
        let (account_id, cur): (i64, String) = self.conn.query_row("SELECT account_id, currency FROM documents WHERE id = ?1", [doc_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let id = self.add_manual(account_id, Some(doc_id), posted, description, amount, &format!("Converted from skipped line: {}", line.text))?;
        report.recovered.push(format!("p{} \u{201c}{}\u{201d} → {amount} (converted by you)", line.page, line.text));
        self.save_report(doc_id, &diag, &report)?;
        self.recheck_document(doc_id, ccy(&cur))?;
        Ok(id)
    }

    /// Re-run the statement equation for a document from what's now in the ledger.
    /// Let a held statement count: its transactions join every view. Approving the first statement of a
    /// kind confirms that bank's layout for that account type, which also releases the others held only
    /// for that reason. Approving a statement held for its layout (new, off the saved one, or changed)
    /// teaches the bank profile what that statement showed (`BankProfile::learn`, undoable), keeps an
    /// anonymised layout sample, and re-reads the bank's other held statements with what was learned.
    pub fn approve_document(&self, doc_id: i64) -> Result<Approval> {
        let (note, profile_id, ty, status, sha, filename): (String, Option<i64>, String, String, String, String) = self.conn.query_row(
            "SELECT d.review_note, a.bank_profile_id, a.account_type, d.reconciliation_status, d.sha256_hash, d.filename FROM documents d JOIN accounts a ON a.id = d.account_id WHERE d.id = ?1",
            [doc_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        let mut out = Approval::default();
        // all of it or none of it: an error part-way leaves the statement held, as it says
        let sp = self.sp()?;
        self.conn.execute("UPDATE documents SET review_hold = 0, review_note = '' WHERE id = ?1", [doc_id])?;
        let why = Review::parse_list(&note);
        let profile = self.profiles()?.into_iter().find(|p| Some(p.id) == profile_id);
        if why.contains(&Review::NewLayout)
            && let Some(mut p) = profile.clone()
        {
            if !p.layout.verified.contains(&ty) {
                p.layout.verified.push(ty.clone());
                self.save_profile(&p)?;
            }
            self.conn.execute(
                "UPDATE documents SET review_hold = 0, review_note = '' WHERE review_hold = 1 AND review_note = ?1
                 AND account_id IN (SELECT id FROM accounts WHERE bank_profile_id = ?2 AND account_type = ?3)",
                params![Review::NewLayout.code(), profile_id, ty],
            )?;
        }
        // a bank nobody had set up: this statement becomes its profile, when page 1 says who the bank is
        let reconciled = Reconciliation::from_db(&status) == Reconciliation::Reconciled;
        let mut profile_id = profile_id;
        if profile_id.is_none() && why.contains(&Review::NewLayout) && reconciled {
            let (diag, _) = self.document_report(doc_id)?;
            let mut p = BankProfile::from_statement(&diag);
            if p.routing_identifiers.is_empty() {
                out.unnamed = true;
            } else {
                let taken: Vec<String> = self.profiles()?.into_iter().map(|p| p.slug).collect();
                let base = p.slug.clone();
                p.slug = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}_{n}") }).find(|s| !taken.contains(s)).unwrap_or(base);
                p.layout.verified = vec![ty.clone()];
                p.id = self.save_profile(&p)?;
                let account: i64 = self.conn.query_row("SELECT account_id FROM documents WHERE id = ?1", [doc_id], |r| r.get(0))?;
                self.adopt_account(account, &p)?;
                out.created = Some(p.short_name.clone());
                profile_id = Some(p.id);
                out.rereads = self.reread_unknown(&p)? + self.reread_held(p.id)?;
            }
        }
        // only a read that adds up is worth learning from
        let layout_hold = why.iter().any(|r| matches!(r, Review::NewLayout | Review::OffLayout | Review::LayoutChanged));
        if layout_hold
            && Reconciliation::from_db(&status) == Reconciliation::Reconciled
            && let Some(before) = self.profiles()?.into_iter().find(|p| Some(p.id) == profile_id)
        {
            let (diag, _) = self.document_report(doc_id)?;
            let mut p = before.clone();
            if p.learn(&diag.observed) {
                self.push_layout_history(&before)?;
                self.save_profile(&p)?;
                out.learned = Some(p.short_name.clone());
                out.rereads = self.reread_held(p.id)?;
            }
            if let Some(dir) = self.layout_samples_dir()
                && let Some(pdf) = self.stored_pdf(&sha, &filename)
                && let Ok(json) = crate::parser::layout_sample_json(&pdf, &self.profiles()?)
                && std::fs::create_dir_all(&dir).is_ok()
            {
                let name = format!("{}-{}.json", p.slug, &sha[..sha.len().min(12)]);
                out.sample = std::fs::write(dir.join(&name), json).ok().map(|_| dir.join(name));
            }
        }
        // statements that count now take their rows back from held copies of them
        let claim: Vec<i64> = {
            let mut st = self.conn.prepare(
                "SELECT DISTINCT s.document_id FROM shared_rows s JOIN documents nd ON nd.id = s.document_id JOIN transactions t ON t.id = s.transaction_id
                 JOIN documents od ON od.id = t.document_id WHERE nd.review_hold = 0 AND od.review_hold = 1",
            )?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for d in claim {
            self.claim_rows(d)?;
        }
        self.match_transfers()?;
        self.mark_card_payments()?;
        self.mark_recurring()?;
        sp.commit()?;
        Ok(out)
    }

    /// The closing balance (statement terms) and file name of the account's statement just before a period
    /// starting `start`: approved, reconciled, and ending at most three days before it starts (or on that day).
    fn previous_closing(&self, account_id: i64, start: NaiveDate) -> Result<Option<(Decimal, String)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT statement_closing_balance, filename FROM documents WHERE account_id = ?1 AND review_hold = 0 AND reconciliation_status = 'Reconciled'
                 AND statement_closing_balance IS NOT NULL AND period_end BETWEEN ?2 AND ?3 ORDER BY period_end DESC, id DESC LIMIT 1",
                params![account_id, (start - chrono::Duration::days(3)).to_string(), (start + chrono::Duration::days(1)).to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(c, f)| (dec(&c), f)))
    }

    /// The layout of the account's last approved statement ending on or before `end` (any, without a period).
    fn previous_layout(&self, account_id: i64, end: Option<NaiveDate>) -> Result<Option<crate::parser::ObservedLayout>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT scan_diagnostics_json FROM documents WHERE account_id = ?1 AND review_hold = 0 AND (?2 IS NULL OR period_end IS NULL OR period_end <= ?2)
                 ORDER BY period_end DESC, id DESC LIMIT 1",
                params![account_id, end.map(|d| d.to_string())],
                |r| r.get(0),
            )
            .optional()?;
        #[derive(serde::Deserialize)]
        struct Stored {
            diagnostics: Diagnostics,
        }
        Ok(json.and_then(|j| serde_json::from_str::<Stored>(&j).ok()).map(|s| s.diagnostics.observed))
    }

    /// Link an account that had no bank to `p`, renaming it from the stand-in `Bank …` to the bank's name.
    fn adopt_account(&self, account: i64, p: &BankProfile) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET bank_profile_id = ?2, name = CASE WHEN name LIKE 'Bank %' THEN ?3 || substr(name, 5) ELSE name END WHERE id = ?1 AND bank_profile_id IS NULL",
            params![account, p.id, p.short_name],
        )?;
        self.conn.execute("UPDATE documents SET bank_profile_id = ?2 WHERE account_id = ?1", params![account, p.id])?;
        Ok(())
    }

    /// Statements held from banks nobody had set up, read again now `p` exists: those that turn out to be
    /// from its bank join it (their accounts too), and the import rules release the ones that read cleanly.
    fn reread_unknown(&self, p: &BankProfile) -> Result<usize> {
        let held: Vec<(String, String, i64, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT d.sha256_hash, d.filename, a.entity_id, a.id FROM documents d JOIN accounts a ON a.id = d.account_id
                 WHERE d.review_hold = 1 AND a.bank_profile_id IS NULL GROUP BY d.sha256_hash",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let profiles = self.profiles()?;
        let scanner = crate::parser::MultiPassScanner::new(&profiles);
        let mut n = 0;
        for (sha, name, entity, account) in held {
            let Some(pdf) = self.stored_pdf(&sha, &name) else { continue };
            let Ok(scan) = scanner.scan_file(&pdf) else { continue };
            if scan.profile_slug.as_deref() != Some(p.slug.as_str()) {
                continue;
            }
            self.adopt_account(account, p)?;
            self.replace_statement(entity, &name, &sha, &scan)?;
            n += 1;
        }
        Ok(n)
    }

    /// Anonymised layout samples of approved statements, next to the ledger (none for an in-memory one).
    pub fn layout_samples_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.path()?.parent()?.join("layout_samples"))
    }

    /// Re-read a bank's other statements held for their layout, now the profile knows more. Those that
    /// read cleanly are released by the import rules themselves. Returns how many were re-read.
    fn reread_held(&self, profile_id: i64) -> Result<usize> {
        let held: Vec<(String, String, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT DISTINCT d.sha256_hash, d.filename, a.entity_id FROM documents d JOIN accounts a ON a.id = d.account_id
                 WHERE d.review_hold = 1 AND a.bank_profile_id = ?1 AND d.review_note LIKE '%layout%'",
            )?;
            st.query_map([profile_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let profiles = self.profiles()?;
        let scanner = crate::parser::MultiPassScanner::new(&profiles);
        let mut n = 0;
        for (sha, name, entity) in held {
            let Some(pdf) = self.stored_pdf(&sha, &name) else { continue };
            let Ok(scan) = scanner.scan_file(&pdf) else { continue };
            self.replace_statement(entity, &name, &sha, &scan)?;
            n += 1;
        }
        Ok(n)
    }

    /// Earlier versions of a bank profile's layout, newest last, kept when approving changed it.
    fn layout_history(&self, profile_id: i64) -> Vec<BankProfile> {
        self.setting(&format!("layout_history:{profile_id}")).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    fn push_layout_history(&self, p: &BankProfile) -> Result<()> {
        let mut h = self.layout_history(p.id);
        h.push(p.clone());
        // ponytail: the last ten layouts per bank; a bank that changes its statements more often than that is unlikely
        let skip = h.len().saturating_sub(10);
        self.set_setting(&format!("layout_history:{}", p.id), &serde_json::to_string(&h[skip..])?)
    }

    /// How many layout changes learned from approvals can be undone for this bank.
    pub fn layout_changes(&self, profile_id: i64) -> usize {
        self.layout_history(profile_id).len()
    }

    /// Put the bank profile back the way it was before the last approval taught it something.
    pub fn undo_layout_change(&self, profile_id: i64) -> Result<bool> {
        let mut h = self.layout_history(profile_id);
        let Some(prev) = h.pop() else { return Ok(false) };
        self.save_profile(&prev)?;
        self.set_setting(&format!("layout_history:{profile_id}"), &serde_json::to_string(&h)?)?;
        Ok(true)
    }

    pub fn recheck_document(&self, doc_id: i64, _currency: Currency) -> Result<Reconciliation> {
        let (open, close, ty): (Option<String>, Option<String>, String) = self.conn.query_row(
            "SELECT d.statement_opening_balance, d.statement_closing_balance, a.account_type FROM documents d JOIN accounts a ON a.id = d.account_id WHERE d.id = ?1",
            [doc_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let amounts: Vec<Decimal> = {
            let mut st = self.conn.prepare("SELECT amount FROM transactions WHERE document_id = ?1")?;
            st.query_map([doc_id], |r| r.get::<_, String>(0))?.filter_map(|s| s.ok()).map(|s| dec(&s)).collect()
        };
        let sum: Decimal = amounts.iter().copied().sum();
        let liability = AccountType::parse(&ty).is_liability();
        let stmt_sum = if liability { -sum } else { sum };
        let status = match (opt_dec(open), opt_dec(close)) {
            (Some(o), Some(c)) if c - (o + stmt_sum) == Decimal::ZERO => Reconciliation::Reconciled,
            (Some(o), Some(c)) => Reconciliation::Discrepancy(c - (o + stmt_sum)),
            _ => Reconciliation::Unverified,
        };
        self.conn.execute(
            "UPDATE documents SET reconciliation_status = ?2, extracted_tx_sum = ?3, extracted_tx_sum_minor = ?4, scan_confidence_score = CASE WHEN ?2 = 'Reconciled' THEN 1.0 ELSE scan_confidence_score END WHERE id = ?1",
            params![doc_id, status.to_db(), sum.to_string(), to_minor(sum)],
        )?;
        // fixed by hand so it adds up now: that reason for holding it is gone
        if status == Reconciliation::Reconciled {
            let note: String = self.conn.query_row("SELECT review_note FROM documents WHERE id = ?1", [doc_id], |r| r.get(0))?;
            let left: Vec<Review> = Review::parse_list(&note).into_iter().filter(|r| !matches!(r, Review::Mismatch | Review::NoBalances)).collect();
            let was_held: bool = self.conn.query_row("SELECT review_hold FROM documents WHERE id = ?1", [doc_id], |r| r.get(0))?;
            self.conn.execute("UPDATE documents SET review_note = ?2, review_hold = review_hold AND ?3 WHERE id = ?1", params![doc_id, Review::join(&left), !left.is_empty()])?;
            // its rows just joined the ledger: pair its transfers and recurring charges like an import does
            if was_held && left.is_empty() {
                self.match_transfers()?;
                self.mark_card_payments()?;
                self.mark_recurring()?;
            }
        }
        Ok(status)
    }

    // ---------------------------------------------------------------- transactions

    pub fn transactions(&self, f: &TxFilter) -> Result<Vec<TxRow>> {
        if f.split_lines {
            return self.with_split_lines(f);
        }
        // a statement waiting for review counts nowhere until it's approved
        let mut sql = format!("{TX_SELECT} WHERE (t.document_id IS NULL OR t.document_id NOT IN (SELECT id FROM documents WHERE review_hold = 1))");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !f.entity_ids.is_empty() {
            let col = if f.by_account { "a.entity_id" } else { "COALESCE(t.business_entity_id, a.entity_id)" };
            sql += &format!(" AND {col} IN ({})", f.entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(","));
            args.extend(f.entity_ids.iter().map(|&i| i.into()));
        }
        if let Some(d) = f.from {
            sql += " AND t.posted_date >= ?";
            args.push(d.to_string().into());
        }
        if let Some(d) = f.to {
            sql += " AND t.posted_date <= ?";
            args.push(d.to_string().into());
        }
        if let Some(a) = f.account_id {
            sql += " AND t.account_id = ?";
            args.push(a.into());
        }
        if let Some(c) = f.category_id {
            sql += " AND (t.category_id = ? OR c.parent_id = ?)";
            args.push(c.into());
            args.push(c.into());
        }
        if f.uncategorized_only {
            sql += " AND t.category_id IS NULL AND t.transfer_peer_tx_id IS NULL";
        }
        if !f.search.trim().is_empty() {
            sql += " AND (t.raw_description LIKE ? OR t.clean_payee LIKE ? OR t.memo LIKE ? OR t.sub_entity_tag LIKE ? OR t.amount LIKE ?)";
            let like = format!("%{}%", f.search.trim());
            for _ in 0..5 {
                args.push(like.clone().into());
            }
        }
        sql += " ORDER BY t.posted_date DESC, t.id DESC";
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args), tx_row)?;
        let fx = self.fx()?;
        let mut rows: Vec<TxRow> = rows.collect::<rusqlite::Result<_>>()?;
        for t in &mut rows { t.amount_entity = revalidated_entity_amount(&fx, t, crate::money::statement_fx_wins()); }
        Ok(rows)
    }

    pub fn transaction(&self, id: i64) -> Result<TxRow> {
        let mut t = self.conn.query_row(&format!("{TX_SELECT} WHERE t.id = ?1"), [id], tx_row)?;
        t.amount_entity = revalidated_entity_amount(&self.fx()?, &t, crate::money::statement_fx_wins());
        Ok(t)
    }

    pub fn set_category(&self, ids: &[i64], category_id: Option<i64>) -> Result<()> {
        let tx = self.sp()?;
        for id in ids {
            tx.execute("UPDATE transactions SET category_id = ?2, review_status = 'Confirmed' WHERE id = ?1", params![id, category_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_payee(&self, id: i64, payee: &str) -> Result<()> {
        self.conn.execute("UPDATE transactions SET clean_payee = ?2, review_status = 'Confirmed' WHERE id = ?1", params![id, payee])?;
        Ok(())
    }

    pub fn set_tag(&self, ids: &[i64], tag: &str) -> Result<()> {
        for id in ids {
            self.conn.execute("UPDATE transactions SET sub_entity_tag = ?2 WHERE id = ?1", params![id, tag.trim()])?;
        }
        Ok(())
    }

    pub fn set_memo(&self, id: i64, memo: &str) -> Result<()> {
        self.conn.execute("UPDATE transactions SET memo = ?2, memo_edited = 1 WHERE id = ?1", params![id, memo])?;
        Ok(())
    }

    pub fn set_subscription(&self, ids: &[i64], on: bool) -> Result<()> {
        for id in ids {
            self.conn.execute("UPDATE transactions SET is_subscription = ?2, is_recurring = is_recurring OR ?2 WHERE id = ?1", params![id, on])?;
        }
        Ok(())
    }

    /// Book these transactions to a business workspace (`None` = back to their own account's).
    /// The workspace amount is recalculated in case the business keeps a different currency.
    pub fn set_business(&self, ids: &[i64], business: Option<i64>) -> Result<()> {
        let fx = self.fx()?;
        let tx = self.sp()?;
        for id in ids {
            let (amt, cur, memo, home, day): (String, String, String, i64, String) = tx.query_row(
                "SELECT t.amount, t.currency, t.memo, a.entity_id, t.posted_date FROM transactions t JOIN accounts a ON a.id = t.account_id WHERE t.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            let target = business.filter(|b| *b != home);
            let base: String = tx.query_row("SELECT base_currency FROM entities WHERE id = ?1", [target.unwrap_or(home)], |r| r.get(0))?;
            let v = entity_amount(&fx, dec(&amt), ccy(&cur), ccy(&base), parse_fx_memo(&memo), date(&day));
            tx.execute(
                "UPDATE transactions SET business_entity_id = ?2, amount_in_entity_currency = ?3, amount_in_entity_minor = ?4 WHERE id = ?1",
                params![id, target, v.map(|v| v.to_string()).unwrap_or_default(), v.map(to_minor).unwrap_or_default()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_manual(&self, account_id: i64, doc_id: Option<i64>, posted: NaiveDate, description: &str, amount: Decimal, memo: &str) -> Result<i64> {
        let (cur, base): (String, String) = self.conn.query_row("SELECT a.currency, e.base_currency FROM accounts a JOIN entities e ON e.id = a.entity_id WHERE a.id = ?1", [account_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let fx = self.fx()?;
        let ent = entity_amount(&fx, amount, ccy(&cur), ccy(&base), None, posted);
        let stem = rules::merchant_stem(description, "");
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND posted_date = ?2", params![account_id, posted.to_string()], |r| r.get(0))?;
        // the first free number: after a delete the count goes down, the fingerprints already used don't
        let mut seq = 1000 + n as usize;
        while self.conn.query_row("SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND fitid = ?2", params![account_id, fitid(account_id, posted, amount, seq)], |r| r.get::<_, i64>(0))? > 0 {
            seq += 1;
        }
        self.conn.execute(
            "INSERT INTO transactions (document_id, account_id, posted_date, raw_description, clean_payee, amount, amount_minor, currency, amount_in_entity_currency, amount_in_entity_minor, memo, fitid, review_status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'Confirmed')",
            params![
                doc_id,
                account_id,
                posted.to_string(),
                description,
                rules::known_merchant(&stem, description).0,
                amount.to_string(),
                to_minor(amount),
                cur,
                ent.map(|v| v.to_string()).unwrap_or_default(),
                ent.map(to_minor).unwrap_or_default(),
                memo,
                fitid(account_id, posted, amount, seq)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Deleted for good: a rescan or another copy of the statement doesn't bring them back.
    pub fn delete_transactions(&self, ids: &[i64]) -> Result<()> {
        let tx = self.sp()?;
        for id in ids {
            tx.execute("INSERT OR IGNORE INTO deleted_rows (account_id, fitid) SELECT account_id, fitid FROM transactions WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM transactions WHERE id = ?1", [id])?;
        }
        tx.commit()
    }

    // ---------------------------------------------------------------- categories & rules

    pub fn categories(&self) -> Result<Vec<Category>> {
        // A–Z by top-level category, each followed by its own sub-categories A–Z
        let mut st = self.conn.prepare(
            "SELECT c.id, c.name, c.parent_id, c.kind, c.icon, c.color FROM categories c LEFT JOIN categories p ON p.id = c.parent_id
             ORDER BY COALESCE(p.name, c.name) COLLATE NOCASE, COALESCE(p.id, c.id), c.parent_id IS NOT NULL, c.name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| Ok(Category { id: r.get(0)?, name: r.get(1)?, parent_id: r.get(2)?, kind: cat_kind(&r.get::<_, String>(3)?), icon: r.get(4)?, color: r.get(5)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_category_look(&self, id: i64, icon: Option<&str>, color: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE categories SET icon = ?2, color = ?3 WHERE id = ?1", params![id, icon, color])?;
        Ok(())
    }

    pub fn category_id(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM categories WHERE name = ?1 ORDER BY parent_id IS NOT NULL LIMIT 1", [name], |r| r.get(0)).optional()?)
    }

    /// The categories you use most for money going out (or coming in), busiest first: what the picker
    /// offers before you type. Transfers aren't a choice anyone needs a shortcut to.
    pub fn top_categories(&self, entity_ids: &[i64], outgoing: bool, limit: usize) -> Result<Vec<i64>> {
        let ents = entity_ids.iter().map(|e| format!(",{e},")).collect::<String>();
        let mut st = self.conn.prepare(
            "SELECT t.category_id FROM transactions t JOIN accounts a ON a.id = t.account_id JOIN categories c ON c.id = t.category_id
             WHERE c.kind <> 'Transfer' AND (t.amount_minor < 0) = ?1 AND (?2 = '' OR instr(?2, ',' || a.entity_id || ',') > 0)
             GROUP BY t.category_id ORDER BY COUNT(*) DESC, t.category_id LIMIT ?3",
        )?;
        let ids = st.query_map(params![outgoing, ents, limit as i64], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// The last categories picked by hand, newest first (the picker's Recent).
    pub fn recent_categories(&self) -> Vec<i64> {
        self.setting("recent_categories").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    pub fn note_recent_category(&self, id: i64) -> Result<()> {
        let mut r = self.recent_categories();
        r.retain(|x| *x != id);
        r.insert(0, id);
        r.truncate(6);
        self.set_setting("recent_categories", &serde_json::to_string(&r)?)
    }

    pub fn add_category(&self, name: &str, parent_id: Option<i64>, kind: CategoryKind) -> Result<i64> {
        let taken: i64 = self.conn.query_row("SELECT COUNT(*) FROM categories WHERE name = ?1 COLLATE NOCASE AND parent_id IS ?2", params![name.trim(), parent_id], |r| r.get(0))?;
        if taken > 0 {
            return Err(Error(format!("“{name}” already exists")));
        }
        let k = match kind {
            CategoryKind::Income => "Income",
            CategoryKind::Expense => "Expense",
            CategoryKind::Transfer => "Transfer",
        };
        self.conn.execute("INSERT INTO categories (name, parent_id, kind) VALUES (?1, ?2, ?3)", params![name, parent_id, k])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        let mut st = self.conn.prepare("SELECT id, kind, pattern, clean_payee, category_id, is_subscription, is_transfer, expected_cadence, entity_id, business_entity_id, amount_min, amount_max FROM learned_rules ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(Rule {
                id: r.get(0)?,
                kind: RuleKind::parse(&r.get::<_, String>(1)?),
                pattern: r.get(2)?,
                clean_payee: r.get(3)?,
                category_id: r.get(4)?,
                is_subscription: r.get(5)?,
                is_transfer: r.get(6)?,
                expected_cadence: r.get(7)?,
                entity_id: r.get(8)?,
                business_entity_id: r.get(9)?,
                amount_min: opt_dec(r.get(10)?),
                amount_max: opt_dec(r.get(11)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Save a rule and apply it to every matching transaction already in the ledger
    /// (except ones the user confirmed by hand). Returns how many rows changed.
    pub fn learn(&self, rule: &Rule) -> Result<usize> {
        self.conn.execute(
            "INSERT INTO learned_rules (kind, pattern, clean_payee, category_id, is_subscription, is_transfer, expected_cadence, entity_id, created_at, business_entity_id, amount_min, amount_max) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![rule.kind.as_str(), rule.pattern, rule.clean_payee, rule.category_id, rule.is_subscription, rule.is_transfer, rule.expected_cadence, rule.entity_id, now(), rule.business_entity_id, rule.amount_min.map(|d| d.to_string()), rule.amount_max.map(|d| d.to_string())],
        )?;
        let mut saved = rule.clone();
        saved.id = self.conn.last_insert_rowid();
        self.apply_rule(&saved, false)
    }

    /// `force`: also overwrite rows marked Confirmed (the wizard's answers are confirmations).
    pub fn apply_rule(&self, rule: &Rule, force: bool) -> Result<usize> {
        // a workspace rule covers what that workspace's accounts paid, wherever it was booked
        let rows: Vec<TxRow> = self.transactions(&TxFilter::default())?.into_iter().filter(|t| rule.entity_id.is_none_or(|e| e == t.account_entity_id || e == t.entity_id)).collect();
        let tx = self.sp()?;
        let mut n = 0;
        for t in rows.iter().filter(|t| rule.applies(&t.raw_description, &t.memo, t.amount)) {
            if t.review == ReviewStatus::Confirmed && !force && t.category_id.is_some() {
                continue;
            }
            tx.execute(
                "UPDATE transactions SET category_id = COALESCE(?2, category_id), clean_payee = COALESCE(?3, clean_payee), is_subscription = is_subscription OR ?4, is_recurring = is_recurring OR ?4,
                 review_status = ?5 WHERE id = ?1",
                params![t.id, rule.category_id, rule.clean_payee, rule.is_subscription, if force { "Confirmed" } else { "AutoCategorized" }],
            )?;
            n += 1;
        }
        tx.commit()?;
        if let Some(b) = rule.business_entity_id {
            let ids: Vec<i64> = rows.iter().filter(|t| rule.applies(&t.raw_description, &t.memo, t.amount) && t.account_entity_id != b).map(|t| t.id).collect();
            self.set_business(&ids, Some(b))?;
            n = n.max(ids.len());
        }
        if rule.is_transfer {
            self.match_transfers()?;
        }
        Ok(n)
    }

/// Run every learned rule again (oldest first, so newer answers win), never touching rows you
    /// confirmed by hand. Returns how many rows changed.
    pub fn reapply_rules(&self) -> Result<usize> {
        let mut n = 0;
        for r in self.rules()? {
            n += self.apply_rule(&r, false)?;
        }
        self.match_transfers()?;
        Ok(n + self.mark_card_payments()? + self.apply_directory()?)
    }

    /// File uncategorized charges from businesses in the merchant directory (and, if allowed in
    /// Preferences, keyword guesses). Your rules always run first; a transaction you deliberately set
    /// to Uncategorized is left alone. Only money going out: a deposit from a business in the
    /// directory is usually pay or a refund (an airline's payroll is not a flight), so it's asked
    /// about instead. Returns how many were filed.
    pub fn apply_directory(&self) -> Result<usize> {
        let p = self.prefs();
        if !p.auto_directory {
            return Ok(0);
        }
        let mut ids: std::collections::HashMap<String, i64> = Default::default();
        for c in self.categories()? {
            // a top-level category wins a name clash with a sub-category
            if c.parent_id.is_none() || !ids.contains_key(&c.name) {
                ids.insert(c.name.clone(), c.id);
            }
        }
        let rows = self.transactions(&TxFilter { uncategorized_only: true, ..Default::default() })?;
        let tx = self.sp()?;
        let mut n = 0;
        for t in rows.iter().filter(|t| t.review != ReviewStatus::Confirmed && t.amount.is_sign_negative()) {
            let Some((cat, name)) = rules::auto_category(&t.raw_description, &t.memo, p.auto_keywords) else { continue };
            let Some(&id) = ids.get(cat) else { continue };
            tx.execute(
                "UPDATE transactions SET category_id = ?2, clean_payee = COALESCE(?3, clean_payee), is_subscription = is_subscription OR ?4, review_status = 'AutoCategorized' WHERE id = ?1",
                params![t.id, id, name, cat == "Subscriptions & Digital"],
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM learned_rules WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------------------------------------------------------------- transfers & recurrence

    /// Pair opposite movements between two of your accounts: same currency and exact amount, or
    /// across KYD/USD within 2% of the peg; within 4 days; at least one side worded like a transfer.
    pub fn match_transfers(&self) -> Result<usize> {
        let fx = self.fx()?;
        let rows: Vec<TxRow> = self.transactions(&TxFilter::default())?.into_iter().filter(|t| t.transfer_peer.is_none()).collect();
        let transfer_cat = self.category_id("Transfers")?;
        // "Transfer to 9063": the account whose last four digits the description names is the likely other side
        let last4: std::collections::HashMap<i64, String> = self.accounts()?.into_iter().map(|a| (a.id, a.account_number_masked.trim_start_matches('•').to_string())).collect();
        let names = |a: &TxRow, b: &TxRow| last4.get(&b.account_id).is_some_and(|l| l.len() == 4 && names_account(&crate::merchants::tokens(&format!("{} {}", a.raw_description, a.memo)), l));
        let apart: std::collections::HashSet<(i64, String, i64, String)> = {
            let mut st = self.conn.prepare("SELECT account_a, fitid_a, account_b, fitid_b FROM unpaired")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut used = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for a in rows.iter().filter(|t| t.amount.is_sign_negative()) {
            if used.contains(&a.id) {
                continue;
            }
            let best = rows
                .iter()
                .filter(|b| b.amount.is_sign_positive() && b.account_id != a.account_id && !used.contains(&b.id))
                .filter(|b| !apart.contains(&(a.account_id, a.fitid.clone(), b.account_id, b.fitid.clone())))
                .filter(|b| (b.posted - a.posted).num_days().abs() <= 5)
                .filter(|b| rules::looks_like_transfer(&a.raw_description, &a.memo) || rules::looks_like_transfer(&b.raw_description, &b.memo) || names(a, b))
                .filter(|b| {
                    if a.currency == b.currency {
                        a.amount.abs() == b.amount
                    } else {
                        let Some(conv) = fx.convert(a.amount.abs(), a.currency, b.currency) else { return false };
                        (conv - b.amount).abs() <= conv * Decimal::new(2, 2)
                    }
                })
                .min_by_key(|b| (!names(a, b), (b.posted - a.posted).num_days().abs(), (b.amount - a.amount.abs()).abs()));
            if let Some(b) = best {
                used.insert(a.id);
                used.insert(b.id);
                pairs.push((a.id, b.id));
            }
        }
        let tx = self.sp()?;
        for (a, b) in &pairs {
            for (x, y) in [(a, b), (b, a)] {
                tx.execute(
                    "UPDATE transactions SET transfer_peer_tx_id = ?2, category_id = COALESCE(category_id, ?3), review_status = CASE WHEN review_status = 'NeedsReview' THEN 'AutoCategorized' ELSE review_status END WHERE id = ?1",
                    params![x, y, transfer_cat],
                )?;
            }
        }
        tx.commit()?;
        Ok(pairs.len())
    }

    /// Recognise payments to your credit cards: money out of a bank account whose description names
    /// one of your cards' last four digits ("Transfer to 9063") or says it pays a card ("VISA PMT",
    /// "CARD PAYMENT"), and payments received on a card account. Filed under Transfers › Credit Card
    /// Payments, so they're never spending, even when the card's own statement isn't imported.
    /// A transaction you filed as something else by hand is left alone. Returns how many changed.
    pub fn mark_card_payments(&self) -> Result<usize> {
        let Some(cat) = self.category_id("Credit Card Payments")? else { return Ok(0) };
        let transfers = self.category_id("Transfers")?;
        let accts = self.accounts()?;
        let cards: Vec<(i64, String)> = accts
            .iter()
            .filter(|a| a.account_type == AccountType::CreditCard)
            .map(|a| (a.id, a.account_number_masked.trim_start_matches('•').to_string()))
            .filter(|(_, l)| l.len() == 4)
            .collect();
        let is_card = |id: i64| accts.iter().any(|a| a.id == id && a.account_type == AccountType::CreditCard);
        let rows = self.transactions(&TxFilter::default())?;
        let tx = self.sp()?;
        let mut n = 0;
        for t in &rows {
            if t.review == ReviewStatus::Confirmed && t.category_id.is_some() && t.category_id != transfers && t.category_id != Some(cat) {
                continue;
            }
            let text = format!("{} {}", t.raw_description, t.memo);
            let words = crate::merchants::tokens(&text);
            let pays_card = if is_card(t.account_id) {
                t.amount.is_sign_positive() && (rules::is_card_payment(&text) || t.transfer_peer.is_some_and(|p| rows.iter().any(|o| o.id == p && !is_card(o.account_id))))
            } else {
                t.amount.is_sign_negative()
                    && (rules::is_card_payment(&text) || (rules::looks_like_transfer(&t.raw_description, &t.memo) && cards.iter().any(|(id, l4)| *id != t.account_id && names_account(&words, l4))))
            };
            if !pays_card {
                continue;
            }
            // name the card it paid, unless you renamed the payee yourself
            let card = if is_card(t.account_id) {
                None
            } else {
                cards.iter().find(|(id, l4)| *id != t.account_id && names_account(&words, l4)).and_then(|(id, _)| accts.iter().find(|a| a.id == *id))
            };
            let lower = t.clean_payee.to_lowercase();
            let default_payee = lower.is_empty() || ["transfer", "pc transfer", "internet", "mobile ob"].iter().any(|p| lower.starts_with(p));
            let payee = match card {
                Some(c) if default_payee => Some(format!("Payment to {}", c.name)),
                None if default_payee && is_card(t.account_id) => Some("Card payment received".to_string()),
                _ => None,
            };
            if t.category_id != Some(cat) {
                n += 1;
            }
            tx.execute(
                "UPDATE transactions SET category_id = ?2, clean_payee = COALESCE(?3, clean_payee), review_status = CASE WHEN review_status = 'Confirmed' THEN 'Confirmed' ELSE 'AutoCategorized' END WHERE id = ?1",
                params![t.id, cat, payee],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Not a transfer after all: the pair stays apart (the next import doesn't pair it again), and loses the
    /// Transfers category the pairing gave it.
    pub fn unlink_transfer(&self, id: i64) -> Result<()> {
        let t = self.transaction(id)?;
        let Some(peer) = t.transfer_peer else { return Ok(()) };
        let p = self.transaction(peer)?;
        let tx = self.sp()?;
        tx.execute("INSERT OR IGNORE INTO unpaired VALUES (?1, ?2, ?3, ?4), (?3, ?4, ?1, ?2)", params![t.account_id, t.fitid, p.account_id, p.fitid])?;
        let transfers = self.category_id("Transfers")?;
        for r in [&t, &p] {
            tx.execute("UPDATE transactions SET transfer_peer_tx_id = NULL WHERE id = ?1", [r.id])?;
            if r.category_id.is_some() && r.category_id == transfers {
                tx.execute("UPDATE transactions SET category_id = NULL, review_status = 'NeedsReview' WHERE id = ?1", [r.id])?;
            }
        }
        tx.commit()
    }

    /// Flag merchants charged on a steady cadence as recurring.
    pub fn mark_recurring(&self) -> Result<()> {
        let rows = self.transactions(&TxFilter::default())?;
        let mut by: std::collections::HashMap<(i64, String), Vec<&TxRow>> = Default::default();
        for t in rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer()) {
            by.entry((t.account_id, rules::merchant_stem(&t.raw_description, &t.memo))).or_default().push(t);
        }
        let tx = self.sp()?;
        for v in by.values().filter(|v| v.len() >= 3) {
            let dates: Vec<NaiveDate> = v.iter().map(|t| t.posted).collect();
            if rules::cadence(&dates).is_some() {
                for t in v {
                    tx.execute("UPDATE transactions SET is_recurring = 1 WHERE id = ?1", [t.id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- triage

    pub fn uncategorized_count(&self, entity_ids: &[i64]) -> Result<usize> {
        Ok(self.transactions(&TxFilter { entity_ids: entity_ids.to_vec(), uncategorized_only: true, ..Default::default() })?.len())
    }

    fn triage_rows(&self, entity_ids: &[i64]) -> Result<(Vec<TxRow>, Vec<rules::TriageGroup>)> {
        let rows = self.transactions(&TxFilter { entity_ids: entity_ids.to_vec(), uncategorized_only: true, ..Default::default() })?;
        let items: Vec<rules::TriageInput> = rows
            .iter()
            .map(|t| rules::TriageInput { id: t.id, entity_id: t.entity_id, posted: t.posted, raw: t.raw_description.clone(), memo: t.memo.clone(), amount: t.amount, currency: t.currency, account: t.account_name.clone() })
            .collect();
        let groups = rules::triage_groups(&items);
        Ok((rows, groups))
    }

    /// How many groups To Review shows, without working out the guesses.
    pub fn triage_count(&self, entity_ids: &[i64]) -> Result<usize> {
        Ok(self.triage_rows(entity_ids)?.1.len())
    }

    pub fn triage(&self, entity_ids: &[i64]) -> Result<Vec<rules::TriageGroup>> {
        let (rows, mut groups) = self.triage_rows(entity_ids)?;
        // what your own categorising suggests, from the group's latest charge
        let known = self.transactions(&TxFilter { entity_ids: entity_ids.to_vec(), ..Default::default() })?;
        if let Some(model) = crate::learn::Model::train(&known) {
            let cats = self.categories()?;
            for g in &mut groups {
                let Some(t) = g.charges.first() else { continue };
                let acct = rows.iter().find(|r| r.id == t.id).map_or(0, |r| r.account_id);
                g.guesses = model
                    .guess(&crate::learn::features(&t.raw, &t.memo, t.amount, t.posted, acct))
                    .into_iter()
                    .filter(|(_, p)| *p >= 0.15)
                    .filter_map(|(id, _)| cats.iter().find(|c| c.id == id).map(|c| c.name.clone()))
                    .filter(|n| Some(n.as_str()) != g.suggested_category && n != "Uncategorized")
                    .take(2)
                    .collect();
            }
        }
        if !self.prefs().ask_subscriptions {
            for g in groups.iter_mut().filter(|g| g.kind == rules::TriageKind::Subscription) {
                g.kind = rules::TriageKind::Merchant;
            }
        }
        Ok(groups)
    }

    /// Mark a triage group as internal transfers: Transfers category, then pair them up.
    pub fn mark_transfers(&self, ids: &[i64], stem: &str, entity_id: Option<i64>) -> Result<usize> {
        let cat = self.category_id("Transfers")?;
        self.set_category(ids, cat)?;
        let rule = Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: stem.into(), clean_payee: None, category_id: cat, is_subscription: false, is_transfer: true, expected_cadence: None, entity_id, business_entity_id: None, ..Default::default() };
        self.learn(&rule)?;
        self.match_transfers()
    }

    // ---------------------------------------------------------------- demo

    /// Load the demo year: creates "Personal" and "Business — Property" workspaces if missing.
    pub fn load_demo(&self) -> Result<usize> {
        let personal = match self.entity_by_name("Personal")? {
            Some(id) => id,
            None => self.create_entity("Personal", EntityKind::Personal, Currency::KYD)?,
        };
        let business = match self.entity_by_name("Business — Property")? {
            Some(id) => id,
            None => self.create_entity("Business — Property", EntityKind::Business, Currency::KYD)?,
        };
        let profiles = self.profiles()?;
        let scanner = crate::parser::MultiPassScanner::new(&profiles);
        let mut n = 0;
        for doc in crate::demo::demo_documents() {
            let scan = scanner.scan_tokens(doc.tokens)?;
            let mut h = Sha256::new();
            h.update(doc.filename.as_bytes());
            let sha: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            let ent = if doc.workspace == "Personal" { personal } else { business };
            // simulated statements: no new layout or OCR to confirm, only a mismatch holds one back
            let sha = format!("demo-{sha}");
            let targets = self.plan_import(&sha, &scan)?.default_targets();
            let all: Vec<(usize, bool)> = (0..scan.sections.len()).map(|i| (i, false)).collect();
            n += self.import_scan_reviewed(ent, &doc.filename, &sha, &scan, &targets, &all)?.inserted;
        }
        // a few answers already learned, so triage starts with the interesting merchants
        for (pattern, cat, payee) in [("SALARY", "Salary", "Harbour Freight (Salary)"), ("RENT RECEIVED", "Rental Income", "Rent"), ("SERVICE CHARGE", "Bank Fees & Interest", "Scotiabank"), ("MONTHLY ACCOUNT FEE", "Bank Fees & Interest", "CNB"), ("MONTHLY FEE", "Bank Fees & Interest", "RBC"), ("FOREIGN TRANSACTION FEE", "Bank Fees & Interest", "Butterfield"), ("LOAN INTEREST", "Bank Fees & Interest", "CICSA Loan"), ("STAMP DUTY", "Government & Fees", "Stamp Duty"), ("HOME LOAN", "Housing", "Home Loan")] {
            let category_id = self.category_id(cat)?;
            self.learn(&Rule { id: 0, kind: RuleKind::Substring, pattern: pattern.into(), clean_payee: Some(payee.into()), category_id, is_subscription: false, is_transfer: false, expected_cadence: None, entity_id: None, business_entity_id: None, ..Default::default() })?;
        }
        for (pattern, tag) in [("UNIT A", "Unit A"), ("UNIT B", "Unit B")] {
            let ids: Vec<i64> = self.transactions(&TxFilter { entity_ids: vec![business], search: pattern.into(), ..Default::default() })?.iter().map(|t| t.id).collect();
            self.set_tag(&ids, tag)?;
        }
        Ok(n)
    }
}

/// The description names an account: its last four digits on their own ("Transfer to 9063") or as the
/// end of a full card/account number ("Transfer to 20416 38817204418").
pub(crate) fn names_account(words: &[String], last4: &str) -> bool {
    words.iter().any(|w| w == last4 || (w.len() >= 8 && w.ends_with(last4) && w.bytes().all(|b| b.is_ascii_digit())))
}

/// `USD 145.00` style original from an FX memo line we stored.
fn parse_fx_memo(memo: &str) -> Option<(Currency, Decimal)> {
    let i = memo.find("FX original ")?;
    let rest = &memo[i + 12..];
    let mut it = rest.split_whitespace();
    Some((Currency::parse(it.next()?)?, Decimal::from_str(it.next()?).ok()?))
}

// Older ledgers can have a valid statement-quoted amount without a machine-readable memo.
// Preserve it only when conversion availability can be established; unknown currencies never
// inherit the old 1:1 fallback. New imports always store the explicit FX original marker.
fn revalidated_entity_amount(fx: &Fx, t: &TxRow, prefer_statement: bool) -> Option<Decimal> {
    let original = parse_fx_memo(&t.memo);
    let available = entity_amount(fx, t.amount, t.currency, t.entity_currency, original, t.posted);
    if prefer_statement && original.is_none() { available.and(t.amount_entity.or(available)) } else { available }
}

/// The amount in the workspace's currency: as printed if it already is; the statement's own FX
/// original if that's in the workspace currency; else the configured rate.
/// `amount` (in `from`) in the workspace's `base` currency, at `day`'s rate — or the statement's own conversion
/// (`original`) when it printed one and that's preferred.
pub fn entity_amount(fx: &Fx, amount: Decimal, from: Currency, base: Currency, original: Option<(Currency, Decimal)>, day: NaiveDate) -> Option<Decimal> {
    if from == base {
        return Some(amount);
    }
    let original = original.filter(|_| crate::money::statement_fx_wins());
    match original {
        Some((c, v)) if c == base => {
            if amount.is_sign_negative() {
                Some(-v.abs())
            } else {
                Some(v.abs())
            }
        }
        _ => fx.convert_on(amount, from, base, Some(day)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_quoted_amounts_survive_but_old_unknown_one_for_one_values_do_not() {
        let mut t = crate::insights::tests::row(1, "2026-01-01".parse().unwrap(), "Purchase", "-120", "Shopping", "");
        t.currency = Currency::USD; t.amount_entity = Some(dec("-99.75"));
        assert_eq!(revalidated_entity_amount(&Fx::default(), &t, true), Some(dec("-99.75")));
        assert_eq!(revalidated_entity_amount(&Fx::default(), &t, false), Some(dec("-100")));
        t.currency = Currency::parse("GBP").unwrap(); t.amount_entity = Some(t.amount);
        assert_eq!(revalidated_entity_amount(&Fx::default(), &t, true), None);
    }

    #[test]
    fn imports_ask_about_new_accounts_hold_new_layouts_and_never_double_up() {
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let profiles = db.profiles().unwrap();
        let docs = crate::demo::demo_documents();
        let cards: Vec<ScanResult> = docs
            .iter()
            .filter(|d| d.filename.starts_with("Butterfield_Visa"))
            .take(2)
            .map(|d| crate::parser::MultiPassScanner::new(&profiles).scan_tokens(d.tokens.clone()).unwrap())
            .collect();
        let rows = |db: &Db| db.transactions(&TxFilter::default()).unwrap().len();

        // a card nobody has imported before: nothing happens without asking
        let plan = db.plan_import("h1", &cards[0]).unwrap();
        assert!(plan.automatic().is_none() && plan.sections[0].matched.is_none() && plan.sections[0].number_read);
        // added, but Butterfield cards haven't been checked on a real statement: held, counting nowhere
        assert_eq!(db.import_scan(ent, "a.pdf", "h1", &cards[0], &[Target::New]).unwrap().held, 1);
        assert_eq!(rows(&db), 0);
        let acct = db.accounts().unwrap()[0].id;
        // the next month goes to that account by itself, and waits for the same reason
        let plan = db.plan_import("h2", &cards[1]).unwrap();
        assert_eq!(plan.automatic(), Some(vec![Target::Existing(acct)]));
        db.import_scan(ent, "b.pdf", "h2", &cards[1], &plan.automatic().unwrap()).unwrap();
        let held = db.documents_all(&[]).unwrap();
        assert!(held.iter().all(|d| d.review == vec![Review::NewLayout]));
        // approving one confirms Butterfield's card layout, which releases the other too
        db.approve_document(held[0].id).unwrap();
        assert!(db.documents_all(&[]).unwrap().iter().all(|d| d.review.is_empty()));
        assert!(db.profiles().unwrap().iter().any(|p| p.slug == "butterfield_ky" && p.layout.verified.contains(&"CreditCard".to_string())));
        let n = cards[0].sections[0].transactions.len() + cards[1].sections[0].transactions.len();
        assert_eq!(rows(&db), n);

        // the same statement read differently (a misread name, a different guess at the account type):
        // same account, flagged as the same period, and not one row added
        let mut again = cards[0].clone();
        again.sections[0].transactions[0].description = "MlSREAD NAME".into();
        again.sections[0].account_type = AccountType::Checking;
        let plan = db.plan_import("h3", &again).unwrap();
        assert_eq!(plan.sections[0].matched, Some(acct));
        assert!(!plan.same_period.is_empty() && plan.automatic().is_none());
        assert_eq!(db.import_auto(ent, "c.pdf", "h3", &again).unwrap().inserted, 0);
        assert_eq!(rows(&db), n);

        // no account number read: no guess, the bank's accounts in that currency are offered instead
        let mut anon = cards[0].clone();
        anon.sections[0].account_last4 = None;
        let plan = db.plan_import("h4", &anon).unwrap();
        assert!(plan.sections[0].matched.is_none() && !plan.sections[0].number_read && plan.sections[0].candidates.iter().any(|c| c.0 == acct));

        // a ledger from before 0.2.4: old fingerprints are rekeyed and receipts follow their transaction
        let t = &db.transactions(&TxFilter::default()).unwrap()[0];
        db.conn.execute("UPDATE transactions SET fitid = 'old-style' WHERE id = ?1", [t.id]).unwrap();
        db.conn.execute("INSERT INTO attachments (account_id, fitid, filename, stored, added_at) VALUES (?1, 'old-style', 'r.pdf', 'r.pdf', '')", [t.account_id]).unwrap();
        db.conn.execute("DELETE FROM settings WHERE key = 'fitid_v2'", []).unwrap();
        db.seed().unwrap();
        let t2 = db.transaction(t.id).unwrap();
        assert_ne!(t2.fitid, "old-style");
        assert_eq!(t2.attachments, 1);
    }

    #[test]
    fn an_older_ledger_gets_the_corrected_cnb_profile_once() {
        let db = Db::open_in_memory().unwrap();
        let cnb = |db: &Db| db.profiles().unwrap().into_iter().find(|p| p.slug == "cnb_ky").unwrap();
        // a ledger from 0.2.2: the guessed layout, and no record of the refresh
        let mut old = cnb(&db);
        old.default_date_format = "DD/MM/YYYY".into();
        old.column_layout_kind = LayoutKind::DebitCreditWithBalance;
        db.save_profile(&old).unwrap();
        db.conn.execute("DELETE FROM settings WHERE key = 'profile_rev:cnb_ky'", []).unwrap();
        db.seed().unwrap();
        assert_eq!(cnb(&db).column_layout_kind, LayoutKind::SignedSingleAmount);
        assert_eq!(cnb(&db).default_date_format, "DD/MM");
        // later edits by the user stick
        let mut mine = cnb(&db);
        mine.default_date_format = "MM/DD".into();
        db.save_profile(&mine).unwrap();
        db.seed().unwrap();
        assert_eq!(cnb(&db).default_date_format, "MM/DD");
    }

    #[test]
    fn demo_imports_reconciled_and_matches_transfers() {
        let db = Db::open_in_memory().unwrap();
        let n = db.load_demo().unwrap();
        assert!(n > 500, "{n}");
        let docs = db.documents_all(&[]).unwrap();
        let bad: Vec<&DocumentRow> = docs.iter().filter(|d| d.reconciliation != Reconciliation::Reconciled).collect();
        assert_eq!(bad.len(), 1, "only the deliberately garbled statement: {bad:?}");
        // it doesn't add up (and has a dated line with no readable amount), so it waits for review and its
        // rows count nowhere yet
        assert_eq!(bad[0].review, vec![Review::Mismatch, Review::OffLayout]);
        assert!(db.documents(&[]).unwrap().iter().all(|d| d.id != bad[0].id));
        assert!(db.transactions(&TxFilter::default()).unwrap().iter().all(|t| t.document_id != Some(bad[0].id)));
        // fix it the way the inspector does, and it reconciles
        let Reconciliation::Discrepancy(gap) = bad[0].reconciliation else { panic!() };
        let (_, rep) = db.document_report(bad[0].id).unwrap();
        let i = rep.skipped.iter().position(|l| l.date.is_some()).unwrap();
        db.convert_skipped_line(bad[0].id, i, rep.skipped[i].date.unwrap(), gap, "INCOMING WIRE").unwrap();
        assert_eq!(db.recheck_document(bad[0].id, Currency::USD).unwrap(), Reconciliation::Reconciled);
        // it adds up now; the unreadable line was the only other reason, so one look and Approve
        db.approve_document(bad[0].id).unwrap();
        assert!(db.documents(&[]).unwrap().iter().any(|d| d.id == bad[0].id));
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let paired = rows.iter().filter(|t| t.transfer_peer.is_some()).count();
        // card payments, RBC savings, loan payments (12 each) and the 6 USD→KYD sweeps
        assert!(paired >= 42 * 2, "transfers pair up: {paired}");
        // re-importing the same statement is a no-op
        let profiles = db.profiles().unwrap();
        let doc = &crate::demo::demo_documents()[0];
        let scan = crate::parser::MultiPassScanner::new(&profiles).scan_tokens(doc.tokens.clone()).unwrap();
        let p = db.entity_by_name("Personal").unwrap().unwrap();
        assert!(!db.plan_import("another-hash", &scan).unwrap().same_period.is_empty(), "another copy of an imported period is caught");
        let rep = db.import_auto(p, "x.pdf", "another-hash", &scan).unwrap();
        assert_eq!((rep.inserted, rep.duplicates), (0, scan.sections[0].transactions.len()));
        // HBO is a known service: filed as a subscription on import, so triage doesn't ask
        let hbo = db.transactions(&TxFilter { search: "HBO MAX".into(), ..Default::default() }).unwrap();
        assert_eq!(hbo.len(), 12);
        assert!(hbo.iter().all(|t| t.category_name.as_deref() == Some("Subscriptions & Digital") && t.is_subscription && t.review == ReviewStatus::AutoCategorized));
        assert!(db.triage(&[p]).unwrap().iter().all(|g| !g.stem.starts_with("HBO")));
    }

    /// Scotiabank renames its column headings: the first statement like that is held (its layout changed),
    /// approving it teaches the profile, the next one imports by itself, and the lesson can be undone.
    #[test]
    fn a_changed_layout_is_held_once_then_learned() {
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let docs: Vec<_> = crate::demo::demo_documents().into_iter().filter(|d| d.filename.starts_with("Scotiabank_Chequing")).collect();
        let renamed = |toks: &[crate::parser::Token]| -> Vec<crate::parser::Token> {
            toks.iter()
                .map(|t| {
                    let text = match t.text.as_str() {
                        "Description" => "Narrative",
                        "DEBIT" => "Out",
                        "CREDIT" => "In",
                        "Balance" => "Running",
                        w => w,
                    };
                    crate::parser::Token { text: text.into(), ..t.clone() }
                })
                .collect()
        };
        let import = |i: usize, toks: Vec<crate::parser::Token>| {
            let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(toks).unwrap();
            assert!(scan.is_reconciled(), "month {i}");
            db.import_auto(ent, &docs[i].filename, &format!("h{i}"), &scan).unwrap()
        };
        assert_eq!(import(0, docs[0].tokens.clone()).held, 0);
        let rep = import(1, renamed(&docs[1].tokens));
        assert_eq!(rep.held, 1);
        let held = db.documents_all(&[]).unwrap().into_iter().find(|d| d.id == rep.document_ids[0]).unwrap();
        assert!(held.review.contains(&Review::LayoutChanged), "{:?}", held.review);
        let (diag, _) = db.document_report(held.id).unwrap();
        assert!(diag.pass3.layout_notes.iter().any(|n| n.contains("column headings are now")), "{:?}", diag.pass3.layout_notes);

        let ap = db.approve_document(held.id).unwrap();
        assert_eq!(ap.learned.as_deref(), Some("Scotiabank"));
        let sb = || db.profiles().unwrap().into_iter().find(|p| p.slug == "scotiabank_ky").unwrap();
        assert!(sb().header_anchors.debit.contains(&"out".to_string()), "{:?}", sb().header_anchors);
        assert_eq!(import(2, renamed(&docs[2].tokens)).held, 0, "the new layout is known now");
        // and the old one still reads cleanly (Rescan All re-reads old statements with the learned profile)
        let old = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(docs[0].tokens.clone()).unwrap();
        assert!(old.is_reconciled() && old.diagnostics.pass3.read_as.is_none() && old.diagnostics.pass3.layout_notes.is_empty());

        assert_eq!(db.layout_changes(sb().id), 1);
        assert!(db.undo_layout_change(sb().id).unwrap());
        assert!(!sb().header_anchors.debit.contains(&"out".to_string()));
        assert_eq!(db.layout_changes(sb().id), 0);
    }

    /// A statement from a bank nobody set up: held, and approving it makes the bank's profile from what page 1
    /// says, so the next statement is recognised and imports by itself into the same account.
    #[test]
    fn approving_an_unknown_banks_statement_makes_its_profile() {
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let docs: Vec<_> = crate::demo::demo_documents().into_iter().filter(|d| d.filename.starts_with("Scotiabank_Chequing")).collect();
        // the same statements from a bank FinGraph doesn't know
        let rebrand = |toks: &[crate::parser::Token]| -> Vec<crate::parser::Token> {
            toks.iter().map(|t| crate::parser::Token { text: t.text.replace("Scotiabank", "Harbour").replace("scotiabank", "harbourbank"), ..t.clone() }).collect()
        };
        let import = |i: usize| {
            let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(rebrand(&docs[i].tokens)).unwrap();
            assert!(scan.is_reconciled(), "month {i}");
            (scan.profile_slug.clone(), db.import_auto(ent, &docs[i].filename, &format!("u{i}"), &scan).unwrap())
        };
        let (bank, rep) = import(0);
        assert_eq!((bank, rep.held), (None, 1), "an unknown bank's first statement waits");
        let doc = rep.document_ids[0];
        let (diag, _) = db.document_report(doc).unwrap();
        assert_eq!(diag.pass1.identifiers, vec!["Harbour & Trust (Cayman) Ltd.".to_string(), "ky.harbourbank.com".into()]);

        let ap = db.approve_document(doc).unwrap();
        assert_eq!(ap.created.as_deref(), Some("Harbour & Trust"));
        let p = db.profiles().unwrap().into_iter().find(|p| p.display_name.starts_with("Harbour")).unwrap();
        assert!(!p.is_builtin && p.layout.verified == vec!["Checking".to_string()], "{p:?}");
        let acct = db.accounts().unwrap().into_iter().find(|a| a.name.starts_with("Harbour & Trust Checking")).expect("the account is named for its bank");

        let (bank, rep) = import(1);
        assert_eq!(bank.as_deref(), Some(p.slug.as_str()), "the next statement is recognised");
        assert_eq!(rep.held, 0, "and imports by itself");
        let d = db.documents_all(&[]).unwrap().into_iter().find(|d| d.id == rep.document_ids[0]).unwrap();
        assert_eq!(d.account_id, acct.id, "into the same account");
    }

    /// Each statement has to start where the account's previous one ended: one that doesn't is held (a statement
    /// or rows may be missing), and one that prints no opening balance is proven from the previous closing.
    #[test]
    fn statements_follow_on_from_the_previous_closing() {
        use crate::demo::{render, Bank, DemoTx, StatementSpec};
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let d = |m: u32, day: u32| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let dec = |s: &str| Decimal::from_str(s).unwrap();
        let month = |m: u32, opening: &str| {
            let txs = vec![DemoTx::new(d(m, 20), "POS PURCHASE", dec("-62.10")).memo("KIRK HOME CENTRE"), DemoTx::new(d(m + 1, 2), "ELECTRONIC CREDIT", dec("500.00")).memo("SALARY")];
            StatementSpec::new(Bank::Scotiabank, AccountType::Checking, Currency::KYD, "2045", dec(opening), (d(m, 16), d(m + 1, 15)), txs)
        };
        let import = |i: usize, toks: Vec<crate::parser::Token>| {
            let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(toks).unwrap();
            let rep = db.import_auto(ent, &format!("m{i}.pdf"), &format!("c{i}"), &scan).unwrap();
            let doc = db.documents_all(&[]).unwrap().into_iter().find(|x| x.id == rep.document_ids[0]).unwrap();
            let notes = db.document_report(doc.id).unwrap().0.pass3.layout_notes;
            (doc, notes)
        };
        // 1,000.00 → 1,437.90 → 1,875.80: each one follows on
        assert!(import(1, render(&[month(1, "1000.00")])).0.review.is_empty());
        assert!(import(2, render(&[month(2, "1437.90")])).0.review.is_empty());
        // March opens 50.00 above February's close
        let (doc, notes) = import(3, render(&[month(3, "1925.80")]));
        assert_eq!(doc.review, vec![Review::Continuity]);
        assert!(notes.iter().any(|n| n.contains("may be missing")), "{notes:?}");

        // April prints no opening balance (nor totals) anywhere: its first row is proven from March's closing
        let db2 = Db::open_in_memory().unwrap();
        let ent2 = db2.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let toks = |spec: StatementSpec| render(&[spec]);
        let without_opening = |toks: Vec<crate::parser::Token>| -> Vec<crate::parser::Token> {
            // the opening line, and the debit/credit totals (which would prove the rows without it)
            let ys: Vec<(u32, f64)> = toks.iter().filter(|t| ["opening", "previous", "withdrawals", "deposits"].contains(&t.text.to_lowercase().as_str())).map(|t| (t.page, t.y0)).collect();
            toks.into_iter().filter(|t| !ys.contains(&(t.page, t.y0))).collect()
        };
        let read = |db: &Db, toks: Vec<crate::parser::Token>, i: usize| {
            let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(toks).unwrap();
            let rep = db.import_auto(ent2, &format!("n{i}.pdf"), &format!("n{i}"), &scan).unwrap();
            (scan, db.documents_all(&[]).unwrap().into_iter().find(|x| x.id == rep.document_ids[0]).unwrap())
        };
        let (_, march) = read(&db2, toks(month(3, "1875.80")), 3);
        assert!(march.review.is_empty());
        let (scan, april) = read(&db2, without_opening(toks(month(4, "2313.70"))), 4);
        assert_ne!(scan.sections[0].reconciliation, Reconciliation::Reconciled, "on its own, April's first row is unproven");
        assert_eq!(april.reconciliation, Reconciliation::Reconciled, "from March's closing it adds up");
        assert!(april.review.is_empty(), "{:?}", april.review);
    }

    /// Two layout changes learned one after the other, then undone: each version of the profile reads exactly the
    /// statements it was taught, the latest one reads both, and undoing puts the older one back.
    #[test]
    fn learned_layouts_build_up_and_undo_cleanly() {
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let docs: Vec<_> = crate::demo::demo_documents().into_iter().filter(|d| d.filename.starts_with("Scotiabank_Chequing")).collect();
        let rename = |toks: &[crate::parser::Token], to: [&str; 4]| -> Vec<crate::parser::Token> {
            toks.iter()
                .map(|t| {
                    let text = match t.text.as_str() {
                        "Description" => to[0],
                        "DEBIT" => to[1],
                        "CREDIT" => to[2],
                        "Balance" => to[3],
                        w => w,
                    };
                    crate::parser::Token { text: text.into(), ..t.clone() }
                })
                .collect()
        };
        let a = rename(&docs[1].tokens, ["Narrative", "Out", "In", "Running"]);
        let b = rename(&docs[2].tokens, ["Particulars", "Spent", "Got", "Left"]);
        let clean = |toks: &[crate::parser::Token]| {
            let r = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(toks.to_vec()).unwrap();
            r.is_reconciled() && r.diagnostics.pass3.read_as.is_none() && r.diagnostics.pass3.layout_notes.is_empty()
        };
        let learn = |i: usize, toks: &[crate::parser::Token]| {
            let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(toks.to_vec()).unwrap();
            assert!(scan.is_reconciled());
            let rep = db.import_auto(ent, &format!("l{i}.pdf"), &format!("l{i}"), &scan).unwrap();
            assert_eq!(rep.held, 1, "a new layout waits");
            assert!(db.approve_document(rep.document_ids[0]).unwrap().learned.is_some());
        };
        let sb = || db.profiles().unwrap().into_iter().find(|p| p.slug == "scotiabank_ky").unwrap().id;
        assert!(!clean(&a) && !clean(&b));
        learn(1, &a);
        assert!(clean(&a) && !clean(&b), "v1 knows A only");
        learn(2, &b);
        assert!(clean(&a) && clean(&b), "v2 knows both");
        assert!(clean(&docs[0].tokens), "and the original layout still");
        assert!(db.undo_layout_change(sb()).unwrap());
        assert!(clean(&a) && !clean(&b), "undone: back to v1");
        assert!(db.undo_layout_change(sb()).unwrap());
        assert!(!clean(&a) && !clean(&b) && clean(&docs[0].tokens), "and to the shipped profile");
    }

    /// A wrong approval can teach a profile the wrong row shape and stray headings (it only adds headings, but
    /// it replaces the row shape). With the profile poisoned like that, a normal statement must still come out
    /// right, or be held — never imported wrong.
    #[test]
    fn a_poisoned_profile_cannot_let_a_normal_statement_through_wrong() {
        let db = Db::open_in_memory().unwrap();
        let mut p = db.profiles().unwrap().into_iter().find(|p| p.slug == "scotiabank_ky").unwrap();
        p.layout.row_shape = profiles::RowShape::AmountLast;
        p.header_anchors.debit.push("description".into());
        p.header_anchors.balance.push("debit".into());
        p.layout.opening.push("closing balance".into());
        db.save_profile(&p).unwrap();
        let money = |r: &crate::parser::ScanResult| r.sections.iter().flat_map(|s| &s.transactions).map(|t| (t.posted, t.amount)).collect::<Vec<_>>();
        for d in crate::demo::demo_documents().into_iter().filter(|d| d.filename.starts_with("Scotiabank")) {
            let want = money(&crate::parser::MultiPassScanner::new(&profiles::builtins()).scan_tokens(d.tokens.clone()).unwrap());
            let r = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(d.tokens.clone()).unwrap();
            let held = !r.is_reconciled() || !r.diagnostics.pass3.layout_notes.is_empty();
            assert!(held, "{}: a poisoned profile's read is held for a look", d.filename);
            assert!(money(&r) == want || !r.is_reconciled(), "{}: if it adds up, it's the right rows", d.filename);
        }
    }

    /// A statement from a bank back home, in pounds: recognised as GBP, its days' rates asked for, and once they're
    /// kept every line counts in the KYD workspace at its own day's rate (through the dollar, then the peg).
    #[test]
    fn a_statement_in_pounds_converts_at_each_days_rate() {
        let db = Db::open_in_memory().unwrap();
        let ent = db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        let gbp = Currency::parse("GBP").unwrap();
        let mut t = Vec::new();
        let mut y = 40.0;
        let mut put = |x: f64, y: f64, s: &str| {
            let mut x = x;
            for w in s.split_whitespace() {
                t.push(crate::parser::Token::new(1, x, y, x + 5.0 * w.chars().count() as f64, y + 8.0, w));
                x += 5.0 * w.chars().count() as f64 + 3.0;
            }
        };
        put(40.0, y, "Albion Bank plc");
        y += 14.0;
        put(40.0, y, "Current Account GBP  Sort code 40-11-22  Account Number 12345678");
        y += 14.0;
        put(40.0, y, "Statement Period: 01/02/2026 to 28/02/2026");
        y += 14.0;
        put(40.0, y, "Opening Balance");
        put(220.0, y, "£1,000.00");
        y += 12.0;
        put(40.0, y, "Closing Balance");
        put(220.0, y, "£1,700.00");
        y += 20.0;
        put(40.0, y, "Date");
        put(120.0, y, "Description");
        put(360.0, y, "Paid Out");
        put(430.0, y, "Paid In");
        put(500.0, y, "Balance");
        for (d, desc, out, inn, bal) in [("05/02/2026", "TESCO STORES", "100.00", "", "900.00"), ("20/02/2026", "SALARY ACME UK", "", "800.00", "1,700.00")] {
            y += 12.0;
            put(40.0, y, d);
            put(120.0, y, desc);
            if !out.is_empty() {
                put(400.0 - 5.0 * out.len() as f64, y, out);
            }
            if !inn.is_empty() {
                put(470.0 - 5.0 * inn.len() as f64, y, inn);
            }
            put(545.0 - 5.0 * bal.len() as f64, y, bal);
        }
        let scan = crate::parser::MultiPassScanner::new(&db.profiles().unwrap()).scan_tokens(t).unwrap();
        assert!(scan.is_reconciled(), "{:?}", scan.sections.iter().map(|s| &s.report).collect::<Vec<_>>());
        assert_eq!(scan.sections[0].currency, gbp);
        let rep = db.import_auto(ent, "albion.pdf", "albion", &scan).unwrap();
        db.approve_document(rep.document_ids[0]).unwrap();

        assert_eq!(db.floating_currencies().unwrap(), vec![gbp]);
        let today = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
        let needed = db.rates_needed(today).unwrap();
        assert!(needed.iter().any(|(cs, from, to)| cs == &vec![gbp] && *from <= NaiveDate::from_ymd_opt(2026, 2, 1).unwrap() && *to >= NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()), "{needed:?}");
        // No rate yet: explicitly unavailable, never one for one.
        let kyd = |db: &Db| {
            let mut v: Vec<Option<Decimal>> = db.transactions(&TxFilter::default()).unwrap().iter().map(|t| t.amount_entity).collect();
            v.sort();
            v
        };
        assert_eq!(kyd(&db), vec![None, None]);
        // a pound bought US$1.25 early in the month and US$1.30 late
        let d = |day: u32| NaiveDate::from_ymd_opt(2026, 2, day).unwrap();
        db.store_rates(&[(d(2), gbp, dec("0.80")), (d(16), gbp, dec("0.7692307692"))], "test").unwrap();
        // -100 GBP on the 5th = -125.00 USD = -104.17 KYD; +800 GBP on the 20th = 1,040.00 USD = 866.67 KYD
        assert_eq!(kyd(&db), vec![Some(dec("-104.17")), Some(dec("866.67"))]);
        // the statement's period is covered now; only the days since the latest rate (for today's) are still wanted
        assert!(db.rates_needed(today).unwrap().iter().all(|(_, from, _)| *from > d(16)), "{:?}", db.rates_needed(today).unwrap());
        assert_eq!(db.rates_status().unwrap()[0].1.as_ref().map(|x| x.0), Some(d(16)));
    }

    #[test]
    fn learning_applies_retroactively() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let cat = db.category_id("Utilities").unwrap();
        let n = db.learn(&Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: "CUC".into(), clean_payee: Some("CUC".into()), category_id: cat, is_subscription: false, is_transfer: false, expected_cadence: None, entity_id: None, business_entity_id: None, ..Default::default() }).unwrap();
        assert!(n >= 24, "{n}");
        let left = db.transactions(&TxFilter { search: "CUC WEB".into(), uncategorized_only: true, ..Default::default() }).unwrap();
        assert!(left.is_empty());
    }

    #[test]
    fn reset_learning_sends_everything_but_transfers_back_to_triage() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let before = db.uncategorized_count(&[]).unwrap();
        let cat = db.category_id("Groceries").unwrap();
        db.learn(&Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: "GENERAL LANDSCAPING".into(), clean_payee: Some("Landscaper".into()), category_id: cat, is_subscription: true, is_transfer: false, expected_cadence: None, entity_id: None, business_entity_id: None, ..Default::default() }).unwrap();
        assert!(db.uncategorized_count(&[]).unwrap() < before);
        db.reset_learning(&[]).unwrap();
        assert!(db.rules().unwrap().is_empty());
        let rows = db.transactions(&TxFilter::default()).unwrap();
        assert!(rows.iter().all(|t| !t.is_subscription && (t.transfer_peer.is_some() || t.category_id.is_none())));
        assert!(rows.iter().any(|t| t.transfer_peer.is_some()), "transfer pairs survive");
        assert!(db.uncategorized_count(&[]).unwrap() >= before);
    }

    #[test]
    fn business_expenses_paid_personally_count_in_the_business() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let personal = db.entity_by_name("Personal").unwrap().unwrap();
        let business = db.entity_by_name("Business — Property").unwrap().unwrap();
        let kirk = |e: i64| db.transactions(&TxFilter { entity_ids: vec![e], search: "KIRK HOME".into(), ..Default::default() }).unwrap();
        let before = kirk(personal);
        assert!(!before.is_empty() && kirk(business).is_empty());
        // one by hand: it moves to the business P&L and out of personal spending
        db.set_business(&[before[0].id], Some(business)).unwrap();
        assert_eq!(kirk(business).len(), 1);
        assert_eq!(kirk(personal).len(), before.len() - 1);
        assert_eq!(db.transaction(before[0].id).unwrap().account_entity_id, personal);
        // remembered as a rule: every Kirk Home Centre payment, and future imports too
        db.learn(&Rule { pattern: "KIRK HOME CENTRE".into(), business_entity_id: Some(business), ..Default::default() }).unwrap();
        assert!(kirk(personal).is_empty());
        assert_eq!(kirk(business).len(), before.len());
        db.set_business(&[before[0].id], None).unwrap();
        assert_eq!(kirk(personal).len(), 1);
    }

    #[test]
    fn categories_are_alphabetical_and_older_ledgers_get_new_ones() {
        let db = Db::open_in_memory().unwrap();
        let cats = db.categories().unwrap();
        let tops: Vec<&str> = cats.iter().filter(|c| c.parent_id.is_none()).map(|c| c.name.as_str()).collect();
        let mut sorted = tops.clone();
        sorted.sort_by_key(|s| s.to_lowercase());
        assert_eq!(tops, sorted);
        // every sub-category sits right under its parent
        for (i, c) in cats.iter().enumerate() {
            if let Some(p) = c.parent_id {
                assert!(cats[..i].iter().rev().find(|x| x.parent_id.is_none()).is_some_and(|x| x.id == p), "{}", c.name);
            }
        }
        let fam = db.category_id("Family Expense").unwrap().unwrap();
        assert!(cats.iter().any(|c| c.parent_id == Some(fam) && c.name == "Childcare & Nanny"));
        // reopening (an older ledger) adds nothing twice
        let n = cats.len();
        db.seed().unwrap();
        assert_eq!(db.categories().unwrap().len(), n);
    }

    #[test]
    fn rescan_keeps_hand_edits() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let rows = db.transactions(&TxFilter { search: "KIRK HOME".into(), ..Default::default() }).unwrap();
        let t = &rows[0];
        let fam = db.category_id("Family Expense").unwrap();
        db.set_category(&[t.id], fam).unwrap();
        db.set_tag(&[t.id], "Kids room").unwrap();
        let sub = db.transactions(&TxFilter { search: "KIRK HOME".into(), ..Default::default() }).unwrap()[1].clone();
        db.set_subscription(&[sub.id], true).unwrap();
        let (sha, name, ent): (String, String, i64) = db
            .conn
            .query_row("SELECT d.sha256_hash, d.filename, a.entity_id FROM documents d JOIN accounts a ON a.id = d.account_id WHERE d.id = ?1", [t.document_id.unwrap()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        let doc = crate::demo::demo_documents().into_iter().find(|d| d.filename == name).unwrap();
        let profiles = db.profiles().unwrap();
        let scan = crate::parser::MultiPassScanner::new(&profiles).scan_tokens(doc.tokens).unwrap();
        db.replace_statement(ent, &name, &sha, &scan).unwrap();
        let again = db.transactions(&TxFilter { search: "KIRK HOME".into(), ..Default::default() }).unwrap();
        let same = again.iter().find(|x| x.fitid == t.fitid).unwrap();
        assert_eq!((same.category_id, same.tag.as_str()), (fam, "Kids room"));
        assert_eq!(again.len(), rows.len(), "no duplicates");
        if sub.document_id == t.document_id {
            assert!(again.iter().find(|x| x.fitid == sub.fitid).unwrap().is_subscription, "Mark Subscription survives a rescan");
        }
    }

    #[test]
    fn workspace_delete_backup_and_erase() {
        let dir = std::env::temp_dir().join(format!("fingraph-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("ledger.db")).unwrap();
        db.load_demo().unwrap();
        let n = db.transactions(&TxFilter::default()).unwrap().len();
        let backup = db.auto_backup("test").unwrap().unwrap();
        let biz = db.entity_by_name("Business — Property").unwrap().unwrap();
        db.delete_entity(biz).unwrap();
        assert!(db.transactions(&TxFilter::default()).unwrap().len() < n);
        db.set_budget("1", 1, Some((Decimal::from(100), Currency::KYD))).unwrap();
        db.erase_all().unwrap();
        assert!(db.entities().unwrap().is_empty() && db.transactions(&TxFilter::default()).unwrap().is_empty());
        // budgets and the rest go too, so demo data leaves nothing behind
        assert!(db.budgets("1").unwrap().is_empty());
        db.restore_from(&backup).unwrap();
        assert_eq!(db.transactions(&TxFilter::default()).unwrap().len(), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_categories_exist_and_respect_your_choices() {
        let db = Db::open_in_memory().unwrap();
        for m in crate::merchants::DIRECTORY.iter() {
            assert!(db.category_id(&m.category).unwrap().is_some(), "{} → unknown category {}", m.name, m.category);
        }
        db.load_demo().unwrap();
        // something you set to Uncategorized by hand stays that way
        let t = db.transactions(&TxFilter { search: "NETFLIX".into(), ..Default::default() }).unwrap()[0].clone();
        db.set_category(&[t.id], None).unwrap();
        db.apply_directory().unwrap();
        assert_eq!(db.transaction(t.id).unwrap().category_id, None);
        // a deposit from a business in the directory is pay or a refund, not a purchase
        let dep = db.transactions(&TxFilter::default()).unwrap().into_iter().find(|t| t.amount.is_sign_positive() && t.transfer_peer.is_none()).unwrap();
        db.conn.execute("UPDATE transactions SET raw_description = 'ELECTRONIC CREDIT, CAYMAN AIRWAYS REFUND', category_id = NULL, review_status = 'NeedsReview' WHERE id = ?1", [dep.id]).unwrap();
        assert!(crate::merchants::lookup("CAYMAN AIRWAYS REFUND").is_some(), "the airline is in the directory");
        db.apply_directory().unwrap();
        assert_eq!(db.transaction(dep.id).unwrap().category_id, None);
        // and switching it off in Preferences stops it
        let mut p = db.prefs();
        p.auto_directory = false;
        db.save_prefs(&p).unwrap();
        db.reset_learning(&[]).unwrap();
        assert_eq!(db.apply_directory().unwrap(), 0);
    }

    #[test]
    fn card_payments_are_recognised_with_or_without_the_card_statement() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let cc = db.category_id("Credit Card Payments").unwrap();
        // Scotiabank side ("TRANSFER TO BUTTERFIELD VISA 7710") and Butterfield side ("PAYMENT RECEIVED")
        let out = db.transactions(&TxFilter { search: "BUTTERFIELD VISA 7710".into(), ..Default::default() }).unwrap();
        let back = db.transactions(&TxFilter { search: "PAYMENT RECEIVED".into(), ..Default::default() }).unwrap();
        assert_eq!(out.len(), 12);
        assert!(out.iter().chain(&back).all(|t| t.category_id == cc && t.is_transfer()), "{:?}", out.iter().chain(&back).find(|t| t.category_id != cc));
        // never spending
        let fx = db.fx().unwrap();
        let rows = db.transactions(&TxFilter::default()).unwrap();
        assert!(crate::insights::by_category(&rows, chrono::NaiveDate::MIN, chrono::NaiveDate::MAX, Currency::KYD, &fx, &Default::default()).iter().all(|(k, _)| k != "Credit Card Payments"));
        // without the card statement: the payment still names the card, so it's still recognised
        let card = db.accounts().unwrap().into_iter().find(|a| a.account_number_masked == "••7710").unwrap();
        db.conn.execute("DELETE FROM transactions WHERE account_id = ?1", [card.id]).unwrap();
        db.conn.execute("UPDATE transactions SET category_id = NULL, transfer_peer_tx_id = NULL WHERE memo LIKE '%VISA 7710%'", []).unwrap();
        assert!(db.mark_card_payments().unwrap() >= 12);
        let named = db.transactions(&TxFilter { search: "BUTTERFIELD VISA 7710".into(), ..Default::default() }).unwrap();
        assert!(named.iter().all(|t| t.category_id == cc));
        // a card named by its full number
        let words = crate::merchants::tokens("PC TRANSFER DEBIT Transfer to 20416 38817204418");
        assert!(names_account(&words, "4418") && !names_account(&words, "0416"));
        // a transfer to your own savings account isn't a card payment
        assert!(db.transactions(&TxFilter { search: "RBC SAVINGS 3390".into(), ..Default::default() }).unwrap().iter().all(|t| t.category_id != cc));
    }

    #[test]
    fn fx_original_wins_over_the_peg() {
        let fx = Fx::default();
        let day = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        assert_eq!(entity_amount(&fx, dec("-72.39"), Currency::USD, Currency::KYD, Some((Currency::KYD, dec("59.14"))), day), Some(dec("-59.14")));
        assert_eq!(entity_amount(&fx, dec("-120.00"), Currency::USD, Currency::KYD, None, day), Some(dec("-100.00")));
    }
}
