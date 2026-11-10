//! Filing statement files the way people keep them: `2026/10 Oct/<the bank's own file name>`.
use chrono::NaiveDate;
use std::fs;
use std::path::{Path, PathBuf};

/// `2026/10 Oct`
pub fn month_folder(d: NaiveDate) -> PathBuf {
    PathBuf::from(d.format("%Y").to_string()).join(d.format("%m %b").to_string())
}

/// A file name that's safe to create anywhere: no folders, no control characters, not hidden.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() { "statement.pdf".into() } else { cleaned }
}

/// `name.pdf` → `name (2).pdf`
fn numbered(name: &str, n: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

#[cfg(unix)]
fn mode(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(m));
}
#[cfg(not(unix))]
fn mode(_: &Path, _: u32) {}

/// Copies `src` into `root/<YYYY>/<MM Mon>/<name>` and returns the path relative to `root`. The same
/// file already there is left as it is; a different file with the same name gets ` (2)`, ` (3)`, ….
pub fn file_copy(root: &Path, src: &Path, sha256: &str, name: &str, month: NaiveDate) -> std::io::Result<PathBuf> {
    let rel_dir = month_folder(month);
    let dir = root.join(&rel_dir);
    fs::create_dir_all(&dir)?;
    for d in [dir.as_path(), dir.parent().unwrap_or(root)] {
        mode(d, 0o750);
    }
    let name = safe_name(name);
    for n in 1.. {
        let candidate = if n == 1 { name.clone() } else { numbered(&name, n) };
        let dest = dir.join(&candidate);
        if dest.exists() {
            if fingraph_core::db::file_sha256(&dest).ok().as_deref() == Some(sha256) {
                return Ok(rel_dir.join(candidate));
            }
            continue;
        }
        // copied beside it first, so a half-written file never appears under the real name
        let tmp = dir.join(format!(".{candidate}.partial"));
        fs::copy(src, &tmp)?;
        mode(&tmp, 0o640);
        fs::rename(&tmp, &dest)?;
        return Ok(rel_dir.join(candidate));
    }
    unreachable!()
}

/// Moves a file somewhere else, also across mounts.
pub fn move_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }
    if fs::rename(src, dest).is_err() {
        fs::copy(src, dest)?;
        fs::remove_file(src)?;
    }
    Ok(())
}

/// A free name in `dir` for `name` (adding ` (2)` … when taken).
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let name = safe_name(name);
    (1..).map(|n| dir.join(if n == 1 { name.clone() } else { numbered(&name, n) })).find(|p| !p.exists()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_follow_the_owners_convention() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        assert_eq!(month_folder(d), PathBuf::from("2026/09 Sep"));
        assert_eq!(month_folder(NaiveDate::from_ymd_opt(2027, 1, 2).unwrap()), PathBuf::from("2027/01 Jan"));
    }

    #[test]
    fn names_are_kept_but_made_safe() {
        assert_eq!(safe_name("statement_Main Account_5311_June_15_26.pdf"), "statement_Main Account_5311_June_15_26.pdf");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(".hidden.pdf"), "hidden.pdf");
        assert_eq!(safe_name("a\u{7}b:c.pdf"), "a_b_c.pdf");
        assert_eq!(safe_name("  "), "statement.pdf");
    }

    #[test]
    fn same_file_is_not_copied_twice_and_a_different_one_is_numbered() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let a = src.path().join("a.pdf");
        let b = src.path().join("b.pdf");
        fs::write(&a, b"first").unwrap();
        fs::write(&b, b"second").unwrap();
        let sha = |p: &Path| fingraph_core::db::file_sha256(p).unwrap();
        let m = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let r1 = file_copy(root.path(), &a, &sha(&a), "October 2026 e-statement.pdf", m).unwrap();
        let r2 = file_copy(root.path(), &a, &sha(&a), "October 2026 e-statement.pdf", m).unwrap();
        let r3 = file_copy(root.path(), &b, &sha(&b), "October 2026 e-statement.pdf", m).unwrap();
        assert_eq!(r1, PathBuf::from("2026/10 Oct/October 2026 e-statement.pdf"));
        assert_eq!(r1, r2);
        assert_eq!(r3, PathBuf::from("2026/10 Oct/October 2026 e-statement (2).pdf"));
        assert_eq!(fs::read(root.path().join(&r3)).unwrap(), b"second");
        assert_eq!(fs::read_dir(root.path().join("2026/10 Oct")).unwrap().count(), 2, "no partial files left");
    }
}
