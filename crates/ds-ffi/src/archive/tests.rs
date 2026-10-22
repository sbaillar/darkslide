use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ds_archive::{CommandOutput, CommandRunner, CommandSpec, SystemClock, SystemRunner, VolumeId};
use ds_core::FileHash;
use ds_session::{ImageMetaLite, Session};
use futures_executor::block_on;

use super::*;
use crate::jobs::CancelToken;
use crate::session::{Probe, SessionHandle, open_in_place, open_session};

fn config(target: &Path, lock_dir: &Path) -> ArchiveConfig {
    ArchiveConfig {
        enabled: true,
        target: target.to_string_lossy().into_owned(),
        interval_secs: 3600,
        debounce_secs: 0,
        lock_dir: Some(lock_dir.to_string_lossy().into_owned()),
        check_timeout_ms: 2000,
        rsync_path: None,
        archive_rejected: true,
        amazon_rejected: false,
        // As the app does when the user confirms the target.
        target_volume: archive_volume_identity(target.to_string_lossy().into_owned()),
    }
}

fn manager(
    cfg: ArchiveConfig,
    check: Arc<CheckFn>,
    runner: Arc<dyn CommandRunner>,
    rsync: PathBuf,
) -> Arc<ArchiveManager> {
    ArchiveManager::with(
        cfg,
        check,
        runner,
        vec![rsync],
        Arc::new(SystemClock::default()),
    )
    .unwrap()
}

fn manager_t(
    cfg: ArchiveConfig,
    check: Arc<CheckFn>,
    runner: Arc<dyn CommandRunner>,
    rsync: PathBuf,
    timing: Timing,
) -> Arc<ArchiveManager> {
    ArchiveManager::with_timing(
        cfg,
        check,
        runner,
        vec![rsync],
        Arc::new(SystemClock::default()),
        timing,
    )
    .unwrap()
}

/// Where a session's archive landed (its claimed folder).
fn dest_of(m: &ArchiveManager, target: &Path, name: &str) -> PathBuf {
    let st = m.status().unwrap();
    let s = st
        .sessions
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no session {name}: {st:?}"));
    target.join(s.archive_folder.as_deref().expect("claimed folder"))
}

fn hash(p: &Path) -> FileHash {
    FileHash(*blake3::hash(&fs::read(p).unwrap()).as_bytes())
}

/// An imported (internal) session named `name` under `parent` holding `files`
/// under `Capture/`, opened through the exported `open_session`.
fn internal_session(parent: &Path, name: &str, files: &[PathBuf]) -> Arc<SessionHandle> {
    let root = parent.join(name);
    let mut s = Session::create(&root, name).unwrap();
    let mut items = Vec::new();
    for f in files {
        let rel = format!("Capture/{}", f.file_name().unwrap().to_string_lossy());
        fs::copy(f, root.join(&rel)).unwrap();
        items.push((
            rel.clone(),
            hash(&root.join(&rel)),
            ImageMetaLite::default(),
        ));
    }
    s.add_originals(items).unwrap();
    drop(s);
    block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap()
}

/// Real samples, or small stand-ins when `samples/` is absent.
fn originals(dir: &Path) -> Vec<PathBuf> {
    let s = ds_testdata::samples_with_ext("ARW");
    if !s.is_empty() {
        return s;
    }
    (0..3)
        .map(|i| {
            let p = dir.join(format!("S{i}.ARW"));
            fs::write(&p, vec![i as u8; 4096 + i]).unwrap();
            p
        })
        .collect()
}

fn wait(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn runs(m: &ArchiveManager) -> Vec<ArchiveRun> {
    m.status().unwrap().runs
}

// ------------------------------------------------------------ target check ---

#[test]
fn target_checks_report_available_missing_and_empty() {
    let t = tempfile::tempdir().unwrap();
    let ok = check_with(t.path(), None, Duration::from_secs(2), Arc::new(real_check));
    assert!(
        ok.available && !ok.timed_out && ok.fs_type.as_deref() == Some("apfs"),
        "{ok:?}"
    );
    // A folder that doesn't exist yet on the volume it was confirmed on is available:
    // the first run creates it (the check itself creates nothing). Outside /Volumes
    // with no recorded volume it is not (review N3).
    let nope = t.path().join("nope");
    let missing = check_with(
        &nope,
        VolumeId::of(&nope),
        Duration::from_secs(2),
        Arc::new(real_check),
    );
    assert!(missing.available && !missing.timed_out, "{missing:?}");
    let unrecorded = check_with(&nope, None, Duration::from_secs(2), Arc::new(real_check));
    assert!(!unrecorded.available, "{unrecorded:?}");
    assert!(!nope.exists());
    let empty = check_with(
        Path::new(""),
        None,
        Duration::from_secs(2),
        Arc::new(real_check),
    );
    assert_eq!(empty.reason.as_deref(), Some("no archive target set"));
    // /Volumes/<name> must be a mount: a missing share is "not mounted".
    let share = check_with(
        Path::new("/Volumes/ds-no-such-share/Darkslide"),
        None,
        Duration::from_secs(2),
        Arc::new(real_check),
    );
    assert!(share.reason.unwrap().contains("not mounted"));
    let t2 = tempfile::tempdir().unwrap();
    let r = block_on(check_archive_target(
        t2.path().to_string_lossy().into_owned(),
        2000,
        CancelToken::new(),
    ))
    .unwrap();
    assert!(r.available);
    assert!(amazon_photos_app_installed().is_ok());
}

/// A hung mount (the check never returns) costs the caller only the timeout,
/// and later checks of the same path don't pile up threads.
#[test]
fn a_hung_target_check_returns_after_the_timeout() {
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (g, c) = (gate.clone(), calls.clone());
    let hang: Arc<CheckFn> = Arc::new(move |_p: &Path, _: Option<&VolumeId>| {
        c.fetch_add(1, Ordering::SeqCst);
        let (m, cv) = &*g;
        let mut open = m.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
        Ok(None)
    });
    let p = Path::new("/private/tmp/ds-hung-mount-test");
    let t0 = Instant::now();
    let r = check_with(p, None, Duration::from_millis(200), hang.clone());
    let took = t0.elapsed();
    assert!(r.timed_out && !r.available, "{r:?}");
    assert!(took < Duration::from_millis(600), "{took:?}");
    // Still stuck: answered at once, no second thread.
    let t1 = Instant::now();
    let again = check_with(p, None, Duration::from_secs(5), hang.clone());
    assert!(again.timed_out && t1.elapsed() < Duration::from_millis(100));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The mount answers: the stuck thread finishes, the next check works.
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    wait("stuck check to finish", 5, || {
        !lock(in_flight()).contains(p)
    });
    assert!(check_with(p, None, Duration::from_secs(2), hang).available);
}

// ------------------------------------------------------------ archive runs ---

fn rsyncs() -> Vec<PathBuf> {
    ds_archive::rsync::DEFAULT_RSYNC_PATHS
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

/// Real rsync (Homebrew rsync 3 and Apple's openrsync, whichever exist) into
/// a temp target: originals + sidecars + a DB snapshot land, the live DB
/// doesn't; a second run changes nothing.
#[test]
fn after_import_archives_with_every_installed_rsync() {
    let found = rsyncs();
    assert!(!found.is_empty(), "no rsync on this Mac");
    for rsync in found {
        let t = tempfile::tempdir().unwrap();
        let files = originals(t.path());
        let h = internal_session(t.path(), "2026-09-26 Trip", &files);
        let target = t.path().join("archive");
        fs::create_dir(&target).unwrap();
        let m = manager(
            config(&target, &t.path().join("locks")),
            Arc::new(real_check),
            Arc::new(SystemRunner),
            rsync.clone(),
        );
        assert!(m.register_session(h.clone()).unwrap());
        let t0 = Instant::now();
        m.after_import(h.clone(), files.len() as u64).unwrap();
        wait("archive run", 120, || !runs(&m).is_empty());
        let took = t0.elapsed();
        let run = runs(&m)[0].clone();
        assert_eq!(
            (run.outcome.as_str(), run.trigger.as_str()),
            ("succeeded", "on demand"),
            "{run:?}"
        );
        eprintln!(
            "ARC6 archive {} files with {}: {took:?} (run {} ms)",
            files.len(),
            rsync.display(),
            run.duration_ms
        );
        let dest = dest_of(&m, &target, "2026-09-26 Trip");
        assert!(
            dest.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("2026-09-26 Trip ("),
            "{dest:?}"
        );
        for f in &files {
            let name = f.file_name().unwrap();
            assert_eq!(
                fs::read(dest.join("Capture").join(name)).unwrap(),
                fs::read(f).unwrap()
            );
        }
        assert!(
            dest.join(ds_archive::SNAPSHOT_REL).is_file(),
            "DB snapshot archived"
        );
        let live_db = fs::read_dir(&dest)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".dsession"));
        assert!(!live_db, "the live index is never copied");
        let st = m.status().unwrap();
        assert!(st.target.available);
        assert!(
            st.rsync
                .as_deref()
                .unwrap()
                .contains(&rsync.to_string_lossy().into_owned())
        );
        let s = &st.sessions[0];
        assert!(s.last_success_ms.is_some() && s.pending_changes == 0 && !s.running);
        // Sync now: nothing new, still succeeds.
        m.sync_now().unwrap();
        wait("second run", 60, || runs(&m).len() >= 2);
        assert_eq!(runs(&m)[0].outcome, "succeeded");
        assert!(m.shutdown(5000).unwrap());
    }
}

#[test]
fn an_offline_target_queues_changes_and_status_stays_instant() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    // Offline = the volume isn't mounted (simulated through the check seam: a missing
    // folder on a mounted volume is created, not offline).
    let mounted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seam = Arc::clone(&mounted);
    let check: Arc<CheckFn> = Arc::new(move |p: &Path, v: Option<&VolumeId>| {
        if seam.load(std::sync::atomic::Ordering::SeqCst) {
            real_check(p, v)
        } else {
            Err(format!("{} is not mounted", p.display()))
        }
    });
    let m = manager(
        config(&t.path().join("not-mounted"), t.path()),
        check,
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.after_import(h.clone(), 3).unwrap();
    wait("target check", 10, || {
        m.status().unwrap().target.reason.as_deref() != Some("checking…")
    });
    let t0 = Instant::now();
    let st = m.status().unwrap();
    let took = t0.elapsed();
    eprintln!("ARC6 status() with an offline target: {took:?}");
    assert!(took < Duration::from_millis(20));
    assert!(!st.target.available && !st.target.timed_out);
    assert!(st.runs.is_empty(), "nothing runs against an offline target");
    assert_eq!(st.sessions[0].pending_changes, 3);
    // The volume appears (mounted): the queued archive runs, creating the folder.
    mounted.store(true, std::sync::atomic::Ordering::SeqCst);
    wait("archive after the target came back", 60, || {
        !runs(&m).is_empty()
    });
    assert_eq!(runs(&m)[0].outcome, "succeeded");
    assert!(m.shutdown(5000).unwrap());
}

/// The driver's check of a hung mount is bounded too: status, "Sync now" and
/// shutdown all answer while the mount never does.
#[test]
fn a_hung_mount_never_blocks_status_or_shutdown() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let release = Arc::new(AtomicBool::new(false));
    let r = release.clone();
    let hang: Arc<CheckFn> = Arc::new(move |_p: &Path, _: Option<&VolumeId>| {
        while !r.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("released".into())
    });
    let mut cfg = config(&t.path().join("hung-share"), t.path());
    cfg.check_timeout_ms = 150;
    let m = manager(
        cfg,
        hang,
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.after_import(h, 1).unwrap();
    wait("timed-out check", 10, || {
        m.status().unwrap().target.timed_out
    });
    let t0 = Instant::now();
    m.sync_now().unwrap();
    let st = m.status().unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(20),
        "{:?}",
        t0.elapsed()
    );
    assert!(st.runs.is_empty() && st.target.reason.unwrap().contains("respond"));
    let t1 = Instant::now();
    assert!(
        m.shutdown(3000).unwrap(),
        "driver stops while the mount hangs"
    );
    eprintln!("ARC6 shutdown with a hung mount: {:?}", t1.elapsed());
    release.store(true, Ordering::SeqCst);
}

/// An rsync that would run forever: quitting kills it; the run is "cancelled"
/// (not a failure) and the changes stay pending.
struct Endless {
    started: AtomicBool,
}

impl CommandRunner for Endless {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        if cmd.args.iter().any(|a| a == "--version") {
            on_line("rsync  version 3.4.1  protocol version 32");
            return Ok(CommandOutput {
                code: Some(0),
                stderr: String::new(),
            });
        }
        self.started.store(true, Ordering::SeqCst);
        on_line("  1,000  10%  1.00MB/s  0:00:09 (xfr#1, to-chk=5/8)");
        while !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(CommandOutput {
            code: None,
            stderr: String::new(),
        })
    }
}

#[test]
fn quitting_cancels_a_running_archive() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let fake = t.path().join("rsync");
    fs::write(&fake, b"").unwrap();
    let runner = Arc::new(Endless {
        started: AtomicBool::new(false),
    });
    let m = manager(
        config(&target, t.path()),
        Arc::new(real_check),
        runner.clone(),
        fake,
    );
    m.after_import(h, 2).unwrap();
    wait("rsync start", 10, || runner.started.load(Ordering::SeqCst));
    wait("progress", 10, || {
        m.status().unwrap().sessions[0].percent == Some(10)
    });
    let s = m.status().unwrap().sessions[0].clone();
    assert!(s.running && s.bytes_per_sec == Some(1e6) && s.eta_secs == Some(9));
    assert!(m.status().unwrap().rsync.unwrap().contains("rsync 3.4.1"));
    let t0 = Instant::now();
    assert!(m.shutdown(3000).unwrap());
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    let run = runs(&m)[0].clone();
    assert_eq!(run.outcome, "cancelled", "{run:?}");
    assert_eq!(
        m.status().unwrap().sessions[0].pending_changes,
        2,
        "still pending"
    );
    // A second shutdown is a no-op.
    assert!(m.shutdown(10).unwrap());
}

#[test]
fn failures_are_recorded_with_backoff_and_a_missing_rsync_is_reported() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let m = manager(
        config(&target, t.path()),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        t.path().join("no-rsync-here"),
    );
    m.after_import(h, 1).unwrap();
    wait("failed run", 20, || !runs(&m).is_empty());
    let st = m.status().unwrap();
    assert_eq!(st.runs[0].outcome, "failed");
    assert!(st.rsync_error.is_some() && st.rsync.is_none());
    let s = &st.sessions[0];
    assert!(s.consecutive_failures == 1 && s.retry_in_ms.is_some() && s.last_error.is_some());
    assert!(m.shutdown(3000).unwrap());
}

/// A folder opened in place on a card (store elsewhere) is never archived:
/// nothing may be written into it, not even the DB snapshot.
#[test]
fn a_session_on_a_card_is_not_archived() {
    let t = tempfile::tempdir().unwrap();
    let card = t.path().join("card");
    fs::create_dir(&card).unwrap();
    for f in originals(t.path()).iter().take(2) {
        fs::copy(f, card.join(f.file_name().unwrap())).unwrap();
    }
    let before = fs::read_dir(&card).unwrap().count();
    let probe = Probe {
        volume_ok: |_| Err("read-only card".into()),
        ..Probe::RAW
    };
    let fb = t.path().join("store");
    let h = Arc::new(
        open_in_place(&card, Some(&fb), None, &probe, &|| false, &mut |_, _, _| {}).unwrap(),
    );
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let m = manager(
        config(&target, t.path()),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    assert!(!m.register_session(h.clone()).unwrap());
    m.after_import(h.clone(), 5).unwrap();
    m.note_changes(h.clone(), 5).unwrap();
    m.sync_now().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let st = m.status().unwrap();
    assert_eq!(st.runs.len(), 0);
    assert!(
        st.sessions[0]
            .not_archivable
            .as_deref()
            .unwrap()
            .contains("card")
    );
    assert_eq!(st.sessions[0].pending_changes, 0);
    assert_eq!(
        fs::read_dir(&card).unwrap().count(),
        before,
        "card untouched"
    );
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    // Closing it just drops it.
    m.session_closed(h).unwrap();
    wait("closed session dropped", 10, || {
        m.status().unwrap().sessions.is_empty()
    });
    assert!(m.shutdown(3000).unwrap());
}

#[test]
fn session_close_archives_then_drops_and_config_keeps_pending() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("archive");
    let mut cfg = config(&target, t.path());
    cfg.enabled = false;
    cfg.debounce_secs = 3600;
    let m = manager(
        cfg.clone(),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.note_changes(h.clone(), 4).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(runs(&m).is_empty(), "disabled");
    // New settings: enabled, target created; the 4 pending changes carry over.
    fs::create_dir(&target).unwrap();
    cfg.enabled = true;
    m.set_config(cfg).unwrap();
    assert_eq!(m.status().unwrap().sessions[0].pending_changes, 4);
    std::thread::sleep(Duration::from_millis(300));
    assert!(runs(&m).is_empty(), "debounced");
    m.session_closed(h).unwrap();
    wait("close archive", 60, || !runs(&m).is_empty());
    let run = runs(&m)[0].clone();
    assert_eq!(
        (run.outcome.as_str(), run.trigger.as_str()),
        ("succeeded", "session closed")
    );
    wait("dropped", 10, || m.status().unwrap().sessions.is_empty());
    assert!(m.shutdown(3000).unwrap());
}

// ------------------------------------------------------------------ amazon ---

#[test]
fn amazon_mirror_places_each_original_once() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "Trip", &files);
    let m = manager(
        ArchiveConfig {
            enabled: false,
            ..config(Path::new(""), t.path())
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.register_session(h.clone()).unwrap();
    let folder = t.path().join("Amazon Photos");
    fs::create_dir(&folder).unwrap();
    let manifest = t.path().join("support/amazon-manifest.json");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    let run = || {
        block_on(m.clone().mirror_to_amazon(
            h.clone(),
            folder.to_string_lossy().into_owned(),
            manifest.to_string_lossy().into_owned(),
            "Trip".into(),
            CancelToken::new(),
        ))
        .unwrap()
    };
    let t0 = Instant::now();
    let first = run();
    eprintln!(
        "ARC6 amazon mirror {} files: {:?}",
        files.len(),
        t0.elapsed()
    );
    assert_eq!(first.placed, files.len() as u64, "{first:?}");
    assert_eq!(first.cloned, files.len() as u64, "same APFS volume: clones");
    assert!(first.failed.is_empty() && first.error.is_none() && !first.running);
    for f in &files {
        assert_eq!(
            fs::read(folder.join("Trip").join(f.file_name().unwrap())).unwrap(),
            fs::read(f).unwrap()
        );
    }
    let second = run();
    assert_eq!(
        (second.placed, second.already_mirrored),
        (0, files.len() as u64)
    );
    assert_eq!(m.status().unwrap().amazon, second);
    assert_eq!(
        m.status().unwrap().sessions[0].amazon.as_ref(),
        Some(&second)
    );
    // A folder that isn't there is an error in the status (never created).
    let blocker = t.path().join("file");
    fs::write(&blocker, b"x").unwrap();
    let err = block_on(m.clone().mirror_to_amazon(
        h,
        blocker.join("sub").to_string_lossy().into_owned(),
        manifest.to_string_lossy().into_owned(),
        "Trip".into(),
        CancelToken::new(),
    ));
    assert!(err.is_err());
    let a = m.status().unwrap().amazon;
    assert!(a.error.is_some() && !a.running && a.folder_missing, "{a:?}");
    assert!(m.shutdown(3000).unwrap());
}

/// Unhashed originals (an in-place folder before its background hashing)
/// wait for a later run instead of being handed over unverified.
#[test]
fn unhashed_originals_wait_for_a_later_mirror() {
    let samples = ds_testdata::samples_with_ext("ARW");
    if samples.len() < 2 {
        return; // needs real RAWs to register in place
    }
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("folder");
    fs::create_dir(&dir).unwrap();
    for f in samples.iter().take(2) {
        fs::copy(f, dir.join(f.file_name().unwrap())).unwrap();
    }
    let probe = Probe {
        volume_ok: |_| Ok(()),
        ..Probe::RAW
    };
    let h =
        Arc::new(open_in_place(&dir, None, None, &probe, &|| false, &mut |_, _, _| {}).unwrap());
    let m = manager(
        ArchiveConfig {
            enabled: false,
            ..config(Path::new(""), t.path())
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    fs::create_dir(t.path().join("amz")).unwrap();
    let r = block_on(m.clone().mirror_to_amazon(
        h,
        t.path().join("amz").to_string_lossy().into_owned(),
        t.path().join("m.json").to_string_lossy().into_owned(),
        "F".into(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!((r.placed, r.waiting), (0, 2), "{r:?}");
    assert!(m.shutdown(3000).unwrap());
}

/// A session reopened (a new handle for the same folder) keeps one schedule.
#[test]
fn a_reopened_session_keeps_one_entry() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h1 = internal_session(t.path(), "S", &files[..1]);
    let h2 = block_on(open_session(h1.root().unwrap(), CancelToken::new())).unwrap();
    assert!(!Arc::ptr_eq(&h1, &h2));
    let mut cfg = config(&t.path().join("archive"), t.path());
    cfg.enabled = false;
    let m = manager(
        cfg,
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.note_changes(h1, 2).unwrap();
    m.note_changes(h2.clone(), 3).unwrap();
    let st = m.status().unwrap();
    assert_eq!(st.sessions.len(), 1);
    assert_eq!(st.sessions[0].pending_changes, 5);
    assert!(m.register_session(h2).unwrap());
    assert_eq!(m.status().unwrap().sessions.len(), 1);
    assert!(m.shutdown(3000).unwrap());
}

// ------------------------------------------------ round A fixes (review) ---

fn fast() -> Timing {
    Timing {
        offline_base: Duration::from_millis(200),
        offline_max: Duration::from_millis(800),
        idle_check: Duration::from_millis(300),
        stall_probe: Duration::from_millis(300),
        stall_hard_progress: Duration::from_secs(1),
        stall_hard_quiet: Duration::from_secs(1),
        amazon_interval: Duration::from_secs(3600),
        amazon_shutdown_wait: Duration::from_millis(500),
    }
}

fn counting(check: Arc<CheckFn>) -> (Arc<CheckFn>, Arc<std::sync::atomic::AtomicUsize>) {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = n.clone();
    (
        Arc::new(move |p: &Path, v: Option<&VolumeId>| {
            n2.fetch_add(1, Ordering::SeqCst);
            check(p, v)
        }),
        n,
    )
}

/// A-H1: an original rsync can't read makes the run "incomplete", never
/// "succeeded": the file is listed with its reason, the error and the
/// pending changes stay (retried); once readable, a clean run clears it.
#[test]
fn an_unreadable_original_is_incomplete_not_archived() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    for rsync in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let files = originals(t.path());
        let h = internal_session(t.path(), "S", &files[..2]);
        let name = files[1].file_name().unwrap().to_owned();
        let bad = t.path().join("S/Capture").join(&name);
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        let target = t.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let m = manager(
            config(&target, t.path()),
            Arc::new(real_check),
            Arc::new(SystemRunner),
            rsync.clone(),
        );
        m.after_import(h.clone(), 2).unwrap();
        wait("run", 60, || !runs(&m).is_empty());
        let st = m.status().unwrap();
        let (run, s) = (&st.runs[0], &st.sessions[0]);
        assert_eq!(run.outcome, "incomplete", "{}: {run:?}", rsync.display());
        let want = format!("Capture/{}: ", name.to_string_lossy());
        assert!(
            run.problems.len() == 1 && run.problems[0].starts_with(&want),
            "{}: {:?}",
            rsync.display(),
            run.problems
        );
        assert!(run.problems[0].contains("Permission denied"));
        assert_eq!(s.problems, run.problems);
        assert!(s.last_error.as_deref().unwrap().contains("not archived"));
        assert!(s.pending_changes > 0 && s.retry_in_ms.is_some() && s.last_success_ms.is_none());
        let dest = dest_of(&m, &target, "S");
        assert!(!dest.join("Capture").join(&name).exists());
        assert!(
            dest.join("Capture")
                .join(files[0].file_name().unwrap())
                .exists()
        );
        // Fixed: the retry ("Sync now" skips the backoff) is clean.
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
        m.sync_now().unwrap();
        wait("clean run", 60, || runs(&m)[0].outcome == "succeeded");
        let s = m.status().unwrap().sessions[0].clone();
        assert!(s.problems.is_empty() && s.last_error.is_none() && s.pending_changes == 0);
        assert!(dest.join("Capture").join(&name).exists());
        assert!(m.shutdown(3000).unwrap());
    }
}

/// An rsync stuck in uninterruptible I/O (a mount gone stale after the
/// check): blocks forever and ignores cancel. Only runs touching
/// `hung_src` hang; everything else goes to the real rsync.
struct Stuck {
    hung_src: String,
    real: SystemRunner,
    entered: AtomicBool,
}

impl CommandRunner for Stuck {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        if cmd
            .args
            .iter()
            .any(|a| a.to_string_lossy().starts_with(&self.hung_src))
        {
            self.entered.store(true, Ordering::SeqCst);
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        self.real.run(cmd, on_line, cancel)
    }
}

/// A-H2: a run that stops responding is detached by the watchdog: it is
/// reported "not responding", the target is checked again, another session
/// still archives, and quitting is prompt.
#[test]
fn a_stuck_run_is_detached_and_other_sessions_keep_archiving() {
    let Some(rsync) = rsyncs()
        .into_iter()
        .find(|p| p.to_string_lossy().contains("homebrew"))
        .or_else(|| rsyncs().into_iter().next())
    else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let a = internal_session(t.path(), "A", &files[..1]);
    let b = internal_session(t.path(), "B", &files[..1]);
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let runner = Arc::new(Stuck {
        hung_src: t.path().join("A").to_string_lossy().into_owned(),
        real: SystemRunner,
        entered: AtomicBool::new(false),
    });
    let (check, checks) = counting(Arc::new(real_check));
    let m = manager_t(
        config(&target, t.path()),
        check,
        runner.clone(),
        rsync,
        fast(),
    );
    m.after_import(a, 1).unwrap();
    wait("stuck run", 20, || runner.entered.load(Ordering::SeqCst));
    let n0 = checks.load(Ordering::SeqCst);
    m.after_import(b, 1).unwrap();
    wait("not responding", 20, || {
        runs(&m).iter().any(|r| r.outcome == "not responding")
    });
    let st = m.status().unwrap();
    let sa = st.sessions.iter().find(|s| s.name == "A").unwrap();
    assert!(sa.not_responding && sa.running, "{sa:?}");
    let nr = runs(&m)
        .into_iter()
        .find(|r| r.outcome == "not responding")
        .unwrap();
    assert!(nr.message.unwrap().contains("will be retried"));
    // B archives although A's rsync never returns.
    wait("B archived", 60, || {
        runs(&m)
            .iter()
            .any(|r| r.session == "B" && r.outcome == "succeeded")
    });
    assert!(
        checks.load(Ordering::SeqCst) > n0,
        "the target is checked again"
    );
    let t0 = Instant::now();
    assert!(m.shutdown(2000).unwrap(), "nothing left to wait for");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
}

/// A-H2: a silent run whose target stops answering the (bounded) check is
/// detached with that reason, before the hard limit.
#[test]
fn a_silent_run_on_a_hung_target_is_detached_after_a_probe() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let a = internal_session(t.path(), "A", &files[..1]);
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let runner = Arc::new(Stuck {
        hung_src: t.path().join("A").to_string_lossy().into_owned(),
        real: SystemRunner,
        entered: AtomicBool::new(false),
    });
    // Healthy until the run starts, then every check hangs.
    let hang = Arc::new(AtomicBool::new(false));
    let h2 = hang.clone();
    let release = Arc::new(AtomicBool::new(false));
    let r2 = release.clone();
    let check: Arc<CheckFn> = Arc::new(move |p: &Path, v: Option<&VolumeId>| {
        while h2.load(Ordering::SeqCst) && !r2.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        real_check(p, v)
    });
    let mut cfg = config(&target, t.path());
    cfg.check_timeout_ms = 100;
    let timing = Timing {
        stall_hard_progress: Duration::from_secs(600),
        stall_hard_quiet: Duration::from_secs(600),
        ..fast()
    };
    let m = manager_t(
        cfg,
        check,
        runner.clone(),
        PathBuf::from("/usr/bin/rsync"),
        timing,
    );
    m.after_import(a, 1).unwrap();
    wait("stuck run", 20, || runner.entered.load(Ordering::SeqCst));
    hang.store(true, Ordering::SeqCst);
    wait("detached", 20, || {
        runs(&m).iter().any(|r| r.outcome == "not responding")
    });
    let r = runs(&m)
        .into_iter()
        .find(|r| r.outcome == "not responding")
        .unwrap();
    assert!(
        r.message.unwrap().contains("target is not responding"),
        "probe reason"
    );
    assert!(m.status().unwrap().target.timed_out);
    let t0 = Instant::now();
    assert!(m.shutdown(2000).unwrap());
    assert!(t0.elapsed() < Duration::from_secs(1));
    release.store(true, Ordering::SeqCst);
}

/// A-H3: two sessions with the same folder name archive into their own
/// folders (claimed with a marker, recorded in each store), never over each
/// other; a legacy `<name>` folder holding a session's files is adopted.
#[test]
fn sessions_with_the_same_name_get_their_own_archive_folders() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    fs::create_dir_all(t.path().join("p1")).unwrap();
    fs::create_dir_all(t.path().join("p2")).unwrap();
    let a = internal_session(&t.path().join("p1"), "Trip", &files[..1]);
    let alt = t.path().join("alt");
    fs::create_dir_all(&alt).unwrap();
    let same = alt.join(files[0].file_name().unwrap());
    fs::write(&same, b"DIFFERENT CONTENT").unwrap();
    let b = internal_session(&t.path().join("p2"), "Trip", std::slice::from_ref(&same));
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let m = manager(
        config(&target, t.path()),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        rsyncs()[0].clone(),
    );
    m.after_import(a.clone(), 1).unwrap();
    wait("a", 60, || {
        runs(&m).iter().any(|r| r.outcome == "succeeded")
    });
    m.after_import(b, 1).unwrap();
    wait("b", 60, || {
        runs(&m).iter().filter(|r| r.outcome == "succeeded").count() >= 2
    });
    let st = m.status().unwrap();
    let folders: Vec<String> = st
        .sessions
        .iter()
        .map(|s| s.archive_folder.clone().unwrap())
        .collect();
    assert_eq!(folders.len(), 2);
    assert_ne!(folders[0], folders[1]);
    let name = files[0].file_name().unwrap();
    let got: Vec<Vec<u8>> = folders
        .iter()
        .map(|f| fs::read(target.join(f).join("Capture").join(name)).unwrap())
        .collect();
    assert_eq!(got[0], fs::read(&files[0]).unwrap(), "A's original intact");
    assert_eq!(got[1], b"DIFFERENT CONTENT");
    assert!(!target.join("Trip/.ds-archive-trash").exists());
    // Recorded in each session's store; a relaunch keeps the folder.
    let stored: ArchiveIdentity = serde_json::from_slice(
        &fs::read(Path::new(&a.root().unwrap()).join(IDENTITY_REL)).unwrap(),
    )
    .unwrap();
    assert_eq!(stored.folder.as_deref(), Some(folders[0].as_str()));
    assert_eq!(
        fs::read_to_string(
            target
                .join(&folders[0])
                .join(ds_archive::ARCHIVE_OWNER_FILE)
        )
        .unwrap(),
        stored.id
    );
    assert!(m.shutdown(3000).unwrap());

    // Legacy: an archive made before per-session folders (`<target>/Trip`)
    // holding A's files is adopted; nothing there is removed.
    let legacy_target = t.path().join("legacy");
    let old = legacy_target.join("Trip");
    fs::create_dir_all(old.join("Capture")).unwrap();
    let src = Path::new(&a.root().unwrap()).join("Capture").join(name);
    fs::copy(&src, old.join("Capture").join(name)).unwrap();
    fs::File::options()
        .write(true)
        .open(old.join("Capture").join(name))
        .unwrap()
        .set_modified(fs::metadata(&src).unwrap().modified().unwrap())
        .unwrap();
    fs::write(old.join("keep.txt"), b"x").unwrap();
    // A fresh identity (as for a session archived by the previous version).
    fs::remove_file(Path::new(&a.root().unwrap()).join(IDENTITY_REL)).unwrap();
    let m = manager(
        config(&legacy_target, t.path()),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        rsyncs()[0].clone(),
    );
    m.after_import(a, 1).unwrap();
    wait("legacy run", 60, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m));
    assert_eq!(
        m.status().unwrap().sessions[0].archive_folder.as_deref(),
        Some("Trip"),
        "adopted"
    );
    assert!(old.join("keep.txt").exists());
    assert!(m.shutdown(3000).unwrap());
    // Identity files: a corrupt one is replaced; an unusable store is an error.
    let store = t.path().join("store");
    fs::create_dir_all(store.join(".ds")).unwrap();
    fs::write(store.join(IDENTITY_REL), b"{nope").unwrap();
    let id = load_identity(&store).unwrap();
    assert_eq!(id.id.len(), 16);
    assert_eq!(load_identity(&store).unwrap(), id, "stable");
    let file = t.path().join("plain");
    fs::write(&file, b"x").unwrap();
    assert!(load_identity(&file).is_err());
}

/// A-H4: a settings change keeps the failure state (last error, failures,
/// backoff); only target/schedule changes touch the schedulers at all.
#[test]
fn a_settings_change_keeps_the_failure_state() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let fake = t.path().join("fake-rsync");
    fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'rsync  version 3.4.1  protocol version 32'; exit 0; fi\n\
         echo 'rsync: [receiver] mkstemp failed: No space left on device (28)' >&2\nexit 11\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = config(&target, t.path());
    let m = manager(
        cfg.clone(),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        fake,
    );
    m.after_import(h, 1).unwrap();
    wait("failed run", 30, || {
        runs(&m).iter().any(|r| r.outcome == "failed")
    });
    let before = m.status().unwrap().sessions[0].clone();
    // N1c: in words; rsync's own text only as the detail.
    let human = before.last_error.clone().unwrap();
    assert!(
        human.starts_with("The archive disk is full (")
            && human.ends_with(" free) — 1 file not archived."),
        "{human}"
    );
    assert!(!human.contains("rsync"), "{human}");
    assert!(
        before
            .last_error_detail
            .as_deref()
            .unwrap()
            .contains("No space left")
    );
    let run = runs(&m)
        .into_iter()
        .find(|r| r.outcome == "failed")
        .unwrap();
    assert_eq!(run.message.as_deref(), Some(human.as_str()));
    assert!(run.detail.unwrap().contains("No space left"));
    for next in [
        ArchiveConfig {
            interval_secs: 1800,
            ..cfg.clone()
        },
        ArchiveConfig {
            check_timeout_ms: 3000,
            ..cfg.clone()
        },
        ArchiveConfig {
            target: t.path().join("target2").to_string_lossy().into_owned(),
            ..cfg.clone()
        },
    ] {
        m.set_config(next).unwrap();
        let after = m.status().unwrap().sessions[0].clone();
        assert_eq!(after.last_error, before.last_error);
        assert_eq!(after.consecutive_failures, before.consecutive_failures);
        assert_eq!(after.pending_changes, before.pending_changes);
        assert!(after.retry_in_ms.is_some());
    }
    assert!(m.shutdown(3000).unwrap());
}

/// A-M1: two hand-offs at the same time (two imports back to back) into
/// one manifest: none of the entries are lost (serialized). A-L5: each
/// session keeps its own hand-off status.
#[test]
fn concurrent_amazon_hand_offs_keep_every_manifest_entry() {
    let t = tempfile::tempdir().unwrap();
    let mut many = vec![];
    for i in 0..40u32 {
        let p = t.path().join(format!("F{i:03}.JPG"));
        fs::write(&p, format!("jpeg-{i}").repeat(2000)).unwrap();
        many.push(p);
    }
    let a = internal_session(t.path(), "A", &many[..20]);
    let b = internal_session(t.path(), "B", &many[20..]);
    let m = manager(
        ArchiveConfig {
            enabled: false,
            ..config(Path::new(""), t.path())
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.register_session(a.clone()).unwrap();
    m.register_session(b.clone()).unwrap();
    let folder = t.path().join("Amazon Photos");
    fs::create_dir(&folder).unwrap();
    let manifest = t.path().join("support/amazon-manifest.json");
    let (f, mf) = (
        folder.to_string_lossy().into_owned(),
        manifest.to_string_lossy().into_owned(),
    );
    let (m1, m2) = (m.clone(), m.clone());
    let (f1, mf1, f2, mf2) = (f.clone(), mf.clone(), f, mf);
    let (ha, hb) = (a.clone(), b.clone());
    let ja = std::thread::spawn(move || {
        block_on(m1.mirror_to_amazon(ha, f1, mf1, "A".into(), CancelToken::new())).unwrap()
    });
    let jb = std::thread::spawn(move || {
        block_on(m2.mirror_to_amazon(hb, f2, mf2, "B".into(), CancelToken::new())).unwrap()
    });
    let (ra, rb) = (ja.join().unwrap(), jb.join().unwrap());
    assert_eq!((ra.placed, rb.placed), (20, 20));
    let text = fs::read_to_string(&manifest).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let placed = json["entries"]
        .as_object()
        .unwrap()
        .values()
        .filter(|e| e["state"] == "placed")
        .count();
    assert_eq!(placed, 40, "{text}");
    // Per-session status (A-L5): each registered session keeps its own.
    let st = m.status().unwrap();
    let by: std::collections::HashMap<String, AmazonStatus> = st
        .sessions
        .iter()
        .map(|s| (s.name.clone(), s.amazon.clone().unwrap()))
        .collect();
    assert_eq!(by["A"].session.as_deref(), Some("A"));
    assert_eq!(by["B"].session.as_deref(), Some("B"));
    assert_eq!((by["A"].placed, by["B"].placed), (20, 20));
    assert!(m.shutdown(3000).unwrap());
}

/// A-M2 + A-M3 + A-L5: the chosen Amazon folder is never created (missing is
/// reported, per session); once it's there, the background hand-off catches
/// up by itself (no import needed), and "Sync now" runs it again.
#[test]
fn amazon_hand_off_catches_up_and_never_recreates_the_folder() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "Trip", &files[..2]);
    let m = manager_t(
        ArchiveConfig {
            enabled: false,
            ..config(Path::new(""), t.path())
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
        fast(),
    );
    assert!(m.register_session(h.clone()).unwrap());
    let folder = t.path().join("Amazon Photos");
    let manifest = t.path().join("support/m.json");
    m.set_amazon(
        Some(folder.to_string_lossy().into_owned()),
        manifest.to_string_lossy().into_owned(),
    )
    .unwrap();
    wait("missing folder reported", 20, || {
        m.status().unwrap().sessions[0]
            .amazon
            .as_ref()
            .is_some_and(|a| a.folder_missing && !a.running)
    });
    let a = m.status().unwrap().sessions[0].amazon.clone().unwrap();
    assert!(a.error.unwrap().contains("folder missing"));
    assert!(!folder.exists(), "never recreated");
    // The folder is back (the user reconnected the disk): "Sync now" hands off.
    fs::create_dir(&folder).unwrap();
    m.sync_now().unwrap();
    wait("handed over", 20, || {
        m.status().unwrap().sessions[0]
            .amazon
            .as_ref()
            .is_some_and(|a| a.placed == 2)
    });
    assert_eq!(fs::read_dir(folder.join("Trip")).unwrap().count(), 2);
    let overall = m.status().unwrap().amazon;
    assert_eq!(overall.session.as_deref(), Some("Trip"));
    // A second pass: nothing new.
    m.sync_now().unwrap();
    wait("second pass", 20, || {
        m.status().unwrap().sessions[0]
            .amazon
            .as_ref()
            .is_some_and(|a| a.already_mirrored == 2 && !a.running)
    });
    m.set_amazon(None, String::new()).unwrap();
    assert!(m.shutdown(3000).unwrap());
    // A cancelled hand-off stops between files (the token is bridged).
    let m = manager(
        ArchiveConfig {
            enabled: false,
            ..config(Path::new(""), t.path())
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    let h2 = internal_session(t.path(), "Other", &files[2..]);
    let r = m
        .inner
        .mirror_session(&entry_key(&h2), "Other", &h2, &folder, &manifest, &|| true)
        .unwrap();
    assert_eq!((r.placed, r.error.as_deref()), (0, Some("cancelled")));
    let token = CancelToken::new();
    token.cancel();
    assert!(
        block_on(m.clone().mirror_to_amazon(
            h2,
            folder.to_string_lossy().into_owned(),
            manifest.to_string_lossy().into_owned(),
            "Other".into(),
            token,
        ))
        .is_err()
    );
    assert!(m.shutdown(3000).unwrap());
}

/// A-M4 + A-L4: an offline target is re-checked with backoff whatever is
/// waiting (no 20 ms spin); a mount change wakes it at once; while idle
/// the target is checked rarely.
#[test]
fn an_offline_target_is_rechecked_with_backoff_not_in_a_loop() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let (check, checks) = counting(Arc::new(|_p: &Path, _: Option<&VolumeId>| {
        Err("not mounted".into())
    }));
    let m = manager(
        config(&t.path().join("offline"), t.path()),
        check,
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    wait("first check", 5, || checks.load(Ordering::SeqCst) >= 1);
    m.after_import(h.clone(), 1).unwrap();
    m.note_changes(h, 3).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let n = checks.load(Ordering::SeqCst);
    assert!(n <= 2, "{n} checks in 2 s while offline");
    // A volume was mounted: checked right away.
    m.volumes_changed().unwrap();
    wait("recheck", 2, || checks.load(Ordering::SeqCst) > n);
    assert!(m.shutdown(3000).unwrap());
    // Idle and online: a check every `idle_check`, not every second.
    let (check, checks) = counting(Arc::new(real_check));
    let target = t.path().join("online");
    fs::create_dir(&target).unwrap();
    let m = manager(
        config(&target, t.path()),
        check,
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    std::thread::sleep(Duration::from_secs(2));
    assert!(checks.load(Ordering::SeqCst) <= 1, "{checks:?}");
    assert!(m.shutdown(3000).unwrap());
}

/// A-L1: another run holding the target (another process) is "busy",
/// recorded once, retried after a backoff, then archives.
#[test]
fn a_busy_target_backs_off_and_is_recorded_once() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let lock_dir = t.path().join("locks");
    let held = TargetLock::try_acquire(&target, Some(&lock_dir)).unwrap();
    let m = manager(
        config(&target, &lock_dir),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        rsyncs()[0].clone(),
    );
    m.after_import(h, 1).unwrap();
    wait("busy", 10, || runs(&m).iter().any(|r| r.outcome == "busy"));
    m.sync_now().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let busy = runs(&m).iter().filter(|r| r.outcome == "busy").count();
    assert_eq!(busy, 1, "{:?}", runs(&m));
    drop(held);
    wait("archived after the backoff", 30, || {
        runs(&m)[0].outcome == "succeeded"
    });
    assert!(m.shutdown(3000).unwrap());
}

/// Final journeys 2 N2: rejected photos (and the session `Trash/`) are archived by
/// default (the archive is the backup) but not handed to Amazon Photos; each is a
/// setting, and rejects Amazon already has aren't counted as excluded.
#[test]
#[allow(clippy::too_many_lines)]
fn rejected_photos_are_archived_but_not_handed_to_amazon_by_default() {
    use crate::session::{PickFlag, SessionFolder};
    let Some(rsync) = rsyncs().into_iter().next() else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let files: Vec<PathBuf> = ["keep.ARW", "rej[1].ARW", "trashed.ARW"]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = t.path().join(n);
            fs::write(&p, vec![i as u8 + 1; 5000 + i]).unwrap();
            p
        })
        .collect();
    let h = internal_session(t.path(), "Cull", &files);
    let ids: Vec<(String, i64)> = block_on(h.clone().variants(CancelToken::new()))
        .unwrap()
        .into_iter()
        .map(|r| (r.file_name, r.variant_id))
        .collect();
    let id = |n: &str| ids.iter().find(|(f, _)| f == n).unwrap().1;
    let reject = vec![id("rej[1].ARW"), id("trashed.ARW")];
    block_on(
        h.clone()
            .set_pick(reject, PickFlag::Rejected, CancelToken::new()),
    )
    .unwrap();
    let moved = block_on(h.clone().move_to_folder(
        vec![id("trashed.ARW")],
        SessionFolder::Trash,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(moved.len(), 1);
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let cfg = ArchiveConfig {
        archive_rejected: false,
        ..config(&target, &t.path().join("locks"))
    };
    let m = manager(
        cfg.clone(),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        rsync,
    );
    m.register_session(h.clone()).unwrap();
    m.after_import(h.clone(), 3).unwrap();
    wait("archive run", 120, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m));
    let dest = dest_of(&m, &target, "Cull");
    assert!(dest.join("Capture/keep.ARW").is_file());
    assert!(
        !dest.join("Capture/rej[1].ARW").exists(),
        "a rejected photo is not archived"
    );
    assert!(
        !dest.join("Trash").exists(),
        "the session Trash is not archived"
    );
    // N2: the status says so.
    let s = m.status().unwrap().sessions[0].clone();
    assert_eq!((s.originals, s.rejects_excluded), (3, 2), "{s:?}");
    // Amazon: only the kept photo.
    let folder = t.path().join("Amazon Photos");
    fs::create_dir(&folder).unwrap();
    let manifest = t.path().join("support/amazon-manifest.json");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    let mirror = || {
        block_on(m.clone().mirror_to_amazon(
            h.clone(),
            folder.to_string_lossy().into_owned(),
            manifest.to_string_lossy().into_owned(),
            "Cull".into(),
            CancelToken::new(),
        ))
        .unwrap()
    };
    let first = mirror();
    assert_eq!((first.placed, first.rejects_excluded), (1, 2));
    assert!(folder.join("Cull/keep.ARW").is_file());
    assert!(!folder.join("Cull/rej[1].ARW").exists());
    // The defaults: rejects are archived (the archive is the backup)…
    m.set_config(config(&target, &t.path().join("locks")))
        .unwrap();
    m.sync_now().unwrap();
    wait("second run", 120, || {
        runs(&m).len() >= 2 && dest.join("Capture/rej[1].ARW").is_file()
    });
    assert!(dest.join("Trash/trashed.ARW").is_file());
    wait("counts", 10, || {
        m.status().unwrap().sessions[0].rejects_excluded == 0
    });
    assert_eq!(m.status().unwrap().sessions[0].originals, 3);
    // …but not handed to Amazon unless that setting is on.
    assert_eq!(mirror().placed, 0);
    m.set_config(ArchiveConfig {
        amazon_rejected: true,
        ..config(&target, &t.path().join("locks"))
    })
    .unwrap();
    let all = mirror();
    assert_eq!((all.placed, all.rejects_excluded), (2, 0));
    // Rejects the app already has aren't "excluded" (nothing is ever removed there).
    m.set_config(config(&target, &t.path().join("locks")))
        .unwrap();
    assert_eq!(mirror().rejects_excluded, 0);
    assert!(m.shutdown(5000).unwrap());
}

/// A fake rsync 3 that always fails with `stderr` (exit 11).
fn failing_rsync(dir: &Path, stderr: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake = dir.join("fake-rsync");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'rsync  version 3.4.1  protocol version 32'; exit 0; fi\n\
             echo '{stderr}' >&2\nexit 11\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

fn offline() -> Arc<CheckFn> {
    Arc::new(|p: &Path, _: Option<&VolumeId>| Err(format!("{} is not mounted", p.display())))
}

/// final-journeys2 N1: a failed run, then a relaunch (a new manager) with the
/// target offline: the session still says the last run failed and what is
/// waiting; a later success is what the next launch shows.
#[test]
fn the_last_result_survives_a_relaunch() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..2]);
    let target = t.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let cfg = config(&target, t.path());
    let fake = failing_rsync(
        t.path(),
        "rsync: [receiver] write failed on \"/x/S.ARW\": No space left on device (28)",
    );
    let m = manager(
        cfg.clone(),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        fake,
    );
    m.after_import(h.clone(), 2).unwrap();
    wait("failed run", 30, || {
        runs(&m).iter().any(|r| r.outcome == "failed")
    });
    m.note_changes(h.clone(), 3).unwrap();
    assert!(m.shutdown(3000).unwrap());
    drop(m);

    // Relaunch, target offline: nothing runs, but nothing claims "archived".
    let m = manager(
        cfg.clone(),
        offline(),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.register_session(h.clone()).unwrap();
    let s = m.status().unwrap().sessions[0].clone();
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("The archive disk is full"),
        "{s:?}"
    );
    assert!(
        s.last_error_detail
            .as_deref()
            .unwrap()
            .contains("No space left")
    );
    assert_eq!(s.last_success_ms, None);
    assert_eq!(
        s.pending_changes, 5,
        "2 imported + 3 changed, none archived"
    );
    assert!(m.shutdown(3000).unwrap());
    drop(m);

    // Relaunch with the target back: the catch-up run goes at once (no
    // debounce, no interval) and its success is saved.
    let Some(rsync) = rsyncs().into_iter().next() else {
        return;
    };
    let m = manager(
        ArchiveConfig {
            debounce_secs: 3600,
            ..cfg.clone()
        },
        Arc::new(real_check),
        Arc::new(SystemRunner),
        rsync.clone(),
    );
    m.register_session(h.clone()).unwrap();
    wait("catch-up run", 60, || {
        runs(&m).iter().any(|r| r.outcome == "succeeded")
    });
    let s = m.status().unwrap().sessions[0].clone();
    assert!(s.last_error.is_none() && s.last_error_detail.is_none() && s.pending_changes == 0);
    assert_eq!(s.originals, 2);
    let archived_at = s.last_success_ms.unwrap();
    assert!(m.shutdown(3000).unwrap());
    drop(m);

    let m = manager(cfg.clone(), offline(), Arc::new(SystemRunner), rsync);
    m.register_session(h.clone()).unwrap();
    let s = m.status().unwrap().sessions[0].clone();
    assert_eq!(s.last_success_ms, Some(archived_at));
    assert!(s.last_error.is_none() && s.pending_changes == 0);
    assert!(m.shutdown(3000).unwrap());
    drop(m);
    // Another target knows nothing of this one.
    let store = PathBuf::from(h.root().unwrap());
    let other = SavedState::load(&store, &t.path().join("other").to_string_lossy());
    assert_eq!(other.last_success_ms, None);
    assert!(!other.clean());
    assert!(SavedState::load(&store, &cfg.target).clean());
}

#[test]
fn errors_are_put_in_words() {
    let rsync = |stderr: &str| ArchiveError::Rsync {
        code: Some(11),
        stderr: stderr.into(),
    };
    let io = |kind: io::ErrorKind| ArchiveError::Io {
        path: "/x".into(),
        source: io::Error::from(kind),
    };
    let none = || None;
    assert_eq!(
        human_error(
            &rsync("write failed: No space left on device (28)"),
            Some(16),
            || Some(1_234_000_000)
        ),
        "The archive disk is full (1.2 GB free) — 16 files not archived."
    );
    assert_eq!(
        human_error(&rsync("No space left on device"), None, none),
        "The archive disk is full."
    );
    assert_eq!(
        human_error(&rsync("No space left on device"), Some(0), none),
        "The archive disk is full.",
        "nothing verifiably missing: no number"
    );
    assert_eq!(
        human_error(&rsync("mkstemp: Permission denied (13)"), Some(1), none),
        "Darkslide isn't allowed to write to the archive folder — 1 file not archived."
    );
    assert_eq!(
        human_error(&io(io::ErrorKind::PermissionDenied), None, none),
        "Darkslide isn't allowed to write to the archive folder."
    );
    assert_eq!(
        human_error(
            &rsync("rsync: connection unexpectedly closed"),
            Some(2),
            none
        ),
        "The connection to the archive was lost — 2 files not archived."
    );
    assert_eq!(
        human_error(&rsync("Read-only file system (30)"), None, none),
        "The archive disk is read-only."
    );
    assert_eq!(
        human_error(&rsync("something odd"), None, none),
        "The archive copy failed."
    );
    assert_eq!(
        human_error(
            &ArchiveError::TargetUnavailable {
                root: "/Volumes/pics".into(),
                reason: "not mounted".into()
            },
            Some(4),
            none
        ),
        "The archive disk is not mounted."
    );
    assert_eq!(
        human_error(&ArchiveError::RsyncNotFound, None, none),
        "No rsync was found on this Mac."
    );
    assert_eq!(
        human_error(
            &ArchiveError::Incomplete {
                problems: vec!["a".into(), "b".into()]
            },
            Some(1),
            none
        ),
        "2 files not archived."
    );
    assert_eq!(
        human_error(&ArchiveError::Cancelled, None, none),
        "Cancelled."
    );
    assert_eq!(
        human_error(
            &ArchiveError::TargetNotUsable {
                root: "/t".into(),
                reason: "it is a file".into()
            },
            None,
            none
        ),
        "The archive folder can't be used: it is a file."
    );
    assert_eq!(human_bytes(512_000), "512 KB");
    assert_eq!(human_bytes(850_000_000), "850 MB");
    assert_eq!(human_bytes(2_500_000_000_000), "2.5 TB");
    assert!(free_space(Path::new("/")).is_some_and(|b| b > 0));
    assert_eq!(free_space(Path::new("/no/such/dir/here")), None);
}

/// N1c: the "not archived" number is what the target verifiably lacks.
#[test]
fn missing_originals_are_counted_on_the_target() {
    let t = tempfile::tempdir().unwrap();
    let (src, dest) = (t.path().join("S"), t.path().join("T"));
    for d in [&src, &dest] {
        fs::create_dir_all(d.join("Capture")).unwrap();
    }
    let originals: Vec<PathBuf> = ["a.ARW", "b.ARW", "c.ARW", "d.ARW"]
        .iter()
        .map(|n| {
            let p = src.join("Capture").join(n);
            fs::write(&p, [1u8; 100]).unwrap();
            p
        })
        .collect();
    fs::write(dest.join("Capture/a.ARW"), [1u8; 100]).unwrap(); // there
    fs::write(dest.join("Capture/b.ARW"), [1u8; 40]).unwrap(); // cut short
    // c and d: absent. Outside the source: not counted.
    let mut all = originals.clone();
    all.push(t.path().join("elsewhere.ARW"));
    assert_eq!(missing_on_target(&src, &dest, &all), 3);
    assert_eq!(missing_on_target(&src, &dest, &originals[..1]), 0);
}

/// App review M8: Move to Selects after an archive run. The archive mirrors the
/// session's current layout: the pick ends up only under `Selects/` on the target
/// (moved there, not re-copied next to the old one), and nothing is deleted.
#[test]
fn move_to_selects_after_an_archive_run_leaves_one_copy() {
    use crate::session::SessionFolder;
    let Some(rsync) = rsyncs().into_iter().next() else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let files: Vec<PathBuf> = ["pick.ARW", "other.ARW"]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = t.path().join(n);
            fs::write(&p, vec![i as u8 + 7; 4000 + i]).unwrap();
            p
        })
        .collect();
    let h = internal_session(t.path(), "Move", &files);
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let cfg = config(&target, &t.path().join("locks"));
    let m = manager(cfg, Arc::new(real_check), Arc::new(SystemRunner), rsync);
    m.register_session(h.clone()).unwrap();
    m.after_import(h.clone(), 2).unwrap();
    wait("archive run", 120, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m));
    let dest = dest_of(&m, &target, "Move");
    assert!(dest.join("Capture/pick.ARW").is_file());
    let ids: Vec<(String, i64)> = block_on(h.clone().variants(CancelToken::new()))
        .unwrap()
        .into_iter()
        .map(|r| (r.file_name, r.variant_id))
        .collect();
    let pick = ids.iter().find(|(f, _)| f == "pick.ARW").unwrap().1;
    let moved = block_on(h.clone().move_to_folder(
        vec![pick],
        SessionFolder::Selects,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(moved.len(), 1);
    m.sync_now().unwrap();
    wait("second run", 120, || {
        runs(&m).len() >= 2 && dest.join("Selects/pick.ARW").is_file()
    });
    assert!(
        runs(&m).iter().all(|r| r.outcome == "succeeded"),
        "{:?}",
        runs(&m)
    );
    assert!(
        !dest.join("Capture/pick.ARW").exists(),
        "no second copy under Capture"
    );
    assert!(dest.join("Capture/other.ARW").is_file());
    assert_eq!(
        fs::read(dest.join("Selects/pick.ARW")).unwrap(),
        fs::read(&files[0]).unwrap()
    );
    assert!(m.shutdown(5000).unwrap());
}
