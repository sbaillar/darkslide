//! Find across sessions: "pics of alex in iceland last summer".
//!
//! The global index keeps one `find_items` row per original of every
//! registered session (path, content key, capture time, rating, flag,
//! keywords, people, location), synced from the session whenever the app asks
//! ([`GlobalIndex::sync_find_items`]; an unchanged session writes nothing).
//! [`GlobalIndex::find`] evaluates a resolved [`FindSpec`] against an
//! in-memory copy of those rows (rebuilt when the index's search generation
//! moves), with the image-text vectors of the search index:
//!
//! - **Hard filters**: people (a term naming known people: the photo is tagged
//!   with one of them, or a face was matched to one), places that resolve to
//!   something in the library (a session name, a folder in the path, a keyword,
//!   a location field), the date, rating and flag.
//! - **Ranking**: the descriptive text's similarity to the photo, plus, for a
//!   place nothing in the library is called, its similarity as a soft signal
//!   ("looks like: iceland"). A person term no one is named yet only boosts
//!   photos whose keywords or folders contain it.
//!
//! Every hit says why it matched ([`MatchReason`]); sessions come back
//! separately when their name, a folder or a keyword matched a place, or when
//! they hold many of the hits.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ds_core::date::DateFilter;
use ds_core::embedding;
use ds_core::meta::PickState;
use ds_core::{FileHash, IptcMeta};
use rusqlite::params;

use crate::embed::KEY_SQL;
use crate::error::Result;
use crate::filter::{pick_code, pick_from_code};
use crate::global::{GlobalIndex, bump_generation};
use crate::relpath::Folder;
use crate::session::Session;

/// v3 of the global index: per-original find data (applied on top of v2);
/// v4 added `find_items.titles` (see [`TO_V4`]).
pub(crate) const FIND_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS find_items(
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    key BLOB,
    capture_ms INTEGER,
    rating INTEGER NOT NULL,
    pick INTEGER NOT NULL,
    keywords TEXT NOT NULL,
    people TEXT NOT NULL,
    suggested TEXT NOT NULL,
    location TEXT NOT NULL,
    rejected TEXT NOT NULL,
    titles TEXT NOT NULL DEFAULT '',
    lat REAL,
    lon REAL,
    PRIMARY KEY(session_id, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS faces(
    key BLOB NOT NULL,
    model TEXT NOT NULL,
    idx INTEGER NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(key, model, idx)
) WITHOUT ROWID;
";

/// v3 -> v4: the titles and captions column. Rows start empty and are filled
/// by each session's next `sync_find_items`.
pub(crate) const TO_V4: &str = "ALTER TABLE find_items ADD COLUMN titles TEXT NOT NULL DEFAULT '';";

/// One original as Find sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FindRecord {
    /// The original's path (as the app shows it).
    pub path: PathBuf,
    /// Content identity (`None` until hashed: no image-text score yet).
    pub key: Option<FileHash>,
    pub capture_ms: Option<i64>,
    /// The best rating of its variants.
    pub rating: u8,
    /// The first variant's flag.
    pub pick: PickState,
    /// Every variant's keywords.
    pub keywords: Vec<String>,
    /// People tagged on it (sidecars), as named in the people store.
    pub people: Vec<String>,
    /// People its faces were matched to but nobody confirmed.
    pub suggested: Vec<String>,
    /// Location fields ("Vik", "Sudurland", "Iceland").
    pub location: Vec<String>,
    /// People someone said aren't on it (never suggested).
    pub rejected: Vec<String>,
    /// Its variants' titles and captions (IPTC), distinct, in order.
    pub titles: Vec<String>,
    /// Where it was taken (latitude, longitude): the master variant's geotag,
    /// else the original's GPS as the places scan read it (EXIF, then XMP).
    pub gps: Option<(f64, f64)>,
}

fn join(list: &[String]) -> String {
    list.iter()
        .map(|s| s.replace('\n', " "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn split(s: &str) -> Vec<String> {
    s.split('\n')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `(image id, rel path, content key, capture ms, location names, GPS)`.
type ImageRow = (
    i64,
    String,
    Option<Vec<u8>>,
    Option<i64>,
    Option<String>,
    Option<(f64, f64)>,
);

/// `(rating, pick, keywords, IPTC JSON, geotag)` of one variant.
type VariantRow = (u8, i64, String, String, Option<(f64, f64)>);

impl Session {
    /// Every present original outside the Trash, for Find, in path order.
    pub fn find_records(&self) -> Result<Vec<FindRecord>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.id, i.rel_path, {KEY_SQL} AS k, i.capture_ms, l.names, l.lat, l.lon
             FROM images i LEFT JOIN locations l ON l.key = {KEY_SQL}
             WHERE i.missing = 0 AND i.folder <> ?1 ORDER BY i.rel_path"
        ))?;
        let images: Vec<ImageRow> = st
            .query_map([Folder::Trash.code()], |r| {
                let gps = r.get::<_, Option<f64>>(5)?.zip(r.get::<_, Option<f64>>(6)?);
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, gps))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut st = self.conn.prepare_cached(
            "SELECT image_id, rating, pick, keywords, iptc, geo_lat, geo_lon FROM variants
             ORDER BY image_id, ordinal",
        )?;
        let mut names: HashMap<i64, (Vec<String>, Vec<String>)> = HashMap::new();
        let mut st2 = self.conn.prepare_cached(
            "SELECT v.image_id, p.name, p.rejected FROM variant_people p
             JOIN variants v ON v.id = p.variant_id ORDER BY v.image_id, v.ordinal, p.ord",
        )?;
        let rows = st2.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        for row in rows {
            let (image, name, rejected) = row?;
            let e = names.entry(image).or_default();
            let list = if rejected { &mut e.1 } else { &mut e.0 };
            if !list.contains(&name) {
                list.push(name);
            }
        }
        let mut variants: HashMap<i64, Vec<VariantRow>> = HashMap::new();
        let rows = st.query_map([], |r| {
            let geo = r.get::<_, Option<f64>>(5)?.zip(r.get::<_, Option<f64>>(6)?);
            Ok((
                r.get::<_, i64>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, geo),
            ))
        })?;
        for row in rows {
            let (image, v) = row?;
            variants.entry(image).or_default().push(v);
        }
        Ok(images
            .into_iter()
            .map(|(id, rel, key, capture_ms, location, gps)| {
                let vs = variants.remove(&id).unwrap_or_default();
                let mut keywords: Vec<String> = Vec::new();
                let mut titles: Vec<String> = Vec::new();
                for (_, _, kw, iptc, _) in &vs {
                    for k in serde_json::from_str::<Vec<String>>(kw).unwrap_or_default() {
                        if !keywords.contains(&k) {
                            keywords.push(k);
                        }
                    }
                    let m: IptcMeta = serde_json::from_str(iptc).unwrap_or_default();
                    for t in [m.title, m.caption] {
                        if !t.is_empty() && !titles.contains(&t) {
                            titles.push(t);
                        }
                    }
                }
                FindRecord {
                    path: self.original_path(&rel),
                    key: key.and_then(|k| <[u8; 32]>::try_from(k).ok().map(FileHash)),
                    capture_ms,
                    rating: vs.iter().map(|v| v.0).max().unwrap_or(0),
                    pick: vs
                        .first()
                        .map_or(PickState::Unflagged, |v| pick_from_code(v.1)),
                    keywords,
                    people: names.get(&id).map(|n| n.0.clone()).unwrap_or_default(),
                    rejected: names.get(&id).map(|n| n.1.clone()).unwrap_or_default(),
                    location: location.as_deref().map(split).unwrap_or_default(),
                    titles,
                    gps: vs.first().and_then(|v| v.4).or(gps),
                    ..FindRecord::default()
                }
            })
            .collect())
    }
}

// ------------------------------------------------------------------ query ---

/// A person term and the known people it may mean (`names` empty: nobody is
/// named that yet).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PersonQuery {
    pub term: String,
    pub names: Vec<String>,
}

/// Which results count. `min_score`/`window` apply to the primary score (the
/// text, else the soft places); `place_weight` scales a soft place's score
/// when it only boosts a text query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FindOptions {
    pub limit: usize,
    pub min_score: f32,
    pub window: f32,
    pub place_weight: f32,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            limit: 500,
            min_score: 0.18,
            window: 0.09,
            place_weight: 0.5,
        }
    }
}

/// A parsed, resolved query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FindSpec {
    pub people: Vec<PersonQuery>,
    /// Place terms (any case).
    pub places: Vec<String>,
    pub date: Option<DateFilter>,
    pub min_rating: Option<u8>,
    pub max_rating: Option<u8>,
    pub pick: Option<PickState>,
    /// The descriptive text (for session-name matches and the reason).
    pub text: String,
    /// Image-text model the vectors are for.
    pub model: String,
    /// The text's embedding (`None`: no text, or no model).
    pub text_vector: Option<Vec<f32>>,
    /// Embeddings of place terms, used for places nothing in the library is called.
    pub place_vectors: HashMap<String, Vec<f32>>,
    pub options: FindOptions,
}

/// Where a place matched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlaceSource {
    /// A photo location field (XMP/IPTC, or a reverse-geocoded position).
    Location(String),
    SessionName(String),
    /// A folder in the photo's path.
    Folder(String),
    Keyword(String),
    /// Nothing in the library is called that: the photo looks like it.
    LooksLike,
}

/// Why a photo is a hit.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchReason {
    Person {
        name: String,
        suggested: bool,
    },
    Place {
        term: String,
        source: PlaceSource,
    },
    Date,
    Rating(u8),
    Flag(PickState),
    /// The description's similarity.
    Visual {
        score: f32,
    },
    /// An unknown person's name in its keywords or folders.
    Text {
        term: String,
    },
    /// The description, word for word, in its title or caption (the one given).
    Title {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FindPhoto {
    pub session_root: PathBuf,
    pub session_name: String,
    pub path: PathBuf,
    pub score: f32,
    pub capture_ms: Option<i64>,
    pub reasons: Vec<MatchReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReason {
    /// Its name contains a place or the description.
    Name { term: String },
    /// A folder of its photos is named after a place.
    Folder { term: String, folder: String },
    /// Photos in it have the place as a keyword.
    Keyword { term: String, keyword: String },
    /// It holds many of the hits.
    ManyHits,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMatch {
    pub root: PathBuf,
    pub name: String,
    pub reason: SessionReason,
    /// Photos of it among all hits (before the limit).
    pub hits: usize,
    pub best_score: f32,
}

/// How a place term was understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceResolution {
    pub term: String,
    /// What matched in the library; empty = only "looks like" (or nothing,
    /// without a vector for it).
    pub sources: Vec<PlaceSource>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FindOutcome {
    pub photos: Vec<FindPhoto>,
    pub sessions: Vec<SessionMatch>,
    pub places: Vec<PlaceResolution>,
    /// Person terms naming nobody in the people store.
    pub unknown_people: Vec<String>,
    /// Hits before `limit`.
    pub total: usize,
}

// ------------------------------------------------------------------ cache ---

/// Lower case, accents and punctuation folded to single spaces (the same
/// folding as the query parser).
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'ç' | 'č' | 'ć' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'ñ' | 'ń' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
            'ý' | 'ÿ' => 'y',
            'ł' => 'l',
            'š' | 'ś' => 's',
            'ž' | 'ź' | 'ż' => 'z',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        };
        if c != ' ' || (!out.is_empty() && !out.ends_with(' ')) {
            out.push(c);
        }
    }
    let n = out.trim_end().len();
    out.truncate(n);
    out
}

/// `needle` appears in `hay` as whole words (both folded).
fn has_phrase(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        before.is_none_or(|c| c == ' ') && after.is_none_or(|c| c == ' ')
    })
}

/// Distinct strings, each stored once (original, folded).
#[derive(Debug, Default)]
struct Vocab {
    items: Vec<(String, String)>,
    ids: HashMap<String, u32>,
}

impl Vocab {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.items.len() as u32;
        self.items.push((s.to_owned(), fold(s)));
        self.ids.insert(s.to_owned(), id);
        id
    }

    /// Ids whose folded text contains `term` as whole words, with the original.
    fn matching(&self, term: &str) -> HashMap<u32, String> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, (_, f))| has_phrase(f, term))
            .map(|(i, (o, _))| (i as u32, o.clone()))
            .collect()
    }

    /// Ids whose folded text equals one of `terms`.
    fn equal_to(&self, terms: &HashSet<String>) -> HashSet<u32> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, (_, f))| terms.contains(f))
            .map(|(i, _)| i as u32)
            .collect()
    }
}

#[derive(Debug)]
struct Item {
    session: i64,
    path: PathBuf,
    /// Its parent folder (index into `folders`).
    folder: u32,
    /// Row in the search matrix.
    row: Option<usize>,
    capture_ms: Option<i64>,
    rating: u8,
    pick: PickState,
    keywords: Vec<u32>,
    people: Vec<u32>,
    suggested: Vec<u32>,
    location: Vec<u32>,
    titles: Vec<u32>,
}

/// Find data in memory, valid for one search generation and model.
#[derive(Debug, Default)]
pub(crate) struct FindCache {
    generation: i64,
    model: String,
    loaded: bool,
    items: Vec<Item>,
    /// Folder path → its folder names (each a vocab entry).
    folders: Vec<Vec<u32>>,
    names: Vocab,
    keywords: Vocab,
    people: Vocab,
    locations: Vocab,
    titles: Vocab,
    sessions: HashMap<i64, (PathBuf, String)>,
}

impl GlobalIndex {
    /// Register the session at `root` and replace its find rows with
    /// `records`. Returns whether anything changed (nothing is written, and
    /// the in-memory data stays valid, when nothing did).
    pub fn sync_find_items(
        &mut self,
        root: &Path,
        name: &str,
        records: &[FindRecord],
    ) -> Result<bool> {
        let id = self.register_session(root, name)?;
        let mut wanted: Vec<FindRecord> = records.to_vec();
        wanted.sort_by(|a, b| a.path.cmp(&b.path));
        wanted.dedup_by(|a, b| a.path == b.path);
        if self.find_items_of(id)? == wanted {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM find_items WHERE session_id = ?", [id])?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO find_items(session_id, path, key, capture_ms, rating, pick,
                    keywords, people, suggested, location, rejected, titles, lat, lon)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for r in &wanted {
                st.execute(params![
                    id,
                    r.path.to_string_lossy(),
                    r.key.as_ref().map(|k| &k.0[..]),
                    r.capture_ms,
                    r.rating,
                    pick_code(r.pick),
                    join(&r.keywords),
                    join(&r.people),
                    join(&r.suggested),
                    join(&r.location),
                    join(&r.rejected),
                    join(&r.titles),
                    r.gps.map(|g| g.0),
                    r.gps.map(|g| g.1),
                ])?;
            }
        }
        prune_faces(&tx)?;
        bump_generation(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    /// Store the faces found on these photos (`(key, index, vector)`),
    /// replacing what the index had for each key. Returns whether anything
    /// changed (a key whose face indices are unchanged isn't rewritten).
    pub fn sync_faces(&mut self, model: &str, faces: &[(FileHash, u32, Vec<f32>)]) -> Result<bool> {
        let mut by_key: HashMap<FileHash, Vec<(u32, &[f32])>> = HashMap::new();
        for (k, i, v) in faces {
            by_key.entry(*k).or_default().push((*i, v));
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut changed = false;
        {
            let mut have = tx
                .prepare_cached("SELECT idx FROM faces WHERE key = ? AND model = ? ORDER BY idx")?;
            let mut del = tx.prepare_cached("DELETE FROM faces WHERE key = ? AND model = ?")?;
            let mut ins =
                tx.prepare_cached("INSERT INTO faces(key, model, idx, vec) VALUES (?, ?, ?, ?)")?;
            for (key, list) in &mut by_key {
                list.sort_by_key(|f| f.0);
                list.dedup_by_key(|f| f.0);
                let had: Vec<u32> = have
                    .query_map(params![&key.0[..], model], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                if had == list.iter().map(|f| f.0).collect::<Vec<_>>() {
                    continue;
                }
                del.execute(params![&key.0[..], model])?;
                for (i, v) in list.iter() {
                    if let Some(unit) = embedding::normalized(v) {
                        ins.execute(params![&key.0[..], model, i, embedding::encode_f16(&unit)])?;
                    }
                }
                changed = true;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Recompute every photo's suggested people: those a face on it resembles
    /// (mean similarity to the person's confirmed faces ≥ `threshold`), minus
    /// people tagged on it or rejected there. Returns how many photos changed.
    pub fn refresh_suggestions(
        &mut self,
        model: &str,
        people: &[PersonCentroid],
        threshold: f32,
    ) -> Result<usize> {
        let mut suggested: HashMap<Vec<u8>, Vec<&str>> = HashMap::new();
        {
            let mut st = self
                .conn
                .prepare_cached("SELECT key, vec FROM faces WHERE model = ?")?;
            let rows = st.query_map([model], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (key, vec) = row?;
                let Some(v) = embedding::decode_f16(&vec) else {
                    continue;
                };
                for p in people {
                    if p.similarity(&v) >= threshold {
                        let list = suggested.entry(key.clone()).or_default();
                        if !list.contains(&p.name.as_str()) {
                            list.push(&p.name);
                        }
                    }
                }
            }
        }
        let rows: Vec<SuggestionRow> = {
            let mut st = self.conn.prepare_cached(
                "SELECT session_id, path, key, people, rejected, suggested FROM find_items",
            )?;
            st.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut n = 0;
        {
            let mut up = tx.prepare_cached(
                "UPDATE find_items SET suggested = ? WHERE session_id = ? AND path = ?",
            )?;
            for (session, path, key, tagged, rejected, had) in rows {
                let not: HashSet<String> = split(&tagged)
                    .into_iter()
                    .chain(split(&rejected))
                    .map(|s| s.to_lowercase())
                    .collect();
                let mut want: Vec<String> = key
                    .and_then(|k| suggested.get(&k))
                    .into_iter()
                    .flatten()
                    .filter(|n| !not.contains(&n.to_lowercase()))
                    .map(|n| (*n).to_owned())
                    .collect();
                want.sort();
                let want = join(&want);
                if want != had {
                    up.execute(params![want, session, path])?;
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

    fn find_items_of(&self, session: i64) -> Result<Vec<FindRecord>> {
        let mut st = self.conn.prepare_cached(
            "SELECT path, key, capture_ms, rating, pick, keywords, people, suggested, location, rejected,
                titles, lat, lon
             FROM find_items WHERE session_id = ? ORDER BY path",
        )?;
        Ok(st
            .query_map([session], record_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Run a find query (see the module docs). The text and place vectors
    /// must come from `spec.model`.
    pub fn find(&mut self, spec: &FindSpec) -> Result<FindOutcome> {
        self.load_find(&spec.model)?;
        Ok(evaluate(&self.find, &self.search.matrix, spec))
    }

    /// Place names the library knows, for the query parser to recognize
    /// anywhere: session names and the names of folders photos sit in, minus
    /// their numbers ("2024-07 Iceland" → "iceland"; generic session folders
    /// left out), and location values. Folded, sorted, unique.
    pub fn find_places(&mut self, model: &str) -> Result<Vec<String>> {
        self.load_find(model)?;
        let c = &self.find;
        let strip = |s: &str| {
            fold(s)
                .split(' ')
                .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let parents: HashSet<u32> = c.folders.iter().filter_map(|f| f.last().copied()).collect();
        let mut out: Vec<String> = c
            .sessions
            .values()
            .map(|(_, n)| strip(n))
            .chain(parents.iter().map(|&i| strip(&c.names.items[i as usize].0)))
            .chain(c.locations.items.iter().map(|(_, f)| f.clone()))
            .filter(|p| p.chars().count() > 2 && !GENERIC_FOLDERS.contains(&p.as_str()))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn load_find(&mut self, model: &str) -> Result<()> {
        // The vectors (and their key → row map) come from the search cache.
        self.load_search(model)?;
        let generation = self.search.generation;
        let c = &self.find;
        if c.loaded && c.generation == generation && c.model == model {
            return Ok(());
        }
        let mut cache = FindCache {
            generation,
            model: model.to_owned(),
            loaded: true,
            ..FindCache::default()
        };
        let mut folders: HashMap<PathBuf, u32> = HashMap::new();
        let mut st = self.conn.prepare_cached(
            "SELECT session_id, path, key, capture_ms, rating, pick, keywords, people, suggested, location, rejected,
                titles, lat, lon
             FROM find_items ORDER BY session_id, path",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let session: i64 = r.get(0)?;
            let rec = record_from_row_at(r, 1)?;
            let parent = rec.path.parent().map(Path::to_path_buf).unwrap_or_default();
            let folder = if let Some(&f) = folders.get(&parent) {
                f
            } else {
                let names = parent
                    .components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .map(|n| cache.names.id(&n))
                    .collect();
                cache.folders.push(names);
                let f = (cache.folders.len() - 1) as u32;
                folders.insert(parent, f);
                f
            };
            let ids = |v: &mut Vocab, list: &[String]| list.iter().map(|s| v.id(s)).collect();
            cache.items.push(Item {
                session,
                folder,
                row: rec.key.and_then(|k| self.search.rows.get(&k.0).copied()),
                capture_ms: rec.capture_ms,
                rating: rec.rating,
                pick: rec.pick,
                keywords: ids(&mut cache.keywords, &rec.keywords),
                people: ids(&mut cache.people, &rec.people),
                suggested: ids(&mut cache.people, &rec.suggested),
                location: ids(&mut cache.locations, &rec.location),
                titles: ids(&mut cache.titles, &rec.titles),
                path: rec.path,
            });
        }
        drop(rows);
        drop(st);
        for s in self.sessions()? {
            cache.sessions.insert(s.id, (s.root, s.name));
        }
        self.find = cache;
        Ok(())
    }
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FindRecord> {
    record_from_row_at(r, 0)
}

fn record_from_row_at(r: &rusqlite::Row<'_>, o: usize) -> rusqlite::Result<FindRecord> {
    let key: Option<Vec<u8>> = r.get(o + 1)?;
    Ok(FindRecord {
        path: PathBuf::from(r.get::<_, String>(o)?),
        key: key.and_then(|k| <[u8; 32]>::try_from(k).ok().map(FileHash)),
        capture_ms: r.get(o + 2)?,
        rating: r.get(o + 3)?,
        pick: pick_from_code(r.get(o + 4)?),
        keywords: split(&r.get::<_, String>(o + 5)?),
        people: split(&r.get::<_, String>(o + 6)?),
        suggested: split(&r.get::<_, String>(o + 7)?),
        location: split(&r.get::<_, String>(o + 8)?),
        rejected: split(&r.get::<_, String>(o + 9)?),
        titles: split(&r.get::<_, String>(o + 10)?),
        gps: r
            .get::<_, Option<f64>>(o + 11)?
            .zip(r.get::<_, Option<f64>>(o + 12)?),
    })
}

/// Folder names that say nothing about a place.
const GENERIC_FOLDERS: &[&str] = &[
    "capture",
    "selects",
    "trash",
    "output",
    "dcim",
    "photos",
    "pictures",
    "raw",
    "export",
    "exports",
    "originals",
    "volumes",
    "users",
    "msdcf",
    "import",
    "imports",
];

/// `(session, path, key, tagged, rejected, suggested)` of a find row.
type SuggestionRow = (i64, String, Option<Vec<u8>>, String, String, String);

/// Faces no photo of any session uses any more.
pub(crate) fn prune_faces(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM faces WHERE key NOT IN (SELECT key FROM find_items WHERE key IS NOT NULL)",
        [],
    )?;
    Ok(())
}

/// A known person for suggestions: the sum of their confirmed faces' unit
/// vectors and how many (mean similarity of a face = `dot(face, sum) / count`).
#[derive(Debug, Clone, PartialEq)]
pub struct PersonCentroid {
    pub name: String,
    pub sum: Vec<f32>,
    pub count: usize,
}

impl PersonCentroid {
    /// From the person's face vectors (any length; normalized here; vectors
    /// of another length than the first are skipped). `None` without any.
    pub fn of(name: &str, vectors: &[Vec<f32>]) -> Option<Self> {
        let units: Vec<Vec<f32>> = vectors
            .iter()
            .filter_map(|v| embedding::normalized(v))
            .collect();
        let dim = units.first()?.len();
        let mut sum = vec![0.0f32; dim];
        let mut count = 0;
        for u in units.iter().filter(|u| u.len() == dim) {
            for (a, b) in sum.iter_mut().zip(u) {
                *a += b;
            }
            count += 1;
        }
        Some(Self {
            name: name.to_owned(),
            sum,
            count,
        })
    }

    /// Mean similarity of `face` (unit length) to this person's faces.
    pub fn similarity(&self, face: &[f32]) -> f32 {
        if self.count == 0 || face.len() != self.sum.len() {
            return f32::NEG_INFINITY;
        }
        embedding::dot(face, &self.sum) / self.count as f32
    }
}

// ------------------------------------------------------------- evaluation ---

/// A place resolved against the library: which sessions, folder names,
/// keywords and location values carry it.
struct ResolvedPlace {
    term: String,
    sessions: HashMap<i64, String>,
    names: HashMap<u32, String>,
    keywords: HashMap<u32, String>,
    locations: HashMap<u32, String>,
    vector: Option<Vec<f32>>,
}

impl ResolvedPlace {
    fn resolved(&self) -> bool {
        !(self.sessions.is_empty()
            && self.names.is_empty()
            && self.keywords.is_empty()
            && self.locations.is_empty())
    }

    /// How `item` matches, most specific source first.
    fn source(&self, c: &FindCache, item: &Item) -> Option<PlaceSource> {
        if let Some(v) = item.location.iter().find_map(|l| self.locations.get(l)) {
            return Some(PlaceSource::Location(v.clone()));
        }
        if let Some(n) = self.sessions.get(&item.session) {
            return Some(PlaceSource::SessionName(n.clone()));
        }
        if let Some(n) = c.folders[item.folder as usize]
            .iter()
            .find_map(|n| self.names.get(n))
        {
            return Some(PlaceSource::Folder(n.clone()));
        }
        item.keywords
            .iter()
            .find_map(|k| self.keywords.get(k))
            .map(|k| PlaceSource::Keyword(k.clone()))
    }
}

fn resolve_place(c: &FindCache, term: &str, vector: Option<&Vec<f32>>) -> ResolvedPlace {
    let t = fold(term);
    ResolvedPlace {
        sessions: c
            .sessions
            .iter()
            .filter(|(_, (_, name))| has_phrase(&fold(name), &t))
            .map(|(id, (_, name))| (*id, name.clone()))
            .collect(),
        names: c.names.matching(&t),
        keywords: c.keywords.matching(&t),
        locations: c.locations.matching(&t),
        vector: vector.and_then(|v| embedding::normalized(v)),
        term: t,
    }
}

/// Bonus for a confirmed (not just suggested) person, and for an unknown
/// name found in keywords or folders: enough to order ties, never to
/// outrank a clearly better visual match.
const CONFIRMED_BONUS: f32 = 0.02;
const TEXT_BONUS: f32 = 0.03;
/// Bonus for the description in a title or caption: the user wrote it, so it
/// ranks above what only looks like it.
const TITLE_BONUS: f32 = 0.1;

struct Scored {
    item: usize,
    /// The score that selects (the text, else the soft places).
    primary: Option<f32>,
    /// The score that orders.
    score: f32,
    /// The description is in its title or caption: kept whatever its visual score.
    titled: bool,
    reasons: Vec<MatchReason>,
}

/// A spec resolved against one cache.
struct Prepared<'a> {
    c: &'a FindCache,
    matrix: &'a embedding::EmbeddingMatrix,
    spec: &'a FindSpec,
    /// Per person term: the people-vocabulary ids it names.
    people: Vec<HashSet<u32>>,
    /// Unknown person terms (folded) and the folder names / keywords containing them.
    unknown: Vec<(String, HashSet<u32>, HashSet<u32>)>,
    hard_places: Vec<&'a ResolvedPlace>,
    soft_places: Vec<&'a ResolvedPlace>,
    text: Option<Vec<f32>>,
    /// Titles and captions containing the description (whole words, folded).
    titles: HashMap<u32, String>,
}

impl<'a> Prepared<'a> {
    fn new(
        c: &'a FindCache,
        matrix: &'a embedding::EmbeddingMatrix,
        spec: &'a FindSpec,
        places: &'a [ResolvedPlace],
        out: &mut FindOutcome,
    ) -> Self {
        let (hard_places, soft): (Vec<&ResolvedPlace>, Vec<&ResolvedPlace>) =
            places.iter().partition(|p| p.resolved());
        let dim_ok = |v: &Option<Vec<f32>>| v.as_ref().is_some_and(|v| v.len() == matrix.dim());
        // A place vector of the wrong size (another model) is ignored.
        let soft_places = soft.into_iter().filter(|p| dim_ok(&p.vector)).collect();
        let mut people = Vec::new();
        let mut unknown = Vec::new();
        for p in &spec.people {
            let names: HashSet<String> = p.names.iter().map(|n| fold(n)).collect();
            if names.is_empty() {
                let t = fold(&p.term);
                let folders = c.names.matching(&t).into_keys().collect();
                let keywords = c.keywords.matching(&t).into_keys().collect();
                unknown.push((t, folders, keywords));
                out.unknown_people.push(p.term.clone());
            } else {
                people.push(c.people.equal_to(&names));
            }
        }
        let text = spec
            .text_vector
            .as_ref()
            .and_then(|v| embedding::normalized(v));
        Self {
            c,
            matrix,
            spec,
            people,
            unknown,
            hard_places,
            soft_places,
            text: Some(text).filter(dim_ok).flatten(),
            titles: c.titles.matching(&fold(&spec.text)),
        }
    }

    /// Whether the date, rating and flag let `item` through.
    fn passes(&self, item: &Item) -> bool {
        let s = self.spec;
        s.date.is_none_or(|d| d.matches(item.capture_ms))
            && s.min_rating.is_none_or(|m| item.rating >= m)
            && s.max_rating.is_none_or(|m| item.rating <= m)
            && s.pick.is_none_or(|p| item.pick == p)
    }

    /// `item` scored with why it matched, or `None` when a hard filter fails.
    fn score(&self, i: usize, item: &Item) -> Option<Scored> {
        if !self.passes(item) {
            return None;
        }
        let c = self.c;
        let mut reasons = Vec::new();
        let mut bonus = 0.0;
        for ids in &self.people {
            let person = |list: &[u32]| list.iter().find(|id| ids.contains(id)).copied();
            let (id, suggested) = match person(&item.people) {
                Some(id) => (id, false),
                None => (person(&item.suggested)?, true),
            };
            if !suggested {
                bonus += CONFIRMED_BONUS;
            }
            reasons.push(MatchReason::Person {
                name: c.people.items[id as usize].0.clone(),
                suggested,
            });
        }
        for p in &self.hard_places {
            reasons.push(MatchReason::Place {
                term: p.term.clone(),
                source: p.source(c, item)?,
            });
        }
        let titled = item.titles.iter().find_map(|t| self.titles.get(t));
        if let Some(t) = titled {
            reasons.push(MatchReason::Title { text: t.clone() });
            bonus += TITLE_BONUS;
        }
        let row = item.row.map(|r| self.matrix.row(r));
        let primary = match (&self.text, row) {
            (Some(q), Some(row)) => {
                let s = embedding::dot(row, q);
                reasons.push(MatchReason::Visual { score: s });
                Some(s)
            }
            // Without its vector a photo can't be judged on the description,
            // unless its title or caption says it.
            (Some(_), None) if titled.is_none() => return None,
            _ => None,
        };
        let structured = !self.hard_places.is_empty() || !self.people.is_empty();
        let soft = if self.soft_places.is_empty() {
            None
        } else if let Some(row) = row {
            for p in &self.soft_places {
                reasons.push(MatchReason::Place {
                    term: p.term.clone(),
                    source: PlaceSource::LooksLike,
                });
            }
            let sum: f32 = self
                .soft_places
                .iter()
                .map(|p| embedding::dot(row, p.vector.as_deref().unwrap_or_default()))
                .sum();
            Some(sum / self.soft_places.len() as f32)
        } else if primary.is_none() && !structured {
            // Only "looks like" was asked, and it can't be judged.
            return None;
        } else {
            // Kept on its other matches, without the boost.
            None
        };
        for (term, folders, keywords) in &self.unknown {
            let in_folder = c.folders[item.folder as usize]
                .iter()
                .any(|n| folders.contains(n));
            if in_folder || item.keywords.iter().any(|k| keywords.contains(k)) {
                reasons.push(MatchReason::Text { term: term.clone() });
                bonus += TEXT_BONUS;
            }
        }
        let s = self.spec;
        if s.date.is_some() {
            reasons.push(MatchReason::Date);
        }
        if s.min_rating.is_some() || s.max_rating.is_some() {
            reasons.push(MatchReason::Rating(item.rating));
        }
        if let Some(p) = s.pick {
            reasons.push(MatchReason::Flag(p));
        }
        let (primary, score) = match (primary, soft) {
            (Some(t), Some(p)) => (Some(t), t + s.options.place_weight * p),
            (Some(t), None) => (Some(t), t),
            (None, Some(p)) if !structured => (Some(p), p),
            (None, Some(p)) => (None, p),
            (None, None) => (None, 0.0),
        };
        Some(Scored {
            item: i,
            primary,
            score: score + bonus,
            titled: titled.is_some(),
            reasons,
        })
    }
}

fn evaluate(c: &FindCache, matrix: &embedding::EmbeddingMatrix, spec: &FindSpec) -> FindOutcome {
    let mut out = FindOutcome::default();
    let places: Vec<ResolvedPlace> = spec
        .places
        .iter()
        .map(|p| resolve_place(c, p, spec.place_vectors.get(p)))
        .collect();
    out.places = places
        .iter()
        .map(|p| PlaceResolution {
            term: p.term.clone(),
            sources: sources_of(p),
        })
        .collect();
    let q = Prepared::new(c, matrix, spec, &places, &mut out);
    let mut scored: Vec<Scored> = c
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| q.score(i, item))
        .collect();
    // Primary-scored queries keep what clears the floor and the window.
    let o = spec.options;
    scored.retain(|s| s.titled || s.primary.is_none_or(|p| p >= o.min_score));
    if let Some(best) = scored.iter().filter_map(|s| s.primary).reduce(f32::max) {
        scored.retain(|s| s.titled || s.primary.is_none_or(|p| p >= best - o.window));
    }
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| c.items[b.item].capture_ms.cmp(&c.items[a.item].capture_ms))
            .then_with(|| a.item.cmp(&b.item))
    });
    out.total = scored.len();
    // Sessions: named after a place (or the description), or holding many hits.
    let mut per_session: HashMap<i64, (usize, f32)> = HashMap::new();
    for s in &scored {
        let e = per_session
            .entry(c.items[s.item].session)
            .or_insert((0, f32::NEG_INFINITY));
        e.0 += 1;
        e.1 = e.1.max(s.score);
    }
    out.sessions = session_matches(c, &places, &spec.text, &per_session);
    out.photos = scored
        .into_iter()
        .take(o.limit)
        .filter_map(|s| {
            let item = &c.items[s.item];
            let (root, name) = c.sessions.get(&item.session)?;
            Some(FindPhoto {
                session_root: root.clone(),
                session_name: name.clone(),
                path: item.path.clone(),
                score: s.score,
                capture_ms: item.capture_ms,
                reasons: s.reasons,
            })
        })
        .collect();
    out
}

/// What a place matched in the library, for the query summary.
fn sources_of(p: &ResolvedPlace) -> Vec<PlaceSource> {
    let mut out: Vec<PlaceSource> = Vec::new();
    let mut sorted = |v: Vec<String>, f: fn(String) -> PlaceSource| {
        let mut v = v;
        v.sort();
        v.dedup();
        out.extend(v.into_iter().map(f));
    };
    sorted(
        p.locations.values().cloned().collect(),
        PlaceSource::Location,
    );
    sorted(
        p.sessions.values().cloned().collect(),
        PlaceSource::SessionName,
    );
    sorted(p.names.values().cloned().collect(), PlaceSource::Folder);
    sorted(p.keywords.values().cloned().collect(), PlaceSource::Keyword);
    if !p.resolved() && p.vector.is_some() {
        out.push(PlaceSource::LooksLike);
    }
    out
}

/// At most this many sessions listed for holding many hits.
const MANY_HITS_SESSIONS: usize = 8;
/// A session "holds many hits" with at least this many.
const MANY_HITS_MIN: usize = 3;

fn session_matches(
    c: &FindCache,
    places: &[ResolvedPlace],
    text: &str,
    hits: &HashMap<i64, (usize, f32)>,
) -> Vec<SessionMatch> {
    let mut out: Vec<SessionMatch> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut add = |id: i64, reason: SessionReason, out: &mut Vec<SessionMatch>| {
        if !seen.insert(id) {
            return;
        }
        if let Some((root, name)) = c.sessions.get(&id) {
            let (n, best) = hits.get(&id).copied().unwrap_or((0, 0.0));
            out.push(SessionMatch {
                root: root.clone(),
                name: name.clone(),
                reason,
                hits: n,
                best_score: best,
            });
        }
    };
    let mut ids: Vec<i64> = c.sessions.keys().copied().collect();
    ids.sort_unstable();
    let text = fold(text);
    for p in places {
        for &id in &ids {
            if p.sessions.contains_key(&id) {
                add(
                    id,
                    SessionReason::Name {
                        term: p.term.clone(),
                    },
                    &mut out,
                );
            }
        }
    }
    for &id in &ids {
        if !text.is_empty()
            && c.sessions
                .get(&id)
                .is_some_and(|(_, n)| has_phrase(&fold(n), &text))
        {
            add(id, SessionReason::Name { term: text.clone() }, &mut out);
        }
    }
    // Folders and keywords: the sessions whose photos carry them.
    for p in places {
        let mut by_session: Vec<(i64, SessionReason)> = Vec::new();
        for item in &c.items {
            if by_session.iter().any(|(s, _)| *s == item.session) {
                continue;
            }
            if let Some(n) = c.folders[item.folder as usize]
                .iter()
                .find_map(|n| p.names.get(n))
            {
                by_session.push((
                    item.session,
                    SessionReason::Folder {
                        term: p.term.clone(),
                        folder: n.clone(),
                    },
                ));
            } else if let Some(k) = item.keywords.iter().find_map(|k| p.keywords.get(k)) {
                by_session.push((
                    item.session,
                    SessionReason::Keyword {
                        term: p.term.clone(),
                        keyword: k.clone(),
                    },
                ));
            }
        }
        by_session.sort_by_key(|(s, _)| *s);
        for (id, reason) in by_session {
            add(id, reason, &mut out);
        }
    }
    let mut busy: Vec<(i64, usize, f32)> = hits
        .iter()
        .filter(|(_, (n, _))| *n >= MANY_HITS_MIN)
        .map(|(id, (n, b))| (*id, *n, *b))
        .collect();
    busy.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)).then(a.0.cmp(&b.0)));
    for (id, _, _) in busy.into_iter().take(MANY_HITS_SESSIONS) {
        add(id, SessionReason::ManyHits, &mut out);
    }
    out
}
