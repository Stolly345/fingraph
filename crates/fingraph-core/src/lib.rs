//! FinGraph core: everything except the GUI. Statement scanning (`parser`), the SQLite ledger (`db`),
//! merchant rules and Smart Triage (`rules`), analytics (`insights`), exporters (`export`) and a
//! deterministic demo data set (`demo`). Money is `rust_decimal::Decimal` everywhere; no floats.

pub mod db;
pub mod demo;
pub mod export;
pub mod insights;
pub mod merchants;
pub mod money;
pub mod parser;
pub mod prefs;
pub mod rules;
pub mod workflows;
pub mod budget;

pub use money::{Currency, Fx};
pub use rust_decimal::Decimal;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error(format!("database: {e}"))
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(format!("json: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccountType {
    Checking,
    Savings,
    CreditCard,
    ShareAccount,
    Loan,
}

impl AccountType {
    pub const ALL: [AccountType; 5] = [Self::Checking, Self::Savings, Self::CreditCard, Self::ShareAccount, Self::Loan];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "Checking",
            Self::Savings => "Savings",
            Self::CreditCard => "CreditCard",
            Self::ShareAccount => "ShareAccount",
            Self::Loan => "Loan",
        }
    }
    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|t| t.as_str() == s).unwrap_or(Self::Checking)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::CreditCard => "Credit Card",
            Self::ShareAccount => "Share Savings",
            t => t.as_str(),
        }
    }
    /// Cards and loans: the printed balance is what you owe, so a debit makes it go *up*.
    pub fn is_liability(self) -> bool {
        matches!(self, Self::CreditCard | Self::Loan)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityKind {
    Personal,
    Business,
    RentalProperty,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "Personal",
            Self::Business => "Business",
            Self::RentalProperty => "RentalProperty",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "Business" => Self::Business,
            "RentalProperty" => Self::RentalProperty,
            _ => Self::Personal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    NeedsReview,
    Confirmed,
    AutoCategorized,
}

impl ReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsReview => "NeedsReview",
            Self::Confirmed => "Confirmed",
            Self::AutoCategorized => "AutoCategorized",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "Confirmed" => Self::Confirmed,
            "AutoCategorized" => Self::AutoCategorized,
            _ => Self::NeedsReview,
        }
    }
}

/// Did the statement's own numbers add up?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reconciliation {
    Reconciled,
    Discrepancy(Decimal),
    /// No balances or totals printed to check against.
    Unverified,
}

impl Reconciliation {
    /// Stored as `Reconciled`, `Unverified` or `Discrepancy(-12.50)`.
    pub fn to_db(&self) -> String {
        match self {
            Self::Reconciled => "Reconciled".into(),
            Self::Unverified => "Unverified".into(),
            Self::Discrepancy(d) => format!("Discrepancy({d})"),
        }
    }
    pub fn from_db(s: &str) -> Self {
        match s {
            "Reconciled" => Self::Reconciled,
            s if s.starts_with("Discrepancy(") => s[12..s.len() - 1].parse().map(Self::Discrepancy).unwrap_or(Self::Unverified),
            _ => Self::Unverified,
        }
    }
}

/// `~/.local/share/fingraph` (honours `XDG_DATA_HOME`).
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("fingraph")
}

/// `~/.config/fingraph` (honours `XDG_CONFIG_HOME`).
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("fingraph")
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
    }
}
