//! Schema migrations and built-in categories.
pub(super) const MIGRATIONS: &[&str] = &[r#"
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
"#, r#"
-- a statement that doesn't reconcile, was read by OCR, or comes from a layout not yet checked on a real
-- statement waits for review: its transactions stay out of every view until it's approved
ALTER TABLE documents ADD COLUMN review_hold INTEGER NOT NULL DEFAULT 0;
ALTER TABLE documents ADD COLUMN review_note TEXT NOT NULL DEFAULT '';
-- a bank's declared layout (row shape, summary labels, verified account types), JSON
ALTER TABLE bank_profiles ADD COLUMN layout TEXT NOT NULL DEFAULT '{}';
"#, r#"
-- other currencies' daily rates (units per US dollar), downloaded once and kept: past rates never change
CREATE TABLE fx_daily (
    day TEXT NOT NULL,
    currency TEXT NOT NULL,
    per_usd TEXT NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY (day, currency)
);
"#, r#"
-- every statement that lists a transaction, not only the one that brought it in: deleting one of two
-- overlapping statements keeps the rows the other also lists, and a held copy can't hide a clean one's rows
CREATE TABLE shared_rows (
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    transaction_id INTEGER NOT NULL REFERENCES transactions(id) ON DELETE CASCADE,
    PRIMARY KEY (document_id, transaction_id)
);
-- rows you deleted stay deleted when their statement is rescanned or imported again
CREATE TABLE deleted_rows (account_id INTEGER NOT NULL, fitid TEXT NOT NULL, PRIMARY KEY (account_id, fitid));
-- transfer pairs you unlinked stay unlinked
CREATE TABLE unpaired (account_a INTEGER NOT NULL, fitid_a TEXT NOT NULL, account_b INTEGER NOT NULL, fitid_b TEXT NOT NULL, PRIMARY KEY (account_a, fitid_a, account_b, fitid_b));
-- a memo you wrote survives a rescan
ALTER TABLE transactions ADD COLUMN memo_edited INTEGER NOT NULL DEFAULT 0;
-- left behind by workspaces and accounts deleted before ids stopped being reused
DELETE FROM manual_values WHERE item_id IN (SELECT id FROM manual_items WHERE entity_id NOT IN (SELECT id FROM entities));
DELETE FROM manual_items WHERE entity_id NOT IN (SELECT id FROM entities);
DELETE FROM attachments WHERE account_id NOT IN (SELECT id FROM accounts);
DELETE FROM reimbursements WHERE account_id NOT IN (SELECT id FROM accounts);
DELETE FROM settings WHERE key LIKE 'apr:%' AND CAST(substr(key, 5) AS INTEGER) NOT IN (SELECT id FROM accounts);
"#, r#"
-- your own icon and colour for a category; empty means its default
ALTER TABLE categories ADD COLUMN icon TEXT;
ALTER TABLE categories ADD COLUMN color TEXT;
"#];

/// (name, kind, children). Missing ones are added to existing ledgers on open; lists are shown A–Z.
pub(super) const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("Bank Fees & Interest", "Expense", &["Account Fees", "Credit Card Interest", "FX & Wire Fees", "Loan Interest"]),
    ("Business Expense", "Expense", &["Advertising & Marketing", "Contractors & Payroll", "Licences & Permits", "Office Supplies", "Software & Tools"]),
    ("Cash Withdrawal", "Expense", &[]),
    ("Clothing & Shoes", "Expense", &[]),
    ("Dining & Takeaway", "Expense", &["Coffee & Snacks", "Restaurants", "Takeaway & Delivery"]),
    ("Education", "Expense", &["Books & Courses", "School Fees & Tuition"]),
    ("Entertainment", "Expense", &["Events & Tickets", "Hobbies", "Movies & Streaming"]),
    ("Family Expense", "Expense", &["Allowances", "Baby & Kids' Supplies", "Childcare & Nanny", "Family Support & Remittances", "Kids' Activities", "School Supplies & Uniforms"]),
    ("Gifts & Donations", "Expense", &["Charity", "Church & Tithes", "Gifts"]),
    ("Government & Fees", "Expense", &["Import Duty", "Stamp Duty", "Taxes", "Vehicle Licensing", "Work Permits & Immigration"]),
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

