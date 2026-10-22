//! Archiver tests: a fake runner for argument/snapshot/error behavior and a
//! real run of the system rsync against temp dirs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ds_archive::*;

struct Fake {
    calls: Mutex<Vec<CommandSpec>>,
    /// Lines of the `--exclude-from` file of the last call (read while it exists).
    excluded: Mutex<Vec<String>>,
    code: Option<i32>,
    /// Remove this directory while "rsync" runs (target unmounted mid-run).
    yank: Option<PathBuf>,
}

impl CommandRunner for Fake {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        _: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        self.calls.lock().unwrap().push(cmd.clone());
        for a in &cmd.args {
            if let Some(f) = a.to_string_lossy().strip_prefix("--exclude-from=") {
                *self.excluded.lock().unwrap() = fs::read_to_string(f)
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect();
            }
        }
        if cmd.args == ["--version"] {
            on_line("rsync  version 3.4.1  protocol version 32");
            return Ok(CommandOutput {
                code: Some(0),
                stderr: String::new(),
            });
        }
        on_line("  10 100%  1.00MB/s 0:00:00 (xfr#1, to-chk=0/1)");
        if let Some(y) = &self.yank {
            fs::remove_dir_all(y).unwrap();
            return Ok(CommandOutput {
                code: Some(23),
                stderr: "rsync: write failed: No such file or directory".into(),
            });
        }
        Ok(CommandOutput {
            code: self.code,
            stderr: String::new(),
        })
    }
}

struct Snap(Mutex<u32>, bool);

impl Snapshotter for Snap {
    fn snapshot(&self, session: &Path, dest: &Path) -> Result<(), String> {
        *self.0.lock().unwrap() += 1;
        assert!(dest.starts_with(session));
        if self.1 {
            return Err("database is locked".into());
        }
        fs::write(dest, b"snapshot").map_err(|e| e.to_string())
    }
}

fn session(root: &Path) -> PathBuf {
    let s = root.join("2026-09-25 Hike");
    fs::create_dir_all(s.join("Capture/.ds/settings")).unwrap();
    fs::create_dir_all(s.join(".ds/cache")).unwrap();
    fs::write(s.join("Capture/DSC1.ARW"), b"raw v1").unwrap();
    fs::write(s.join("Capture/.ds/settings/DSC1.ARW.json"), b"{}").unwrap();
    fs::write(s.join(".ds/cache/thumb.jpg"), b"cache").unwrap();
    fs::write(s.join("Capture/DSC2.ARW.partial"), b"half").unwrap();
    fs::write(s.join("2026-09-25 Hike.dsession"), b"db").unwrap();
    fs::write(s.join("2026-09-25 Hike.dsession-wal"), b"wal").unwrap();
    s
}

fn fake_archiver(fake: Arc<Fake>, snap: Option<Arc<dyn Snapshotter>>, dir: &Path) -> Archiver {
    let exe = dir.join("rsync");
    fs::write(&exe, b"").unwrap();
    Archiver::new(RsyncRunner::detect_from(fake, &[exe]).unwrap(), snap)
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
}

#[test]
fn snapshot_then_rsync_with_safe_args() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let target_root = t.path().join("archive");
    fs::create_dir(&target_root).unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let snap = Arc::new(Snap(Mutex::new(0), false));
    let a = fake_archiver(fake.clone(), Some(snap.clone()), t.path());
    assert!(format!("{a:?}").contains("snapshotter: true"));
    let mut progress = vec![];
    let out = a
        .archive(
            &s,
            &ArchiveTarget::new(&target_root),
            now(),
            &mut |p| progress.push(p.percent),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(*snap.0.lock().unwrap(), 1);
    assert_eq!(out.snapshot, Some(s.join(SNAPSHOT_REL)));
    assert_eq!(fs::read(s.join(SNAPSHOT_REL)).unwrap(), b"snapshot");
    assert_eq!(out.dest, target_root.join("2026-09-25 Hike"));
    assert!(out.backup_dir.starts_with(".ds-archive-trash/20"));
    assert_eq!(
        progress,
        [100, 100, 100],
        "main, checksum and snapshot passes"
    );
    let calls = fake.calls.lock().unwrap();
    let strings = |c: &CommandSpec| -> Vec<String> {
        c.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    // The snapshot pass is last and is never backed up.
    let snap_pass = strings(&calls[calls.len() - 1]);
    assert!(!snap_pass.iter().any(|a| a.starts_with("--backup")));
    assert!(snap_pass.contains(&"--include=/.ds/index-snapshot.db".to_owned()));
    let args = strings(&calls[calls.len() - 2]);
    assert!(args.contains(&format!("--backup-dir={}", out.backup_dir)));
    // Patterns go through an --exclude-from file, not the command line.
    assert!(args.iter().any(|a| a.starts_with("--exclude-from=")));
    assert!(!args.iter().any(|a| a.contains("dsession")));
    assert!(
        fake.excluded
            .lock()
            .unwrap()
            .contains(&"*.dsession-wal".to_owned())
    );
    assert!(!args.iter().any(|a| a.contains("delete")));
    assert!(
        !fake
            .excluded
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.contains("index-snapshot")),
        "snapshot must not be excluded"
    );
}

#[test]
fn snapshot_failure_is_a_problem_but_the_originals_still_archive() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(
        fake.clone(),
        Some(Arc::new(Snap(Mutex::new(0), true))),
        t.path(),
    );
    let out = a
        .archive(
            &s,
            &ArchiveTarget::new(&root),
            now(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.snapshot, None);
    // Not a mere warning: the archived snapshot is stale, so the pass is incomplete.
    assert_eq!(out.warnings, [] as [std::string::String; 0]);
    assert!(
        out.problems[0].contains("database is locked") && out.problems[0].contains("snapshot"),
        "{:?}",
        out.problems
    );
    assert_eq!(
        fake.calls.lock().unwrap().len(),
        4,
        "rsync still ran (version probe + 3 passes)"
    );
}

#[test]
fn unavailable_target_runs_nothing() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let snap = Arc::new(Snap(Mutex::new(0), false));
    let a = fake_archiver(fake.clone(), Some(snap.clone()), t.path());
    // A stale mount-point directory (not a mount): offline, nothing created.
    let mut target = ArchiveTarget::new(t.path().join("not-mounted").join("Darkslide"));
    target.mount_point = Some(t.path().join("not-mounted"));
    std::fs::create_dir(t.path().join("not-mounted")).unwrap();
    let err = a
        .archive(&s, &target, now(), &mut |_| {}, &AtomicBool::new(false))
        .unwrap_err();
    assert!(matches!(err, ArchiveError::TargetUnavailable { .. }));
    assert_eq!(*snap.0.lock().unwrap(), 0);
    assert_eq!(
        fake.calls.lock().unwrap().len(),
        1,
        "only the --version probe"
    );
    let err = a.archive(
        &t.path().join("no-session"),
        &target,
        now(),
        &mut |_| {},
        &AtomicBool::new(false),
    );
    assert!(matches!(err, Err(ArchiveError::InvalidSession(_))));
    // A relative session (resolved against whatever the working directory is) is refused.
    let err = a.archive(
        Path::new("src"),
        &target,
        now(),
        &mut |_| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(err, Err(ArchiveError::InvalidSession(_))),
        "{err:?}"
    );
}

#[test]
fn target_unmounted_mid_run_fails_then_scheduler_backs_off() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: Some(root.clone()),
    });
    let a = fake_archiver(fake, None, t.path());
    // After the first run the volume is gone: its mount point is a stale plain directory
    // (a missing root on a mounted volume would be created instead).
    let unmounted = AtomicBool::new(false);
    let target = || {
        let mut t = ArchiveTarget::new(&root);
        if unmounted.load(std::sync::atomic::Ordering::SeqCst) {
            t.mount_point = root.parent().map(Path::to_path_buf);
        }
        t
    };
    let clock = Arc::new(ManualClock::new(now()));
    let sched = Scheduler::new(
        SchedulerConfig {
            lock_dir: Some(t.path().join("locks")),
            ..SchedulerConfig::default()
        },
        clock.clone(),
        &root,
    );
    sched.note_changes(3);
    sched.request_now();
    let mut job = |_| {
        a.archive(
            &s,
            &target(),
            clock.wall(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .map(|_| ())
    };
    let rec = sched.run_if_due(&mut job).unwrap();
    assert!(
        matches!(rec, RunRecord::Failed(Trigger::OnDemand, ref e) if e.contains("23")),
        "{rec:?}"
    );
    // Next attempt (after backoff): the volume is gone → Unavailable, still pending.
    unmounted.store(true, std::sync::atomic::Ordering::SeqCst);
    clock.advance(Duration::from_secs(61));
    // A Busy (the target's lock file held for a moment by anything else; seen once in
    // a coverage run) is not what this test is about: the scheduler backs off and
    // retries, so do the same (Busy counts neither as a failure nor as a change).
    let mut rec = sched.run_if_due(&mut job).unwrap();
    for _ in 0..10 {
        if !matches!(rec, RunRecord::Busy(_)) {
            break;
        }
        clock.advance(SchedulerConfig::default().busy_backoff + Duration::from_secs(1));
        rec = sched.run_if_due(&mut job).unwrap();
    }
    assert!(matches!(rec, RunRecord::Unavailable(..)), "{rec:?}");
    let st = sched.status();
    assert_eq!(st.pending_changes, 3);
    assert!(st.behind_since.is_some());
    assert_eq!(st.consecutive_failures, 2);
}

/// Real rsync (openrsync on stock macOS, 3.x if Homebrew's is installed).
#[test]
fn real_rsync_archives_safely() {
    let Ok(rsync) = RsyncRunner::detect(Arc::new(SystemRunner)) else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let a = Archiver::new(rsync, Some(Arc::new(Snap(Mutex::new(0), false))));
    let target = ArchiveTarget::new(&root);
    let out = a
        .archive(&s, &target, now(), &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    let d = &out.dest;
    assert_eq!(fs::read(d.join("Capture/DSC1.ARW")).unwrap(), b"raw v1");
    assert!(d.join("Capture/.ds/settings/DSC1.ARW.json").exists());
    assert_eq!(fs::read(d.join(SNAPSHOT_REL)).unwrap(), b"snapshot");
    assert!(!d.join(".ds/cache").exists(), "cache excluded");
    assert!(
        !d.join("Capture/DSC2.ARW.partial").exists(),
        "partials excluded"
    );
    assert!(
        !d.join("2026-09-25 Hike.dsession-wal").exists(),
        "live WAL excluded"
    );

    // Change one file and delete another in the session, archive again.
    fs::write(s.join("Capture/DSC1.ARW"), b"raw v2 (edited in place)").unwrap();
    fs::remove_file(s.join("Capture/.ds/settings/DSC1.ARW.json")).unwrap();
    let out2 = a
        .archive(
            &s,
            &target,
            now() + Duration::from_secs(3600),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        fs::read(d.join("Capture/DSC1.ARW")).unwrap(),
        b"raw v2 (edited in place)"
    );
    assert!(
        d.join("Capture/.ds/settings/DSC1.ARW.json").exists(),
        "never --delete"
    );
    let old = d.join(&out2.backup_dir).join("Capture/DSC1.ARW");
    assert_eq!(
        fs::read(&old).unwrap(),
        b"raw v1",
        "replaced file kept in the archive trash"
    );
}

/// Thousands of rejected originals: one `--exclude-from` file, a short command line.
#[test]
fn many_excludes_go_through_a_file() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(fake.clone(), None, t.path());
    let mut target = ArchiveTarget::new(&root);
    let rejects: Vec<String> = (0..5000)
        .map(|i| format!("/Capture/DSC{i:05}.ARW"))
        .collect();
    target.excludes.extend(rejects.iter().cloned());
    target.excludes.push("/bad\nname".to_owned());
    a.archive(&s, &target, now(), &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    let calls = fake.calls.lock().unwrap();
    let args = &calls.last().unwrap().args;
    assert!(args.len() < 40, "{} arguments", args.len());
    let excluded = fake.excluded.lock().unwrap();
    assert!(rejects.iter().all(|r| excluded.contains(r)));
    assert!(
        !excluded.iter().any(|l| l.contains("bad")),
        "a line break can't be written"
    );
}

fn tree(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// Move to Selects after an archive run: the target ends up with the file only under
/// Selects (renamed there; if it already had a Selects copy, the old Capture one goes
/// to the archive trash). A reject moved to the session's Trash stays archived where it
/// was. Nothing on the target is deleted.
#[test]
fn moves_between_session_folders_are_mirrored_without_deleting() {
    let t = tempfile::tempdir().unwrap();
    let s = t.path().join("sess");
    for d in ["Capture", "Selects", "Trash"] {
        fs::create_dir_all(s.join(d)).unwrap();
    }
    let dest = t.path().join("archive/sess");
    fs::create_dir_all(dest.join("Capture/sub")).unwrap();
    fs::create_dir_all(dest.join("Selects")).unwrap();
    for (p, body) in [
        ("Capture/moved.ARW", "m"),
        ("Capture/sub/nested.ARW", "n"),
        ("Capture/dup.ARW", "old"),
        ("Selects/dup.ARW", "new"),
        ("Capture/reject.ARW", "r"),
        ("Capture/stays.ARW", "s"),
    ] {
        fs::write(dest.join(p), body).unwrap();
    }
    // The session now: moved and dup (and nested) in Selects, reject in Trash.
    for p in [
        "Selects/moved.ARW",
        "Selects/sub/nested.ARW",
        "Selects/dup.ARW",
        "Trash/reject.ARW",
        "Capture/stays.ARW",
    ] {
        fs::create_dir_all(s.join(p).parent().unwrap()).unwrap();
        fs::write(s.join(p), "x").unwrap();
    }
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(fake, None, t.path());
    let mut target = ArchiveTarget::new(t.path().join("archive"));
    // Without mirror folders nothing moves.
    a.archive_to(
        &s,
        &target,
        &dest,
        now(),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(dest.join("Capture/moved.ARW").exists());
    target.mirror_folders = vec!["Capture".into(), "Selects".into()];
    let out = a
        .archive_to(
            &s,
            &target,
            &dest,
            now(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    let files = tree(&dest);
    let trash = format!("{}/Capture/dup.ARW", out.backup_dir);
    assert_eq!(
        files,
        {
            let mut v = vec![
                "Capture/reject.ARW".to_owned(),
                "Capture/stays.ARW".to_owned(),
                "Selects/dup.ARW".to_owned(),
                "Selects/moved.ARW".to_owned(),
                "Selects/sub/nested.ARW".to_owned(),
                trash.clone(),
            ];
            v.sort();
            v
        },
        "one copy per file, the displaced one in the trash"
    );
    assert_eq!(
        fs::read_to_string(dest.join("Selects/moved.ARW")).unwrap(),
        "m"
    );
    assert_eq!(
        fs::read_to_string(dest.join("Selects/dup.ARW")).unwrap(),
        "new"
    );
    assert_eq!(fs::read_to_string(dest.join(&trash)).unwrap(), "old");
}

/// The session's moves are found from what it listed at the last pass, not by walking
/// the target: a stray file the session happens to hold elsewhere is left alone, a real
/// move since the last pass is still mirrored.
/// Audit: the mirror listing lives in the session's cache; a tampered entry with `..`
/// (or an absolute path) is dropped, so no rename ever reaches outside the destination.
#[test]
fn a_tampered_listing_never_moves_files_outside_the_destination() {
    let t = tempfile::tempdir().unwrap();
    let s = t.path().join("sess");
    for d in ["Capture", "Output/Selects", ".ds/cache"] {
        fs::create_dir_all(s.join(d)).unwrap();
    }
    let dest = t.path().join("archive/sess");
    for d in ["Capture", "Output/Selects"] {
        fs::create_dir_all(dest.join(d)).unwrap();
    }
    // `Output/Selects/../../x` on the target, `Capture/../../x` next to the session.
    fs::write(dest.join("x"), "archived").unwrap();
    fs::write(t.path().join("x"), "bait").unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(fake, None, t.path());
    let mut target = ArchiveTarget::new(t.path().join("archive"));
    target.mirror_folders = vec!["Capture".into(), "Output/Selects".into()];
    let run = || {
        a.archive_to(
            &s,
            &target,
            &dest,
            now(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap()
    };
    run(); // records the listing
    let listing = fs::read_dir(s.join(".ds/cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains("archive-mirror-"))
        .unwrap();
    fs::write(
        &listing,
        r#"{"Capture":[],"Output/Selects":["../../x","/abs/x","./x",""]}"#,
    )
    .unwrap();
    run();
    assert_eq!(fs::read_to_string(dest.join("x")).unwrap(), "archived");
    assert!(!t.path().join("archive/x").exists());
}

#[test]
fn later_passes_find_moves_without_walking_the_target() {
    let t = tempfile::tempdir().unwrap();
    let s = t.path().join("sess");
    for d in ["Capture", "Selects"] {
        fs::create_dir_all(s.join(d)).unwrap();
    }
    fs::write(s.join("Capture/a.ARW"), "a").unwrap();
    let dest = t.path().join("archive/sess");
    fs::create_dir_all(dest.join("Capture")).unwrap();
    fs::create_dir_all(dest.join("Selects")).unwrap();
    fs::write(dest.join("Capture/a.ARW"), "a").unwrap();
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(fake, None, t.path());
    let mut target = ArchiveTarget::new(t.path().join("archive"));
    target.mirror_folders = vec!["Capture".into(), "Selects".into()];
    let run = || {
        a.archive_to(
            &s,
            &target,
            &dest,
            now(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap()
    };
    run(); // first pass records the listing
    // Not a move the session made since: the target just holds an extra file that the
    // session has under Selects. A full walk would mirror it; the incremental pass must not.
    fs::write(dest.join("Capture/stray.ARW"), "s").unwrap();
    fs::write(s.join("Selects/stray.ARW"), "s").unwrap();
    // A real move since the last pass.
    fs::rename(s.join("Capture/a.ARW"), s.join("Selects/a.ARW")).unwrap();
    run();
    assert!(dest.join("Selects/a.ARW").exists());
    assert!(!dest.join("Capture/a.ARW").exists());
    assert!(
        dest.join("Capture/stray.ARW").exists(),
        "target was not walked"
    );
    // Losing the record falls back to the full walk.
    for e in fs::read_dir(s.join(".ds/cache")).unwrap() {
        fs::remove_file(e.unwrap().path()).unwrap();
    }
    run();
    assert!(dest.join("Selects/stray.ARW").exists());
}

fn make_trash(dest: &Path, names: &[(&str, usize)]) {
    for (n, bytes) in names {
        let d = dest.join(TRASH_DIR).join(n);
        fs::create_dir_all(&d).unwrap();
        if *bytes > 0 {
            fs::write(d.join("old.bin"), vec![0u8; *bytes]).unwrap();
        }
    }
}

fn trash_names(dest: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dest.join(TRASH_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn clean_run_removes_only_empty_trash_folders() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let target = ArchiveTarget::new(&root);
    let dest = target.dest_for(&s).unwrap();
    make_trash(
        &dest,
        &[
            ("20260101T000000.000000Z", 10),
            ("20260102T000000.000000Z", 10),
            ("20260103T000000.000000Z", 0),
        ],
    );
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    let a = fake_archiver(fake, None, t.path());
    let out = a
        .archive(&s, &target, now(), &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    // Nothing is ever deleted from the archive: only the empty folder goes.
    assert_eq!(
        trash_names(&out.dest),
        ["20260101T000000.000000Z", "20260102T000000.000000Z"].map(str::to_owned),
    );
    assert_eq!(out.warnings.len(), 0, "{:?}", out.warnings);
}

#[test]
fn prune_trash_keeps_the_current_folder_and_every_file() {
    let t = tempfile::tempdir().unwrap();
    let dest = t.path();
    make_trash(
        dest,
        &[
            ("20260101T000000.000000Z", 0),
            ("20260102T000000.000000Z", 0),
            ("20260103T000000.000000Z", 100),
        ],
    );
    let cur = format!("{TRASH_DIR}/20260102T000000.000000Z");
    assert_eq!(prune_trash(dest, &cur).unwrap(), 1);
    assert_eq!(
        trash_names(dest),
        ["20260102T000000.000000Z", "20260103T000000.000000Z"].map(str::to_owned)
    );
    // No trash folder at all is an error the caller turns into a warning.
    assert!(prune_trash(&dest.join("missing"), &cur).is_err());
}

#[test]
fn incomplete_run_keeps_the_trash() {
    let t = tempfile::tempdir().unwrap();
    let s = session(t.path());
    let root = t.path().join("archive");
    fs::create_dir(&root).unwrap();
    let target = ArchiveTarget::new(&root);
    let dest = target.dest_for(&s).unwrap();
    make_trash(
        &dest,
        &[
            ("20260101T000000.000000Z", 10),
            ("20260102T000000.000000Z", 10),
        ],
    );
    let fake = Arc::new(Fake {
        calls: Mutex::default(),
        excluded: Mutex::default(),
        code: Some(0),
        yank: None,
    });
    // A failed snapshot makes the pass incomplete: nothing is pruned.
    let a = fake_archiver(fake, Some(Arc::new(Snap(Mutex::new(0), true))), t.path());
    let out = a
        .archive(&s, &target, now(), &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    assert_ne!(out.problems, [] as [std::string::String; 0]);
    assert_eq!(trash_names(&out.dest).len(), 2);
}
