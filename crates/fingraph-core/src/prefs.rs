//! Preferences, stored as one JSON value in the ledger's `settings` table so they travel with
//! backups. Anything missing takes its default, so older ledgers just work.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    // General
    /// `last`, `consolidated`, or a workspace id.
    pub start_workspace: String,
    /// The workspace selection when the app last closed (`consolidated` or an id).
    pub last_workspace: String,
    /// A date-range label: This Month, Last Month, Last 90 Days, Year to Date, This Year, All Time.
    pub default_range: String,
    /// `system`, `light` or `dark`.
    pub appearance: String,
    /// The page that was open last (FinGraph reopens on it).
    pub last_page: String,
    /// Once a day, ask GitHub whether a newer release is out (only the release page is fetched).
    pub check_updates: bool,

    // Money
    /// A rate printed on the statement beats the configured exchange rate.
    pub statement_fx_wins: bool,
    /// `KYD 12.00` instead of `CI$ 12.00`.
    pub currency_codes: bool,
    /// `(CI$ 12.00)` instead of `-CI$ 12.00`.
    pub brackets: bool,
    /// Download exchange rates for currencies other than KYD and USD (Frankfurter, then the fawazahmed0 API).
    pub download_rates: bool,

    // Import & scanning
    /// Where imported PDFs are kept; `None` = `statements/` beside the ledger (`Db::statements_dir`).
    pub statements_dir: Option<PathBuf>,
    pub watch_enabled: bool,
    pub watch_folder: Option<PathBuf>,
    pub triage_after_import: bool,
    /// Same charge this many days apart (or fewer) is a possible double charge.
    pub duplicate_days: i64,

    // Triage & rules
    /// Treat truncated spellings (DISCOVERYDI HELP / HEL) as one merchant.
    pub fuzzy_merchants: bool,
    /// New rules apply to every workspace (otherwise just the one the charge came from).
    pub rules_all_workspaces: bool,
    pub ask_subscriptions: bool,
    /// Billing cycles you set for subscriptions: merchant → days between charges.
    pub subscription_cadence: std::collections::BTreeMap<String, i64>,
    /// File charges from businesses in the directory automatically.
    pub auto_directory: bool,
    /// Also file keyword guesses (MARKET → Groceries) instead of only suggesting them.
    pub auto_keywords: bool,
    /// Rescan keeps hand-made changes (categories, payees, tags, business flags) by transaction fingerprint.
    pub rescan_keeps_edits: bool,

    // Dashboard
    /// A bill this many percent above its average is a spike.
    pub spike_percent: i64,
    /// A category this many percent above its 3-month average is drifting.
    pub drift_percent: i64,
    pub history_months: i32,
    pub band_months: i32,
    /// Top-level categories left out of spending totals and charts.
    pub excluded_categories: Vec<String>,

    // Data
    /// Back up automatically before Reset, Rescan, deleting a workspace and erasing.
    pub auto_backup: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            start_workspace: "last".into(),
            last_workspace: String::new(),
            default_range: "This Year".into(),
            appearance: "system".into(),
            last_page: "overview".into(),
            check_updates: true,
            statement_fx_wins: true,
            currency_codes: false,
            brackets: false,
            download_rates: false,
            statements_dir: None,
            watch_enabled: false,
            watch_folder: None,
            triage_after_import: false,
            duplicate_days: 3,
            fuzzy_merchants: true,
            rules_all_workspaces: false,
            ask_subscriptions: true,
            subscription_cadence: Default::default(),
            auto_directory: true,
            auto_keywords: false,
            rescan_keeps_edits: true,
            spike_percent: 20,
            drift_percent: 10,
            history_months: 12,
            band_months: 6,
            excluded_categories: Vec::new(),
            auto_backup: true,
        }
    }
}

impl Prefs {
    /// The parts of the engine that read preferences as process-wide switches.
    pub fn apply_globals(&self) {
        crate::money::set_style(self.currency_codes, self.brackets, self.statement_fx_wins);
        crate::rules::set_fuzzy(self.fuzzy_merchants);
        crate::insights::set_cadence_overrides(&self.subscription_cadence);
    }
}
