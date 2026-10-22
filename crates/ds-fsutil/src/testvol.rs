//! Real FAT32/exFAT/APFS volumes for tests, backed by disk images
//! (`hdiutil create` + `diskutil image attach`, no root needed).
//!
//! Used to reproduce card-like behavior (no `RENAME_EXCL`, no hard links,
//! 2 s mtimes, remounting under another name, yanking the card).
//!
//! **Never hangs.** Every external command runs with a deadline and is
//! killed on expiry (`io::ErrorKind::TimedOut`); disk-image services can
//! wedge system-wide. Tests call [`require`], which probes once per process
//! with a tiny image, caches the verdict, and prints `SKIP testvol: …` and
//! returns `None` when images are unavailable, so a skip is visible in the
//! test output instead of a silent pass or a hung suite.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

pub use crate::run_with_deadline;

/// Deadline for one `hdiutil`/`diskutil` call (override with
/// `DS_TESTVOL_TIMEOUT_SECS`).
pub fn command_timeout() -> Duration {
    let secs = std::env::var("DS_TESTVOL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Deadline for the one-time availability probe (override with
/// `DS_TESTVOL_PROBE_SECS`).
fn probe_timeout() -> Duration {
    let secs = std::env::var("DS_TESTVOL_PROBE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    Duration::from_secs(secs)
}

/// File system of a test image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fs {
    Fat32,
    ExFat,
    Apfs,
}

impl Fs {
    fn hdiutil_name(self) -> &'static str {
        match self {
            Self::Fat32 => "MS-DOS FAT32",
            Self::ExFat => "ExFAT",
            Self::Apfs => "APFS",
        }
    }
}

/// A disk image, attached at a mount point; detached (bounded) on drop.
#[derive(Debug)]
pub struct DiskImage {
    image: PathBuf,
    mount: Option<PathBuf>,
    device: Option<String>,
    deadline: Duration,
}

impl DiskImage {
    /// Create a `size_mb` image named `volname` in `dir` and mount it at
    /// `dir/<volname>-mnt`. Each step is bounded by [`command_timeout`].
    pub fn create(dir: &Path, fs: Fs, size_mb: u32, volname: &str) -> io::Result<Self> {
        Self::create_with_deadline(dir, fs, size_mb, volname, command_timeout())
    }

    fn create_with_deadline(
        dir: &Path,
        fs: Fs,
        size_mb: u32,
        volname: &str,
        deadline: Duration,
    ) -> io::Result<Self> {
        let image = dir.join(format!("{volname}.dmg"));
        run_with_deadline(
            Command::new("/usr/bin/hdiutil")
                .args([
                    "create",
                    "-quiet",
                    "-size",
                    &format!("{size_mb}m"),
                    "-fs",
                    fs.hdiutil_name(),
                    "-volname",
                    volname,
                ])
                .arg(&image),
            deadline,
        )?;
        let mut img = Self {
            image,
            mount: None,
            device: None,
            deadline,
        };
        img.attach(&dir.join(format!("{volname}-mnt")))?;
        Ok(img)
    }

    /// (Re)attach at `mount_point` (created if missing).
    pub fn attach(&mut self, mount_point: &Path) -> io::Result<()> {
        std::fs::create_dir_all(mount_point)?;
        let out = run_with_deadline(
            // `hdiutil attach` (deprecated, but it keeps working when
            // `diskutil image attach` wedges on the FSKit path).
            Command::new("/usr/bin/hdiutil")
                // -noautoopen: no Finder window (and none of its metadata writes).
                .args(["attach", "-nobrowse", "-noautoopen", "-mountpoint"])
                .arg(mount_point)
                .arg(&self.image),
            self.deadline,
        )?;
        // First column of the first line is the whole-disk device.
        self.device = out.split_whitespace().next().map(str::to_owned);
        self.mount = Some(mount_point.to_path_buf());
        Ok(())
    }

    /// Current mount point.
    pub fn mount_point(&self) -> &Path {
        self.mount.as_deref().expect("attached")
    }

    /// Detach (like ejecting a card). `force` simulates yanking it while in use.
    pub fn detach(&mut self, force: bool) -> io::Result<()> {
        let Some(dev) = self.device.take() else {
            return Ok(());
        };
        // A plain detach fails with "resource busy" while a system daemon (Spotlight,
        // fseventsd) still has the fresh volume open: retry a few times before failing
        // (the ds-ffi exFAT import test flaked on exactly that). Processes are never killed.
        let mut last = None;
        for attempt in 0..5u64 {
            let mut cmd = Command::new("/usr/bin/hdiutil");
            cmd.args(["detach", "-quiet"]);
            if force {
                cmd.arg("-force");
            }
            match run_with_deadline(cmd.arg(&dev), self.deadline) {
                Ok(_) => {
                    self.mount = None;
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
        }
        self.device = Some(dev); // still attached: Drop force-detaches it
        Err(last.unwrap_or_else(|| io::Error::other("detach failed")))
    }

    /// Whole-disk device of the attached image (e.g. `/dev/disk7`).
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = self.detach(true);
    }
}

static VERDICT: Mutex<Option<HashMap<Fs, Result<(), String>>>> = Mutex::new(None);

/// Can this process create and attach `fs` images? Probed once per file
/// system with a 64 MB image (hdiutil refuses FAT32 below ~32 MB);
/// after any timeout every file system is
/// reported unavailable, so a wedged system costs one timeout per process.
pub fn available(fs: Fs) -> Result<(), String> {
    let mut g = VERDICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    decide(g.get_or_insert_with(HashMap::new), fs, || {
        let t = tempfile_dir()?;
        let res = DiskImage::create_with_deadline(&t, fs, 64, "PROBE", probe_timeout()).map(drop);
        let _ = std::fs::remove_dir_all(&t);
        res
    })
}

/// Cached verdict for `fs`; a recorded timeout for any file system short-
/// circuits the rest without probing.
fn decide(
    cache: &mut HashMap<Fs, Result<(), String>>,
    fs: Fs,
    probe: impl FnOnce() -> io::Result<()>,
) -> Result<(), String> {
    if let Some(v) = cache.get(&fs) {
        return v.clone();
    }
    let verdict = match cache
        .values()
        .find_map(|v| v.as_ref().err().filter(|e| e.contains("did not finish")))
    {
        Some(e) => Err(e.clone()),
        None => probe().map_err(|e| e.to_string()),
    };
    cache.insert(fs, verdict.clone());
    verdict
}

fn tempfile_dir() -> io::Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("ds-testvol-probe-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// A test volume, or `None` (after printing `SKIP testvol: <reason>`) when
/// disk images are unavailable or wedged. Genuine creation errors other than
/// timeouts panic, so real bugs are not skipped.
pub fn require(dir: &Path, fs: Fs, size_mb: u32, volname: &str) -> Option<DiskImage> {
    if let Err(e) = available(fs) {
        eprintln!("SKIP testvol: {fs:?} images unavailable: {e}");
        return None;
    }
    match DiskImage::create(dir, fs, size_mb, volname) {
        Ok(img) => Some(img),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            eprintln!("SKIP testvol: {fs:?} image timed out: {e}");
            None
        }
        Err(e) => panic!("creating {fs:?} test image: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn deadline_kills_hung_commands() {
        let t0 = Instant::now();
        let e = run_with_deadline(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run_with_deadline(Command::new("/bin/echo").arg("hi"), Duration::from_secs(5)).unwrap(),
            "hi\n"
        );
        let e = run_with_deadline(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 2"]),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(e.to_string().contains("out") && e.to_string().contains("err"));
        assert!(
            run_with_deadline(&mut Command::new("/nonexistent"), Duration::from_secs(1)).is_err()
        );
        assert_eq!(command_timeout(), Duration::from_secs(60));
        assert_eq!(Fs::ExFat.hdiutil_name(), "ExFAT");
    }

    #[test]
    fn verdict_cache_short_circuits_after_a_timeout() {
        let mut cache = HashMap::new();
        let mut probes = 0;
        let timeout = || {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "x did not finish within 20s",
            ))
        };
        assert!(
            decide(&mut cache, Fs::Apfs, || {
                probes += 1;
                timeout()
            })
            .is_err()
        );
        // Other file systems inherit the timeout without probing again.
        assert!(
            decide(&mut cache, Fs::ExFat, || {
                probes += 1;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(probes, 1);
        // Cached verdicts are reused; a plain failure does not short-circuit.
        let mut cache = HashMap::new();
        assert!(decide(&mut cache, Fs::Fat32, || Err(io::Error::other("no FSKit"))).is_err());
        assert!(decide(&mut cache, Fs::Apfs, || Ok(())).is_ok());
        assert!(decide(&mut cache, Fs::Apfs, || panic!("cached")).is_ok());
    }

    #[test]
    fn fat32_image_lifecycle_and_remount() {
        let t = tempfile::tempdir().unwrap();
        let Some(mut img) = require(t.path(), Fs::Fat32, 40, "CARD") else {
            return;
        };
        let mp = img.mount_point().to_path_buf();
        assert!(crate::mount_point_of(&mp).unwrap().ends_with("CARD-mnt"));
        std::fs::write(mp.join("A.ARW"), b"x").unwrap();
        let uuid = crate::volume_uuid(&mp).expect("FAT volumes have a UUID");
        assert!(img.device().unwrap().starts_with("/dev/disk"));
        img.detach(false).unwrap();
        img.detach(false).unwrap(); // idempotent
        let mp2 = t.path().join("CARD 1");
        img.attach(&mp2).unwrap();
        assert_eq!(std::fs::read(mp2.join("A.ARW")).unwrap(), b"x");
        assert_eq!(
            crate::volume_uuid(&mp2),
            Some(uuid),
            "UUID survives a remount under another name"
        );
    }
}
