//! Phase 0: proves the core runs on the phone (scanner, import and SQLite), nothing more.
use fingraph_core::db::Db;
use fingraph_core::merchants;

pub struct CoreCheck {
    pub version: String,
    /// Businesses in the built-in directory.
    pub directory: u32,
    /// Demo statements scanned and imported into an in-memory ledger.
    pub demo_transactions: u32,
    /// How a sample statement line is recognised.
    pub sample: String,
}

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

pub fn core_check() -> Result<CoreCheck, String> {
    let db = Db::open_in_memory().map_err(|e| e.to_string())?;
    let demo_transactions = db.load_demo().map_err(|e| e.to_string())? as u32;
    let line = "NETFLIX.COM LOS GATOS CA";
    let sample = match merchants::find(line) {
        Some(f) => format!("{line} → {} ({})", f.merchant.name, f.merchant.category),
        None => format!("{line} → not recognised"),
    };
    Ok(CoreCheck { version: env!("CARGO_PKG_VERSION").into(), directory: merchants::DIRECTORY.len() as u32, demo_transactions, sample })
}
