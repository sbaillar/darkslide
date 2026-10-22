//! Review regressions against **both** real rsyncs: Homebrew rsync 3.x and
//! Apple's openrsync. Each test runs once per installed binary.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ds_archive::rsync::{
    CommandOutput, CommandRunner, CommandSpec, PARTIAL_DIR, RsyncFlavor, RsyncRunner, SystemRunner,
};
use ds_archive::*;

fn rsyncs() -> Vec<RsyncRunner> {
    ["/opt/homebrew/bin/rsync", "/usr/bin/rsync"]
        .iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| RsyncRunner::detect_from(Arc::new(SystemRunner), &[PathBuf::from(p)]).unwrap())
        .collect()
}

struct Snap(bool);
impl Snapshotter for Snap {
    fn snapshot(&self, _: &Path, dest: &Path) -> Result<(), String> {
        if self.0 {
            Err("database is locked".into())
        } else {
            fs::write(dest, b"fresh snapshot").map_err(|e| e.to_string())
        }
    }
}

fn setup(t: &Path) -> (PathBuf, ArchiveTarget) {
    let s = t.join("Sess");
    fs::create_dir_all(s.join("Capture/.ds/settings")).unwrap();
    fs::write(s.join("Capture/A.ARW"), b"raw").unwrap();
    fs::write(
        s.join("Capture/.ds/settings/A.ARW.json"),
        b"{\"exposure\":1}",
    )
    .unwrap();
    for live in [
        "Sess.dsession",
        "Sess.dsession-wal",
        "Sess.dsession-shm",
        "Sess.dsession-journal",
    ] {
        fs::write(s.join(live), b"live").unwrap();
    }
    fs::write(s.join(".ds-1a-2.tmp"), b"temp").unwrap();
    let root = t.join("archive");
    fs::create_dir(&root).unwrap();
    (s, ArchiveTarget::new(&root))
}

fn when(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn excludes_live_db_and_temp_files_on_both_rsyncs() {
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let a = Archiver::new(r.clone(), Some(Arc::new(Snap(false))));
        let out = a
            .archive(
                &s,
                &target,
                when(1_790_000_000),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        let d = &out.dest;
        assert!(d.join("Capture/A.ARW").exists(), "{:?}", r.flavor());
        assert_eq!(fs::read(d.join(SNAPSHOT_REL)).unwrap(), b"fresh snapshot");
        for live in [
            "Sess.dsession",
            "Sess.dsession-wal",
            "Sess.dsession-shm",
            "Sess.dsession-journal",
            ".ds-1a-2.tmp",
        ] {
            assert!(
                !d.join(live).exists(),
                "{live} archived by {:?}",
                r.flavor()
            );
        }
        assert!(
            !d.join(&out.backup_dir).exists(),
            "empty backup dir removed"
        );
    }
}

/// a sidecar edited twice within one second, same size, must still be
/// archived (rsync 3: `--modify-window=-1`; openrsync: checksum pass).
#[test]
fn h3_same_second_same_size_sidecar_edit_is_archived() {
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let side = s.join("Capture/.ds/settings/A.ARW.json");
        let sec = 1_790_000_000;
        let set = |body: &[u8], nanos: u32| {
            fs::write(&side, body).unwrap();
            fs::File::options()
                .write(true)
                .open(&side)
                .unwrap()
                .set_modified(when(sec) + Duration::from_nanos(u64::from(nanos)))
                .unwrap();
        };
        set(b"{\"exposure\":1}", 100);
        let a = Archiver::new(r.clone(), None);
        let out = a
            .archive(&s, &target, when(sec), &mut |_| {}, &AtomicBool::new(false))
            .unwrap();
        set(b"{\"exposure\":2}", 900_000_000);
        a.archive(
            &s,
            &target,
            when(sec + 1),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            fs::read(out.dest.join("Capture/.ds/settings/A.ARW.json")).unwrap(),
            b"{\"exposure\":2}",
            "{:?}",
            r.flavor()
        );
    }
}

/// Wraps the real runner and slows the transfer (`--bwlimit`) so a cancel
/// reliably lands mid-file. openrsync ignores `--bwlimit`, so for it the
/// test uses a much larger file and an early cancel instead.
struct Slow;
impl CommandRunner for Slow {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        let mut c = cmd.clone();
        if c.args != ["--version"] {
            c.args.insert(0, "--bwlimit=500".into());
        }
        SystemRunner.run(&c, on_line, cancel)
    }
}

/// an interrupted transfer never leaves a truncated file under the real
/// name; the previous archived version stays intact.
#[test]
fn h4_interrupted_transfer_never_truncates_the_archived_file() {
    for path in ["/opt/homebrew/bin/rsync", "/usr/bin/rsync"]
        .iter()
        .filter(|p| Path::new(p).exists())
    {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let open = r_is_openrsync(path);
        let size: usize = if open { 768 << 20 } else { 3 << 20 };
        let cancel_after = Duration::from_millis(if open { 150 } else { 1500 });
        let big = s.join("Capture/B.ARW");
        fs::write(&big, vec![1u8; size]).unwrap();
        let fast =
            RsyncRunner::detect_from(Arc::new(SystemRunner), &[PathBuf::from(path)]).unwrap();
        let out = Archiver::new(fast, None)
            .archive(&s, &target, when(1), &mut |_| {}, &AtomicBool::new(false))
            .unwrap();
        // Re-shot / rewritten file: new content, same name.
        fs::write(&big, vec![2u8; size]).unwrap();
        // A later rewrite has a later mtime (rsync quick-checks whole seconds).
        fs::File::options()
            .write(true)
            .open(&big)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(10))
            .unwrap();
        let slow = RsyncRunner::detect_from(Arc::new(Slow), &[PathBuf::from(path)]).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(cancel_after);
            c2.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let res = Archiver::new(slow, None).archive(&s, &target, when(2), &mut |_| {}, &cancel);
        timer.join().unwrap();
        assert!(
            matches!(res, Err(ArchiveError::Cancelled)),
            "{path}: {res:?}"
        );
        let archived = fs::read(out.dest.join("Capture/B.ARW")).unwrap();
        assert_eq!(
            archived.len(),
            size,
            "{path}: truncated file under the real name"
        );
        assert!(
            archived.iter().all(|b| *b == 1),
            "{path}: old version intact"
        );
        // Whatever was transferred sits in the partial dir, not in place.
        let pdir = out.dest.join("Capture").join(PARTIAL_DIR);
        if let Ok(p) = fs::metadata(pdir.join("B.ARW")) {
            assert!(p.len() < size as u64);
        }
    }
}

fn r_is_openrsync(path: &str) -> bool {
    matches!(
        RsyncRunner::detect_from(Arc::new(SystemRunner), &[PathBuf::from(path)])
            .unwrap()
            .flavor(),
        RsyncFlavor::Openrsync
    )
}

/// an unreadable source file is listed as a problem (it is not in the
/// archive), with its path and reason; everything else still archives.
#[test]
fn unreadable_source_file_is_a_problem_and_the_rest_archives() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let locked = s.join("Capture/LOCKED.ARW");
        fs::write(&locked, b"x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let out = Archiver::new(r.clone(), None).archive(
            &s,
            &target,
            when(3),
            &mut |_| {},
            &AtomicBool::new(false),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        let out = out.unwrap_or_else(|e| panic!("{:?}: {e}", r.flavor()));
        assert_eq!(
            out.problems.len(),
            1,
            "{:?}: {:?}",
            r.flavor(),
            out.problems
        );
        assert!(
            out.problems[0].starts_with("Capture/LOCKED.ARW: ")
                && out.problems[0].contains("Permission denied"),
            "{:?}: {:?}",
            r.flavor(),
            out.problems
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert!(out.dest.join("Capture/A.ARW").exists());
        assert!(!out.dest.join("Capture/LOCKED.ARW").exists());
    }
}

/// two runs in the same second (or a repeated DST hour) never share a
/// backup dir; every replaced version is kept.
#[test]
fn h5_same_second_runs_keep_every_replaced_version() {
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let f = s.join("Capture/A.ARW");
        let a = Archiver::new(r.clone(), None);
        let now = when(1_790_000_000);
        let first = a
            .archive(&s, &target, now, &mut |_| {}, &AtomicBool::new(false))
            .unwrap();
        let mut dirs = Vec::new();
        for v in [b"v2".as_slice(), b"v3-longer"] {
            fs::write(&f, v).unwrap();
            let o = a
                .archive(&s, &target, now, &mut |_| {}, &AtomicBool::new(false))
                .unwrap();
            dirs.push(o.backup_dir);
        }
        assert_ne!(dirs[0], dirs[1], "{:?}", r.flavor());
        assert_eq!(
            fs::read(first.dest.join(&dirs[0]).join("Capture/A.ARW")).unwrap(),
            b"raw"
        );
        assert_eq!(
            fs::read(first.dest.join(&dirs[1]).join("Capture/A.ARW")).unwrap(),
            b"v2"
        );
        assert_eq!(
            fs::read(first.dest.join("Capture/A.ARW")).unwrap(),
            b"v3-longer"
        );
    }
}

/// Medium: a failed snapshot must not ship the stale snapshot file.
#[test]
fn failed_snapshot_does_not_archive_the_stale_one() {
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        fs::create_dir_all(s.join(".ds")).unwrap();
        fs::write(s.join(SNAPSHOT_REL), b"stale from last week").unwrap();
        let out = Archiver::new(r.clone(), Some(Arc::new(Snap(true))))
            .archive(&s, &target, when(4), &mut |_| {}, &AtomicBool::new(false))
            .unwrap();
        assert!(out.snapshot.is_none());
        assert!(!out.dest.join(SNAPSHOT_REL).exists(), "{:?}", r.flavor());
        assert!(out.dest.join("Capture/A.ARW").exists());
    }
}

#[test]
fn both_flavors_detected_when_installed() {
    let flavors: Vec<RsyncFlavor> = rsyncs().iter().map(|r| r.flavor().clone()).collect();
    if Path::new("/opt/homebrew/bin/rsync").exists() {
        assert!(
            flavors
                .iter()
                .any(|f| matches!(f, RsyncFlavor::Rsync3 { .. }))
        );
    }
    let _ = Mutex::new(());
}

/// Rejects excluded through the `--exclude-from` file (anchored, with a character rsync
/// would read as a wildcard) and a Move to Selects mirrored on the target (sidecar
/// included): with both real rsyncs, one copy of each pick, the reject not archived,
/// nothing deleted.
#[test]
fn rejects_stay_out_and_moves_are_mirrored_with_real_rsync() {
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, mut target) = setup(t.path());
        fs::write(s.join("Capture/R[1].ARW"), b"reject").unwrap();
        // A RAW big enough that re-sending it would show.
        let raw: Vec<u8> = (0..4_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
        fs::write(s.join("Capture/A.ARW"), &raw).unwrap();
        target.excludes.push("/Capture/R\\[1\\].ARW".to_owned());
        target.mirror_folders = vec!["Capture".into(), "Selects".into()];
        let a = Archiver::new(r.clone(), None);
        let out = a
            .archive(
                &s,
                &target,
                when(1_790_000_000),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        let dest = out.dest.clone();
        assert!(dest.join("Capture/A.ARW").exists(), "{r:?}");
        assert!(
            !dest.join("Capture/R[1].ARW").exists(),
            "{r:?}: reject excluded"
        );
        let archived = fs::metadata(dest.join("Capture/A.ARW")).unwrap();
        // Move to Selects, sidecar and all.
        fs::create_dir_all(s.join("Selects/.ds/settings")).unwrap();
        fs::rename(s.join("Capture/A.ARW"), s.join("Selects/A.ARW")).unwrap();
        fs::rename(
            s.join("Capture/.ds/settings/A.ARW.json"),
            s.join("Selects/.ds/settings/A.ARW.json"),
        )
        .unwrap();
        let mut sent = 0u64;
        let second = a
            .archive(
                &s,
                &target,
                when(1_790_000_100),
                &mut |p| sent = sent.max(p.bytes),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(dest.join("Selects/A.ARW").exists(), "{r:?}");
        // the move was a rename on the target, not a re-copy: the same file
        // (inode, mtime), no RAW data sent, and no displaced copy in the backup dir.
        let moved = fs::metadata(dest.join("Selects/A.ARW")).unwrap();
        assert_eq!(moved.ino(), archived.ino(), "{r:?}: re-copied");
        assert_eq!(moved.mtime(), archived.mtime(), "{r:?}");
        assert!(sent < 100_000, "{r:?}: {sent} bytes sent");
        let backup = dest.join(&second.backup_dir);
        let backed_up_raws = walk(&backup)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "ARW"))
            .count();
        assert_eq!(backed_up_raws, 0, "{r:?}");
        assert!(
            dest.join("Selects/.ds/settings/A.ARW.json").exists(),
            "{r:?}"
        );
        assert!(!dest.join("Capture/A.ARW").exists(), "{r:?}: one copy");
        assert!(
            !dest.join("Capture/.ds/settings/A.ARW.json").exists(),
            "{r:?}"
        );
        assert_eq!(fs::read(dest.join("Selects/A.ARW")).unwrap(), raw);
    }
}

/// Every file under `dir` (none if it doesn't exist).
fn walk(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .flat_map(|e| {
            let p = e.path();
            if p.is_dir() { walk(&p) } else { vec![p] }
        })
        .collect()
}

/// The index snapshot is replaced in place every run: its old copies never pile up in
/// the archive trash, while a replaced sidecar still is kept there.
#[test]
fn replaced_snapshot_is_not_backed_up_but_replaced_sidecars_are() {
    struct Counting(Mutex<u32>);
    impl Snapshotter for Counting {
        fn snapshot(&self, _: &Path, dest: &Path) -> Result<(), String> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            fs::write(dest, format!("snapshot {n}")).map_err(|e| e.to_string())?;
            // A real snapshot is written minutes apart; the quick check compares mtimes.
            let f = fs::File::options()
                .write(true)
                .open(dest)
                .map_err(|e| e.to_string())?;
            f.set_modified(when(1_790_000_000 + 100 * u64::from(*n)))
                .map_err(|e| e.to_string())
        }
    }
    for r in rsyncs() {
        let t = tempfile::tempdir().unwrap();
        let (s, target) = setup(t.path());
        let a = Archiver::new(r.clone(), Some(Arc::new(Counting(Mutex::new(0)))));
        let first = a
            .archive(
                &s,
                &target,
                when(1_790_000_000),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        fs::write(
            s.join("Capture/.ds/settings/A.ARW.json"),
            b"{\"exposure\":2}",
        )
        .unwrap();
        let out = a
            .archive(
                &s,
                &target,
                when(1_790_000_100),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        let d = &out.dest;
        assert_eq!(first.dest, *d);
        assert_eq!(
            fs::read(d.join(SNAPSHOT_REL)).unwrap(),
            b"snapshot 2",
            "{:?}",
            r.flavor()
        );
        let trashed: Vec<String> = walk(&d.join(TRASH_DIR))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            !trashed.iter().any(|n| n == "index-snapshot.db"),
            "{trashed:?} {:?}",
            r.flavor()
        );
        assert!(trashed.iter().any(|n| n == "A.ARW.json"), "{trashed:?}");
    }
}
