//! Regression tests from the adversarial review: resume after crashes and
//! power loss, dedupe that must not trust stale copies, sink ordering,
//! card removal/remount on real FAT32 volumes, exFAT destinations, locks.

mod support;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ds_fsutil::testvol::{DiskImage, Fs};
use ds_import::journal::{EntryState, Journal, JournalEntry};
use ds_import::*;
use support::*;

/// Session stand-in: records delivered files and exposes them as the dedupe
/// index (exactly how the real session + global index will be wired).
#[derive(Default)]
struct Session {
    lookup: Arc<MemoryDedupe>,
    delivered: Mutex<Vec<PathBuf>>,
    panic_on: Option<String>,
}

impl SessionSink for Session {
    fn imported(&self, f: &ImportedFile) -> Result<(), String> {
        if self
            .panic_on
            .as_deref()
            .is_some_and(|p| f.dest.ends_with(p))
        {
            panic!("index write failed");
        }
        self.lookup.insert(
            f.size,
            f.partial,
            KnownCopy {
                hash: f.hash,
                path: Some(f.dest.clone()),
            },
        );
        self.delivered.lock().unwrap().push(f.dest.clone());
        Ok(())
    }
}

struct Hooks {
    /// Set `cancel` when this source is first read (simulates the app dying).
    stop_at: Option<(String, &'static AtomicBool)>,
    fail_flush: bool,
    flushes: AtomicUsize,
}

impl FaultInjector for Hooks {
    fn before_read(&self, src: &Path, _: u64) -> io::Result<()> {
        if let Some((name, flag)) = &self.stop_at
            && src.ends_with(name)
        {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn before_flush(&self) -> io::Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        if self.fail_flush {
            Err(io::Error::other("F_FULLFSYNC failed"))
        } else {
            Ok(())
        }
    }
}

fn pending_journal(plan: &ImportPlan) {
    let j = Journal {
        backup_dest: None,
        version: ds_import::journal::JOURNAL_VERSION,
        entries: plan
            .items
            .iter()
            .flat_map(|i| &i.files)
            .map(|f| JournalEntry {
                key: f.key.clone(),
                dest_rel: f.rel.clone(),
                state: EntryState::Pending,
            })
            .collect(),
    };
    j.save(&journal_path(&plan.dest, &plan.journal_id)).unwrap();
}

/// C1: the app dies after the first files were delivered; power loss leaves
/// the ARW renamed but zero-filled and its JPG never renamed. Resume must
/// quarantine the torn ARW, re-copy both, and must not let dedupe (fed by
/// the sink) claim the zero-filled copy is "already imported".
#[test]
fn c1_zero_filled_copy_after_power_loss_is_recopied_not_deduped() {
    static STOP: AtomicBool = AtomicBool::new(false);
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 3, 200_000);
    let session = Session::default();
    let mk_plan = || {
        let mut o = PlanOptions::new(&dest);
        o.dedupe = Some(session.lookup.clone());
        o.concurrency = 1;
        ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap()
    };
    let plan = mk_plan();
    let hooks = Hooks {
        stop_at: Some(("DSC00001.ARW".into(), &STOP)),
        fail_flush: false,
        flushes: AtomicUsize::new(0),
    };
    let r1 = run_with(
        &plan,
        RunHooks {
            sink: Some(&session),
            faults: Some(&hooks),
        },
        &no_progress,
        &STOP,
    )
    .unwrap();
    assert!(r1.cancelled);
    assert_eq!(
        session.delivered.lock().unwrap().len(),
        2,
        "DSC00000 ARW+JPG delivered"
    );

    // Power loss: rename persisted, data did not; the JPG rename was lost.
    fs::write(dest.join("DSC00000.ARW"), vec![0u8; 200_000]).unwrap();
    fs::remove_file(dest.join("DSC00000.JPG")).unwrap();

    let plan2 = mk_plan();
    let r2 = run_with(
        &plan2,
        RunHooks {
            sink: Some(&session),
            faults: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert!(
        r2.skipped_duplicates.is_empty(),
        "our own copies are never duplicates: {r2:?}"
    );
    assert_eq!(r2.quarantined.len(), 1);
    assert_eq!(
        fs::read(&r2.quarantined[0]).unwrap(),
        vec![0u8; 200_000],
        "torn copy kept, not deleted"
    );
    assert_all_imported(&plan2, &dest);
    assert_eq!(r2.copied.len(), 6);
}

/// C1 (dedupe half): the library says it has the file, but its copy is gone
/// or corrupt. The card file must be imported, not skipped.
#[test]
fn c1_dedupe_ignores_missing_or_corrupt_library_copies() {
    // Dedupe only trusts library copies on another device than the card,
    // so the card is a real FAT32 volume here.
    let t = tempfile::tempdir().unwrap();
    let Some(img) =
        ds_fsutil::testvol::require(t.path(), ds_fsutil::testvol::Fs::Fat32, 64, "CARD")
    else {
        return;
    };
    let (card, dest) = (img.mount_point().to_path_buf(), t.path().join("dest"));
    make_card(&card, 2, 50_000);
    let session = Session::default();
    let r = run_with(
        &plan_for(&card, &dest),
        RunHooks {
            sink: Some(&session),
            faults: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.is_complete());
    // Later: one copy zero-filled, another deleted (session removed).
    fs::write(dest.join("DSC00000.ARW"), vec![0u8; 50_000]).unwrap();
    fs::remove_file(dest.join("DSC00001.JPG")).unwrap();
    let mut o = PlanOptions::new(t.path().join("dest-b"));
    o.dedupe = Some(session.lookup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete());
    let copied: Vec<_> = r
        .copied
        .iter()
        .map(|f| f.dest.file_name().unwrap().to_owned())
        .collect();
    assert_eq!(copied.len(), 2, "{copied:?}");
    assert!(
        copied.iter().any(|n| n == "DSC00000.ARW") && copied.iter().any(|n| n == "DSC00001.JPG")
    );
    assert_eq!(
        r.skipped_duplicates.len(),
        2,
        "intact library copies still dedupe"
    );
}

/// C1: the sink hears about a file only after it is durable. A failing
/// device flush reports nothing as imported; resume then delivers.
#[test]
fn sink_is_told_only_after_the_durability_flush() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 2, 10_000);
    let session = Session::default();
    let plan = plan_for(&card, &dest);
    let hooks = Hooks {
        stop_at: None,
        fail_flush: true,
        flushes: AtomicUsize::new(0),
    };
    let r = run_with(
        &plan,
        RunHooks {
            sink: Some(&session),
            faults: Some(&hooks),
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(session.delivered.lock().unwrap().is_empty());
    assert_eq!(r.copied, [] as [ds_import::ImportedFile; 0]);
    assert!(
        r.durability_error
            .as_deref()
            .unwrap()
            .contains("F_FULLFSYNC")
    );
    assert_eq!(r.failed.len(), 4);
    assert!(
        r.failed
            .iter()
            .all(|f| f.reason.contains("not confirmed durable"))
    );
    assert_eq!(
        pending_resume(&dest).unwrap().unwrap().pending,
        4,
        "journal still Pending"
    );
    let j = Journal::load(&journal_path(&dest, &plan.journal_id))
        .unwrap()
        .unwrap();
    assert!(
        j.entries.iter().all(|e| e.state == EntryState::Pending),
        "saved, nothing marked Done"
    );
    assert!(hooks.flushes.load(Ordering::SeqCst) >= 1);

    let r2 = run_with(
        &plan,
        RunHooks {
            sink: Some(&session),
            faults: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.bytes_copied, 0, "verified in place, not copied again");
    assert_eq!(session.delivered.lock().unwrap().len(), 4);
}

/// H1: crash after the primary rename, before the backup rename (and a stale
/// backup partial). Resume copies only the missing backup.
#[test]
fn h1_crash_between_primary_and_backup_renames_resumes() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 2, 30_000);
    let mut o = PlanOptions::new(&dest);
    o.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
    pending_journal(&plan);
    let lost = backup.join("DSC00001.ARW");
    fs::remove_file(&lost).unwrap();
    fs::write(ds_import::fsio::partial_path(&lost), b"half").unwrap();

    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert!(r.failed.is_empty() && r.backup_failed.is_empty());
    assert_eq!(r.bytes_copied, 0);
    assert!(r.copied.iter().all(|f| f.resumed && f.backup.is_some()));
    assert_eq!(
        fs::read(&lost).unwrap(),
        fs::read(card.join("DCIM/100MSDCF/DSC00001.ARW")).unwrap()
    );
    assert_eq!(partials_under(&backup), [] as [std::path::PathBuf; 0]);
}

/// Progress on a resume that only re-copies a missing backup counts each
/// file's bytes once: `bytes_done` ends exactly at `bytes_total`.
#[test]
fn backup_only_resume_progress_reaches_total_exactly() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 2, 30_000);
    let mut o = PlanOptions::new(&dest);
    o.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
    pending_journal(&plan);
    fs::remove_file(backup.join("DSC00001.ARW")).unwrap();

    let seen = Mutex::new(Vec::new());
    let progress = |p: &ImportProgress| seen.lock().unwrap().push((p.bytes_done, p.bytes_total));
    let r = run(&plan, &progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    let seen = seen.into_inner().unwrap();
    let &(done, total) = seen.last().unwrap();
    assert_eq!(done, total);
    assert!(seen.iter().all(|&(d, t)| d <= t), "{seen:?}");
}

/// H2: our own torn file (journal Pending, bytes differ) is quarantined and
/// re-copied instead of failing forever.
#[test]
fn h2_own_torn_file_is_quarantined_and_recopied() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 30_000);
    let plan = plan_for(&card, &dest);
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
    pending_journal(&plan);
    let torn = dest.join("DSC00000.JPG");
    let len = fs::metadata(&torn).unwrap().len() as usize;
    fs::write(&torn, vec![0xAB; len]).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_eq!(r.quarantined.len(), 1);
    assert!(r.quarantined[0].starts_with(dest.join(".ds/quarantine")));
    assert_all_imported(&plan, &dest);
    // Running again is a no-op.
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete() && r.quarantined.is_empty() && r.bytes_copied == 0);
}

/// A file that appears at a planned name and is not ours is never touched.
#[test]
fn strangers_file_at_a_planned_name_is_never_overwritten() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 10_000);
    let plan = plan_for(&card, &dest);
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("DSC00000.JPG"), b"someone else's").unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].reason.contains("different content"));
    assert_eq!(
        fs::read(dest.join("DSC00000.JPG")).unwrap(),
        b"someone else's"
    );
    assert_eq!(r.quarantined, [] as [std::path::PathBuf; 0]);
}

/// Medium: a FAT32 card remounted under another name ("CARD 1") resumes by
/// volume UUID + relative path; no `-1` duplicates.
#[test]
fn fat32_card_remounted_under_new_name_resumes_without_duplicates() {
    struct FailOnce(AtomicBool);
    impl FaultInjector for FailOnce {
        fn before_read(&self, src: &Path, off: u64) -> io::Result<()> {
            if src.ends_with("DSC00002.ARW") && off > 0 && !self.0.swap(true, Ordering::SeqCst) {
                return Err(io::Error::other("read error"));
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let mut img = DiskImage::create(t.path(), Fs::Fat32, 64, "CARD").unwrap();
    make_card(img.mount_point(), 4, 200_000);
    let dest = t.path().join("dest");
    let mut plan = plan_for(img.mount_point(), &dest);
    plan.chunk_size = 64 * 1024;
    let f = FailOnce(AtomicBool::new(false));
    let r1 = run_with(
        &plan,
        RunHooks {
            faults: Some(&f),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r1.failed.len(), 1);
    let names1 = files_under(&dest);

    img.detach(false).unwrap();
    img.attach(&t.path().join("CARD 1")).unwrap();
    let plan2 = plan_for(img.mount_point(), &dest);
    let rels1: Vec<_> = plan
        .items
        .iter()
        .flat_map(|i| &i.files)
        .map(|f| f.rel.clone())
        .collect();
    let rels2: Vec<_> = plan2
        .items
        .iter()
        .flat_map(|i| &i.files)
        .map(|f| f.rel.clone())
        .collect();
    assert_eq!(rels1, rels2, "same names after remount");
    assert_eq!(plan.journal_id, plan2.journal_id);
    let r2 = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.copied.iter().filter(|f| !f.resumed).count(), 1);
    assert!(
        files_under(&dest)
            .iter()
            .all(|p| !p.to_string_lossy().contains("-1"))
    );
    assert!(!names1.contains(&PathBuf::from("DSC00002.ARW")) && dest.join("DSC00002.ARW").exists());
    assert_all_imported(&plan2, &dest);
}

/// Medium: yanking a real FAT32 card mid-copy stops the run with a single
/// "removed" error, no per-file failures, no partials; resume completes.
#[test]
fn fat32_card_yanked_mid_copy_stops_with_one_error() {
    struct Yank(Mutex<Option<String>>);
    impl FaultInjector for Yank {
        fn before_read(&self, src: &Path, off: u64) -> io::Result<()> {
            if src.ends_with("DSC00002.ARW")
                && off > 0
                && let Some(dev) = self.0.lock().unwrap().take()
            {
                let st = std::process::Command::new("/usr/bin/hdiutil")
                    .args(["detach", "-force", "-quiet", &dev])
                    .status()
                    .unwrap();
                assert!(st.success());
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let mut img = DiskImage::create(t.path(), Fs::Fat32, 64, "CARD").unwrap();
    make_card(img.mount_point(), 6, 300_000);
    let dest = t.path().join("dest");
    let mut plan = plan_for(img.mount_point(), &dest);
    plan.chunk_size = 64 * 1024;
    plan.concurrency = 1;
    let y = Yank(Mutex::new(img.device().map(str::to_owned)));
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&y),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(
        r.source_lost
            .as_deref()
            .is_some_and(|m| m.contains("removed")),
        "{r:?}"
    );
    assert!(
        r.failed.is_empty(),
        "no per-file failures for a pulled card: {:?}",
        r.failed
    );
    assert_eq!(r.copied.len(), 4);
    assert!(!r.is_complete());
    assert_eq!(partials_under(&dest), [] as [std::path::PathBuf; 0]);

    // Reinsert (the image was force-detached; attach again) and resume.
    img.attach(&t.path().join("CARD")).unwrap();
    let plan2 = plan_for(img.mount_point(), &dest);
    let r2 = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.copied.iter().filter(|f| f.resumed).count(), 4);
    assert_all_imported(&plan2, &dest);
}

/// Same, on a plain directory: the card root vanishes mid-copy.
#[test]
fn card_directory_vanishing_is_source_lost() {
    struct Vanish(PathBuf);
    impl FaultInjector for Vanish {
        fn before_read(&self, src: &Path, off: u64) -> io::Result<()> {
            if src.ends_with("DSC00001.ARW") && off > 0 {
                let _ = fs::remove_dir_all(&self.0);
                return Err(io::ErrorKind::NotFound.into());
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 5, 200_000);
    let mut plan = plan_for(&card, &dest);
    plan.chunk_size = 32 * 1024;
    plan.concurrency = 1;
    let v = Vanish(card.clone());
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&v),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.source_lost.is_some() && r.failed.is_empty(), "{r:?}");
    assert_eq!(pending_resume(&dest).unwrap().unwrap().failed, 0);
}

/// H7-equivalent for import: an exFAT destination (no `RENAME_EXCL`, no hard
/// links) imports, and name collisions there still get `-1`, never replace.
#[test]
fn exfat_destination_imports_and_never_replaces() {
    let t = tempfile::tempdir().unwrap();
    let card = t.path().join("card");
    make_card(&card, 2, 100_000);
    let img = DiskImage::create(t.path(), Fs::ExFat, 64, "BACKUP").unwrap();
    let dest = img.mount_point().join("Capture");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("DSC00000.ARW"), b"older photo").unwrap();
    let plan = plan_for(&card, &dest);
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_eq!(fs::read(dest.join("DSC00000.ARW")).unwrap(), b"older photo");
    assert!(dest.join("DSC00000-1.ARW").exists() && dest.join("DSC00000-1.JPG").exists());
    assert_all_imported(&plan, &dest);
}

/// Medium: two imports into one destination cannot run at once.
#[test]
fn concurrent_import_into_same_destination_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    let plan = plan_for(&card, &dest);
    fs::create_dir_all(&dest).unwrap();
    let held = ds_fsutil::DirLock::try_lock_dir(&dest, "import")
        .unwrap()
        .unwrap();
    assert!(matches!(
        run(&plan, &no_progress, &AtomicBool::new(false)),
        Err(ImportError::Busy(_))
    ));
    drop(held);
    assert!(
        run(&plan, &no_progress, &AtomicBool::new(false))
            .unwrap()
            .is_complete()
    );
}

/// Medium: failing to record the plan is a loud error, before any copy.
#[test]
fn journal_save_failure_before_copying_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    let plan = plan_for(&card, &dest);
    let ds = dest.join(".ds");
    fs::create_dir_all(&ds).unwrap();
    fs::write(ds.join("import.lock"), b"").unwrap();
    fs::set_permissions(&ds, fs::Permissions::from_mode(0o555)).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false));
    fs::set_permissions(&ds, fs::Permissions::from_mode(0o755)).unwrap();
    if rustix::process::geteuid().is_root() {
        return;
    }
    assert!(matches!(r, Err(ImportError::Io { .. })), "{r:?}");
    assert!(!dest.join("DSC00000.ARW").exists(), "nothing copied");
}

/// H6: a panicking sink fails that file only; a panicking progress callback
/// is ignored.
#[test]
fn panicking_callbacks_fail_one_file_not_the_run() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 2, 1000);
    let session = Session {
        panic_on: Some("DSC00000.ARW".into()),
        ..Session::default()
    };
    let plan = plan_for(&card, &dest);
    let boom = |_: &ImportProgress| panic!("ui gone");
    let r = run_with(
        &plan,
        RunHooks {
            sink: Some(&session),
            faults: None,
        },
        &boom,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].reason.contains("sink panicked"));
    assert_eq!(r.copied.len(), 3);
    // Resume re-delivers the file whose delivery failed.
    let ok = Session::default();
    let r = run_with(
        &plan,
        RunHooks {
            sink: Some(&ok),
            faults: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.is_complete());
    assert!(
        ok.delivered
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.ends_with("DSC00000.ARW"))
    );
}

/// Low: 254-byte names import (temp/partial names are short).
#[test]
fn very_long_file_names_import() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    let mut o = PlanOptions::new(&dest);
    o.rename = Some(format!(
        "{}_{{name}}",
        "x".repeat(250 - "_DSC00000.ARW".len())
    ));
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    assert_eq!(plan.items[0].files[0].rel.as_os_str().len(), 250);
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_all_imported(&plan, &dest);
}
