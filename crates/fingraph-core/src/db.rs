//! The ledger: SQLite in WAL mode at `~/.local/share/fingraph/ledger.db`. Money is stored twice,
//! as the exact decimal string and as integer minor units, always next to its ISO currency.

use chrono::{Local, NaiveDate};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::str::FromStr;

use crate::money::{to_minor, Fx};
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
"#];

/// (name, kind, children)
const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("Income", "Income", &["Salary", "Rental Income", "Business Income", "Interest & Dividends", "Refunds"]),
    ("Groceries", "Expense", &[]),
    ("Dining & Takeaway", "Expense", &[]),
    ("Utilities", "Expense", &["Electricity", "Water", "Phone & Internet", "Gas"]),
    ("Transport", "Expense", &["Fuel", "Taxi & Rideshare"]),
    ("Subscriptions & Digital", "Expense", &[]),
    ("Shopping", "Expense", &[]),
    ("Home & Garden", "Expense", &[]),
    ("Health & Fitness", "Expense", &[]),
    ("Insurance", "Expense", &[]),
    ("Bank Fees & Interest", "Expense", &[]),
    ("Government & Fees", "Expense", &[]),
    ("Travel", "Expense", &[]),
    ("Repairs & Maintenance", "Expense", &[]),
    ("Property Expense", "Expense", &[]),
    ("Professional Services", "Expense", &[]),
    ("Cash Withdrawal", "Expense", &[]),
    ("Gifts & Donations", "Expense", &[]),
    ("Transfers", "Transfer", &[]),
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
#[derive(Debug, Clone)]
pub struct TxRow {
    pub id: i64,
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

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or_default()
}
fn opt_dec(s: Option<String>) -> Option<Decimal> {
    s.and_then(|s| Decimal::from_str(&s).ok())
}
fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}
fn ccy(s: &str) -> Currency {
    Currency::parse(s).unwrap_or(Currency::KYD)
}
fn cat_kind(s: &str) -> CategoryKind {
    match s {
        "Income" => CategoryKind::Income,
        "Transfer" => CategoryKind::Transfer,
        _ => CategoryKind::Expense,
    }
}
fn now() -> String {
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

const TX_SELECT: &str = "SELECT t.id, t.document_id, t.account_id, a.name, a.account_type, COALESCE(bp.short_name, ''), a.entity_id, e.base_currency,
    t.posted_date, t.effective_date, t.raw_description, t.clean_payee, t.amount, t.currency, t.amount_in_entity_currency, t.running_balance_reported,
    t.category_id, c.name, c.kind, t.sub_entity_tag, t.memo, t.fitid, t.is_recurring, t.is_subscription, t.transfer_peer_tx_id, t.review_status
    FROM transactions t JOIN accounts a ON a.id = t.account_id JOIN entities e ON e.id = a.entity_id
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
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        db.seed()?;
        Ok(db)
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
        let fx = Fx::default();
        self.conn.execute("INSERT OR IGNORE INTO fx_rates VALUES ('KYD', 'USD', ?1)", [fx.kyd_to_usd.to_string()])?;
        self.conn.execute("INSERT OR IGNORE INTO fx_rates VALUES ('USD', 'KYD', ?1)", [fx.usd_to_kyd.to_string()])?;
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
        if n == 0 {
            for (name, kind, kids) in CATEGORIES {
                self.conn.execute("INSERT INTO categories (name, parent_id, kind) VALUES (?1, NULL, ?2)", params![name, kind])?;
                let pid = self.conn.last_insert_rowid();
                for k in *kids {
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
    fn revalue(&self, entity: Option<i64>) -> Result<()> {
        let fx = self.fx()?;
        let rows: Vec<(i64, String, String, String, String)> = {
            let mut st = self.conn.prepare(
                "SELECT t.id, t.amount, t.currency, e.base_currency, t.memo FROM transactions t JOIN accounts a ON a.id = t.account_id JOIN entities e ON e.id = a.entity_id WHERE (?1 IS NULL OR e.id = ?1)",
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
            "SELECT a.id, a.entity_id, a.bank_profile_id, COALESCE(bp.short_name, ''), a.name, a.account_number_masked, a.account_type, a.currency, a.ofx_bank_id, a.ofx_intu_bid
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_account(&self, a: &Account) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET name = ?2, ofx_bank_id = ?3, ofx_intu_bid = ?4, entity_id = ?5 WHERE id = ?1",
            params![a.id, a.name, a.ofx_bank_id, a.ofx_intu_bid, a.entity_id],
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
                let rule = rules::first_match(&rules, entity_id, &t.description, &memo);
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
        self.mark_recurring()?;
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
        self.conn.execute("DELETE FROM documents WHERE sha256_hash = ?1", [sha256])?;
        self.import_scan(entity_id, filename, sha256, scan)
    }

    /// Re-read every statement whose PDF can still be found with the current scanner and profiles.
    /// Returns (statements rescanned, transactions imported, files that couldn't be found).
    pub fn rescan_all(&self) -> Result<(usize, usize, Vec<String>)> {
        let profiles = self.profiles()?;
        let scanner = crate::parser::MultiPassScanner::new(&profiles);
        let (mut n, mut txs, mut missing) = (0, 0, Vec::new());
        for (sha, name, entity) in self.statement_files()? {
            let Some(path) = stored_pdf(&sha, &name) else {
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
                    "UPDATE transactions SET category_id = NULL, clean_payee = ?2, is_subscription = 0, review_status = 'NeedsReview' WHERE id = ?1",
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
        let mut sql = format!("{TX_SELECT} WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !f.entity_ids.is_empty() {
            sql += &format!(" AND a.entity_id IN ({})", f.entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(","));
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
        let mut st = self.conn.prepare("SELECT id, name, parent_id, kind FROM categories ORDER BY COALESCE(parent_id, id), parent_id IS NOT NULL, name")?;
        let rows = st.query_map([], |r| Ok(Category { id: r.get(0)?, name: r.get(1)?, parent_id: r.get(2)?, kind: cat_kind(&r.get::<_, String>(3)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn category_id(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT id FROM categories WHERE name = ?1 ORDER BY parent_id IS NOT NULL LIMIT 1", [name], |r| r.get(0)).optional()?)
    }

    pub fn add_category(&self, name: &str, parent_id: Option<i64>, kind: CategoryKind) -> Result<i64> {
        let k = match kind {
            CategoryKind::Income => "Income",
            CategoryKind::Expense => "Expense",
            CategoryKind::Transfer => "Transfer",
        };
        self.conn.execute("INSERT INTO categories (name, parent_id, kind) VALUES (?1, ?2, ?3)", params![name, parent_id, k])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        let mut st = self.conn.prepare("SELECT id, kind, pattern, clean_payee, category_id, is_subscription, is_transfer, expected_cadence, entity_id FROM learned_rules ORDER BY id")?;
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Save a rule and apply it to every matching transaction already in the ledger
    /// (except ones the user confirmed by hand). Returns how many rows changed.
    pub fn learn(&self, rule: &Rule) -> Result<usize> {
        self.conn.execute(
            "INSERT INTO learned_rules (kind, pattern, clean_payee, category_id, is_subscription, is_transfer, expected_cadence, entity_id, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![rule.kind.as_str(), rule.pattern, rule.clean_payee, rule.category_id, rule.is_subscription, rule.is_transfer, rule.expected_cadence, rule.entity_id, now()],
        )?;
        let mut saved = rule.clone();
        saved.id = self.conn.last_insert_rowid();
        self.apply_rule(&saved, false)
    }

    /// `force`: also overwrite rows marked Confirmed (the wizard's answers are confirmations).
    pub fn apply_rule(&self, rule: &Rule, force: bool) -> Result<usize> {
        let rows = self.transactions(&TxFilter { entity_ids: rule.entity_id.into_iter().collect(), ..Default::default() })?;
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        for t in rows.iter().filter(|t| rule.matches(&t.raw_description, &t.memo)) {
            if t.review == ReviewStatus::Confirmed && !force && t.category_id.is_some() {
                continue;
            }
            tx.execute(
                "UPDATE transactions SET category_id = COALESCE(?2, category_id), clean_payee = COALESCE(?3, clean_payee), is_subscription = ?4, is_recurring = is_recurring OR ?4,
                 review_status = ?5 WHERE id = ?1",
                params![t.id, rule.category_id, rule.clean_payee, rule.is_subscription, if force { "Confirmed" } else { "AutoCategorized" }],
            )?;
            n += 1;
        }
        tx.commit()?;
        if rule.is_transfer {
            self.match_transfers()?;
        }
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
        let mut used = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for a in rows.iter().filter(|t| t.amount.is_sign_negative()) {
            if used.contains(&a.id) {
                continue;
            }
            let best = rows
                .iter()
                .filter(|b| b.amount.is_sign_positive() && b.account_id != a.account_id && !used.contains(&b.id))
                .filter(|b| (b.posted - a.posted).num_days().abs() <= 4)
                .filter(|b| rules::looks_like_transfer(&a.raw_description, &a.memo) || rules::looks_like_transfer(&b.raw_description, &b.memo))
                .filter(|b| {
                    if a.currency == b.currency {
                        a.amount.abs() == b.amount
                    } else {
                        let conv = fx.convert(a.amount.abs(), a.currency, b.currency);
                        (conv - b.amount).abs() <= conv * Decimal::new(2, 2)
                    }
                })
                .min_by_key(|b| ((b.posted - a.posted).num_days().abs(), (b.amount - a.amount.abs()).abs()));
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

    pub fn triage(&self, entity_ids: &[i64]) -> Result<Vec<rules::TriageGroup>> {
        let rows = self.transactions(&TxFilter { entity_ids: entity_ids.to_vec(), uncategorized_only: true, ..Default::default() })?;
        let items: Vec<rules::TriageInput> = rows
            .iter()
            .map(|t| rules::TriageInput { id: t.id, entity_id: t.entity_id, posted: t.posted, raw: t.raw_description.clone(), memo: t.memo.clone(), amount: t.amount, currency: t.currency })
            .collect();
        Ok(rules::triage_groups(&items))
    }

    /// Mark a triage group as internal transfers: Transfers category, then pair them up.
    pub fn mark_transfers(&self, ids: &[i64], stem: &str, entity_id: Option<i64>) -> Result<usize> {
        let cat = self.category_id("Transfers")?;
        self.set_category(ids, cat)?;
        let rule = Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: stem.into(), clean_payee: None, category_id: cat, is_subscription: false, is_transfer: true, expected_cadence: None, entity_id };
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
            self.learn(&Rule { id: 0, kind: RuleKind::Substring, pattern: pattern.into(), clean_payee: Some(payee.into()), category_id, is_subscription: false, is_transfer: false, expected_cadence: None, entity_id: None })?;
        }
        for (pattern, tag) in [("UNIT A", "Unit A"), ("UNIT B", "Unit B")] {
            let ids: Vec<i64> = self.transactions(&TxFilter { entity_ids: vec![business], search: pattern.into(), ..Default::default() })?.iter().map(|t| t.id).collect();
            self.set_tag(&ids, tag)?;
        }
        Ok(n)
    }
}

/// Where imported PDFs are kept, named by their SHA-256, so statements can be rescanned later.
pub fn statements_dir() -> std::path::PathBuf {
    crate::data_dir().join("statements")
}

/// Keep a copy of an imported PDF (no-op when it's already there).
pub fn keep_pdf(src: &Path, sha256: &str) -> Result<()> {
    let dir = statements_dir();
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(format!("{sha256}.pdf"));
    if !dst.exists() {
        std::fs::copy(src, &dst)?;
    }
    Ok(())
}

/// A statement's PDF: our kept copy, else a file of the same name and contents in the data folder.
pub fn stored_pdf(sha256: &str, filename: &str) -> Option<std::path::PathBuf> {
    let kept = statements_dir().join(format!("{sha256}.pdf"));
    if kept.exists() {
        return Some(kept);
    }
    let old = crate::data_dir().join("documents").join(filename);
    (file_sha256(&old).ok().as_deref() == Some(sha256)).then_some(old)
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
        // HBO is waiting in triage as a subscription
        let groups = db.triage(&[p]).unwrap();
        let hbo = groups.iter().find(|g| g.stem.starts_with("HBO")).expect("HBO group");
        assert_eq!(hbo.kind, rules::TriageKind::Subscription);
        assert_eq!(hbo.count, 12);
    }

    #[test]
    fn learning_applies_retroactively() {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        let cat = db.category_id("Utilities").unwrap();
        let n = db.learn(&Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: "CUC".into(), clean_payee: Some("CUC".into()), category_id: cat, is_subscription: false, is_transfer: false, expected_cadence: None, entity_id: None }).unwrap();
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
        db.learn(&Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: "KIRK MARKET".into(), clean_payee: Some("Kirk".into()), category_id: cat, is_subscription: true, is_transfer: false, expected_cadence: None, entity_id: None }).unwrap();
        assert!(db.uncategorized_count(&[]).unwrap() < before);
        db.reset_learning(&[]).unwrap();
        assert!(db.rules().unwrap().is_empty());
        let rows = db.transactions(&TxFilter::default()).unwrap();
        assert!(rows.iter().all(|t| !t.is_subscription && (t.transfer_peer.is_some() || t.category_id.is_none())));
        assert!(rows.iter().any(|t| t.transfer_peer.is_some()), "transfer pairs survive");
        assert!(db.uncategorized_count(&[]).unwrap() >= before);
    }

    #[test]
    fn fx_original_wins_over_the_peg() {
        let fx = Fx::default();
        assert_eq!(entity_amount(&fx, dec("-72.39"), Currency::USD, Currency::KYD, Some((Currency::KYD, dec("59.14")))), dec("-59.14"));
        assert_eq!(entity_amount(&fx, dec("-120.00"), Currency::USD, Currency::KYD, None), dec("-100.00"));
    }
}
