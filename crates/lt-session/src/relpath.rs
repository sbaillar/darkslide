//! Validated session-relative paths and the top-level image folders.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, SessionError};

/// The session folders that hold originals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Folder {
    Capture,
    Selects,
    Trash,
}

impl Folder {
    pub const ALL: [Folder; 3] = [Folder::Capture, Folder::Selects, Folder::Trash];

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Capture => "Capture",
            Self::Selects => "Selects",
            Self::Trash => "Trash",
        }
    }

    pub fn from_dir_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.dir_name() == s)
    }

    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Capture => 0,
            Self::Selects => 1,
            Self::Trash => 2,
        }
    }
}

/// A path relative to the session root, `/`-separated, whose first component
/// is one of the [`Folder`] directories (e.g. `Capture/Day 1/DSC00412.ARW`).
///
/// Construction rejects absolute paths, `..`/`.` components, empty components,
/// hidden components (leading `.`, which also reserves `.lt`), backslashes and NUL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(s: &str) -> Result<Self> {
        let bad = |reason| {
            Err(SessionError::InvalidPath {
                path: s.to_owned(),
                reason,
            })
        };
        if s.is_empty() {
            return bad("empty");
        }
        if s.starts_with('/') || Path::new(s).is_absolute() {
            return bad("absolute paths are not allowed");
        }
        if s.contains('\0') || s.contains('\\') {
            return bad("NUL and backslash are not allowed");
        }
        let parts: Vec<&str> = s.split('/').collect();
        for p in &parts {
            match *p {
                "" => return bad("empty path component"),
                "." | ".." => return bad("`.` and `..` components are not allowed"),
                p if p.starts_with('.') => return bad("hidden components are reserved"),
                _ => {}
            }
        }
        if parts.len() < 2 {
            return bad("must name a file inside Capture/, Selects/ or Trash/");
        }
        if Folder::from_dir_name(parts[0]).is_none() {
            return bad("first component must be Capture, Selects or Trash");
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn folder(&self) -> Folder {
        let first = self.0.split('/').next().unwrap_or_default();
        Folder::from_dir_name(first).unwrap_or(Folder::Capture)
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or_default()
    }

    /// Directory part (always non-empty: at least the folder name).
    pub fn parent(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(p, _)| p)
    }

    pub fn to_path(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }

    /// Same path under a different top-level folder.
    #[must_use]
    pub fn with_folder(&self, folder: Folder) -> Self {
        let rest = self.0.split_once('/').map_or("", |(_, r)| r);
        Self(format!("{}/{rest}", folder.dir_name()))
    }

    /// Same directory, different file name (validated).
    pub fn with_file_name(&self, name: &str) -> Result<Self> {
        Self::new(&format!("{}/{name}", self.parent()))
    }

    /// Relative path of the sidecar for variant `ordinal` of this original:
    /// `<dir>/.lt/settings/<file>.json` for the first variant and
    /// `<dir>/.lt/settings/v<N>/<file>.json` for the rest. Separate directories
    /// keep the mapping injective whatever the original's file name is.
    /// Names longer than [`MAX_PLAIN_NAME`] bytes are shortened
    /// deterministically (see [`sidecar_file_name`]).
    pub fn sidecar_rel(&self, ordinal: u32) -> String {
        let file = sidecar_file_name(self.file_name());
        if ordinal <= 1 {
            format!("{}/.lt/settings/{file}", self.parent())
        } else {
            format!("{}/.lt/settings/v{ordinal}/{file}", self.parent())
        }
    }

    /// Inverse of [`RelPath::sidecar_rel`] for non-shortened names, given the
    /// ordinal stored in the sidecar. `None` if `sidecar_rel` is not that
    /// ordinal's sidecar location or the name was shortened (the session
    /// resolves those by listing the original's directory).
    pub fn from_sidecar_rel(sidecar_rel: &str, ordinal: u32) -> Option<Self> {
        let (parent, _) = sidecar_parts(sidecar_rel, ordinal)?;
        let name = sidecar_rel.rsplit_once('/')?.1;
        let file = name.strip_suffix(".json")?;
        if looks_shortened(file) {
            return None;
        }
        let rel = Self::new(&format!("{parent}/{file}")).ok()?;
        (rel.sidecar_rel(ordinal) == sidecar_rel).then_some(rel)
    }
}

/// `<≤150 bytes>~<16 hex>`: the form [`sidecar_file_name`] gives long names.
fn looks_shortened(stem: &str) -> bool {
    stem.len() > 17
        && stem.len() <= 150 + 17
        && stem.as_bytes()[stem.len() - 17] == b'~'
        && stem[stem.len() - 16..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// `(original's directory, sidecar file name)` of a sidecar location.
fn sidecar_parts(sidecar_rel: &str, ordinal: u32) -> Option<(&str, &str)> {
    let (dir, name) = sidecar_rel.rsplit_once('/')?;
    let settings = if ordinal <= 1 {
        dir
    } else {
        dir.strip_suffix(&format!("/v{ordinal}"))?
    };
    Some((settings.strip_suffix("/.lt/settings")?, name))
}

/// Original file names up to this many bytes keep a readable `<name>.json`
/// sidecar; longer ones are shortened so the sidecar (and its temp file)
/// stay under the 255-byte file name limit.
pub const MAX_PLAIN_NAME: usize = 200;

/// `<file>.json`, or for long names `<first ≤150 bytes>~<16 hex of BLAKE3(file)>.json`.
pub fn sidecar_file_name(file: &str) -> String {
    if file.len() <= MAX_PLAIN_NAME {
        return format!("{file}.json");
    }
    let mut cut = 150;
    while !file.is_char_boundary(cut) {
        cut -= 1;
    }
    let hash = blake3::hash(file.as_bytes()).to_hex();
    format!("{}~{}.json", &file[..cut], &hash[..16])
}

/// Map a sidecar location back to its original. Shortened names are resolved
/// by finding the file in the original's directory whose sidecar name
/// matches; `None` if nothing does (e.g. the original is gone).
pub(crate) fn resolve_sidecar(
    root: &Path,
    external: Option<&Path>,
    sidecar_rel: &str,
    ordinal: u32,
) -> Option<RelPath> {
    if let Some(rel) = RelPath::from_sidecar_rel(sidecar_rel, ordinal) {
        return Some(rel);
    }
    let (parent, _) = sidecar_parts(sidecar_rel, ordinal)?;
    std::fs::read_dir(original_path(root, external, parent))
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.len() > MAX_PLAIN_NAME)
        .filter_map(|n| RelPath::new(&format!("{parent}/{n}")).ok())
        .find(|rel| rel.sidecar_rel(ordinal) == sidecar_rel)
}

/// Where the file (or directory) at session-relative `rel` lives: under the
/// session root, or for sessions with external originals, under that root
/// (`Capture/<rest>` → `<external>/<rest>`; external sessions only use Capture).
pub fn original_path(root: &Path, external: Option<&Path>, rel: &str) -> PathBuf {
    match external {
        Some(ext) => match rel.split_once('/') {
            Some((_, rest)) => ext.join(rest),
            None => ext.to_path_buf(),
        },
        None => root.join(rel),
    }
}

/// Refuse paths that leave the session through a symlink: every existing
/// component of `rel` below `root` must be a real directory or file.
pub(crate) fn ensure_no_symlinks(root: &Path, rel: &str) -> Result<()> {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        match p.symlink_metadata() {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(SessionError::InvalidPath {
                    path: rel.to_owned(),
                    reason: "symbolic links inside the session are not followed",
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Path of `abs` relative to `root` as a `/`-joined UTF-8 string, if it is inside.
pub(crate) fn rel_string(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_and_unicode_paths() {
        for ok in [
            "Capture/a.ARW",
            "Selects/Day 1/DSC00412.ARW",
            "Trash/été/日本 📷.arw",
            "Capture/a/b/c/d.ARW",
        ] {
            let p = RelPath::new(ok).unwrap();
            assert_eq!(p.as_str(), ok);
        }
    }

    #[test]
    fn rejects_traversal_absolute_and_reserved() {
        let bad = [
            "",
            "/Capture/a.ARW",
            "Capture/../x.ARW",
            "../Capture/a.ARW",
            "Capture/./a.ARW",
            "Capture//a.ARW",
            "Capture/a.ARW/",
            "Capture/.lt/settings/a.json",
            "Capture/.hidden",
            "Capture",
            "Output/a.jpg",
            "Other/a.ARW",
            "Capture/a\\b.ARW",
            "Capture/a\0.ARW",
        ];
        for b in bad {
            let e = RelPath::new(b).unwrap_err();
            assert!(
                matches!(e, SessionError::InvalidPath { .. }),
                "{b:?} -> {e}"
            );
        }
    }

    #[test]
    fn parts_and_folder_swaps() {
        let p = RelPath::new("Capture/Day 1/a.ARW").unwrap();
        assert_eq!(p.folder(), Folder::Capture);
        assert_eq!(p.file_name(), "a.ARW");
        assert_eq!(p.parent(), "Capture/Day 1");
        let s = p.with_folder(Folder::Selects);
        assert_eq!(s.as_str(), "Selects/Day 1/a.ARW");
        assert_eq!(s.folder(), Folder::Selects);
        assert_eq!(
            p.with_file_name("b.ARW").unwrap().as_str(),
            "Capture/Day 1/b.ARW"
        );
        assert!(p.with_file_name("..").is_err());
        assert_eq!(
            p.to_path(Path::new("/r")),
            PathBuf::from("/r/Capture/Day 1/a.ARW")
        );
        assert_eq!(p.to_string(), "Capture/Day 1/a.ARW");
    }

    #[test]
    fn sidecar_paths_round_trip_and_are_injective() {
        // `IMG` variant 2 and `IMG.v2` variant 1 must not share a sidecar.
        let a = RelPath::new("Capture/x/IMG").unwrap();
        let b = RelPath::new("Capture/x/IMG.v2").unwrap();
        assert_ne!(a.sidecar_rel(2), b.sidecar_rel(1));
        for p in [&a, &b] {
            for ord in [1, 2, 3, 17] {
                let s = p.sidecar_rel(ord);
                assert_eq!(
                    RelPath::from_sidecar_rel(&s, ord).as_ref(),
                    Some(p),
                    "ordinal {ord}"
                );
            }
        }
        assert_eq!(b.sidecar_rel(1), "Capture/x/.lt/settings/IMG.v2.json");
        assert_eq!(a.sidecar_rel(2), "Capture/x/.lt/settings/v2/IMG.json");
        assert!(RelPath::from_sidecar_rel("Capture/x/IMG.json", 1).is_none());
        assert!(RelPath::from_sidecar_rel("Capture/x/.lt/settings/IMG.json", 2).is_none());
        assert!(RelPath::from_sidecar_rel("Capture/x/.lt/settings/IMG.txt", 1).is_none());
        assert!(RelPath::from_sidecar_rel("nodir", 1).is_none());
    }

    #[test]
    fn long_names_are_shortened_and_resolved_via_directory() {
        let d = tempfile::tempdir().unwrap();
        let long = format!("{}é.ARW", "x".repeat(240));
        let rel = RelPath::new(&format!("Capture/{long}")).unwrap();
        let sc = rel.sidecar_rel(2);
        let name = sc.rsplit_once('/').unwrap().1;
        assert!(name.len() < 200, "{}", name.len());
        assert!(
            std::path::Path::new(name)
                .extension()
                .is_some_and(|e| e == "json")
        );
        assert!(name.contains('~'));
        assert_ne!(
            RelPath::new(&format!("Capture/{}y.ARW", "x".repeat(240)))
                .unwrap()
                .sidecar_rel(2),
            sc,
            "different long names get different sidecars"
        );
        assert_eq!(RelPath::from_sidecar_rel(&sc, 2), None);
        assert_eq!(
            resolve_sidecar(d.path(), None, &sc, 2),
            None,
            "original absent"
        );
        std::fs::create_dir_all(d.path().join("Capture")).unwrap();
        std::fs::write(d.path().join("Capture").join(&long), b"x").unwrap();
        std::fs::write(d.path().join("Capture/short.ARW"), b"x").unwrap();
        assert_eq!(resolve_sidecar(d.path(), None, &sc, 2), Some(rel));
        let short = RelPath::new("Capture/short.ARW").unwrap();
        assert_eq!(
            resolve_sidecar(d.path(), None, &short.sidecar_rel(1), 1),
            Some(short)
        );
        assert_eq!(resolve_sidecar(d.path(), None, "junk", 1), None);
        // Exactly at the limit stays readable.
        assert_eq!(
            sidecar_file_name(&"a".repeat(MAX_PLAIN_NAME)).len(),
            MAX_PLAIN_NAME + 5
        );
    }

    #[test]
    fn symlinked_components_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("s");
        std::fs::create_dir_all(root.join("Capture/.lt")).unwrap();
        std::os::unix::fs::symlink(d.path(), root.join("Capture/.lt/settings")).unwrap();
        assert!(ensure_no_symlinks(&root, "Capture/.lt/settings/a.json").is_err());
        assert!(ensure_no_symlinks(&root, "Capture/.lt/x/a.json").is_ok());
        assert!(ensure_no_symlinks(&root, "Capture/new/deeper").is_ok());
    }

    #[test]
    fn folder_names_and_codes() {
        for f in Folder::ALL {
            assert_eq!(Folder::from_dir_name(f.dir_name()), Some(f));
        }
        assert_eq!(Folder::ALL.map(Folder::code), [0, 1, 2]);
        assert_eq!(Folder::from_dir_name("Output"), None);
    }

    #[test]
    fn original_paths_internal_and_external() {
        let root = Path::new("/s/sess");
        assert_eq!(
            original_path(root, None, "Capture/a/b.ARW"),
            PathBuf::from("/s/sess/Capture/a/b.ARW")
        );
        let ext = Path::new("/Volumes/CARD");
        assert_eq!(
            original_path(root, Some(ext), "Capture/DCIM/100/b.ARW"),
            PathBuf::from("/Volumes/CARD/DCIM/100/b.ARW")
        );
        assert_eq!(original_path(root, Some(ext), "Capture"), ext.to_path_buf());
    }

    #[test]
    fn external_long_names_resolve_against_the_originals_root() {
        let d = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let long = format!("{}.ARW", "x".repeat(MAX_PLAIN_NAME + 10));
        std::fs::write(ext.path().join(&long), b"r").unwrap();
        let rel = RelPath::new(&format!("Capture/{long}")).unwrap();
        let sc = rel.sidecar_rel(1);
        assert_eq!(
            resolve_sidecar(d.path(), None, &sc, 1),
            None,
            "not under the session root"
        );
        assert_eq!(
            resolve_sidecar(d.path(), Some(ext.path()), &sc, 1),
            Some(rel)
        );
    }

    #[test]
    fn rel_string_inside_and_outside() {
        let root = Path::new("/s/sess");
        assert_eq!(
            rel_string(root, Path::new("/s/sess/Capture/a b.ARW")).unwrap(),
            "Capture/a b.ARW"
        );
        assert!(rel_string(root, Path::new("/elsewhere/a")).is_none());
    }
}
