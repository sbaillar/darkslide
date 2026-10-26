//! Batch journal: makes a multi-sidecar write (a rating on 500 photos, an
//! "Edit All Selected" change, an AI Cull) all-or-nothing across crashes.
//!
//! Before a batch touches any sidecar, the full new content of every sidecar
//! is written, atomically, to `<root>/.lt/journal/<nanos>-<id>.json`, where
//! `<nanos>` is the time the batch began. The sidecars are then written (each
//! atomically) and the journal is removed, whether or not every write
//! succeeded: a running session reports and indexes exactly the writes that
//! landed, so a failed write is resolved there and then, not left behind. If
//! the process dies in between, the journal is still there, and the next
//! [`crate::Session::open`] rolls the batch forward before indexing, so the
//! session never shows half a batch. A journal that can't be parsed was
//! never completely written, so no sidecar of that batch was touched, and it
//! is simply discarded.
//!
//! Replay never puts back content older than the disk: an entry is skipped
//! when its sidecar was modified after the batch began (by the batch itself,
//! or by a later edit). Leftover journals are applied oldest-first, the
//! newest journal's entry winning for a sidecar; journals named by older
//! builds (`<id>.json`) are dated by their file's mtime.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::{IoContext, Result};
use crate::relpath::rel_string;

pub(crate) const JOURNAL_DIR: &str = ".lt/journal";

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Session-relative sidecar path.
    rel: String,
    /// The sidecar's full new JSON.
    json: String,
    /// The batch creates this sidecar (it did not exist when the batch began).
    #[serde(default)]
    created: bool,
}

/// `<Capture|Selects|Trash>/…/.lt/settings/[v<N>/]<file>.json`: the only
/// paths a batch of sidecar writes can have. A journal from a folder someone
/// else made must not be able to rewrite `session.json` or `albums.json`.
fn is_sidecar_rel(rel: &str) -> bool {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let Some(name) = parts
        .pop()
        .filter(|n| Path::new(n).extension().is_some_and(|e| e == "json"))
    else {
        return false;
    };
    if name.starts_with('.') {
        return false;
    }
    // An optional `v<N>` variant folder, then `.lt/settings`.
    if parts.last().is_some_and(|v| {
        v.strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }) {
        parts.pop();
    }
    let [top, .., ds, settings] = parts.as_slice() else {
        return false;
    };
    ["Capture", "Selects", "Trash"].contains(top) && *ds == ".lt" && *settings == "settings"
}

/// Record `items` (absolute sidecar path, new bytes) before writing them.
/// Single writes are already atomic and need no journal (`Ok(None)`).
pub(crate) fn begin(root: &Path, items: &[(PathBuf, Vec<u8>)]) -> io::Result<Option<PathBuf>> {
    if items.len() < 2 {
        return Ok(None);
    }
    let began = nanos(SystemTime::now());
    let mut entries = Vec::with_capacity(items.len());
    for (path, bytes) in items {
        let Some(rel) = rel_string(root, path) else {
            return Err(io::Error::other("journal entry outside the session"));
        };
        entries.push(Entry {
            created: fs::symlink_metadata(path).is_err(),
            rel,
            json: String::from_utf8_lossy(bytes).into_owned(),
        });
    }
    let dir = root.join(JOURNAL_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{began:020}-{}.json", crate::session::random_id()));
    let bytes = serde_json::to_vec(&entries).map_err(io::Error::other)?;
    atomic::write_atomic(&path, &bytes)?;
    Ok(Some(path))
}

/// The batch's writes are done (or failed and were reported): drop its journal.
pub(crate) fn finish(journal: Option<PathBuf>) {
    if let Some(p) = journal {
        let _ = fs::remove_file(p);
    }
}

fn nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

fn mtime(path: &Path) -> Option<u128> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(nanos)
}

/// When the journal's batch began: from its name, or for an old-format
/// `<id>.json` name, its mtime.
fn began(path: &Path) -> u128 {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.split_once('-'))
        .and_then(|(t, _)| t.parse().ok())
        .or_else(|| mtime(path))
        .unwrap_or_default()
}

/// Roll forward every journaled batch left by a crash, oldest first, never
/// over a sidecar modified after its batch began. Returns how many batches
/// were finished.
pub(crate) fn replay(root: &Path) -> Result<usize> {
    let dir = root.join(JOURNAL_DIR);
    let Ok(rd) = fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut journals: Vec<(u128, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| (began(&p), p))
        .collect();
    journals.sort();
    let mut done = 0;
    // Sidecar → the newest journaled content still due. Nothing is written
    // until every journal is read, so each mtime is the pre-replay one.
    // A sidecar that is gone is not recreated (its variant was deleted or moved
    // after the crash) unless the batch itself created it.
    let mut due: BTreeMap<String, String> = BTreeMap::new();
    for (began, path) in &journals {
        let entries: Option<Vec<Entry>> = atomic::read_json_file(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let Some(entries) = entries else { continue };
        done += 1;
        for e in entries {
            // A journal from before the `.ds` → `.lt` migration names the old folders.
            let rel = crate::legacy::current_rel(&e.rel);
            let current = mtime(&root.join(&rel));
            if current.map_or(e.created, |m| m <= *began) {
                due.insert(rel, e.json);
            }
        }
    }
    let items: Vec<(PathBuf, Vec<u8>)> = due
        .into_iter()
        .map(|(rel, json)| Entry {
            rel,
            json,
            created: false,
        })
        // Only sidecar paths inside the session (no absolute or `..` paths).
        .filter(|e| is_sidecar_rel(&e.rel) && e.rel.split('/').all(|c| !c.is_empty() && c != ".."))
        // Never through a symlinked component (a crafted `.lt/session`
        // could otherwise make replay write outside the store).
        .filter(|e| crate::relpath::ensure_no_symlinks(root, &e.rel).is_ok())
        .map(|e| (root.join(&e.rel), e.json.into_bytes()))
        .collect();
    for (p, _) in &items {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
    }
    // Re-check: a symlink created meanwhile is followed by create_dir_all.
    let items: Vec<_> = items
        .into_iter()
        .filter(|(p, _)| {
            rel_string(root, p)
                .is_some_and(|r| crate::relpath::ensure_no_symlinks(root, &r).is_ok())
        })
        .collect();
    // One unwritable sidecar (a read-only folder) must not lock the session:
    // the journals stay for the next open and the rest is rolled forward.
    let mut complete = true;
    for res in atomic::write_many(&items, None) {
        complete &= res.is_ok();
    }
    if complete {
        for (_, path) in &journals {
            fs::remove_file(path).at(path)?;
        }
    }
    Ok(done)
}
