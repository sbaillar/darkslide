//! The Library (global index v6): every registered session's photos in one
//! place, to browse, filter, sort and edit without opening each session.
//!
//! - **Rows**: one `library_items` row per variant outside the Trash of every
//!   session (capture time, rating, label, flag, camera, lens, keywords,
//!   title, whether it has a position, and its thumbnail-cache key: the
//!   variant id and the hash of its edit). Sidecars stay the source of truth:
//!   the rows are rebuilt from each session ([`Session::library_records`],
//!   [`GlobalIndex::sync_library`]) and a sync writes only the rows that
//!   changed. A sidecar is re-read for its edit hash only when its stamp
//!   (mtime, size) moved.
//! - **Queries** ([`GlobalIndex::library_query`]) run on an in-memory copy of
//!   the rows (names interned, one capture-time order computed per load),
//!   reloaded when `library_generation` moves; the Library's own edits patch it
//!   in place. A query returns the ordered ids; the app pages their details in
//!   with [`GlobalIndex::library_items`].
//! - **Collections** span sessions: plain ones list their photos by session,
//!   original and variant ordinal (so a resync never loses them; the global
//!   index is their only copy), smart ones store a [`LibraryQuery`].
//! - **Edits** (rating, label, flag) go through the owning session, which
//!   writes its sidecars atomically ([`apply_library_edit`]); the rows are
//!   then patched ([`GlobalIndex::library_patch`]).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ds_core::date::civil_from_days;
use ds_core::{Adjustments, ColorTag, IptcMeta, PickState, Rating};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SessionError, json_err};
use crate::filter::{pick_code, pick_from_code, tag_code, tag_from_code};
use crate::find::fold;
use crate::global::GlobalIndex;
use crate::relpath::{Folder, original_path};
use crate::session::{DB_EXT, Session, VariantId};

/// v6 of the global index (applied on top of v5). Idempotent: two openers
/// racing on a v5 file both run it. Rows are read whole into memory, so the
/// only indexes are the ones sync, collections and paging look rows up by
/// (`UNIQUE(session_id, rel, ordinal)`, the id).
pub(crate) const LIBRARY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS library_sources(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    store TEXT NOT NULL,
    external TEXT,
    stamp INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS library_items(
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    variant INTEGER NOT NULL,
    capture_ms INTEGER,
    rating INTEGER NOT NULL,
    label INTEGER NOT NULL,
    pick INTEGER NOT NULL,
    camera TEXT NOT NULL,
    lens TEXT NOT NULL,
    keywords TEXT NOT NULL,
    title TEXT NOT NULL,
    gps INTEGER NOT NULL,
    stamp INTEGER NOT NULL,
    edit BLOB,
    UNIQUE(session_id, rel, ordinal)
);
CREATE TABLE IF NOT EXISTS collections(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT
);
CREATE TABLE IF NOT EXISTS collection_items(
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    PRIMARY KEY(collection_id, session_id, rel, ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS library_generation(id INTEGER PRIMARY KEY CHECK (id = 0), n INTEGER NOT NULL);
INSERT OR IGNORE INTO library_generation VALUES (0, 0);
";

/// Global index v5 -> v6: the Library tables (filled by each session's next sync).
pub(crate) fn to_v6(conn: &Connection, version: i64) -> Result<()> {
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE; {LIBRARY_SCHEMA} PRAGMA user_version={version}; COMMIT;"
    ))?;
    Ok(())
}

/// Hash of an edit as the thumbnail cache keys it (`ds_cache::adjustments_hash`:
/// BLAKE3 of the `serde_json` form).
pub fn edit_hash(adj: &Adjustments) -> [u8; 32] {
    let json = serde_json::to_vec(adj).expect("Adjustments always serialize");
    *blake3::hash(&json).as_bytes()
}

/// One variant as the Library stores it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryRecord {
    /// The original, relative to the session (`Capture/DSC0001.ARW`).
    pub rel: String,
    pub ordinal: u32,
    /// The session's variant id (stable; part of the thumbnail-cache key).
    pub variant: i64,
    pub capture_ms: Option<i64>,
    pub rating: u8,
    pub label: ColorTag,
    pub pick: PickState,
    pub camera: String,
    pub lens: String,
    pub keywords: Vec<String>,
    /// Its IPTC title, else its caption.
    pub title: String,
    /// It has a position (a geotag, or GPS in the file).
    pub gps: bool,
    /// The sidecar's (mtime, size), folded: the edit is re-hashed when it moves.
    pub stamp: i64,
    /// [`edit_hash`] of an edited variant (`None`: unedited, or unreadable).
    pub edit: Option<[u8; 32]>,
}

/// What a sync already knows of a session's rows: `(rel, ordinal)` → `(stamp, edit)`.
pub type KnownEdits = HashMap<(String, u32), (i64, Option<[u8; 32]>)>;

/// Where a session's sidecars are and where its originals live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySource {
    /// The folder [`Session::open`] takes.
    pub store: PathBuf,
    /// The originals' folder of an external (in-place) session.
    pub external: Option<PathBuf>,
    /// [`index_stamp`] of the store when it was synced.
    pub stamp: i64,
}

/// A registered session and, once synced, where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySession {
    pub id: i64,
    /// The folder the app opens.
    pub root: PathBuf,
    pub name: String,
    pub source: Option<LibrarySource>,
}

/// The stamp of a session's index files (`<name>.dsession` and its WAL):
/// it moves on every write the session makes. `None` when there is none.
pub fn index_stamp(store: &Path) -> Option<i64> {
    let mut stamp: Option<i64> = None;
    for e in fs::read_dir(store).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Some((_, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if ext != DB_EXT && ext != format!("{DB_EXT}-wal") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        let len = i64::try_from(md.len()).unwrap_or(i64::MAX);
        let s = stamp.unwrap_or(0);
        stamp = Some(s.rotate_left(7) ^ mtime.wrapping_mul(31).wrapping_add(len));
    }
    stamp
}

fn fold_stamp(mtime: i64, size: i64) -> i64 {
    mtime.wrapping_mul(1_000_003).wrapping_add(size)
}

impl Session {
    /// Every variant outside the Trash whose original is present, for the
    /// Library, in path then ordinal order. An edited variant's sidecar is
    /// read for its edit hash unless `known` has it at the same stamp.
    pub fn library_records(&self, known: &KnownEdits) -> Result<Vec<LibraryRecord>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.rel_path, v.ordinal, v.id, i.capture_ms, v.rating, v.color_tag, v.pick,
                    COALESCE(i.camera, ''), COALESCE(i.lens, ''), v.keywords, v.iptc,
                    v.geo_lat IS NOT NULL OR l.lat IS NOT NULL, v.sidecar_mtime, v.sidecar_size, v.edited
             FROM variants v JOIN images i ON i.id = v.image_id
             LEFT JOIN locations l ON l.key = {}
             WHERE i.missing = 0 AND i.folder <> ?1 ORDER BY i.rel_path, v.ordinal",
            crate::embed::KEY_SQL
        ))?;
        let rows = st.query_map([Folder::Trash.code()], |r| {
            let iptc: String = r.get(10)?;
            let title = if iptc.is_empty() {
                String::new()
            } else {
                let m: IptcMeta = serde_json::from_str(&iptc).unwrap_or_default();
                if m.title.is_empty() {
                    m.caption
                } else {
                    m.title
                }
            };
            let keywords: String = r.get(9)?;
            Ok((
                LibraryRecord {
                    rel: r.get(0)?,
                    ordinal: r.get(1)?,
                    variant: r.get(2)?,
                    capture_ms: r.get(3)?,
                    rating: r.get(4)?,
                    label: tag_from_code(r.get(5)?),
                    pick: pick_from_code(r.get(6)?),
                    camera: r.get(7)?,
                    lens: r.get(8)?,
                    keywords: serde_json::from_str(&keywords).unwrap_or_default(),
                    title,
                    gps: r.get(11)?,
                    stamp: fold_stamp(r.get(12)?, r.get(13)?),
                    edit: None,
                },
                r.get::<_, bool>(14)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut rec, edited) = row?;
            if edited {
                rec.edit = match known.get(&(rec.rel.clone(), rec.ordinal)) {
                    Some((stamp, edit)) if *stamp == rec.stamp => *edit,
                    // An unreadable sidecar has no edit to show: the embedded preview it is.
                    _ => self
                        .load_adjustments(VariantId(rec.variant))
                        .ok()
                        .map(|a| edit_hash(&a)),
                };
            }
            out.push(rec);
        }
        Ok(out)
    }

    /// The variants at `targets` (`(rel, ordinal)`), each `None` when it isn't
    /// in this session any more.
    pub fn variants_at(&self, targets: &[(String, u32)]) -> Result<Vec<Option<VariantId>>> {
        let mut st = self.conn.prepare_cached(
            "SELECT v.id FROM variants v JOIN images i ON i.id = v.image_id
             WHERE i.rel_path = ? AND v.ordinal = ?",
        )?;
        targets
            .iter()
            .map(|(rel, ord)| {
                Ok(st
                    .query_row(params![rel, ord], |r| r.get(0).map(VariantId))
                    .optional()?)
            })
            .collect()
    }
}

/// A rating, label or flag set from the Library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryEdit {
    Rating(Rating),
    Label(ColorTag),
    Pick(PickState),
}

/// Write `edit` to the variants at `targets` of `session` (atomic sidecar
/// writes, one undo step in that session). Returns the indexes of `targets`
/// written; one no longer in the session is left out.
pub fn apply_library_edit(
    session: &mut Session,
    targets: &[(String, u32)],
    edit: LibraryEdit,
) -> Result<Vec<usize>> {
    let found = session.variants_at(targets)?;
    let ids: Vec<VariantId> = found.iter().flatten().copied().collect();
    match edit {
        LibraryEdit::Rating(r) => session.set_rating(&ids, r)?,
        LibraryEdit::Label(t) => session.set_color_tag(&ids, t)?,
        LibraryEdit::Pick(p) => session.set_pick(&ids, p)?,
    }
    Ok(found
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|_| i))
        .collect())
}

/// Which flags a query keeps (the browser's choices).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickFilter {
    #[default]
    All,
    Picked,
    Unflagged,
    Unrejected,
    Rejected,
}

impl PickFilter {
    fn keeps(self, p: PickState) -> bool {
        match self {
            Self::All => true,
            Self::Picked => p == PickState::Picked,
            Self::Unflagged => p == PickState::Unflagged,
            Self::Unrejected => p != PickState::Rejected,
            Self::Rejected => p == PickState::Rejected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    /// Capture time (photos without one last), then file.
    #[default]
    Captured,
    /// Most stars first, then capture time.
    Rating,
    /// File name.
    Name,
    /// Session name, then capture time.
    Session,
}

/// What the Library shows: every criterion must hold (empty lists: any). A
/// smart collection stores one (its `collection` and sort are ignored).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryQuery {
    /// Session ids (any of).
    pub sessions: Vec<i64>,
    /// Capture time in `[from_ms, to_ms)` (photos without one never match).
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub min_rating: u8,
    /// Only photos without stars.
    pub unrated: bool,
    /// Any of these labels.
    pub labels: Vec<ColorTag>,
    pub pick: PickFilter,
    /// Any of these cameras (exact names).
    pub cameras: Vec<String>,
    /// Every one of these keywords (any case).
    pub keywords: Vec<String>,
    /// Every word in its file name, title, keywords, camera, lens or session
    /// name (accents and case folded).
    pub text: String,
    /// Only this collection's photos.
    pub collection: Option<i64>,
    pub sort: LibrarySort,
    pub descending: bool,
}

/// One photo as the Library grid shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryItem {
    pub id: i64,
    pub session_id: i64,
    pub session_root: PathBuf,
    pub session_name: String,
    /// The original.
    pub path: PathBuf,
    pub rel: String,
    pub ordinal: u32,
    pub variant: i64,
    pub capture_ms: Option<i64>,
    pub rating: u8,
    pub label: ColorTag,
    pub pick: PickState,
    pub camera: String,
    pub lens: String,
    pub keywords: Vec<String>,
    pub title: String,
    pub gps: bool,
    pub edit: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySessionCount {
    pub id: i64,
    pub root: PathBuf,
    pub name: String,
    pub count: u64,
}

/// Photos taken on one day (capture time's calendar day).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryDay {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCollection {
    pub id: i64,
    pub name: String,
    /// A smart collection's rules.
    pub rules: Option<LibraryQuery>,
    pub count: u64,
}

/// What the sidebar and the filter menus list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryOverview {
    pub total: u64,
    /// Every registered session, by name (with no photos synced yet: 0).
    pub sessions: Vec<LibrarySessionCount>,
    /// Days with photos, oldest first.
    pub days: Vec<LibraryDay>,
    /// Photos without a capture time.
    pub undated: u64,
    /// `(name, photos)`, by name.
    pub cameras: Vec<(String, u64)>,
    pub keywords: Vec<(String, u64)>,
    pub collections: Vec<LibraryCollection>,
}

// ------------------------------------------------------------------ cache ---

const NONE: u32 = u32::MAX;

/// One row in memory (names are interned ids, strings live in `text`).
#[derive(Debug, Clone)]
struct Row {
    id: i64,
    session: i64,
    capture: Option<i64>,
    rating: u8,
    label: ColorTag,
    pick: PickState,
    gps: bool,
    camera: u32,
    lens: u32,
    /// `keywords[kw.0 .. kw.0 + kw.1]`.
    kw: (u32, u32),
    rel: (u32, u32),
    title: (u32, u32),
    ordinal: u32,
    variant: i64,
    edit: Option<[u8; 32]>,
}

#[derive(Debug, Default)]
struct Names {
    list: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Names {
    fn id(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return NONE;
        }
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = u32::try_from(self.list.len()).unwrap_or(NONE);
        self.list.push(s.to_owned());
        self.ids.insert(s.to_owned(), id);
        id
    }

    fn get(&self, id: u32) -> &str {
        self.list.get(id as usize).map_or("", String::as_str)
    }
}

#[derive(Debug, Clone)]
struct CachedSession {
    root: PathBuf,
    name: String,
    store: Option<PathBuf>,
    external: Option<PathBuf>,
}

/// The rows in memory, valid for one `library_generation`.
#[derive(Debug, Default)]
pub(crate) struct LibraryCache {
    loaded: bool,
    pub(crate) generation: i64,
    rows: Vec<Row>,
    /// Id → index in `rows`.
    index: HashMap<i64, u32>,
    /// Row indexes by capture time (undated last), then rel, then ordinal.
    by_capture: Vec<u32>,
    /// Row indexes by file name, and by session name (each then by capture
    /// time), built on the first query sorted that way.
    by_name: Option<Vec<u32>>,
    by_session: Option<Vec<u32>>,
    names: Names,
    keywords: Vec<u32>,
    text: String,
    sessions: HashMap<i64, CachedSession>,
    /// Folded searchable text per row, built on the first text query.
    folded: Option<Vec<String>>,
}

impl LibraryCache {
    fn str(&self, r: (u32, u32)) -> &str {
        &self.text[r.0 as usize..(r.0 + r.1) as usize]
    }

    fn kws(&self, row: &Row) -> &[u32] {
        &self.keywords[row.kw.0 as usize..(row.kw.0 + row.kw.1) as usize]
    }

    fn file_name<'a>(&'a self, row: &Row) -> &'a str {
        let rel = self.str(row.rel);
        rel.rsplit('/').next().unwrap_or(rel)
    }

    fn push_text(&mut self, s: &str) -> (u32, u32) {
        let start = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        self.text.push_str(s);
        (start, u32::try_from(s.len()).unwrap_or(0))
    }

    /// Build the order `sort` filters through, once per load.
    fn order(&mut self, sort: LibrarySort) {
        let missing = match sort {
            LibrarySort::Name => self.by_name.is_none(),
            LibrarySort::Session => self.by_session.is_none(),
            LibrarySort::Captured | LibrarySort::Rating => false,
        };
        if !missing {
            return;
        }
        // Capture rank of each row: the tie-break after the name.
        let mut capture_rank = vec![0u32; self.rows.len()];
        for (pos, &i) in self.by_capture.iter().enumerate() {
            capture_rank[i as usize] = pos as u32;
        }
        let mut order: Vec<u32> = (0..self.rows.len() as u32).collect();
        if sort == LibrarySort::Name {
            // One key per row (a memcmp sort): ~10× faster than comparing naturally.
            order.sort_by_cached_key(|&i| {
                (
                    natural_key(self.file_name(&self.rows[i as usize])),
                    capture_rank[i as usize],
                )
            });
            self.by_name = Some(order);
        } else {
            let name = |i: u32| {
                self.sessions
                    .get(&self.rows[i as usize].session)
                    .map_or("", |s| s.name.as_str())
            };
            order.sort_by(|&a, &b| {
                natural(name(a), name(b))
                    .then(
                        self.rows[a as usize]
                            .session
                            .cmp(&self.rows[b as usize].session),
                    )
                    .then(capture_rank[a as usize].cmp(&capture_rank[b as usize]))
            });
            self.by_session = Some(order);
        }
    }

    fn folded(&mut self) -> &[String] {
        if self.folded.is_none() {
            let f = self
                .rows
                .iter()
                .map(|r| {
                    let mut s = String::new();
                    s.push_str(self.file_name(r));
                    for part in [
                        self.str(r.title),
                        self.names.get(r.camera),
                        self.names.get(r.lens),
                    ] {
                        s.push(' ');
                        s.push_str(part);
                    }
                    for &k in self.kws(r) {
                        s.push(' ');
                        s.push_str(self.names.get(k));
                    }
                    format!(" {} ", fold(&s))
                })
                .collect();
            self.folded = Some(f);
        }
        self.folded.as_deref().unwrap_or_default()
    }

    fn item(&self, row: &Row) -> LibraryItem {
        let s = self.sessions.get(&row.session);
        let rel = self.str(row.rel).to_owned();
        let path = match s {
            Some(CachedSession {
                store: Some(store),
                external,
                ..
            }) => original_path(store, external.as_deref(), &rel),
            Some(s) => s.root.join(&rel),
            None => PathBuf::from(&rel),
        };
        LibraryItem {
            id: row.id,
            session_id: row.session,
            session_root: s.map(|s| s.root.clone()).unwrap_or_default(),
            session_name: s.map(|s| s.name.clone()).unwrap_or_default(),
            path,
            rel,
            ordinal: row.ordinal,
            variant: row.variant,
            capture_ms: row.capture,
            rating: row.rating,
            label: row.label,
            pick: row.pick,
            camera: self.names.get(row.camera).to_owned(),
            lens: self.names.get(row.lens).to_owned(),
            keywords: self
                .kws(row)
                .iter()
                .map(|&k| self.names.get(k).to_owned())
                .collect(),
            title: self.str(row.title).to_owned(),
            gps: row.gps,
            edit: row.edit,
        }
    }
}

fn split(s: &str) -> impl Iterator<Item = &str> {
    s.split('\n').filter(|s| !s.is_empty())
}

fn join(list: &[String]) -> String {
    list.iter()
        .map(|s| s.replace('\n', " "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Column `i` as text, borrowed from the row (no allocation).
fn text_at<'a>(r: &'a rusqlite::Row<'_>, i: usize) -> rusqlite::Result<&'a str> {
    r.get_ref(i)?.as_str().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn generation(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT n FROM library_generation", [], |r| r.get(0))?)
}

/// Bump the Library's generation; returns the new one.
fn bump(tx: &rusqlite::Transaction<'_>) -> Result<i64> {
    Ok(tx.query_row(
        "UPDATE library_generation SET n = n + 1 RETURNING n",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn bump_library(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    bump(tx).map(|_| ())
}

/// The words of a text query (folded).
fn words(text: &str) -> Vec<String> {
    fold(text)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

impl GlobalIndex {
    /// What the Library has of the session at `root`, for
    /// [`Session::library_records`].
    pub fn library_known(&self, root: &Path) -> Result<KnownEdits> {
        let mut st = self.conn.prepare_cached(
            "SELECT l.rel, l.ordinal, l.stamp, l.edit FROM library_items l
             JOIN sessions s ON s.id = l.session_id WHERE s.root = ?",
        )?;
        let rows = st.query_map([root.to_string_lossy()], |r| {
            let edit: Option<Vec<u8>> = r.get(3)?;
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, u32>(1)?),
                (
                    r.get::<_, i64>(2)?,
                    edit.and_then(|e| <[u8; 32]>::try_from(e).ok()),
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn library_rows_of(
        conn: &Connection,
        session: i64,
    ) -> Result<HashMap<(String, u32), (i64, LibraryRecord)>> {
        let mut st = conn.prepare_cached(
            "SELECT id, rel, ordinal, variant, capture_ms, rating, label, pick, camera, lens,
                    keywords, title, gps, stamp, edit
             FROM library_items WHERE session_id = ?",
        )?;
        let rows = st.query_map([session], |r| {
            let kw: String = r.get(10)?;
            let edit: Option<Vec<u8>> = r.get(14)?;
            let rec = LibraryRecord {
                rel: r.get(1)?,
                ordinal: r.get(2)?,
                variant: r.get(3)?,
                capture_ms: r.get(4)?,
                rating: r.get(5)?,
                label: tag_from_code(r.get(6)?),
                pick: pick_from_code(r.get(7)?),
                camera: r.get(8)?,
                lens: r.get(9)?,
                keywords: split(&kw).map(str::to_owned).collect(),
                title: r.get(11)?,
                gps: r.get(12)?,
                stamp: r.get(13)?,
                edit: edit.and_then(|e| <[u8; 32]>::try_from(e).ok()),
            };
            Ok(((rec.rel.clone(), rec.ordinal), (r.get::<_, i64>(0)?, rec)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The rows to write (`None` id: new) and the row ids to delete to bring session
    /// `id` to `records`, or `None` when its rows and `source_row` are already current.
    fn library_diff<'r>(
        conn: &Connection,
        id: i64,
        records: &'r [LibraryRecord],
        source_row: &(String, Option<String>, i64),
    ) -> Result<Option<LibraryDiff<'r>>> {
        let mut had = Self::library_rows_of(conn, id)?;
        let mut upserts: Vec<(Option<i64>, &LibraryRecord)> = Vec::new();
        let mut seen = HashSet::new();
        for r in records {
            let k = (r.rel.clone(), r.ordinal);
            if !seen.insert(k.clone()) {
                continue;
            }
            match had.remove(&k) {
                Some((_, old)) if old == *r => {}
                Some((rid, _)) => upserts.push((Some(rid), r)),
                None => upserts.push((None, r)),
            }
        }
        let gone: Vec<i64> = had.into_values().map(|(rid, _)| rid).collect();
        let known: Option<(String, Option<String>, i64)> = conn
            .query_row(
                "SELECT store, external, stamp FROM library_sources WHERE session_id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if upserts.is_empty() && gone.is_empty() && known.as_ref() == Some(source_row) {
            return Ok(None);
        }
        Ok(Some((upserts, gone)))
    }

    /// Register the session at `root` (the folder the app opens), record
    /// where it is, and bring its rows to `records`: only rows that changed
    /// are written, and nothing at all when none did. Returns whether any row
    /// changed.
    pub fn sync_library(
        &mut self,
        root: &Path,
        name: &str,
        source: &LibrarySource,
        records: &[LibraryRecord],
    ) -> Result<bool> {
        let id = self.register_session(root, name)?;
        let source_row = (
            source.store.to_string_lossy().into_owned(),
            source
                .external
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            source.stamp,
        );
        // Unchanged: nothing is written and the write lock is never taken.
        if Self::library_diff(&self.conn, id, records, &source_row)?.is_none() {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Again under the write lock: another instance may have synced this session since.
        let Some((upserts, gone)) = Self::library_diff(&tx, id, records, &source_row)? else {
            return Ok(false);
        };
        tx.execute(
            "INSERT INTO library_sources(session_id, store, external, stamp) VALUES (?, ?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET store = excluded.store,
                external = excluded.external, stamp = excluded.stamp",
            params![id, source_row.0, source_row.1, source_row.2],
        )?;
        let rows_changed = !upserts.is_empty() || !gone.is_empty();
        {
            let mut del = tx.prepare_cached("DELETE FROM library_items WHERE id = ?")?;
            for rid in &gone {
                del.execute([rid])?;
            }
            let mut ins = tx.prepare_cached(
                "INSERT INTO library_items(id, session_id, rel, ordinal, variant, capture_ms, rating,
                    label, pick, camera, lens, keywords, title, gps, stamp, edit)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET variant = excluded.variant,
                    capture_ms = excluded.capture_ms, rating = excluded.rating,
                    label = excluded.label, pick = excluded.pick, camera = excluded.camera,
                    lens = excluded.lens, keywords = excluded.keywords, title = excluded.title,
                    gps = excluded.gps, stamp = excluded.stamp, edit = excluded.edit",
            )?;
            for (rid, r) in &upserts {
                ins.execute(params![
                    rid,
                    id,
                    r.rel,
                    r.ordinal,
                    r.variant,
                    r.capture_ms,
                    r.rating,
                    tag_code(r.label),
                    pick_code(r.pick),
                    r.camera,
                    r.lens,
                    join(&r.keywords),
                    r.title,
                    r.gps,
                    r.stamp,
                    r.edit.as_ref().map(|e| &e[..]),
                ])?;
            }
        }
        if rows_changed {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(rows_changed)
    }

    /// Every registered session and where it was when last synced.
    pub fn library_sessions(&self) -> Result<Vec<LibrarySession>> {
        let mut st = self.conn.prepare_cached(
            "SELECT s.id, s.root, s.name, l.store, l.external, l.stamp
             FROM sessions s LEFT JOIN library_sources l ON l.session_id = s.id
             ORDER BY s.name, s.root",
        )?;
        let rows = st.query_map([], |r| {
            let store: Option<String> = r.get(3)?;
            let external: Option<String> = r.get(4)?;
            let stamp: Option<i64> = r.get(5)?;
            Ok(LibrarySession {
                id: r.get(0)?,
                root: PathBuf::from(r.get::<_, String>(1)?),
                name: r.get(2)?,
                source: store.map(|s| LibrarySource {
                    store: PathBuf::from(s),
                    external: external.map(PathBuf::from),
                    stamp: stamp.unwrap_or(0),
                }),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (Re)load the rows when the Library changed since.
    pub(crate) fn load_library(&mut self) -> Result<()> {
        let generation = generation(&self.conn)?;
        if self.library.loaded && self.library.generation == generation {
            return Ok(());
        }
        let mut c = LibraryCache {
            loaded: true,
            generation,
            ..LibraryCache::default()
        };
        {
            let mut st = self.conn.prepare_cached(
                "SELECT s.id, s.root, s.name, l.store, l.external
                 FROM sessions s LEFT JOIN library_sources l ON l.session_id = s.id",
            )?;
            let mut q = st.query([])?;
            while let Some(r) = q.next()? {
                c.sessions.insert(
                    r.get(0)?,
                    CachedSession {
                        root: PathBuf::from(r.get::<_, String>(1)?),
                        name: r.get(2)?,
                        store: r.get::<_, Option<String>>(3)?.map(PathBuf::from),
                        external: r.get::<_, Option<String>>(4)?.map(PathBuf::from),
                    },
                );
            }
        }
        {
            let mut st = self.conn.prepare_cached(
                "SELECT id, session_id, rel, ordinal, variant, capture_ms, rating, label, pick,
                        camera, lens, keywords, title, gps, edit
                 FROM library_items",
            )?;
            let mut q = st.query([])?;
            while let Some(r) = q.next()? {
                let camera = c.names.id(text_at(r, 9)?);
                let lens = c.names.id(text_at(r, 10)?);
                let start = u32::try_from(c.keywords.len()).unwrap_or(u32::MAX);
                for k in split(text_at(r, 11)?) {
                    let id = c.names.id(k);
                    c.keywords.push(id);
                }
                let kw = (
                    start,
                    u32::try_from(c.keywords.len()).unwrap_or(u32::MAX) - start,
                );
                let rel = c.push_text(text_at(r, 2)?);
                let title = c.push_text(text_at(r, 12)?);
                let edit = match r.get_ref(14)? {
                    rusqlite::types::ValueRef::Blob(b) => <[u8; 32]>::try_from(b).ok(),
                    _ => None,
                };
                c.rows.push(Row {
                    id: r.get(0)?,
                    session: r.get(1)?,
                    capture: r.get(5)?,
                    rating: r.get(6)?,
                    label: tag_from_code(r.get(7)?),
                    pick: pick_from_code(r.get(8)?),
                    gps: r.get(13)?,
                    camera,
                    lens,
                    kw,
                    rel,
                    title,
                    ordinal: r.get(3)?,
                    variant: r.get(4)?,
                    edit,
                });
            }
        }
        c.index = c
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id, i as u32))
            .collect();
        let mut order: Vec<u32> = (0..c.rows.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let (x, y) = (&c.rows[a as usize], &c.rows[b as usize]);
            capture_order(x.capture, y.capture)
                .then_with(|| c.str(x.rel).cmp(c.str(y.rel)))
                .then(x.ordinal.cmp(&y.ordinal))
        });
        c.by_capture = order;
        self.library = c;
        Ok(())
    }

    /// The ids of the photos `q` keeps, in its order.
    pub fn library_query(&mut self, q: &LibraryQuery) -> Result<Vec<i64>> {
        self.load_library()?;
        let members = match q.collection {
            Some(id) => Some(self.collection_mask(id)?),
            None => None,
        };
        let words = words(&q.text);
        if !words.is_empty() {
            self.library.folded();
        }
        self.library.order(q.sort);
        let c = &self.library;
        let keep = Matcher::new(c, q, &words);
        let base = match q.sort {
            LibrarySort::Name => c.by_name.as_deref(),
            LibrarySort::Session => c.by_session.as_deref(),
            LibrarySort::Captured | LibrarySort::Rating => None,
        }
        .unwrap_or(&c.by_capture);
        let mut out: Vec<u32> = base
            .iter()
            .copied()
            .filter(|&i| members.as_ref().is_none_or(|m| m[i as usize]) && keep.matches(i))
            .collect();
        if q.sort == LibrarySort::Rating {
            // Stable: equal ratings keep capture order.
            out.sort_by(|&a, &b| c.rows[b as usize].rating.cmp(&c.rows[a as usize].rating));
        }
        if q.descending {
            out.reverse();
        }
        Ok(out.into_iter().map(|i| c.rows[i as usize].id).collect())
    }

    /// The photos with these ids, in that order (ids no longer in the
    /// Library are left out).
    pub fn library_items(&mut self, ids: &[i64]) -> Result<Vec<LibraryItem>> {
        self.load_library()?;
        let c = &self.library;
        Ok(ids
            .iter()
            .filter_map(|id| c.index.get(id).map(|&i| c.item(&c.rows[i as usize])))
            .collect())
    }

    /// Sessions, days, cameras, keywords and collections with their counts.
    pub fn library_overview(&mut self) -> Result<LibraryOverview> {
        self.load_library()?;
        let collections = self.collections()?;
        let c = &self.library;
        let mut sessions: HashMap<i64, u64> = HashMap::new();
        let mut days: HashMap<i64, u64> = HashMap::new();
        let mut cameras: HashMap<u32, u64> = HashMap::new();
        let mut keywords: HashMap<u32, u64> = HashMap::new();
        let mut undated = 0;
        for r in &c.rows {
            *sessions.entry(r.session).or_default() += 1;
            match r.capture {
                Some(ms) => *days.entry(ms.div_euclid(DAY_MS)).or_default() += 1,
                None => undated += 1,
            }
            if r.camera != NONE {
                *cameras.entry(r.camera).or_default() += 1;
            }
            for &k in c.kws(r) {
                *keywords.entry(k).or_default() += 1;
            }
        }
        let mut s: Vec<LibrarySessionCount> = c
            .sessions
            .iter()
            .map(|(&id, s)| LibrarySessionCount {
                id,
                root: s.root.clone(),
                name: s.name.clone(),
                count: sessions.get(&id).copied().unwrap_or(0),
            })
            .collect();
        s.sort_by(|a, b| natural(&a.name, &b.name).then_with(|| a.root.cmp(&b.root)));
        let mut d: Vec<LibraryDay> = days
            .into_iter()
            .map(|(day, count)| {
                let cd = civil_from_days(day);
                LibraryDay {
                    year: cd.year,
                    month: cd.month,
                    day: cd.day,
                    count,
                }
            })
            .collect();
        d.sort_by_key(|d| (d.year, d.month, d.day));
        let named = |m: HashMap<u32, u64>| {
            let mut v: Vec<(String, u64)> = m
                .into_iter()
                .map(|(k, n)| (c.names.get(k).to_owned(), n))
                .collect();
            v.sort_by(|a, b| natural(&a.0, &b.0));
            v
        };
        Ok(LibraryOverview {
            total: c.rows.len() as u64,
            sessions: s,
            days: d,
            undated,
            cameras: named(cameras),
            keywords: named(keywords),
            collections,
        })
    }

    // ---------------------------------------------------------------- edits ---

    /// Where the photos with these ids live: per session, its store and the
    /// `(rel, ordinal)` of each photo with its id. Ids not in the Library, and
    /// sessions never synced (no store known), are left out.
    pub fn library_targets(&mut self, ids: &[i64]) -> Result<Vec<LibraryTargets>> {
        self.load_library()?;
        let c = &self.library;
        let mut by: HashMap<i64, LibraryTargets> = HashMap::new();
        for id in ids {
            let Some(&i) = c.index.get(id) else { continue };
            let row = &c.rows[i as usize];
            let Some(s) = c.sessions.get(&row.session) else {
                continue;
            };
            let Some(store) = &s.store else { continue };
            let t = by.entry(row.session).or_insert_with(|| LibraryTargets {
                session_root: s.root.clone(),
                session_name: s.name.clone(),
                store: store.clone(),
                photos: Vec::new(),
            });
            t.photos
                .push((row.id, (c.str(row.rel).to_owned(), row.ordinal)));
        }
        let mut out: Vec<LibraryTargets> = by.into_values().collect();
        out.sort_by(|a, b| a.session_root.cmp(&b.session_root));
        Ok(out)
    }

    /// Record `edit` on these rows (after their sessions wrote it). The
    /// in-memory rows are patched, not reloaded, unless someone else wrote
    /// meanwhile. Returns how many rows changed.
    pub fn library_patch(&mut self, ids: &[i64], edit: LibraryEdit) -> Result<usize> {
        let before = generation(&self.conn)?;
        let (col, value) = match edit {
            LibraryEdit::Rating(r) => ("rating", i64::from(r.stars())),
            LibraryEdit::Label(t) => ("label", tag_code(t)),
            LibraryEdit::Pick(p) => ("pick", pick_code(p)),
        };
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut n = 0;
        {
            let mut st = tx.prepare(&format!(
                "UPDATE library_items SET {col} = ?1 WHERE id = ?2 AND {col} <> ?1"
            ))?;
            for id in ids {
                n += st.execute(params![value, id])?;
            }
        }
        if n == 0 {
            return Ok(0);
        }
        let after = bump(&tx)?;
        tx.commit()?;
        let c = &mut self.library;
        if c.loaded && c.generation == before {
            for id in ids {
                let Some(&i) = c.index.get(id) else { continue };
                let row = &mut c.rows[i as usize];
                match edit {
                    LibraryEdit::Rating(r) => row.rating = r.stars(),
                    LibraryEdit::Label(t) => row.label = t,
                    LibraryEdit::Pick(p) => row.pick = p,
                }
            }
            c.generation = after;
        } else {
            c.loaded = false;
        }
        Ok(n)
    }

    // ---------------------------------------------------------- collections ---

    /// Every collection by name, with how many photos it holds now.
    pub fn collections(&mut self) -> Result<Vec<LibraryCollection>> {
        let rows: Vec<(i64, String, Option<String>)> = {
            let mut st = self
                .conn
                .prepare_cached("SELECT id, name, rules FROM collections")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for (id, name, rules) in rows {
            let rules: Option<LibraryQuery> = rules.and_then(|r| serde_json::from_str(&r).ok());
            let count = match &rules {
                Some(q) => self.library_query(&smart(q))?.len() as u64,
                None => self.collection_ids(id)?.len() as u64,
            };
            out.push(LibraryCollection {
                id,
                name,
                rules,
                count,
            });
        }
        out.sort_by(|a, b| natural(&a.name, &b.name).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// A new collection (`rules`: a smart one) holding `ids` (a plain one).
    pub fn create_collection(
        &mut self,
        name: &str,
        rules: Option<&LibraryQuery>,
        ids: &[i64],
    ) -> Result<i64> {
        let name = collection_name(name)?;
        let rules = rules.map(rules_json).transpose()?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO collections(name, rules) VALUES (?, ?)",
            params![name, rules],
        )?;
        let id = tx.last_insert_rowid();
        bump(&tx)?;
        tx.commit()?;
        if rules.is_none() {
            self.add_to_collection(id, ids)?;
        }
        Ok(id)
    }

    /// Rename a collection, or change a smart one's rules. Returns whether it exists.
    pub fn update_collection(
        &mut self,
        id: i64,
        name: &str,
        rules: Option<&LibraryQuery>,
    ) -> Result<bool> {
        let name = collection_name(name)?;
        let tx = self.conn.transaction()?;
        let smart_now: Option<Option<String>> = tx
            .query_row("SELECT rules FROM collections WHERE id = ?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(had) = smart_now else {
            return Ok(false);
        };
        // A plain collection stays plain (its photos are its definition).
        let rules = match (had, rules) {
            (Some(_), Some(q)) => Some(rules_json(q)?),
            (had, _) => had,
        };
        tx.execute(
            "UPDATE collections SET name = ?, rules = ? WHERE id = ?",
            params![name, rules, id],
        )?;
        bump(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn delete_collection(&mut self, id: i64) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM collections WHERE id = ?", [id])?;
        if n > 0 {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Add photos to a plain collection; returns how many were new to it.
    pub fn add_to_collection(&mut self, id: i64, ids: &[i64]) -> Result<usize> {
        self.change_members(id, ids, true)
    }

    /// Take photos out of a plain collection; returns how many it held.
    pub fn remove_from_collection(&mut self, id: i64, ids: &[i64]) -> Result<usize> {
        self.change_members(id, ids, false)
    }

    fn change_members(&mut self, id: i64, ids: &[i64], add: bool) -> Result<usize> {
        let smart: Option<Option<String>> = self
            .conn
            .query_row("SELECT rules FROM collections WHERE id = ?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        match smart {
            None => {
                return Err(SessionError::NotFound {
                    kind: "collection",
                    id,
                });
            }
            Some(Some(_)) => return Err(SessionError::SmartCollection(id)),
            Some(None) => {}
        }
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let sql = if add {
                "INSERT OR IGNORE INTO collection_items(collection_id, session_id, rel, ordinal)
                 SELECT ?1, session_id, rel, ordinal FROM library_items WHERE id = ?2"
            } else {
                "DELETE FROM collection_items WHERE collection_id = ?1 AND (session_id, rel, ordinal) =
                 (SELECT session_id, rel, ordinal FROM library_items WHERE id = ?2)"
            };
            let mut st = tx.prepare(sql)?;
            for item in ids {
                n += st.execute(params![id, item])?;
            }
        }
        if n > 0 {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// The Library ids of a plain collection's photos (those still synced).
    fn collection_ids(&self, id: i64) -> Result<HashSet<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT l.id FROM collection_items c JOIN library_items l
             ON l.session_id = c.session_id AND l.rel = c.rel AND l.ordinal = c.ordinal
             WHERE c.collection_id = ?",
        )?;
        Ok(st
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Which rows (by index) a collection holds: a plain one's photos, or
    /// those a smart one's rules keep. A collection deleted meanwhile holds none.
    fn collection_mask(&mut self, id: i64) -> Result<Vec<bool>> {
        let rules: Option<Option<String>> = self
            .conn
            .query_row("SELECT rules FROM collections WHERE id = ?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let n = self.library.rows.len();
        Ok(match rules {
            Some(Some(r)) => {
                // Unreadable rules (a newer build's) keep nothing, as the list says.
                let Ok(q) = serde_json::from_str::<LibraryQuery>(&r) else {
                    return Ok(vec![false; n]);
                };
                let words = words(&q.text);
                if !words.is_empty() {
                    self.library.folded();
                }
                let c = &self.library;
                let m = Matcher::new(c, &q, &words);
                (0..n as u32).map(|i| m.matches(i)).collect()
            }
            Some(None) => {
                let ids = self.collection_ids(id)?;
                let c = &self.library;
                let mut mask = vec![false; n];
                for i in ids.iter().filter_map(|id| c.index.get(id)) {
                    mask[*i as usize] = true;
                }
                mask
            }
            None => vec![false; n],
        })
    }
}

/// One session's photos to edit (see [`GlobalIndex::library_targets`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryTargets {
    pub session_root: PathBuf,
    pub session_name: String,
    pub store: PathBuf,
    /// `(Library id, (rel, ordinal))`.
    pub photos: Vec<(i64, (String, u32))>,
}

const DAY_MS: i64 = 86_400_000;

fn collection_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SessionError::InvalidName(name.to_owned()));
    }
    Ok(trimmed)
}

fn rules_json(q: &LibraryQuery) -> Result<String> {
    serde_json::to_string(&smart(q)).map_err(|e| json_err("collection rules", &e))
}

/// A smart collection's rules: the criteria without a collection or order.
fn smart(q: &LibraryQuery) -> LibraryQuery {
    LibraryQuery {
        collection: None,
        sort: LibrarySort::default(),
        descending: false,
        ..q.clone()
    }
}

fn capture_order(a: Option<i64>, b: Option<i64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// A byte-comparable key in [`natural`] order (ties aside): lower case, each
/// digit run without leading zeros, left-padded to 20 digits.
fn natural_key(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 16);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut run = String::from(c);
            while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                run.push(d);
                chars.next();
            }
            let n = run.trim_start_matches('0');
            out.extend(std::iter::repeat_n(b'0', 20usize.saturating_sub(n.len())));
            out.extend_from_slice(n.as_bytes());
        } else {
            let mut buf = [0u8; 4];
            for l in c.to_lowercase() {
                out.extend_from_slice(l.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

/// Case-insensitive, with digit runs compared as numbers ("DSC2" < "DSC10").
pub(crate) fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let num = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (num(&mut x), num(&mut y));
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let o = m.len().cmp(&n.len()).then_with(|| m.cmp(n));
                if o.is_ne() {
                    return o;
                }
            }
            (Some(p), Some(q)) => {
                let o = p.to_lowercase().cmp(q.to_lowercase());
                if o.is_ne() {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// A query's criteria resolved against the cache (names to ids).
/// Rows to upsert (`None` id: a new row) and row ids to delete.
type LibraryDiff<'r> = (Vec<(Option<i64>, &'r LibraryRecord)>, Vec<i64>);

struct Matcher<'a> {
    c: &'a LibraryCache,
    q: &'a LibraryQuery,
    words: &'a [String],
    sessions: HashSet<i64>,
    cameras: Option<HashSet<u32>>,
    /// Keyword ids that may stand for each wanted keyword (any case).
    keywords: Vec<HashSet<u32>>,
    /// Session ids whose name holds each word.
    session_words: Vec<HashSet<i64>>,
    /// Each word with a leading space (a word start in the folded text), built once.
    needles: Vec<String>,
}

impl<'a> Matcher<'a> {
    fn new(c: &'a LibraryCache, q: &'a LibraryQuery, words: &'a [String]) -> Self {
        let cameras = (!q.cameras.is_empty()).then(|| {
            q.cameras
                .iter()
                .filter_map(|n| c.names.ids.get(n).copied())
                .collect()
        });
        let keywords = q
            .keywords
            .iter()
            .map(|k| {
                let k = k.to_lowercase();
                c.names
                    .list
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.to_lowercase() == k)
                    .map(|(i, _)| i as u32)
                    .collect()
            })
            .collect();
        let session_words = words
            .iter()
            .map(|w| {
                c.sessions
                    .iter()
                    .filter(|(_, s)| format!(" {} ", fold(&s.name)).contains(&format!(" {w}")))
                    .map(|(&id, _)| id)
                    .collect()
            })
            .collect();
        Self {
            c,
            q,
            words,
            sessions: q.sessions.iter().copied().collect(),
            cameras,
            keywords,
            session_words,
            needles: words.iter().map(|w| format!(" {w}")).collect(),
        }
    }

    fn matches(&self, i: u32) -> bool {
        let (c, q) = (self.c, self.q);
        let r = &c.rows[i as usize];
        if !self.sessions.is_empty() && !self.sessions.contains(&r.session) {
            return false;
        }
        if q.from_ms.is_some() || q.to_ms.is_some() {
            let Some(t) = r.capture else { return false };
            if q.from_ms.is_some_and(|f| t < f) || q.to_ms.is_some_and(|e| t >= e) {
                return false;
            }
        }
        if r.rating < q.min_rating || (q.unrated && r.rating > 0) {
            return false;
        }
        if !q.labels.is_empty() && !q.labels.contains(&r.label) {
            return false;
        }
        if !q.pick.keeps(r.pick) {
            return false;
        }
        if let Some(cams) = &self.cameras
            && !cams.contains(&r.camera)
        {
            return false;
        }
        if !self.keywords.is_empty() {
            let kws = c.kws(r);
            if !self
                .keywords
                .iter()
                .all(|want| kws.iter().any(|k| want.contains(k)))
            {
                return false;
            }
        }
        if !self.words.is_empty() {
            let hay = c.folded.as_ref().map_or("", |f| f[i as usize].as_str());
            for (w, sessions) in self.needles.iter().zip(&self.session_words) {
                // Word starts: "dsc" finds "DSC0001", "hike" finds "Fall Hike".
                if !hay.contains(w.as_str()) && !sessions.contains(&r.session) {
                    return false;
                }
            }
        }
        true
    }
}
