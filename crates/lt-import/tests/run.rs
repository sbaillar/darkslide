//! End-to-end import tests, including adversarial cases: card pulled mid-copy,
//! destination full, bit rot between write and verify, cancellation, crash
//! recovery, collisions, huge file counts, and never touching the source.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use lt_import::*;

/// (len, blake3, mtime, permissions) for every entry under `root`.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, [u8; 32], SystemTime, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let m = fs::symlink_metadata(e.path()).unwrap();
            let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
            if m.is_dir() {
                stack.push(e.path());
                out.insert(
                    rel,
                    (0, [0; 32], m.modified().unwrap(), m.permissions().mode()),
                );
            } else {
                let h = *blake3::hash(&fs::read(e.path()).unwrap()).as_bytes();
                out.insert(
                    rel,
                    (m.len(), h, m.modified().unwrap(), m.permissions().mode()),
                );
            }
        }
    }
    out
}

fn bytes(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A card with `n` RAW+JPG pairs of `size` bytes, mtimes one second apart.
fn make_card(root: &Path, n: usize, size: usize) {
    let dir = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000 + i as u64);
        for (ext, len) in [("ARW", size), ("JPG", size / 4 + 1)] {
            let p = dir.join(format!("DSC{i:05}.{ext}"));
            fs::write(&p, bytes(i as u8, len)).unwrap();
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(base)
                .unwrap();
        }
    }
}

fn partials_under(root: &Path) -> Vec<PathBuf> {
    snapshot(root)
        .into_keys()
        .filter(|p| p.to_string_lossy().ends_with(".partial"))
        .collect()
}

fn plan_for(card: &Path, dest: &Path) -> ImportPlan {
    ImportPlan::new(
        &scan_volume(card).unwrap().candidates,
        PlanOptions::new(dest),
        &NoMeta,
    )
    .unwrap()
}

fn no_progress(_: &ImportProgress) {}

#[test]
fn copies_verifies_backs_up_and_never_touches_source() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    make_card(&card, 6, 300_000);
    let before = snapshot(&card);

    let mut opts = PlanOptions::new(&dest);
    opts.backup_dest = Some(backup.clone());
    opts.rename = Some("trip_{seq:3}".into());
    opts.concurrency = 2;
    let mut plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts, &NoMeta).unwrap();
    plan.chunk_size = 64 * 1024; // many chunks per file
    let events = Mutex::new(Vec::new());
    let report = run(
        &plan,
        &|p| events.lock().unwrap().push(p.clone()),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.copied.len(), 12);
    assert_eq!(report.bytes_copied, plan.total_bytes());
    for f in &report.copied {
        let src = fs::read(&f.src).unwrap();
        assert_eq!(fs::read(&f.dest).unwrap(), src);
        assert_eq!(fs::read(f.backup.as_ref().unwrap()).unwrap(), src);
        assert_eq!(f.hash, full_hash(&f.src).unwrap());
        assert_eq!(f.partial, partial_hash(&f.src).unwrap());
        assert_eq!(
            fs::metadata(&f.dest).unwrap().modified().unwrap(),
            fs::metadata(&f.src).unwrap().modified().unwrap()
        );
        assert!(!f.resumed);
    }
    // RAW and JPG share the new stem.
    assert!(dest.join("trip_001.ARW").exists() && dest.join("trip_001.JPG").exists());
    assert!(partials_under(&dest).is_empty() && partials_under(&backup).is_empty());
    assert!(
        journal_paths(&dest).is_empty() && !dest.join(".lt").exists(),
        "journal removed after a complete import"
    );
    assert_eq!(snapshot(&card), before, "source must be byte-identical");

    let ev = events.into_inner().unwrap();
    let last = ev.last().unwrap();
    assert_eq!((last.files_done, last.bytes_done), (12, plan.total_bytes()));
    assert_eq!(last.files_total, 12);
    assert!(
        ev.windows(2)
            .all(|w| w[1].bytes_done >= w[0].bytes_done || w[1].files_done != w[0].files_done)
    );
    assert!(last.bytes_per_sec > 0.0 && last.eta == Some(Duration::ZERO));
}

struct Faults {
    yank_after: Option<(String, u64)>,
    full_after: Option<usize>,
    writes: AtomicUsize,
    corrupt: Option<String>,
}

impl Faults {
    fn none() -> Self {
        Self {
            yank_after: None,
            full_after: None,
            writes: AtomicUsize::new(0),
            corrupt: None,
        }
    }
}

impl FaultInjector for Faults {
    fn before_read(&self, src: &Path, offset: u64) -> io::Result<()> {
        match &self.yank_after {
            Some((name, at)) if src.to_string_lossy().contains(name.as_str()) && offset >= *at => {
                Err(io::Error::new(io::ErrorKind::NotConnected, "card removed"))
            }
            _ => Ok(()),
        }
    }
    fn before_write(&self, _: &Path, _: u64) -> io::Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst);
        match self.full_after {
            Some(limit) if n >= limit => Err(io::Error::from(io::ErrorKind::StorageFull)),
            _ => Ok(()),
        }
    }
    fn before_verify(&self, partial: &Path) {
        if self
            .corrupt
            .as_deref()
            .is_some_and(|c| partial.to_string_lossy().contains(c))
        {
            let mut b = fs::read(partial).unwrap();
            b[10] ^= 0xff;
            fs::write(partial, b).unwrap();
        }
    }
}

#[test]
fn card_pulled_mid_copy_fails_cleanly_then_resumes() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 4, 200_000);
    let before = snapshot(&card);
    let mut plan = plan_for(&card, &dest);
    plan.chunk_size = 32 * 1024;
    plan.concurrency = 1;

    let faults = Faults {
        yank_after: Some(("DSC00002.ARW".into(), 64 * 1024)),
        ..Faults::none()
    };
    let hooks = RunHooks {
        faults: Some(&faults),
        ..RunHooks::default()
    };
    let r1 = run_with(&plan, hooks, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r1.failed.len(), 1);
    assert!(
        r1.failed[0].reason.contains("card removed"),
        "{:?}",
        r1.failed
    );
    assert_eq!(r1.copied.len(), 7);
    assert!(!r1.is_complete());
    assert!(partials_under(&dest).is_empty(), "no partial left behind");
    assert!(!dest.join("DSC00002.ARW").exists());
    let info = pending_resume(&dest).unwrap().unwrap();
    assert_eq!(
        (info.done, info.pending, info.failed),
        (7, 0, 1),
        "{info:?}"
    );
    assert_eq!(info.volumes.len(), 1, "the card that resumes it: {info:?}");
    let _: ResumeInfo = info;

    // Card reinserted: re-scan + re-plan resumes. Only the failed file is copied.
    let plan2 = plan_for(&card, &dest);
    let r2 = run(&plan2, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.copied.iter().filter(|f| !f.resumed).count(), 1);
    assert_eq!(r2.copied.iter().filter(|f| f.resumed).count(), 7);
    assert_eq!(r2.bytes_copied, 200_000);
    assert!(
        !dest.join("DSC00002-1.ARW").exists(),
        "resume must not make -1 copies"
    );
    assert_eq!(
        fs::read(dest.join("DSC00002.ARW")).unwrap(),
        fs::read(card.join("DCIM/100MSDCF/DSC00002.ARW")).unwrap()
    );
    assert!(pending_resume(&dest).unwrap().is_none());
    assert_eq!(snapshot(&card), before);
}

#[test]
fn destination_full_stops_new_work_and_leaves_no_partials() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 5, 100_000);
    let mut plan = plan_for(&card, &dest);
    plan.concurrency = 1;
    plan.chunk_size = 16 * 1024;
    let faults = Faults {
        full_after: Some(12),
        ..Faults::none()
    };
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&faults),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_ne!(r.copied, [] as [lt_import::ImportedFile; 0]);
    assert!(r.failed.iter().any(|f| f.reason.contains("writing")));
    assert!(
        r.failed
            .iter()
            .any(|f| f.reason.contains("destination is full"))
    );
    assert_eq!(r.copied.len() + r.failed.len(), 10);
    assert_eq!(partials_under(&dest), [] as [std::path::PathBuf; 0]);
}

#[test]
fn bit_rot_in_backup_fails_only_the_backup_and_in_primary_fails_the_file() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("bk"),
    );
    make_card(&card, 2, 50_000);
    let mut opts = PlanOptions::new(&dest);
    opts.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts, &NoMeta).unwrap();
    let rotten = |p: PathBuf| Faults {
        corrupt: Some(
            lt_import::fsio::partial_path(&p)
                .to_string_lossy()
                .into_owned(),
        ),
        ..Faults::none()
    };
    // Backup copy corrupted between write and verify: primary still lands.
    let faults = rotten(backup.join("DSC00001.ARW"));
    let hooks = RunHooks {
        faults: Some(&faults),
        sink: None,
    };
    let r = run_with(&plan, hooks, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(r.backup_failed.len(), 1);
    assert!(r.backup_failed[0].reason.contains("verification failed"));
    assert!(!r.is_complete(), "backup still owed");
    assert!(dest.join("DSC00001.ARW").exists() && !backup.join("DSC00001.ARW").exists());
    let imported = r
        .copied
        .iter()
        .find(|f| f.dest.ends_with("DSC00001.ARW"))
        .unwrap();
    assert_eq!(imported.backup, None);
    assert!(partials_under(&dest).is_empty() && partials_under(&backup).is_empty());
    assert_eq!(pending_resume(&dest).unwrap().unwrap().pending, 1);
    // Resume copies only the missing backup.
    let r2 = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r2.is_complete(), "{r2:?}");
    assert_eq!(r2.bytes_copied, 0, "primary was not copied again");
    assert!(r2.copied.iter().all(|f| f.resumed && f.backup.is_some()));
    assert_eq!(
        fs::read(backup.join("DSC00001.ARW")).unwrap(),
        fs::read(card.join("DCIM/100MSDCF/DSC00001.ARW")).unwrap()
    );

    // Primary copy corrupted: the file fails and nothing lands at the primary.
    let dest2 = t.path().join("dest2");
    let plan2 = plan_for(&card, &dest2);
    let faults = rotten(dest2.join("DSC00000.JPG"));
    let r = run_with(
        &plan2,
        RunHooks {
            faults: Some(&faults),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].reason.contains("verification failed"));
    assert!(!dest2.join("DSC00000.JPG").exists());
    assert_eq!(partials_under(&dest2), [] as [std::path::PathBuf; 0]);
}

struct CancelMidWrite<'a>(&'a AtomicBool, AtomicUsize);

impl FaultInjector for CancelMidWrite<'_> {
    fn before_write(&self, _: &Path, _: u64) -> io::Result<()> {
        if self.1.fetch_add(1, Ordering::SeqCst) == 100 {
            self.0.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn cancel_leaves_no_partials_and_resume_finishes() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 8, 400_000);
    let mut plan = plan_for(&card, &dest);
    plan.chunk_size = 8 * 1024;
    let cancel = AtomicBool::new(false);
    // Cancel from inside a write, i.e. genuinely mid-file.
    let fi = CancelMidWrite(&cancel, AtomicUsize::new(0));
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&fi),
            sink: None,
        },
        &no_progress,
        &cancel,
    )
    .unwrap();
    assert!(r.cancelled && !r.is_complete());
    assert!(r.copied.len() < 16);
    assert!(
        partials_under(&dest).is_empty(),
        "{:?}",
        partials_under(&dest)
    );
    assert_ne!(journal_paths(&dest), [] as [std::path::PathBuf; 0]);

    let r2 = run(
        &plan_for(&card, &dest),
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r2.is_complete());
    assert_eq!(r2.copied.len(), 16);
    assert_eq!(
        r2.copied.iter().filter(|f| f.resumed).count(),
        r.copied.len()
    );
}

/// The library's copy of a card file (a real file, so dedupe can verify it).
fn library_copy(lib: &Path, card_file: &Path) -> (u64, PartialHash, KnownCopy) {
    fs::create_dir_all(lib).unwrap();
    let dest = lib.join(card_file.file_name().unwrap());
    fs::copy(card_file, &dest).unwrap();
    let size = fs::metadata(card_file).unwrap().len();
    (
        size,
        partial_hash(card_file).unwrap(),
        KnownCopy {
            hash: full_hash(card_file).unwrap(),
            path: Some(dest),
        },
    )
}

#[test]
fn duplicates_are_judged_per_file_including_sidecars() {
    let t = tempfile::tempdir().unwrap();
    // Dedupe only trusts library copies on another device than the card.
    let Some(img) =
        lt_fsutil::testvol::require(t.path(), lt_fsutil::testvol::Fs::Fat32, 64, "CARD")
    else {
        return;
    };
    let (card, dest, lib) = (
        img.mount_point().to_path_buf(),
        t.path().join("dest"),
        t.path().join("lib"),
    );
    make_card(&card, 3, 150_000);
    let dir = card.join("DCIM/100MSDCF");
    let lookup = Arc::new(MemoryDedupe::default());
    // The library holds DSC1's RAW+JPG, and only DSC2's RAW.
    for f in ["DSC00001.ARW", "DSC00001.JPG", "DSC00002.ARW"] {
        let (size, partial, copy) = library_copy(&lib, &dir.join(f));
        lookup.insert(size, partial, copy);
    }
    let mut opts = PlanOptions::new(&dest);
    opts.dedupe = Some(lookup);
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts, &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_eq!(r.skipped_duplicates.len(), 3);
    assert_eq!(r.copied.len(), 3);
    assert!(!dest.join("DSC00001.ARW").exists() && !dest.join("DSC00001.JPG").exists());
    assert!(!dest.join("DSC00002.ARW").exists());
    assert!(
        dest.join("DSC00002.JPG").exists(),
        "a sidecar never inherits the primary's duplicate status"
    );
}

#[test]
fn dedupe_read_error_fails_the_file() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    let lookup = Arc::new(MemoryDedupe::default());
    lookup.insert(
        1000,
        PartialHash([0; 32]),
        KnownCopy {
            hash: lt_core::FileHash([0; 32]),
            path: Some(t.path().join("x")),
        },
    );
    let mut opts = PlanOptions::new(&dest);
    opts.dedupe = Some(lookup);
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts, &NoMeta).unwrap();
    let faults = Faults {
        yank_after: Some(("DSC00000".into(), 0)),
        ..Faults::none()
    };
    let r = run_with(
        &plan,
        RunHooks {
            faults: Some(&faults),
            sink: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.failed.len(), 2);
    assert!(
        r.failed
            .iter()
            .any(|f| f.reason.contains("duplicate check")),
        "{r:?}"
    );
}

#[test]
fn source_changing_size_is_caught() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 5000);
    let mut plan = plan_for(&card, &dest);
    plan.items[0].files[0].src.size = 6000; // scanned size no longer matches
    let r = run(&plan, &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].reason.contains("changed size"));
    assert_eq!(partials_under(&dest), [] as [std::path::PathBuf; 0]);
}

#[test]
fn sink_sees_every_verified_file() {
    struct Sink(Mutex<Vec<PathBuf>>);
    impl SessionSink for Sink {
        fn imported(&self, f: &ImportedFile) -> Result<(), String> {
            self.0.lock().unwrap().push(f.dest.clone());
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 3, 1000);
    let sink = Sink(Mutex::new(Vec::new()));
    let hooks = RunHooks {
        sink: Some(&sink),
        faults: None,
    };
    assert!(format!("{hooks:?}").contains("sink: true"));
    let r = run_with(
        &plan_for(&card, &dest),
        hooks,
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(sink.0.lock().unwrap().len(), r.copied.len());
}

#[test]
fn unwritable_destination_fails_files_not_the_run() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    make_card(&card, 1, 1000);
    let mut opts = PlanOptions::new(&dest);
    opts.subfolder = Some("sub".into());
    let plan = ImportPlan::new(&scan_volume(&card).unwrap().candidates, opts, &NoMeta).unwrap();
    fs::create_dir_all(dest.join("sub")).unwrap();
    fs::set_permissions(dest.join("sub"), fs::Permissions::from_mode(0o555)).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(false));
    fs::set_permissions(dest.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
    let r = r.unwrap();
    assert_eq!(r.failed.len(), 2, "{r:?}");
    assert!(r.failed[0].reason.contains("creating"));
    // Destination root that cannot be created is a run error.
    let file = t.path().join("file");
    fs::write(&file, b"x").unwrap();
    let mut bad = plan_for(&card, &dest);
    bad.dest = file.join("x");
    assert!(matches!(
        run(&bad, &no_progress, &AtomicBool::new(false)),
        Err(ImportError::Io { .. })
    ));
}

#[test]
fn huge_file_count_scans_fast_and_imports() {
    let t = tempfile::tempdir().unwrap();
    let (card, dest) = (t.path().join("card"), t.path().join("dest"));
    let dir = card.join("DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    for i in 0..2000 {
        fs::write(dir.join(format!("DSC{i:05}.ARW")), [i as u8; 16]).unwrap();
    }
    let t0 = Instant::now();
    let c = scan_volume(&card).unwrap().candidates;
    let scan_time = t0.elapsed();
    println!("scan of 2000 files: {scan_time:?}");
    assert_eq!(c.len(), 2000);
    // Budget: 100 ms for 2,000 files in release (measured ~5 ms). Debug
    // builds (coverage runs) are unoptimized, so allow 10x there.
    let budget = if cfg!(debug_assertions) { 1000 } else { 100 };
    assert!(
        scan_time < Duration::from_millis(budget),
        "scan took {scan_time:?}"
    );
    let mut opts = PlanOptions::new(&dest);
    opts.concurrency = 8;
    let plan = ImportPlan::new(&c, opts, &NoMeta).unwrap();
    assert_eq!(plan.file_count(), 2000);
    // Copy a slice of it: every file costs a full-device flush (F_FULLFSYNC).
    let mut some = plan.clone();
    some.items.truncate(400);
    let r = run(&some, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete());
    assert_eq!(r.copied.len(), 400);
}

#[test]
fn real_arw_samples_import_when_present() {
    let samples = lt_testdata::samples_with_ext("arw");
    if samples.len() < 2 {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let card = t.path().join("card/DCIM/100MSDCF");
    fs::create_dir_all(&card).unwrap();
    for s in &samples[..2] {
        // Clone (APFS) keeps this cheap; falls back to a copy elsewhere.
        fs::copy(s, card.join(s.file_name().unwrap())).unwrap();
    }
    let dest = t.path().join("dest");
    let r = run(
        &plan_for(&t.path().join("card"), &dest),
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.is_complete());
    for f in &r.copied {
        assert_eq!(f.hash, full_hash(&f.src).unwrap());
    }
}
