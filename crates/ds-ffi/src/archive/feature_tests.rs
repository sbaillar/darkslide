//! Settings ▸ Plugins gates the archive driver: no rsync run, Amazon hand-off or Immich
//! step starts while its feature is off; a run already going finishes cleanly; turning
//! the feature back on picks up what was waiting.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ds_archive::{CommandOutput, CommandRunner, CommandSpec, SystemClock, SystemRunner};
use ds_core::FileHash;
use ds_session::{ImageMetaLite, Session};
use futures_executor::block_on;

use super::*;
use crate::jobs::CancelToken;
use crate::plugins::PluginHost;
use crate::session::open_session;

fn builtin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/builtin")
}

fn session(parent: &Path, name: &str) -> Arc<SessionHandle> {
    let root = parent.join(name);
    let mut s = Session::create(&root, name).unwrap();
    let rel = "Capture/A.ARW".to_owned();
    fs::write(root.join(&rel), vec![7u8; 4096]).unwrap();
    let hash = FileHash(*blake3::hash(&fs::read(root.join(&rel)).unwrap()).as_bytes());
    s.add_originals(vec![(rel, hash, ImageMetaLite::default())])
        .unwrap();
    drop(s);
    block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap()
}

fn wait(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Nothing happens for a while (the driver had many chances to start something).
fn stays(secs: f64, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        assert!(cond(), "changed while its feature was off");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn fast() -> Timing {
    Timing {
        offline_base: Duration::from_millis(200),
        offline_max: Duration::from_millis(800),
        idle_check: Duration::from_millis(200),
        stall_probe: Duration::from_secs(30),
        stall_hard_progress: Duration::from_secs(60),
        stall_hard_quiet: Duration::from_secs(60),
        amazon_interval: Duration::from_millis(300),
        amazon_shutdown_wait: Duration::from_millis(500),
    }
}

/// Holds each rsync transfer until released, then runs the real rsync.
struct Held {
    entered: AtomicBool,
    release: AtomicBool,
}

impl CommandRunner for Held {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        if !cmd.args.iter().any(|a| a == "--version") {
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        SystemRunner.run(cmd, on_line, cancel)
    }
}

fn rsync() -> Option<PathBuf> {
    ds_archive::rsync::DEFAULT_RSYNC_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn manager(t: &Path, enabled: bool, runner: Arc<dyn CommandRunner>) -> Arc<ArchiveManager> {
    let target = t.join("archive");
    fs::create_dir_all(&target).unwrap();
    let cfg = ArchiveConfig {
        enabled,
        target: target.to_string_lossy().into_owned(),
        interval_secs: 3600,
        debounce_secs: 0,
        lock_dir: Some(t.join("locks").to_string_lossy().into_owned()),
        check_timeout_ms: 2000,
        rsync_path: None,
        archive_rejected: true,
        amazon_rejected: false,
        target_volume: archive_volume_identity(target.to_string_lossy().into_owned()),
        object_store: None,
    };
    ArchiveManager::with_timing(
        cfg,
        Arc::new(real_check),
        runner,
        vec![rsync().unwrap()],
        Arc::new(SystemClock::default()),
        fast(),
    )
    .unwrap()
}

fn runs(m: &ArchiveManager) -> Vec<ArchiveRun> {
    m.status().unwrap().runs
}

#[test]
fn the_archive_waits_while_turned_off_and_a_run_in_flight_finishes() {
    if rsync().is_none() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let plugins = PluginHost::unshared(Some(builtin()), t.path().join("Plugins"));
    plugins
        .set_enabled("darkslide.archive".into(), true)
        .unwrap();
    let runner = Arc::new(Held {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
    });
    let m = manager(t.path(), true, runner.clone());
    m.set_plugin_host(Some(plugins.clone())).unwrap();
    let h = session(t.path(), "Trip");
    m.after_import(h.clone(), 1).unwrap();
    wait("the run to start", 20, || {
        runner.entered.load(Ordering::SeqCst)
    });
    // Turned off with a run in flight: it isn't cut off; it finishes and is recorded.
    plugins
        .set_enabled("darkslide.archive".into(), false)
        .unwrap();
    runner.release.store(true, Ordering::SeqCst);
    wait("the run to finish", 60, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m)[0]);
    // While off: changes are kept, nothing runs, not even "Sync now".
    m.note_changes(h.clone(), 3).unwrap();
    m.sync_now().unwrap();
    stays(1.5, || runs(&m).len() == 1);
    assert_eq!(m.status().unwrap().sessions[0].pending_changes, 3);
    // Back on: what waited is archived, without a relaunch.
    plugins
        .set_enabled("darkslide.archive".into(), true)
        .unwrap();
    m.sync_now().unwrap();
    wait("the catch-up run", 60, || runs(&m).len() >= 2);
    assert_eq!(runs(&m)[0].outcome, "succeeded");
    assert!(m.shutdown(3000).unwrap());
}

#[test]
fn the_amazon_hand_off_waits_while_turned_off() {
    if rsync().is_none() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let plugins = PluginHost::unshared(Some(builtin()), t.path().join("Plugins"));
    let m = manager(t.path(), false, Arc::new(SystemRunner));
    m.set_plugin_host(Some(plugins.clone())).unwrap();
    let h = session(t.path(), "Trip");
    assert!(m.register_session(h.clone()).unwrap());
    let folder = t.path().join("Amazon Photos");
    fs::create_dir(&folder).unwrap();
    let manifest = t.path().join("support/m.json");
    m.set_amazon(
        Some(folder.to_string_lossy().into_owned()),
        manifest.to_string_lossy().into_owned(),
    )
    .unwrap();
    m.sync_now().unwrap();
    stays(1.5, || !folder.join("Trip").exists());
    // An explicit mirror is refused too, and says where to turn it on.
    let e = block_on(m.clone().mirror_to_amazon(
        h.clone(),
        folder.to_string_lossy().into_owned(),
        manifest.to_string_lossy().into_owned(),
        "Trip".into(),
        CancelToken::new(),
    ))
    .unwrap_err();
    assert!(e.to_string().contains("Settings ▸ Plugins"), "{e}");
    // The folder setting was kept: turning it on hands off at once.
    plugins
        .set_enabled("darkslide.amazon-photos".into(), true)
        .unwrap();
    m.sync_now().unwrap();
    wait("handed over", 20, || {
        folder
            .join("Trip")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some())
    });
    assert!(m.shutdown(3000).unwrap());
}

#[test]
fn immich_steps_wait_while_turned_off() {
    let t = tempfile::tempdir().unwrap();
    let plugins = PluginHost::unshared(Some(builtin()), t.path().join("Plugins"));
    let m = manager(t.path(), false, Arc::new(SystemRunner));
    m.set_plugin_host(Some(plugins.clone())).unwrap();
    m.set_immich_publish(Some(crate::immich::ArchivePublish {
        xmp_sidecars: true,
        label_tags: false,
    }));
    let h = session(t.path(), "Trip");
    // A run that archived the session queues no album sync while Immich is off…
    m.inner.queue_album_sync(&h);
    assert_eq!(m.immich_queued(), 0);
    // …and does once it is on.
    plugins
        .set_enabled("darkslide.immich".into(), true)
        .unwrap();
    m.inner.queue_album_sync(&h);
    assert_eq!(m.immich_queued(), 1);
    assert!(m.shutdown(3000).unwrap());
}
