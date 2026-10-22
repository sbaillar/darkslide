//! Second-review regressions not covered by the reviewer's repros (adv.rs):
//! N2 reporting, N6 failed-after-rename and TZ-shifted Done entries, N8
//! shared backup roots, N9 unmounted backup volumes, N10 source re-read and
//! RAW structural checks, and the ds-raw `MetaProvider`.

mod support;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ds_import::journal::{EntryState, Journal};
use ds_import::*;
use support::*;

/// N2: scan errors keep the import incomplete; skipped files require an
/// acknowledgement before eject.
#[test]
fn scan_errors_and_skipped_files_reach_the_report() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    fs::write(card.join("DCIM/100MSDCF/INDEX.BIN"), b"db").unwrap();
    let scan = scan_volume(&card).unwrap();
    let plan = ImportPlan::from_scan(&scan, PlanOptions::new(&dest), &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete());
    assert_eq!(r.skipped_unknown.len(), 1);
    assert!(
        r.eject_needs_confirmation(),
        "skipped files need an explicit OK"
    );

    let bad = card.join("DCIM/101MSDCF");
    fs::create_dir_all(&bad).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
    let scan = scan_volume(&card).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    if rustix::process::geteuid().is_root() {
        return;
    }
    let plan = ImportPlan::from_scan(&scan, PlanOptions::new(&dest), &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r.scan_errors.len(), 1);
    assert!(!r.is_complete() && r.eject_needs_confirmation());
    // A clean card with nothing skipped needs no confirmation.
    let clean = t.path().join("clean");
    make_card(&clean, 1, 1000);
    let plan = ImportPlan::from_scan(
        &scan_volume(&clean).unwrap(),
        PlanOptions::new(t.path().join("d2")),
        &NoMeta,
    )
    .unwrap();
    assert!(
        !run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .eject_needs_confirmation()
    );
}

/// N6: a file that failed *after* being placed ("source changed") leaves a
/// wrong copy at its name. The next run reuses that name, sees the bytes
/// differ, quarantines the file and copies again, with no `-1` duplicate.
#[test]
fn failed_after_placement_is_quarantined_on_the_next_run() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 20_000);
    let plan = plan_for(&card, &dest);
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
    // Journal says Failed, and the placed file is wrong.
    let entries = plan
        .items
        .iter()
        .flat_map(|i| &i.files)
        .map(|f| ds_import::journal::JournalEntry {
            key: f.key.clone(),
            dest_rel: f.rel.clone(),
            state: EntryState::Failed {
                reason: "source changed".into(),
            },
        })
        .collect();
    Journal {
        backup_dest: None,
        version: ds_import::journal::JOURNAL_VERSION,
        entries,
    }
    .save(&journal_path(&dest, &plan.journal_id))
    .unwrap();
    fs::write(dest.join("DSC00000.ARW"), vec![9u8; 20_000]).unwrap();

    let plan2 = plan_for(&card, &dest);
    assert_eq!(
        plan2.items[0].files[0].rel,
        PathBuf::from("DSC00000.ARW"),
        "same name reused"
    );
    let r = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_eq!(r.quarantined.len(), 1);
    assert_all_imported(&plan2, &dest);
    assert!(!dest.join("DSC00000-1.ARW").exists());
}

/// N6 (Done entries): after a backup failure the journal keeps `Done`
/// entries; if the card's FAT mtimes then shift by 2 h, the entries are
/// still matched (partial hash confirms) and only the backup is copied.
#[test]
fn done_entries_survive_a_timezone_shift() {
    struct FailBackup;
    impl FaultInjector for FailBackup {
        fn before_write(&self, partial: &Path, _: u64) -> io::Result<()> {
            if partial.to_string_lossy().contains("/backup/") {
                return Err(io::Error::other("backup disk error"));
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 2, 30_000);
    let opts = || {
        let mut o = PlanOptions::new(&dest);
        o.backup_dest = Some(backup.clone());
        o
    };
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts(), &NoMeta).unwrap();
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&FailBackup),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.backup_failed.len(), 4);
    for e in fs::read_dir(card.join("DCIM/100MSDCF")).unwrap() {
        let p = e.unwrap().path();
        let m = fs::metadata(&p).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(m + Duration::from_secs(2 * 3600))
            .unwrap();
    }
    let plan2 = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts(), &NoMeta).unwrap();
    let r2 = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.bytes_copied, 0, "primary not copied again");
    assert!(r2.copied.iter().all(|f| f.resumed && f.backup.is_some()));
    assert!(
        files_under(&dest)
            .iter()
            .all(|p| !p.to_string_lossy().contains("-1."))
    );
    assert_all_imported(&plan2, &backup);
}

/// N8: two imports that share a backup root cannot run at once, and a
/// partial file we did not record is never deleted.
#[test]
fn shared_backup_root_is_locked_and_foreign_partials_are_left_alone() {
    let t = tempfile::tempdir().unwrap();
    let (card, d1, d2, backup) = (
        t.path().join("card"),
        t.path().join("d1"),
        t.path().join("d2"),
        t.path().join("backup"),
    );
    make_card(&card, 1, 1000);
    fs::create_dir_all(&backup).unwrap();
    let held = ds_fsutil::DirLock::try_lock_dir(&backup, "import")
        .unwrap()
        .unwrap();
    let mut o = PlanOptions::new(&d1);
    o.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    assert!(
        matches!(run(&plan, &no_progress, &AtomicBool::new(false)), Err(ImportError::Busy(p)) if p == backup)
    );
    drop(held);

    // Another import's in-flight partial at the name we would use.
    let foreign = ds_import::fsio::partial_path(&d2.join("DSC00000.ARW"));
    fs::create_dir_all(&d2).unwrap();
    fs::write(&foreign, b"someone else's partial").unwrap();
    let r = run(&plan_for(&card, &d2), &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        fs::read(&foreign).unwrap(),
        b"someone else's partial",
        "not ours: never deleted"
    );
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(!r.is_complete());
}

/// N9: a backup on a volume that is not mounted is refused instead of
/// silently becoming a folder on the internal disk.
#[test]
fn unmounted_backup_volume_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let card = t.path().join("card");
    make_card(&card, 1, 1000);
    let missing = PathBuf::from("/Volumes/ds-test-backup-not-plugged-in/Capture");
    let mut o = PlanOptions::new(t.path().join("dest"));
    o.backup_dest = Some(missing.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    let err = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap_err();
    assert!(err.to_string().contains("not mounted"), "{err}");
    assert!(!Path::new("/Volumes/ds-test-backup-not-plugged-in").exists());
}

/// N10: with `verify_source_reread`, a card that returns different bytes on
/// a second read is not trusted.
#[test]
fn source_reread_catches_a_card_reader_glitch() {
    struct Glitch(PathBuf, AtomicBool);
    impl FaultInjector for Glitch {
        fn before_verify(&self, _: &Path) {
            // Between the copy read and the re-read, the "card" changes.
            if !self.1.swap(true, Ordering::SeqCst) {
                let mut b = fs::read(&self.0).unwrap();
                b[100] ^= 0xff;
                fs::write(&self.0, b).unwrap();
            }
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 10_000);
    let target = card.join("DCIM/100MSDCF/DSC00000.ARW");
    let mut o = PlanOptions::new(&dest);
    o.verify_source_reread = true;
    o.concurrency = 1;
    assert!(format!("{o:?}").contains("verify_source_reread: true"));
    let plan =
        ImportPlan::new(&scan_volume(&card).unwrap().candidates, o.clone(), &NoMeta).unwrap();
    let g = Glitch(target, AtomicBool::new(false));
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&g),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(r.failed[0].reason.contains("second read"));
    // Without glitches the option passes cleanly.
    let dest2 = t.path().join("dest2");
    let mut o2 = o;
    o2.dest = dest2;
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o2, &NoMeta).unwrap();
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
}

/// N10 + ds-raw: real A9 files import with the structural RAW check on, and
/// the ds-raw `MetaProvider` names them from real capture data.
#[test]
fn real_raws_pass_the_structural_check_and_name_from_metadata() {
    let samples = ds_testdata::samples_with_ext("arw");
    let Some(s) = samples.first() else { return };
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("card/DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(s, dir.join("DSC00001.ARW")).unwrap();
    let dest = t.path().join("dest");
    let mut o = PlanOptions::new(&dest);
    o.rename = Some("{camera}_{date:%Y%m%d}_{time}".into());
    let plan = ImportPlan::new(
        &scan_volume(&t.path().join("card")).unwrap().candidates,
        o,
        &RawMeta,
    )
    .unwrap();
    let rel = plan.items[0].files[0].rel.to_string_lossy().into_owned();
    assert!(rel.starts_with("ILCE-9_20"), "{rel}");
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert!(structurally_valid(&r.copied[0].dest));
}
