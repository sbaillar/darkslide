//! Amazon Photos backup by folder hand-off (PLAN D3 option A, D8 = originals).
//!
//! Amazon has no public Photos API, so Darkslide mirrors originals into a
//! folder that the official Amazon Photos Mac app is set to back up. This
//! module only manages what lands in that folder:
//!
//! * RAW + JPEG/HEIC originals only (Amazon ignores sidecars).
//! * APFS clone (`clonefile(2)`, via the `reflink-copy` crate, which keeps
//!   `unsafe` out of this crate) when source and folder share a volume, so
//!   the mirror costs no space; otherwise a verified copy.
//! * A manifest (BLAKE3 → mirrored path) makes the hand-off idempotent:
//!   a file already handed over is never placed again, even if it was renamed
//!   in the session or removed from the folder after upload.
//! * Files are placed atomically (temp → rename, never replacing).
//! * The chosen folder is never created: if it is gone (moved, deleted, its
//!   disk unplugged) the hand-off fails with
//!   [`ArchiveError::AmazonFolderMissing`] instead of filling a folder the
//!   Amazon app doesn't watch. Only album subfolders are created.
//! * One writer per manifest: [`AmazonBackup`] holds an `flock` on
//!   `<manifest>.lock` while open, so two processes never overwrite each
//!   other's entries (callers serialize within a process).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ds_core::FileHash;
use serde::{Deserialize, Serialize};

use crate::ArchiveError;

/// Where the official app installs.
pub const AMAZON_PHOTOS_APP: &str = "/Applications/Amazon Photos.app";

/// Is the official Amazon Photos app installed?
pub fn amazon_app_installed() -> bool {
    app_installed_at(Path::new(AMAZON_PHOTOS_APP))
}

/// Is an app bundle present at `bundle`?
pub fn app_installed_at(bundle: &Path) -> bool {
    bundle.join("Contents").is_dir()
}

const ELIGIBLE: &[&str] = &[
    "arw", "cr2", "cr3", "nef", "raf", "orf", "rw2", "dng", "pef", "srw", "jpg", "jpeg", "heic",
    "heif", "hif",
];

/// RAW or JPEG/HEIC original (what Amazon Photos stores). Sidecars are not.
pub fn is_eligible(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ELIGIBLE.contains(&e.to_ascii_lowercase().as_str()))
}

/// An original to hand off, with its verified hash from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    pub path: PathBuf,
    pub hash: FileHash,
}

const MANIFEST_VERSION: u32 = 2;
/// Files whose intents (and later results) share one manifest write.
const SAVE_CHUNK: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryState {
    /// About to place a file with this hash into `rel` (an album dir).
    Intent,
    /// Placed at `rel` (a file).
    Placed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    rel: PathBuf,
    size: u64,
    state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// hex BLAKE3 → entry (paths relative to the folder).
    entries: BTreeMap<String, Entry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// What [`AmazonBackup::mirror`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorReport {
    pub cloned: Vec<PathBuf>,
    pub copied: Vec<PathBuf>,
    /// Already handed off earlier (by hash).
    pub already_mirrored: usize,
    /// Not RAW/JPEG/HEIC.
    pub skipped_unsupported: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

/// The managed Amazon Photos hand-off folder.
#[derive(Debug)]
pub struct AmazonBackup {
    folder: PathBuf,
    manifest_path: PathBuf,
    manifest: Manifest,
    rebuilt: bool,
    writes: std::sync::atomic::AtomicUsize,
    /// Exclusive while open (other processes wait in [`Self::open`]).
    _lock: ds_fsutil::DirLock,
}

/// How long [`AmazonBackup::open`] waits for another process's hand-off.
pub const MANIFEST_LOCK_WAIT: Duration = Duration::from_secs(30);

impl AmazonBackup {
    /// Open the hand-off folder (which must exist: it is never created) and
    /// its manifest, waiting up to [`MANIFEST_LOCK_WAIT`] for another
    /// process that has it open. Keep the manifest outside the folder (e.g.
    /// Application Support) so the Amazon app never sees it.
    ///
    /// A truncated/corrupt/unknown-version manifest is set aside
    /// (`<manifest>.corrupt-<stamp>`) and **rebuilt** by hashing what is in
    /// the folder, so the mirror keeps working and never re-places files.
    /// `Intent` entries left by a crash are resolved: if a file with that
    /// hash is in the album dir it counts as placed, otherwise it is retried.
    pub fn open(
        folder: impl Into<PathBuf>,
        manifest_path: impl Into<PathBuf>,
    ) -> Result<Self, ArchiveError> {
        Self::open_waiting(folder, manifest_path, MANIFEST_LOCK_WAIT)
    }

    /// [`Self::open`] waiting at most `wait` for the manifest lock.
    pub fn open_waiting(
        folder: impl Into<PathBuf>,
        manifest_path: impl Into<PathBuf>,
        wait: Duration,
    ) -> Result<Self, ArchiveError> {
        let folder = folder.into();
        let manifest_path = manifest_path.into();
        if !folder.is_dir() {
            return Err(ArchiveError::AmazonFolderMissing(folder));
        }
        let mut lock_path = manifest_path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let deadline = Instant::now() + wait;
        let lock = loop {
            match ds_fsutil::DirLock::try_lock(&lock_path) {
                Ok(Some(l)) => break l,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    return Err(ArchiveError::Manifest(
                        "another Darkslide is handing files to Amazon Photos; try again later"
                            .into(),
                    ));
                }
                Err(e) => return Err(ArchiveError::io(&lock_path, e)),
            }
        };
        let parsed = match fs::read(&manifest_path) {
            Ok(b) => serde_json::from_slice::<Manifest>(&b)
                .ok()
                .filter(|m| m.version == MANIFEST_VERSION)
                .ok_or(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => return Err(ArchiveError::io(&manifest_path, e)),
        };
        let mut me = Self {
            folder,
            manifest_path,
            manifest: Manifest::default(),
            rebuilt: false,
            writes: std::sync::atomic::AtomicUsize::new(0),
            _lock: lock,
        };
        if let Ok(m) = parsed {
            me.manifest = m;
            me.resolve_intents();
        } else {
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
            let mut aside = me.manifest_path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{stamp}"));
            fs::rename(&me.manifest_path, &aside)
                .map_err(|e| ArchiveError::io(&me.manifest_path, e))?;
            tracing::warn!(
                ?aside,
                "amazon manifest unreadable; rebuilding from the folder"
            );
            me.rebuild().map_err(|e| ArchiveError::io(&me.folder, e))?;
            me.rebuilt = true;
        }
        me.save()?;
        Ok(me)
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// The manifest was rebuilt from the folder's contents on open.
    pub fn rebuilt(&self) -> bool {
        self.rebuilt
    }

    /// Was this content already handed off? Returns its mirrored path.
    pub fn mirrored_path(&self, hash: &FileHash) -> Option<PathBuf> {
        self.manifest
            .entries
            .get(&hash.to_hex())
            .filter(|e| e.state == EntryState::Placed)
            .map(|e| self.folder.join(&e.rel))
    }

    /// Hash every eligible file under the folder into a fresh manifest.
    fn rebuild(&mut self) -> io::Result<()> {
        self.manifest = Manifest::default();
        let mut stack = vec![self.folder.clone()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir)?.flatten() {
                let p = e.path();
                let ft = e.file_type()?;
                if ft.is_dir() {
                    stack.push(p);
                } else if ft.is_file() && is_eligible(&p) && !is_hidden(&p) {
                    let size = e.metadata()?.len();
                    let rel = p.strip_prefix(&self.folder).unwrap_or(&p).to_path_buf();
                    self.manifest.entries.insert(
                        hash_file(&p)?.to_hex(),
                        Entry {
                            rel,
                            size,
                            state: EntryState::Placed,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn resolve_intents(&mut self) {
        let intents: Vec<(String, Entry)> = self
            .manifest
            .entries
            .iter()
            .filter(|(_, e)| e.state == EntryState::Intent)
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        for (key, e) in intents {
            let dir = self.folder.join(&e.rel);
            let found = match fs::read_dir(&dir) {
                Ok(rd) => rd
                    .flatten()
                    .map(|x| x.path())
                    .filter(|p| {
                        !is_hidden(p)
                            && fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == e.size)
                    })
                    .find(|p| hash_file(p).is_ok_and(|h| h.to_hex() == key)),
                Err(_) => None,
            };
            match found {
                Some(p) => {
                    let rel = p.strip_prefix(&self.folder).unwrap_or(&p).to_path_buf();
                    self.manifest.entries.insert(
                        key,
                        Entry {
                            rel,
                            size: e.size,
                            state: EntryState::Placed,
                        },
                    );
                }
                None => {
                    self.manifest.entries.remove(&key);
                }
            }
        }
    }

    /// Mirror `originals` into `<folder>/<album>/`. Each file is placed at
    /// most once per hash across all calls. The manifest records the intent
    /// before placing and the result after, atomically each time, so a crash
    /// at any point never causes a second upload.
    pub fn mirror(
        &mut self,
        album: &str,
        originals: &[Original],
        cancel: &AtomicBool,
    ) -> Result<MirrorReport, ArchiveError> {
        self.mirror_with(album, originals, &|| cancel.load(Ordering::Relaxed))
    }

    /// [`Self::mirror`] with a cancel check (e.g. a job's cancel token),
    /// polled before each file.
    pub fn mirror_with(
        &mut self,
        album: &str,
        originals: &[Original],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<MirrorReport, ArchiveError> {
        let album = ds_template::sanitize_component(album);
        let dir = self.folder.join(&album);
        let mut report = MirrorReport::default();
        let mut next = 0;
        while next < originals.len() && !report.cancelled {
            // Record the intents for a chunk of files with one manifest write, place
            // them, then record the results with one more: a crash anywhere still
            // leaves an intent for every file that may have been placed.
            let mut chunk: Vec<(&Original, String, u64)> = Vec::new();
            while next < originals.len() && chunk.len() < SAVE_CHUNK {
                let o = &originals[next];
                if cancelled() {
                    report.cancelled = true;
                    break;
                }
                next += 1;
                if !is_eligible(&o.path) {
                    report.skipped_unsupported.push(o.path.clone());
                    continue;
                }
                let key = o.hash.to_hex();
                if self.manifest.entries.contains_key(&key) {
                    report.already_mirrored += 1;
                    continue;
                }
                let size = match fs::metadata(&o.path) {
                    Ok(m) => m.len(),
                    Err(e) => {
                        report.failed.push((o.path.clone(), e.to_string()));
                        continue;
                    }
                };
                self.manifest.entries.insert(
                    key.clone(),
                    Entry {
                        rel: PathBuf::from(&album),
                        size,
                        state: EntryState::Intent,
                    },
                );
                chunk.push((o, key, size));
            }
            if chunk.is_empty() {
                continue;
            }
            self.save()?;
            let mut stop = false;
            for (o, key, size) in chunk {
                if !stop && cancelled() {
                    stop = true;
                    report.cancelled = true;
                }
                if stop {
                    // Not placed: drop the intent.
                    self.manifest.entries.remove(&key);
                    continue;
                }
                match place(&o.path, &o.hash, &dir, true) {
                    Ok((dest, cloned)) => {
                        let rel = dest
                            .strip_prefix(&self.folder)
                            .unwrap_or(&dest)
                            .to_path_buf();
                        self.manifest.entries.insert(
                            key,
                            Entry {
                                rel,
                                size,
                                state: EntryState::Placed,
                            },
                        );
                        if cloned {
                            report.cloned.push(dest);
                        } else {
                            report.copied.push(dest);
                        }
                    }
                    Err(e) => {
                        self.manifest.entries.remove(&key);
                        report.failed.push((o.path.clone(), e.to_string()));
                    }
                }
            }
            self.save()?;
        }
        Ok(report)
    }

    /// How many times the manifest has been written since this handle opened.
    pub fn manifest_writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    fn save(&self) -> Result<(), ArchiveError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        let bytes = serde_json::to_vec(&self.manifest)
            .map_err(|e| ArchiveError::Manifest(e.to_string()))?;
        ds_fsutil::write_atomic(&self.manifest_path, &bytes)
            .map_err(|e| ArchiveError::io(&self.manifest_path, e))
    }
}

fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.as_encoded_bytes().first() == Some(&b'.'))
}

/// BLAKE3 of a file, read with the page cache bypassed (so a clone or copy
/// is verified against what is on the device).
fn hash_file(p: &Path) -> io::Result<FileHash> {
    let f = fs::File::open(p)?;
    ds_fsutil::set_nocache(&f);
    let mut h = blake3::Hasher::new();
    h.update_reader(f)?;
    Ok(FileHash(*h.finalize().as_bytes()))
}

/// Clone or copy `src` into `dir` under a free name; returns (path, cloned).
/// Either way the placed bytes are read back and must match `hash`.
/// `allow_clone = false` forces the copy path (as across volumes).
fn place(
    src: &Path,
    hash: &FileHash,
    dir: &Path,
    allow_clone: bool,
) -> io::Result<(PathBuf, bool)> {
    fs::create_dir_all(dir)?;
    let src_meta = fs::metadata(src)?;
    let name =
        ds_template::sanitize_component(&src.file_name().unwrap_or_default().to_string_lossy());
    let tmp = ds_fsutil::temp_path(dir, "tmp");

    let same_volume = fs::metadata(dir)?.dev() == src_meta.dev();
    let cloned = allow_clone && same_volume && reflink_copy::reflink(src, &tmp).is_ok();
    let res = (|| {
        if !cloned {
            let _ = fs::remove_file(&tmp);
            let mut input = fs::File::open(src)?;
            let mut out = fs::File::options()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            io::copy(&mut input, &mut out)?;
        }
        let out = fs::File::options().write(true).open(&tmp)?;
        out.set_modified(src_meta.modified()?)?;
        ds_fsutil::sync_file(&out)?;
        drop(out);
        if hash_file(&tmp)? != *hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "placed bytes do not match the original's recorded hash",
            ));
        }
        for n in 0u32.. {
            let candidate = if n == 0 {
                name.clone()
            } else {
                ds_template::with_suffix(&name, n)
            };
            let dest = dir.join(candidate);
            match ds_fsutil::rename_noreplace(&tmp, &dest) {
                Ok(()) => return Ok(dest),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        unreachable!("u32 suffixes exhausted")
    })();
    match res {
        Ok(dest) => Ok((dest, cloned)),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Amazon folder the user chose (it must exist).
    fn chosen(p: PathBuf) -> PathBuf {
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn original(dir: &Path, name: &str, body: &[u8]) -> Original {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        Original {
            path,
            hash: FileHash(*blake3::hash(body).as_bytes()),
        }
    }

    #[test]
    fn app_detection() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("Amazon Photos.app");
        assert!(!app_installed_at(&app));
        fs::create_dir_all(app.join("Contents")).unwrap();
        assert!(app_installed_at(&app));
        // Just exercise the real path; the result depends on the machine.
        let _ = amazon_app_installed();
    }

    #[test]
    fn eligibility() {
        for ok in ["a.ARW", "b.jpg", "c.HEIC", "d.dng", "e.hif"] {
            assert!(is_eligible(Path::new(ok)), "{ok}");
        }
        for no in ["a.xmp", "b.json", "c", "d.MP4"] {
            assert!(!is_eligible(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn manifest_writes_are_batched_not_per_file() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("Capture");
        fs::create_dir_all(&src).unwrap();
        let files: Vec<Original> = (0..40)
            .map(|i| original(&src, &format!("D{i}.ARW"), format!("body-{i}").as_bytes()))
            .collect();
        let manifest = t.path().join("support/m.json");
        let mut b = AmazonBackup::open(chosen(t.path().join("Amazon")), &manifest).unwrap();
        let before = b.manifest_writes();
        let r = b.mirror("Album", &files, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.cloned.len() + r.copied.len(), 40);
        assert_eq!(
            b.manifest_writes() - before,
            2,
            "one intent + one result save"
        );
        // Compact JSON, and every file is recorded as placed.
        let text = fs::read_to_string(&manifest).unwrap();
        assert!(!text.contains("\n  "), "manifest is not pretty-printed");
        drop(b);
        let b = AmazonBackup::open(chosen(t.path().join("Amazon")), &manifest).unwrap();
        assert!(files.iter().all(|f| b.mirrored_path(&f.hash).is_some()));
    }

    #[test]
    fn cancel_mid_chunk_drops_unplaced_intents() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("Capture");
        fs::create_dir_all(&src).unwrap();
        let files: Vec<Original> = (0..5)
            .map(|i| original(&src, &format!("D{i}.ARW"), format!("c-{i}").as_bytes()))
            .collect();
        let manifest = t.path().join("support/m.json");
        let mut b = AmazonBackup::open(chosen(t.path().join("Amazon")), &manifest).unwrap();
        // Cancel flips after the gather phase's 5 checks and the first placement check.
        let calls = std::cell::Cell::new(0);
        let r = b
            .mirror_with("Album", &files, &|| {
                calls.set(calls.get() + 1);
                calls.get() > 6
            })
            .unwrap();
        assert!(r.cancelled);
        let placed = r.cloned.len() + r.copied.len();
        assert_eq!(placed, 1);
        // The unplaced ones are not left as intents: a later run hands them off.
        let r = b.mirror("Album", &files, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.already_mirrored, 1);
        assert_eq!(r.cloned.len() + r.copied.len(), 4);
    }

    #[test]
    fn mirrors_once_per_hash_with_clone_or_copy() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("Capture");
        fs::create_dir_all(&src).unwrap();
        let a = original(&src, "DSC1.ARW", b"raw-one");
        let j = original(&src, "DSC1.JPG", b"jpg-one");
        let x = original(&src, "DSC1.xmp", b"<xmp/>");
        let manifest = t.path().join("support/amazon-manifest.json");
        let mut b = AmazonBackup::open(chosen(t.path().join("Amazon")), &manifest).unwrap();
        let r = b
            .mirror(
                "2026-09-25 Hike",
                &[a.clone(), j.clone(), x.clone()],
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(r.cloned.len() + r.copied.len(), 2, "{r:?}");
        assert_eq!(r.skipped_unsupported, std::slice::from_ref(&x.path));
        let dest = b.mirrored_path(&a.hash).unwrap();
        assert_eq!(dest, t.path().join("Amazon/2026-09-25 Hike/DSC1.ARW"));
        assert_eq!(fs::read(&dest).unwrap(), b"raw-one");
        assert_eq!(
            fs::metadata(&dest).unwrap().modified().unwrap(),
            fs::metadata(&a.path).unwrap().modified().unwrap()
        );
        // Same folder on the same volume (tempdir) → APFS clone on macOS.
        if cfg!(target_os = "macos") {
            assert_eq!(r.cloned.len(), 2, "expected clones on APFS: {r:?}");
        }

        // Reopen (new process): the manifest prevents a second hand-off, even
        // after the file was renamed in the session or removed from the folder.
        drop(b);
        fs::remove_file(&dest).unwrap();
        let renamed = Original {
            path: src.join("renamed.ARW"),
            hash: a.hash,
        };
        fs::rename(&a.path, &renamed.path).unwrap();
        let mut b = AmazonBackup::open(chosen(t.path().join("Amazon")), &manifest).unwrap();
        let r = b
            .mirror("2026-09-25 Hike", &[renamed, j], &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.already_mirrored, 2);
        assert!(r.cloned.is_empty() && r.copied.is_empty());
        assert_eq!(b.folder(), t.path().join("Amazon"));
    }

    #[test]
    fn name_collisions_never_overwrite() {
        let t = tempfile::tempdir().unwrap();
        let (s1, s2) = (t.path().join("s1"), t.path().join("s2"));
        fs::create_dir_all(&s1).unwrap();
        fs::create_dir_all(&s2).unwrap();
        let a = original(&s1, "DSC1.ARW", b"first");
        let b2 = original(&s2, "DSC1.ARW", b"second");
        let mut b =
            AmazonBackup::open(chosen(t.path().join("A")), t.path().join("m.json")).unwrap();
        let r = b
            .mirror("album", &[a, b2], &AtomicBool::new(false))
            .unwrap();
        let placed: Vec<_> = r.cloned.iter().chain(&r.copied).cloned().collect();
        assert_eq!(
            placed,
            [
                t.path().join("A/album/DSC1.ARW"),
                t.path().join("A/album/DSC1-1.ARW")
            ]
        );
        assert_eq!(fs::read(&placed[0]).unwrap(), b"first");
        assert_eq!(fs::read(&placed[1]).unwrap(), b"second");
        // No temp files left in the folder.
        assert_eq!(fs::read_dir(t.path().join("A/album")).unwrap().count(), 2);
    }

    #[test]
    fn forced_copy_places_verified_file_and_cleans_up_on_mismatch() {
        let t = tempfile::tempdir().unwrap();
        let o = original(t.path(), "a.ARW", b"payload");
        let dir = t.path().join("out");
        let (dest, cloned) = place(&o.path, &o.hash, &dir, false).unwrap();
        assert!(!cloned);
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        assert_eq!(
            fs::metadata(&dest).unwrap().modified().unwrap(),
            fs::metadata(&o.path).unwrap().modified().unwrap()
        );
        // Source changed since the session hashed it: nothing is placed.
        let err = place(&o.path, &FileHash([9; 32]), &dir, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp or second file left"
        );
    }

    #[test]
    fn clones_are_read_back_verified_too() {
        let t = tempfile::tempdir().unwrap();
        let o = original(t.path(), "a.JPG", b"hello");
        let dir = t.path().join("out");
        // Same volume → clone path; a wrong recorded hash is still caught.
        let err = place(&o.path, &FileHash([0; 32]), &dir, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "temp cleaned");
        let (dest, _) = place(&o.path, &o.hash, &dir, true).unwrap();
        assert_eq!(fs::read(dest).unwrap(), b"hello");
    }

    #[test]
    fn crash_between_intent_and_placed_never_uploads_twice() {
        let t = tempfile::tempdir().unwrap();
        let folder = t.path().join("A");
        let mpath = t.path().join("m.json");
        let a = original(t.path(), "a.ARW", b"placed before the crash");
        let b = original(t.path(), "b.ARW", b"never placed");
        // Crash state: both intents recorded; only `a` made it into the album.
        fs::create_dir_all(folder.join("album")).unwrap();
        fs::write(folder.join("album/a.ARW"), b"placed before the crash").unwrap();
        let intent = |o: &Original| {
            (
                o.hash.to_hex(),
                Entry {
                    rel: "album".into(),
                    size: fs::metadata(&o.path).unwrap().len(),
                    state: EntryState::Intent,
                },
            )
        };
        let m = Manifest {
            version: MANIFEST_VERSION,
            entries: [intent(&a), intent(&b)].into_iter().collect(),
        };
        fs::write(&mpath, serde_json::to_vec(&m).unwrap()).unwrap();

        let mut backup = AmazonBackup::open(&folder, &mpath).unwrap();
        assert_eq!(
            backup.mirrored_path(&a.hash),
            Some(folder.join("album/a.ARW"))
        );
        assert_eq!(backup.mirrored_path(&b.hash), None);
        let r = backup
            .mirror("album", &[a, b], &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.already_mirrored, 1);
        assert_eq!(r.cloned.len() + r.copied.len(), 1);
        assert_eq!(
            fs::read_dir(folder.join("album")).unwrap().count(),
            2,
            "no second copy of a"
        );
    }

    #[test]
    fn corrupt_manifest_is_rebuilt_from_the_folder() {
        let t = tempfile::tempdir().unwrap();
        let folder = chosen(t.path().join("A"));
        let mpath = t.path().join("m.json");
        let a = original(t.path(), "a.ARW", b"already uploaded");
        let mut backup = AmazonBackup::open(&folder, &mpath).unwrap();
        backup
            .mirror("album", std::slice::from_ref(&a), &AtomicBool::new(false))
            .unwrap();
        drop(backup);
        // Truncated by a crash / disk error.
        let full = fs::read(&mpath).unwrap();
        fs::write(&mpath, &full[..full.len() / 2]).unwrap();

        let mut backup = AmazonBackup::open(&folder, &mpath).unwrap();
        assert!(backup.rebuilt());
        let aside: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("m.json.corrupt-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "corrupt manifest kept for inspection");
        let r = backup
            .mirror("album", &[a], &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.already_mirrored, 1, "{r:?}");
        drop(backup);
        // Old-version manifests are rebuilt too.
        fs::write(&mpath, br#"{"version":1,"entries":{}}"#).unwrap();
        assert!(AmazonBackup::open(&folder, &mpath).unwrap().rebuilt());
    }

    #[test]
    fn failures_cancel_and_bad_manifest() {
        let t = tempfile::tempdir().unwrap();
        let missing = Original {
            path: t.path().join("gone.ARW"),
            hash: FileHash([1; 32]),
        };
        let mut b =
            AmazonBackup::open(chosen(t.path().join("A")), t.path().join("m.json")).unwrap();
        let r = b
            .mirror("x", std::slice::from_ref(&missing), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.failed.len(), 1);
        assert!(b.mirrored_path(&missing.hash).is_none());
        let r = b.mirror("x", &[missing], &AtomicBool::new(true)).unwrap();
        assert!(r.cancelled && r.failed.is_empty());

        fs::create_dir(t.path().join("dir.json")).unwrap();
        assert!(matches!(
            AmazonBackup::open(t.path().join("A"), t.path().join("dir.json")),
            Err(ArchiveError::Io { .. })
        ));
        let file = t.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(AmazonBackup::open(file.join("sub"), t.path().join("m2.json")).is_err());
    }

    /// a folder that is gone (moved or deleted in Finder, disk
    /// unplugged) is reported, never silently recreated.
    #[test]
    fn a_missing_folder_is_reported_and_never_created() {
        let t = tempfile::tempdir().unwrap();
        let gone = t.path().join("Amazon Photos");
        let err = AmazonBackup::open(&gone, t.path().join("m.json")).unwrap_err();
        assert!(matches!(err, ArchiveError::AmazonFolderMissing(ref p) if *p == gone));
        assert!(err.to_string().contains("Amazon Photos folder missing"));
        assert!(!gone.exists(), "never recreated");
        assert!(!t.path().join("m.json").exists(), "manifest untouched");
    }

    /// one writer per manifest across processes: a second open waits
    /// for the lock (here: gives up after the wait), and gets it once the
    /// first one is closed, seeing its entries.
    #[test]
    fn one_writer_per_manifest() {
        let t = tempfile::tempdir().unwrap();
        let folder = chosen(t.path().join("A"));
        let mpath = t.path().join("m.json");
        let o = original(t.path(), "a.ARW", b"one");
        let mut first = AmazonBackup::open(&folder, &mpath).unwrap();
        let t0 = Instant::now();
        let busy = AmazonBackup::open_waiting(&folder, &mpath, Duration::from_millis(150));
        assert!(
            matches!(busy, Err(ArchiveError::Manifest(ref m)) if m.contains("another Darkslide"))
        );
        assert!(t0.elapsed() >= Duration::from_millis(150));
        first
            .mirror_with("x", std::slice::from_ref(&o), &|| false)
            .unwrap();
        drop(first);
        let second =
            AmazonBackup::open_waiting(&folder, &mpath, Duration::from_millis(150)).unwrap();
        assert!(
            second.mirrored_path(&o.hash).is_some(),
            "sees the first writer's entry"
        );
        // A lock path that can't be created is an I/O error.
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            AmazonBackup::open(&folder, file.join("m.json")),
            Err(ArchiveError::Io { .. })
        ));
        // A cancel check stops before the next file.
        drop(second);
        let mut b = AmazonBackup::open(&folder, &mpath).unwrap();
        let p = original(t.path(), "b.ARW", b"two");
        let r = b.mirror_with("x", &[p], &|| true).unwrap();
        assert!(r.cancelled && r.cloned.is_empty() && r.copied.is_empty());
    }

    #[test]
    fn manifest_save_failure_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let o = original(t.path(), "a.ARW", b"x");
        let support = t.path().join("support");
        fs::create_dir(&support).unwrap();
        let mut b = AmazonBackup::open(chosen(t.path().join("A")), support.join("m.json")).unwrap();
        fs::set_permissions(&support, fs::Permissions::from_mode(0o555)).unwrap();
        let res = b.mirror("x", &[o], &AtomicBool::new(false));
        fs::set_permissions(&support, fs::Permissions::from_mode(0o755)).unwrap();
        if !rustix::process::geteuid().is_root() {
            assert!(matches!(res, Err(ArchiveError::Io { .. })), "{res:?}");
        }
    }
}
