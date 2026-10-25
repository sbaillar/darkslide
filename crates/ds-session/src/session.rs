//! The session: a self-contained folder of originals, sidecars, and an index.
//!
//! ```text
//! <root>/
//!   <name>.dsession            SQLite index (WAL), rebuildable from sidecars
//!   Capture/ Selects/ Trash/   originals; each dir has .ds/settings/<file>.json sidecars
//!   Output/                    exports
//!   .ds/settings/              session.json (settings, auto-apply preset), albums.json
//!   .ds/cache/                 thumbnails/previews (ds-cache), disposable
//! ```
//!
//! Every mutation writes the sidecar first (atomically), then the index.
//! If the process dies in between, [`Session::open`] notices the sidecar's
//! changed mtime/size and re-indexes it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ds_core::{
    Adjustments, ColorTag, FileHash, GeoPoint, IptcMeta, PickState, Rating, SIDECAR_SCHEMA_VERSION,
};
use rusqlite::{Connection, MAIN_DB, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::album::{Album, read_albums_or_quarantine};
use crate::atomic::{self, WriteHook};
use crate::db::{self, Recovery};
use crate::error::{IoContext, Result, SessionError, json_err};
use crate::filter::{pick_code, pick_from_code, tag_code, tag_from_code};
use crate::preset::Preset;
use crate::relpath::{
    Folder, RelPath, ensure_no_symlinks, original_path, rel_string, resolve_sidecar,
};
use crate::sidecar::{ImageMetaLite, Loaded, Sidecar};
use crate::undo::{UndoConfig, UndoManager};

/// What undo/redo restores for a variant: its adjustments and organization.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UndoState {
    pub adjustments: Adjustments,
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    pub cull_note: Option<String>,
    pub keywords: Vec<String>,
    pub iptc: IptcMeta,
    pub geotag: Option<GeoPoint>,
    /// A geotag the file had that couldn't be read: undo writes it back as it was.
    pub geotag_unreadable: Option<Value>,
}

impl UndoState {
    pub fn of(sc: &Sidecar) -> Self {
        Self {
            adjustments: sc.adjustments.clone(),
            rating: sc.rating,
            color_tag: sc.color_tag,
            pick: sc.pick,
            cull_note: sc.cull_note.clone(),
            keywords: sc.keywords.clone(),
            iptc: sc.iptc.clone(),
            geotag: sc.geotag,
            geotag_unreadable: sc.geotag_unreadable.clone(),
        }
    }

    pub fn restore(self, sc: &mut Sidecar) {
        sc.adjustments = self.adjustments;
        sc.rating = self.rating;
        sc.color_tag = self.color_tag;
        sc.pick = self.pick;
        sc.cull_note = self.cull_note;
        sc.keywords = self.keywords;
        sc.iptc = self.iptc;
        sc.geotag = self.geotag;
        sc.geotag_unreadable = self.geotag_unreadable;
    }
}

/// Identity of an original's bytes on disk as last verified: when any of
/// these change, the content must be re-hashed before trusting the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileStat {
    pub size: u64,
    pub mtime: i64,
    pub ino: u64,
}

impl FileStat {
    pub fn of(md: &fs::Metadata) -> Self {
        Self {
            size: md.len(),
            mtime: mtime_ns(md),
            ino: md.ino(),
        }
    }
}

/// Identifies an original within a session. Stable across index rebuilds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImageId(pub i64);

/// Identifies a variant (an original plus one adjustment stack). Stable across
/// index rebuilds and never reused, so it is safe as a cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VariantId(pub i64);

#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub id: ImageId,
    pub rel_path: RelPath,
    pub hash: FileHash,
    pub file_size: u64,
    pub meta: ImageMetaLite,
    /// The original was not found at `rel_path` during the last sync/rescan.
    pub missing: bool,
    /// Hash of the sensor data only (see [`Sidecar::content_hash`]).
    pub content_hash: Option<FileHash>,
    /// The file at this path replaced a different photo ("file changed");
    /// that photo's metadata can be restored ([`Session::restore_replaced`]).
    pub file_changed: bool,
}

impl ImageInfo {
    pub fn folder(&self) -> Folder {
        self.rel_path.folder()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub id: VariantId,
    pub image: ImageId,
    pub ordinal: u32,
    pub name: String,
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    /// Why AI Cull rejected it (sidecar `cull_note`); only while still rejected.
    pub cull_note: Option<String>,
    pub keywords: Vec<String>,
    /// Title, caption, credits, location and rights (sidecar).
    pub iptc: IptcMeta,
    /// People tagged on it (sidecar), in order.
    pub people: Vec<String>,
    /// Where it was taken, set by hand on the map (sidecar `geotag`).
    pub geotag: Option<GeoPoint>,
    /// Adjustments differ from the defaults.
    pub edited: bool,
    /// The sidecar is from a newer build and cannot be modified here.
    pub read_only: bool,
}

/// What an index sync or rebuild did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncReport {
    pub indexed: usize,
    pub removed: usize,
    /// Sidecars that could not be indexed: (relative path, reason).
    pub errors: Vec<(String, String)>,
    /// Why the index was recreated on open, if it was.
    pub recovery: Option<Recovery>,
}

/// Where a session's originals live.
///
/// `Internal`: under the session root (`Capture/`, `Selects/`, `Trash/`), the
/// normal layout for imported shoots. `External(dir)`: the originals are the
/// user's own files in `dir` ("open folder as session"); `Capture/<rel>`
/// resolves to `dir/<rel>`, sidecars and the index stay under the session root,
/// and nothing ever moves, renames, trashes or writes those files.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
pub enum Originals {
    #[default]
    Internal,
    External(PathBuf),
}

/// Hash of an original registered by stat and not hashed yet (external
/// sessions hash lazily; see [`Session::unhashed_images`]).
pub const UNHASHED: FileHash = FileHash([0; 32]);

/// Most photos (RAW, JPEG/HEIF, video; sidecars and other files don't count)
/// an external walk lists before stopping. Past it nothing is marked missing.
#[cfg(not(test))]
pub const MAX_EXTERNAL_FILES: usize = 250_000;
/// Small in this crate's tests so the cap is cheap to reach.
#[cfg(test)]
pub const MAX_EXTERNAL_FILES: usize = 20;
/// Hard cap on every file an external walk records, photo or not, so a
/// hostile folder of junk cannot grow the scan without bound.
const MAX_EXTERNAL_ENTRIES: usize = 4 * MAX_EXTERNAL_FILES;
/// Sidecars written per journal and transaction in a batch edit.
#[cfg(not(test))]
const BATCH_CHUNK: usize = 2_000;
#[cfg(test)]
const BATCH_CHUNK: usize = 50;

/// Deepest sub-folder an external walk descends into.
pub const MAX_EXTERNAL_DEPTH: usize = 8;

/// The folder (at the top of the archived folder: an imported session's root, or
/// a folder opened in place) holding developed copies for photo servers. Never
/// photos of the session: an imported session only has photos under `Capture/`,
/// `Selects/` and `Trash/`, and the walk of a folder opened in place skips it once
/// it holds [`DEVELOPED_MARKER`].
pub const DEVELOPED_DIR: &str = "Developed";

/// File inside [`DEVELOPED_DIR`] that marks it as Darkslide's (a user's own
/// `Developed` folder without it stays part of the session).
pub const DEVELOPED_MARKER: &str = ".darkslide-developed";

/// The top-level folder of a walk that holds Darkslide's developed copies.
fn is_developed_dir(dir: &Path, name: &str, depth: usize) -> bool {
    depth == 0 && name == DEVELOPED_DIR && dir.join(DEVELOPED_MARKER).is_file()
}

/// Session-level settings stored in `.ds/settings/session.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionSettings {
    #[serde(default)]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub auto_apply: Option<Preset>,
    #[serde(default)]
    pub originals: Originals,
    /// Which folder an external session belongs to (the app's volume
    /// identity plus path), so a store is never silently taken over by
    /// another folder that maps to the same place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_identity: Option<String>,
}

/// Variants written with their state before the write, plus the first
/// write error (earlier writes stay applied).
pub(crate) type Collected = (Vec<(VariantId, UndoState)>, Option<SessionError>);

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A sidecar found during sync: (relative path, mtime, size, parse result).
type Parsed = (String, i64, i64, std::result::Result<Loaded, String>);

/// Preference order among sidecars claiming the same ids (lower wins):
/// (original missing, content mismatch, sidecar birth time, path length, path).
type RankKey = (bool, bool, u128, usize, String);

/// An open session.
pub struct Session {
    pub(crate) root: PathBuf,
    pub(crate) settings: SessionSettings,
    pub(crate) conn: Connection,
    pub(crate) undo: UndoManager<UndoState>,
    pub(crate) albums: Vec<Album>,
    pub(crate) clock: Clock,
    pub(crate) write_hook: Option<WriteHook>,
    last_sync: SyncReport,
    /// Batches journaled but not applied at open (finished by roll-forward).
    pub(crate) journal_replayed: usize,
    /// Search vectors in memory (`embed.rs`), dropped on every write.
    pub(crate) embed_cache: crate::embed::Cache,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("root", &self.root)
            .field("name", &self.settings.name)
            .finish_non_exhaustive()
    }
}

pub(crate) const SETTINGS_DIR: &str = ".ds/settings";
pub(crate) const CACHE_DIR: &str = ".ds/cache";
/// Where deleted variants' sidecars wait for "empty trash".
pub(crate) const DELETED_DIR: &str = "Trash/.ds/deleted";
const SESSION_FILE: &str = "session.json";
pub(crate) const DB_EXT: &str = "dsession";
/// Temp files younger than this may belong to a live writer and are left alone.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60);

fn validate_name(name: &str) -> Result<()> {
    let bad = name.trim().is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\0', ':'])
        || name.len() > 200;
    if bad {
        Err(SessionError::InvalidName(name.to_owned()))
    } else {
        Ok(())
    }
}

pub(crate) fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// Lower-cased text the `text` filter searches, besides the file name.
pub(crate) fn search_text(sc: &Sidecar) -> String {
    let mut parts: Vec<&str> = vec![&sc.name];
    parts.extend(sc.keywords.iter().map(String::as_str));
    parts.extend([sc.iptc.title.as_str(), sc.iptc.caption.as_str()]);
    parts.extend(sc.people.iter().map(|p| p.name.as_str()));
    parts.extend(sc.meta.camera.as_deref());
    parts.extend(sc.meta.lens.as_deref());
    parts.join("\n").to_lowercase()
}

impl Session {
    // ------------------------------------------------------------ lifecycle ---

    /// Create a new session folder at `dir` (created if needed; must be empty).
    pub fn create(dir: &Path, name: &str) -> Result<Session> {
        validate_name(name)?;
        if dir.exists() && fs::read_dir(dir).at(dir)?.next().is_some() {
            return Err(SessionError::AlreadyExists(dir.to_owned()));
        }
        for sub in [
            "Capture",
            "Selects",
            "Output",
            "Trash",
            SETTINGS_DIR,
            CACHE_DIR,
        ] {
            fs::create_dir_all(dir.join(sub)).at(dir.join(sub))?;
        }
        let settings = SessionSettings {
            schema_version: SIDECAR_SCHEMA_VERSION,
            name: name.to_owned(),
            auto_apply: None,
            originals: Originals::Internal,
            folder_identity: None,
        };
        write_settings(dir, &settings)?;
        Self::open(dir)
    }

    /// Create a session at `dir` whose originals are the files in `originals`
    /// (see [`Originals::External`]). `dir` must not exist or be empty; nothing
    /// is written under `originals`.
    pub fn create_external(dir: &Path, name: &str, originals: &Path) -> Result<Session> {
        validate_name(name)?;
        if !originals.is_dir() {
            return Err(SessionError::OriginalMissing(
                originals.display().to_string(),
            ));
        }
        if dir.exists() && fs::read_dir(dir).at(dir)?.next().is_some() {
            return Err(SessionError::AlreadyExists(dir.to_owned()));
        }
        for sub in [
            "Capture",
            "Selects",
            "Output",
            "Trash",
            SETTINGS_DIR,
            CACHE_DIR,
        ] {
            fs::create_dir_all(dir.join(sub)).at(dir.join(sub))?;
        }
        let settings = SessionSettings {
            schema_version: SIDECAR_SCHEMA_VERSION,
            name: name.to_owned(),
            auto_apply: None,
            originals: Originals::External(originals.to_owned()),
            folder_identity: None,
        };
        write_settings(dir, &settings)?;
        Self::open(dir)
    }

    /// Where the originals are.
    pub fn originals(&self) -> &Originals {
        &self.settings.originals
    }

    /// The external originals folder, if this session has one.
    pub fn external_root(&self) -> Option<&Path> {
        match &self.settings.originals {
            Originals::External(p) => Some(p),
            Originals::Internal => None,
        }
    }

    pub fn is_external(&self) -> bool {
        self.external_root().is_some()
    }

    /// Absolute path of an original (or folder) given its session-relative path.
    pub fn original_path(&self, rel: &str) -> PathBuf {
        original_path(&self.root, self.external_root(), rel)
    }

    /// Point an external session at its folder's new location (the folder was
    /// renamed or moved; sidecars and ids are unchanged).
    pub fn set_external_root(&mut self, originals: &Path) -> Result<()> {
        if !self.is_external() {
            return Err(SessionError::ExternalOnly("set_external_root"));
        }
        let tx = self.write_lock()?;
        let mut next = read_or_recover_settings(&self.root)?;
        next.originals = Originals::External(originals.to_owned());
        write_settings(&self.root, &next)?;
        tx.commit()?;
        self.settings = next;
        Ok(())
    }

    /// The identity of the folder an external session belongs to (see
    /// [`Self::set_folder_identity`]).
    pub fn folder_identity(&self) -> Option<&str> {
        self.settings.folder_identity.as_deref()
    }

    /// Record which folder this session belongs to (an opaque string chosen
    /// by the app: volume identity plus path).
    pub fn set_folder_identity(&mut self, identity: &str) -> Result<()> {
        let tx = self.write_lock()?;
        let mut next = read_or_recover_settings(&self.root)?;
        next.folder_identity = Some(identity.to_owned());
        write_settings(&self.root, &next)?;
        tx.commit()?;
        self.settings = next;
        Ok(())
    }

    /// Refuse an operation that would move, rename, trash or delete originals
    /// of an external session (the user's own files).
    pub(crate) fn refuse_external(&self, op: &'static str) -> Result<()> {
        if self.is_external() {
            Err(SessionError::ExternalOriginals(op))
        } else {
            Ok(())
        }
    }

    /// Open an existing session, validating the index and re-syncing it with
    /// the sidecars (full rebuild if it is missing, corrupt, or outdated).
    pub fn open(dir: &Path) -> Result<Session> {
        let settings = read_or_recover_settings(dir)?;
        for sub in ["Capture", "Selects", "Output", "Trash", CACHE_DIR] {
            fs::create_dir_all(dir.join(sub)).at(dir.join(sub))?;
        }
        atomic::remove_stale_temps(&dir.join(SETTINGS_DIR), STALE_TEMP_AGE);
        let db_path = dir.join(format!("{}.{DB_EXT}", settings.name));
        let (conn, recovery) = db::open_or_recreate(&db_path)?;
        let mut s = Session {
            root: dir.to_owned(),
            settings,
            conn,
            undo: UndoManager::new(UndoConfig::default()),
            albums: Vec::new(),
            clock: Arc::new(Instant::now),
            write_hook: None,
            last_sync: SyncReport::default(),
            journal_replayed: 0,
            embed_cache: crate::embed::Cache::default(),
        };
        s.journal_replayed = crate::journal::replay(&s.root)?;
        let mut report = s.sync_index()?;
        report.recovery = recovery;
        s.last_sync = report;
        Ok(s)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> &str {
        &self.settings.name
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join(format!("{}.{DB_EXT}", self.settings.name))
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(CACHE_DIR)
    }

    /// What the index sync on [`Session::open`] found.
    pub fn open_report(&self) -> &SyncReport {
        &self.last_sync
    }

    /// Replace the undo configuration (drops existing history).
    pub fn set_undo_config(&mut self, cfg: UndoConfig) {
        self.undo = UndoManager::new(cfg);
    }

    /// Inject a clock (tests use it to drive undo coalescing deterministically).
    pub fn set_clock(&mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) {
        self.clock = Arc::new(clock);
    }

    #[cfg(test)]
    pub(crate) fn set_write_hook(&mut self, hook: Option<WriteHook>) {
        self.write_hook = hook;
    }

    /// Consistent copy of the index via the SQLite online backup API (safe
    /// while the session is in use; the archive runs this before rsync).
    pub fn snapshot_to(&self, path: &Path) -> Result<()> {
        snapshot(&self.conn, path)
    }

    // -------------------------------------------------------------- settings ---

    pub fn auto_apply_preset(&self) -> Option<&Preset> {
        self.settings.auto_apply.as_ref()
    }

    /// Preset applied to every original added from now on (e.g. usual WB/lens/sharpening).
    pub fn set_auto_apply_preset(&mut self, preset: Option<Preset>) -> Result<()> {
        // Re-read under the write lock and apply only our change: another
        // handle may have changed settings since we loaded ours, and setting
        // the value we already saw is not a change.
        let tx = self.write_lock()?;
        let mut next = read_or_recover_settings(&self.root)?;
        if preset != self.settings.auto_apply {
            next.auto_apply = preset;
            write_settings(&self.root, &next)?;
        }
        tx.commit()?;
        self.settings = next;
        Ok(())
    }

    // ------------------------------------------------------------ index sync ---

    /// Discard the index contents and re-read every sidecar.
    pub fn rebuild_index(&mut self) -> Result<SyncReport> {
        self.sync(true)
    }

    /// Bring the index in line with the sidecars on disk: new or changed
    /// sidecars (by mtime/size) are (re)indexed, vanished ones removed. Also
    /// reloads settings and albums written by other handles.
    pub fn sync_index(&mut self) -> Result<SyncReport> {
        self.sync(false)
    }

    fn sync(&mut self, full: bool) -> Result<SyncReport> {
        // The write lock is taken before scanning so another Session on the
        // same folder cannot change sidecars between the scan and the update.
        let tx = self.write_lock()?;
        if full {
            tx.execute_batch("DELETE FROM variants; DELETE FROM images;")?;
        }
        let disk = self.scan_disk(false)?;
        let mut report = SyncReport::default();
        let known: HashMap<String, (i64, i64)> = {
            let mut st = tx.prepare("SELECT sidecar, sidecar_mtime, sidecar_size FROM variants")?;
            st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
                .collect::<rusqlite::Result<_>>()?
        };
        // Deletions first so moved sidecars don't collide with their old rows.
        for gone in known.keys().filter(|k| !disk.sidecars.contains_key(*k)) {
            report.removed += tx.execute("DELETE FROM variants WHERE sidecar = ?", [gone])?;
        }
        tx.execute(
            "DELETE FROM images WHERE id NOT IN (SELECT image_id FROM variants)",
            [],
        )?;
        let parsed: Vec<Parsed> = disk
            .sidecars
            .iter()
            .filter(|(k, stat)| known.get(*k) != Some(stat))
            .map(|(rel, &(mtime, size))| {
                let res = atomic::read_json_file(&self.root.join(rel))
                    .map_err(|e| e.to_string())
                    .and_then(|b| Sidecar::from_json(&b).map_err(|e| e.to_string()));
                (rel.clone(), mtime, size, res)
            })
            .collect();
        for (rel, mtime, size, res) in self.order_duplicates(parsed) {
            let res = res.and_then(|l| {
                index_sidecar(&tx, &self.root, self.external_root(), &rel, &l, mtime, size)
                    .map_err(|e| e.to_string())
            });
            match res {
                Ok(()) => report.indexed += 1,
                Err(e) => report.errors.push((rel, e)),
            }
        }
        let (albums, quarantined) = read_albums_or_quarantine(&self.root)?;
        if let Some(why) = quarantined {
            report
                .errors
                .push((format!("{SETTINGS_DIR}/albums.json"), why));
        }
        crate::album::mirror_albums(&tx, &albums)?;
        if let Some(why) = crate::faces::sync_face_links(&tx, &self.root)? {
            report
                .errors
                .push((format!("{SETTINGS_DIR}/face_links.json"), why));
        }
        let settings = read_or_recover_settings(&self.root)?;
        tx.commit()?;
        self.albums = albums;
        self.settings.auto_apply = settings.auto_apply;
        Ok(report)
    }

    /// When several sidecars claim the same variant or image id (a folder
    /// duplicated in Finder, `.ds` included), index the most credible one
    /// first so it wins: original present, original content matching the
    /// sidecar's hash, oldest sidecar, then the shortest path.
    fn order_duplicates(&self, parsed: Vec<Parsed>) -> Vec<Parsed> {
        let mut claims: HashMap<i64, usize> = HashMap::new();
        for (_, _, _, res) in &parsed {
            if let Ok(l) = res {
                *claims.entry(l.sidecar.variant_id).or_default() += 1;
                *claims.entry(-l.sidecar.image_id).or_default() += 1;
            }
        }
        let contested = |l: &Loaded| {
            claims.get(&l.sidecar.variant_id).copied().unwrap_or(0) > 1
                || claims.get(&-l.sidecar.image_id).copied().unwrap_or(0) > 1
        };
        let mut hashes: HashMap<PathBuf, Option<FileHash>> = HashMap::new();
        let mut rank = |rel: &str, l: &Loaded| -> (bool, bool, u128) {
            let original =
                resolve_sidecar(&self.root, self.external_root(), rel, l.sidecar.ordinal)
                    .map(|r| self.original_path(r.as_str()));
            let Some(original) = original.filter(|p| p.is_file()) else {
                return (true, true, 0);
            };
            let hash = *hashes
                .entry(original.clone())
                .or_insert_with(|| crate::files::hash_file(&original).ok());
            let created = fs::metadata(self.root.join(rel))
                .and_then(|m| m.created())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(u128::MAX, |d| d.as_nanos());
            (false, hash != Some(l.sidecar.hash), created)
        };
        let mut keyed: Vec<(RankKey, Parsed)> = parsed
            .into_iter()
            .map(|p| {
                let r = match &p.3 {
                    Ok(l) if contested(l) => rank(&p.0, l),
                    _ => (false, false, 0),
                };
                ((r.0, r.1, r.2, p.0.len(), p.0.clone()), p)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.into_iter().map(|(_, p)| p).collect()
    }

    /// Walk the image folders: sidecars (with mtime/size) and, if asked, originals.
    pub(crate) fn scan_disk(&self, originals: bool) -> Result<DiskScan> {
        let mut scan = DiskScan::default();
        let internal_originals = originals && !self.is_external();
        for f in Folder::ALL {
            walk(
                &self.root,
                &self.root.join(f.dir_name()),
                internal_originals,
                &mut scan,
            )?;
        }
        if let (true, Some(ext)) = (originals, self.external_root()) {
            walk_external(ext, ext, 0, is_capture_one_session(ext), &mut scan).at(ext)?;
        }
        Ok(scan)
    }

    /// Files under the external originals root (as `Capture/<rel>` paths with
    /// their stat), walked with the same limits as [`Session::rescan`]:
    /// hidden entries, `.ds`, and bundles are skipped, at most
    /// [`MAX_EXTERNAL_DEPTH`] levels and [`MAX_EXTERNAL_FILES`] files.
    /// The second value is the number of files seen (more than returned when truncated).
    pub fn external_files(&self) -> Result<(Vec<(String, u64)>, usize)> {
        let Some(ext) = self.external_root() else {
            return Ok((Vec::new(), 0));
        };
        let mut scan = DiskScan::default();
        walk_external(ext, ext, 0, is_capture_one_session(ext), &mut scan).at(ext)?;
        let mut v: Vec<(String, u64)> = scan
            .originals
            .into_iter()
            .map(|(k, st)| (k, st.size))
            .collect();
        v.sort();
        Ok((v, scan.seen))
    }

    // ------------------------------------------------------------ originals ---

    /// Register an original already copied into the session at `rel_path`
    /// (e.g. `Capture/DSC00412.ARW`). Creates its first variant, with the
    /// session's auto-apply preset applied.
    pub fn add_original(
        &mut self,
        rel_path: &str,
        hash: FileHash,
        meta: ImageMetaLite,
    ) -> Result<ImageId> {
        let ids = self.add_originals(vec![(rel_path.to_owned(), hash, meta)])?;
        Ok(ids[0])
    }

    /// Batch form of [`Session::add_original`]: validates everything first,
    /// writes the sidecars, then indexes them in one transaction.
    pub fn add_originals(
        &mut self,
        items: Vec<(String, FileHash, ImageMetaLite)>,
    ) -> Result<Vec<ImageId>> {
        self.add_new_originals(
            items
                .into_iter()
                .map(|(rel, hash, meta)| NewOriginal {
                    rel,
                    hash,
                    content_hash: None,
                    meta,
                    replaced: Vec::new(),
                    iptc: IptcMeta::default(),
                })
                .collect(),
        )
    }

    /// [`Session::add_originals`] with the content hash and "file changed"
    /// link of each new original.
    pub fn add_new_originals(&mut self, items: Vec<NewOriginal>) -> Result<Vec<ImageId>> {
        self.add_new_originals_with(items, true)
    }

    /// [`Self::add_new_originals`]; `auto_apply = false` leaves the session's
    /// auto-apply preset out (video clips, which are never developed).
    pub fn add_new_originals_with(
        &mut self,
        items: Vec<NewOriginal>,
        auto_apply: bool,
    ) -> Result<Vec<ImageId>> {
        let tx = self.write_lock()?;
        let mut seen = HashSet::new();
        let mut planned = Vec::with_capacity(items.len());
        for item in items {
            let NewOriginal {
                rel,
                hash,
                content_hash,
                meta,
                replaced,
                iptc,
            } = item;
            let rel = RelPath::new(&rel)?;
            if self.is_external() {
                if rel.folder() != Folder::Capture {
                    return Err(SessionError::ExternalOriginals("add outside Capture"));
                }
            } else {
                ensure_no_symlinks(&self.root, rel.as_str())?;
            }
            ensure_no_symlinks(&self.root, &rel.sidecar_rel(1))?;
            if !self.original_path(rel.as_str()).is_file() {
                return Err(SessionError::OriginalMissing(rel.as_str().to_owned()));
            }
            // Never overwrite an existing sidecar, even one the index lost track of.
            let sidecar_exists = self
                .root
                .join(rel.sidecar_rel(1))
                .symlink_metadata()
                .is_ok();
            if !seen.insert(rel.clone()) || sidecar_exists || self.image_id_at(&rel)?.is_some() {
                return Err(SessionError::DuplicatePath(rel.as_str().to_owned()));
            }
            planned.push((rel, hash, meta, content_hash, replaced, iptc));
        }
        let mut writes = Vec::with_capacity(planned.len());
        let originals: HashMap<String, RelPath> = planned
            .iter()
            .map(|(rel, ..)| (rel.sidecar_rel(1), rel.clone()))
            .collect();
        for (rel, hash, meta, content_hash, replaced, iptc) in planned {
            let adjustments = match self.settings.auto_apply.as_ref().filter(|_| auto_apply) {
                Some(p) => p.apply(&Adjustments::default(), meta.oriented_size()),
                None => Adjustments::default(),
            };
            let sc = Sidecar {
                schema_version: SIDECAR_SCHEMA_VERSION,
                variant_id: self.fresh_id("variants")?,
                image_id: self.fresh_id("images")?,
                ordinal: 1,
                name: String::new(),
                hash,
                meta,
                adjustments,
                rating: Rating::default(),
                color_tag: ColorTag::None,
                pick: PickState::Unflagged,
                cull_note: None,
                keywords: Vec::new(),
                content_hash,
                replaced,
                optimize_report: None,
                ai_changes: None,
                people: Vec::new(),
                people_rejected: Vec::new(),
                iptc: iptc.trimmed(),
                geotag: None,
                geotag_unreadable: None,
            };
            writes.push(Pending::new(rel.sidecar_rel(1), sc));
        }
        let (done, err) = self.write_sidecars(writes);
        let mut ids = Vec::with_capacity(done.len());
        for w in &done {
            index_written(&tx, &self.root, self.external_root(), w)?;
            // The importer verified this content (or, for lazily hashed
            // external originals, it is hashed later): trust the file as it is now.
            if let Some(md) = originals
                .get(&w.rel)
                .and_then(|r| fs::metadata(self.original_path(r.as_str())).ok())
            {
                record_stat(&tx, w.sidecar.image_id, FileStat::of(&md))?;
            }
            ids.push(ImageId(w.sidecar.image_id));
        }
        tx.commit()?;
        match err {
            Some(e) => Err(e),
            None => Ok(ids),
        }
    }

    /// Register a photo made from another one (AI Upscale's DNG), already in the
    /// session at `item.rel`. It starts with `adjustments` (the source's) instead of the
    /// auto-apply preset, without an undo step: it is a new photo, not an edit.
    pub fn add_derived(
        &mut self,
        item: NewOriginal,
        adjustments: &Adjustments,
    ) -> Result<VariantId> {
        let image = self.add_new_originals_with(vec![item], false)?[0];
        let v = self.variants(image)?[0];
        let (_, err) = self.modify_collect(&[v], |_, sc| {
            sc.adjustments.clone_from(adjustments);
            true
        })?;
        err.map_or(Ok(v), Err)
    }

    /// Whether `rel` names a photo of this session (an index row, its file present or
    /// not) or a sidecar left behind: a new file must not take that name.
    pub fn path_taken(&self, rel: &str) -> Result<bool> {
        let rel = RelPath::new(rel)?;
        Ok(self.image_id_at(&rel)?.is_some()
            || self
                .root
                .join(rel.sidecar_rel(1))
                .symlink_metadata()
                .is_ok())
    }

    fn image_id_at(&self, rel: &RelPath) -> Result<Option<ImageId>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id FROM images WHERE rel_path = ?")?;
        Ok(st
            .query_row([rel.as_str()], |r| r.get(0))
            .optional()?
            .map(ImageId))
    }

    /// A random positive id not yet used in `table`. Random (not max+1) so ids
    /// are never reused after deletions, even across index rebuilds.
    pub(crate) fn fresh_id(&self, table: &str) -> Result<i64> {
        loop {
            let id = random_id();
            let mut st = self
                .conn
                .prepare_cached(&format!("SELECT 1 FROM {table} WHERE id = ?"))?;
            if !st.exists([id])? {
                return Ok(id);
            }
        }
    }

    pub fn image(&self, id: ImageId) -> Result<ImageInfo> {
        let mut st = self
            .conn
            .prepare_cached(&format!("{IMAGE_SELECT} WHERE id = ?"))?;
        st.query_row([id.0], image_from_row)
            .optional()?
            .ok_or(SessionError::NotFound {
                kind: "image",
                id: id.0,
            })
    }

    /// Every original, sorted by path.
    pub fn images(&self) -> Result<Vec<ImageInfo>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("{IMAGE_SELECT} ORDER BY rel_path"))?;
        Ok(st
            .query_map([], image_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Up to `limit` originals after path `after` (all of them when `None`),
    /// sorted by path: one page of [`Session::images`].
    pub fn images_after(&self, after: Option<&RelPath>, limit: usize) -> Result<Vec<ImageInfo>> {
        let mut st = self.conn.prepare_cached(&format!(
            "{IMAGE_SELECT} WHERE rel_path > ?1 ORDER BY rel_path LIMIT ?2"
        ))?;
        let after = after.map_or("", RelPath::as_str);
        Ok(st
            .query_map(params![after, limit as i64], image_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The original at `rel`, by the unique path index (no table scan).
    pub fn image_by_rel(&self, rel: &RelPath) -> Result<Option<ImageInfo>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("{IMAGE_SELECT} WHERE rel_path = ?"))?;
        Ok(st.query_row([rel.as_str()], image_from_row).optional()?)
    }

    /// The first original (by path) with this content hash, by index.
    pub fn image_by_hash(&self, hash: &FileHash) -> Result<Option<ImageInfo>> {
        let mut st = self.conn.prepare_cached(&format!(
            "{IMAGE_SELECT} WHERE hash = ? ORDER BY rel_path LIMIT 1"
        ))?;
        Ok(st.query_row([&hash.0[..]], image_from_row).optional()?)
    }

    pub fn image_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    // -------------------------------------------------------------- variants ---

    /// Variants of `image`, in creation order.
    pub fn variants(&self, image: ImageId) -> Result<Vec<VariantId>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id FROM variants WHERE image_id = ? ORDER BY ordinal")?;
        let ids: Vec<VariantId> = st
            .query_map([image.0], |r| r.get(0).map(VariantId))?
            .collect::<rusqlite::Result<_>>()?;
        if ids.is_empty() {
            self.image(image)?;
        }
        Ok(ids)
    }

    pub fn variant(&self, v: VariantId) -> Result<VariantInfo> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, image_id, ordinal, name, rating, color_tag, pick, keywords, edited, read_only, iptc,
                geo_lat, geo_lon, geo_alt, cull_note
             FROM variants WHERE id = ?",
        )?;
        let row = st
            .query_row([v.0], |r| {
                let kw: String = r.get(7)?;
                Ok(VariantInfo {
                    id: VariantId(r.get(0)?),
                    image: ImageId(r.get(1)?),
                    ordinal: r.get(2)?,
                    name: r.get(3)?,
                    rating: Rating::new(r.get(4)?).unwrap_or_default(),
                    color_tag: tag_from_code(r.get(5)?),
                    pick: pick_from_code(r.get(6)?),
                    cull_note: r.get(14)?,
                    keywords: serde_json::from_str(&kw).unwrap_or_default(),
                    iptc: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
                    people: Vec::new(),
                    geotag: geo_point(r.get(11)?, r.get(12)?, r.get(13)?),
                    edited: r.get(8)?,
                    read_only: r.get(9)?,
                })
            })
            .optional()?;
        let mut info = row.ok_or(SessionError::NotFound {
            kind: "variant",
            id: v.0,
        })?;
        let mut st = self.conn.prepare_cached(
            "SELECT name FROM variant_people WHERE variant_id = ? AND rejected = 0 ORDER BY ord",
        )?;
        info.people = st
            .query_map([v.0], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(info)
    }

    /// All variant ids in the session (any folder), unordered.
    pub fn all_variants(&self) -> Result<Vec<VariantId>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM variants")?;
        Ok(st
            .query_map([], |r| r.get(0).map(VariantId))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Variants whose adjustments differ from `Adjustments::default()` other than by
    /// the profile alone being Neutral: the edits the app must read from their sidecars.
    /// Those plus [`Self::neutral_default_variants`] are every non-default edit (a
    /// superset of the `edited` ones).
    pub fn variants_with_adjustments(&self) -> Result<Vec<VariantId>> {
        self.variants_where_custom(CUSTOM_EDIT)
    }

    /// Variants whose adjustments are exactly [`neutral_default`]: an untouched sidecar
    /// from before profiles (or Neutral picked and nothing else). Not "edited", but not
    /// the default either; known from the index alone, so no sidecar is read.
    pub fn neutral_default_variants(&self) -> Result<Vec<VariantId>> {
        self.variants_where_custom(CUSTOM_NEUTRAL)
    }

    fn variants_where_custom(&self, state: i64) -> Result<Vec<VariantId>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id FROM variants WHERE custom = ? ORDER BY id")?;
        Ok(st
            .query_map([state], |r| r.get(0).map(VariantId))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn locate(&self, v: VariantId) -> Result<Located> {
        let mut st = self.conn.prepare_cached(
            "SELECT v.sidecar, v.image_id, i.rel_path FROM variants v JOIN images i ON i.id = v.image_id
             WHERE v.id = ?",
        )?;
        st.query_row([v.0], |r| {
            let rel: String = r.get(2)?;
            Ok(Located {
                sidecar: r.get(0)?,
                image: ImageId(r.get(1)?),
                image_rel: rel,
            })
        })
        .optional()?
        .ok_or(SessionError::NotFound {
            kind: "variant",
            id: v.0,
        })
    }

    pub(crate) fn read_sidecar(&self, rel: &str) -> Result<Loaded> {
        let path = self.root.join(rel);
        let bytes = atomic::read_json_file(&path).at(&path)?;
        Sidecar::from_json(&bytes).map_err(|e| match e {
            crate::sidecar::SidecarError::NoMigration(found) => SessionError::NoMigration {
                path,
                found,
                supported: SIDECAR_SCHEMA_VERSION,
            },
            other => SessionError::Json {
                path,
                message: other.to_string(),
            },
        })
    }

    /// Current adjustments of a variant (from its sidecar).
    pub fn load_adjustments(&self, v: VariantId) -> Result<Adjustments> {
        let loc = self.locate(v)?;
        Ok(self.read_sidecar(&loc.sidecar)?.sidecar.adjustments)
    }

    /// Create a new variant of `image` whose adjustments are a copy of its
    /// first variant's (organizational metadata starts fresh).
    pub fn new_variant(&mut self, image: ImageId) -> Result<VariantId> {
        let tx = self.write_lock()?;
        let existing = self.variants(image)?;
        let first = self.locate(existing[0])?;
        let loaded = self.read_sidecar(&first.sidecar)?;
        if loaded.too_new {
            // Copying would silently drop what this build cannot read.
            return Err(SessionError::SidecarTooNew {
                path: self.root.join(&first.sidecar),
                found: loaded.sidecar.schema_version,
                supported: SIDECAR_SCHEMA_VERSION,
            });
        }
        let src = loaded.sidecar;
        let max_ord: u32 = self.conn.query_row(
            "SELECT max(ordinal) FROM variants WHERE image_id = ?",
            [image.0],
            |r| r.get(0),
        )?;
        let image_rel = RelPath::new(&first.image_rel)?;
        // Skip ordinals whose sidecar exists on disk but not in the index.
        let mut ordinal = max_ord + 1;
        while self
            .root
            .join(image_rel.sidecar_rel(ordinal))
            .symlink_metadata()
            .is_ok()
        {
            ordinal += 1;
        }
        let sc = Sidecar {
            schema_version: SIDECAR_SCHEMA_VERSION,
            variant_id: self.fresh_id("variants")?,
            name: format!("Variant {ordinal}"),
            ordinal,
            rating: Rating::default(),
            color_tag: ColorTag::None,
            pick: PickState::Unflagged,
            cull_note: None,
            keywords: Vec::new(),
            ..src
        };
        let pending = Pending {
            rel: image_rel.sidecar_rel(ordinal),
            sidecar: sc,
            base: loaded.raw,
        };
        let (done, err) = self.write_sidecars(vec![pending]);
        if let Some(e) = err {
            return Err(e);
        }
        let w = &done[0];
        index_written(&tx, &self.root, self.external_root(), w)?;
        tx.commit()?;
        Ok(VariantId(w.sidecar.variant_id))
    }

    /// Delete a variant. Its sidecar is moved to `Trash/.ds/deleted/` (never
    /// removed), and goes to the macOS Trash with [`Session::empty_trash`].
    /// The last variant of an image cannot be deleted (move the image to
    /// Trash instead), so originals are never orphaned.
    pub fn delete_variant(&mut self, v: VariantId) -> Result<()> {
        {
            let tx = self.write_lock()?;
            let loc = self.locate(v)?;
            if self.variants(loc.image)?.len() <= 1 {
                return Err(SessionError::LastVariant);
            }
            let path = self.root.join(&loc.sidecar);
            let dest = self.root.join(DELETED_DIR).join(format!("{}.json", v.0));
            crate::files::run_ops(&[(path, dest)])?;
            tx.execute("DELETE FROM variants WHERE id = ?", [v.0])?;
            tx.commit()?;
        }
        self.undo.forget(v.0);
        self.record_album_memberships(v)?;
        self.drop_from_albums(&[v])?;
        Ok(())
    }

    pub fn rename_variant(&mut self, v: VariantId, name: &str) -> Result<()> {
        let name = name.trim().to_owned();
        self.modify(&[v], |_, sc| {
            let changed = sc.name != name;
            sc.name.clone_from(&name);
            changed
        })
    }

    // ------------------------------------------------------ organizational ---

    // Ratings, tags, picks and keywords are undoable: one variant is a plain
    // step on its history; several form one group step.

    pub fn set_rating(&mut self, vs: &[VariantId], rating: Rating) -> Result<()> {
        self.modify_undoable(vs, |_, sc| {
            std::mem::replace(&mut sc.rating, rating) != rating
        })
    }

    pub fn set_color_tag(&mut self, vs: &[VariantId], tag: ColorTag) -> Result<()> {
        self.modify_undoable(vs, |_, sc| std::mem::replace(&mut sc.color_tag, tag) != tag)
    }

    /// Changing the pick clears the AI Cull note: it no longer explains the flag.
    pub fn set_pick(&mut self, vs: &[VariantId], pick: PickState) -> Result<()> {
        self.modify_undoable(vs, |_, sc| {
            let changed = std::mem::replace(&mut sc.pick, pick) != pick;
            if changed {
                sc.cull_note = None;
            }
            changed
        })
    }

    /// Store why AI Cull rejected the variants (`None` clears it). Only a
    /// rejected variant keeps a note; on any other it is cleared. Not an undo
    /// step: it goes with the reject (whose undo restores it).
    pub fn set_cull_note(&mut self, vs: &[VariantId], note: Option<&str>) -> Result<()> {
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        self.modify(vs, |_, sc| {
            let note = note.filter(|_| sc.pick == PickState::Rejected);
            if sc.cull_note.as_deref() == note {
                return false;
            }
            sc.cull_note = note.map(str::to_owned);
            true
        })
    }

    /// Replace the keywords (trimmed, de-duplicated, order kept).
    pub fn set_keywords(&mut self, vs: &[VariantId], keywords: &[String]) -> Result<()> {
        let clean = normalize_keywords(keywords.iter().map(String::as_str));
        self.modify_undoable(vs, |_, sc| {
            std::mem::replace(&mut sc.keywords, clean.clone()) != clean
        })
    }

    pub fn add_keyword(&mut self, vs: &[VariantId], keyword: &str) -> Result<()> {
        let kw = keyword.trim();
        if kw.is_empty() {
            return Ok(());
        }
        self.modify_undoable(vs, |_, sc| {
            if sc.keywords.iter().any(|k| k == kw) {
                return false;
            }
            sc.keywords.push(kw.to_owned());
            true
        })
    }

    pub fn remove_keyword(&mut self, vs: &[VariantId], keyword: &str) -> Result<()> {
        let kw = keyword.trim();
        self.modify_undoable(vs, |_, sc| {
            let before = sc.keywords.len();
            sc.keywords.retain(|k| k != kw);
            sc.keywords.len() != before
        })
    }

    /// Set (or, with `None`, remove) where the photos were taken: the map's
    /// geotag, kept in the sidecars of every variant of their originals (a
    /// position belongs to the photo, so variants never disagree) and never
    /// written to the original. One undo step. Callers build it with
    /// [`GeoPoint::new`] (an out-of-range one would read back as none).
    pub fn set_geotag(&mut self, vs: &[VariantId], geotag: Option<GeoPoint>) -> Result<()> {
        let mut all = Vec::new();
        for v in vs {
            for sibling in self.variants(self.variant(*v)?.image)? {
                if !all.contains(&sibling) {
                    all.push(sibling);
                }
            }
        }
        self.modify_undoable(&all, |_, sc| {
            let changed = sc.geotag != geotag || sc.geotag_unreadable.is_some();
            sc.geotag = geotag;
            sc.geotag_unreadable = None;
            changed
        })
    }

    /// Replace the IPTC fields (trimmed; undoable like keywords). Never touches
    /// the adjustments.
    pub fn set_iptc(&mut self, vs: &[VariantId], iptc: &IptcMeta) -> Result<()> {
        let clean = iptc.clone().trimmed();
        self.modify_undoable(vs, |_, sc| {
            std::mem::replace(&mut sc.iptc, clean.clone()) != clean
        })
    }

    /// The last applied AI Optimize report of a variant (opaque JSON).
    pub fn optimize_report(&self, v: VariantId) -> Result<Option<Value>> {
        let loc = self.locate(v)?;
        Ok(self.read_sidecar(&loc.sidecar)?.sidecar.optimize_report)
    }

    /// Store `report` as the variants' last applied AI Optimize report (`None` clears
    /// it). Not an undo step: the report explains the edit, which has its own history.
    pub fn set_optimize_report(&mut self, vs: &[VariantId], report: Option<&Value>) -> Result<()> {
        self.modify(vs, |_, sc| {
            if sc.optimize_report.as_ref() == report {
                return false;
            }
            sc.optimize_report = report.cloned();
            true
        })
    }

    /// The AI changes the app keeps for a variant (opaque JSON).
    pub fn ai_changes(&self, v: VariantId) -> Result<Option<Value>> {
        let loc = self.locate(v)?;
        Ok(self.read_sidecar(&loc.sidecar)?.sidecar.ai_changes)
    }

    /// Store the variants' AI changes (`None` clears them). Not an undo step: the app
    /// writes them again whenever its undo moves them.
    pub fn set_ai_changes(&mut self, vs: &[VariantId], changes: Option<&Value>) -> Result<()> {
        self.modify(vs, |_, sc| {
            if sc.ai_changes.as_ref() == changes {
                return false;
            }
            sc.ai_changes = changes.cloned();
            true
        })
    }

    /// Apply `f` to each variant's sidecar; changed ones are written and
    /// re-indexed in one transaction.
    pub(crate) fn modify(
        &mut self,
        vs: &[VariantId],
        f: impl FnMut(VariantId, &mut Sidecar) -> bool,
    ) -> Result<()> {
        let (_, err) = self.modify_collect(vs, f)?;
        err.map_or(Ok(()), Err)
    }

    /// [`Session::modify`] plus undo recording for organizational edits.
    fn modify_undoable(
        &mut self,
        vs: &[VariantId],
        f: impl FnMut(VariantId, &mut Sidecar) -> bool,
    ) -> Result<()> {
        let now = (self.clock)();
        let (written, err) = self.modify_collect(vs, f)?;
        if let [(v, before)] = written.as_slice() {
            self.undo.record(v.0, before, now, None);
        } else if !written.is_empty() {
            let g = self.undo.new_group();
            for (v, before) in &written {
                self.undo.record_group(g, v.0, before);
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// Like [`Session::modify`], returning `(variant, state before)` for
    /// every sidecar actually written plus the first write error, if any.
    /// Nothing is written if any variant is unknown or read-only.
    ///
    /// A large selection is written in chunks of [`BATCH_CHUNK`] sidecars,
    /// each with its own journal and transaction (a select-all edit of 100k
    /// photos must not hold gigabytes or the write lock throughout). Unknown
    /// and read-only variants are still rejected before anything is written;
    /// a failure after the first chunk returns what was written so far.
    pub(crate) fn modify_collect(
        &mut self,
        vs: &[VariantId],
        mut f: impl FnMut(VariantId, &mut Sidecar) -> bool,
    ) -> Result<Collected> {
        if vs.len() <= BATCH_CHUNK {
            return self.modify_chunk(vs, &mut f);
        }
        let mut seen = HashSet::new();
        let unique: Vec<VariantId> = vs.iter().copied().filter(|v| seen.insert(*v)).collect();
        for &v in &unique {
            let loc = self.locate(v)?;
            let read_only: bool =
                self.conn
                    .query_row("SELECT read_only FROM variants WHERE id = ?", [v.0], |r| {
                        r.get(0)
                    })?;
            if read_only {
                // The same refusal `modify_chunk` makes, before any chunk is written.
                let loaded = self.read_sidecar(&loc.sidecar)?;
                if loaded.too_new {
                    return Err(SessionError::SidecarTooNew {
                        path: self.root.join(&loc.sidecar),
                        found: loaded.sidecar.schema_version,
                        supported: SIDECAR_SCHEMA_VERSION,
                    });
                }
            }
        }
        let mut out = Vec::new();
        for chunk in unique.chunks(BATCH_CHUNK) {
            match self.modify_chunk(chunk, &mut f) {
                Ok((done, err)) => {
                    out.extend(done);
                    if err.is_some() {
                        return Ok((out, err));
                    }
                }
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => return Ok((out, Some(e))),
            }
        }
        Ok((out, None))
    }

    fn modify_chunk(
        &mut self,
        vs: &[VariantId],
        f: &mut impl FnMut(VariantId, &mut Sidecar) -> bool,
    ) -> Result<Collected> {
        // Read-modify-write under the index's write lock, so two Sessions on
        // the same folder (two app instances) never lose each other's updates.
        let tx = self.write_lock()?;
        let mut seen = HashSet::new();
        let mut writes = Vec::new();
        let mut befores = HashMap::new();
        for &v in vs {
            if !seen.insert(v) {
                continue;
            }
            let loc = self.locate(v)?;
            let loaded = self.read_sidecar(&loc.sidecar)?;
            if loaded.too_new {
                return Err(SessionError::SidecarTooNew {
                    path: self.root.join(&loc.sidecar),
                    found: loaded.sidecar.schema_version,
                    supported: SIDECAR_SCHEMA_VERSION,
                });
            }
            let mut sc = loaded.sidecar;
            let before = UndoState::of(&sc);
            if f(v, &mut sc) {
                befores.insert(sc.variant_id, before);
                writes.push(Pending {
                    rel: loc.sidecar,
                    sidecar: sc,
                    base: loaded.raw,
                });
            }
        }
        let (done, err) = self.write_sidecars(writes);
        let mut out = Vec::with_capacity(done.len());
        for w in &done {
            update_variant_row(&tx, &w.sidecar, w.mtime, w.size)?;
            let id = w.sidecar.variant_id;
            if let Some(before) = befores.remove(&id) {
                out.push((VariantId(id), before));
            }
        }
        tx.commit()?;
        Ok((out, err))
    }

    /// Begin an `IMMEDIATE` transaction: the cross-process write lock.
    pub(crate) fn write_lock(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    /// Write sidecars atomically as one batch (one drive flush; see
    /// [`atomic`]). Returns the successful writes with their new stat, and
    /// the first error.
    pub(crate) fn write_sidecars(
        &self,
        writes: Vec<Pending>,
    ) -> (Vec<Written>, Option<SessionError>) {
        let mut err = None;
        let mut ready = Vec::with_capacity(writes.len());
        for mut w in writes {
            // NaN/inf would be written as `null`: never store an invalid stack.
            w.sidecar.adjustments.sanitize();
            let path = self.root.join(&w.rel);
            let parent = path.parent().unwrap_or(&self.root);
            let prepared = ensure_no_symlinks(&self.root, &w.rel)
                .and_then(|()| fs::create_dir_all(parent).at(parent))
                // Re-check: create_dir_all follows a symlink created meanwhile.
                .and_then(|()| ensure_no_symlinks(&self.root, &w.rel));
            match prepared {
                Ok(()) => ready.push((w, path)),
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        }
        let items: Vec<(PathBuf, Vec<u8>)> = ready
            .iter()
            .map(|(w, path)| (path.clone(), w.sidecar.to_json_preserving(w.base.as_ref())))
            .collect();
        // A multi-sidecar batch is journaled first so a crash can't leave half of it
        // (rolled forward on the next open; see `journal`). Once the writes return,
        // the outcome is reported below, so the journal goes even if some failed:
        // left behind, it would later be replayed over newer edits.
        let journal = match crate::journal::begin(&self.root, &items) {
            Ok(j) => j,
            Err(e) => {
                let e = Err::<(), _>(e).at(self.root.join(crate::journal::JOURNAL_DIR));
                return (Vec::new(), e.err());
            }
        };
        let results = atomic::write_many(&items, self.write_hook.as_ref());
        crate::journal::finish(journal);
        let mut done = Vec::with_capacity(ready.len());
        for ((w, path), res) in ready.into_iter().zip(results) {
            match res.and_then(|()| fs::metadata(&path)).at(&path) {
                Ok(md) => done.push(Written {
                    rel: w.rel,
                    sidecar: w.sidecar,
                    mtime: mtime_ns(&md),
                    size: md.len() as i64,
                }),
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        }
        (done, err)
    }
}

pub(crate) fn snapshot(conn: &Connection, path: &Path) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.partial", path.display()));
    let _ = fs::remove_file(&tmp);
    conn.backup(MAIN_DB, &tmp, None)?;
    fs::rename(&tmp, path).at(path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        atomic::sync_dir(dir).at(dir)?;
    }
    Ok(())
}

/// A random positive 63-bit id (time, process, counter through BLAKE3).
pub(crate) fn random_id() -> i64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut h = blake3::Hasher::new();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        h.update(&nanos.to_le_bytes());
        h.update(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        h.update(&std::process::id().to_le_bytes());
        let bytes: [u8; 8] = h.finalize().as_bytes()[..8].try_into().expect("8 bytes");
        let id = i64::from_le_bytes(bytes) & i64::MAX;
        if id != 0 {
            return id;
        }
    }
}

pub(crate) fn normalize_keywords<'a>(kws: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    kws.map(str::trim)
        .filter(|k| !k.is_empty() && seen.insert(k.to_owned()))
        .map(str::to_owned)
        .collect()
}

/// A new original for [`Session::add_new_originals`].
#[derive(Debug, Clone)]
pub struct NewOriginal {
    pub rel: String,
    pub hash: FileHash,
    pub content_hash: Option<FileHash>,
    pub meta: ImageMetaLite,
    /// Variant ids of the photo this file replaced (see [`Sidecar::replaced`]).
    pub replaced: Vec<i64>,
    /// The first variant's IPTC fields (read from the file, a metadata preset).
    pub iptc: IptcMeta,
}

/// A sidecar about to be written; `base` is the file as last read, whose
/// unknown fields are preserved.
#[derive(Debug)]
pub(crate) struct Pending {
    pub rel: String,
    pub sidecar: Sidecar,
    pub base: Option<Value>,
}

impl Pending {
    pub fn new(rel: String, sidecar: Sidecar) -> Self {
        Self {
            rel,
            sidecar,
            base: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Written {
    pub rel: String,
    pub sidecar: Sidecar,
    pub mtime: i64,
    pub size: i64,
}

fn index_written(
    tx: &Transaction<'_>,
    root: &Path,
    external: Option<&Path>,
    w: &Written,
) -> Result<()> {
    let loaded = Loaded {
        sidecar: w.sidecar.clone(),
        too_new: false,
        raw: None,
    };
    index_sidecar(tx, root, external, &w.rel, &loaded, w.mtime, w.size)
}

/// Remember the stat of an original whose content was just verified.
pub(crate) fn record_stat(conn: &Connection, image: i64, st: FileStat) -> Result<()> {
    conn.execute(
        "UPDATE images SET file_size = ?, file_mtime = ?, file_ino = ?, missing = 0 WHERE id = ?",
        params![st.size as i64, st.mtime, st.ino as i64, image],
    )?;
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Located {
    pub sidecar: String,
    pub image: ImageId,
    pub image_rel: String,
}

#[derive(Debug, Default)]
pub(crate) struct DiskScan {
    /// Sidecar relative path → (mtime ns, size).
    pub sidecars: HashMap<String, (i64, i64)>,
    /// Original relative path → stat (only when requested).
    pub originals: HashMap<String, FileStat>,
    /// Photos an external walk saw (more than listed when truncated).
    pub seen: usize,
    /// The walk stopped listing: files it did not list are unknown, not gone.
    pub truncated: bool,
    /// Sub-folders an external walk couldn't read, as `Capture/<rel>/`
    /// prefixes: what's under them is unknown, neither gone nor there.
    pub unreadable: Vec<String>,
}

impl DiskScan {
    /// An original the walk could not account for (under an unreadable
    /// folder, or past the cap): neither present nor gone.
    pub(crate) fn unknown(&self, rel: &str) -> bool {
        self.truncated || self.unreadable.iter().any(|d| rel.starts_with(d.as_str()))
    }
}

/// Photos and videos: what counts toward [`MAX_EXTERNAL_FILES`].
fn is_photo_name(name: &str) -> bool {
    const EXTS: &[&str] = &[
        "arw", "arq", "srf", "sr2", "cr2", "cr3", "crw", "nef", "nrw", "raf", "orf", "rw2", "rwl",
        "dng", "pef", "srw", "3fr", "fff", "iiq", "erf", "mos", "mrw", "kdc", "dcr", "x3f", "raw",
        "jpg", "jpeg", "heic", "heif", "hif", "tif", "tiff", "png", "mp4", "mts", "m2ts", "mov",
        "mxf", "m4v", "avi",
    ];
    name.rsplit_once('.')
        .is_some_and(|(_, e)| EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn walk(root: &Path, dir: &Path, originals: bool, out: &mut DiskScan) -> Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(dir),
    };
    for entry in rd {
        let entry = entry.at(dir)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let ft = entry.file_type().at(entry.path())?;
        if ft.is_dir() {
            if name == ".ds" {
                let settings = entry.path().join("settings");
                // Never follow a symlinked settings dir out of the session.
                if settings
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink())
                {
                    continue;
                }
                collect_sidecars(root, &settings, out)?;
                for sub in fs::read_dir(&settings).into_iter().flatten().flatten() {
                    let is_variant_dir = sub.file_name().to_str().is_some_and(|n| {
                        n.len() > 1
                            && n.starts_with('v')
                            && n[1..].bytes().all(|b| b.is_ascii_digit())
                    });
                    if is_variant_dir && sub.file_type().is_ok_and(|t| t.is_dir()) {
                        collect_sidecars(root, &sub.path(), out)?;
                    }
                }
            } else if !name.starts_with('.') {
                walk(root, &entry.path(), originals, out)?;
            }
        } else if ft.is_file() && name.starts_with('.') {
            // A new photo's file a crash left half-written (AI Upscale, merges).
            ds_fsutil::remove_stale_temp(&entry.path());
        } else if ft.is_file() && originals {
            let md = entry.metadata().at(entry.path())?;
            if let Some(rel) = rel_string(root, &entry.path()) {
                out.originals.insert(rel, FileStat::of(&md));
            }
        }
    }
    Ok(())
}

/// Folder names an external walk never enters: packages and app libraries
/// (their contents are not the user's loose photos).
fn is_bundle(name: &str) -> bool {
    const EXT: [&str; 11] = [
        "app",
        "bundle",
        "photoslibrary",
        "photolibrary",
        "migratedphotolibrary",
        "lrdata",
        "cocatalog",
        "cosessiondb",
        "dsession",
        "fcpbundle",
        "aplibrary",
    ];
    name.rsplit_once('.')
        .is_some_and(|(_, e)| EXT.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// A Capture One session folder (a `*.cosessiondb` at its top): its `Trash/`
/// (C1 rejects), `Output/` (exports) and `CaptureOne/` (settings, caches)
/// folders are not the user's photos.
pub fn is_capture_one_session(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.rsplit_once('.'))
                .is_some_and(|(_, x)| x.eq_ignore_ascii_case("cosessiondb"))
        })
    })
}

/// Folders a walk of a Capture One session skips (see [`is_capture_one_session`]).
fn capture_one_skips(name: &str, depth: usize) -> bool {
    name.eq_ignore_ascii_case("CaptureOne")
        || (depth == 0
            && (name.eq_ignore_ascii_case("Trash") || name.eq_ignore_ascii_case("Output")))
}

/// Walk the user's folder for an external session (see [`Session::external_files`]).
fn walk_external(
    ext: &Path,
    dir: &Path,
    depth: usize,
    capture_one: bool,
    out: &mut DiskScan,
) -> std::io::Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) if depth > 0 => {
            // Unreadable sub-folder (permissions, a share that hiccuped): skip
            // it, keep the rest, and say so (its photos aren't missing).
            if let Some(rel) = rel_string(ext, dir) {
                out.unreadable.push(format!("Capture/{rel}/"));
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            // A new photo's file a crash left half-written (AI Upscale, merges).
            ds_fsutil::remove_stale_temp(&entry.path());
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let skipped = is_bundle(&name)
                || (capture_one && capture_one_skips(&name, depth))
                || is_developed_dir(&entry.path(), &name, depth);
            if depth < MAX_EXTERNAL_DEPTH && !skipped {
                walk_external(ext, &entry.path(), depth + 1, capture_one, out)?;
            }
        } else if ft.is_file() {
            let photo = is_photo_name(&name);
            if photo {
                out.seen += 1;
            }
            if (photo && out.seen > MAX_EXTERNAL_FILES)
                || out.originals.len() >= MAX_EXTERNAL_ENTRIES
            {
                out.truncated = true;
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            if let Some(rel) = rel_string(ext, &entry.path()) {
                out.originals
                    .insert(format!("Capture/{rel}"), FileStat::of(&md));
            }
        }
    }
    Ok(())
}

fn collect_sidecars(root: &Path, dir: &Path, out: &mut DiskScan) -> Result<()> {
    atomic::remove_stale_temps(dir, STALE_TEMP_AGE);
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let is_json = path.extension().is_some_and(|e| e == "json");
        let hidden = entry
            .file_name()
            .to_str()
            .is_none_or(|n| n.starts_with('.'));
        if !is_json || hidden || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let md = entry.metadata().at(&path)?;
        if let Some(rel) = rel_string(root, &path) {
            out.sidecars.insert(rel, (mtime_ns(&md), md.len() as i64));
        }
    }
    Ok(())
}

const IMAGE_SELECT: &str =
    "SELECT id, rel_path, hash, file_size, capture_ms, camera, lens, iso, focal, width, height,
    orientation, missing, as_shot_temp, as_shot_tint, content_hash, file_changed FROM images";

fn image_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImageInfo> {
    let rel: String = r.get(1)?;
    let hash: Vec<u8> = r.get(2)?;
    let orientation: u16 = r.get(11)?;
    Ok(ImageInfo {
        id: ImageId(r.get(0)?),
        // Rows only ever come from validated paths.
        rel_path: RelPath::new(&rel).map_err(|_| rusqlite::Error::InvalidQuery)?,
        hash: FileHash(hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?),
        file_size: r.get::<_, i64>(3)? as u64,
        meta: ImageMetaLite {
            capture_time_ms: r.get(4)?,
            camera: r.get(5)?,
            lens: r.get(6)?,
            iso: r.get(7)?,
            focal_length_mm: r.get(8)?,
            width: r.get(9)?,
            height: r.get(10)?,
            orientation: ds_core::Orientation::from_exif(orientation),
            as_shot_wb: r
                .get::<_, Option<f32>>(13)?
                .zip(r.get::<_, Option<f32>>(14)?),
        },
        missing: r.get(12)?,
        content_hash: r
            .get::<_, Option<Vec<u8>>>(15)?
            .and_then(|b| b.try_into().ok())
            .map(FileHash),
        file_changed: r.get(16)?,
    })
}

/// Insert or update the image and variant rows for one sidecar.
pub(crate) fn index_sidecar(
    tx: &Transaction<'_>,
    root: &Path,
    external: Option<&Path>,
    sidecar_rel: &str,
    loaded: &Loaded,
    mtime: i64,
    size: i64,
) -> Result<()> {
    let sc = &loaded.sidecar;
    let image_rel = resolve_sidecar(root, external, sidecar_rel, sc.ordinal).ok_or_else(|| {
        SessionError::InvalidPath {
            path: sidecar_rel.to_owned(),
            reason: "not a sidecar location (or its long-named original is gone)",
        }
    })?;
    // Another sidecar on disk already claims this variant or image id at a
    // different location (e.g. a folder duplicated in Finder): refuse.
    let dup_variant: Option<String> = tx
        .query_row(
            "SELECT sidecar FROM variants WHERE id = ? AND sidecar <> ?",
            params![sc.variant_id, sidecar_rel],
            |r| r.get(0),
        )
        .optional()?;
    let dup_image: Option<String> = tx
        .query_row(
            "SELECT rel_path FROM images WHERE id = ? AND rel_path <> ?",
            params![sc.image_id, image_rel.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(other) = dup_variant.or(dup_image) {
        return Err(SessionError::DuplicatePath(other));
    }
    let md = fs::metadata(original_path(root, external, image_rel.as_str()));
    let (file_size, missing) = md.map_or((0, true), |m| (m.len() as i64, false));
    let m = &sc.meta;
    // The file's stat (file_mtime/file_ino) is only recorded once its content
    // has been verified against `hash` (import or rescan). A new row, or a
    // row whose path or hash changed, starts unverified (NULL); an existing
    // content-mismatch flag survives re-indexing.
    let mut st = tx.prepare_cached(
        "INSERT INTO images(id, rel_path, folder, file_name_lc, hash, file_size, capture_ms, camera, camera_lc, lens,
            lens_lc, iso, focal, width, height, orientation, missing, as_shot_temp, as_shot_tint, content_hash,
            file_changed)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
         ON CONFLICT(id) DO UPDATE SET
            sample_hash = CASE WHEN rel_path = ?2 AND hash = ?5 THEN sample_hash ELSE NULL END,
            content_hash = ?20, file_changed = ?21,
            file_mtime = CASE WHEN rel_path = ?2 AND hash = ?5 THEN file_mtime ELSE NULL END,
            file_ino = CASE WHEN rel_path = ?2 AND hash = ?5 THEN file_ino ELSE NULL END,
            missing = CASE WHEN ?17 THEN 1 WHEN rel_path = ?2 AND hash = ?5 THEN missing ELSE 0 END,
            rel_path=?2, folder=?3, file_name_lc=?4, hash=?5, file_size=?6, capture_ms=?7,
            camera=?8, camera_lc=?9, lens=?10, lens_lc=?11, iso=?12, focal=?13, width=?14, height=?15,
            orientation=?16, as_shot_temp=?18, as_shot_tint=?19",
    )?;
    st.execute(params![
        sc.image_id,
        image_rel.as_str(),
        image_rel.folder().code(),
        image_rel.file_name().to_lowercase(),
        &sc.hash.0[..],
        file_size,
        m.capture_time_ms,
        m.camera,
        m.camera.as_deref().map(str::to_lowercase),
        m.lens,
        m.lens.as_deref().map(str::to_lowercase),
        m.iso,
        m.focal_length_mm,
        m.width,
        m.height,
        m.orientation.to_exif(),
        missing,
        m.as_shot_wb.map(|w| w.0),
        m.as_shot_wb.map(|w| w.1),
        sc.content_hash.as_ref().map(|h| &h.0[..]),
        !sc.replaced.is_empty(),
    ])?;
    let mut st = tx.prepare_cached(
        "INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, keywords, search, sidecar,
            sidecar_mtime, sidecar_size, read_only, custom, iptc, geo_lat, geo_lon, geo_alt, cull_note)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
         ON CONFLICT(id) DO UPDATE SET image_id=?2, ordinal=?3, name=?4, rating=?5, color_tag=?6, pick=?7, edited=?8,
            keywords=?9, search=?10, sidecar=?11, sidecar_mtime=?12, sidecar_size=?13, read_only=?14, custom=?15,
            iptc=?16, geo_lat=?17, geo_lon=?18, geo_alt=?19, cull_note=?20",
    )?;
    st.execute(params![
        sc.variant_id,
        sc.image_id,
        sc.ordinal,
        sc.name,
        sc.rating.stars(),
        tag_code(sc.color_tag),
        pick_code(sc.pick),
        !sc.adjustments.is_identity(),
        serde_json::to_string(&sc.keywords).unwrap_or_default(),
        search_text(sc),
        sidecar_rel,
        mtime,
        size,
        loaded.too_new,
        custom_state(&sc.adjustments),
        iptc_json(&sc.iptc),
        sc.geotag.map(|g| g.latitude),
        sc.geotag.map(|g| g.longitude),
        sc.geotag.and_then(|g| g.altitude_m),
        cull_note(sc),
    ])?;
    write_people_rows(tx, sc)
}

/// `variants.cull_note`: the sidecar's note while the variant is rejected.
fn cull_note(sc: &Sidecar) -> Option<&str> {
    sc.cull_note
        .as_deref()
        .filter(|_| sc.pick == PickState::Rejected)
}

/// A geotag from its index columns (both coordinates, or none).
pub(crate) fn geo_point(lat: Option<f64>, lon: Option<f64>, alt: Option<f64>) -> Option<GeoPoint> {
    GeoPoint::new(lat?, lon?, alt)
}

/// Mirror a sidecar's people tags and rejections into `variant_people`.
fn write_people_rows(tx: &Transaction<'_>, sc: &Sidecar) -> Result<()> {
    tx.execute(
        "DELETE FROM variant_people WHERE variant_id = ?",
        [sc.variant_id],
    )?;
    let mut st = tx.prepare_cached(
        "INSERT INTO variant_people(variant_id, ord, name, person_id, x, y, w, h, rejected)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let tags = sc.people.iter().map(|t| (t, false));
    let rejected = sc.people_rejected.iter().map(|t| (t, true));
    for (ord, (t, rejected)) in tags.chain(rejected).enumerate() {
        let r = t.region;
        st.execute(params![
            sc.variant_id,
            ord as i64,
            t.name,
            t.id,
            r.map(|r| r.x),
            r.map(|r| r.y),
            r.map(|r| r.w),
            r.map(|r| r.h),
            rejected,
        ])?;
    }
    Ok(())
}

/// `variants.custom`: the adjustments are the default.
const CUSTOM_DEFAULT: i64 = 0;
/// `variants.custom`: anything else; the app reads them from the sidecar.
const CUSTOM_EDIT: i64 = 1;
/// `variants.custom`: exactly [`neutral_default`] (every untouched sidecar from before
/// profiles), which the app gets without reading the sidecar.
const CUSTOM_NEUTRAL: i64 = 2;

/// The default adjustments with the Neutral profile: what a sidecar without a
/// `profile` key and no other edit loads as.
pub fn neutral_default() -> Adjustments {
    Adjustments {
        profile: ds_core::adjust::Profile::legacy(),
        ..Adjustments::default()
    }
}

fn custom_state(a: &Adjustments) -> i64 {
    if *a == Adjustments::default() {
        CUSTOM_DEFAULT
    } else if *a == neutral_default() {
        CUSTOM_NEUTRAL
    } else {
        CUSTOM_EDIT
    }
}

/// `variants.iptc`: the fields as JSON, `""` when none is set.
fn iptc_json(m: &IptcMeta) -> String {
    if m.is_empty() {
        String::new()
    } else {
        serde_json::to_string(m).unwrap_or_default()
    }
}

/// Update the organizational columns of an existing variant row after a write.
fn update_variant_row(tx: &Transaction<'_>, sc: &Sidecar, mtime: i64, size: i64) -> Result<()> {
    let mut st = tx.prepare_cached(
        "UPDATE variants SET name=?, rating=?, color_tag=?, pick=?, edited=?, custom=?, keywords=?, search=?,
            iptc=?, sidecar_mtime=?, sidecar_size=?, geo_lat=?, geo_lon=?, geo_alt=?, cull_note=? WHERE id=?",
    )?;
    st.execute(params![
        sc.name,
        sc.rating.stars(),
        tag_code(sc.color_tag),
        pick_code(sc.pick),
        !sc.adjustments.is_identity(),
        custom_state(&sc.adjustments),
        serde_json::to_string(&sc.keywords).unwrap_or_default(),
        search_text(sc),
        iptc_json(&sc.iptc),
        mtime,
        size,
        sc.geotag.map(|g| g.latitude),
        sc.geotag.map(|g| g.longitude),
        sc.geotag.and_then(|g| g.altitude_m),
        cull_note(sc),
        sc.variant_id,
    ])?;
    write_people_rows(tx, sc)
}

pub(crate) fn read_settings(dir: &Path) -> Result<SessionSettings> {
    read_or_recover_settings(dir)
}

pub(crate) fn write_settings(root: &Path, s: &SessionSettings) -> Result<()> {
    let dir = root.join(SETTINGS_DIR);
    fs::create_dir_all(&dir).at(&dir)?;
    let path = dir.join(SESSION_FILE);
    let bytes = serde_json::to_vec_pretty(s).map_err(|e| json_err(&path, &e))?;
    atomic::write_atomic(&path, &bytes).at(&path)
}

/// Read `session.json`; if it is missing but exactly one `*.dsession` index
/// exists, recover the name from it and rewrite the settings.
fn read_or_recover_settings(dir: &Path) -> Result<SessionSettings> {
    let path = dir.join(SETTINGS_DIR).join(SESSION_FILE);
    match atomic::read_json_file(&path) {
        Ok(b) => {
            let s: SessionSettings = serde_json::from_slice(&b).map_err(|e| json_err(&path, &e))?;
            validate_name(&s.name)?;
            Ok(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let dbs: Vec<String> = fs::read_dir(dir)
                .map_err(|_| SessionError::NotASession(dir.to_owned()))?
                .flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".dsession"))
                        .map(str::to_owned)
                })
                .collect();
            match dbs.as_slice() {
                [name] if dir.join("Capture").is_dir() => {
                    let s = SessionSettings {
                        schema_version: SIDECAR_SCHEMA_VERSION,
                        name: name.clone(),
                        auto_apply: None,
                        originals: Originals::Internal,
                        folder_identity: None,
                    };
                    validate_name(name)?;
                    write_settings(dir, &s)?;
                    Ok(s)
                }
                _ => Err(SessionError::NotASession(dir.to_owned())),
            }
        }
        Err(e) => Err(e).at(&path),
    }
}
