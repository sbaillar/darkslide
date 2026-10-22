//! The global index: every registered session and every original's hash, for
//! cross-session search and "already imported?" dedupe. Only an index; never
//! needed to open a session.
//!
//! Search across sessions: each session's embedded originals (`search_items`:
//! the path the app shows, and the content key) and the vectors themselves
//! (`embeddings`, f16, shared by every session holding the same content).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::collections::{HashMap, HashSet};

use ds_core::FileHash;
use ds_core::embedding::{self, EmbeddingMatrix, RankOptions};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::is_corruption;
use crate::error::{IoContext, Result, SessionError};
use crate::find::{FIND_SCHEMA, TO_V4};
use crate::library::{LIBRARY_SCHEMA, LibraryCache, bump_library, to_v6};
use crate::session::{Session, snapshot};

const GLOBAL_SCHEMA_VERSION: i64 = 6;

/// v1. Idempotent: two openers racing on a new file both run it.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, root TEXT NOT NULL UNIQUE, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS originals(
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY(session_id, rel_path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS originals_hash ON originals(hash);
";

/// v2: cross-session search (applied on top of v1). Idempotent: two openers
/// racing on a v1 file both run it.
const SEARCH_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS search_items(
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    key BLOB NOT NULL,
    PRIMARY KEY(session_id, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS search_items_key ON search_items(key);
CREATE TABLE IF NOT EXISTS embeddings(
    key BLOB NOT NULL,
    model TEXT NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(key, model)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS search_generation(id INTEGER PRIMARY KEY CHECK (id = 0), n INTEGER NOT NULL);
INSERT OR IGNORE INTO search_generation VALUES (0, 0);
";

/// A cross-session search result.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalSearchHit {
    pub session_root: PathBuf,
    pub session_name: String,
    /// The original's path.
    pub path: PathBuf,
    pub score: f32,
}

/// Search data in memory, valid for one `search_generation`.
#[derive(Debug, Default)]
pub(crate) struct SearchCache {
    pub(crate) generation: i64,
    model: String,
    pub(crate) matrix: EmbeddingMatrix,
    /// Content key → matrix row.
    pub(crate) rows: HashMap<[u8; 32], usize>,
    /// `(session id, path, matrix row)` of every item with a vector.
    items: Vec<(i64, PathBuf, usize)>,
    sessions: HashMap<i64, (PathBuf, String)>,
    loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSession {
    pub id: i64,
    pub root: PathBuf,
    pub name: String,
}

/// Where a hash was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalHit {
    pub session_root: PathBuf,
    pub session_name: String,
    pub rel_path: String,
}

#[derive(Debug)]
pub struct GlobalIndex {
    pub(crate) conn: Connection,
    pub(crate) search: SearchCache,
    pub(crate) find: crate::find::FindCache,
    pub(crate) library: LibraryCache,
}

fn root_str(root: &Path) -> Result<&str> {
    root.to_str().ok_or_else(|| SessionError::InvalidPath {
        path: root.to_string_lossy().into_owned(),
        reason: "session paths must be UTF-8",
    })
}

impl GlobalIndex {
    /// `~/Library/Application Support/Darkslide/global.db`.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|h| Path::new(&h).join("Library/Application Support/Darkslide/global.db"))
    }

    /// Open or create. A corrupt file is replaced (it only mirrors the
    /// sessions, which can be re-synced). Any other failure (busy, permission,
    /// I/O, a newer build's schema) is returned and the file left alone.
    pub fn open(path: &Path) -> Result<GlobalIndex> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).at(dir)?;
        }
        match Self::try_open(path) {
            Err(SessionError::Sqlite(e)) if is_corruption(&e) => {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = fs::remove_file(format!("{}{suffix}", path.display()));
                }
                Self::try_open(path)
            }
            other => other,
        }
    }

    fn try_open(path: &Path) -> Result<GlobalIndex> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA trusted_schema=OFF;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SCHEMA} {SEARCH_SCHEMA} {FIND_SCHEMA} {LIBRARY_SCHEMA} PRAGMA user_version={GLOBAL_SCHEMA_VERSION}; COMMIT;"
            ))?,
            // v1 -> v2 only adds the search tables, v2 -> v3 the find rows (with v4's
            // titles and v5's positions), v5 -> v6 the Library.
            1 => conn.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SEARCH_SCHEMA} {FIND_SCHEMA} {LIBRARY_SCHEMA} PRAGMA user_version={GLOBAL_SCHEMA_VERSION}; COMMIT;"
            ))?,
            2 => conn.execute_batch(&format!(
                "BEGIN IMMEDIATE; {FIND_SCHEMA} {LIBRARY_SCHEMA} PRAGMA user_version={GLOBAL_SCHEMA_VERSION}; COMMIT;"
            ))?,
            // v3 -> v4 adds the titles column (unless a racing opener already did),
            // v4 -> v5 the map's positions on the find rows (filled by the next sync).
            3 | 4 => {
                if version == 3 {
                    let tx = rusqlite::Transaction::new_unchecked(
                        &conn,
                        rusqlite::TransactionBehavior::Immediate,
                    )?;
                    if !crate::db::has_column(&tx, "find_items", "titles")? {
                        tx.execute_batch(TO_V4)?;
                    }
                    tx.execute_batch("PRAGMA user_version=4")?;
                    tx.commit()?;
                }
                crate::map::find_items_to_v5(&conn, 5)?;
                to_v6(&conn, GLOBAL_SCHEMA_VERSION)?;
            }
            5 => to_v6(&conn, GLOBAL_SCHEMA_VERSION)?,
            GLOBAL_SCHEMA_VERSION => {}
            _ => {
                return Err(SessionError::IndexTooNew {
                    found: version,
                    supported: GLOBAL_SCHEMA_VERSION,
                });
            }
        }
        crate::map::ensure_positions(&conn)?;
        Ok(GlobalIndex {
            conn,
            search: SearchCache::default(),
            find: crate::find::FindCache::default(),
            library: LibraryCache::default(),
        })
    }

    pub fn register_session(&mut self, root: &Path, name: &str) -> Result<i64> {
        let r = root_str(root)?;
        let known: Option<(i64, String)> = self
            .conn
            .query_row("SELECT id, name FROM sessions WHERE root = ?", [r], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some((id, old)) = &known
            && old == name
        {
            return Ok(*id);
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(root, name) VALUES (?, ?) ON CONFLICT(root) DO UPDATE SET name = excluded.name",
            params![r, name],
        )?;
        let id = tx.query_row("SELECT id FROM sessions WHERE root = ?", [r], |row| {
            row.get(0)
        })?;
        // Search results and the Library show session names: a rename must reach the caches.
        bump_generation(&tx)?;
        bump_library(&tx)?;
        tx.commit()?;
        Ok(id)
    }

    /// Forget a session, its originals and its search items, and the vectors
    /// no other session uses. Returns whether it was registered.
    pub fn unregister_session(&mut self, root: &Path) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM sessions WHERE root = ?", [root_str(root)?])?;
        prune_embeddings(&tx)?;
        bump_generation(&tx)?;
        bump_library(&tx)?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn sessions(&self) -> Result<Vec<RegisteredSession>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, root, name FROM sessions ORDER BY name, root")?;
        Ok(st
            .query_map([], |r| {
                Ok(RegisteredSession {
                    id: r.get(0)?,
                    root: PathBuf::from(r.get::<_, String>(1)?),
                    name: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Register `session` and replace its originals with the current set.
    /// Returns how many originals are recorded.
    pub fn sync_session(&mut self, session: &Session) -> Result<usize> {
        let id = self.register_session(session.root(), session.name())?;
        let images = session.images()?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM originals WHERE session_id = ?", [id])?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO originals(session_id, rel_path, hash) VALUES (?, ?, ?)",
            )?;
            for img in &images {
                st.execute(params![id, img.rel_path.as_str(), &img.hash.0[..]])?;
            }
        }
        tx.commit()?;
        Ok(images.len())
    }

    /// Record one original (e.g. right after an import copies it).
    pub fn add_original(&mut self, root: &Path, rel_path: &str, hash: &FileHash) -> Result<()> {
        let r = root_str(root)?;
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM sessions WHERE root = ?", [r], |row| {
                row.get(0)
            })
            .optional()?;
        let id = if let Some(id) = id {
            id
        } else {
            let name = root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(r)
                .to_owned();
            self.register_session(root, &name)?
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO originals(session_id, rel_path, hash) VALUES (?, ?, ?)",
            params![id, rel_path, &hash.0[..]],
        )?;
        Ok(())
    }

    /// Has a file with this content been imported into any session?
    pub fn contains(&self, hash: &FileHash) -> Result<bool> {
        let mut st = self
            .conn
            .prepare_cached("SELECT 1 FROM originals WHERE hash = ?")?;
        Ok(st.exists([&hash.0[..]])?)
    }

    /// Every place this content lives.
    pub fn lookup(&self, hash: &FileHash) -> Result<Vec<GlobalHit>> {
        let mut st = self.conn.prepare_cached(
            "SELECT s.root, s.name, o.rel_path FROM originals o JOIN sessions s ON s.id = o.session_id
             WHERE o.hash = ? ORDER BY s.root, o.rel_path",
        )?;
        Ok(st
            .query_map([&hash.0[..]], |r| {
                Ok(GlobalHit {
                    session_root: PathBuf::from(r.get::<_, String>(0)?),
                    session_name: r.get(1)?,
                    rel_path: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every recorded original: `(session root, rel_path, hash)` (import dedupe
    /// builds its size/partial-hash lookup from these).
    pub fn originals(&self) -> Result<Vec<(PathBuf, String, FileHash)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT s.root, o.rel_path, o.hash FROM originals o JOIN sessions s ON s.id = o.session_id
             ORDER BY s.root, o.rel_path",
        )?;
        let rows = st.query_map([], |r| {
            let hash: Vec<u8> = r.get(2)?;
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                r.get::<_, String>(1)?,
                hash,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (root, rel, hash) = row?;
            // A malformed hash can't match anything: skip it rather than fail.
            if let Ok(h) = <[u8; 32]>::try_from(hash.as_slice()) {
                out.push((root, rel, FileHash(h)));
            }
        }
        Ok(out)
    }

    pub fn snapshot_to(&self, path: &Path) -> Result<()> {
        snapshot(&self.conn, path)
    }

    // ---------------------------------------------------------------- search ---

    /// Register the session at `root` (the folder the app opens) and replace
    /// its searchable originals with `items` (`(path, content key)`).
    pub fn sync_search_items(
        &mut self,
        root: &Path,
        name: &str,
        items: &[(PathBuf, FileHash)],
    ) -> Result<bool> {
        let id = self.register_session(root, name)?;
        let mut wanted: Vec<(String, [u8; 32])> = items
            .iter()
            .map(|(p, k)| (p.to_string_lossy().into_owned(), k.0))
            .collect();
        wanted.sort();
        wanted.dedup_by(|a, b| a.0 == b.0);
        // Nothing changed (the usual case while indexing polls): no write, and
        // the search cache stays valid.
        let had = self.search_items_of(id)?;
        if had == wanted {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM search_items WHERE session_id = ?", [id])?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO search_items(session_id, path, key) VALUES (?, ?, ?)",
            )?;
            for (path, key) in &wanted {
                st.execute(params![id, path, &key[..]])?;
            }
            // Photos removed from the session (forgotten, trashed): their vectors
            // go too unless another item still uses them.
            let mut prune = tx.prepare(
                "DELETE FROM embeddings WHERE key = ?
                 AND NOT EXISTS (SELECT 1 FROM search_items WHERE key = embeddings.key)",
            )?;
            let kept: HashSet<[u8; 32]> = wanted.iter().map(|w| w.1).collect();
            for key in had.iter().map(|h| h.1).filter(|k| !kept.contains(k)) {
                prune.execute([&key[..]])?;
            }
        }
        bump_generation(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    /// Add or update some of a session's searchable originals (after a batch
    /// was embedded), leaving the others. Returns whether anything changed.
    pub fn add_search_items(
        &mut self,
        root: &Path,
        name: &str,
        items: &[(PathBuf, FileHash)],
    ) -> Result<bool> {
        let id = self.register_session(root, name)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut changed = 0;
        {
            let mut st = tx.prepare(
                "INSERT INTO search_items(session_id, path, key) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, path) DO UPDATE SET key = excluded.key
                 WHERE search_items.key <> excluded.key",
            )?;
            for (path, key) in items {
                changed += st.execute(params![id, path.to_string_lossy(), &key.0[..]])?;
            }
        }
        if changed > 0 {
            bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    fn search_items_of(&self, session: i64) -> Result<Vec<(String, [u8; 32])>> {
        let mut st = self.conn.prepare_cached(
            "SELECT path, key FROM search_items WHERE session_id = ? ORDER BY path",
        )?;
        let rows: Vec<(String, Vec<u8>)> = st
            .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(p, k)| Some((p, <[u8; 32]>::try_from(k).ok()?)))
            .collect())
    }

    /// The session at `root` is no longer offered (forgotten or deleted): drop
    /// its searchable originals, and the session itself unless import dedupe
    /// still needs its originals; vectors no session uses any more are pruned.
    /// Returns whether it had search items.
    pub fn forget_search(&mut self, root: &Path) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "DELETE FROM search_items WHERE session_id IN (SELECT id FROM sessions WHERE root = ?)",
            [root_str(root)?],
        )?;
        tx.execute(
            "DELETE FROM find_items WHERE session_id IN (SELECT id FROM sessions WHERE root = ?)",
            [root_str(root)?],
        )?;
        // The Library forgets it too (its collections keep their entries while
        // the session stays registered for dedupe).
        for table in ["library_items", "library_sources"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE session_id IN (SELECT id FROM sessions WHERE root = ?)"
                ),
                [root_str(root)?],
            )?;
        }
        tx.execute(
            "DELETE FROM sessions WHERE root = ?
             AND NOT EXISTS (SELECT 1 FROM originals o WHERE o.session_id = sessions.id)",
            [root_str(root)?],
        )?;
        prune_embeddings(&tx)?;
        bump_generation(&tx)?;
        bump_library(&tx)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Vectors stored for `model` (diagnostics, tests).
    pub fn embedding_count(&self, model: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM embeddings WHERE model = ?",
            [model],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Which of `keys` have no vector for `model` here.
    pub fn missing_embeddings(&self, model: &str, keys: &[FileHash]) -> Result<Vec<FileHash>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT 1 FROM embeddings WHERE key = ? AND model = ?")?;
        let mut out = Vec::new();
        for k in keys {
            if !st.exists(params![&k.0[..], model])? {
                out.push(*k);
            }
        }
        Ok(out)
    }

    /// Stored vectors for those of `keys` that have one (unit length).
    pub fn embeddings(&self, model: &str, keys: &[FileHash]) -> Result<Vec<(FileHash, Vec<f32>)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT vec FROM embeddings WHERE key = ? AND model = ?")?;
        let mut out = Vec::new();
        for k in keys {
            let v: Option<Vec<u8>> = st
                .query_row(params![&k.0[..], model], |r| r.get(0))
                .optional()?;
            if let Some(v) = v.and_then(|b| embedding::decode_f16(&b)) {
                out.push((*k, v));
            }
        }
        Ok(out)
    }

    /// Store vectors (normalized, f16). Unusable vectors are skipped; returns
    /// how many were stored.
    pub fn store_embeddings(
        &mut self,
        model: &str,
        items: &[(FileHash, Vec<f32>)],
    ) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO embeddings(key, model, vec) VALUES (?, ?, ?)",
            )?;
            for (key, v) in items {
                if let Some(unit) = embedding::normalized(v) {
                    st.execute(params![&key.0[..], model, embedding::encode_f16(&unit)])?;
                    n += 1;
                }
            }
        }
        if n > 0 {
            bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Originals of every session ranked by similarity to `query`, best first.
    /// The same content in two sessions is a hit in each.
    pub fn search(
        &mut self,
        model: &str,
        query: &[f32],
        opts: RankOptions,
    ) -> Result<Vec<GlobalSearchHit>> {
        let Some(q) = embedding::normalized(query) else {
            return Ok(Vec::new());
        };
        self.load_search(model)?;
        let c = &self.search;
        let scored = c
            .items
            .iter()
            .enumerate()
            .map(|(i, (_, _, row))| (i, embedding::dot(c.matrix.row(*row), &q)));
        Ok(embedding::rank(scored, opts)
            .into_iter()
            .filter_map(|(i, score)| {
                let (sid, path, _) = &c.items[i];
                let (root, name) = c.sessions.get(sid)?;
                Some(GlobalSearchHit {
                    session_root: root.clone(),
                    session_name: name.clone(),
                    path: path.clone(),
                    score,
                })
            })
            .collect())
    }

    pub(crate) fn generation(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT n FROM search_generation", [], |r| r.get(0))?)
    }

    /// (Re)load the search data when another write happened since.
    pub(crate) fn load_search(&mut self, model: &str) -> Result<()> {
        let generation = self.generation()?;
        let c = &self.search;
        if c.loaded && c.generation == generation && c.model == model {
            return Ok(());
        }
        let mut cache = SearchCache {
            generation,
            model: model.to_owned(),
            loaded: true,
            ..SearchCache::default()
        };
        let encoded = {
            let mut st = self
                .conn
                .prepare_cached("SELECT key, vec FROM embeddings WHERE model = ?")?;
            st.query_map([model], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let (matrix, keys) = EmbeddingMatrix::from_f16_rows(encoded);
        cache.matrix = matrix;
        let rows: HashMap<Vec<u8>, usize> =
            keys.into_iter().enumerate().map(|(i, k)| (k, i)).collect();
        cache.rows = rows
            .iter()
            .filter_map(|(k, &i)| Some((<[u8; 32]>::try_from(k.as_slice()).ok()?, i)))
            .collect();
        {
            let mut st = self.conn.prepare_cached(
                "SELECT session_id, path, key FROM search_items ORDER BY session_id, path",
            )?;
            let mut q = st.query([])?;
            while let Some(r) = q.next()? {
                let key: Vec<u8> = r.get(2)?;
                if let Some(&row) = rows.get(&key) {
                    cache
                        .items
                        .push((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?), row));
                }
            }
        }
        for s in self.sessions()? {
            cache.sessions.insert(s.id, (s.root, s.name));
        }
        self.search = cache;
        Ok(())
    }
}

/// Drop vectors no session's search items use any more.
fn prune_embeddings(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM embeddings WHERE key NOT IN (SELECT key FROM search_items)",
        [],
    )?;
    crate::find::prune_faces(tx)
}

pub(crate) fn bump_generation(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute("UPDATE search_generation SET n = n + 1", [])?;
    Ok(())
}
