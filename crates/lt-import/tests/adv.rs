//! Second adversarial re-review repros (a1–a5, lock race), ported as
//! regression tests. Disk-image variants go through `testvol::require` and
//! print `SKIP testvol: …` when disk images are unavailable.

mod support;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lt_fsutil::testvol::{self, Fs};
use lt_import::*;
use support::*;

/// a1 (plain dir): the destination root is swapped out (as by an unmount
/// that leaves an empty mount-point directory) during the final flush.
#[test]
fn a1_dest_replaced_before_final_flush_is_not_complete() {
    struct Swap(PathBuf, AtomicBool);
    impl FaultInjector for Swap {
        fn before_flush(&self) -> io::Result<()> {
            if !self.1.swap(true, Ordering::SeqCst) {
                let moved = self.0.with_extension("gone");
                fs::rename(&self.0, &moved).unwrap();
                fs::create_dir(&self.0).unwrap();
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 3, 30_000);
    let plan = plan_for(&card, &dest);
    let s = Swap(dest.clone(), AtomicBool::new(false));
    let r = run_with(
        &plan,
        RunHooks {
            sink: None,
            faults: Some(&s),
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(s.1.load(Ordering::SeqCst));
    assert!(
        !r.is_complete(),
        "destination vanished before the final flush, yet complete: {r:?}"
    );
    assert!(
        r.copied.is_empty(),
        "nothing may be reported copied without a successful flush"
    );
    assert!(r.durability_error.is_some());
}

/// a1 (backup root): same for the backup destination.
#[test]
fn a1_backup_replaced_before_final_flush_is_not_complete() {
    struct Swap(PathBuf, AtomicBool);
    impl FaultInjector for Swap {
        fn before_flush(&self) -> io::Result<()> {
            if !self.1.swap(true, Ordering::SeqCst) {
                fs::rename(&self.0, self.0.with_extension("gone")).unwrap();
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
    let mut o = PlanOptions::new(&dest);
    o.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    let s = Swap(backup, AtomicBool::new(false));
    let r = run_with(
        &plan,
        RunHooks {
            sink: None,
            faults: Some(&s),
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!r.is_complete(), "{r:?}");
    assert!(
        r.copied.iter().all(|f| f.backup.is_none()),
        "no backup reported without its flush"
    );
}

/// a1 (real volume): force-unmount an APFS destination during the final
/// flush. Gated on disk images.
#[test]
fn a1_dest_unmounted_before_final_flush_is_not_complete() {
    struct Yank(Mutex<Option<String>>, AtomicBool);
    impl FaultInjector for Yank {
        fn before_flush(&self) -> io::Result<()> {
            if let Some(mp) = self.0.lock().unwrap().take() {
                // `umount -f`: `diskutil unmount` can wedge with disk services.
                let st = testvol::run_with_deadline(
                    std::process::Command::new("/sbin/umount")
                        .arg("-f")
                        .arg(&mp),
                    testvol::command_timeout(),
                );
                self.1.store(st.is_ok(), Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let card = t.path().join("card");
    make_card(&card, 3, 300_000);
    let Some(img) = testvol::require(t.path(), Fs::Apfs, 64, "DEST") else {
        return;
    };
    let dest = img.mount_point().join("Capture");
    let plan = plan_for(&card, &dest);
    let y = Yank(
        Mutex::new(Some(img.mount_point().display().to_string())),
        AtomicBool::new(false),
    );
    let r = run_with(
        &plan,
        RunHooks {
            sink: None,
            faults: Some(&y),
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(y.1.load(Ordering::SeqCst), "unmounted");
    assert!(!r.is_complete(), "{r:?}");
    assert_eq!(r.copied, [] as [lt_import::ImportedFile; 0]);
}

/// a2: a crash left a torn copy with a Pending journal, then the card's FAT
/// mtimes shifted by +1 h (time zone change between mounts). The torn file
/// must not be left in the library.
#[test]
fn a2_timezone_shift_after_crash_leaves_no_torn_file() {
    static STOP: AtomicBool = AtomicBool::new(false);
    struct StopAt;
    impl FaultInjector for StopAt {
        fn before_read(&self, src: &Path, _: u64) -> io::Result<()> {
            if src.ends_with("DSC00001.ARW") {
                STOP.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 3, 100_000);
    let mut o = PlanOptions::new(&dest);
    o.concurrency = 1;
    let plan =
        ImportPlan::new(&scan_volume(&card).unwrap().candidates, o.clone(), &NoMeta).unwrap();
    let r1 = run_with(
        &plan,
        RunHooks {
            sink: None,
            faults: Some(&StopAt),
        },
        &no_progress,
        &STOP,
    )
    .unwrap();
    assert!(r1.cancelled);
    fs::write(dest.join("DSC00000.ARW"), vec![0u8; 100_000]).unwrap();
    let jp = journal_paths(&dest)[0].clone();
    let j = fs::read_to_string(&jp)
        .unwrap()
        .replace("\"state\":\"done\"", "\"state\":\"pending\"");
    fs::write(&jp, j).unwrap();
    for e in fs::read_dir(card.join("DCIM/100MSDCF")).unwrap() {
        let p = e.unwrap().path();
        let m = fs::metadata(&p).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(m + Duration::from_secs(3600))
            .unwrap();
    }
    let plan2 = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    let r2 = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_all_imported(&plan2, &dest);
    let torn = fs::read(dest.join("DSC00000.ARW")).unwrap();
    assert!(
        !torn.iter().all(|&b| b == 0),
        "zero-filled torn copy left in the library"
    );
    assert!(
        files_under(&dest)
            .iter()
            .all(|p| !p.to_string_lossy().contains("-1.")),
        "no -1 duplicates after a time zone shift: {:?}",
        files_under(&dest)
    );
}

/// a3: dedupe must not trust the card file itself, or a copy in the Trash.
#[test]
fn a3_known_copy_on_the_card_or_in_trash_is_not_a_duplicate() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 2, 100_000);
    let lookup = Arc::new(MemoryDedupe::default());
    let trash = t.path().join(".Trash");
    fs::create_dir_all(&trash).unwrap();
    for c in scan_volume(&card).unwrap().candidates {
        for f in c.files() {
            let copy = if f.kind == FileKind::Raw {
                f.path.clone()
            } else {
                let p = trash.join(f.path.file_name().unwrap());
                fs::copy(&f.path, &p).unwrap();
                p
            };
            lookup.insert(
                f.size,
                partial_hash(&f.path).unwrap(),
                KnownCopy {
                    hash: full_hash(&f.path).unwrap(),
                    path: Some(copy),
                },
            );
        }
    }
    let mut o = PlanOptions::new(&dest);
    o.dedupe = Some(lookup);
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, o, &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete());
    assert!(
        r.skipped_duplicates.is_empty(),
        "{:?}",
        r.skipped_duplicates
    );
    assert_eq!(r.copied.len(), 4);
}

/// a4: an unreadable card folder and unknown file types make the import
/// incomplete and are listed; media (ARQ, MP4) are imported.
#[test]
fn a4_unreadable_folders_and_unknown_files_are_reported() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 2, 10_000);
    let bad = card.join("DCIM/101MSDCF");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("DSC09999.ARW"), b"unreachable photo").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(card.join("DCIM/100MSDCF/DSC00005.ARQ"), b"pixel shift").unwrap();
    fs::write(card.join("DCIM/100MSDCF/C0001.MP4"), b"video").unwrap();
    fs::write(card.join("DCIM/100MSDCF/NOTES.BIN"), b"camera db").unwrap();
    let scan = scan_volume(&card).unwrap();
    let plan = ImportPlan::from_scan(&scan, PlanOptions::new(&dest), &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    if !rustix::process::geteuid().is_root() {
        assert!(
            !scan.errors.is_empty(),
            "unreadable folder must be a scan error"
        );
        assert!(!scan.is_complete());
    }
    assert!(
        scan.skipped_unknown
            .iter()
            .any(|s| s.path.ends_with("NOTES.BIN")),
        "{:?}",
        scan.skipped_unknown
    );
    assert!(dest.join("DSC00005.ARQ").exists() && dest.join("C0001.MP4").exists());
    assert!(r.failed.is_empty(), "every readable file copied");
    assert_eq!(
        r.is_complete(),
        rustix::process::geteuid().is_root(),
        "unreadable folder => not complete"
    );
    assert!(r.eject_needs_confirmation());
}

/// a5: a torn journal must not brick the destination.
#[test]
fn a5_torn_journal_is_set_aside_not_fatal() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 10_000);
    let plan = plan_for(&card, &dest);
    let jp = journal_path(&dest, &plan.journal_id);
    fs::create_dir_all(jp.parent().unwrap()).unwrap();
    fs::write(&jp, vec![0u8; 300]).unwrap();
    let p = ImportPlan::new(
        &scan_volume(&card).unwrap().candidates,
        PlanOptions::new(&dest),
        &NoMeta,
    );
    assert!(
        p.is_ok(),
        "a torn journal makes the destination unusable: {:?}",
        p.err()
    );
    let other = t.path().join("card2");
    make_card(&other, 1, 10_001);
    assert!(
        ImportPlan::new(
            &scan_volume(&other).unwrap().candidates,
            PlanOptions::new(&dest),
            &NoMeta
        )
        .is_ok()
    );
    assert!(pending_resume(&dest).is_ok());
    let r = run(&p.unwrap(), &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    let aside: Vec<_> = fs::read_dir(dest.join(".lt"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".bad-"))
        .collect();
    assert_eq!(aside.len(), 1, "torn journal kept for inspection");
}

/// Lock race: the import used to `remove_file` its lock file after
/// unlocking, so a second importer that had opened (not yet flocked) the old
/// file and a third on the new file could both hold "the" lock. The import
/// now locks the destination directory itself: nothing is ever unlinked.
#[test]
fn import_lock_cannot_be_held_twice() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    // B opens a handle on the destination while A runs…
    fs::create_dir_all(&dest).unwrap();
    let b_fd = fs::File::open(&dest).unwrap();
    let r = run(
        &plan_for(&card, &dest),
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.is_complete());
    assert!(
        !dest.join(".lt").exists(),
        "no lock file (or stray .lt) left behind"
    );
    // …and flocks after A finishes; C (or A's next import) must be refused.
    // A process another test spawns at that instant shares A's just-released
    // lock until it execs, so B retries briefly (as lt-fsutil's lock tests do).
    let b_locked = (0..200).any(|_| {
        rustix::fs::flock(&b_fd, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            false
        }
    });
    assert!(b_locked, "A released the lock");
    assert!(
        lt_fsutil::DirLock::try_lock_dir(&dest, "import")
            .unwrap()
            .is_none(),
        "B and C both hold the lock"
    );
    assert!(matches!(
        run(
            &plan_for(&card, &dest),
            &no_progress,
            &AtomicBool::new(false)
        ),
        Err(ImportError::Busy(_))
    ));
    drop(b_fd);
    assert!(
        run(
            &plan_for(&card, &dest),
            &no_progress,
            &AtomicBool::new(false)
        )
        .is_ok()
    );
}
