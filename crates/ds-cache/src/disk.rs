//! On-disk JPEG cache under `<session>/.ds/cache/`.
//!
//! Layout:
//! ```text
//! .ds/cache/index.db    SQLite: key → size, atime (LRU clock); persistence only
//! .ds/cache/thumb/<variant:016x>_<original>_<adj>_p<PIPELINE_VERSION>.jpg
//! .ds/cache/preview/…   same naming
//! ```
//! A rendition is identified by the variant, the BLAKE3 of the **original
//! file's content**, the adjustments hash, and [`PIPELINE_VERSION`], so a file
//! replaced under the same variant id or a pipeline change never serves a
//! stale render.
//!
//! The cache is disposable: a missing, corrupt, or outdated index (including
//! one written for another pipeline version) is deleted and rebuilt by
//! scanning the files, and files that do not match the current naming are
//! removed. Writes are atomic (temp file in the destination directory → fsync
//! → rename → fsync directory).
//!
//! The index is mirrored in memory. `get` only bumps an in-memory logical
//! clock; dirty access times are written to SQLite in one transaction when
//! [`FLUSH_BATCH`] accumulate, when [`FLUSH_INTERVAL`] has passed at a later
//! operation, on [`DiskCache::flush`], and on drop. Eviction uses the
//! in-memory order, so it is LRU-exact regardless of flushing. One
//! `DiskCache` per cache directory is assumed; a second process sharing the
//! directory only sees the other's entries after a reopen (a miss re-renders).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ds_core::{Adjustments, FileHash};
use rusqlite::{Connection, params};

/// Index file name inside the cache root.
pub const INDEX_FILE: &str = "index.db";
/// Rendition version ([`ds_core::PIPELINE_VERSION`], the single source of truth): bump it
/// whenever rendering output changes (pipeline math, JPEG encoder settings, thumbnail
/// sizes); every cached rendition is then discarded.
pub use ds_core::PIPELINE_VERSION;
const SCHEMA_VERSION: i32 = 2;
/// `user_version` stamped in the index: schema and pipeline together.
const INDEX_VERSION: i32 = (SCHEMA_VERSION << 16) | PIPELINE_VERSION as i32;
const TMP_SUFFIX: &str = ".tmp";
/// Dirty access times flushed once this many accumulate.
pub const FLUSH_BATCH: usize = 512;
/// ...or once this much time has passed since the last flush.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Which rendition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// 512 px long edge.
    Thumb,
    /// 2560 px long edge.
    Preview,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Thumb, Kind::Preview];

    /// Long-edge size in pixels the caller should render at.
    pub fn max_edge(self) -> u32 {
        match self {
            Kind::Thumb => 512,
            Kind::Preview => 2560,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Kind::Thumb => "thumb",
            Kind::Preview => "preview",
        }
    }

    fn code(self) -> i64 {
        match self {
            Kind::Thumb => 0,
            Kind::Preview => 1,
        }
    }

    fn from_code(c: i64) -> Option<Self> {
        match c {
            0 => Some(Kind::Thumb),
            1 => Some(Kind::Preview),
            _ => None,
        }
    }
}

/// BLAKE3 of an [`Adjustments`]' canonical JSON.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdjHash(pub [u8; 32]);

impl AdjHash {
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        blake3::Hash::from_hex(s).ok().map(|h| Self(*h.as_bytes()))
    }
}

impl fmt::Debug for AdjHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AdjHash({})", &self.to_hex()[..12])
    }
}

/// Hash of the adjustments as serialized by `serde_json` (fields in declaration
/// order, shortest round-trip floats), so equal stacks always hash equal.
pub fn adjustments_hash(adj: &Adjustments) -> AdjHash {
    let json = serde_json::to_vec(adj).expect("Adjustments always serialize");
    AdjHash(*blake3::hash(&json).as_bytes())
}

/// Identity of a rendition of `adj` whose removals are filled by `removal_engine` (the
/// inpainter's id) and whose depth lens blur reads `depth` (the map's bytes; `None` when
/// it reads none): [`adjustments_hash`], plus the engine when there are removals and the
/// map when there is one, so a model that arrives later (a better fill, the depth model
/// replacing the fallback depth) re-renders the edits it changes.
pub fn render_hash(adj: &Adjustments, removal_engine: &str, depth: Option<&[u8]>) -> AdjHash {
    let h = adjustments_hash(adj);
    if adj.removals.is_empty() && depth.is_none() {
        return h;
    }
    let mut b = blake3::Hasher::new();
    b.update(&h.0);
    if !adj.removals.is_empty() {
        b.update(removal_engine.as_bytes());
    }
    if let Some(d) = depth {
        b.update(b"depth").update(d);
    }
    AdjHash(*b.finalize().as_bytes())
}

/// Identity of one cached rendition. The pipeline version is implicit
/// ([`PIPELINE_VERSION`]) and part of the file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey {
    pub variant: u64,
    /// BLAKE3 of the original file's bytes (the session's `FileHash`).
    pub original: FileHash,
    pub adj: AdjHash,
    pub kind: Kind,
}

impl CacheKey {
    pub fn new(variant: u64, original: FileHash, adj: AdjHash, kind: Kind) -> Self {
        Self {
            variant,
            original,
            adj,
            kind,
        }
    }

    fn file_name(&self) -> String {
        format!(
            "{:016x}_{}_{}_p{PIPELINE_VERSION}.jpg",
            self.variant,
            self.original.to_hex(),
            self.adj.to_hex()
        )
    }

    /// Parse a current-format file name; anything else (older naming, other
    /// pipeline versions, junk) is `None`.
    fn parse(kind: Kind, name: &str) -> Option<Self> {
        let stem = name.strip_suffix(&format!("_p{PIPELINE_VERSION}.jpg"))?;
        let mut parts = stem.split('_');
        let (v, o, a) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || v.len() != 16 {
            return None;
        }
        Some(Self {
            variant: u64::from_str_radix(v, 16).ok()?,
            original: FileHash::from_hex(o)?,
            adj: AdjHash::from_hex(a)?,
            kind,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache I/O: {0}")]
    Io(#[from] io::Error),
    #[error("cache index: {0}")]
    Index(#[from] rusqlite::Error),
    #[error("cache index version {found:#x} unsupported (want {INDEX_VERSION:#x})")]
    Schema { found: i32 },
    #[error("entry of {size} bytes exceeds the cache cap of {cap} bytes")]
    TooLarge { size: u64, cap: u64 },
}

/// Test seam: simulate a crash at a point in the write path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailPoint {
    /// Writing the temp file fails (disk full).
    Write,
    /// After the temp file is written and fsynced, before rename.
    BeforeRename,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: u64,
    atime: u64,
    /// `atime` changed in memory but not yet in SQLite.
    dirty: bool,
}

struct Inner {
    conn: Connection,
    entries: HashMap<CacheKey, Entry>,
    /// LRU order: (atime, key), oldest first.
    order: BTreeSet<(u64, CacheKey)>,
    total: u64,
    clock: u64,
    dirty: usize,
    last_flush: Instant,
    flush_interval: Duration,
}

impl Inner {
    fn insert(&mut self, key: CacheKey, size: u64) -> u64 {
        self.clock += 1;
        let atime = self.clock;
        let old = self.entries.insert(
            key,
            Entry {
                size,
                atime,
                dirty: false,
            },
        );
        if let Some(old) = old {
            self.order.remove(&(old.atime, key));
            self.total = self.total.saturating_sub(old.size);
            if old.dirty {
                self.dirty -= 1;
            }
        }
        self.order.insert((atime, key));
        self.total = self.total.saturating_add(size);
        atime
    }

    fn forget(&mut self, key: &CacheKey) -> Option<Entry> {
        let e = self.entries.remove(key)?;
        self.order.remove(&(e.atime, *key));
        self.total = self.total.saturating_sub(e.size);
        if e.dirty {
            self.dirty -= 1;
        }
        Some(e)
    }

    /// Mark `key` most recently used. Skipped when it already is.
    fn touch(&mut self, key: &CacheKey) -> bool {
        let clock = self.clock;
        let Some(e) = self.entries.get_mut(key) else {
            return false;
        };
        if e.atime == clock {
            return true;
        }
        self.clock += 1;
        self.order.remove(&(e.atime, *key));
        e.atime = self.clock;
        if !e.dirty {
            e.dirty = true;
            self.dirty += 1;
        }
        self.order.insert((self.clock, *key));
        true
    }

    fn flush_due(&self) -> bool {
        self.dirty >= FLUSH_BATCH
            || (self.dirty > 0 && self.last_flush.elapsed() >= self.flush_interval)
    }

    fn flush(&mut self) -> Result<(), CacheError> {
        if self.dirty > 0 {
            let tx = self.conn.transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "UPDATE entries SET atime = ?5 WHERE variant = ?1 AND orig = ?2 AND adj = ?3 AND kind = ?4",
                )?;
                for (k, e) in self.entries.iter_mut().filter(|(_, e)| e.dirty) {
                    st.execute(params![
                        k.variant as i64,
                        &k.original.0[..],
                        &k.adj.0[..],
                        k.kind.code(),
                        e.atime as i64
                    ])?;
                    e.dirty = false;
                }
            }
            tx.commit()?;
            self.dirty = 0;
        }
        self.last_flush = Instant::now();
        Ok(())
    }

    fn delete_row(&self, key: &CacheKey) -> Result<(), CacheError> {
        self.conn
            .prepare_cached(
                "DELETE FROM entries WHERE variant = ?1 AND orig = ?2 AND adj = ?3 AND kind = ?4",
            )?
            .execute(params![
                key.variant as i64,
                &key.original.0[..],
                &key.adj.0[..],
                key.kind.code()
            ])?;
        Ok(())
    }
}

/// Size-capped, LRU-evicted JPEG cache. Safe to share across threads.
pub struct DiskCache {
    root: PathBuf,
    cap: AtomicU64,
    inner: Mutex<Inner>,
    tmp_seq: AtomicU64,
    fail: Mutex<Option<FailPoint>>,
}

impl fmt::Debug for DiskCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskCache")
            .field("root", &self.root)
            .field("cap", &self.capacity())
            .finish_non_exhaustive()
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        // Best effort: losing recency on a crash only degrades LRU order.
        let _ = self.lock().flush();
    }
}

fn open_index(path: &Path) -> Result<Connection, CacheError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    match version {
        0 => {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS entries(
                     variant INTEGER NOT NULL,
                     orig BLOB NOT NULL,
                     adj BLOB NOT NULL,
                     kind INTEGER NOT NULL,
                     size INTEGER NOT NULL,
                     atime INTEGER NOT NULL,
                     PRIMARY KEY(variant, orig, adj, kind)) WITHOUT ROWID;",
            )?;
            conn.pragma_update(None, "user_version", INDEX_VERSION)?;
        }
        INDEX_VERSION => {}
        found => return Err(CacheError::Schema { found }),
    }
    // Touch the table so a truncated/garbage file fails here, not later.
    conn.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))?;
    Ok(conn)
}

fn remove_index_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = fs::remove_file(PathBuf::from(p));
    }
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn is_not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

type Row = (CacheKey, u64, u64);

fn load_rows(conn: &Connection) -> Result<Vec<Row>, CacheError> {
    let mut st = conn.prepare("SELECT variant, orig, adj, kind, size, atime FROM entries")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (variant, orig, adj, kind, size, atime) = row?;
        // The index lives in a folder that may be shared or restored: skip
        // anything malformed, including negative sizes and times (they would
        // wrap to ~2^64 as `u64`).
        if size < 0 || atime < 0 {
            continue;
        }
        let (Ok(orig), Ok(adj), Some(kind)) = (
            <[u8; 32]>::try_from(orig.as_slice()),
            <[u8; 32]>::try_from(adj.as_slice()),
            Kind::from_code(kind),
        ) else {
            continue;
        };
        let key = CacheKey {
            variant: variant as u64,
            original: FileHash(orig),
            adj: AdjHash(adj),
            kind,
        };
        out.push((key, size as u64, atime as u64));
    }
    Ok(out)
}

impl DiskCache {
    /// Open (creating if needed) a cache rooted at `root` (normally
    /// `<session>/.ds/cache`) holding at most `cap_bytes` of files.
    pub fn open(root: impl AsRef<Path>, cap_bytes: u64) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        for k in Kind::ALL {
            fs::create_dir_all(root.join(k.dir()))?;
            Self::remove_stray_temps(&root.join(k.dir()))?;
        }
        let db = root.join(INDEX_FILE);
        let existed = db.exists();
        let (conn, rescan) = if let Ok(c) = open_index(&db) {
            (c, !existed)
        } else {
            remove_index_files(&db);
            (open_index(&db)?, true)
        };
        let cache = Self {
            root,
            cap: AtomicU64::new(cap_bytes),
            inner: Mutex::new(Inner {
                conn,
                entries: HashMap::new(),
                order: BTreeSet::new(),
                total: 0,
                clock: 0,
                dirty: 0,
                last_flush: Instant::now(),
                flush_interval: FLUSH_INTERVAL,
            }),
            tmp_seq: AtomicU64::new(0),
            fail: Mutex::new(None),
        };
        {
            let mut g = cache.lock();
            if rescan {
                cache.rescan_locked(&mut g)?;
            } else {
                let rows = load_rows(&g.conn)?;
                Self::fill(&mut g, rows);
            }
            cache.evict_locked(&mut g, None)?;
        }
        Ok(cache)
    }

    fn fill(g: &mut Inner, rows: Vec<Row>) {
        g.entries.clear();
        g.order.clear();
        g.total = 0;
        g.dirty = 0;
        g.clock = 0;
        for (key, size, atime) in rows {
            g.entries.insert(
                key,
                Entry {
                    size,
                    atime,
                    dirty: false,
                },
            );
            g.order.insert((atime, key));
            g.total = g.total.saturating_add(size);
            g.clock = g.clock.max(atime);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remove_stray_temps(dir: &Path) -> io::Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            if e.file_name().to_string_lossy().ends_with(TMP_SUFFIX) {
                let _ = fs::remove_file(e.path());
            }
        }
        Ok(())
    }

    /// Rebuild the index from the files on disk (LRU order ≈ file mtime).
    /// `.jpg` files that are not current-format renditions are deleted.
    pub fn rescan(&self) -> Result<(), CacheError> {
        let mut g = self.lock();
        self.rescan_locked(&mut g)?;
        self.evict_locked(&mut g, None)
    }

    fn rescan_locked(&self, g: &mut Inner) -> Result<(), CacheError> {
        let mut found = Vec::new();
        for kind in Kind::ALL {
            for e in fs::read_dir(self.root.join(kind.dir()))? {
                let e = e?;
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                let md = e.metadata()?;
                match CacheKey::parse(kind, name) {
                    Some(key) if md.is_file() => found.push((md.modified().ok(), key, md.len())),
                    // Older naming or another pipeline version: a stale render.
                    None if md.is_file()
                        && Path::new(name).extension().is_some_and(|e| e == "jpg") =>
                    {
                        let _ = fs::remove_file(e.path());
                    }
                    _ => {}
                }
            }
        }
        found.sort_by_key(|(t, _, _)| *t);
        let rows: Vec<Row> = found
            .iter()
            .enumerate()
            .map(|(i, (_, k, s))| (*k, *s, (i + 1) as u64))
            .collect();
        let tx = g.conn.transaction()?;
        tx.execute("DELETE FROM entries", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO entries(variant, orig, adj, kind, size, atime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (k, size, atime) in &rows {
                ins.execute(params![
                    k.variant as i64,
                    &k.original.0[..],
                    &k.adj.0[..],
                    k.kind.code(),
                    *size as i64,
                    *atime as i64
                ])?;
            }
        }
        tx.commit()?;
        Self::fill(g, rows);
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn capacity(&self) -> u64 {
        self.cap.load(Ordering::Relaxed)
    }

    /// Change the cap; evicts immediately if now over it.
    pub fn set_capacity(&self, cap_bytes: u64) -> Result<(), CacheError> {
        self.cap.store(cap_bytes, Ordering::Relaxed);
        let mut g = self.lock();
        self.evict_locked(&mut g, None)
    }

    /// Bytes currently indexed.
    pub fn total_bytes(&self) -> u64 {
        self.lock().total
    }

    pub fn len(&self) -> Result<usize, CacheError> {
        Ok(self.lock().entries.len())
    }

    pub fn is_empty(&self) -> Result<bool, CacheError> {
        Ok(self.len()? == 0)
    }

    /// Where `key`'s file lives (whether or not it exists).
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        self.root.join(key.kind.dir()).join(key.file_name())
    }

    /// Indexed? Does not affect recency.
    pub fn contains(&self, key: &CacheKey) -> Result<bool, CacheError> {
        Ok(self.lock().entries.contains_key(key))
    }

    /// Write pending access times to the index now.
    pub fn flush(&self) -> Result<(), CacheError> {
        self.lock().flush()
    }

    /// Read `key`'s JPEG bytes and mark it most recently used.
    pub fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, CacheError> {
        {
            let mut g = self.lock();
            if !g.touch(key) {
                return Ok(None);
            }
            if g.flush_due() {
                g.flush()?;
            }
        }
        match fs::read(self.path_for(key)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if is_not_found(&e) => {
                // Deleted behind our back (or evicted concurrently): forget it.
                self.forget_if_missing(key)?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Drop `key` from the index when its file is still absent. Checked under
    /// the lock: a concurrent evict-then-`put` of the same key may have
    /// written a fresh file since the read failed, and that entry must stay
    /// indexed (otherwise it is uncounted, never evicted, and survives a
    /// restart).
    fn forget_if_missing(&self, key: &CacheKey) -> Result<(), CacheError> {
        let mut g = self.lock();
        if !self.path_for(key).exists() && g.forget(key).is_some() {
            g.delete_row(key)?;
        }
        Ok(())
    }

    /// Store `jpeg` under `key` atomically, replacing any previous entry, then
    /// evict least-recently-used entries until under the cap.
    pub fn put(&self, key: &CacheKey, jpeg: &[u8]) -> Result<(), CacheError> {
        let size = jpeg.len() as u64;
        let cap = self.capacity();
        if size > cap {
            return Err(CacheError::TooLarge { size, cap });
        }
        let dst = self.path_for(key);
        let dir = dst.parent().expect("cache file has a parent").to_path_buf();
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        // Short temp name: the final name is already long.
        let tmp = dir.join(format!(
            ".{:016x}.{}.{seq}{TMP_SUFFIX}",
            key.variant,
            std::process::id()
        ));
        let fail = *self.fail.lock().unwrap_or_else(PoisonError::into_inner);
        let written = File::create(&tmp).and_then(|mut f| {
            if fail == Some(FailPoint::Write) {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            f.write_all(jpeg)?;
            f.sync_all()
        });
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        if fail == Some(FailPoint::BeforeRename) {
            return Err(io::Error::other("injected crash before rename").into());
        }
        {
            // Rename and index update under one lock so the indexed size always
            // matches the file that won a concurrent same-key race.
            let mut g = self.lock();
            if let Err(e) = fs::rename(&tmp, &dst) {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
            let atime = g.insert(*key, size);
            g.conn
                .prepare_cached(
                    "INSERT INTO entries(variant, orig, adj, kind, size, atime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(variant, orig, adj, kind) DO UPDATE SET size = excluded.size, atime = excluded.atime",
                )?
                .execute(params![
                    key.variant as i64,
                    &key.original.0[..],
                    &key.adj.0[..],
                    key.kind.code(),
                    size as i64,
                    atime as i64
                ])?;
            self.evict_locked(&mut g, Some(key))?;
            if g.flush_due() {
                g.flush()?;
            }
        }
        fsync_dir(&dir)?;
        Ok(())
    }

    /// Drop one entry. Returns whether it was indexed.
    pub fn remove(&self, key: &CacheKey) -> Result<bool, CacheError> {
        let mut g = self.lock();
        if !g.entries.contains_key(key) {
            return Ok(false);
        }
        self.remove_file(key)?;
        g.forget(key);
        g.delete_row(key)?;
        Ok(true)
    }

    fn remove_file(&self, key: &CacheKey) -> Result<(), CacheError> {
        match fs::remove_file(self.path_for(key)) {
            Err(e) if !is_not_found(&e) => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn remove_where(&self, pred: impl Fn(&CacheKey) -> bool) -> Result<usize, CacheError> {
        let mut g = self.lock();
        let keys: Vec<CacheKey> = g.entries.keys().filter(|k| pred(k)).copied().collect();
        for k in &keys {
            self.remove_file(k)?;
            g.forget(k);
            g.delete_row(k)?;
        }
        Ok(keys.len())
    }

    /// Drop every rendition of `variant` (e.g. the variant was deleted).
    pub fn invalidate_variant(&self, variant: u64) -> Result<usize, CacheError> {
        self.remove_where(|k| k.variant == variant)
    }

    /// Drop renditions of `variant` made for any adjustments other than
    /// `current` or for another original file content.
    pub fn invalidate_stale(
        &self,
        variant: u64,
        original: &FileHash,
        current: &AdjHash,
    ) -> Result<usize, CacheError> {
        self.remove_where(|k| {
            k.variant == variant && (k.adj != *current || k.original != *original)
        })
    }

    fn evict_locked(&self, g: &mut Inner, keep: Option<&CacheKey>) -> Result<(), CacheError> {
        let cap = self.capacity();
        while g.total > cap {
            let victim = g.order.iter().map(|(_, k)| *k).find(|k| Some(k) != keep);
            let Some(k) = victim else { break };
            self.remove_file(&k)?;
            g.forget(&k);
            g.delete_row(&k)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const ORIG: FileHash = FileHash([7; 32]);

    fn key(v: u64, k: Kind) -> CacheKey {
        CacheKey::new(v, ORIG, adjustments_hash(&Adjustments::default()), k)
    }

    fn open(dir: &Path, cap: u64) -> DiskCache {
        DiskCache::open(dir.join(".ds/cache"), cap).unwrap()
    }

    #[test]
    fn adjustments_hash_is_stable_and_sensitive() {
        let a = Adjustments::default();
        let mut b = a.clone();
        assert_eq!(adjustments_hash(&a), adjustments_hash(&b));
        b.exposure.exposure_ev = 0.3;
        assert_ne!(adjustments_hash(&a), adjustments_hash(&b));
        // Round-tripping through JSON keeps the hash (canonical form).
        let rt: Adjustments = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(adjustments_hash(&rt), adjustments_hash(&b));
        let h = adjustments_hash(&a);
        assert_eq!(AdjHash::from_hex(&h.to_hex()), Some(h));
        assert_eq!(AdjHash::from_hex("zz"), None);
        assert!(format!("{h:?}").starts_with("AdjHash("));
    }

    #[test]
    fn render_hash_includes_the_removal_engine_only_with_removals() {
        let plain = Adjustments::default();
        assert_eq!(render_hash(&plain, "lama", None), adjustments_hash(&plain));
        assert_eq!(
            render_hash(&plain, "lama", None),
            render_hash(&plain, "patchmatch-1", None)
        );
        let removed = Adjustments {
            removals: vec![ds_core::adjust::Removal::default()],
            ..Adjustments::default()
        };
        assert_ne!(
            render_hash(&removed, "lama", None),
            adjustments_hash(&removed)
        );
        assert_ne!(
            render_hash(&removed, "lama", None),
            render_hash(&removed, "patchmatch-1", None)
        );
        assert_eq!(
            render_hash(&removed, "lama", None),
            render_hash(&removed, "lama", None)
        );
    }

    #[test]
    fn render_hash_includes_the_depth_map_when_one_is_read() {
        let adj = Adjustments::default();
        let plain = render_hash(&adj, "lama", None);
        let fallback = render_hash(&adj, "lama", Some(&[1, 2, 3]));
        let model = render_hash(&adj, "lama", Some(&[1, 2, 4]));
        assert_ne!(fallback, plain, "with depth is another rendition");
        assert_ne!(fallback, model, "another map is another rendition");
        assert_eq!(
            fallback,
            render_hash(&adj, "patchmatch-1", Some(&[1, 2, 3]))
        );
        let removed = Adjustments {
            removals: vec![ds_core::adjust::Removal::default()],
            ..Adjustments::default()
        };
        assert_ne!(
            render_hash(&removed, "lama", Some(&[1])),
            render_hash(&removed, "patchmatch-1", Some(&[1])),
            "both fold in"
        );
    }

    #[test]
    fn kinds_and_names() {
        assert_eq!(Kind::Thumb.max_edge(), 512);
        assert_eq!(Kind::Preview.max_edge(), 2560);
        assert_eq!(Kind::from_code(7), None);
        let k = key(0xabc, Kind::Preview);
        assert_eq!(CacheKey::parse(Kind::Preview, &k.file_name()), Some(k));
        let (o, a) = (ORIG.to_hex(), k.adj.to_hex());
        for bad in [
            "x.jpg".to_owned(),
            format!("0000000000000abc_zz_{a}_p{PIPELINE_VERSION}.jpg"),
            format!("0000000000000abc_{o}_zz_p{PIPELINE_VERSION}.jpg"),
            format!("abc_{o}_{a}_p{PIPELINE_VERSION}.jpg"),
            format!("0000000000000abc_{o}_{a}_p{}.jpg", PIPELINE_VERSION + 1),
            format!("0000000000000abc_{o}_{a}_x_p{PIPELINE_VERSION}.jpg"),
            format!("0000000000000abc_{a}.jpg"),
            "noUnderscore.jpg".to_owned(),
        ] {
            assert_eq!(CacheKey::parse(Kind::Thumb, &bad), None, "{bad}");
        }
    }

    #[test]
    fn put_get_roundtrip_and_persistence() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(1, Kind::Thumb);
        assert_eq!(c.get(&k).unwrap(), None);
        assert!(c.is_empty().unwrap());
        c.put(&k, b"jpegdata").unwrap();
        assert!(c.contains(&k).unwrap());
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"jpegdata"[..]));
        assert_eq!(c.total_bytes(), 8);
        // Replace updates size, not count.
        c.put(&k, b"jpg").unwrap();
        assert_eq!((c.len().unwrap(), c.total_bytes()), (1, 3));
        assert!(c.path_for(&k).ends_with(format!("thumb/{}", k.file_name())));
        assert!(format!("{c:?}").contains("DiskCache"));
        drop(c);
        let c = open(d.path(), 1 << 20);
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"jpg"[..]));
        assert_eq!(c.total_bytes(), 3);
        assert!(c.remove(&k).unwrap());
        assert!(!c.remove(&k).unwrap());
        assert!(!c.path_for(&k).exists());
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn lru_eviction_respects_recency() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 30);
        let ks: Vec<_> = (0..3).map(|i| key(i, Kind::Thumb)).collect();
        for k in &ks {
            c.put(k, &[0u8; 10]).unwrap();
        }
        c.get(&ks[0]).unwrap(); // 1 is now the LRU
        c.put(&key(3, Kind::Preview), &[0u8; 10]).unwrap();
        assert!(c.contains(&ks[0]).unwrap());
        assert!(!c.contains(&ks[1]).unwrap());
        assert!(!c.path_for(&ks[1]).exists(), "evicted file deleted");
        assert!(c.contains(&ks[2]).unwrap());
        assert_eq!(c.total_bytes(), 30);
        // A big entry evicts several but never itself.
        let big = key(9, Kind::Preview);
        c.put(&big, &[0u8; 25]).unwrap();
        assert!(c.contains(&big).unwrap());
        assert_eq!(c.len().unwrap(), 1);
        assert_eq!(c.total_bytes(), 25);
        // Larger than the whole cap is refused.
        assert!(matches!(
            c.put(&key(10, Kind::Thumb), &[0u8; 31]),
            Err(CacheError::TooLarge { size: 31, cap: 30 })
        ));
        // Shrinking the cap evicts.
        c.set_capacity(10).unwrap();
        assert_eq!(c.capacity(), 10);
        assert!(c.is_empty().unwrap());
        // LRU order survives reopen.
        c.set_capacity(100).unwrap();
        for k in &ks {
            c.put(k, &[0u8; 10]).unwrap();
        }
        c.get(&ks[0]).unwrap();
        drop(c);
        let c = open(d.path(), 20);
        assert!(c.contains(&ks[0]).unwrap());
        assert!(!c.contains(&ks[1]).unwrap());
        assert!(c.contains(&ks[2]).unwrap());
    }

    #[test]
    fn invalidate_variant_and_stale() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let mut edited = Adjustments::default();
        edited.hdr.shadows = 40.0;
        let old = adjustments_hash(&Adjustments::default());
        let new = adjustments_hash(&edited);
        for kind in Kind::ALL {
            c.put(&CacheKey::new(1, ORIG, old, kind), b"old").unwrap();
            c.put(&CacheKey::new(1, ORIG, new, kind), b"new").unwrap();
            c.put(&CacheKey::new(2, ORIG, old, kind), b"two").unwrap();
        }
        // A replaced original (new content hash) is stale too.
        let other = FileHash([8; 32]);
        c.put(&CacheKey::new(1, other, new, Kind::Thumb), b"swapped")
            .unwrap();
        assert_eq!(c.invalidate_stale(1, &ORIG, &new).unwrap(), 3);
        assert!(
            c.contains(&CacheKey::new(1, ORIG, new, Kind::Thumb))
                .unwrap()
        );
        assert!(
            !c.contains(&CacheKey::new(1, ORIG, old, Kind::Thumb))
                .unwrap()
        );
        assert_eq!(c.invalidate_variant(1).unwrap(), 2);
        assert_eq!(c.invalidate_variant(1).unwrap(), 0);
        assert_eq!(c.len().unwrap(), 2);
        assert_eq!(c.total_bytes(), 6);
    }

    #[test]
    fn crash_before_rename_leaves_old_entry_and_temp_is_cleaned() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(4, Kind::Thumb);
        c.put(&k, b"good").unwrap();
        *c.fail.lock().unwrap() = Some(FailPoint::BeforeRename);
        assert!(matches!(c.put(&k, b"half-written"), Err(CacheError::Io(_))));
        assert_eq!(
            c.get(&k).unwrap().as_deref(),
            Some(&b"good"[..]),
            "old entry intact"
        );
        let thumb_dir = c.root().join("thumb");
        let temps = || {
            fs::read_dir(&thumb_dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
                .count()
        };
        assert_eq!(temps(), 1, "crash leaves the temp behind");
        drop(c);
        let c = open(d.path(), 1 << 20);
        assert_eq!(temps(), 0, "reopen removes stray temps");
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"good"[..]));
    }

    #[test]
    fn write_failure_is_reported_and_temp_removed() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(4, Kind::Thumb);
        c.put(&k, b"good").unwrap();
        *c.fail.lock().unwrap() = Some(FailPoint::Write);
        assert!(matches!(c.put(&k, b"no room"), Err(CacheError::Io(_))));
        let names: Vec<String> = fs::read_dir(c.root().join("thumb"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(TMP_SUFFIX)), "{names:?}");
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"good"[..]));
    }

    #[test]
    fn rename_failure_is_reported_and_temp_removed() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(5, Kind::Thumb);
        // A directory at the destination makes rename fail.
        fs::create_dir(c.path_for(&k)).unwrap();
        fs::write(c.path_for(&k).join("x"), b"x").unwrap();
        assert!(matches!(c.put(&k, b"data"), Err(CacheError::Io(_))));
        assert!(!c.contains(&k).unwrap());
        let n = fs::read_dir(c.root().join("thumb")).unwrap().count();
        assert_eq!(n, 1, "only the blocking dir remains");
    }

    #[test]
    fn corrupt_index_is_rebuilt_from_files() {
        let d = tempfile::tempdir().unwrap();
        let ks: Vec<_> = (0..5).map(|i| key(i, Kind::Preview)).collect();
        {
            let c = open(d.path(), 1 << 20);
            for k in &ks {
                c.put(k, b"abcd").unwrap();
            }
        }
        let root = d.path().join(".ds/cache");
        remove_index_files(&root.join(INDEX_FILE));
        fs::write(
            root.join(INDEX_FILE),
            b"this is not a sqlite database at all, just garbage bytes....",
        )
        .unwrap();
        // Junk files in the cache dirs are ignored.
        fs::write(root.join("preview/readme.txt"), b"x").unwrap();
        let dir_named_like_entry = root.join("thumb").join(key(1, Kind::Thumb).file_name());
        fs::create_dir(&dir_named_like_entry).unwrap();
        let c = open(d.path(), 1 << 20);
        assert_eq!(c.len().unwrap(), 5);
        assert_eq!(c.total_bytes(), 20);
        for k in &ks {
            assert_eq!(c.get(k).unwrap().as_deref(), Some(&b"abcd"[..]));
        }
    }

    #[test]
    fn missing_index_and_old_schema_trigger_rescan() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1, Kind::Thumb);
        {
            let c = open(d.path(), 1 << 20);
            c.put(&k, b"abc").unwrap();
        }
        let db = d.path().join(".ds/cache").join(INDEX_FILE);
        remove_index_files(&db);
        {
            let c = open(d.path(), 1 << 20);
            assert!(c.contains(&k).unwrap());
            c.lock()
                .conn
                .pragma_update(None, "user_version", 99)
                .unwrap();
        }
        assert!(matches!(
            open_index(&db),
            Err(CacheError::Schema { found: 99 })
        ));
        assert!(
            CacheError::Schema { found: 99 }
                .to_string()
                .contains("0x63")
        );
        let c = open(d.path(), 1 << 20);
        assert!(c.contains(&k).unwrap());
        // Explicit rescan after files vanish behind our back.
        fs::remove_file(c.path_for(&k)).unwrap();
        c.rescan().unwrap();
        assert!(c.is_empty().unwrap());
    }

    #[test]
    fn index_rows_with_negative_size_or_time_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let (good, bad) = (key(1, Kind::Thumb), key(2, Kind::Thumb));
        {
            let c = open(d.path(), 1 << 20);
            c.put(&good, b"abc").unwrap();
            c.put(&bad, b"abcd").unwrap();
            c.flush().unwrap();
        }
        let conn = Connection::open(d.path().join(".ds/cache").join(INDEX_FILE)).unwrap();
        conn.execute("UPDATE entries SET size = -1 WHERE variant = 2", [])
            .unwrap();
        conn.execute("UPDATE entries SET atime = -5 WHERE variant = 1", [])
            .unwrap();
        drop(conn);
        let c = open(d.path(), 1 << 20);
        assert_eq!(c.total_bytes(), 0, "both rows dropped, no wrapped total");
        assert!(!c.contains(&bad).unwrap());
        // The cache still works and evicts normally.
        c.put(&good, b"abc").unwrap();
        assert_eq!(c.total_bytes(), 3);
    }

    #[test]
    fn file_deleted_behind_our_back_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(1, Kind::Thumb);
        c.put(&k, b"abc").unwrap();
        fs::remove_file(c.path_for(&k)).unwrap();
        assert_eq!(c.get(&k).unwrap(), None);
        assert!(!c.contains(&k).unwrap());
        assert_eq!(c.total_bytes(), 0);
        // remove() tolerates a missing file too.
        c.put(&k, b"abc").unwrap();
        fs::remove_file(c.path_for(&k)).unwrap();
        assert!(c.remove(&k).unwrap());
    }

    #[test]
    fn fresh_entry_survives_a_stale_not_found() {
        // get() read the old file's absence, then another thread evicted and
        // re-put the key before get() re-took the lock.
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(1, Kind::Thumb);
        c.put(&k, b"old").unwrap();
        fs::remove_file(c.path_for(&k)).unwrap();
        c.put(&k, b"fresh!").unwrap();
        c.forget_if_missing(&k).unwrap();
        assert!(c.contains(&k).unwrap());
        assert_eq!(c.total_bytes(), 6);
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"fresh!"[..]));
        // Still forgotten when the file really is gone.
        fs::remove_file(c.path_for(&k)).unwrap();
        c.forget_if_missing(&k).unwrap();
        assert!(!c.contains(&k).unwrap());
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn unreadable_entry_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let k = key(1, Kind::Thumb);
        c.put(&k, b"abc").unwrap();
        fs::remove_file(c.path_for(&k)).unwrap();
        fs::create_dir(c.path_for(&k)).unwrap();
        assert!(matches!(c.get(&k), Err(CacheError::Io(_))));
        // remove_file on a directory fails with a non-NotFound error.
        assert!(matches!(c.remove(&k), Err(CacheError::Io(_))));
        assert!(DiskCache::open(c.path_for(&k).join("\0bad"), 1).is_err());
    }

    #[test]
    fn unicode_session_path() {
        let d = tempfile::tempdir().unwrap();
        let session = d.path().join("2026-09-25 Hawaiʻi — 夏 🌺");
        let c = DiskCache::open(session.join(".ds/cache"), 1 << 20).unwrap();
        let k = key(u64::MAX, Kind::Preview);
        c.put(&k, "ü".as_bytes()).unwrap();
        drop(c);
        let c = DiskCache::open(session.join(".ds/cache"), 1 << 20).unwrap();
        assert_eq!(c.get(&k).unwrap().as_deref(), Some("ü".as_bytes()));
    }

    #[test]
    fn concurrent_writers_same_key_stay_consistent() {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(open(d.path(), 1 << 20));
        let k = key(42, Kind::Thumb);
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (c, b) = (Arc::clone(&c), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let body = vec![i as u8; 100 + i * 10];
                    b.wait();
                    for _ in 0..10 {
                        c.put(&k, &body).unwrap();
                        let got = c.get(&k).unwrap().unwrap();
                        // Whatever we read is one writer's complete body.
                        assert!(got.iter().all(|&x| x == got[0]));
                        assert_eq!(got.len(), 100 + got[0] as usize * 10);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let got = c.get(&k).unwrap().unwrap();
        assert_eq!(c.len().unwrap(), 1);
        assert_eq!(
            c.total_bytes(),
            got.len() as u64,
            "index size matches the winning file"
        );
        let temps = fs::read_dir(c.root().join("thumb")).unwrap().count();
        assert_eq!(temps, 1, "no leftover temps");
    }

    #[test]
    fn concurrent_writers_many_keys_respect_cap() {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(open(d.path(), 5_000));
        let handles: Vec<_> = (0..4u64)
            .map(|t| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for i in 0..50 {
                        c.put(&key(t * 1000 + i, Kind::Thumb), &[1u8; 100]).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(c.total_bytes() <= 5_000);
        assert_eq!(c.len().unwrap() as u64 * 100, c.total_bytes());
        let on_disk: u64 = fs::read_dir(c.root().join("thumb"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(on_disk, c.total_bytes(), "no orphaned files");
    }

    #[test]
    fn malformed_rows_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        c.lock()
            .conn
            .execute(
                "INSERT INTO entries VALUES (7, x'00', zeroblob(32), 0, 1, 1),
                     (7, zeroblob(32), x'00', 0, 1, 1), (7, zeroblob(32), zeroblob(32), 9, 1, 1)",
                [],
            )
            .unwrap();
        drop(c);
        let c = open(d.path(), 1 << 20);
        assert_eq!(c.invalidate_variant(7).unwrap(), 0);
    }

    #[test]
    fn same_variant_and_adjustments_but_new_original_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let a = key(1, Kind::Thumb);
        c.put(&a, b"render of A").unwrap();
        let b = CacheKey {
            original: FileHash([9; 32]),
            ..a
        };
        assert_eq!(
            c.get(&b).unwrap(),
            None,
            "B's pixels must not get A's render"
        );
        assert_ne!(c.path_for(&a), c.path_for(&b));
    }

    #[test]
    fn old_format_and_other_pipeline_files_are_purged_on_rebuild() {
        let d = tempfile::tempdir().unwrap();
        let k = key(3, Kind::Preview);
        {
            let c = open(d.path(), 1 << 20);
            c.put(&k, b"current").unwrap();
        }
        let root = d.path().join(".ds/cache");
        let v1_name = format!("{:016x}_{}.jpg", 3, k.adj.to_hex());
        let other_pipeline = k
            .file_name()
            .replace(&format!("_p{PIPELINE_VERSION}.jpg"), "_p0.jpg");
        fs::write(root.join("preview").join(&v1_name), b"old naming").unwrap();
        fs::write(root.join("preview").join(&other_pipeline), b"old pipeline").unwrap();
        // An index stamped for another pipeline version is discarded.
        let conn = Connection::open(root.join(INDEX_FILE)).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION << 16)
            .unwrap();
        drop(conn);
        let c = open(d.path(), 1 << 20);
        assert!(!root.join("preview").join(&v1_name).exists());
        assert!(!root.join("preview").join(&other_pipeline).exists());
        assert_eq!(c.len().unwrap(), 1);
        assert_eq!(c.get(&k).unwrap().as_deref(), Some(&b"current"[..]));
    }

    fn persisted_atime(c: &DiskCache, k: &CacheKey) -> i64 {
        c.lock()
            .conn
            .query_row(
                "SELECT atime FROM entries WHERE variant = ?1 AND kind = ?2",
                params![k.variant as i64, k.kind.code()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn get_does_not_write_the_index_until_a_flush() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 1 << 20);
        let (a, b) = (key(1, Kind::Thumb), key(2, Kind::Thumb));
        c.put(&a, b"a").unwrap();
        c.put(&b, b"b").unwrap();
        let before = persisted_atime(&c, &a);
        c.get(&a).unwrap();
        assert_eq!(persisted_atime(&c, &a), before, "no per-get write");
        assert_eq!(c.lock().dirty, 1);
        // Already most recent: repeated gets do not advance the clock.
        let clock = c.lock().clock;
        c.get(&a).unwrap();
        assert_eq!(c.lock().clock, clock);
        c.flush().unwrap();
        assert!(persisted_atime(&c, &a) > persisted_atime(&c, &b));
        assert_eq!(c.lock().dirty, 0);
        c.flush().unwrap(); // nothing dirty: no-op
    }

    #[test]
    fn flush_happens_by_interval_batch_and_drop() {
        let d = tempfile::tempdir().unwrap();
        let ks: Vec<_> = (0..(FLUSH_BATCH as u64 + 2))
            .map(|i| key(i, Kind::Thumb))
            .collect();
        {
            let c = open(d.path(), 1 << 30);
            for k in &ks {
                c.put(k, b"x").unwrap();
            }
            // Interval: with a zero interval the next get flushes.
            c.lock().flush_interval = Duration::ZERO;
            c.get(&ks[0]).unwrap();
            assert_eq!(c.lock().dirty, 0);
            c.lock().flush_interval = Duration::from_secs(3600);
            // Batch: FLUSH_BATCH dirty entries trigger a flush.
            for k in &ks[1..=FLUSH_BATCH] {
                c.get(k).unwrap();
            }
            assert_eq!(c.lock().dirty, 0, "batch flush");
            c.get(&ks[0]).unwrap();
            assert_eq!(c.lock().dirty, 1);
            // A put overwriting a dirty entry keeps the count right.
            c.put(&ks[0], b"y").unwrap();
            assert_eq!(c.lock().dirty, 0);
            c.get(&ks[5]).unwrap();
            c.remove(&ks[5]).unwrap();
            assert_eq!(c.lock().dirty, 0);
            c.get(&ks[6]).unwrap();
        } // drop flushes ks[6]
        let c = open(d.path(), 1 << 30);
        let newest = c.lock().order.iter().next_back().map(|(_, k)| *k);
        assert_eq!(newest, Some(ks[6]), "recency written on drop");
    }

    #[test]
    fn eviction_uses_unflushed_recency() {
        let d = tempfile::tempdir().unwrap();
        let c = open(d.path(), 20);
        let (a, b) = (key(1, Kind::Thumb), key(2, Kind::Thumb));
        c.put(&a, &[0; 10]).unwrap();
        c.put(&b, &[0; 10]).unwrap();
        c.get(&a).unwrap(); // only in memory
        c.put(&key(3, Kind::Thumb), &[0; 10]).unwrap();
        assert!(c.contains(&a).unwrap());
        assert!(!c.contains(&b).unwrap());
    }
}
