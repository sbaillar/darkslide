//! repeated archive runs with no source change must not re-send
//! files (which moves the previous archived copy into `.ds-archive-trash`
//! every run), on fine- and coarse-timestamp targets, with both rsyncs.
//! Before the fix (`--modify-window=-1` on rsync 3) a 2 s-granularity target
//! collected 12 trash files after 3 idle runs (rsync 3), 4 with openrsync.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use ds_archive::rsync::{RsyncRunner, SystemRunner};
use ds_archive::target::fs_type;
use ds_archive::*;
use ds_fsutil::testvol::{self, Fs};

const RSYNCS: [&str; 2] = ["/opt/homebrew/bin/rsync", "/usr/bin/rsync"];

fn rsync(path: &str) -> Option<RsyncRunner> {
    Path::new(path)
        .exists()
        .then(|| RsyncRunner::detect_from(Arc::new(SystemRunner), &[PathBuf::from(path)]).unwrap())
}

/// A session whose files have odd-second, sub-second mtimes (worst case for
/// coarse targets).
fn session(t: &Path) -> PathBuf {
    let s = t.join("Sess");
    fs::create_dir_all(s.join("Capture/.ds/settings")).unwrap();
    for i in 0..3u64 {
        let f = s.join(format!("Capture/B{i}.ARW"));
        fs::write(&f, vec![i as u8; 1000]).unwrap();
        set_mtime(
            &f,
            SystemTime::UNIX_EPOCH + Duration::new(1_780_000_001 + i, 123_456_789),
        );
        let j = s.join(format!("Capture/.ds/settings/B{i}.ARW.json"));
        fs::write(&j, b"{\"e\":0}").unwrap();
        set_mtime(
            &j,
            SystemTime::UNIX_EPOCH + Duration::new(1_780_000_101 + i, 987_654_321),
        );
    }
    s
}

fn set_mtime(p: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Round every archived mtime down to `g`, as a coarse file system stores it.
fn truncate_mtimes(root: &Path, g: Duration) {
    for e in fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        let n = e.file_name();
        if n == ".ds-archive-trash" || n == ".ds-rsync-partial" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            truncate_mtimes(&p, g);
        } else {
            let d = fs::metadata(&p)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let q = d.as_nanos() / g.as_nanos() * g.as_nanos();
            set_mtime(&p, SystemTime::UNIX_EPOCH + Duration::from_nanos(q as u64));
        }
    }
}

fn files_in(root: &Path) -> usize {
    let mut n = 0;
    let mut st = vec![root.to_path_buf()];
    while let Some(d) = st.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().unwrap().is_dir() {
                st.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Archive `runs` times with no source change; returns trash file count.
fn idle_runs(
    r: &RsyncRunner,
    s: &Path,
    target: &ArchiveTarget,
    runs: u64,
    coarsen: Option<Duration>,
) -> usize {
    let a = Archiver::new(r.clone(), None);
    for k in 0..runs {
        a.archive(
            s,
            target,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000 + k),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        if let Some(g) = coarsen {
            truncate_mtimes(&target.root.join("Sess"), g);
        }
    }
    files_in(&target.root.join("Sess/.ds-archive-trash"))
}

/// After the idle runs, a same-second, same-size sidecar edit still lands.
fn sidecar_edit_lands(r: &RsyncRunner, s: &Path, target: &ArchiveTarget) {
    let j = s.join("Capture/.ds/settings/B0.ARW.json");
    let m = fs::metadata(&j).unwrap().modified().unwrap();
    fs::write(&j, b"{\"e\":9}").unwrap();
    set_mtime(&j, m);
    Archiver::new(r.clone(), None)
        .archive(
            s,
            target,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        fs::read(target.root.join("Sess/Capture/.ds/settings/B0.ARW.json")).unwrap(),
        b"{\"e\":9}",
        "{:?}",
        r.flavor()
    );
}

#[test]
fn idle_runs_do_not_churn_on_apfs() {
    for r in RSYNCS.iter().filter_map(|p| rsync(p)) {
        let t = tempfile::tempdir().unwrap();
        let s = session(t.path());
        let root = t.path().join("archive");
        fs::create_dir(&root).unwrap();
        let target = ArchiveTarget::new(&root);
        assert_eq!(target.modify_window(), 0);
        assert_eq!(idle_runs(&r, &s, &target, 3, None), 0, "{:?}", r.flavor());
        sidecar_edit_lands(&r, &s, &target);
    }
}

/// Simulated coarse targets on a plain directory (runs everywhere):
/// exFAT-like 10 ms needs no window; FAT-like 2 s needs the 1 s window that
/// `mtime_window_for` picks for `msdos` (forced here, since the temp dir is APFS).
#[test]
fn idle_runs_do_not_churn_on_simulated_coarse_targets() {
    for r in RSYNCS.iter().filter_map(|p| rsync(p)) {
        for (g, window) in [
            (Duration::from_millis(10), None),
            (Duration::from_secs(2), Some(1)),
        ] {
            let t = tempfile::tempdir().unwrap();
            let s = session(t.path());
            let root = t.path().join("archive");
            fs::create_dir(&root).unwrap();
            let mut target = ArchiveTarget::new(&root);
            target.modify_window = window;
            assert_eq!(
                idle_runs(&r, &s, &target, 3, Some(g)),
                0,
                "{:?} at {g:?}",
                r.flavor()
            );
            sidecar_edit_lands(&r, &s, &target);
        }
    }
}

/// Real FAT32 and exFAT targets (disk images; SKIP if unavailable). The
/// window is detected from the file system, not forced.
#[test]
fn idle_runs_do_not_churn_on_real_fat32_and_exfat() {
    for (fs_kind, name, want_type, want_window) in [
        (Fs::Fat32, "ARCHF", "msdos", 1),
        (Fs::ExFat, "ARCHX", "exfat", 0),
    ] {
        let t = tempfile::tempdir().unwrap();
        let Some(img) = testvol::require(t.path(), fs_kind, 64, name) else {
            continue;
        };
        assert_eq!(fs_type(img.mount_point()).as_deref(), Some(want_type));
        for r in RSYNCS.iter().filter_map(|p| rsync(p)) {
            let s = session(
                &t.path()
                    .join(format!("src-{}", r.flavor().supports_progress2())),
            );
            let root = img
                .mount_point()
                .join(format!("A-{}", r.flavor().supports_progress2()));
            fs::create_dir(&root).unwrap();
            let target = ArchiveTarget::new(&root);
            assert_eq!(target.modify_window(), want_window);
            assert_eq!(
                idle_runs(&r, &s, &target, 3, None),
                0,
                "{fs_kind:?} {:?}",
                r.flavor()
            );
            sidecar_edit_lands(&r, &s, &target);
        }
    }
}
