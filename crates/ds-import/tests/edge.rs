//! Edge paths of `run`: backup-name conflicts, card files that changed since
//! they were recorded, and source errors while verifying an existing copy.

mod support;

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use ds_import::journal::{EntryState, JOURNAL_VERSION, Journal, JournalEntry};
use ds_import::*;
use support::*;

fn backup_opts(dest: &Path, backup: &Path) -> PlanOptions {
    let mut o = PlanOptions::new(dest);
    o.backup_dest = Some(backup.to_path_buf());
    o
}

struct FailBackup;
impl FaultInjector for FailBackup {
    fn before_write(&self, partial: &Path, _: u64) -> io::Result<()> {
        if partial.to_string_lossy().contains("/backup/") {
            return Err(io::Error::other("backup disk error"));
        }
        Ok(())
    }
}

/// A stranger's file (or another import's partial) at the backup name only
/// fails the backup copy; the primary lands and nothing is overwritten.
#[test]
fn backup_name_conflicts_fail_only_the_backup() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 1, 5000);
    let plan = ImportPlan::new(
        &scan_volume(&card).unwrap().candidates,
        backup_opts(&dest, &backup),
        &NoMeta,
    )
    .unwrap();
    fs::create_dir_all(&backup).unwrap();
    fs::write(backup.join("DSC00000.ARW"), b"stranger").unwrap();
    let foreign = ds_import::fsio::partial_path(&backup.join("DSC00000.JPG"));
    fs::write(&foreign, b"foreign partial").unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(r.backup_failed.len(), 2, "{r:?}");
    assert!(
        r.backup_failed
            .iter()
            .any(|f| f.reason.contains("different content"))
    );
    assert_eq!(fs::read(backup.join("DSC00000.ARW")).unwrap(), b"stranger");
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign partial");
    assert!(dest.join("DSC00000.ARW").exists() && dest.join("DSC00000.JPG").exists());
}

/// A card file whose bytes changed (same size and mtime) after an earlier
/// run recorded it as done is refused, not silently re-imported.
#[test]
fn card_file_changed_since_it_was_recorded_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 1, 5000);
    let plan = ImportPlan::new(
        &scan_volume(&card).unwrap().candidates,
        backup_opts(&dest, &backup),
        &NoMeta,
    )
    .unwrap();
    let hooks = RunHooks {
        faults: Some(&FailBackup),
        sink: None,
    };
    run_with(&plan, hooks, &no_progress, &AtomicBool::new(false)).unwrap();
    // The journal keeps Done entries (backup owed). Now the primary copy is
    // lost and the card file's bytes change without a size/mtime change.
    fs::remove_file(dest.join("DSC00000.ARW")).unwrap();
    let src = card.join("DCIM/100MSDCF/DSC00000.ARW");
    let m = fs::metadata(&src).unwrap().modified().unwrap();
    fs::write(&src, vec![0xEE; 5000]).unwrap();
    fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(m)
        .unwrap();
    let plan2 = ImportPlan::new(
        &scan_volume(&card).unwrap().candidates,
        backup_opts(&dest, &backup),
        &NoMeta,
    )
    .unwrap();
    let r = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(
        r.failed
            .iter()
            .any(|f| f.reason.contains("changed since it was first imported")),
        "{r:?}"
    );
}

/// Resume hashes the card file to judge an existing copy: a read error there
/// is a per-file failure, and a size change since the scan is caught.
#[test]
fn resume_verification_source_errors() {
    struct FailRead;
    impl FaultInjector for FailRead {
        fn before_read(&self, _: &Path, _: u64) -> io::Result<()> {
            Err(io::Error::other("bad sector"))
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 5000);
    let plan = plan_for(&card, &dest);
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
    let entries = plan
        .items
        .iter()
        .flat_map(|i| &i.files)
        .map(|f| JournalEntry {
            key: f.key.clone(),
            dest_rel: f.rel.clone(),
            state: EntryState::Pending,
        })
        .collect();
    let pending = Journal {
        backup_dest: None,
        version: JOURNAL_VERSION,
        entries,
    };
    pending
        .save(&journal_path(&dest, &plan.journal_id))
        .unwrap();
    let hooks = RunHooks {
        faults: Some(&FailRead),
        sink: None,
    };
    let r = run_with(&plan, hooks, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r.failed.len(), 2, "{r:?}");
    assert!(r.failed.iter().all(|f| f.reason.contains("bad sector")));
    // Size changed since the scan (the plan says one byte more).
    pending
        .save(&journal_path(&dest, &plan.journal_id))
        .unwrap();
    let mut wrong = plan.clone();
    wrong.items[0].files[0].src.size += 1;
    let r = run(&wrong, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(
        r.failed.iter().any(|f| f.reason.contains("changed size")),
        "{r:?}"
    );
}

/// A crash left `.<hash>.partial` and the journal is gone: an old partial is
/// cleared and the file imports; a fresh one (a live writer?) still blocks it.
#[test]
fn stale_partial_without_a_journal_does_not_block_the_name() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 5000);
    let plan = ImportPlan::new(
        &scan_volume(&card).unwrap().candidates,
        PlanOptions::new(&dest),
        &NoMeta,
    )
    .unwrap();
    fs::create_dir_all(&dest).unwrap();
    let old = ds_import::fsio::partial_path(&dest.join("DSC00000.ARW"));
    let fresh = ds_import::fsio::partial_path(&dest.join("DSC00000.JPG"));
    fs::write(&old, b"torn").unwrap();
    fs::write(&fresh, b"torn").unwrap();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(
            std::time::SystemTime::now()
                - ds_fsutil::STALE_TEMP_AGE
                - std::time::Duration::from_secs(60),
        )
        .unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r.copied.len(), 1, "{r:?}");
    assert!(dest.join("DSC00000.ARW").exists());
    assert!(!old.exists());
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert_eq!(
        fs::read(&fresh).unwrap(),
        b"torn",
        "a fresh partial is untouched"
    );
}

/// Workers finishing a file queue it under the ledger lock. That lock must
/// not be held across the checkpoint's `F_FULLFSYNC`: while the first
/// (intermediate) flush is in progress, another file has to complete.
#[test]
fn files_complete_while_a_checkpoint_is_flushing() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct SlowCard {
        files_done: AtomicUsize,
        flushes: AtomicUsize,
        progressed_during_first_flush: AtomicBool,
    }
    impl FaultInjector for SlowCard {
        fn before_read(&self, _: &Path, offset: u64) -> io::Result<()> {
            if offset == 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            Ok(())
        }
        fn before_flush(&self) -> io::Result<()> {
            if self.flushes.fetch_add(1, SeqCst) == 0 {
                let entry = self.files_done.load(SeqCst);
                let deadline = Instant::now() + Duration::from_secs(4);
                while Instant::now() < deadline {
                    if self.files_done.load(SeqCst) > entry {
                        self.progressed_during_first_flush.store(true, SeqCst);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }
    }

    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 12, 5000);
    let plan = plan_for(&card, &dest);
    let hooks = SlowCard::default();
    let progress = |p: &ImportProgress| hooks.files_done.store(p.files_done, SeqCst);
    let r = run_with(
        &plan,
        RunHooks {
            sink: None,
            faults: Some(&hooks),
        },
        &progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert!(
        hooks.flushes.load(SeqCst) >= 2,
        "an intermediate checkpoint ran"
    );
    assert!(
        hooks.progressed_during_first_flush.load(SeqCst),
        "workers were blocked behind the flush"
    );
}

/// A later Darkslide left an unfinished import journal for this card, then
/// the app was downgraded: planning ignores it, and the run refuses instead
/// of overwriting the journal that version needs to resume.
#[test]
fn a_newer_journal_for_this_card_is_never_rewritten() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 5000);
    let plan = plan_for(&card, &dest);
    let jp = journal_path(&dest, &plan.journal_id);
    let newer = format!(
        r#"{{"version":{},"entries":[{{"state":"teleported"}}]}}"#,
        JOURNAL_VERSION + 1
    );
    fs::create_dir_all(jp.parent().unwrap()).unwrap();
    fs::write(&jp, &newer).unwrap();
    let plan = plan_for(&card, &dest);
    let e = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap_err();
    assert!(
        matches!(e, ImportError::JournalVersion { version, .. } if version == JOURNAL_VERSION + 1),
        "{e}"
    );
    assert_eq!(fs::read_to_string(&jp).unwrap(), newer);
    assert!(!dest.join("DSC00000.ARW").exists(), "nothing copied");
}
