//! Safe filesystem primitives shared by `lt-import`, `lt-archive` and
//! `lt-export`.
//!
//! * [`rename_noreplace`]: atomic "place without replacing" that also works on
//!   exFAT (no `RENAME_EXCL`, no hard links) via a check-then-rename fallback.
//! * [`write_atomic`] / [`temp_path`]: temp → fsync → rename with short,
//!   unique temp names (safe for 255-byte file names and concurrent writers).
//! * [`sync_file`] / [`flush_device`]: cheap per-file `fsync` plus one
//!   device-wide `F_FULLFSYNC` checkpoint.
//! * [`set_nocache`]: `F_NOCACHE` so verification re-reads hit the device.
//! * [`DirLock`]: cross-process advisory lock (`flock`).
//! * [`mount_point_of`] / [`volume_uuid`]: stable identity of a card.
//! * [`catch_panic`]: run an injected callback without letting its panic
//!   take down a batch.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
pub mod testvol;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Rename `from` → `to`, failing with `AlreadyExists` instead of replacing.
///
/// Uses `renameatx_np(RENAME_EXCL)` / `renameat2(RENAME_NOREPLACE)`. File
/// systems without it (exFAT returns `ENOTSUP`; some NAS shares `EINVAL`) fall
/// back to check-then-rename. That window is closed for our own writers by
/// holding a [`DirLock`] on the destination; it only matters against other
/// programs writing the same name at the same instant.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::io::Errno;
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(e) if e == Errno::EXIST => Err(io::ErrorKind::AlreadyExists.into()),
        Err(e)
            if e == Errno::NOTSUP
                || e == Errno::INVAL
                || e == Errno::NOSYS
                || e == Errno::OPNOTSUPP =>
        {
            rename_checked(from, to)
        }
        Err(e) => Err(e.into()),
    }
}

/// Fallback for [`rename_noreplace`]: check, then rename.
pub fn rename_checked(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short, process- and call-unique hidden temp path in `dir`, e.g.
/// `.lt-1a2b-7.tmp`. Never derived from the final name, so a 255-byte final
/// name still has a valid temp name.
pub fn temp_path(dir: &Path, ext: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".lt-{:x}-{n:x}.{ext}", std::process::id()))
}

/// Migration source: the prefix Darkslide 0.1 gave its temp files. Its leftovers
/// are cleaned like ours.
pub const LEGACY_TEMP_PREFIX: &str = ".ds-";

/// A [`temp_path`] file not modified for this long was left behind by a crash.
pub const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Remove `path` if it is one of our temp files a crash left behind: named like
/// [`temp_path`] (`.lt-<hex>-<hex>.partial` or `.tmp`, or Darkslide 0.1's `.ds-…`) and not modified for
/// [`STALE_TEMP_AGE`] (a write in progress keeps touching it). Anything else is never
/// touched. Returns whether it was removed.
pub fn remove_stale_temp(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = (name.strip_prefix(".lt-"))
        .or_else(|| name.strip_prefix(LEGACY_TEMP_PREFIX))
        .and_then(|n| {
            n.strip_suffix(".partial")
                .or_else(|| n.strip_suffix(".tmp"))
        })
    else {
        return false;
    };
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !stem.split_once('-').is_some_and(|(a, b)| hex(a) && hex(b)) {
        return false;
    }
    let old = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= STALE_TEMP_AGE);
    old && fs::remove_file(path).is_ok()
}

/// Write `bytes` to `path` atomically: unique temp in the same dir → fsync →
/// rename (replacing `path`; use for our own metadata files only).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = temp_path(dir, "tmp");
    let res = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Read a regular file of at most `max` bytes into memory. For files from
/// someone else's folder: a FIFO or device is refused without blocking, the
/// limit is enforced on the bytes actually read (a growing file cannot slip
/// past a size check), and with `follow_symlinks == false` a symlink is
/// refused too. Failures are `InvalidData` (not a regular file / too large)
/// or the underlying error.
#[allow(clippy::cast_possible_wrap)] // open flags are small
pub fn read_regular_capped(path: &Path, max: u64, follow_symlinks: bool) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = rustix::fs::OFlags::NONBLOCK;
    if !follow_symlinks {
        flags |= rustix::fs::OFlags::NOFOLLOW;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut out = Vec::new();
    file.take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {max} bytes"),
        ));
    }
    Ok(out)
}

/// Remove a file, treating "already gone" as success.
pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Push a file's data to the device (`fsync`). On macOS this does *not*
/// flush the drive's own cache; [`flush_device`] does that once per
/// checkpoint for everything written so far (per-file `F_FULLFSYNC` measured
/// ~3× slower on the M4 Pro SSD).
pub fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return rustix::fs::fsync(file).map_err(io::Error::from);
    #[cfg(not(target_vendor = "apple"))]
    file.sync_all()
}

/// Make everything written so far to the volume holding `path` durable
/// (`F_FULLFSYNC` on macOS; `fsync` elsewhere).
pub fn flush_device(path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    #[cfg(target_vendor = "apple")]
    return rustix::fs::fcntl_fullfsync(&f).map_err(io::Error::from);
    #[cfg(not(target_vendor = "apple"))]
    f.sync_all()
}

/// Bypass the unified buffer cache for this descriptor (`F_NOCACHE`). Set on
/// the writer *and* the verify reader so verification reads the device, not
/// the pages we just wrote. Best effort: failure only weakens verification.
pub fn set_nocache(file: &File) {
    #[cfg(target_vendor = "apple")]
    if let Err(err) = rustix::fs::fcntl_nocache(file, true) {
        tracing::debug!(%err, "F_NOCACHE unavailable");
    }
    #[cfg(not(target_vendor = "apple"))]
    let _ = file;
}

/// Drop cached pages where supported (`posix_fadvise(DONTNEED)` on Linux).
pub fn drop_cache(file: &File) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = file;
}

/// Exclusive advisory lock on a lock file (`flock`), released on drop.
/// Works across processes; also between two opens in one process.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
    path: PathBuf,
}

impl DirLock {
    /// Try to take the lock at `path` (created if missing). `Ok(None)` when
    /// another holder has it.
    ///
    /// Lock files must never be deleted while in use (callers leave them in
    /// place). As a second line of defence, after `flock` succeeds the path
    /// is re-checked to still name the locked inode; if someone unlinked or
    /// replaced it, the lock is retried on the new file.
    pub fn try_lock(path: &Path) -> io::Result<Option<Self>> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        for _ in 0..8 {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                // A symlink planted at the lock path must not be followed.
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(path)?;
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(e) if e == rustix::io::Errno::WOULDBLOCK => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            let held = file.metadata()?;
            if fs::metadata(path).is_ok_and(|m| m.dev() == held.dev() && m.ino() == held.ino()) {
                return Ok(Some(Self {
                    _file: file,
                    path: path.to_path_buf(),
                }));
            }
        }
        Err(io::Error::other(format!(
            "lock file {} keeps being replaced",
            path.display()
        )))
    }

    /// Lock the directory `dir` itself (`flock` on a directory handle): no
    /// lock file exists that could be deleted or left behind. Where the file
    /// system refuses directory locks, falls back to `dir/.lt/<name>.lock`
    /// (a lock file that is never deleted).
    pub fn try_lock_dir(dir: &Path, fallback_name: &str) -> io::Result<Option<Self>> {
        let file = File::open(dir)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self {
                _file: file,
                path: dir.to_path_buf(),
            })),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(None),
            Err(e)
                if e == rustix::io::Errno::NOTSUP
                    || e == rustix::io::Errno::OPNOTSUPP
                    || e == rustix::io::Errno::INVAL =>
            {
                Self::try_lock(&dir.join(".lt").join(format!("{fallback_name}.lock")))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// An open handle on a destination root, used to make everything written
/// under it durable. Flushing goes through the handle, and first checks that
/// the path still names the same directory on the same device: a volume that
/// was unmounted (or replaced by an empty mount-point directory) is an
/// error, never a silent skip.
#[derive(Debug)]
pub struct RootHandle {
    file: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl RootHandle {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let m = file.metadata()?;
        if !m.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `F_FULLFSYNC` through the handle, after checking the root is still
    /// the directory it was when opened.
    pub fn flush(&self) -> io::Result<()> {
        let still =
            fs::metadata(&self.path).is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino);
        if !still {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} was unmounted or replaced since the import started",
                    self.path.display()
                ),
            ));
        }
        #[cfg(target_vendor = "apple")]
        return rustix::fs::fcntl_fullfsync(&self.file).map_err(io::Error::from);
        #[cfg(not(target_vendor = "apple"))]
        self.file.sync_all()
    }
}

/// True when `dir` exists and lives on a different device than its parent.
pub fn is_mount_point(dir: &Path) -> bool {
    let Some(parent) = dir.parent() else {
        return true;
    };
    match (fs::metadata(dir), fs::metadata(parent)) {
        (Ok(d), Ok(p)) => d.is_dir() && d.dev() != p.dev(),
        _ => false,
    }
}

/// For a path under `/Volumes/<name>/…`, require `/Volumes/<name>` to be a
/// real mount. A backup drive that is not plugged in otherwise becomes an
/// ordinary folder on the internal disk the moment anything creates it.
pub fn require_mounted(path: &Path) -> io::Result<()> {
    use std::path::Component;
    let mut c = path.components();
    if let (Some(Component::RootDir), Some(Component::Normal(v)), Some(Component::Normal(name))) =
        (c.next(), c.next(), c.next())
        && v == "Volumes"
    {
        let vol = Path::new("/Volumes").join(name);
        if !is_mount_point(&vol) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not mounted", vol.display()),
            ));
        }
    }
    Ok(())
}

/// How long a failed (timed-out) removable probe is remembered, so a wedged
/// volume costs one deadline per window instead of one per file.
const UNKNOWN_TTL: Duration = Duration::from_secs(30);

/// Per-mount cache of "is this removable?". Successful answers are kept for
/// the process lifetime; failed probes only for [`UNKNOWN_TTL`].
#[derive(Default)]
struct RemovableCache(
    std::sync::Mutex<std::collections::HashMap<PathBuf, (Option<bool>, Instant)>>,
);

impl RemovableCache {
    /// Answer for `mount`, running `probe` (outside the lock) on a miss.
    /// Unknown (`probe` returned `None`) is reported as removable: callers use
    /// this to decide whether a copy can be trusted, and a volume that cannot
    /// be identified must not be.
    fn get(&self, mount: &Path, now: Instant, probe: impl FnOnce() -> Option<bool>) -> bool {
        let lock = || {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some((v, at)) = lock().get(mount)
            && (v.is_some() || now.saturating_duration_since(*at) < UNKNOWN_TTL)
        {
            return v.unwrap_or(true);
        }
        let v = probe();
        lock().insert(mount.to_path_buf(), (v, now));
        v.unwrap_or(true)
    }
}

/// Is the volume holding `path` removable or ejectable media (SD card, USB
/// stick, disk image)? Cached per mount point. **`true` when unknown** (the
/// path or `diskutil` cannot be resolved in time): treating an unidentified
/// volume as fixed storage could mark card files as already safe.
pub fn is_removable(path: &Path) -> bool {
    static CACHE: std::sync::LazyLock<RemovableCache> = std::sync::LazyLock::new(Default::default);
    let Ok(mount) = mount_point_of(path) else {
        return true;
    };
    CACHE.get(&mount, Instant::now(), || removable_from_diskutil(&mount))
}

/// How long `diskutil info` may take before the answer is "unknown".
#[cfg(target_os = "macos")]
const DISKUTIL_DEADLINE: Duration = Duration::from_secs(5);

/// `diskutil info -plist <mount>` with a deadline (`None`: unavailable,
/// failed or timed out).
#[cfg(target_os = "macos")]
fn diskutil_info(mount: &Path) -> Option<String> {
    run_with_deadline(
        Command::new("/usr/sbin/diskutil")
            .arg("info")
            .arg("-plist")
            .arg(mount),
        DISKUTIL_DEADLINE,
    )
    .ok()
}

/// Run `cmd`, killing it after `deadline`. Returns stdout on success.
pub fn run_with_deadline(cmd: &mut Command, deadline: Duration) -> io::Result<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{cmd:?} did not finish within {deadline:?} (disk image services wedged?)"),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let (stdout, stderr) = (
        out_t.join().unwrap_or_default(),
        err_t.join().unwrap_or_default(),
    );
    if !status.success() {
        return Err(io::Error::other(format!(
            "{cmd:?} failed: {stdout}{stderr}"
        )));
    }
    Ok(stdout)
}

fn removable_from_diskutil(mount: &Path) -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        let pl = diskutil_info(mount)?;
        Some(
            [
                "Removable",
                "RemovableMedia",
                "Ejectable",
                "RemovableMediaOrExternalDevice",
            ]
            .iter()
            .any(|k| plist_bool(&pl, k) == Some(true)),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = mount;
        Some(false)
    }
}

/// Value of `<key>k</key><true/>|<false/>` in an XML plist.
pub fn plist_bool(plist: &str, key: &str) -> Option<bool> {
    let after = plist
        .split(&format!("<key>{key}</key>"))
        .nth(1)?
        .trim_start();
    if after.starts_with("<true/>") {
        Some(true)
    } else if after.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

/// The mount point containing `path` (walks up while the device stays the
/// same). `path` must exist.
pub fn mount_point_of(path: &Path) -> io::Result<PathBuf> {
    let mut cur = fs::canonicalize(path)?;
    let dev = fs::metadata(&cur)?.dev();
    while let Some(parent) = cur.parent() {
        if fs::metadata(parent)?.dev() != dev {
            break;
        }
        cur = parent.to_path_buf();
    }
    Ok(cur)
}

/// Stable identity of the volume mounted at `mount`: the file-system UUID
/// from `diskutil` on macOS (FAT/exFAT derive it from the volume serial, so
/// it survives remounts under another name). `None` if unavailable.
pub fn volume_uuid(mount: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let Some(info) = diskutil_info(mount) else {
            tracing::warn!(mount = %mount.display(), "diskutil info failed or timed out; volume identity unknown");
            return None;
        };
        plist_string(&info, "VolumeUUID")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = mount;
        None
    }
}

/// Value of `<key>k</key><string>v</string>` in an XML plist.
pub fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = plist.split(&format!("<key>{key}</key>")).nth(1)?;
    let s = after.trim_start().strip_prefix("<string>")?;
    Some(s.split("</string>").next()?.to_owned())
}

/// Run a caller-supplied callback, converting a panic into `Err(message)` so
/// one bad callback fails one item, not the whole batch.
pub fn catch_panic<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_read_enforces_size_type_and_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f.json");
        fs::write(&f, b"abcd").unwrap();
        assert_eq!(read_regular_capped(&f, 4, false).unwrap(), b"abcd");
        let e = read_regular_capped(&f, 3, false).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let link = t.path().join("link.json");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(read_regular_capped(&link, 10, false).is_err(), "no follow");
        assert_eq!(read_regular_capped(&link, 10, true).unwrap(), b"abcd");

        // A FIFO neither blocks nor reads.
        let fifo = t.path().join("fifo.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let e = read_regular_capped(&fifo, 10, true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(read_regular_capped(&t.path().join("none"), 1, true).is_err());
    }

    #[test]
    fn only_our_stale_temp_files_are_removed() {
        let t = tempfile::tempdir().unwrap();
        let make = |name: &str, age: u64| {
            let p = t.path().join(name);
            let f = File::create(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age))
                .unwrap();
            p
        };
        let stale = make(".lt-1a2b-7.partial", 7200);
        let stale_tmp = make(".lt-ff-0.tmp", 7200);
        let fresh = make(".lt-1a2b-8.partial", 10);
        assert!(remove_stale_temp(&stale) && !stale.exists());
        assert!(remove_stale_temp(&stale_tmp));
        let legacy = make(".ds-1a2b-9.partial", 7200);
        assert!(remove_stale_temp(&legacy), "Darkslide 0.1's leftovers too");
        assert!(
            !remove_stale_temp(&fresh) && fresh.exists(),
            "a write in progress"
        );
        // Never a photo, nor anything merely similar.
        for name in [
            "DSC00001.partial",
            ".lt-zz-1.partial",
            ".lt-1a2b.partial",
            ".lt-1a2b-7.ARW",
            ".lt--7.partial",
        ] {
            let p = make(name, 7200);
            assert!(!remove_stale_temp(&p) && p.exists(), "{name}");
        }
        let dir = t.path().join(".lt-1-2.partial-dir.partial");
        fs::create_dir(&dir).unwrap();
        assert!(!remove_stale_temp(&dir));
        assert!(!remove_stale_temp(Path::new("/")));
        assert!(
            !remove_stale_temp(&t.path().join(".lt-1-3.partial")),
            "missing"
        );
    }

    #[test]
    fn rename_noreplace_never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let (a, b, c) = (t.path().join("a"), t.path().join("b"), t.path().join("c"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert_eq!(
            rename_noreplace(&a, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&b).unwrap(), b"b");
        rename_noreplace(&a, &c).unwrap();
        assert!(
            rename_noreplace(&a, &t.path().join("d")).is_err(),
            "source gone"
        );
        assert_eq!(
            rename_checked(&c, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        rename_checked(&c, &a).unwrap();
        assert!(a.exists() && !c.exists());
    }

    #[test]
    fn temp_names_are_short_and_unique() {
        let d = Path::new("/x");
        let (a, b) = (temp_path(d, "partial"), temp_path(d, "partial"));
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().len() < 40);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with(".lt-"));
    }

    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/j.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
        let d = t.path().join("dir");
        fs::create_dir_all(d.join("x")).unwrap();
        assert!(write_atomic(&d, b"z").is_err());
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 2, "temp removed");
        // 255-byte final names still work (temp name is short).
        let long = t.path().join("a".repeat(255));
        write_atomic(&long, b"x").unwrap();
        assert_eq!(fs::read(&long).unwrap(), b"x");
    }

    #[test]
    fn durability_helpers() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("f");
        fs::write(&p, b"x").unwrap();
        let f = File::open(&p).unwrap();
        sync_file(&f).unwrap();
        set_nocache(&f);
        drop_cache(&f);
        flush_device(t.path()).unwrap();
        assert!(flush_device(&t.path().join("missing")).is_err());
        remove_if_exists(&p).unwrap();
        remove_if_exists(&p).unwrap();
        assert!(remove_if_exists(t.path()).is_err());
    }

    #[test]
    fn dir_lock_is_exclusive_until_dropped() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("locks/x.lock");
        let a = DirLock::try_lock(&p).unwrap().expect("first");
        assert_eq!(a.path(), p);
        assert!(
            DirLock::try_lock(&p).unwrap().is_none(),
            "second open is refused"
        );
        drop(a);
        // Other tests spawn processes; a child between fork and exec briefly
        // shares our open file description (and so the flock). Real callers
        // treat "busy" as "retry later"; do the same here.
        let reacquired = (0..200).any(|_| {
            DirLock::try_lock(&p).unwrap().is_some() || {
                std::thread::sleep(std::time::Duration::from_millis(5));
                false
            }
        });
        assert!(reacquired);
        let file = t.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(DirLock::try_lock(&file.join("x.lock")).is_err());
    }

    #[test]
    fn mount_points_and_plist() {
        let t = tempfile::tempdir().unwrap();
        let mp = mount_point_of(t.path()).unwrap();
        assert!(t.path().canonicalize().unwrap().starts_with(&mp));
        assert_eq!(
            mount_point_of(Path::new("/dev")).unwrap(),
            PathBuf::from("/dev")
        );
        assert!(mount_point_of(&t.path().join("nope")).is_err());
        let pl = "<dict>\n<key>VolumeUUID</key>\n\t<string>AB-CD</string></dict>";
        assert_eq!(plist_string(pl, "VolumeUUID").as_deref(), Some("AB-CD"));
        assert_eq!(plist_string(pl, "Nope"), None);
        assert_eq!(
            plist_string("<key>VolumeUUID</key><integer>1</integer>", "VolumeUUID"),
            None
        );
        let _ = volume_uuid(Path::new("/"));
        assert_eq!(volume_uuid(&t.path().join("nope")), None);
    }

    #[test]
    fn root_handle_flushes_and_detects_replacement() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("dest");
        fs::create_dir(&root).unwrap();
        let h = RootHandle::open(&root).unwrap();
        assert_eq!(h.path(), root);
        h.flush().unwrap();
        // Unmount stand-in: the path now names a different (empty) directory.
        fs::rename(&root, t.path().join("moved")).unwrap();
        fs::create_dir(&root).unwrap();
        assert_eq!(h.flush().unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::remove_dir(&root).unwrap();
        assert!(h.flush().is_err(), "gone entirely");
        let f = t.path().join("file");
        fs::write(&f, b"x").unwrap();
        assert!(RootHandle::open(&f).is_err());
        assert!(RootHandle::open(&t.path().join("none")).is_err());
    }

    #[test]
    fn mount_checks() {
        assert!(is_mount_point(Path::new("/")));
        assert!(is_mount_point(Path::new("/dev")));
        let t = tempfile::tempdir().unwrap();
        assert!(!is_mount_point(t.path()));
        assert!(!is_mount_point(&t.path().join("x")));
        require_mounted(t.path()).unwrap();
        require_mounted(Path::new("relative/x")).unwrap();
        let e = require_mounted(Path::new("/Volumes/lt-test-not-plugged-in/Backup")).unwrap_err();
        assert!(e.to_string().contains("not mounted"));
        // The boot volume is not removable; a temp dir lives on it.
        assert!(!is_removable(t.path()));
        assert!(!is_removable(t.path()), "cached");
        assert_eq!(
            plist_bool("<key>Ejectable</key>\n\t<true/>", "Ejectable"),
            Some(true)
        );
        assert_eq!(
            plist_bool("<key>Ejectable</key><false/>", "Ejectable"),
            Some(false)
        );
        assert_eq!(
            plist_bool("<key>Ejectable</key><string>x</string>", "Ejectable"),
            None
        );
        assert_eq!(plist_bool("", "Ejectable"), None);
    }

    #[test]
    fn directory_lock_is_exclusive_and_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        let a = DirLock::try_lock_dir(t.path(), "import").unwrap().unwrap();
        assert_eq!(a.path(), t.path());
        assert!(DirLock::try_lock_dir(t.path(), "import").unwrap().is_none());
        drop(a);
        // A child spawned concurrently by another test holds a copy of the fd
        // between fork and exec (O_CLOEXEC only closes it at exec), so the
        // released lock can stay held for a moment. Retry briefly.
        let relocked = (0..200).any(|_| {
            DirLock::try_lock_dir(t.path(), "import").unwrap().is_some() || {
                std::thread::sleep(std::time::Duration::from_millis(5));
                false
            }
        });
        assert!(relocked, "lock was released");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 0, "no lock file");
        assert!(DirLock::try_lock_dir(&t.path().join("missing"), "x").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_lock_on_card_file_systems() {
        for fs_ in [testvol::Fs::Fat32, testvol::Fs::ExFat] {
            let t = tempfile::tempdir().unwrap();
            let Some(img) = testvol::require(t.path(), fs_, 64, "LCK") else {
                continue;
            };
            let d = img.mount_point();
            let a = DirLock::try_lock_dir(d, "import").unwrap().expect("first");
            assert!(
                DirLock::try_lock_dir(d, "import").unwrap().is_none(),
                "{fs_:?}"
            );
            drop(a);
        }
    }

    #[test]
    fn dir_lock_follows_a_replaced_lock_file() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.lock");
        let a = DirLock::try_lock(&p).unwrap().unwrap();
        // Someone unlinks the held lock file: a new locker must not believe
        // it holds the same lock as `a` on a different inode... it gets the
        // new file (the old inode is unreachable), which is the documented
        // reason lock files are never deleted.
        fs::remove_file(&p).unwrap();
        let b = DirLock::try_lock(&p).unwrap();
        assert!(b.is_some());
        drop(a);
    }

    #[test]
    fn catch_panic_converts_panics() {
        assert_eq!(catch_panic(|| 5), Ok(5));
        assert_eq!(catch_panic(|| -> u8 { panic!("boom") }), Err("boom".into()));
        assert_eq!(
            catch_panic(|| -> u8 { panic!("{}", String::from("dyn")) }),
            Err("dyn".into())
        );
        assert_eq!(
            catch_panic(|| -> u8 { std::panic::panic_any(7u8) }),
            Err("panic".into())
        );
    }

    #[test]
    fn run_with_deadline_kills_a_hung_command() {
        let t0 = Instant::now();
        let r = run_with_deadline(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(150),
        );
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert_eq!(
            run_with_deadline(Command::new("/bin/echo").arg("hi"), Duration::from_secs(10))
                .unwrap()
                .trim(),
            "hi"
        );
        assert!(
            run_with_deadline(&mut Command::new("/usr/bin/false"), Duration::from_secs(10))
                .is_err()
        );
    }

    #[test]
    fn dir_lock_does_not_follow_a_symlinked_lock_path() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("victim");
        let link = t.path().join("x.lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(DirLock::try_lock(&link).is_err());
        assert!(!target.exists(), "dangling target must not be created");
        let plain = t.path().join("y.lock");
        assert!(DirLock::try_lock(&plain).unwrap().is_some());
    }

    #[test]
    fn removable_cache_is_conservative_lock_free_and_expires_failures() {
        use std::cell::Cell;
        let cache = RemovableCache::default();
        let m = Path::new("/Volumes/CARD");
        let t0 = Instant::now();
        let calls = Cell::new(0);
        let probe = |r: Option<bool>| {
            calls.set(calls.get() + 1);
            r
        };
        // A probe that fails (timeout): reported removable, remembered briefly.
        assert!(cache.get(m, t0, || probe(None)));
        assert!(cache.get(m, t0 + Duration::from_secs(5), || probe(None)));
        assert_eq!(calls.get(), 1, "no re-probe inside the window");
        // After the window it is probed again; a real answer sticks forever.
        assert!(!cache.get(m, t0 + UNKNOWN_TTL, || probe(Some(false))));
        assert!(!cache.get(m, t0 + UNKNOWN_TTL * 100, || probe(None)));
        assert_eq!(calls.get(), 2);
        // The probe runs without the cache lock: another mount can be
        // answered from inside it.
        assert!(cache.get(Path::new("/a"), t0, || {
            assert!(!cache.get(Path::new("/b"), t0, || Some(false)));
            Some(true)
        }));
    }

    #[test]
    fn unresolvable_path_counts_as_removable() {
        let t = tempfile::tempdir().unwrap();
        assert!(is_removable(&t.path().join("missing")));
    }
}
