//! The files a photo server reads from the archive: developed copies and XMP
//! sidecars, written into the archived folder (an imported session's root, or a
//! folder opened in place) so the archive run carries them to the NAS like the
//! rest of the session.
//!
//! - **Developed copies** (`Developed/<original's path>.jpg`, [`developed_rel`])
//!   exist only for the photos the user chose. Which ones, and what each copy
//!   was rendered from (a [`fingerprint`] of its edits and metadata), is kept in
//!   the session's store ([`STATE_REL`]); a copy whose fingerprint no longer
//!   matches is out of date and rendered again. Unmarking a photo leaves its
//!   copy where it is (nothing here deletes a file).
//! - **XMP sidecars** (`IMG_0001.ARW.xmp`, [`sidecar_path`]) are written only
//!   where there is none, or over one Darkslide wrote itself and nobody changed
//!   since ([`write_sidecar`]): a Lightroom or Capture One sidecar is never
//!   touched.
//!
//! Every write is atomic (temp file → fsync → rename).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The publish state, in the session's store.
pub const STATE_REL: &str = ".ds/immich.json";

/// Folder of the developed copies, at the top of the archived folder (the same
/// name `ds-session` skips when it walks a folder opened in place).
pub const DEVELOPED_DIR: &str = "Developed";

/// Marks [`DEVELOPED_DIR`] as Darkslide's (`ds-session` only skips a marked one).
pub const DEVELOPED_MARKER: &str = ".darkslide-developed";

/// Extension of the developed copies.
pub const DEVELOPED_EXT: &str = "jpg";

/// Largest sidecar read back to check whose it is.
const SIDECAR_READ_MAX: u64 = 8 << 20;

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// A `Developed` folder that isn't Darkslide's (no marker, not empty).
    #[error(
        "{} already exists and isn't Darkslide's; rename it to make developed copies here",
        .0.display()
    )]
    NotOurs(PathBuf),
    /// The developed copy's path holds a file Darkslide didn't write.
    #[error(
        "{0} already exists and isn't a developed copy Darkslide made; move it to update this one"
    )]
    Taken(String),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> PublishError + '_ {
    move |source| PublishError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// One photo (variant) marked for a developed copy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Developed {
    /// The copy, relative to the archived folder (`Developed/Capture/IMG_1.jpg`),
    /// once rendered.
    pub file: Option<String>,
    /// [`fingerprint`] of what the copy was rendered from.
    pub fingerprint: Option<String>,
    /// When it was rendered, ms since the Unix epoch.
    pub rendered_ms: Option<i64>,
}

/// A sidecar Darkslide wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidecarRecord {
    /// BLAKE3 (hex) of the bytes written: a sidecar that changed since is
    /// someone else's now and left alone.
    pub hash: String,
}

/// What a session publishes (`.ds/immich.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PublishState {
    /// Photos marked for a developed copy, by variant id.
    pub developed: BTreeMap<i64, Developed>,
    /// Sidecars Darkslide wrote, by path relative to the archived folder.
    pub sidecars: BTreeMap<String, SidecarRecord>,
}

/// What [`PublishState::mark`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marked {
    /// Photos whose mark changed.
    pub changed: usize,
    /// Developed copies of unmarked photos, left in place (relative paths).
    pub kept: Vec<String>,
}

impl PublishState {
    /// The state in `store` (none yet: empty). A file that can't be parsed is
    /// set aside as `immich.json.corrupt` (never lost) and the state starts over.
    pub fn load(store: &Path) -> Result<Self, PublishError> {
        let path = store.join(STATE_REL);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(io_at(&path)(e)),
        };
        if let Ok(s) = serde_json::from_slice(&bytes) {
            return Ok(s);
        }
        let aside = path.with_extension("json.corrupt");
        fs::rename(&path, &aside).map_err(io_at(&path))?;
        Ok(Self::default())
    }

    /// Save to `store` atomically.
    pub fn save(&self, store: &Path) -> Result<(), PublishError> {
        let path = store.join(STATE_REL);
        let bytes = serde_json::to_vec_pretty(self).expect("plain data serializes");
        ds_fsutil::write_atomic(&path, &bytes).map_err(io_at(&path))
    }

    pub fn is_marked(&self, variant: i64) -> bool {
        self.developed.contains_key(&variant)
    }

    /// Mark (`on`) or unmark `variants`. Unmarking forgets the record but leaves
    /// the copy on disk (and in the archive); their paths are returned.
    pub fn mark(&mut self, variants: &[i64], on: bool) -> Marked {
        let mut out = Marked::default();
        for v in variants {
            if on {
                if !self.developed.contains_key(v) {
                    self.developed.insert(*v, Developed::default());
                    out.changed += 1;
                }
            } else if let Some(d) = self.developed.remove(v) {
                out.changed += 1;
                out.kept.extend(d.file);
            }
        }
        out
    }

    /// Whether `rel` (relative to the archived folder) is a copy this state made.
    pub fn owns_copy(&self, rel: &str) -> bool {
        self.developed
            .values()
            .any(|d| d.file.as_deref() == Some(rel))
    }

    /// Record `variant`'s copy as rendered at `file` from `fingerprint`.
    pub fn rendered(&mut self, variant: i64, file: &str, fingerprint: &str, now_ms: i64) {
        let d = self.developed.entry(variant).or_default();
        d.file = Some(file.to_owned());
        d.fingerprint = Some(fingerprint.to_owned());
        d.rendered_ms = Some(now_ms);
    }
}

/// Whether `d`'s copy is out of date for the `current` fingerprint (never
/// rendered counts as out of date).
pub fn is_stale(d: &Developed, current: &str) -> bool {
    d.file.is_none() || d.fingerprint.as_deref() != Some(current)
}

/// BLAKE3 (hex) of `bytes`.
pub fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A fingerprint of everything a copy is rendered from (each part
/// length-prefixed, so parts can't run into each other).
pub fn fingerprint(parts: &[&[u8]]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// Where the developed copy of an original goes, relative to the archived
/// folder: its relative path (`/`-separated) under [`DEVELOPED_DIR`], named
/// after its stem. When another original in the same folder (`siblings`, file
/// names) shares the stem (`IMG_1.ARW` and `IMG_1.JPG`), the extension is added
/// (`IMG_1_ARW.jpg`, `IMG_1_JPG.jpg`); a second variant gets `_v2`, and so on.
pub fn developed_rel(original_rel: &str, siblings: &[String], ordinal: u32) -> String {
    let (dir, file) = original_rel
        .rsplit_once('/')
        .map_or(("", original_rel), |(d, f)| (d, f));
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, e),
        _ => (file, ""),
    };
    let shared = siblings.iter().any(|s| {
        s != file
            && s.rsplit_once('.')
                .map_or(s.as_str(), |(st, _)| st)
                .eq_ignore_ascii_case(stem)
    });
    let mut name = stem.to_owned();
    if shared && !ext.is_empty() {
        name.push('_');
        name.push_str(&ext.to_ascii_uppercase());
    }
    if ordinal > 1 {
        name.push_str("_v");
        name.push_str(&ordinal.to_string());
    }
    let mut out = String::from(DEVELOPED_DIR);
    if !dir.is_empty() {
        out.push('/');
        out.push_str(dir);
    }
    out.push('/');
    out.push_str(&name);
    out.push('.');
    out.push_str(DEVELOPED_EXT);
    out
}

/// `<root>/Developed`, created with its marker. A `Developed` folder that isn't
/// Darkslide's is refused unless it is empty (then it is marked).
pub fn prepare_developed_dir(root: &Path) -> Result<PathBuf, PublishError> {
    let dir = root.join(DEVELOPED_DIR);
    let marker = dir.join(DEVELOPED_MARKER);
    if marker.is_file() {
        return Ok(dir);
    }
    match fs::read_dir(&dir) {
        Ok(mut rd) => {
            if rd.next().is_some() {
                return Err(PublishError::NotOurs(dir));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&dir).map_err(io_at(&dir))?;
        }
        Err(e) => return Err(io_at(&dir)(e)),
    }
    ds_fsutil::write_atomic(
        &marker,
        b"Developed copies made by Darkslide for a photo server.\n",
    )
    .map_err(io_at(&marker))?;
    Ok(dir)
}

/// Move the rendered file `staged` to `<root>/<rel>` (replacing the previous
/// copy). `owned`: the path is a copy Darkslide made ([`PublishState::owns_copy`]);
/// any other file there is never replaced.
pub fn place_developed(
    root: &Path,
    rel: &str,
    staged: &Path,
    owned: bool,
) -> Result<PathBuf, PublishError> {
    prepare_developed_dir(root)?;
    let dest = root.join(rel);
    if !owned && fs::symlink_metadata(&dest).is_ok() {
        return Err(PublishError::Taken(rel.to_owned()));
    }
    let parent = dest.parent().unwrap_or(root);
    fs::create_dir_all(parent).map_err(io_at(parent))?;
    match fs::rename(staged, &dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            // Another volume: copy next to the destination, then rename over it.
            let tmp = ds_fsutil::temp_path(parent, "tmp");
            let res = fs::copy(staged, &tmp)
                .and_then(|_| fs::File::open(&tmp)?.sync_all())
                .and_then(|()| fs::rename(&tmp, &dest));
            if let Err(e) = res {
                let _ = fs::remove_file(&tmp);
                return Err(io_at(&dest)(e));
            }
            let _ = fs::remove_file(staged);
        }
        Err(e) => return Err(io_at(&dest)(e)),
    }
    Ok(dest)
}

/// The sidecar of an original: its file name plus `.xmp` (`IMG_0001.ARW.xmp`,
/// the form Immich looks for first).
pub fn sidecar_path(original: &Path) -> PathBuf {
    let mut name = original.file_name().unwrap_or_default().to_os_string();
    name.push(".xmp");
    original.with_file_name(name)
}

/// What [`write_sidecar`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarOutcome {
    /// Written; BLAKE3 of the bytes (record it).
    Written(String),
    /// Already exactly these bytes.
    Unchanged(String),
    /// Someone else's sidecar (or one changed since Darkslide wrote it): left alone.
    Foreign,
}

/// Write the sidecar `bytes` at `path` unless a sidecar there isn't
/// Darkslide's: `is_ours` recognises Darkslide's marker, and `recorded` is the
/// hash of what Darkslide last wrote there (a file that changed since is left
/// alone). Symlinks and non-files are never written through.
pub fn write_sidecar(
    path: &Path,
    bytes: &[u8],
    recorded: Option<&str>,
    is_ours: impl Fn(&[u8]) -> bool,
) -> Result<SidecarOutcome, PublishError> {
    let hash = hash_hex(bytes);
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_at(path)(e)),
        Ok(md) if !md.file_type().is_file() => return Ok(SidecarOutcome::Foreign),
        Ok(_) => {
            let existing = match ds_fsutil::read_regular_capped(path, SIDECAR_READ_MAX, false) {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    return Ok(SidecarOutcome::Foreign);
                }
                Err(e) => return Err(io_at(path)(e)),
            };
            if existing == bytes {
                return Ok(SidecarOutcome::Unchanged(hash));
            }
            let unchanged_since = recorded.is_none_or(|h| h == hash_hex(&existing));
            if !(is_ours(&existing) && unchanged_since) {
                return Ok(SidecarOutcome::Foreign);
            }
        }
    }
    ds_fsutil::write_atomic(path, bytes).map_err(io_at(path))?;
    Ok(SidecarOutcome::Written(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours(b: &[u8]) -> bool {
        b.starts_with(b"OURS")
    }

    #[test]
    fn developed_names_mirror_the_original() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            developed_rel("Capture/Day 1/IMG_1.ARW", &none, 1),
            "Developed/Capture/Day 1/IMG_1.jpg"
        );
        assert_eq!(developed_rel("IMG_2.NEF", &none, 1), "Developed/IMG_2.jpg");
        assert_eq!(
            developed_rel("Selects/IMG_1.ARW", &none, 3),
            "Developed/Selects/IMG_1_v3.jpg"
        );
        // Two originals sharing a stem get their extension, case-insensitively.
        let sib: Vec<String> = vec!["IMG_1.ARW".into(), "img_1.jpg".into(), "IMG_10.ARW".into()];
        assert_eq!(
            developed_rel("Capture/IMG_1.ARW", &sib, 1),
            "Developed/Capture/IMG_1_ARW.jpg"
        );
        assert_eq!(
            developed_rel("Capture/img_1.jpg", &sib, 2),
            "Developed/Capture/img_1_JPG_v2.jpg"
        );
        assert_eq!(
            developed_rel("Capture/IMG_10.ARW", &sib, 1),
            "Developed/Capture/IMG_10.jpg",
            "a longer stem is not a collision"
        );
        // No extension, a dot file.
        assert_eq!(
            developed_rel("Capture/RAW", &none, 1),
            "Developed/Capture/RAW.jpg"
        );
        assert_eq!(
            developed_rel("Capture/.hidden", &none, 1),
            "Developed/Capture/.hidden.jpg"
        );
    }

    #[test]
    fn fingerprints_change_with_any_part() {
        let a = fingerprint(&[b"ab", b"c"]);
        assert_eq!(a.len(), 32);
        assert_eq!(a, fingerprint(&[b"ab", b"c"]));
        assert_ne!(a, fingerprint(&[b"a", b"bc"]), "parts are length-prefixed");
        assert_ne!(a, fingerprint(&[b"ab", b"d"]));
        assert_eq!(hash_hex(b"x").len(), 64);
    }

    #[test]
    fn marks_and_unmarks_keep_files() {
        let mut s = PublishState::default();
        assert_eq!(s.mark(&[1, 2, 2], true).changed, 2);
        assert_eq!(s.mark(&[1], true).changed, 0, "already marked");
        assert!(s.is_marked(1) && !s.is_marked(3));
        s.rendered(1, "Developed/Capture/a.jpg", "f1", 5);
        assert!(s.owns_copy("Developed/Capture/a.jpg"));
        assert!(!s.owns_copy("Developed/Capture/b.jpg"));
        assert!(!is_stale(&s.developed[&1], "f1"));
        assert!(is_stale(&s.developed[&1], "f2"));
        assert!(is_stale(&s.developed[&2], "f1"), "never rendered");
        let m = s.mark(&[1, 2, 9], false);
        assert_eq!(m.changed, 2);
        assert_eq!(m.kept, ["Developed/Capture/a.jpg"]);
        assert!(s.developed.is_empty());
    }

    #[test]
    fn state_round_trips_and_survives_corruption() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            PublishState::load(t.path()).unwrap(),
            PublishState::default()
        );
        let mut s = PublishState::default();
        s.mark(&[7], true);
        s.rendered(7, "Developed/x.jpg", "fp", 1);
        s.sidecars.insert(
            "Capture/x.ARW.xmp".into(),
            SidecarRecord { hash: "h".into() },
        );
        s.save(t.path()).unwrap();
        assert_eq!(PublishState::load(t.path()).unwrap(), s);
        let path = t.path().join(STATE_REL);
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            PublishState::load(t.path()).unwrap(),
            PublishState::default()
        );
        assert_eq!(
            fs::read(t.path().join(".ds/immich.json.corrupt")).unwrap(),
            b"{ not json",
            "set aside, not lost"
        );
        // A directory where the file should be is an error, not an empty state.
        fs::create_dir_all(&path).unwrap();
        assert!(matches!(
            PublishState::load(t.path()),
            Err(PublishError::Io { .. })
        ));
        assert!(s.save(t.path()).is_err());
    }

    #[test]
    fn developed_dir_is_marked_and_a_foreign_one_refused() {
        let t = tempfile::tempdir().unwrap();
        let dir = prepare_developed_dir(t.path()).unwrap();
        assert!(dir.join(DEVELOPED_MARKER).is_file());
        assert_eq!(prepare_developed_dir(t.path()).unwrap(), dir, "idempotent");
        // The user's own non-empty Developed folder: refused, untouched.
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(other.path().join("Developed")).unwrap();
        fs::write(other.path().join("Developed/mine.jpg"), b"m").unwrap();
        let e = prepare_developed_dir(other.path()).unwrap_err();
        assert!(matches!(e, PublishError::NotOurs(_)));
        assert!(e.to_string().contains("isn't Darkslide's"), "{e}");
        assert!(
            !other
                .path()
                .join("Developed")
                .join(DEVELOPED_MARKER)
                .exists()
        );
        // An empty one is taken over.
        let empty = tempfile::tempdir().unwrap();
        fs::create_dir_all(empty.path().join("Developed")).unwrap();
        assert!(prepare_developed_dir(empty.path()).is_ok());
        // A file named Developed can't be used.
        let file = tempfile::tempdir().unwrap();
        fs::write(file.path().join("Developed"), b"x").unwrap();
        assert!(matches!(
            prepare_developed_dir(file.path()),
            Err(PublishError::Io { .. })
        ));
    }

    #[test]
    fn placing_replaces_only_own_copies() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("session");
        let staged = t.path().join("staged.jpg");
        fs::write(&staged, b"v1").unwrap();
        let rel = "Developed/Capture/Day 1/IMG_1.jpg";
        let dest = place_developed(&root, rel, &staged, false).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"v1");
        assert!(!staged.exists());
        // Re-rendered: replaces its own copy.
        fs::write(&staged, b"v2").unwrap();
        place_developed(&root, rel, &staged, true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
        // A file Darkslide didn't make at that path is never replaced.
        fs::write(&staged, b"v3").unwrap();
        let e = place_developed(&root, rel, &staged, false).unwrap_err();
        assert!(matches!(e, PublishError::Taken(_)), "{e}");
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
        assert!(staged.exists(), "the render is kept for a retry");
        // A missing render is an error.
        assert!(matches!(
            place_developed(&root, "Developed/x.jpg", &t.path().join("gone.jpg"), false),
            Err(PublishError::Io { .. })
        ));
    }

    #[test]
    fn sidecars_never_replace_foreign_ones() {
        let t = tempfile::tempdir().unwrap();
        let raw = t.path().join("IMG_0001.ARW");
        let path = sidecar_path(&raw);
        assert_eq!(path, t.path().join("IMG_0001.ARW.xmp"));
        // None there: written.
        let SidecarOutcome::Written(h1) = write_sidecar(&path, b"OURS 1", None, ours).unwrap()
        else {
            panic!("not written");
        };
        assert_eq!(h1, hash_hex(b"OURS 1"));
        // Same bytes: nothing written.
        assert_eq!(
            write_sidecar(&path, b"OURS 1", Some(&h1), ours).unwrap(),
            SidecarOutcome::Unchanged(h1.clone())
        );
        // Ours and unchanged since: replaced.
        assert!(matches!(
            write_sidecar(&path, b"OURS 2", Some(&h1), ours).unwrap(),
            SidecarOutcome::Written(_)
        ));
        // Ours by marker but changed since it was written (edited by hand): kept.
        fs::write(&path, b"OURS edited").unwrap();
        assert_eq!(
            write_sidecar(&path, b"OURS 3", Some(&hash_hex(b"OURS 2")), ours).unwrap(),
            SidecarOutcome::Foreign
        );
        assert_eq!(fs::read(&path).unwrap(), b"OURS edited");
        // No record, but our marker (the state was lost): ours.
        assert!(matches!(
            write_sidecar(&path, b"OURS 4", None, ours).unwrap(),
            SidecarOutcome::Written(_)
        ));
        // A Lightroom sidecar is never touched.
        let lr = t.path().join("LR.ARW.xmp");
        fs::write(&lr, b"<x:xmpmeta Lightroom/>").unwrap();
        assert_eq!(
            write_sidecar(&lr, b"OURS", None, ours).unwrap(),
            SidecarOutcome::Foreign
        );
        assert_eq!(fs::read(&lr).unwrap(), b"<x:xmpmeta Lightroom/>");
        // Nor a symlink or a folder in its place.
        let link = t.path().join("L.ARW.xmp");
        std::os::unix::fs::symlink(&lr, &link).unwrap();
        assert_eq!(
            write_sidecar(&link, b"OURS", None, ours).unwrap(),
            SidecarOutcome::Foreign
        );
        let dir = t.path().join("D.ARW.xmp");
        fs::create_dir(&dir).unwrap();
        assert_eq!(
            write_sidecar(&dir, b"OURS", None, ours).unwrap(),
            SidecarOutcome::Foreign
        );
        // A folder that can't be written: an error.
        let ro = t.path().join("ro");
        fs::create_dir(&ro).unwrap();
        let mut perm = fs::metadata(&ro).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o555);
        fs::set_permissions(&ro, perm.clone()).unwrap();
        assert!(write_sidecar(&ro.join("x.ARW.xmp"), b"OURS", None, ours).is_err());
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        fs::set_permissions(&ro, perm).unwrap();
    }
}
