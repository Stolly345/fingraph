//! The inbox: PDFs dropped into `/statements/inbox` are read, imported with the same checks as the
//! desktop's watched folder, and filed under their month. Anything that needs a person goes to
//! `needs-attention/` with a note saying why.
use crate::filing;
use crate::state::State;
use chrono::{Local, NaiveDate};
use fingraph_core::db::{Db, Target};
use fingraph_core::parser::ScanResult;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Imported and filed at this path (relative to the statements folder).
    Imported { filed: PathBuf, inserted: usize, held: usize },
    /// Already in the ledger; the copy was filed (or was already there) and removed from the inbox.
    AlreadyImported { filed: PathBuf },
    /// Moved to `needs-attention/`, with the reason beside it.
    NeedsAttention { reason: String },
}

/// Which statements in the inbox are ready to look at: PDFs that haven't changed for `settle` (a file
/// still arriving over the network keeps changing).
pub fn ready_files(inbox: &Path, settle: std::time::Duration) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(inbox) else { return Vec::new() };
    let mut v: Vec<PathBuf> = dir
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            !n.starts_with('.') && n.to_lowercase().ends_with(".pdf")
        })
        .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() >= settle))
        .collect();
    v.sort();
    v
}

/// The month a statement is filed under: its closing date, else the latest period the ledger has for
/// it, else today.
pub fn filing_month(scan: Option<&ScanResult>, db: &Db, sha256: &str) -> NaiveDate {
    scan.and_then(|s| s.period.map(|p| p.1)).or_else(|| period_end_in_ledger(db, sha256)).unwrap_or_else(|| Local::now().date_naive())
}

fn period_end_in_ledger(db: &Db, sha256: &str) -> Option<NaiveDate> {
    db.statement_periods().ok()?.into_iter().find(|(s, _, _)| s == sha256).and_then(|(_, _, end)| end)
}

/// The decision the desktop's watched folder makes, without anyone to ask: a statement whose account
/// number was read goes into its account (a new one is made the first time); an unreadable account
/// number, or a period already imported from a different file, waits for a person.
pub fn import(db: &Db, entity: i64, name: &str, sha256: &str, scan: &ScanResult) -> Result<(usize, usize, bool), String> {
    if scan.profile_slug.is_none() || scan.sections.is_empty() {
        return Err("It isn't from a bank FinGraph knows yet, or isn't a statement. Import it on the desktop, where you can teach FinGraph the layout.".into());
    }
    let plan = db.plan_import(sha256, scan).map_err(|e| e.to_string())?;
    if plan.already_imported {
        return Ok((0, 0, true));
    }
    if plan.no_transactions {
        return Err("No transactions or balances could be read from it.".into());
    }
    if !plan.same_period.is_empty() {
        let names: Vec<_> = plan.same_period.iter().map(|(_, n)| n.as_str()).collect();
        return Err(format!(
            "The ledger already has a statement for the same account and period ({}). Import it on the desktop to choose which to keep.",
            names.join(", ")
        ));
    }
    if plan.sections.iter().any(|s| s.matched.is_none() && !s.number_read) {
        return Err("The account number couldn't be read, so FinGraph can't tell which account it belongs to. Import it on the desktop to choose.".into());
    }
    let targets: Vec<Target> = plan.default_targets();
    let rep = db.import_scan(entity, name, sha256, scan, &targets).map_err(|e| e.to_string())?;
    Ok((rep.inserted, rep.held, rep.already_imported))
}

/// The workspace inbox statements go into: the first one (as the desktop does when several are shown).
pub fn entity(db: &Db) -> Option<i64> {
    db.entities().ok()?.first().map(|e| e.id)
}

/// Handles one inbox file whose scan has already been done (scanning runs outside the ledger's thread).
pub fn process(db: &Db, state: &State, statements: &Path, path: &Path, sha256: &str, scan: Result<ScanResult, String>) -> Outcome {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let attention = |reason: String| {
        let dir = statements.join("needs-attention");
        let dest = filing::free_name(&dir, &name);
        if let Err(e) = filing::move_file(path, &dest) {
            return Outcome::NeedsAttention { reason: format!("{reason} (and it couldn't be moved out of the inbox: {e})") };
        }
        let note = dest.with_file_name(format!("{}.txt", dest.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::write(&note, format!("{reason}\n\nWhen it's sorted out, move the PDF back into the inbox folder (or import it on the desktop) and delete this note.\n"));
        Outcome::NeedsAttention { reason }
    };
    let scan = match scan {
        Ok(s) => s,
        Err(e) => return attention(format!("It couldn't be read: {e}")),
    };
    let Some(entity) = entity(db) else {
        return attention("The ledger has no workspace yet. Set FinGraph up on the desktop and move the ledger to this server, then put the file back in the inbox.".into());
    };
    match import(db, entity, &name, sha256, &scan) {
        Err(reason) => attention(reason),
        Ok((inserted, held, already)) => {
            if !already {
                // the ledger's own copy, so the statement can be rescanned and backed up whatever
                // happens to the filed one
                if let Err(e) = db.keep_pdf(path, sha256) {
                    eprintln!("{name}: couldn't keep the ledger's copy: {e}");
                }
            }
            let month = filing_month(Some(&scan), db, sha256);
            let filed = match state.filed(sha256) {
                Some(rel) if statements.join(&rel).exists() => Ok(PathBuf::from(rel)),
                _ => filing::file_copy(statements, path, sha256, &name, month),
            };
            match filed {
                Ok(rel) => {
                    let _ = state.set_filed(sha256, &rel.to_string_lossy());
                    let _ = std::fs::remove_file(path);
                    if already { Outcome::AlreadyImported { filed: rel } } else { Outcome::Imported { filed: rel, inserted, held } }
                }
                Err(e) => attention(format!("It was imported, but couldn't be filed: {e}")),
            }
        }
    }
}

/// Files every statement the ledger has a copy of but that isn't in the statements folder yet (after a
/// ledger is moved to the server, or uploaded from the desktop). Returns how many were filed.
pub fn file_missing(db: &Db, state: &State, statements: &Path) -> usize {
    let Ok(files) = db.statement_periods() else { return 0 };
    let mut n = 0;
    for (sha, filename, end) in files {
        if state.filed(&sha).is_some() {
            continue;
        }
        let Some(src) = db.stored_pdf(&sha, &filename) else { continue };
        let month = end.unwrap_or_else(|| Local::now().date_naive());
        match filing::file_copy(statements, &src, &sha, &filename, month) {
            Ok(rel) => {
                let _ = state.set_filed(&sha, &rel.to_string_lossy());
                n += 1;
            }
            Err(e) => eprintln!("{filename}: couldn't be filed: {e}"),
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use fingraph_core::EntityKind;
    use fingraph_core::money::Currency;
    use fingraph_core::parser::MultiPassScanner;

    struct Setup {
        _dir: tempfile::TempDir,
        db: Db,
        state: State,
        statements: PathBuf,
    }

    fn setup(workspace: bool) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("data/ledger.db")).unwrap();
        if workspace {
            db.create_entity("Personal", EntityKind::Personal, Currency::KYD).unwrap();
        }
        let state = State::open(&dir.path().join("data/server.db")).unwrap();
        let statements = dir.path().join("statements");
        std::fs::create_dir_all(statements.join("inbox")).unwrap();
        Setup { _dir: dir, db, state, statements }
    }

    /// The first demo statement, scanned, and a stand-in file for it in the inbox.
    fn statement(s: &Setup, name: &str, bytes: &[u8]) -> (PathBuf, String, ScanResult) {
        let doc = fingraph_core::demo::demo_documents().into_iter().next().unwrap();
        let scan = MultiPassScanner::new(&s.db.profiles().unwrap()).scan_tokens(doc.tokens).unwrap();
        let path = s.statements.join("inbox").join(name);
        std::fs::write(&path, bytes).unwrap();
        let sha = fingraph_core::db::file_sha256(&path).unwrap();
        (path, sha, scan)
    }

    #[test]
    fn a_statement_is_imported_filed_under_its_closing_month_and_kept() {
        let s = setup(true);
        let (path, sha, scan) = statement(&s, "November e-statement.pdf", b"%PDF one");
        let end = scan.period.unwrap().1;
        let out = process(&s.db, &s.state, &s.statements, &path, &sha, Ok(scan.clone()));
        let Outcome::Imported { filed, inserted, .. } = out else { panic!("{out:?}") };
        assert_eq!(filed, filing::month_folder(end).join("November e-statement.pdf"));
        assert!(inserted > 10);
        assert!(!path.exists(), "it left the inbox");
        assert!(s.statements.join(&filed).exists());
        assert!(s.db.stored_pdf(&sha, "November e-statement.pdf").is_some(), "the ledger kept its own copy");
        assert_eq!(s.state.filed(&sha).as_deref(), Some(filed.to_string_lossy().as_ref()));

        // the same file dropped in again is recognised, not imported twice
        let (again, sha2, _) = statement(&s, "copy.pdf", b"%PDF one");
        assert_eq!(sha, sha2);
        let out = process(&s.db, &s.state, &s.statements, &again, &sha2, Ok(scan));
        assert_eq!(out, Outcome::AlreadyImported { filed });
        assert!(!again.exists());
    }

    #[test]
    fn what_needs_a_person_goes_to_needs_attention_with_a_note() {
        let s = setup(false);
        let (path, sha, scan) = statement(&s, "a.pdf", b"%PDF a");
        let out = process(&s.db, &s.state, &s.statements, &path, &sha, Ok(scan));
        let Outcome::NeedsAttention { reason } = out else { panic!("{out:?}") };
        assert!(reason.contains("workspace"), "{reason}");
        assert!(s.statements.join("needs-attention/a.pdf").exists());
        let note = std::fs::read_to_string(s.statements.join("needs-attention/a.pdf.txt")).unwrap();
        assert!(note.starts_with(&reason));

        let s = setup(true);
        let (path, sha, _) = statement(&s, "b.pdf", b"%PDF b");
        let out = process(&s.db, &s.state, &s.statements, &path, &sha, Err("couldn't run pdftotext".into()));
        assert!(matches!(out, Outcome::NeedsAttention { ref reason } if reason.contains("couldn't be read")), "{out:?}");

        let (path, sha, mut scan) = statement(&s, "c.pdf", b"%PDF c");
        scan.profile_slug = None;
        let out = process(&s.db, &s.state, &s.statements, &path, &sha, Ok(scan));
        assert!(matches!(out, Outcome::NeedsAttention { ref reason } if reason.contains("isn't from a bank")), "{out:?}");
    }

    #[test]
    fn an_unreadable_account_number_waits_for_a_person_once_accounts_exist() {
        let s = setup(true);
        let (path, sha, mut scan) = statement(&s, "d.pdf", b"%PDF d");
        for sec in &mut scan.sections {
            sec.account_last4 = None;
        }
        let out = process(&s.db, &s.state, &s.statements, &path, &sha, Ok(scan));
        assert!(matches!(out, Outcome::NeedsAttention { ref reason } if reason.contains("account number")), "{out:?}");
    }

    #[test]
    fn statements_already_in_a_moved_ledger_get_filed() {
        let s = setup(true);
        let (path, sha, scan) = statement(&s, "Scotia Nov.pdf", b"%PDF moved");
        let ent = s.db.entities().unwrap()[0].id;
        s.db.import_auto(ent, "Scotia Nov.pdf", &sha, &scan).unwrap();
        s.db.keep_pdf(&path, &sha).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(file_missing(&s.db, &s.state, &s.statements), 1);
        assert_eq!(file_missing(&s.db, &s.state, &s.statements), 0, "only once");
        let rel = s.state.filed(&sha).unwrap();
        assert!(rel.ends_with("Scotia Nov.pdf"));
        assert!(s.statements.join(rel).exists());
    }

    #[test]
    fn only_settled_pdfs_are_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.pdf", "B.PDF", ".partial.pdf", "notes.txt"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let names: Vec<String> = ready_files(dir.path(), std::time::Duration::ZERO).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["B.PDF", "a.pdf"]);
        assert!(ready_files(dir.path(), std::time::Duration::from_secs(60)).is_empty(), "still arriving");
    }
}
