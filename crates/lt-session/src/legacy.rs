//! Sessions made before the rename (Darkslide 0.1.x) keep their data in `.ds`
//! folders (`<root>/.ds`, `Capture/…/.ds`, `Trash/.ds`, and `<folder>/.ds` around
//! an in-place store) and their index in `<name>.dsession`. Luxtable uses `.lt`
//! and `<name>.ltsession`. [`migrate`] moves an old session over the first time it
//! is opened, with renames on the same volume: nothing is copied, nothing is deleted.
//!
//! - It runs before the index is opened, so nothing holds the database across a rename.
//! - The top-level `.ds` → `.lt` rename goes first; if it fails (read-only volume,
//!   permissions, a share that refuses it) nothing has changed and the session is
//!   not opened ([`SessionError::LegacyMigration`] says why), so it is never half
//!   migrated by a failure. A later failure renames everything back.
//! - A marker (`.lt/migrating-from-ds`) is written right after the first rename and
//!   removed last: a migration interrupted by a crash is finished on the next open.
//! - When both `.ds` and `.lt` exist, `.lt` is used and `.ds` is left untouched
//!   ([`Legacy::BothPresent`], for the caller to log).
//!
//! A migrated session can't be opened by Darkslide 0.1.x any more (it looks for `.ds`).

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{IoContext, Result, SessionError};

/// Migration source: the data folder Darkslide 0.1.x kept in every session folder.
pub const LEGACY_DIR: &str = ".ds";
/// Migration source: the index extension Darkslide 0.1.x used (`<name>.dsession`).
pub const LEGACY_DB_EXT: &str = "dsession";
/// Migration source: Darkslide's temp-file mark (leftovers are cleaned like ours).
pub(crate) const LEGACY_TEMP_MARK: &str = ".dstmp-";

const DIR: &str = ".lt";
const DB_EXT: &str = "ltsession";
/// In `<root>/.lt` while a migration is under way.
const MARKER: &str = "migrating-from-ds";
/// Sidecar folders sit at most this deep under a store (folders of folders of photos).
const MAX_DEPTH: usize = 64;

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Legacy {
    /// Moved over: how many `.ds` folders were renamed.
    Migrated { folders: usize },
    /// Both `.ds` and `.lt` exist at the top: `.lt` is used, `.ds` left as it is.
    BothPresent,
}

/// Whether `dir` is a session store, in the current layout or the old one.
pub fn is_session_dir(dir: &Path) -> bool {
    dir.join(DIR).join("settings/session.json").is_file()
        || dir.join(LEGACY_DIR).join("settings/session.json").is_file()
}

/// The session-relative path `rel` with an old `.ds` segment named `.lt`
/// (journals and the index written before the migration).
pub(crate) fn current_rel(rel: &str) -> String {
    rel.split('/')
        .map(|s| if s == LEGACY_DIR { DIR } else { s })
        .collect::<Vec<_>>()
        .join("/")
}

/// Move the old layout under `dir` (a session store, or a folder holding an
/// in-place store in `.ds/session`) to the new one. `Ok(None)`: nothing to do.
pub fn migrate(dir: &Path) -> Result<Option<Legacy>> {
    let old = dir.join(LEGACY_DIR);
    let new = dir.join(DIR);
    if new.exists() {
        if new.join(MARKER).exists() {
            return finish(dir).map(|n| Some(Legacy::Migrated { folders: n + 1 }));
        }
        return Ok(is_real_dir(&old).then_some(Legacy::BothPresent));
    }
    if !is_real_dir(&old) {
        return Ok(None);
    }
    // A Darkslide still running on this session would lose its index under it.
    for db in legacy_dbs(dir) {
        checkpoint(&db)?;
    }
    fs::rename(&old, &new).map_err(|e| failed(dir, &e.to_string()))?;
    if let Err(e) = fs::write(new.join(MARKER), b"") {
        let _ = fs::rename(&new, &old);
        return Err(failed(dir, &e.to_string()));
    }
    finish(dir).map(|n| Some(Legacy::Migrated { folders: n + 1 }))
}

fn failed(dir: &Path, why: &str) -> SessionError {
    SessionError::LegacyMigration {
        path: dir.to_owned(),
        reason: why.to_owned(),
    }
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Rename the remaining `.ds` folders and the index, fix the index's paths,
/// then drop the marker. Returns how many folders it renamed. On failure,
/// everything this pass renamed is put back, the top-level one included.
fn finish(dir: &Path) -> Result<usize> {
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = rename_all(dir, &mut done);
    if let Err(e) = result {
        for (from, to) in done.iter().rev() {
            let _ = fs::rename(to, from);
        }
        let new = dir.join(DIR);
        let _ = fs::remove_file(new.join(MARKER));
        let _ = fs::rename(&new, dir.join(LEGACY_DIR));
        return Err(e);
    }
    // Only an index: a failure here leaves its old paths, which the next sync
    // of the sidecars replaces.
    for db in current_dbs(dir) {
        let _ = fix_index(&db);
    }
    fs::remove_file(dir.join(DIR).join(MARKER)).at(dir.join(DIR).join(MARKER))?;
    Ok(done.iter().filter(|(_, to)| to.is_dir()).count())
}

fn rename_all(dir: &Path, done: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    // Only a store's own tree has sidecar folders; a folder around an in-place
    // store (the user's photos) has just its top-level one, already renamed.
    if dir.join(DIR).join("settings").is_dir() {
        let mut stack = vec![(dir.to_path_buf(), 0usize)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let Ok(t) = e.file_type() else { continue };
                if !t.is_dir() {
                    continue;
                }
                let p = e.path();
                if e.file_name() == LEGACY_DIR {
                    let to = d.join(DIR);
                    if to.exists() {
                        continue; // both: the new one wins, the old one stays
                    }
                    fs::rename(&p, &to).map_err(|err| failed(&p, &err.to_string()))?;
                    done.push((p, to));
                } else if depth < MAX_DEPTH && e.file_name() != DIR {
                    stack.push((p, depth + 1));
                }
            }
        }
    }
    for db in legacy_dbs(dir) {
        let to = db.with_extension(DB_EXT);
        if to.exists() {
            continue;
        }
        checkpoint(&db)?;
        fs::rename(&db, &to).map_err(|e| failed(&db, &e.to_string()))?;
        done.push((db, to));
    }
    Ok(())
}

fn dbs(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == ext) && p.is_file())
        .collect();
    v.sort();
    v
}

fn legacy_dbs(dir: &Path) -> Vec<PathBuf> {
    dbs(dir, LEGACY_DB_EXT)
}

fn current_dbs(dir: &Path) -> Vec<PathBuf> {
    dbs(dir, DB_EXT)
}

/// Fold the index's write-ahead log into it, so the file can be renamed on
/// its own. A log that stays (another process has the index open) stops the
/// migration: renaming the file away from its log would lose edits.
fn checkpoint(db: &Path) -> Result<()> {
    let side = |s: &str| PathBuf::from(format!("{}{s}", db.display()));
    if side("-wal").exists() || side("-journal").exists() {
        let conn = Connection::open(db)?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        conn.close().map_err(|(_, e)| e)?;
    }
    if side("-wal").exists() || side("-journal").exists() {
        return Err(failed(
            db,
            "its index is in use (quit Darkslide and open it again)",
        ));
    }
    let _ = fs::remove_file(side("-shm")); // a shared-memory index only, rebuilt on open
    Ok(())
}

/// The index stores each variant's sidecar path (`Capture/.ds/settings/…`).
fn fix_index(db: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    let has_variants: bool = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'variants'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if has_variants {
        conn.execute(
            "UPDATE variants SET sidecar = substr(replace('/' || sidecar, '/.ds/', '/.lt/'), 2)
             WHERE instr('/' || sidecar, '/.ds/') > 0",
            [],
        )?;
    }
    conn.close().map_err(|(_, e)| e)?;
    Ok(())
}

/// The global index registers an in-place session by its store,
/// `<folder>/.ds/session` before the migration: point those at `.lt/session`.
/// A store registered under both names keeps its `.lt` row.
pub(crate) fn fix_global(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE OR IGNORE sessions SET root = substr(root, 1, length(root) - length('/.ds/session')) || '/.lt/session'
         WHERE root LIKE '%/.ds/session'",
        [],
    )?;
    Ok(())
}

impl crate::GlobalIndex {
    /// Point every session registered under `from` at the same place under `to`
    /// (the app's support folder was copied to its new name, with the stores it
    /// keeps for folders that can't hold their own). A root already registered
    /// under `to` keeps its row. Returns how many moved.
    pub fn relocate_roots(&mut self, from: &Path, to: &Path) -> Result<usize> {
        let (Some(from), Some(to)) = (from.to_str(), to.to_str()) else {
            return Ok(0);
        };
        let (from, to) = (from.trim_end_matches('/'), to.trim_end_matches('/'));
        if from.is_empty() || from == to {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE OR IGNORE sessions SET root = ?2 || substr(root, length(?1) + 1)
             WHERE substr(root, 1, length(?1) + 1) = ?1 || '/'",
            rusqlite::params![from, to],
        )?;
        crate::global::bump_generation(&tx)?;
        crate::library::bump_library(&tx)?;
        tx.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
