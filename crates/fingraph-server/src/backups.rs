//! Nightly complete backups (ledger + statements + receipts, checked by SHA-256), kept 7 daily, 4 weekly
//! and 12 monthly.
use chrono::{Datelike, NaiveDate};
use fingraph_core::db::Db;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const PREFIX: &str = "fingraph-";

/// Writes today's backup. A complete one when every statement is there; otherwise the ledger alone (and
/// says why), so a missing file never means no backup at all.
pub fn write(db: &Db, dir: &Path, day: NaiveDate) -> Result<(PathBuf, Option<String>), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tar = dir.join(format!("{PREFIX}{day}.tar"));
    match db.backup_archive(&tar) {
        Ok(()) => {
            let _ = std::fs::remove_file(dir.join(format!("{PREFIX}{day}.db")));
            Ok((tar, None))
        }
        Err(complete) => {
            let only = dir.join(format!("{PREFIX}{day}.db"));
            db.backup_to(&only).map_err(|e| e.to_string())?;
            Ok((only, Some(complete.to_string())))
        }
    }
}

fn day_of(p: &Path) -> Option<NaiveDate> {
    let n = p.file_name()?.to_str()?;
    let stem = n.strip_prefix(PREFIX)?.split('.').next()?;
    stem.parse().ok()
}

/// Which backup days to keep: the newest 7, the newest of each of the last 4 weeks and the newest of
/// each of the last 12 months.
pub fn keep(days: &[NaiveDate]) -> BTreeSet<NaiveDate> {
    let mut sorted: Vec<NaiveDate> = days.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.dedup();
    let mut keep: BTreeSet<NaiveDate> = sorted.iter().take(7).copied().collect();
    let mut weeks = BTreeSet::new();
    let mut months = BTreeSet::new();
    for d in &sorted {
        let w = d.iso_week();
        if weeks.len() < 4 && weeks.insert((w.year(), w.week())) {
            keep.insert(*d);
        }
        if months.len() < 12 && months.insert((d.year(), d.month())) {
            keep.insert(*d);
        }
    }
    keep
}

/// Removes the backups `keep` doesn't want. Returns how many went.
pub fn prune(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let files: Vec<(PathBuf, NaiveDate)> = rd.flatten().map(|e| e.path()).filter_map(|p| day_of(&p).map(|d| (p, d))).collect();
    let wanted = keep(&files.iter().map(|(_, d)| *d).collect::<Vec<_>>());
    let mut n = 0;
    for (p, d) in files {
        if !wanted.contains(&d) && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// The newest backup, if any.
pub fn latest(dir: &Path) -> Option<(PathBuf, NaiveDate)> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter_map(|p| day_of(&p).map(|d| (p, d))).max_by_key(|(_, d)| *d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_week_of_days_a_month_of_weeks_and_a_year_of_months() {
        let start = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        let days: Vec<NaiveDate> = (0..700).map(|i| start + chrono::Duration::days(i)).collect();
        let k = keep(&days);
        let newest = *days.last().unwrap();
        for i in 0..7 {
            assert!(k.contains(&(newest - chrono::Duration::days(i))));
        }
        // 7 days + up to 3 more weeks + up to 11 more months (some overlap)
        assert!(k.len() <= 7 + 4 + 12 && k.len() >= 18, "{}", k.len());
        assert!(k.iter().min().unwrap() > &(newest - chrono::Duration::days(370)));
    }

    #[test]
    fn writes_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("ledger.db")).unwrap();
        let backups = dir.path().join("backups");
        let d = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let (p, note) = write(&db, &backups, d).unwrap();
        assert!(note.is_none(), "{note:?}");
        assert!(p.ends_with("fingraph-2026-11-10.tar"));
        for i in 1..40 {
            std::fs::write(backups.join(format!("fingraph-{}.tar", d - chrono::Duration::days(i))), b"x").unwrap();
        }
        prune(&backups);
        assert_eq!(latest(&backups).unwrap().1, d);
        let left = std::fs::read_dir(&backups).unwrap().count();
        assert!((7..=12).contains(&left), "{left}");
    }
}
