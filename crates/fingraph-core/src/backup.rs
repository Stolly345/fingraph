//! Portable ledger + document backups. Restore validates before replacing anything.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use serde::{Deserialize, Serialize};
use chrono::Local;
use rusqlite::Connection;
use crate::db::{Db, file_sha256, private};
use crate::{Error, Result};

#[derive(Serialize, Deserialize)]
struct Manifest { version: u32, files: Vec<BackupFile> }
#[derive(Serialize, Deserialize)]
struct BackupFile { path: String, sha256: String }

fn safe_path(path: &Path) -> bool {
    let parts: Vec<_> = path.components().collect();
    match parts.as_slice() {
        [Component::Normal(name)] => *name == "ledger.db" || *name == "manifest.json",
        [Component::Normal(dir), Component::Normal(_)] => *dir == "statements" || *dir == "attachments",
        _ => false,
    }
}

impl Db {
    /// A consistent copy of the whole ledger (settings and preferences included).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() && self.path().and_then(|p| p.canonicalize().ok()) == dest.canonicalize().ok() {
            return Err(Error("Choose a backup destination other than the live ledger".into()));
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // written beside it first, so a failed backup never costs the one already there
        let tmp = dest.with_extension("db-partial");
        let _ = std::fs::remove_file(&tmp);
        self.conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()])?;
        private(&tmp, 0o600);
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }

    pub fn backups_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.path()?.parent()?.join("backups"))
    }

    /// Before something drastic: a timestamped backup, keeping the newest ten. Off in Preferences = no-op.
    pub fn auto_backup(&self, reason: &str) -> Result<Option<std::path::PathBuf>> {
        let Some(dir) = self.backups_dir().filter(|_| self.prefs().auto_backup) else { return Ok(None) };
        let dest = dir.join(format!("ledger-{}-{reason}.tar", Local::now().format("%Y%m%d-%H%M%S")));
        self.backup_archive(&dest)?;
        let mut old: Vec<_> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ledger-"))).collect();
        old.sort();
        for p in old.iter().rev().skip(10) {
            let _ = std::fs::remove_file(p);
        }
        Ok(Some(dest))
    }

    /// Replace the ledger's contents with a backup's (then bring it up to this version's schema).
    pub fn restore_from(&self, src: &Path) -> Result<()> {
        let mut signature = [0; 16];
        use std::io::Read;
        let mut f = std::fs::File::open(src)?;
        if f.read_exact(&mut signature).is_ok() && signature == *b"SQLite format 3\0" {
            self.restore_database_from(src)
        } else { self.restore_archive(src) }
    }

    pub(crate) fn restore_database_from(&self, src: &Path) -> Result<()> {
        Self::validate_backup_database(src)?;
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


    /// Complete backup: a consistent ledger snapshot plus every referenced statement and receipt.
    /// Refuses to call a backup complete if a referenced source document is missing.
    pub fn backup_archive(&self, dest: &Path) -> Result<()> {
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        if self.path().and_then(|p| p.canonicalize().ok()) == dest.canonicalize().ok() && dest.exists() {
            return Err(Error("Choose a backup destination other than the live ledger".into()));
        }
        let stage = tempfile::tempdir_in(parent)?;
        self.backup_to(&stage.path().join("ledger.db"))?;
        let snapshot = rusqlite::Connection::open_with_flags(stage.path().join("ledger.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut sources = vec![("ledger.db".to_string(), stage.path().join("ledger.db"))];
        let mut st = snapshot.prepare("SELECT DISTINCT sha256_hash, filename FROM documents")?;
        let docs = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (sha, name) in docs {
            if sha.starts_with("demo-") { continue; }
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(Error(format!("Invalid statement identifier for {name}"))); }
            let src = self.stored_pdf(&sha, &name).ok_or_else(|| Error(format!("Complete backup needs the missing statement {name}. Re-import it, or choose a database-only backup.")))?;
            if file_sha256(&src)? != sha { return Err(Error(format!("The kept statement {name} no longer matches its original contents"))); }
            sources.push((format!("statements/{sha}.pdf"), src));
        }
        let mut st = snapshot.prepare("SELECT DISTINCT stored FROM attachments")?;
        for name in st.query_map([], |r| r.get::<_, String>(0))? {
            let name = name?;
            let rel = format!("attachments/{name}");
            if !safe_path(Path::new(&rel)) { return Err(Error("Invalid receipt path in ledger".into())); }
            sources.push((rel, self.attachments_dir().join(name)));
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0)); sources.dedup_by(|a, b| a.0 == b.0);
        let mut archive_file = tempfile::NamedTempFile::new_in(parent)?;
        private(archive_file.path(), 0o600);
        let mut archive = tar::Builder::new(archive_file.as_file_mut());
        let mut manifest = Manifest { version: 1, files: Vec::new() };
        for (rel, src) in sources {
            if !fs::symlink_metadata(&src)?.is_file() { return Err(Error(format!("Backup source is not a regular file: {}", src.display()))); }
            // Copy into staging so the hash describes exactly the bytes placed in the archive.
            let copy = stage.path().join(&rel);
            if src != copy {
                if let Some(dir) = copy.parent() { fs::create_dir_all(dir)?; }
                fs::copy(&src, &copy)?; private(&copy, 0o600);
            }
            manifest.files.push(BackupFile { path: rel.clone(), sha256: file_sha256(&copy)? });
            archive.append_path_with_name(&copy, &rel)?;
        }
        let bytes = serde_json::to_vec(&manifest)?;
        let mut header = tar::Header::new_gnu(); header.set_size(bytes.len() as u64); header.set_mode(0o600); header.set_cksum();
        archive.append_data(&mut header, "manifest.json", bytes.as_slice())?;
        archive.finish()?; drop(archive);
        archive_file.as_file().sync_all()?;
        archive_file.persist(dest).map_err(|e| Error(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn restore_archive(&self, src: &Path) -> Result<()> {
        struct Styles<'a>(&'a Db);
        impl Drop for Styles<'_> { fn drop(&mut self) { self.0.prefs().apply_globals(); } }
        let _styles = Styles(self);
        let home = self.path().and_then(|p| p.parent().map(Path::to_path_buf)).ok_or_else(|| Error("A complete backup needs a file-backed ledger".into()))?;
        let stage = tempfile::tempdir_in(&home)?;
        let mut archive = tar::Archive::new(File::open(src)?);
        let mut found = BTreeSet::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let rel = entry.path()?.into_owned();
            if !safe_path(&rel) || !entry.header().entry_type().is_file() || !found.insert(rel.clone()) {
                return Err(Error("Backup contains an unsafe or duplicate entry".into()));
            }
            let dest = stage.path().join(rel);
            fs::create_dir_all(dest.parent().unwrap())?;
            let mut file = File::create(&dest)?; private(&dest, 0o600);
            std::io::copy(&mut entry, &mut file)?; file.flush()?;
        }
        let manifest: Manifest = serde_json::from_reader(File::open(stage.path().join("manifest.json"))?.take(16 * 1024 * 1024))?;
        if manifest.version != 1 { return Err(Error("This backup format needs a newer FinGraph version".into())); }
        let mut listed = BTreeSet::from([PathBuf::from("manifest.json")]);
        for f in &manifest.files {
            let rel = Path::new(&f.path);
            if !safe_path(rel) || !listed.insert(rel.to_path_buf()) || file_sha256(&stage.path().join(rel))? != f.sha256 {
                return Err(Error("Backup failed its document integrity check".into()));
            }
        }
        if listed != found || !found.contains(Path::new("ledger.db")) { return Err(Error("Backup contents do not match its manifest".into())); }
        // Verify and migrate an isolated copy before touching the user's ledger or documents.
        Self::validate_backup_database(&stage.path().join("ledger.db"))?;
        {
            let check = Db::open(&stage.path().join("ledger.db"))?;
            let docs = check.documents_all(&[])?;
            for d in docs.iter().filter(|d| !d.filename.is_empty()) {
                let sha: String = check.conn.query_row("SELECT sha256_hash FROM documents WHERE id = ?1", [d.id], |r| r.get(0))?;
                if !sha.starts_with("demo-") && !found.contains(Path::new(&format!("statements/{sha}.pdf"))) { return Err(Error("Backup is missing a referenced statement".into())); }
            }
            let mut st = check.conn.prepare("SELECT DISTINCT stored FROM attachments")?;
            for stored in st.query_map([], |r| r.get::<_, String>(0))? {
                if !found.contains(Path::new(&format!("attachments/{}", stored?))) { return Err(Error("Backup is missing a referenced receipt".into())); }
            }
        }
        let rollback_db = stage.path().join("previous.db"); self.backup_to(&rollback_db)?;
        let mut installed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let mut ledger_changed = false;
        let result = (|| -> Result<()> {
            for name in ["statements", "attachments"] {
                let new = stage.path().join(name); fs::create_dir_all(&new)?; private(&new, 0o700);
                let dest = home.join(name);
                let old = if dest.exists() {
                    let old = stage.path().join(format!("previous-{name}")); fs::rename(&dest, &old)?; Some(old)
                } else { None };
                installed.push((dest.clone(), old));
                fs::rename(new, dest)?;
            }
            ledger_changed = true;
            self.restore_database_from(&stage.path().join("ledger.db"))?;
            // Paths from another computer must not point to its external statement folder.
            let mut p = self.prefs(); p.statements_dir = None; self.save_prefs(&p)?;
            Ok(())
        })();
        if let Err(error) = result {
            let mut rollback_error = None;
            if ledger_changed && let Err(e) = self.restore_database_from(&rollback_db) { rollback_error = Some(e); }
            for (dest, old) in installed.into_iter().rev() {
                if dest.exists() && let Err(e) = fs::remove_dir_all(&dest) { rollback_error = Some(e.into()); continue; }
                if let Some(old) = old && let Err(e) = fs::rename(old, dest) { rollback_error = Some(e.into()); }
            }
            if let Some(e) = rollback_error {
                let recovery = stage.keep();
                return Err(Error(format!("{error}; recovery also failed ({e}). Previous files are preserved at {}", recovery.display())));
            }
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_backup_restores_documents_and_receipts_to_a_different_directory() {
        let source = tempfile::tempdir().unwrap(); let dest = tempfile::tempdir().unwrap();
        let db = Db::open(&source.path().join("ledger.db")).unwrap(); db.load_demo().unwrap();
        let pdf = source.path().join("original.pdf"); fs::write(&pdf, b"statement contents").unwrap();
        let sha = file_sha256(&pdf).unwrap(); db.keep_pdf(&pdf, &sha).unwrap();
        let t = db.transactions(&Default::default()).unwrap().remove(0);
        db.conn.execute("UPDATE documents SET sha256_hash=?1 WHERE id=?2", rusqlite::params![sha, t.document_id]).unwrap();
        let receipt = source.path().join("receipt.pdf"); fs::write(&receipt, b"receipt contents").unwrap(); db.attach(t.id, &receipt).unwrap();
        let archive = source.path().join("complete.tar"); db.backup_archive(&archive).unwrap();
        let restored = Db::open(&dest.path().join("ledger.db")).unwrap(); restored.restore_from(&archive).unwrap();
        assert_eq!(fs::read(restored.stored_pdf(&sha, "original.pdf").unwrap()).unwrap(), b"statement contents");
        assert_eq!(fs::read(&restored.attachments(t.id).unwrap()[0].path).unwrap(), b"receipt contents");
        assert_eq!(restored.transactions(&Default::default()).unwrap().len(), db.transactions(&Default::default()).unwrap().len());
        assert_eq!(restored.statements_dir(), dest.path().join("statements"));
        restored.set_setting("keep-after-damage", "yes").unwrap();
        let mut bytes = fs::read(&archive).unwrap();
        let offset = bytes.windows(b"receipt contents".len()).position(|w| w == b"receipt contents").unwrap();
        bytes[offset] = b'R';
        let damaged = source.path().join("damaged.tar"); fs::write(&damaged, bytes).unwrap();
        assert!(restored.restore_from(&damaged).is_err());
        assert_eq!(restored.setting("keep-after-damage").as_deref(), Some("yes"));
        assert_eq!(fs::read(&restored.attachments(t.id).unwrap()[0].path).unwrap(), b"receipt contents");
    }

    #[test]
    fn a_late_restore_failure_puts_the_original_ledger_and_files_back() {
        let source = tempfile::tempdir().unwrap(); let dest = tempfile::tempdir().unwrap();
        let incoming = Db::open(&source.path().join("ledger.db")).unwrap();
        incoming.save_prefs(&incoming.prefs()).unwrap();
        incoming.conn.execute_batch("CREATE TRIGGER reject_restore_preferences BEFORE INSERT ON settings WHEN NEW.key='preferences' BEGIN SELECT RAISE(ABORT, 'simulated late restore failure'); END;").unwrap();
        let backup = source.path().join("complete.tar"); incoming.backup_archive(&backup).unwrap();
        let original = Db::open(&dest.path().join("ledger.db")).unwrap(); original.set_setting("keep", "yes").unwrap();
        fs::create_dir_all(original.statements_dir()).unwrap();
        fs::write(original.statements_dir().join("existing.txt"), b"keep this document").unwrap();
        assert!(original.restore_from(&backup).is_err());
        assert_eq!(original.setting("keep").as_deref(), Some("yes"));
        assert_eq!(fs::read(original.statements_dir().join("existing.txt")).unwrap(), b"keep this document");
    }

    #[test]
    fn unsafe_archive_leaves_existing_ledger_untouched() {
        let dir = tempfile::tempdir().unwrap(); let db = Db::open(&dir.path().join("ledger.db")).unwrap();
        db.set_setting("keep", "yes").unwrap();
        let file = dir.path().join("bad.tar"); let mut tar = tar::Builder::new(File::create(&file).unwrap());
        let mut h = tar::Header::new_gnu(); h.set_size(0); h.set_entry_type(tar::EntryType::Symlink); h.set_link_name("/tmp/elsewhere").unwrap(); h.set_cksum();
        tar.append_data(&mut h, "statements/link", std::io::empty()).unwrap(); tar.finish().unwrap();
        assert!(db.restore_from(&file).is_err()); assert_eq!(db.setting("keep").as_deref(), Some("yes"));
    }
}
