//! Integration A review fixes: free-space admission (M1), a refused file is
//! not "imported" (H3), discard cleans the backup root (M5).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ds_import::*;

fn bytes(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn no_progress(_: &ImportProgress) {}

fn plan_for(card: &Path, dest: &Path) -> ImportPlan {
    ImportPlan::new(
        &scan_volume(card).unwrap().candidates,
        PlanOptions::new(dest),
        &NoMeta,
    )
    .unwrap()
}

/// `n` RAWs of `size` bytes under DCIM.
fn card(root: &Path, n: u8, size: usize) {
    let dir = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        fs::write(dir.join(format!("DSC0000{i}.ARW")), bytes(i, size)).unwrap();
    }
}

/// Bytes of every regular file under `root` (partials included).
fn used(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

fn partials(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else if e.path().to_string_lossy().ends_with(".partial") {
                out.push(e.path());
            }
        }
    }
    out
}

/// A destination "volume" of `capacity` bytes: free = capacity − bytes written.
/// Writes past the capacity fail like a real full disk (ENOSPC).
struct SmallDisk {
    capacity: u64,
    dest: PathBuf,
}

impl FaultInjector for SmallDisk {
    fn free_bytes(&self, root: &Path) -> Option<io::Result<u64>> {
        assert_eq!(root, self.dest);
        Some(Ok(self.capacity.saturating_sub(used(&self.dest))))
    }
    fn before_write(&self, _partial: &Path, _offset: u64) -> io::Result<()> {
        if used(&self.dest) > self.capacity {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        Ok(())
    }
}

fn run_on(plan: &ImportPlan, disk: &SmallDisk) -> ImportReport {
    let hooks = RunHooks {
        sink: None,
        faults: Some(disk),
    };
    run_with(plan, hooks, &no_progress, &AtomicBool::new(false)).unwrap()
}

/// M1: a near-full destination copies what fits instead of letting parallel
/// copies each take part of the space and all fail (review: 0 of 3 copied).
#[test]
fn near_full_destination_copies_what_fits_without_starving() {
    let t = tempfile::tempdir().unwrap();
    let (src, dest) = (t.path().join("card"), t.path().join("dest"));
    card(&src, 3, 50_000);
    fs::create_dir_all(&dest).unwrap();
    let mut plan = plan_for(&src, &dest);
    plan.concurrency = 3;
    let r = run_on(
        &plan,
        &SmallDisk {
            capacity: 90_000, // one 50 kB file fits, two don't
            dest: dest.clone(),
        },
    );
    assert_eq!(r.copied.len(), 1, "exactly one fits: {r:?}");
    assert_eq!(r.failed.len(), 2);
    assert!(
        r.failed
            .iter()
            .all(|f| f.reason.contains("not enough free space")),
        "no ENOSPC race: {r:?}"
    );
    assert_eq!(partials(&dest), [] as [std::path::PathBuf; 0]);

    // Room for everything: all three, still in parallel.
    let dest2 = t.path().join("dest2");
    fs::create_dir_all(&dest2).unwrap();
    let mut plan = plan_for(&src, &dest2);
    plan.concurrency = 3;
    let r = run_on(
        &plan,
        &SmallDisk {
            capacity: 200_000,
            dest: dest2.clone(),
        },
    );
    assert_eq!(r.copied.len(), 3, "{r:?}");
    assert!(r.is_complete());
}

/// H3: a file the session refuses is not "imported": it is reported failed,
/// the journal stays, and a resume delivers it again.
#[test]
fn a_refused_file_fails_keeps_the_journal_and_is_redelivered() {
    struct Sink {
        refuse: AtomicBool,
        took: Mutex<Vec<PathBuf>>,
    }
    impl SessionSink for Sink {
        fn imported(&self, f: &ImportedFile) -> Result<(), String> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err("Permission denied".into());
            }
            self.took.lock().unwrap().push(f.dest.clone());
            Ok(())
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (src, dest) = (t.path().join("card"), t.path().join("dest"));
    card(&src, 2, 1000);
    let sink = Sink {
        refuse: AtomicBool::new(true),
        took: Mutex::default(),
    };
    let hooks = RunHooks {
        sink: Some(&sink),
        faults: None,
    };
    let r = run_with(
        &plan_for(&src, &dest),
        hooks,
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.copied.is_empty() && r.failed.len() == 2, "{r:?}");
    assert!(
        r.failed
            .iter()
            .all(|f| f.reason.starts_with(NOT_REGISTERED))
    );
    assert!(!r.is_complete());
    assert!(pending_resume(&dest).unwrap().is_some(), "journal kept");
    // Fixed: the resume re-delivers the (already verified) files.
    sink.refuse.store(false, Ordering::SeqCst);
    let r = run_with(
        &plan_for(&src, &dest),
        hooks,
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(r.is_complete(), "{r:?}");
    assert_eq!(r.copied.len(), 2);
    assert!(r.copied.iter().all(|c| c.resumed));
    assert_eq!(sink.took.lock().unwrap().len(), 2);
    assert!(pending_resume(&dest).unwrap().is_none());
}

/// A sink that panics fails the file the same way.
#[test]
fn a_panicking_sink_fails_the_file() {
    struct Sink;
    impl SessionSink for Sink {
        fn imported(&self, _: &ImportedFile) -> Result<(), String> {
            panic!("index broke")
        }
    }
    let t = tempfile::tempdir().unwrap();
    let (src, dest) = (t.path().join("card"), t.path().join("dest"));
    card(&src, 1, 500);
    let r = run_with(
        &plan_for(&src, &dest),
        RunHooks {
            sink: Some(&Sink),
            faults: None,
        },
        &no_progress,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].reason.contains("panicked"), "{r:?}");
}

/// M5: discarding an unfinished import also removes the backup root's
/// `.partial`s (the journal remembers the backup root).
#[test]
fn discard_cleans_the_recorded_backup_root() {
    let t = tempfile::tempdir().unwrap();
    let (src, dest, backup) = (
        t.path().join("card"),
        t.path().join("dest"),
        t.path().join("backup"),
    );
    card(&src, 2, 1000);
    let mut opts = PlanOptions::new(&dest);
    opts.backup_dest = Some(backup.clone());
    let plan = ImportPlan::new(&scan_volume(&src).unwrap().candidates, opts, &NoMeta).unwrap();
    let r = run(&plan, &no_progress, &AtomicBool::new(true)).unwrap(); // stopped at once
    assert!(r.cancelled);
    let info = pending_resume(&dest).unwrap().unwrap();
    assert_eq!(info.backup_dests, vec![backup.clone()]);
    assert_eq!(info.volumes.len(), 1);
    // A torn backup copy left by the interrupted run.
    let partial = ds_import::fsio::partial_path(&backup.join(&plan.items[0].files[0].rel));
    fs::create_dir_all(partial.parent().unwrap()).unwrap();
    fs::write(&partial, b"torn").unwrap();
    discard_journal(&dest, None).unwrap();
    assert!(!partial.exists(), "backup partial removed");
    assert!(pending_resume(&dest).unwrap().is_none());
}
