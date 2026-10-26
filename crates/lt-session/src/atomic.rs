//! Crash-safe file replacement: temp file in the same directory → fsync →
//! rename over the target → fsync the directory.
//!
//! On macOS `fsync(2)` only hands data to the drive; `F_FULLFSYNC` also
//! flushes the drive cache and costs ~4 ms. Batches therefore stage every
//! temp file with a plain `fsync`, then issue **one** `F_FULLFSYNC` barrier
//! before any rename. Every rename thus points at data already on stable
//! storage, so a sidecar is never torn or empty after a crash or power cut.
//! The directory is then `fsync`ed (without a drive flush): at worst a power
//! cut in the next moments loses the newest edit and leaves the previous
//! complete sidecar.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Points in [`write_many`] where a fault can be injected in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    /// The temp file is written and durable; the target is untouched.
    TempWritten,
}

/// Fault-injection seam: called at each [`WriteStage`]; an `Err` aborts that write.
pub type WriteHook = Arc<dyn Fn(&Path, WriteStage) -> io::Result<()> + Send + Sync>;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Marker in temp file names; [`is_temp_name`] recognizes leftovers from a crash.
const TEMP_MARK: &str = ".lttmp-";

/// Batches at least this large stage their temp files on several threads.
const PARALLEL_MIN: usize = 16;

/// Read a JSON file that may come from someone else's folder: a regular
/// file of at most [`lt_core::limits::MAX_JSON_BYTES`] (see
/// `lt_fsutil::read_regular_capped`). Links to such files are followed, as
/// they are everywhere else in a session.
pub(crate) fn read_json_file(path: &Path) -> io::Result<Vec<u8>> {
    lt_fsutil::read_regular_capped(path, lt_core::limits::MAX_JSON_BYTES, true)
}

pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.')
        && (name.contains(TEMP_MARK) || name.contains(crate::legacy::LEGACY_TEMP_MARK))
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Hand a file's data to the device (no drive-cache flush on macOS).
fn push(f: &File) -> io::Result<()> {
    rustix::fs::fsync(f).map_err(io::Error::from)
}

/// Make everything pushed so far reach stable storage.
fn barrier(f: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return rustix::fs::fcntl_fullfsync(f).map_err(io::Error::from);
    #[cfg(not(target_vendor = "apple"))]
    return push(f);
}

/// Write `bytes` to a fresh temp file next to `dst` and push it to the device.
fn stage(dst: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent_of(dst).join(format!(
        ".{name}{TEMP_MARK}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let res = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        push(&f)
    })();
    match res {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_many(&[(path.to_owned(), bytes.to_vec())], None)
        .pop()
        .expect("one result")
}

/// Atomically replace several files, returning one result per item (in
/// order). A failure affects only its own item unless the barrier fails, in
/// which case nothing is renamed.
pub fn write_many(items: &[(PathBuf, Vec<u8>)], hook: Option<&WriteHook>) -> Vec<io::Result<()>> {
    let staged: Vec<io::Result<PathBuf>> = if items.len() < PARALLEL_MIN {
        items.iter().map(|(dst, bytes)| stage(dst, bytes)).collect()
    } else {
        let threads = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .min(8);
        let chunk = items.len().div_ceil(threads);
        std::thread::scope(|s| {
            let handles: Vec<_> = items
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || {
                        c.iter()
                            .map(|(dst, bytes)| stage(dst, bytes))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("stager panicked"))
                .collect()
        })
    };

    // One drive flush makes every staged temp durable before any rename.
    let first_ok = items
        .iter()
        .zip(&staged)
        .find(|(_, s)| s.is_ok())
        .map(|((dst, _), _)| parent_of(dst));
    if let Some(dir) = first_ok
        && let Err(e) = File::open(dir).and_then(|d| barrier(&d))
    {
        for tmp in staged.iter().flatten() {
            let _ = fs::remove_file(tmp);
        }
        let kind = e.kind();
        let msg = e.to_string();
        return staged
            .into_iter()
            .map(|_| Err(io::Error::new(kind, msg.clone())))
            .collect();
    }

    let mut dirs = BTreeSet::new();
    let results: Vec<io::Result<()>> = items
        .iter()
        .zip(staged)
        .map(|((dst, _), tmp)| {
            let tmp = tmp?;
            let res = hook
                .map_or(Ok(()), |h| h(dst, WriteStage::TempWritten))
                .and_then(|()| fs::rename(&tmp, dst));
            if res.is_err() {
                let _ = fs::remove_file(&tmp);
            } else {
                dirs.insert(parent_of(dst).to_owned());
            }
            res
        })
        .collect();
    for dir in dirs {
        // Best effort: the rename itself already happened; see the module docs.
        let _ = File::open(&dir).and_then(|d| push(&d));
    }
    results
}

/// fsync a directory with a full drive flush (used where durability of the
/// rename itself matters: settings files, snapshots).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    barrier(&File::open(dir)?)
}

/// Remove temp files left in `dir` by a crashed writer. Only files older than
/// `min_age` are touched, so a concurrent writer's in-flight temp survives.
/// Returns how many were removed.
pub fn remove_stale_temps(dir: &Path, min_age: Duration) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    rd.filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(is_temp_name))
        .filter(|e| {
            let age = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| now.duration_since(t).unwrap_or_default());
            age.is_ok_and(|a| a >= min_age)
        })
        .filter(|e| fs::remove_file(e.path()).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
        sync_dir(d.path()).unwrap();
    }

    #[test]
    fn crash_after_temp_write_keeps_old_content() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        write_atomic(&p, b"old").unwrap();
        let hook: WriteHook = Arc::new(|_, stage| {
            assert_eq!(stage, WriteStage::TempWritten);
            Err(io::Error::other("simulated crash"))
        });
        let e = write_many(&[(p.clone(), b"new".to_vec())], Some(&hook))
            .pop()
            .unwrap()
            .unwrap_err();
        assert_eq!(e.to_string(), "simulated crash");
        assert_eq!(fs::read(&p).unwrap(), b"old");
        assert_eq!(
            fs::read_dir(d.path()).unwrap().count(),
            1,
            "temp cleaned up"
        );
    }

    #[test]
    fn batch_results_are_per_item_and_in_order() {
        let d = tempfile::tempdir().unwrap();
        let mut items: Vec<(PathBuf, Vec<u8>)> = (0..40)
            .map(|i| (d.path().join(format!("{i}.json")), vec![i as u8; 10]))
            .collect();
        items[7].0 = d.path().join("missing-dir/7.json");
        items[30].0 = d.path().join("missing-dir/30.json");
        let res = write_many(&items, None);
        assert_eq!(res.len(), 40);
        for (i, r) in res.iter().enumerate() {
            assert_eq!(r.is_err(), i == 7 || i == 30, "item {i}");
        }
        assert_eq!(fs::read(d.path().join("39.json")).unwrap(), vec![39u8; 10]);
        assert_eq!(
            fs::read_dir(d.path()).unwrap().count(),
            38,
            "no temp debris"
        );
        assert!(write_many(&[], None).is_empty());
    }

    #[test]
    fn stale_temps_are_recognized_and_removed() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".a.json.lttmp-1-2"), b"partial").unwrap();
        fs::write(d.path().join("a.json"), b"{}").unwrap();
        fs::write(d.path().join(".other"), b"x").unwrap();
        assert_eq!(
            remove_stale_temps(d.path(), Duration::from_secs(3600)),
            0,
            "too young"
        );
        assert_eq!(remove_stale_temps(d.path(), Duration::ZERO), 1);
        assert_eq!(
            remove_stale_temps(&d.path().join("missing"), Duration::ZERO),
            0
        );
        assert!(d.path().join("a.json").exists());
        assert!(d.path().join(".other").exists());
        assert!(!is_temp_name("a.lttmp-1"));
    }

    #[test]
    fn missing_directory_errors() {
        let d = tempfile::tempdir().unwrap();
        assert!(write_atomic(&d.path().join("nope/a.json"), b"x").is_err());
    }

    #[test]
    fn concurrent_writers_never_tear() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.json");
        std::thread::scope(|s| {
            for t in 0..8u8 {
                let p = &p;
                s.spawn(move || {
                    for _ in 0..20 {
                        write_atomic(p, &[t; 4096]).unwrap();
                    }
                });
            }
        });
        let bytes = fs::read(&p).unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(
            bytes.iter().all(|b| *b == bytes[0]),
            "content from a single writer"
        );
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }
}
