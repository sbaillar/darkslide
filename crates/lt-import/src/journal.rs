//! Import journals (`<dest>/.lt/import-journal-<id>.json`): let a pulled card
//! or a crash resume without re-copying finished files or overwriting anything.
//!
//! * Written atomically (unique temp → fsync → rename).
//! * One journal per set of source volumes (`<id>` hashes their UUIDs), so
//!   two cards imported into one session resume independently.
//! * Entries are keyed by **volume UUID + path relative to the volume + size +
//!   mtime**, so a card remounted as `/Volumes/CARD 1` is still recognized.
//! * The whole plan is recorded as `Pending` before copying. Entries become
//!   `Done` only at a durability checkpoint (after `F_FULLFSYNC`).
//! * Removed once an import finishes complete.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::ImportError;

pub const JOURNAL_VERSION: u32 = 2;
const PREFIX: &str = "import-journal-";
/// Marker in the name of a journal that could not be read and was set aside.
const BAD_MARK: &str = ".bad-";

/// `<dest>/.lt`
pub fn lt_dir(dest: &Path) -> PathBuf {
    dest.join(".lt")
}

/// Journal file for a journal id.
pub fn journal_path(dest: &Path, id: &str) -> PathBuf {
    lt_dir(dest).join(format!("{PREFIX}{id}.json"))
}

/// Migration source: the folder Darkslide 0.1 kept its journals in. A session
/// that has not been opened since the rename still has them there; opening it
/// (`lt_session::Session::open`) moves them to `.lt` before an import resumes.
pub const LEGACY_DIR: &str = ".ds";

/// Every import journal present in `dest` (in `.lt`, else still in the old `.ds`).
pub fn journal_paths(dest: &Path) -> Vec<PathBuf> {
    let dir = lt_dir(dest);
    let dir = if dir.exists() {
        dir
    } else {
        dest.join(LEGACY_DIR)
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && !n.contains(BAD_MARK))
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    v.sort();
    v
}

/// Journal id for a set of source volume ids (order-independent).
pub fn journal_id<'a>(volumes: impl IntoIterator<Item = &'a str>) -> String {
    let mut v: Vec<&str> = volumes.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    blake3::hash(v.join("\n").as_bytes()).to_hex()[..16].to_owned()
}

/// Identity of a source file across runs and remounts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    /// File-system UUID of the source volume (or `mount:<path>` if unknown).
    pub volume: String,
    /// Path relative to the volume's mount point.
    pub rel: PathBuf,
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
}

impl SourceKey {
    pub fn new(volume: &str, rel: &Path, size: u64, mtime: SystemTime) -> Self {
        let d = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        Self {
            volume: volume.to_owned(),
            rel: rel.to_path_buf(),
            size,
            mtime_secs: d.as_secs(),
            mtime_nanos: d.subsec_nanos(),
        }
    }
}

/// Maps source paths to (mount point, volume id), caching per directory.
#[derive(Debug, Default)]
pub struct VolumeCache {
    by_dir: HashMap<PathBuf, (PathBuf, String)>,
    by_mount: HashMap<PathBuf, String>,
}

impl VolumeCache {
    /// (mount point, volume id, path relative to the mount) for a source file.
    pub fn identify(&mut self, path: &Path) -> (PathBuf, String, PathBuf) {
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        if !self.by_dir.contains_key(&dir) {
            let mount = lt_fsutil::mount_point_of(&dir).unwrap_or_else(|_| PathBuf::from("/"));
            let id = self
                .by_mount
                .entry(mount.clone())
                .or_insert_with(|| {
                    lt_fsutil::volume_uuid(&mount)
                        .unwrap_or_else(|| format!("mount:{}", mount.display()))
                })
                .clone();
            self.by_dir.insert(dir.clone(), (mount, id));
        }
        let (mount, id) = self.by_dir[&dir].clone();
        let canon = std::fs::canonicalize(&dir).map_or_else(
            |_| path.to_path_buf(),
            |d| d.join(path.file_name().unwrap_or_default()),
        );
        let rel = canon
            .strip_prefix(&mount)
            .map_or_else(|_| canon.clone(), Path::to_path_buf);
        (mount, id, rel)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EntryState {
    Pending,
    /// Copied, verified and durable. Hashes are hex BLAKE3 (full, partial).
    /// `backup_ok = false`: the primary is done, the backup copy is not.
    Done {
        hash: String,
        partial: String,
        backup_ok: bool,
    },
    Failed {
        reason: String,
    },
    Duplicate {
        existing: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    #[serde(flatten)]
    pub key: SourceKey,
    /// Destination path relative to the destination (and backup) root.
    pub dest_rel: PathBuf,
    #[serde(flatten)]
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub entries: Vec<JournalEntry>,
    /// The backup root of the run (its `.partial`s are cleaned by [`discard`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_dest: Option<PathBuf>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            entries: Vec::new(),
            backup_dest: None,
        }
    }
}

impl Journal {
    /// Load one journal file; `Ok(None)` if it does not exist.
    ///
    /// The format version is read on its own first, so a journal of another
    /// version is always [`ImportError::JournalVersion`], even when the rest
    /// of it (a newer entry shape) would not parse here.
    pub fn load(path: &Path) -> Result<Option<Self>, ImportError> {
        #[derive(Deserialize)]
        struct Header {
            version: u32,
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ImportError::io(path, e)),
        };
        let bad = |e: serde_json::Error| ImportError::Journal(format!("{}: {e}", path.display()));
        let Header { version } = serde_json::from_slice(&bytes).map_err(bad)?;
        if version != JOURNAL_VERSION {
            return Err(ImportError::JournalVersion {
                path: path.to_path_buf(),
                version,
            });
        }
        let j: Journal = serde_json::from_slice(&bytes).map_err(bad)?;
        j.validate_paths()
            .map_err(|why| ImportError::Journal(format!("{}: {why}", path.display())))?;
        Ok(Some(j))
    }

    /// The journal is plain JSON in a folder that may be synced or restored
    /// from backup, so its paths are untrusted: every `dest_rel` is joined
    /// onto a root and used for copies, renames and deletes. Reject anything
    /// that could leave the root (absolute, `..`, empty), and a backup root
    /// that is not an absolute, normalized path.
    fn validate_paths(&self) -> Result<(), String> {
        use std::path::Component;
        for e in &self.entries {
            let p = &e.dest_rel;
            if p.as_os_str().is_empty()
                || !p.components().all(|c| matches!(c, Component::Normal(_)))
            {
                return Err(format!("unsafe destination path {}", p.display()));
            }
        }
        if let Some(b) = &self.backup_dest
            && (!b.is_absolute() || b.components().any(|c| matches!(c, Component::ParentDir)))
        {
            return Err(format!("unsafe backup root {}", b.display()));
        }
        Ok(())
    }

    /// Every journal in `dest`.
    ///
    /// A journal that cannot be parsed (torn by a power loss), holds unsafe
    /// paths or is from an older format version is renamed to
    /// `import-journal-<id>.bad-<ts>.json` and ignored, so it can never block
    /// imports into this destination. Its files are then "not ours": a later
    /// plan only reuses their names after re-verifying the bytes by hash.
    ///
    /// A journal from a newer format version (a later Luxtable, then a
    /// downgrade) is skipped but left in place for that version to resume;
    /// [`run`](crate::run()) refuses to rewrite it.
    pub fn load_all(dest: &Path) -> Result<Vec<(PathBuf, Self)>, ImportError> {
        let mut out = Vec::new();
        for p in journal_paths(dest) {
            match Self::load(&p) {
                Ok(Some(j)) => out.push((p, j)),
                Ok(None) => {}
                Err(ImportError::JournalVersion { version, .. }) if version > JOURNAL_VERSION => {
                    tracing::warn!(version, path = ?p, "import journal from a newer version left in place");
                }
                Err(ImportError::Journal(why)) => {
                    let aside = set_aside(&p)?;
                    tracing::warn!(%why, ?aside, "unreadable import journal set aside");
                }
                Err(e @ ImportError::JournalVersion { .. }) => {
                    let aside = set_aside(&p)?;
                    tracing::warn!(%e, ?aside, "old import journal set aside");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub fn save(&self, path: &Path) -> Result<(), ImportError> {
        let bytes = serde_json::to_vec(self).map_err(|e| ImportError::Journal(e.to_string()))?;
        lt_fsutil::write_atomic(path, &bytes).map_err(|e| ImportError::io(path, e))
    }

    pub fn find(&self, key: &SourceKey) -> Option<&JournalEntry> {
        self.entries.iter().find(|e| &e.key == key)
    }
}

/// Rename an unreadable journal out of the way (never deleted).
fn set_aside(p: &Path) -> Result<PathBuf, ImportError> {
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
    let aside = p.with_file_name(format!("{stem}{BAD_MARK}{ts}.json"));
    std::fs::rename(p, &aside).map_err(|e| ImportError::io(p, e))?;
    Ok(aside)
}

/// Summary for the "resume import?" prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResumeInfo {
    pub done: usize,
    pub pending: usize,
    pub failed: usize,
    /// Source volume ids (UUIDs) of the unfinished imports: which card(s)
    /// resume them.
    pub volumes: Vec<String>,
    /// Backup roots those imports were writing to.
    pub backup_dests: Vec<PathBuf>,
}

/// Is there an unfinished import into `dest`? Sums all journals.
pub fn pending_resume(dest: &Path) -> Result<Option<ResumeInfo>, ImportError> {
    let all = Journal::load_all(dest)?;
    if all.is_empty() {
        return Ok(None);
    }
    let mut info = ResumeInfo::default();
    for (_, j) in &all {
        if let Some(b) = &j.backup_dest
            && !info.backup_dests.contains(b)
        {
            info.backup_dests.push(b.clone());
        }
        for e in &j.entries {
            if !info.volumes.contains(&e.key.volume) {
                info.volumes.push(e.key.volume.clone());
            }
            match e.state {
                EntryState::Done {
                    backup_ok: true, ..
                }
                | EntryState::Duplicate { .. } => info.done += 1,
                EntryState::Pending
                | EntryState::Done {
                    backup_ok: false, ..
                } => info.pending += 1,
                EntryState::Failed { .. } => info.failed += 1,
            }
        }
    }
    Ok(Some(info))
}

/// Abandon unfinished imports: delete leftover `.partial` files they created
/// (never finished files) in the destination, in `backup` and in the backup
/// root each journal recorded, then the journals.
pub fn discard(dest: &Path, backup: Option<&Path>) -> Result<(), ImportError> {
    for (path, j) in Journal::load_all(dest)? {
        let mut roots: Vec<&Path> = vec![dest];
        roots.extend(backup);
        if let Some(b) = j.backup_dest.as_deref()
            && !roots.contains(&b)
        {
            roots.push(b);
        }
        for e in &j.entries {
            for root in &roots {
                let p = crate::fsio::partial_path(&root.join(&e.dest_rel));
                lt_fsutil::remove_if_exists(&p).map_err(|err| ImportError::io(&p, err))?;
            }
        }
        lt_fsutil::remove_if_exists(&path).map_err(|e| ImportError::io(&path, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: &str, state: EntryState) -> JournalEntry {
        JournalEntry {
            key: SourceKey::new(
                "VOL",
                Path::new(n),
                5,
                SystemTime::UNIX_EPOCH + Duration::new(9, 7),
            ),
            dest_rel: PathBuf::from(n),
            state,
        }
    }

    #[test]
    fn roundtrip_find_and_summary_across_journals() {
        let t = tempfile::tempdir().unwrap();
        assert!(pending_resume(t.path()).unwrap().is_none());
        assert_eq!(journal_paths(t.path()).len(), 0);
        let done = |b| EntryState::Done {
            hash: "h".into(),
            partial: "p".into(),
            backup_ok: b,
        };
        let j = Journal {
            backup_dest: None,
            version: JOURNAL_VERSION,
            entries: vec![
                entry("a", EntryState::Pending),
                entry("b", done(true)),
                entry("c", EntryState::Failed { reason: "r".into() }),
                entry(
                    "d",
                    EntryState::Duplicate {
                        existing: "e".into(),
                    },
                ),
                entry("e", done(false)),
            ],
        };
        let p1 = journal_path(t.path(), "one");
        j.save(&p1).unwrap();
        Journal {
            backup_dest: None,
            version: JOURNAL_VERSION,
            entries: vec![entry("x", EntryState::Pending)],
        }
        .save(&journal_path(t.path(), "two"))
        .unwrap();
        std::fs::write(lt_dir(t.path()).join("unrelated.json"), b"{}").unwrap();
        assert_eq!(journal_paths(t.path()).len(), 2);
        let back = Journal::load(&p1).unwrap().unwrap();
        assert_eq!(back, j);
        let key = SourceKey::new(
            "VOL",
            Path::new("b"),
            5,
            SystemTime::UNIX_EPOCH + Duration::new(9, 7),
        );
        assert!(matches!(
            back.find(&key).unwrap().state,
            EntryState::Done { .. }
        ));
        assert_eq!(
            pending_resume(t.path()).unwrap(),
            Some(ResumeInfo {
                done: 2,
                pending: 3,
                failed: 1,
                volumes: vec!["VOL".into()],
                backup_dests: Vec::new(),
            })
        );
    }

    #[test]
    fn journal_id_is_order_independent() {
        assert_eq!(journal_id(["a", "b"]), journal_id(["b", "a", "a"]));
        assert_ne!(journal_id(["a"]), journal_id(["b"]));
        assert_eq!(journal_id(["a"]).len(), 16);
    }

    #[test]
    fn corrupt_or_old_journal_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let p = journal_path(t.path(), "x");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{not json").unwrap();
        assert!(matches!(Journal::load(&p), Err(ImportError::Journal(_))));
        // load_all sets it aside instead of failing.
        assert!(pending_resume(t.path()).unwrap().is_none());
        assert!(!p.exists());
        let aside: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].to_string_lossy().contains(".bad-"));
        assert!(
            journal_paths(t.path()).is_empty(),
            "set-aside journals are not loaded again"
        );
        std::fs::remove_file(p.with_file_name(&aside[0])).unwrap();
        // An older version is a version error, and is set aside.
        std::fs::write(&p, br#"{"version":1,"entries":[]}"#).unwrap();
        assert!(matches!(
            Journal::load(&p),
            Err(ImportError::JournalVersion { version: 1, .. })
        ));
        assert_eq!(Journal::load_all(t.path()).unwrap().len(), 0);
        assert!(!p.exists(), "older journal set aside");
        // A newer version, even one whose entries don't parse here, is a
        // version error and is never moved aside.
        let newer = br#"{"version":99,"entries":[{"state":"hologram"}],"x":1}"#;
        std::fs::write(&p, newer).unwrap();
        let e = Journal::load(&p).unwrap_err();
        assert!(
            matches!(e, ImportError::JournalVersion { version: 99, .. }),
            "{e}"
        );
        assert!(e.to_string().contains("format version 99"), "{e}");
        assert_eq!(Journal::load_all(t.path()).unwrap().len(), 0);
        assert!(pending_resume(t.path()).unwrap().is_none());
        assert_eq!(std::fs::read(&p).unwrap(), newer, "newer journal untouched");
        // No version at all: not a journal we understand, set aside.
        std::fs::write(&p, br#"{"entries":[]}"#).unwrap();
        assert!(matches!(Journal::load(&p), Err(ImportError::Journal(_))));
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert!(matches!(Journal::load(&p), Err(ImportError::Io { .. })));
    }

    #[test]
    fn discard_removes_partials_and_journals_only() {
        let t = tempfile::tempdir().unwrap();
        let (dest, backup) = (t.path().join("d"), t.path().join("b"));
        Journal {
            backup_dest: None,
            version: JOURNAL_VERSION,
            entries: vec![entry("x.ARW", EntryState::Pending)],
        }
        .save(&journal_path(&dest, "id"))
        .unwrap();
        for root in [&dest, &backup] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(crate::fsio::partial_path(&root.join("x.ARW")), b"p").unwrap();
            std::fs::write(root.join("keep.ARW"), b"k").unwrap();
        }
        discard(&dest, Some(&backup)).unwrap();
        assert_eq!(journal_paths(&dest).len(), 0);
        for root in [&dest, &backup] {
            assert!(!crate::fsio::partial_path(&root.join("x.ARW")).exists());
            assert!(root.join("keep.ARW").exists());
        }
        discard(&dest, None).unwrap();
    }

    #[test]
    fn volume_cache_identifies_mount_and_relative_path() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("DCIM/A.ARW");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"x").unwrap();
        let mut c = VolumeCache::default();
        let (mount, id, rel) = c.identify(&f);
        assert!(f.canonicalize().unwrap().starts_with(&mount));
        assert!(rel.ends_with("DCIM/A.ARW") && rel.is_relative(), "{rel:?}");
        assert_ne!(id.len(), 0);
        assert_eq!(c.identify(&f), (mount, id, rel), "cached");
        // Nonexistent dirs fall back gracefully.
        let (_, _, rel) = c.identify(Path::new("/nonexistent/x/A.ARW"));
        assert!(rel.ends_with("A.ARW"));
    }

    #[test]
    fn journal_with_escaping_paths_is_set_aside() {
        for bad in ["/etc/passwd", "../../x", "a/../../x", ""] {
            let t = tempfile::tempdir().unwrap();
            let j = Journal {
                backup_dest: None,
                version: JOURNAL_VERSION,
                entries: vec![entry("ok.ARW", EntryState::Pending), {
                    let mut e = entry("x", EntryState::Pending);
                    e.dest_rel = PathBuf::from(bad);
                    e
                }],
            };
            let p = journal_path(t.path(), "evil");
            j.save(&p).unwrap();
            assert!(
                matches!(Journal::load(&p), Err(ImportError::Journal(m)) if m.contains("unsafe"))
            );
            assert!(Journal::load_all(t.path()).unwrap().is_empty(), "{bad}");
            assert!(!p.exists(), "{bad}: moved aside, not trusted");
        }
        let mut j = Journal::default();
        for b in ["relative/root", "/a/../b"] {
            j.backup_dest = Some(PathBuf::from(b));
            assert!(j.validate_paths().is_err(), "{b}");
        }
        j.backup_dest = Some(PathBuf::from("/Volumes/backup"));
        j.entries.push(entry("sub/dir/x.ARW", EntryState::Pending));
        assert!(j.validate_paths().is_ok());
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        /// Fixed seed and no persistence file: the same inputs on every run.
        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: 64,
                rng_seed: proptest::test_runner::RngSeed::Fixed(1_592_590_338),
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(config())]
            /// Arbitrary bytes, and valid JSON with arbitrary path strings,
            /// either load cleanly with safe paths or return an error.
            #[test]
            fn load_never_panics_and_only_yields_safe_paths(
                bytes in proptest::collection::vec(any::<u8>(), 0..256),
                rel in "\\PC{0,24}",
                backup in proptest::option::of("\\PC{0,24}"),
            ) {
                let t = tempfile::tempdir().unwrap();
                let p = t.path().join("j.json");
                std::fs::write(&p, &bytes).unwrap();
                let _ = Journal::load(&p);

                let mut e = entry("x", EntryState::Pending);
                e.dest_rel = PathBuf::from(rel);
                let j = Journal {
                    version: JOURNAL_VERSION,
                    entries: vec![e],
                    backup_dest: backup.map(PathBuf::from),
                };
                j.save(&p).unwrap();
                if let Ok(Some(back)) = Journal::load(&p) {
                    for e in &back.entries {
                        prop_assert!(e.dest_rel.components().all(|c| matches!(c, std::path::Component::Normal(_))));
                        prop_assert!(!e.dest_rel.as_os_str().is_empty());
                    }
                }
            }
        }
    }
}
