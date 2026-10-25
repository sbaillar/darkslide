//! One archive pass: check the target, snapshot the session DB, rsync.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::ArchiveError;
use crate::rsync::{RsyncProgress, RsyncRunner};
use crate::target::ArchiveTarget;

/// Session-relative path of the consistent DB snapshot that gets archived.
pub const SNAPSHOT_REL: &str = ".ds/index-snapshot.db";
/// Target-relative directory that receives replaced files (`--backup-dir`).
pub const TRASH_DIR: &str = ".ds-archive-trash";

/// Writes a consistent copy of the session index to `dest` (SQLite backup
/// API in `ds-session`). Injected so this crate does not depend on SQLite.
pub trait Snapshotter: Send + Sync {
    fn snapshot(&self, session_dir: &Path, dest: &Path) -> Result<(), String>;
}

/// What an archive pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveOutcome {
    pub dest: PathBuf,
    /// Target-relative `--backup-dir` used for replaced files.
    pub backup_dir: String,
    /// Snapshot included in this pass (None if the snapshot failed).
    pub snapshot: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// What is **not** in the archive after this pass: unreadable originals
    /// (`"<path>: <reason>"`) and a failed DB snapshot. Empty on a clean run;
    /// otherwise the pass is incomplete (see [`crate::ArchiveError::Incomplete`]).
    pub problems: Vec<String>,
}

/// Archives a session folder to a target.
#[derive(Clone)]
pub struct Archiver {
    rsync: RsyncRunner,
    snapshotter: Option<Arc<dyn Snapshotter>>,
}

impl std::fmt::Debug for Archiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archiver")
            .field("rsync", &self.rsync)
            .field("snapshotter", &self.snapshotter.is_some())
            .finish()
    }
}

impl Archiver {
    pub fn new(rsync: RsyncRunner, snapshotter: Option<Arc<dyn Snapshotter>>) -> Self {
        Self { rsync, snapshotter }
    }

    /// Archive `session_dir` into `target`.
    ///
    /// 1. Refuse (`TargetUnavailable`) unless the target is mounted, and
    ///    (`TargetNotUsable`) unless it is writable; a missing root folder on a mounted
    ///    volume is created ([`ArchiveTarget::ensure_root`]).
    /// 2. Snapshot the DB to `<session>/.ds/index-snapshot.db`. A failed
    ///    snapshot is a problem (the archived copy would be stale), but
    ///    originals and sidecars are archived anyway: they are the source of truth.
    /// 3. Mirror moves between [`ArchiveTarget::mirror_folders`] on the target (a
    ///    rename, or into the backup dir; see [`mirror_moves`]).
    /// 4. rsync with `--backup-dir=.ds-archive-trash/<timestamp>`; never `--delete`.
    ///
    /// The session lands in `<target>/<session folder name>`; see
    /// [`Self::archive_to`] for a destination claimed per session.
    pub fn archive(
        &self,
        session_dir: &Path,
        target: &ArchiveTarget,
        now: SystemTime,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        let dest = target.dest_for(session_dir)?;
        self.archive_to(session_dir, target, &dest, now, progress, cancel)
    }

    /// [`Self::archive`] into `dest` (normally from [`ArchiveTarget::claim_dest`]).
    pub fn archive_to(
        &self,
        session_dir: &Path,
        target: &ArchiveTarget,
        dest: &Path,
        now: SystemTime,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        if !session_dir.is_absolute() || !session_dir.is_dir() {
            return Err(ArchiveError::InvalidSession(session_dir.to_path_buf()));
        }
        target.ensure_root()?;
        let dest = dest.to_path_buf();

        let mut warnings = Vec::new();
        let mut problems = Vec::new();
        let mut excludes = target.excludes.clone();
        let snapshot = take_snapshot(
            self.snapshotter.as_deref(),
            session_dir,
            &mut excludes,
            &mut problems,
        );

        // The volume may have gone since `ensure_root`: never create on the boot disk.
        target.check_mounted()?;
        let backup_dir = reserve_backup_dir(&dest, now).map_err(|e| ArchiveError::io(&dest, e))?;
        let moved = mirror_moves(session_dir, &dest, &backup_dir, target)?;
        if moved > 0 {
            tracing::info!(moved, dest = %dest.display(), "archive: mirrored session moves");
        }
        let rsync = self
            .rsync
            .clone()
            .with_modify_window(target.modify_window());
        let res = rsync.sync(session_dir, &dest, &excludes, &backup_dir, progress, cancel);
        // Our reserved (and, if nothing was replaced, still empty) backup dir.
        let _ = std::fs::remove_dir(dest.join(&backup_dir));
        let res = res?;
        if !target.mirror_folders.is_empty() {
            save_listing(session_dir, &dest, &target.mirror_folders);
        }
        warnings.extend(res.warnings);
        problems.extend(res.skipped);
        if problems.is_empty() {
            // A clean run: drop the trash's empty folders (never ones holding files).
            match prune_trash(&dest, &backup_dir) {
                Ok(n) if n > 0 => {
                    tracing::info!(pruned = n, "archive: removed empty trash folders");
                }
                Ok(_) => {}
                Err(e) => warnings.push(format!("archive trash not pruned: {e}")),
            }
        }
        Ok(ArchiveOutcome {
            dest,
            backup_dir,
            snapshot,
            warnings,
            problems,
        })
    }
}

/// Write the consistent DB snapshot to `<session>/.ds/index-snapshot.db`. When it
/// fails, the snapshot is excluded (a stale one is never archived as if it were
/// current) and the failure is a problem: the run is incomplete.
pub(crate) fn take_snapshot(
    snapshotter: Option<&dyn Snapshotter>,
    session_dir: &Path,
    excludes: &mut Vec<String>,
    problems: &mut Vec<String>,
) -> Option<PathBuf> {
    let s = snapshotter?;
    let path = session_dir.join(SNAPSHOT_REL);
    let res = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| e.to_string())
        .and_then(|()| s.snapshot(session_dir, &path));
    match res {
        Ok(()) => Some(path),
        Err(e) => {
            excludes.push(SNAPSHOT_REL.to_owned());
            problems.push(format!(
                "{SNAPSHOT_REL}: session DB snapshot failed (the archived copy is older): {e}"
            ));
            None
        }
    }
}

/// Files (target-relative to their folder) under `dir`, recursively; symlinks and our
/// partial-transfer folders are skipped.
fn files_under(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir.join(rel)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == crate::rsync::PARTIAL_DIR {
            continue;
        }
        let path = rel.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            files_under(dir, &path, out)?;
        } else if ty.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Files (relative to their folder) the session holds under each mirror folder, as of
/// the end of the last successful pass. Kept in `.ds/cache` (never archived) per
/// destination; it lets the next pass find moves from the session alone, without
/// walking the whole target. File names that are not UTF-8 are not tracked.
type Listing = BTreeMap<String, BTreeSet<String>>;

fn listing_path(session: &Path, dest: &Path) -> PathBuf {
    let key = blake3::hash(dest.as_os_str().as_encoded_bytes()).to_hex();
    session
        .join(".ds/cache")
        .join(format!("archive-mirror-{}.json", &key[..16]))
}

/// The session's current files under each of `folders` (a local walk).
fn session_listing(session: &Path, folders: &[String]) -> std::io::Result<Listing> {
    let mut out = Listing::new();
    for f in folders {
        let mut files = Vec::new();
        files_under(&session.join(f), Path::new(""), &mut files)?;
        out.insert(
            f.clone(),
            files
                .iter()
                .filter_map(|p| p.to_str().map(str::to_owned))
                .collect(),
        );
    }
    Ok(out)
}

/// The saved listing, keeping only plain relative paths: it sits in the session's
/// folder, and an entry with `..` (or an absolute one) must never steer a rename.
fn load_listing(path: &Path) -> Option<Listing> {
    use std::path::Component;
    let mut l: Listing = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    for files in l.values_mut() {
        files.retain(|f| {
            !f.is_empty()
                && Path::new(f)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
        });
    }
    Some(l)
}

/// Remember what the session holds now, for the next pass. Best effort: without it the
/// next pass just walks the target again.
fn save_listing(session: &Path, dest: &Path, folders: &[String]) {
    let path = listing_path(session, dest);
    let saved = session_listing(session, folders)
        .ok()
        .and_then(|l| serde_json::to_vec(&l).ok())
        .and_then(|b| ds_fsutil::write_atomic(&path, &b).ok());
    if saved.is_none() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Mirror the session's moves between `target.mirror_folders` on the archive: a file
/// archived as `<F>/x` that the session no longer has under `F` but has as `<G>/x` (a
/// Move to Selects, say) is renamed to `<G>/x` on the target, so rsync finds it in
/// place and the archive holds one copy; if the target already has `<G>/x`, the old
/// `<F>/x` goes into `backup_dir` instead. A file the session has under neither (a
/// reject moved to its Trash, a file removed) stays where it is: nothing is deleted.
/// Returns how many files moved. The mount is re-checked before each move.
///
/// The candidates are the files the session listed under `F` at the last successful
/// pass and no longer does; only a folder with no such record (the first pass, or the
/// record was lost) is walked on the target.
fn mirror_moves(
    session: &Path,
    dest: &Path,
    backup_dir: &str,
    target: &ArchiveTarget,
) -> Result<usize, ArchiveError> {
    let folders = &target.mirror_folders;
    let prev = load_listing(&listing_path(session, dest)).unwrap_or_default();
    let now = session_listing(session, folders).ok();
    let mut moved = 0;
    for f in folders {
        let root = dest.join(f);
        let files: Vec<PathBuf> =
            if let (Some(was), Some(is)) = (prev.get(f), now.as_ref().and_then(|n| n.get(f))) {
                was.difference(is).map(PathBuf::from).collect()
            } else {
                let mut files = Vec::new();
                files_under(&root, Path::new(""), &mut files)
                    .map_err(|e| ArchiveError::io(&root, e))?;
                files
            };
        for rel in files {
            let here =
                |folder: &str| std::fs::symlink_metadata(session.join(folder).join(&rel)).is_ok();
            if here(f) {
                continue;
            }
            let Some(g) = folders.iter().find(|g| *g != f && here(g)) else {
                continue;
            };
            let from = root.join(&rel);
            if std::fs::symlink_metadata(&from).is_err() {
                continue; // never archived (or already moved)
            }
            let mut to = dest.join(g).join(&rel);
            if std::fs::symlink_metadata(&to).is_ok() {
                to = dest.join(backup_dir).join(f).join(&rel);
            }
            if std::fs::symlink_metadata(&to).is_ok() {
                continue; // never overwrite
            }
            target.check_mounted()?;
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ArchiveError::io(parent, e))?;
            }
            std::fs::rename(&from, &to).map_err(|e| ArchiveError::io(&from, e))?;
            moved += 1;
        }
    }
    Ok(moved)
}

/// Tidy `<dest>/.ds-archive-trash` after a clean run: remove the empty folders, except
/// `current` (this run's folder, target-relative). Folders that hold files are never
/// touched: they keep archived files that rsync replaced or the mirror moved aside, and
/// nothing is ever deleted from the archive. Returns how many folders were removed.
pub fn prune_trash(dest: &Path, current: &str) -> std::io::Result<usize> {
    let mut removed = 0;
    for e in std::fs::read_dir(dest.join(TRASH_DIR))? {
        let e = e?;
        if !e.file_type()?.is_dir()
            || format!("{TRASH_DIR}/{}", e.file_name().to_string_lossy()) == current
        {
            continue;
        }
        // `remove_dir` only succeeds on an empty folder.
        if std::fs::remove_dir(e.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// The trash folder name for a run started at `now`: UTC with microseconds.
pub(crate) fn trash_stamp(now: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(now)
        .format("%Y%m%dT%H%M%S%.6fZ")
        .to_string()
}

/// Create a fresh `.ds-archive-trash/<UTC stamp with µs>[-n]` on the target
/// (exclusively, so two runs in one second, or the repeated hour of a DST
/// fall-back, never share one) and return it relative to `dest`.
fn reserve_backup_dir(dest: &Path, now: SystemTime) -> std::io::Result<String> {
    let trash = dest.join(TRASH_DIR);
    std::fs::create_dir_all(&trash)?;
    let stamp = trash_stamp(now);
    for n in 0u32.. {
        let name = if n == 0 {
            stamp.clone()
        } else {
            format!("{stamp}-{n}")
        };
        match std::fs::create_dir(trash.join(&name)) {
            Ok(()) => return Ok(format!("{TRASH_DIR}/{name}")),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("u32 suffixes exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_dirs_are_unique_even_in_the_same_microsecond() {
        let t = tempfile::tempdir().unwrap();
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_micros(1_790_000_000_123_456);
        let a = reserve_backup_dir(t.path(), now).unwrap();
        let b = reserve_backup_dir(t.path(), now).unwrap();
        assert_eq!(a, ".ds-archive-trash/20260921T141320.123456Z");
        assert_eq!(b, format!("{a}-1"));
        assert!(t.path().join(&b).is_dir());
        let file = t.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(reserve_backup_dir(&file, now).is_err());
    }
}
