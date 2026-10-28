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
use crate::parser::profiles::{self, BankProfile, HeaderAnchors, LayoutKind};
use crate::parser::{Diagnostics, ScanResult, Section, SectionReport};
use crate::rules::{self, Rule, RuleKind};
use crate::{AccountType, Currency, EntityKind, Error, Reconciliation, Result, ReviewStatus};

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE entities (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    base_currency TEXT NOT NULL DEFAULT 'KYD',
    icon_name TEXT NOT NULL DEFAULT 'avatar-default-symbolic',
    created_at TEXT NOT NULL
);
CREATE TABLE bank_profiles (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    short_name TEXT NOT NULL,
    routing_identifiers TEXT NOT NULL,
    default_date_format TEXT NOT NULL,
    column_layout_kind TEXT NOT NULL,
    header_anchors TEXT NOT NULL,
    noise_patterns TEXT NOT NULL,
    fx_note_regex TEXT NOT NULL,
    is_builtin INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    bank_profile_id INTEGER REFERENCES bank_profiles(id) ON DELETE SET NULL,
    name TEXT NOT NULL,
    account_number_masked TEXT NOT NULL DEFAULT '',
    account_type TEXT NOT NULL,
    currency TEXT NOT NULL,
    ofx_bank_id TEXT NOT NULL DEFAULT '',
    ofx_intu_bid TEXT NOT NULL DEFAULT '',
    opening_balance TEXT NOT NULL DEFAULT '0',
    opening_balance_minor INTEGER NOT NULL DEFAULT 0,
    opening_date TEXT
);
CREATE TABLE fx_rates (
    from_currency TEXT NOT NULL,
    to_currency TEXT NOT NULL,
    rate TEXT NOT NULL,
    PRIMARY KEY (from_currency, to_currency)
);
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    bank_profile_id INTEGER REFERENCES bank_profiles(id) ON DELETE SET NULL,
    filename TEXT NOT NULL,
    sha256_hash TEXT NOT NULL,
    section_index INTEGER NOT NULL DEFAULT 0,
    period_start TEXT,
    period_end TEXT,
    statement_opening_balance TEXT,
    statement_closing_balance TEXT,
    statement_total_debits TEXT,
    statement_total_credits TEXT,
    extracted_tx_sum TEXT NOT NULL,
    extracted_tx_sum_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    reconciliation_status TEXT NOT NULL,
    scan_confidence_score REAL NOT NULL,
    scan_diagnostics_json TEXT NOT NULL,
    imported_at TEXT NOT NULL,
    UNIQUE (sha256_hash, section_index)
);
CREATE TABLE categories (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    parent_id INTEGER REFERENCES categories(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    UNIQUE (name, parent_id)
);
CREATE TABLE transactions (
    id INTEGER PRIMARY KEY,
    document_id INTEGER REFERENCES documents(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    posted_date TEXT NOT NULL,
    effective_date TEXT,
    raw_description TEXT NOT NULL,
    clean_payee TEXT NOT NULL,
    amount TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    amount_in_entity_currency TEXT NOT NULL,
    amount_in_entity_minor INTEGER NOT NULL,
    running_balance_reported TEXT,
    category_id INTEGER REFERENCES categories(id) ON DELETE SET NULL,
    sub_entity_tag TEXT NOT NULL DEFAULT '',
    memo TEXT NOT NULL DEFAULT '',
    fitid TEXT NOT NULL,
    is_recurring INTEGER NOT NULL DEFAULT 0,
    is_subscription INTEGER NOT NULL DEFAULT 0,
    transfer_peer_tx_id INTEGER REFERENCES transactions(id) ON DELETE SET NULL,
    review_status TEXT NOT NULL DEFAULT 'NeedsReview',
    UNIQUE (account_id, fitid)
);
CREATE INDEX tx_posted ON transactions(posted_date);
CREATE INDEX tx_account ON transactions(account_id);
CREATE TABLE learned_rules (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    pattern TEXT NOT NULL,
    clean_payee TEXT,
    category_id INTEGER REFERENCES categories(id) ON DELETE CASCADE,
    is_subscription INTEGER NOT NULL DEFAULT 0,
    is_transfer INTEGER NOT NULL DEFAULT 0,
    expected_cadence INTEGER,
    entity_id INTEGER REFERENCES entities(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#, r#"
-- a payment from a personal account that was really a business expense is booked to that workspace
ALTER TABLE transactions ADD COLUMN business_entity_id INTEGER REFERENCES entities(id) ON DELETE SET NULL;
ALTER TABLE learned_rules ADD COLUMN business_entity_id INTEGER REFERENCES entities(id) ON DELETE SET NULL;
"#, r#"
-- closed accounts can be hidden from lists and charts; their history still counts
ALTER TABLE accounts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
"#, r#"
-- rules that only apply to some amounts ("Adriana, CI$ 125.00 only")
ALTER TABLE learned_rules ADD COLUMN amount_min TEXT;
ALTER TABLE learned_rules ADD COLUMN amount_max TEXT;
-- one charge split over categories / workspaces (the A.L. Thompson's receipt): the statement row
-- stays as printed for reconciliation, reports read the lines
CREATE TABLE splits (
    id INTEGER PRIMARY KEY,
    transaction_id INTEGER NOT NULL REFERENCES transactions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    amount TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    category_id INTEGER REFERENCES categories(id) ON DELETE SET NULL,
    business_entity_id INTEGER REFERENCES entities(id) ON DELETE SET NULL,
    tag TEXT NOT NULL DEFAULT '',
    memo TEXT NOT NULL DEFAULT '',
    UNIQUE (transaction_id, seq)
);
-- business spending you paid personally, settled by a later transfer; keyed by fingerprint so a
-- rescan keeps it (split_seq -1 = the whole transaction)
CREATE TABLE reimbursements (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    fitid TEXT NOT NULL,
    split_seq INTEGER NOT NULL DEFAULT -1,
    paid_account_id INTEGER NOT NULL,
    paid_fitid TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (account_id, fitid, split_seq)
);
-- receipts and invoices; files live in attachments/ beside the ledger
CREATE TABLE attachments (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    fitid TEXT NOT NULL,
    filename TEXT NOT NULL,
    stored TEXT NOT NULL,
    added_at TEXT NOT NULL
);
CREATE INDEX attachments_tx ON attachments(account_id, fitid);
"#, r#"
CREATE TABLE budgets (
    scope TEXT NOT NULL,
    category_id INTEGER NOT NULL,
    amount TEXT NOT NULL,
    currency TEXT NOT NULL,
    PRIMARY KEY (scope, category_id)
);
"#, r#"
-- what each budget was from a month on, so changing it doesn't rewrite past months' rollovers
CREATE TABLE budget_history (
    scope TEXT NOT NULL,
    category_id INTEGER NOT NULL,
    month TEXT NOT NULL,
    amount TEXT NOT NULL,
    currency TEXT NOT NULL,
    PRIMARY KEY (scope, category_id, month)
);
INSERT INTO budget_history SELECT scope, category_id, '2000-01-01', amount, currency FROM budgets;
-- money moved between categories in one month (cover an overspend)
CREATE TABLE budget_moves (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    month TEXT NOT NULL,
    from_category INTEGER NOT NULL,
    to_category INTEGER NOT NULL,
    amount TEXT NOT NULL,
    currency TEXT NOT NULL,
    created_at TEXT NOT NULL
);
-- yearly or quarterly bills set aside for monthly
CREATE TABLE sinking_funds (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    amount TEXT NOT NULL,
    currency TEXT NOT NULL,
    every_months INTEGER NOT NULL,
    next_due TEXT NOT NULL,
    stem TEXT NOT NULL DEFAULT ''
);
-- a month you've been through in Monthly Close
CREATE TABLE month_closes (
    scope TEXT NOT NULL,
    month TEXT NOT NULL,
    summary TEXT NOT NULL,
    closed_at TEXT NOT NULL,
    PRIMARY KEY (scope, month)
);
-- assets and debts that aren't on a statement: the house, the car, a pension
CREATE TABLE manual_items (
    id INTEGER PRIMARY KEY,
    entity_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    liability INTEGER NOT NULL,
    currency TEXT NOT NULL,
    apr TEXT
);
CREATE TABLE manual_values (
    item_id INTEGER NOT NULL,
    as_of TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (item_id, as_of)
);
CREATE TABLE goals (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    paydown INTEGER NOT NULL,
    target TEXT NOT NULL,
    currency TEXT NOT NULL,
    target_date TEXT,
    monthly TEXT NOT NULL,
    account_id INTEGER,
    start_value TEXT NOT NULL,
    created TEXT NOT NULL
);
CREATE TABLE goal_entries (
    goal_id INTEGER NOT NULL,
    month TEXT NOT NULL,
    amount TEXT NOT NULL,
    PRIMARY KEY (goal_id, month)
);
"#, r#"
-- the statements list counts each statement's transactions; without this it scans them all per statement
CREATE INDEX IF NOT EXISTS tx_document ON transactions(document_id);
"#];

/// (name, kind, children). Missing ones are added to existing ledgers on open; lists are shown A–Z.
const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("Bank Fees & Interest", "Expense", &["Account Fees", "Credit Card Interest", "FX & Wire Fees", "Loan Interest"]),
    ("Business Expense", "Expense", &["Advertising & Marketing", "Contractors & Payroll", "Licences & Permits", "Office Supplies", "Software & Tools"]),
    ("Cash Withdrawal", "Expense", &[]),
    ("Clothing & Shoes", "Expense", &[]),
    ("Dining & Takeaway", "Expense", &["Coffee & Snacks", "Restaurants", "Takeaway & Delivery"]),
    ("Education", "Expense", &["Books & Courses", "School Fees & Tuition"]),
    ("Entertainment", "Expense", &["Events & Tickets", "Hobbies", "Movies & Streaming"]),
    ("Family Expense", "Expense", &["Allowances", "Baby & Kids' Supplies", "Childcare & Nanny", "Family Support & Remittances", "Kids' Activities", "School Supplies & Uniforms"]),
    ("Gifts & Donations", "Expense", &["Charity", "Church & Tithes", "Gifts"]),
    ("Government & Fees", "Expense", &["Import Duty", "Stamp Duty", "Vehicle Licensing", "Work Permits & Immigration"]),
    ("Groceries", "Expense", &["Liquor"]),
    ("Health & Fitness", "Expense", &["Doctor & Dental", "Gym & Sports", "Optical", "Pharmacy"]),
    ("Home & Garden", "Expense", &["Appliances", "Furniture & Decor", "Household Supplies"]),
    ("Housing", "Expense", &["Mortgage", "Rent", "Strata Fees"]),
    ("Income", "Income", &["Business Income", "Gifts Received", "Interest & Dividends", "Refunds", "Reimbursements", "Rental Income", "Salary", "Sales"]),
    ("Insurance", "Expense", &["Car Insurance", "Health Insurance", "Home Insurance", "Life Insurance"]),
    ("Loan Payments", "Transfer", &[]),
    ("Personal Care", "Expense", &["Hair & Beauty", "Spa & Massage"]),
    ("Pets", "Expense", &["Pet Food & Supplies", "Vet"]),
    ("Professional Services", "Expense", &["Accounting", "Legal"]),
    ("Property Expense", "Expense", &["Cleaning", "Landscaping", "Pest Control", "Pool Service", "Property Management"]),
    ("Repairs & Maintenance", "Expense", &[]),
    ("Savings & Investments", "Transfer", &[]),
    ("Shopping", "Expense", &["Electronics", "Online Shopping", "Shipping & Freight"]),
    ("Subscriptions & Digital", "Expense", &[]),
    ("Transfers", "Transfer", &["Credit Card Payments"]),
    ("Transport", "Expense", &["Car Payment", "Fuel", "Parking", "Taxi & Rideshare", "Vehicle Maintenance"]),
    ("Travel", "Expense", &["Car Rental", "Flights", "Hotels"]),
    ("Utilities", "Expense", &["Electricity", "Gas", "Phone & Internet", "Water"]),
];

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
    pub amount_entity: Decimal,
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
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub document_ids: Vec<i64>,
    pub inserted: usize,
    pub duplicates: usize,
    pub already_imported: bool,
    pub auto_categorized: usize,
}

pub struct Db {
    pub conn: Connection,
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
pub fn fitid(account_id: i64, posted: NaiveDate, amount: Decimal, raw: &str, seq: usize) -> String {
    let norm: String = raw.to_uppercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let mut h = Sha256::new();
    h.update(format!("{account_id}|{posted}|{}|{norm}|{seq}", crate::money::round2(amount)).as_bytes());
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
        amount_entity: dec(&r.get::<_, String>(14)?),
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
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
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
        let db = Db { conn };
        db.migrate()?;
        db.seed()?;
        db.prefs().apply_globals();
        Ok(db)
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

    /// A consistent copy of the whole ledger (settings and preferences included).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }

    pub fn backups_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.path()?.parent()?.join("backups"))
    }

    /// Before something drastic: a timestamped backup, keeping the newest ten. Off in Preferences = no-op.
    pub fn auto_backup(&self, reason: &str) -> Result<Option<std::path::PathBuf>> {
        let Some(dir) = self.backups_dir().filter(|_| self.prefs().auto_backup) else { return Ok(None) };
        let dest = dir.join(format!("ledger-{}-{reason}.db", Local::now().format("%Y%m%d-%H%M%S")));
        self.backup_to(&dest)?;
        let mut old: Vec<_> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ledger-"))).collect();
        old.sort();
        for p in old.iter().rev().skip(10) {
            let _ = std::fs::remove_file(p);
        }
        Ok(Some(dest))
    }

    /// Replace the ledger's contents with a backup's (then bring it up to this version's schema).
    pub fn restore_from(&self, src: &Path) -> Result<()> {
        let from = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        from.query_row("SELECT COUNT(*) FROM entities", [], |r| r.get::<_, i64>(0)).map_err(|_| Error("that file isn't a FinGraph ledger".into()))?;
        let path = self.path().ok_or_else(|| Error("an in-memory ledger can't be restored into".into()))?;
        // a second handle on the same file takes the pages; this one sees them on its next read
        let mut to = Connection::open(&path)?;
        {
            let b = rusqlite::backup::Backup::new(&from, &mut to)?;
            b.run_to_completion(256, std::time::Duration::from_millis(0), None)?;
        }
        drop(to);
        self.migrate()?;
        self.seed()?;
        self.prefs().apply_globals();
        Ok(())
    }

    /// Everything you put in goes: workspaces, accounts, statements, transactions, rules and
    /// settings. Bank profiles, categories and preferences stay.
    pub fn erase_all(&self) -> Result<()> {
        let prefs = self.setting("preferences");
        let tx = self.conn.unchecked_transaction()?;
        for t in [
            "transactions", "documents", "accounts", "learned_rules", "entities", "settings", "splits", "reimbursements", "attachments", "budgets", "budget_history", "budget_moves", "sinking_funds",
            "month_closes", "manual_values", "manual_items", "goal_entries", "goals",
        ] {
            tx.execute(&format!("DELETE FROM {t}"), [])?;
        }
        tx.commit()?;
        if let Some(p) = prefs {
            self.set_setting("preferences", &p)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- kept statement PDFs

    /// Keep a copy of an imported PDF, named by its SHA-256, so it can be rescanned later.
    pub fn keep_pdf(&self, src: &Path, sha256: &str) -> Result<()> {
        let dir = self.prefs().statements_dir();
        std::fs::create_dir_all(&dir)?;
        let dst = dir.join(format!("{sha256}.pdf"));
        if !dst.exists() {
            std::fs::copy(src, &dst)?;
        }
        Ok(())
    }

    /// A statement's PDF: the kept copy, else a file with the same name and contents in the data folder.
    pub fn stored_pdf(&self, sha256: &str, filename: &str) -> Option<std::path::PathBuf> {
        let kept = self.prefs().statements_dir().join(format!("{sha256}.pdf"));
        if kept.exists() {
            return Some(kept);
        }
        let old = crate::data_dir().join("documents").join(filename);
        (file_sha256(&old).ok().as_deref() == Some(sha256)).then_some(old)
    }

    /// Move the kept PDFs to a new folder and remember it. Returns how many moved.
    pub fn move_statements_dir(&self, to: &Path) -> Result<usize> {
        let mut p = self.prefs();
        let from = p.statements_dir();
        std::fs::create_dir_all(to)?;
        let mut n = 0;
        if from != to && from.exists() {
            for e in std::fs::read_dir(&from)?.flatten() {
                let dst = to.join(e.file_name());
                if std::fs::rename(e.path(), &dst).is_err() {
                    std::fs::copy(e.path(), &dst)?;
                    std::fs::remove_file(e.path())?;
                }
                n += 1;
            }
        }
        p.statements_dir = Some(to.to_path_buf());
        self.save_prefs(&p)?;
        Ok(n)
    }

    fn migrate(&self) -> Result<()> {
        let v: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(v as usize) {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    fn seed(&self) -> Result<()> {
        for p in profiles::builtins() {
            let exists: bool = self.conn.query_row("SELECT COUNT(*) FROM bank_profiles WHERE slug = ?1", [&p.slug], |r| r.get::<_, i64>(0))? > 0;
            if !exists {
                self.save_profile(&p)?;
            }
        }
        // a built-in rewritten from a real statement replaces the stored guess, once (CNB: 0.2.3);
        // the next bank checked against a real PDF gets its own key here
        if self.setting("profile_rev:cnb_ky").is_none() {
            if let Some(id) = self.conn.query_row("SELECT id FROM bank_profiles WHERE slug = 'cnb_ky' AND is_builtin = 1", [], |r| r.get::<_, i64>(0)).optional()? {
                self.delete_or_reset_profile(id)?;
            }
            self.set_setting("profile_rev:cnb_ky", "1")?;
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
        self.conn.execute("INSERT INTO entities (name, kind, base_currency, icon_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![name, kind.as_str(), base.code(), icon, now()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_entity(&self, id: i64, name: &str, base: Currency) -> Result<()> {
        self.conn.execute("UPDATE entities SET name = ?2, base_currency = ?3 WHERE id = ?1", params![id, name, base.code()])?;
        self.revalue(Some(id))
    }

    pub fn entity_by_name(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM entities WHERE name = ?1", [name], |r| r.get(0)).optional()?)
    }

    // ---------------------------------------------------------------- bank profiles

    pub fn profiles(&self) -> Result<Vec<BankProfile>> {
        let mut st = self.conn.prepare("SELECT id, slug, display_name, short_name, routing_identifiers, default_date_format, column_layout_kind, header_anchors, noise_patterns, fx_note_regex, is_builtin FROM bank_profiles ORDER BY is_builtin DESC, display_name")?;
        let rows = st.query_map([], profile_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert (id 0) or update a profile; returns its id.
    pub fn save_profile(&self, p: &BankProfile) -> Result<i64> {
        let ids = serde_json::to_string(&p.routing_identifiers)?;
        let anchors = serde_json::to_string(&p.header_anchors)?;
        let noise = serde_json::to_string(&p.noise_patterns)?;
        if p.id == 0 {
            self.conn.execute(
                "INSERT INTO bank_profiles (slug, display_name, short_name, routing_identifiers, default_date_format, column_layout_kind, header_anchors, noise_patterns, fx_note_regex, is_builtin) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![p.slug, p.display_name, p.short_name, ids, p.default_date_format, p.column_layout_kind.as_str(), anchors, noise, p.fx_note_regex, p.is_builtin],
            )?;
            Ok(self.conn.last_insert_rowid())
        } else {
            self.conn.execute(
                "UPDATE bank_profiles SET display_name=?2, short_name=?3, routing_identifiers=?4, default_date_format=?5, column_layout_kind=?6, header_anchors=?7, noise_patterns=?8, fx_note_regex=?9 WHERE id=?1",
                params![p.id, p.display_name, p.short_name, ids, p.default_date_format, p.column_layout_kind.as_str(), anchors, noise, p.fx_note_regex],
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
        Ok(Fx { kyd_to_usd: get("KYD", "USD")?.unwrap_or(d.kyd_to_usd), usd_to_kyd: get("USD", "KYD")?.unwrap_or(d.usd_to_kyd) })
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
        let rows: Vec<(i64, String, String, String, String)> = {
            let mut st = self.conn.prepare(
                "SELECT t.id, t.amount, t.currency, e.base_currency, t.memo FROM transactions t JOIN accounts a ON a.id = t.account_id
                 JOIN entities e ON e.id = COALESCE(t.business_entity_id, a.entity_id) WHERE (?1 IS NULL OR e.id = ?1 OR a.entity_id = ?1)",
            )?;
            st.query_map([entity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let tx = self.conn.unchecked_transaction()?;
        for (id, amt, c, base, memo) in rows {
            let v = entity_amount(&fx, dec(&amt), ccy(&c), ccy(&base), parse_fx_memo(&memo));
            tx.execute("UPDATE transactions SET amount_in_entity_currency = ?2, amount_in_entity_minor = ?3 WHERE id = ?1", params![id, v.to_string(), to_minor(v)])?;
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
        self.conn.execute(
            "UPDATE accounts SET name = ?2, ofx_bank_id = ?3, ofx_intu_bid = ?4, entity_id = ?5, hidden = ?6 WHERE id = ?1",
            params![a.id, a.name, a.ofx_bank_id, a.ofx_intu_bid, a.entity_id, a.hidden],
        )?;
        self.revalue(Some(a.entity_id))
    }

    /// The account a statement section belongs to: same bank, last four digits, currency and type.
    fn account_for(&self, entity_id: i64, profile: Option<&BankProfile>, s: &Section) -> Result<i64> {
        let masked = s.account_last4.as_ref().map(|l| format!("••{l}")).unwrap_or_default();
        let pid = profile.map(|p| p.id);
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM accounts WHERE bank_profile_id IS ?1 AND account_number_masked = ?2 AND currency = ?3 AND account_type = ?4",
                params![pid, masked, s.currency.code(), s.account_type.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            return Ok(id);
        }
        let bank = profile.map_or("Bank", |p| p.short_name.as_str());
        let name = format!("{bank} {} {}{}", s.account_type.label(), s.currency.code(), s.account_last4.as_ref().map(|l| format!(" ••{l}")).unwrap_or_default());
        self.conn.execute(
            "INSERT INTO accounts (entity_id, bank_profile_id, name, account_number_masked, account_type, currency, opening_balance, opening_balance_minor, opening_date) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
            params![entity_id, pid, name, masked, s.account_type.as_str(), s.currency.code(), s.opening.unwrap_or_default().to_string(), to_minor(s.opening.unwrap_or_default())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // ---------------------------------------------------------------- import

    /// Store a scanned statement: one document per section, transactions deduplicated by FITID,
    /// learned rules applied, transfers matched.
    pub fn import_scan(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult) -> Result<ImportReport> {
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
        let base: Currency = self.conn.query_row("SELECT base_currency FROM entities WHERE id = ?1", [entity_id], |r| r.get::<_, String>(0)).map(|s| ccy(&s))?;
        let rules = self.rules()?;
        let tx = self.conn.unchecked_transaction()?;
        for (si, s) in scan.sections.iter().enumerate() {
            let account_id = self.account_for(entity_id, profile, s)?;
            #[derive(serde::Serialize)]
            struct Stored<'a> {
                diagnostics: &'a Diagnostics,
                report: &'a SectionReport,
            }
            let diag = serde_json::to_string(&Stored { diagnostics: &scan.diagnostics, report: &s.report })?;
            let sum = s.extracted_sum();
            tx.execute(
                "INSERT INTO documents (account_id, bank_profile_id, filename, sha256_hash, section_index, period_start, period_end, statement_opening_balance, statement_closing_balance,
                 statement_total_debits, statement_total_credits, extracted_tx_sum, extracted_tx_sum_minor, currency, reconciliation_status, scan_confidence_score, scan_diagnostics_json, imported_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
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
                    now()
                ],
            )?;
            let doc_id = tx.last_insert_rowid();
            rep.document_ids.push(doc_id);
            let mut seq: std::collections::HashMap<(NaiveDate, String, String), usize> = Default::default();
            for t in &s.transactions {
                let k = (t.posted, t.amount.to_string(), t.description.to_uppercase());
                let n = seq.entry(k).or_insert(0);
                let fid = fitid(account_id, t.posted, t.amount, &t.description, *n);
                *n += 1;
                let stem = rules::merchant_stem(&t.description, &t.memo);
                let (payee, _) = rules::known_merchant(&stem, &t.description);
                let ent = entity_amount(&fx, t.amount, s.currency, base, t.fx_original);
                let mut memo = t.memo.clone();
                if let Some((oc, oa)) = t.fx_original
                    && !memo.contains(&oa.to_string())
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
                        ent.to_string(),
                        to_minor(ent),
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
                    let v = entity_amount(&fx, t.amount, s.currency, ccy(&base), t.fx_original);
                    tx.execute("UPDATE transactions SET business_entity_id = ?2, amount_in_entity_currency = ?3, amount_in_entity_minor = ?4 WHERE id = ?1", params![id, b, v.to_string(), to_minor(v)])?;
                }
                if n == 0 {
                    rep.duplicates += 1;
                } else {
                    rep.inserted += 1;
                    rep.auto_categorized += rule.is_some_and(|r| r.category_id.is_some()) as usize;
                }
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

    pub fn documents(&self, entity_ids: &[i64]) -> Result<Vec<DocumentRow>> {
        let mut st = self.conn.prepare(
            "SELECT d.id, d.account_id, a.name, a.entity_id, COALESCE(bp.short_name, ''), d.filename, d.period_start, d.period_end, d.statement_opening_balance, d.statement_closing_balance,
             d.statement_total_debits, d.statement_total_credits, d.extracted_tx_sum, d.currency, d.reconciliation_status, d.scan_confidence_score, d.imported_at,
             (SELECT COUNT(*) FROM transactions t WHERE t.document_id = d.id)
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

    /// Deletes the whole statement (every section of the same file) and its transactions.
    pub fn delete_document(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM documents WHERE sha256_hash = (SELECT sha256_hash FROM documents WHERE id = ?1)", [id])?;
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
    /// come in through the normal import (rules applied, transfers re-matched).
    pub fn replace_statement(&self, entity_id: i64, filename: &str, sha256: &str, scan: &ScanResult) -> Result<ImportReport> {
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
            manual: Option<(NaiveDate, String, Decimal, String)>,
            splits: Vec<crate::workflows::SplitLine>,
        }
        let keep = self.prefs().rescan_keeps_edits;
        let kept: Vec<Kept> = if keep {
            let mut st = self.conn.prepare(
                "SELECT t.account_id, t.fitid, t.category_id, t.clean_payee, t.sub_entity_tag, t.business_entity_id, t.is_subscription, t.review_status, t.posted_date, t.raw_description, t.amount, t.memo
                 FROM transactions t JOIN documents d ON d.id = t.document_id WHERE d.sha256_hash = ?1",
            )?;
            let rows = st.query_map([sha256], |r| {
                let memo: String = r.get(11)?;
                Ok(Kept {
                    account: r.get(0)?,
                    fitid: r.get(1)?,
                    category: r.get(2)?,
                    payee: r.get(3)?,
                    tag: r.get(4)?,
                    business: r.get(5)?,
                    subscription: r.get(6)?,
                    confirmed: r.get::<_, String>(7)? == "Confirmed",
                    manual: memo.starts_with("Converted from skipped line").then(|| (date(&r.get::<_, String>(8).unwrap_or_default()), r.get(9).unwrap_or_default(), dec(&r.get::<_, String>(10).unwrap_or_default()), memo.clone())),
                    splits: Vec::new(),
                })
            })?;
            let mut v: Vec<Kept> = rows.collect::<rusqlite::Result<_>>()?;
            for k in &mut v {
                if let Some(id) = self.conn.query_row("SELECT id FROM transactions WHERE account_id = ?1 AND fitid = ?2", params![k.account, k.fitid], |r| r.get::<_, i64>(0)).optional()? {
                    k.splits = self.splits(id)?;
                }
            }
            v
        } else {
            Vec::new()
        };
        self.conn.execute("DELETE FROM documents WHERE sha256_hash = ?1", [sha256])?;
        let rep = self.import_scan(entity_id, filename, sha256, scan)?;
        for k in kept {
            let id: Option<i64> = self.conn.query_row("SELECT id FROM transactions WHERE account_id = ?1 AND fitid = ?2", params![k.account, k.fitid], |r| r.get(0)).optional()?;
            let id = match (id, &k.manual) {
                (Some(id), _) => Some(id),
                // a line you converted in the inspector: put it back unless the scanner now reads it itself
                (None, Some((day, desc, amt, memo))) => {
                    let found: i64 = self.conn.query_row("SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND posted_date = ?2 AND amount = ?3", params![k.account, day.to_string(), amt.to_string()], |r| r.get(0))?;
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
            if k.business.is_some() {
                self.set_business(&[id], k.business)?;
            }
            if !k.splits.is_empty() {
                self.set_splits(id, &k.splits)?;
            }
        }
        self.recheck_documents(sha256)?;
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
        self.conn.execute("DELETE FROM entities WHERE id = ?1", [id])?;
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
            let scan = scanner.scan_pdf(&path)?;
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
        let tx = self.conn.unchecked_transaction()?;
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
                n += 1;
            }
        }
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
        Ok(status)
    }

    // ---------------------------------------------------------------- transactions

    pub fn transactions(&self, f: &TxFilter) -> Result<Vec<TxRow>> {
        if f.split_lines {
            return self.with_split_lines(f);
        }
        let mut sql = format!("{TX_SELECT} WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !f.entity_ids.is_empty() {
            sql += &format!(" AND COALESCE(t.business_entity_id, a.entity_id) IN ({})", f.entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(","));
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
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn transaction(&self, id: i64) -> Result<TxRow> {
        Ok(self.conn.query_row(&format!("{TX_SELECT} WHERE t.id = ?1"), [id], tx_row)?)
    }

    pub fn set_category(&self, ids: &[i64], category_id: Option<i64>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
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
        self.conn.execute("UPDATE transactions SET memo = ?2 WHERE id = ?1", params![id, memo])?;
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
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            let (amt, cur, memo, home): (String, String, String, i64) = tx.query_row(
                "SELECT t.amount, t.currency, t.memo, a.entity_id FROM transactions t JOIN accounts a ON a.id = t.account_id WHERE t.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let target = business.filter(|b| *b != home);
            let base: String = tx.query_row("SELECT base_currency FROM entities WHERE id = ?1", [target.unwrap_or(home)], |r| r.get(0))?;
            let v = entity_amount(&fx, dec(&amt), ccy(&cur), ccy(&base), parse_fx_memo(&memo));
            tx.execute(
                "UPDATE transactions SET business_entity_id = ?2, amount_in_entity_currency = ?3, amount_in_entity_minor = ?4 WHERE id = ?1",
                params![id, target, v.to_string(), to_minor(v)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_manual(&self, account_id: i64, doc_id: Option<i64>, posted: NaiveDate, description: &str, amount: Decimal, memo: &str) -> Result<i64> {
        let (cur, base): (String, String) = self.conn.query_row("SELECT a.currency, e.base_currency FROM accounts a JOIN entities e ON e.id = a.entity_id WHERE a.id = ?1", [account_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let fx = self.fx()?;
        let ent = entity_amount(&fx, amount, ccy(&cur), ccy(&base), None);
        let stem = rules::merchant_stem(description, "");
        let seq: i64 = self.conn.query_row("SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND posted_date = ?2", params![account_id, posted.to_string()], |r| r.get(0))?;
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
                ent.to_string(),
                to_minor(ent),
                memo,
                fitid(account_id, posted, amount, description, 1000 + seq as usize)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_transactions(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn.execute("DELETE FROM transactions WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- categories & rules

    pub fn categories(&self) -> Result<Vec<Category>> {
        // A–Z by top-level category, each followed by its own sub-categories A–Z
        let mut st = self.conn.prepare(
            "SELECT c.id, c.name, c.parent_id, c.kind FROM categories c LEFT JOIN categories p ON p.id = c.parent_id
             ORDER BY COALESCE(p.name, c.name) COLLATE NOCASE, COALESCE(p.id, c.id), c.parent_id IS NOT NULL, c.name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| Ok(Category { id: r.get(0)?, name: r.get(1)?, parent_id: r.get(2)?, kind: cat_kind(&r.get::<_, String>(3)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn category_id(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM categories WHERE name = ?1 ORDER BY parent_id IS NOT NULL LIMIT 1", [name], |r| r.get(0)).optional()?)
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
        let tx = self.conn.unchecked_transaction()?;
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
    /// to Uncategorized is left alone. Returns how many were filed.
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
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        for t in rows.iter().filter(|t| t.review != ReviewStatus::Confirmed) {
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
        // "Transfer to 2197": the account whose last four digits the description names is the likely other side
        let last4: std::collections::HashMap<i64, String> = self.accounts()?.into_iter().map(|a| (a.id, a.account_number_masked.trim_start_matches('•').to_string())).collect();
        let names = |a: &TxRow, b: &TxRow| last4.get(&b.account_id).is_some_and(|l| l.len() == 4 && names_account(&crate::merchants::tokens(&format!("{} {}", a.raw_description, a.memo)), l));
        let mut used = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for a in rows.iter().filter(|t| t.amount.is_sign_negative()) {
            if used.contains(&a.id) {
                continue;
            }
            let best = rows
                .iter()
                .filter(|b| b.amount.is_sign_positive() && b.account_id != a.account_id && !used.contains(&b.id))
                .filter(|b| (b.posted - a.posted).num_days().abs() <= 5)
                .filter(|b| rules::looks_like_transfer(&a.raw_description, &a.memo) || rules::looks_like_transfer(&b.raw_description, &b.memo) || names(a, b))
                .filter(|b| {
                    if a.currency == b.currency {
                        a.amount.abs() == b.amount
                    } else {
                        let conv = fx.convert(a.amount.abs(), a.currency, b.currency);
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
        let tx = self.conn.unchecked_transaction()?;
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
    /// one of your cards' last four digits ("Transfer to 2197") or says it pays a card ("VISA PMT",
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
        let tx = self.conn.unchecked_transaction()?;
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

    pub fn unlink_transfer(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE transactions SET transfer_peer_tx_id = NULL WHERE id = ?1 OR transfer_peer_tx_id = ?1", [id])?;
        Ok(())
    }

    /// Flag merchants charged on a steady cadence as recurring.
    pub fn mark_recurring(&self) -> Result<()> {
        let rows = self.transactions(&TxFilter::default())?;
        let mut by: std::collections::HashMap<(i64, String), Vec<&TxRow>> = Default::default();
        for t in rows.iter().filter(|t| t.amount.is_sign_negative() && !t.is_transfer()) {
            by.entry((t.account_id, rules::merchant_stem(&t.raw_description, &t.memo))).or_default().push(t);
        }
        let tx = self.conn.unchecked_transaction()?;
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
            n += self.import_scan(ent, &doc.filename, &format!("demo-{sha}"), &scan)?.inserted;
        }
        // a few answers already learned, so triage starts with the interesting merchants
        for (pattern, cat, payee) in [("SALARY", "Salary", "Island Air (Salary)"), ("RENT RECEIVED", "Rental Income", "Rent"), ("SERVICE CHARGE", "Bank Fees & Interest", "Scotiabank")] {
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

/// The description names an account: its last four digits on their own ("Transfer to 2197") or as the
/// end of a full card/account number ("Transfer to 10785 19544403575").
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

/// The amount in the workspace's currency: as printed if it already is; the statement's own FX
/// original if that's in the workspace currency; else the configured rate.
pub fn entity_amount(fx: &Fx, amount: Decimal, from: Currency, base: Currency, original: Option<(Currency, Decimal)>) -> Decimal {
    if from == base {
        return amount;
    }
    let original = original.filter(|_| crate::money::statement_fx_wins());
    match original {
        Some((c, v)) if c == base => {
            if amount.is_sign_negative() {
                -v.abs()
            } else {
                v.abs()
            }
        }
        _ => fx.convert(amount, from, base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let docs = db.documents(&[]).unwrap();
        let bad: Vec<&DocumentRow> = docs.iter().filter(|d| d.reconciliation != Reconciliation::Reconciled).collect();
        assert_eq!(bad.len(), 1, "only the deliberately garbled statement: {bad:?}");
        // fix it the way the inspector does, and it reconciles
        let Reconciliation::Discrepancy(gap) = bad[0].reconciliation else { panic!() };
        let (_, rep) = db.document_report(bad[0].id).unwrap();
        let i = rep.skipped.iter().position(|l| l.date.is_some()).unwrap();
        db.convert_skipped_line(bad[0].id, i, rep.skipped[i].date.unwrap(), gap, "INCOMING WIRE").unwrap();
        assert_eq!(db.recheck_document(bad[0].id, Currency::USD).unwrap(), Reconciliation::Reconciled);
        let rows = db.transactions(&TxFilter::default()).unwrap();
        let paired = rows.iter().filter(|t| t.transfer_peer.is_some()).count();
        // card payments, RBC savings, loan payments (12 each) and the 6 USD→KYD sweeps
        assert!(paired >= 42 * 2, "transfers pair up: {paired}");
        // re-importing the same statement is a no-op
        let profiles = db.profiles().unwrap();
        let doc = &crate::demo::demo_documents()[0];
        let scan = crate::parser::MultiPassScanner::new(&profiles).scan_tokens(doc.tokens.clone()).unwrap();
        let p = db.entity_by_name("Personal").unwrap().unwrap();
        let rep = db.import_scan(p, "x.pdf", "another-hash", &scan).unwrap();
        assert_eq!((rep.inserted, rep.duplicates), (0, scan.sections[0].transactions.len()));
        // HBO is a known service: filed as a subscription on import, so triage doesn't ask
        let hbo = db.transactions(&TxFilter { search: "HBO MAX".into(), ..Default::default() }).unwrap();
        assert_eq!(hbo.len(), 12);
        assert!(hbo.iter().all(|t| t.category_name.as_deref() == Some("Subscriptions & Digital") && t.is_subscription && t.review == ReviewStatus::AutoCategorized));
        assert!(db.triage(&[p]).unwrap().iter().all(|g| !g.stem.starts_with("HBO")));
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
        let words = crate::merchants::tokens("PC TRANSFER DEBIT Transfer to 10785 19544403575");
        assert!(names_account(&words, "3575") && !names_account(&words, "0785"));
        // a transfer to your own savings account isn't a card payment
        assert!(db.transactions(&TxFilter { search: "RBC SAVINGS 3390".into(), ..Default::default() }).unwrap().iter().all(|t| t.category_id != cc));
    }

    #[test]
    fn fx_original_wins_over_the_peg() {
        let fx = Fx::default();
        assert_eq!(entity_amount(&fx, dec("-72.39"), Currency::USD, Currency::KYD, Some((Currency::KYD, dec("59.14")))), dec("-59.14"));
        assert_eq!(entity_amount(&fx, dec("-120.00"), Currency::USD, Currency::KYD, None), dec("-100.00"));
    }
}
