//! Collision-free, atomic output files.
//!
//! Names are reserved per destination directory using the APFS-style
//! [`collision_key`] (case + Unicode normalization folded), seeded from the
//! directory's existing entries, so `a.jpg` never overwrites `A.JPG` or an
//! NFD-spelled twin. Files are written to a short hidden temp
//! ([`ds_fsutil::temp_path`], so 255-byte output names work), fsync'd, then
//! placed with [`ds_fsutil::rename_noreplace`] (which falls back to
//! check-then-rename on exFAT/FAT, where `RENAME_EXCL` and hard links are
//! unsupported); a lost race bumps the suffix. The device-wide flush
//! (`F_FULLFSYNC`) is done once per batch by the caller.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ds_template::{collision_key, with_suffix};

use crate::ExportError;

pub(crate) fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ExportError + '_ {
    move |source| ExportError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Longest file name, in bytes, the volumes we write to accept (APFS, exFAT, FAT).
const NAME_MAX: usize = 255;

/// `desired` (with `-n` before its extension when `n > 0`), its stem cut on a
/// character boundary where the name would pass [`NAME_MAX`]: a suffixed 255-byte name
/// would otherwise fail the rename with `ENAMETOOLONG`.
fn candidate(desired: &str, n: u32) -> String {
    let suffixed = |name: &str| {
        if n == 0 {
            name.to_owned()
        } else {
            with_suffix(name, n)
        }
    };
    let name = suffixed(desired);
    if name.len() <= NAME_MAX {
        return name;
    }
    let (stem, ext) = match desired.rfind('.') {
        Some(dot) if dot > 0 => desired.split_at(dot),
        _ => (desired, ""),
    };
    let keep = stem.len().saturating_sub(name.len() - NAME_MAX);
    let cut = format!("{}{ext}", crate::xmp::truncate(stem, keep));
    let name = suffixed(&cut);
    // An extension that is itself too long: cut the whole name.
    crate::xmp::truncate(&name, NAME_MAX).to_owned()
}

/// Reserves unique output names within a batch.
#[derive(Debug, Default)]
pub struct Namer {
    dirs: Mutex<HashMap<PathBuf, HashSet<String>>>,
}

impl Namer {
    /// Reserve `desired` in `dir`, or the first free `name-N.ext`.
    pub fn reserve(&self, dir: &Path, desired: &str) -> Result<String, ExportError> {
        let mut dirs = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken = match dirs.entry(dir.to_owned()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut set = HashSet::new();
                for e in fs::read_dir(dir).map_err(io_err(dir))? {
                    let e = e.map_err(io_err(dir))?;
                    set.insert(collision_key(&e.file_name().to_string_lossy()));
                }
                v.insert(set)
            }
        };
        for n in 0..100_000u32 {
            let name = candidate(desired, n);
            if taken.insert(collision_key(&name)) {
                return Ok(name);
            }
        }
        Err(ExportError::Io {
            path: dir.join(desired),
            source: io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"),
        })
    }

    /// Give back a name that was reserved but never written (the write failed), so the
    /// next file wanting it gets it instead of a `-N` variant. Touches only the
    /// namer's memory: it never lists or creates the directory.
    pub fn release(&self, dir: &Path, name: &str) {
        let mut dirs = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(taken) = dirs.get_mut(dir) {
            taken.remove(&collision_key(name));
        }
    }
}

/// Removes the temp file unless disarmed.
#[derive(Debug)]
pub(crate) struct TempGuard(pub(crate) Option<PathBuf>);

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Write a new file in `dir` named `desired` (or a free `-N` variant):
/// temp → `write` → flush → fsync → no-replace rename. Nothing is left behind on error,
/// and the reserved name is released.
/// Durability against power loss additionally needs [`ds_fsutil::flush_device`]
/// once after the last file (done by `run_batch`).
pub fn write_atomic(
    namer: &Namer,
    dir: &Path,
    desired: &str,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), ExportError>,
) -> Result<PathBuf, ExportError> {
    write_atomic_sized(namer, dir, desired, write).map(|(path, _)| path)
}

/// [`write_atomic`] that also returns the file's length, read from the file before it
/// is placed, so a failure to stat it fails the export instead of reporting 0 bytes.
pub fn write_atomic_sized(
    namer: &Namer,
    dir: &Path,
    desired: &str,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), ExportError>,
) -> Result<(PathBuf, u64), ExportError> {
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let name = namer.reserve(dir, desired)?;
    let tmp = ds_fsutil::temp_path(dir, "partial");
    let staged = (|| -> Result<(TempGuard, u64), ExportError> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        let guard = TempGuard(Some(tmp.clone()));
        let mut w = BufWriter::with_capacity(1 << 20, file);
        write(&mut w)?;
        w.flush().map_err(io_err(&tmp))?;
        let file = w.into_inner().map_err(|e| io_err(&tmp)(e.into_error()))?;
        ds_fsutil::sync_file(&file).map_err(io_err(&tmp))?;
        let bytes = file.metadata().map_err(io_err(&tmp))?.len();
        Ok((guard, bytes))
    })();
    let (mut guard, bytes) = match staged {
        Ok(staged) => staged,
        Err(e) => {
            namer.release(dir, &name);
            return Err(e);
        }
    };
    let dest = place(namer, &tmp, dir, desired, name)?;
    guard.0 = None;
    Ok((dest, bytes))
}

/// Rename the finished temp file `tmp` (in `dir`) to `name`, reserved from `desired`,
/// never replacing a file: a name taken meanwhile gets the next free `-N`.
pub(crate) fn place(
    namer: &Namer,
    tmp: &Path,
    dir: &Path,
    desired: &str,
    mut name: String,
) -> Result<PathBuf, ExportError> {
    loop {
        let dest = dir.join(&name);
        match ds_fsutil::rename_noreplace(tmp, &dest) {
            Ok(()) => return Ok(dest),
            // Someone else created it after we listed the directory.
            // Reserve again from the *desired* name: the namer already holds
            // `name`, so this yields the next free `desired-N` (never `x-1-1`).
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                name = namer.reserve(dir, desired)?;
            }
            Err(e) => {
                namer.release(dir, &name);
                return Err(io_err(&dest)(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn reserves_suffixes_against_disk_and_batch() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("IMG.JPG"), b"x").unwrap();
        let n = Namer::default();
        // Case-folded collision with the existing file.
        assert_eq!(n.reserve(d.path(), "img.jpg").unwrap(), "img-1.jpg");
        assert_eq!(n.reserve(d.path(), "img.jpg").unwrap(), "img-2.jpg");
        // NFD vs NFC spelling of the same name collide.
        assert_eq!(
            n.reserve(d.path(), "Caf\u{e9}.jpg").unwrap(),
            "Caf\u{e9}.jpg"
        );
        assert_eq!(
            n.reserve(d.path(), "Cafe\u{301}.jpg").unwrap(),
            "Cafe\u{301}-1.jpg"
        );
        assert!(matches!(
            n.reserve(&d.path().join("missing"), "a"),
            Err(ExportError::Io { .. })
        ));
    }

    #[test]
    fn writes_atomically_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("sub");
        let n = Namer::default();
        let p1 = write_atomic(&n, &out, "a.jpg", |w| {
            w.write_all(b"one").map_err(io_err(Path::new("")))
        })
        .unwrap();
        // An existing file (seen when the namer lists the dir) is never replaced.
        fs::write(out.join("b.jpg"), b"theirs").unwrap();
        let n2 = Namer::default();
        let p2 = write_atomic(&n2, &out, "b.jpg", |w| {
            w.write_all(b"two").map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!(p1, out.join("a.jpg"));
        assert_eq!(p2, out.join("b-1.jpg"));
        assert_eq!(fs::read(out.join("b.jpg")).unwrap(), b"theirs");
        assert_eq!(fs::read(&p2).unwrap(), b"two");
        assert_eq!(names(&out), ["a.jpg", "b-1.jpg", "b.jpg"]);
    }

    #[test]
    fn lost_race_bumps_suffix() {
        let d = tempfile::tempdir().unwrap();
        let n = Namer::default();
        n.reserve(d.path(), "seed").unwrap(); // namer lists the (empty) dir now
        fs::write(d.path().join("c.jpg"), b"late arrival").unwrap();
        let p = write_atomic(&n, d.path(), "c.jpg", |w| {
            w.write_all(b"ours").map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!(p, d.path().join("c-1.jpg"));
        assert_eq!(fs::read(d.path().join("c.jpg")).unwrap(), b"late arrival");
        assert_eq!(fs::read(&p).unwrap(), b"ours");
    }

    /// Losing the race on an already-suffixed name bumps the number, it
    /// never compounds suffixes (`IMG-1-1.jpg`).
    #[test]
    fn repeated_lost_races_never_compound_suffixes() {
        let d = tempfile::tempdir().unwrap();
        let n = Namer::default();
        n.reserve(d.path(), "seed").unwrap();
        // Two other writers take IMG.jpg and IMG-1.jpg behind the namer's back.
        fs::write(d.path().join("IMG.jpg"), b"a").unwrap();
        fs::write(d.path().join("IMG-1.jpg"), b"b").unwrap();
        let p = write_atomic(&n, d.path(), "IMG.jpg", |w| {
            w.write_all(b"ours").map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!(p, d.path().join("IMG-2.jpg"));
        assert_eq!(names(d.path()), ["IMG-1.jpg", "IMG-2.jpg", "IMG.jpg"]);
    }

    #[test]
    fn failed_write_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let n = Namer::default();
        let r = write_atomic(&n, d.path(), "x.tif", |w| {
            w.write_all(&[0; 10_000]).unwrap();
            Err(ExportError::Io {
                path: "x".into(),
                source: io::Error::new(io::ErrorKind::StorageFull, "disk full"),
            })
        });
        assert!(r.unwrap_err().to_string().contains("disk full"));
        assert_eq!(names(d.path()).len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_destination_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let ro = d.path().join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let n = Namer::default();
        let r = write_atomic(&n, &ro, "a.jpg", |_| Ok(()));
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(r, Err(ExportError::Io { .. })), "{r:?}");
        // Destination is a file, not a directory.
        let f = d.path().join("file");
        fs::write(&f, b"").unwrap();
        assert!(write_atomic(&n, &f, "a.jpg", |_| Ok(())).is_err());
    }

    #[test]
    fn max_length_output_name_works() {
        // The temp name must not be derived from the final name (255-byte limit).
        let d = tempfile::tempdir().unwrap();
        let name = format!("{}.jpg", "a".repeat(250));
        let n = Namer::default();
        let p = write_atomic(&n, d.path(), &name, |w| {
            w.write_all(b"x").map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!(p.file_name().unwrap().len(), 254);
        assert_eq!(names(d.path()), [name]);
    }

    /// A 255-byte name plus a `-N` suffix passes the filesystem's limit: the stem is
    /// cut so the second file still gets a name instead of failing with ENAMETOOLONG.
    #[test]
    fn suffixed_names_stay_within_the_name_limit() {
        let d = tempfile::tempdir().unwrap();
        let full = format!("{}.jpg", "a".repeat(251));
        assert_eq!(full.len(), NAME_MAX);
        let n = Namer::default();
        let w = |b: &'static [u8]| {
            move |w: &mut BufWriter<File>| w.write_all(b).map_err(io_err(Path::new("")))
        };
        let first = write_atomic(&n, d.path(), &full, w(b"1")).unwrap();
        let second = write_atomic(&n, d.path(), &full, w(b"2")).unwrap();
        let third = write_atomic(&n, d.path(), &full, w(b"3")).unwrap();
        assert_eq!(first.file_name().unwrap().len(), NAME_MAX);
        for p in [&second, &third] {
            let name = p.file_name().unwrap().to_str().unwrap();
            assert!(
                name.len() <= NAME_MAX && p.extension() == Some("jpg".as_ref()),
                "{name}"
            );
        }
        assert!(
            second
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-1.jpg")
        );
        assert!(
            third
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-2.jpg")
        );
        assert_eq!(fs::read(&second).unwrap(), b"2");
        // A name already too long, with multi-byte characters, is cut on a boundary.
        let long = format!("{}.tif", "é".repeat(200));
        let p = write_atomic(&n, d.path(), &long, w(b"4")).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() <= NAME_MAX && p.extension() == Some("tif".as_ref()),
            "{}",
            name.len()
        );
        // An extension too long to keep is cut with the rest.
        assert!(candidate(&format!("a.{}", "e".repeat(300)), 0).len() <= NAME_MAX);
    }

    /// A write that fails gives its name back: the next file gets `a.jpg`, not
    /// `a-1.jpg`. Releasing is memory only, so a destination that has since been removed
    /// is not recreated.
    #[test]
    fn a_failed_write_releases_its_name() {
        let d = tempfile::tempdir().unwrap();
        let n = Namer::default();
        let r = write_atomic(&n, d.path(), "a.jpg", |_| {
            Err(ExportError::Encode("boom".into()))
        });
        assert!(r.is_err());
        let p = write_atomic(&n, d.path(), "a.jpg", |w| {
            w.write_all(b"ok").map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!(p, d.path().join("a.jpg"));
        // Releasing is memory only: a destination removed since is not recreated.
        let gone = d.path().join("gone");
        fs::create_dir(&gone).unwrap();
        n.reserve(&gone, "x.jpg").unwrap();
        fs::remove_dir(&gone).unwrap();
        n.release(&gone, "x.jpg");
        n.release(&d.path().join("never-seen"), "x.jpg");
        assert!(!gone.exists() && !d.path().join("never-seen").exists());
        // Released: reserved again without a suffix.
        fs::create_dir(&gone).unwrap();
        assert_eq!(n.reserve(&gone, "x.jpg").unwrap(), "x.jpg");
    }

    #[test]
    fn the_length_is_that_of_the_placed_file() {
        let d = tempfile::tempdir().unwrap();
        let (p, bytes) = write_atomic_sized(&Namer::default(), d.path(), "a.bin", |w| {
            w.write_all(&[7; 3000]).map_err(io_err(Path::new("")))
        })
        .unwrap();
        assert_eq!((bytes, fs::metadata(p).unwrap().len()), (3000, 3000));
    }

    /// Real FAT32/exFAT volumes: no `RENAME_EXCL` on exFAT (ENOTSUP) and no
    /// hard links on either. Exports (and collisions) must still work.
    #[cfg(target_os = "macos")]
    #[test]
    fn exports_to_exfat_and_fat32_with_collisions() {
        use ds_fsutil::testvol::{DiskImage, Fs};
        let t = tempfile::tempdir().unwrap();
        for (fs_kind, vol) in [(Fs::ExFat, "EXP"), (Fs::Fat32, "FATEXP")] {
            let img = DiskImage::create(t.path(), fs_kind, 64, vol).unwrap();
            let out = img.mount_point().join("Output");
            let n = Namer::default();
            let w = |b: &'static [u8]| {
                move |w: &mut BufWriter<File>| w.write_all(b).map_err(io_err(Path::new("")))
            };
            let p1 = write_atomic(&n, &out, "DSC1.jpg", w(b"one")).unwrap();
            let p2 = write_atomic(&n, &out, "dsc1.JPG", w(b"two")).unwrap();
            // A file created behind the namer's back still is not replaced.
            fs::write(out.join("late.jpg"), b"theirs").unwrap();
            let n2 = Namer::default();
            n2.reserve(&out, "seed").unwrap();
            fs::write(out.join("later.jpg"), b"theirs too").unwrap();
            let p3 = write_atomic(&n2, &out, "later.jpg", w(b"three")).unwrap();
            assert_eq!(p1, out.join("DSC1.jpg"), "{vol}");
            assert_eq!(p2, out.join("dsc1-1.JPG"), "{vol}");
            assert_eq!(p3, out.join("later-1.jpg"), "{vol}");
            assert_eq!(fs::read(&p1).unwrap(), b"one");
            assert_eq!(fs::read(&p2).unwrap(), b"two");
            assert_eq!(fs::read(out.join("later.jpg")).unwrap(), b"theirs too");
            assert!(
                names(&out).iter().all(|n| !n.starts_with(".ds-")),
                "no temp left: {:?}",
                names(&out)
            );
            ds_fsutil::flush_device(&out).unwrap();
        }
    }
}
