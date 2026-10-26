//! Card import (`lt-import`) for the app: scan a card, plan and run a
//! verified copy into a session, register what was copied, and resume.
//!
//! - [`scan_card`] walks the card read-only (`lt_import::scan_volume`), records
//!   the card's identity (volume UUID) and marks candidates the library already
//!   holds (global index dedupe: size → partial hash → full BLAKE3 of both
//!   copies, see `lt_import::find_duplicate`). A library copy that no longer
//!   matches its recorded hash is reported as damaged.
//! - [`CardScan::import`] first checks that the card in the reader is still
//!   the one scanned (same volume, every chosen file with the scanned size and
//!   mtime, and — for files an earlier run of this import already copied — the
//!   same partial hash as its journal); a different card is never mixed into a
//!   run. It then checks free space (before creating anything), creates or
//!   opens the destination session (Internal originals under `Capture/`), runs
//!   `lt_import::run_with` with a [`SessionSink`] that registers each verified,
//!   durable primary file, and records the session in the global index.
//! - A file counts as imported only when it is copied, verified **and**
//!   registered. The result lists every media file on the card (from a fresh
//!   scan after the run) that has no verified library copy (`card_only`);
//!   `safe_to_remove` is true only when there is none and nothing else stays
//!   only on the card.
//! - [`pending_imports`] / [`discard_pending_import`] back the "resume import?"
//!   prompt; a pending import names the card(s) that resume it.
//! - [`recover_unregistered`] (on session open) registers originals an import
//!   copied into `Capture/` but could not add to the session.
//!
//! Safety rules come from lt-import: the card is only ever opened read-only,
//! nothing is overwritten, and a file counts as copied only after its bytes
//! were re-read from the destination and matched.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use lt_core::{FileHash, IptcMeta};
use lt_import::journal::{EntryState, Journal, SourceKey};
use lt_import::{
    DedupeLookup, FaultInjector, FileKind, ImportError, ImportPlan, ImportedFile, KnownCopy,
    MetaProvider, NOT_REGISTERED, PartialHash, PlanOptions, RunHooks, ScanReport, SessionSink,
    find_duplicate, full_hash, partial_hash,
};
use lt_session::{GlobalIndex, ImageMetaLite, NewOriginal, Session, SessionError};
use lt_template::{Template, Vars};

use crate::error::DsError;
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::session::metadata::{PhotoMetadata, credits, iptc_of, raw_credits};
use crate::session::places::Listings;
use crate::session::{AutoApplyPreset, read_meta};

/// Start of the error message when the card in the reader is not the one
/// scanned (the app then scans again; two cards are never mixed in one run).
pub const CARD_CHANGED: &str = "the card changed since it was scanned";
/// Start of the error message when the destination can't hold the import.
pub const NOT_ENOUGH_SPACE: &str = "not enough free space";
/// Kept free beyond an import's bytes (file-system metadata, journal, sidecars).
const SPACE_MARGIN: u64 = 16 << 20;

impl From<ImportError> for DsError {
    fn from(e: ImportError) -> Self {
        let message = e.to_string();
        match e {
            ImportError::Template(_) => Self::InvalidArgument { message },
            ImportError::Busy(_) | ImportError::JournalVersion { .. } => {
                Self::Unsupported { message }
            }
            ImportError::Io { .. } | ImportError::Journal(_) => Self::Io { message },
        }
    }
}

/// What a card file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ImportFileKind {
    Raw,
    Jpeg,
    Heif,
    /// Imported as a non-editable item.
    Video,
    Xmp,
    VideoSidecar,
}

impl From<FileKind> for ImportFileKind {
    fn from(k: FileKind) -> Self {
        match k {
            FileKind::Raw => Self::Raw,
            FileKind::Jpeg => Self::Jpeg,
            FileKind::Heif => Self::Heif,
            FileKind::Video => Self::Video,
            FileKind::Xmp => Self::Xmp,
            FileKind::VideoSidecar => Self::VideoSidecar,
        }
    }
}

/// One photo or clip on the card (its primary file plus sidecars).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImportCandidateRecord {
    pub path: String,
    pub size: u64,
    /// File modification time, ms since the Unix epoch.
    pub mtime_ms: i64,
    pub kind: ImportFileKind,
    /// Sidecar files that travel with it (JPG of a RAW+JPG pair, XMP, THM).
    pub sidecars: Vec<String>,
    /// Bytes of the primary and its sidecars.
    pub total_bytes: u64,
    /// The library already holds an identical, verified copy.
    pub already_imported: bool,
    /// The library has a copy of this file that no longer matches its
    /// recorded hash (damaged): the file is imported again, and the user told.
    pub damaged_library_copy: Option<String>,
}

/// A path and why it is listed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CardIssue {
    pub path: String,
    pub reason: String,
}

/// What [`scan_card`] found.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CardScanSummary {
    pub root: String,
    /// The card's identity: the volume UUID (or `mount:<path>` if unknown).
    pub volume_id: String,
    pub candidates: Vec<ImportCandidateRecord>,
    /// Parts of the card that could not be read: an import can't be complete.
    pub scan_errors: Vec<CardIssue>,
    /// Card files deliberately not imported (unknown types, orphan sidecars…).
    pub skipped_unknown: Vec<CardIssue>,
    pub total_bytes: u64,
}

/// The user's choices from the import sheet.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImportOptions {
    /// Session folder: created (must not exist or be empty) or opened. A
    /// folder that already is a session (an interrupted import) is opened.
    pub session_dir: String,
    pub session_name: String,
    pub create_session: bool,
    /// Primary paths to import; `None` imports every candidate.
    pub include: Option<Vec<String>>,
    /// Subfolder template under `Capture/`, e.g. `{date:%Y-%m-%d}`.
    pub subfolder: Option<String>,
    /// Rename template, e.g. `{date}_{seq:4}`; `None` keeps names.
    pub rename: Option<String>,
    /// Second verified copy, written in the same pass.
    pub backup_dest: Option<String>,
    /// Re-read the card after copying (catches a flaky reader; doubles reads).
    pub verify_source_reread: bool,
    /// Skip files the library already holds (reported, never copied).
    pub skip_duplicates: bool,
    /// Global index for dedupe and for recording the session (`None`: neither).
    pub global_index: Option<String>,
    /// The session's auto-apply preset; `None` clears it (photos only, never clips).
    pub auto_apply: Option<AutoApplyPreset>,
    /// A metadata preset (creator, copyright…) set on every imported photo, over
    /// what the files say (photos only, never clips).
    #[uniffi(default = None)]
    pub metadata_preset: Option<PhotoMetadata>,
    /// Debug/test: sleep this long before each card read (0 = full speed).
    pub throttle_ms: u32,
}

/// One copied file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImportedRecord {
    pub src: String,
    pub dest: String,
    pub backup: Option<String>,
    pub size: u64,
    pub kind: ImportFileKind,
    /// Verified by an earlier, interrupted run (not copied again).
    pub resumed: bool,
}

/// The outcome of [`CardScan::import`]. `safe_to_remove` is the only thing
/// that may lead to "safe to remove" wording; `card_only` lists exactly which
/// media files stay only on the card.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
#[allow(clippy::struct_excessive_bools)] // a flat FFI record
pub struct ImportResult {
    pub session_dir: String,
    pub session_name: String,
    /// The card this run read (see [`CardScanSummary::volume_id`]).
    pub volume_id: String,
    pub files_planned: u32,
    /// Copied (or resumed), verified, durable and registered.
    pub copied: Vec<ImportedRecord>,
    pub skipped_duplicates: Vec<String>,
    pub failed: Vec<CardIssue>,
    pub backup_failed: Vec<CardIssue>,
    /// Torn copies from an earlier run, set aside (never deleted).
    pub quarantined: Vec<String>,
    pub cancelled: bool,
    pub source_lost: Option<String>,
    pub durability_error: Option<String>,
    pub scan_errors: Vec<CardIssue>,
    pub skipped_unknown: Vec<CardIssue>,
    pub bytes_copied: u64,
    pub elapsed_ms: u64,
    /// lt-import's `is_complete` for this run's plan.
    pub is_complete: bool,
    pub eject_needs_confirmation: bool,
    /// Photos/clips added to the session by this run.
    pub registered: u32,
    /// Copied and verified, but the session could not add them: not imported
    /// (the journal stays; opening the session or importing again adds them).
    pub register_errors: Vec<CardIssue>,
    /// Media files on the card with no verified copy in the library (not
    /// selected, failed, not registered, stopped before they were copied…).
    pub card_only: Vec<CardIssue>,
    /// Every media file on the card has a verified library copy and nothing
    /// else stays only on the card: the card may be called safe to remove.
    pub safe_to_remove: bool,
    /// Card files whose library copy was damaged, imported again (path → reason).
    pub damaged_library_copies: Vec<CardIssue>,
    /// Recording the session in the global index failed (dedupe won't see it).
    pub index_error: Option<String>,
}

/// An unfinished import (a journal in `<session>/Capture`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PendingImport {
    pub session_dir: String,
    pub done: u32,
    pub pending: u32,
    pub failed: u32,
    /// The card(s) that resume it (volume ids, as in [`CardScanSummary::volume_id`]).
    pub volumes: Vec<String>,
    /// Backup roots it was writing to.
    pub backup_dests: Vec<String>,
}

/// Camera metadata of a card file, as the import's templates see it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CardFileMeta {
    pub camera: Option<String>,
    /// Capture time, the camera clock as UTC (like `capture_time_ms`).
    pub capture_ms: Option<i64>,
}

fn issue(path: &Path, reason: &str) -> CardIssue {
    CardIssue {
        path: path.to_string_lossy().into_owned(),
        reason: reason.to_owned(),
    }
}

fn ms(t: std::time::SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The identity of the volume holding `root`, as lt-import's journals key it.
fn volume_id_of(root: &Path) -> String {
    let mount = lt_fsutil::mount_point_of(root).unwrap_or_else(|_| PathBuf::from("/"));
    lt_fsutil::volume_uuid(&mount).unwrap_or_else(|| format!("mount:{}", mount.display()))
}

/// Free bytes on the volume holding `path` (its nearest existing ancestor).
fn free_bytes_at(path: &Path) -> Option<u64> {
    let mut p = path;
    loop {
        if p.exists() {
            let st = rustix::fs::statvfs(p).ok()?;
            return Some(st.f_bavail.saturating_mul(st.f_frsize));
        }
        p = p.parent()?;
    }
}

/// Camera-database/index files (`MEDIAPRO.XML`, `AVIN0001.BNP`…) are not media.
fn is_camera_database(i: &CardIssue) -> bool {
    i.reason.contains("camera database")
}

// ------------------------------------------------------------------ dedupe ---

/// [`DedupeLookup`] over the global index: sizes by `stat` of every recorded
/// original, partial hashes computed lazily (only for same-size files) and
/// cached. `find_duplicate` then re-hashes both copies in full, so a stale
/// index entry can never hide a card file.
pub(crate) struct GlobalDedupe {
    by_size: HashMap<u64, Vec<(PathBuf, FileHash)>>,
    partials: Mutex<HashMap<PathBuf, Option<PartialHash>>>,
}

impl GlobalDedupe {
    pub(crate) fn open(index: &Path) -> Result<Self, DsError> {
        let g = GlobalIndex::open(index)?;
        Ok(Self::from_entries(g.originals()?))
    }

    fn from_entries(entries: Vec<(PathBuf, String, FileHash)>) -> Self {
        let mut by_size: HashMap<u64, Vec<(PathBuf, FileHash)>> = HashMap::new();
        for (root, rel, hash) in entries {
            let path = root.join(rel);
            if let Ok(m) = fs::metadata(&path)
                && m.is_file()
            {
                by_size.entry(m.len()).or_default().push((path, hash));
            }
        }
        Self {
            by_size,
            partials: Mutex::default(),
        }
    }

    /// A library copy recorded with this card file's content that no longer
    /// re-hashes to it (bit rot, a truncated or replaced file).
    fn damaged_copy(&self, path: &Path, size: u64) -> Option<PathBuf> {
        let partial = partial_hash(path).ok()?;
        let known = self.matches(size, &partial);
        if known.is_empty() {
            return None;
        }
        let card = full_hash(path).ok()?;
        known
            .into_iter()
            .filter(|k| k.hash == card)
            .filter_map(|k| k.path)
            .find(|p| full_hash(p).map_or(true, |h| h != card))
    }
}

impl DedupeLookup for GlobalDedupe {
    fn has_size(&self, size: u64) -> bool {
        self.by_size.contains_key(&size)
    }

    fn matches(&self, size: u64, partial: &PartialHash) -> Vec<KnownCopy> {
        let Some(known) = self.by_size.get(&size) else {
            return Vec::new();
        };
        let mut cache = self.partials.lock().unwrap_or_else(PoisonError::into_inner);
        known
            .iter()
            .filter(|(path, _)| {
                cache
                    .entry(path.clone())
                    .or_insert_with(|| partial_hash(path).ok())
                    .as_ref()
                    == Some(partial)
            })
            .map(|(path, hash)| KnownCopy {
                hash: *hash,
                path: Some(path.clone()),
            })
            .collect()
    }
}

// -------------------------------------------------------------------- scan ---

/// A scanned card, ready to import.
#[derive(Debug, uniffi::Object)]
pub struct CardScan {
    root: PathBuf,
    volume_id: String,
    report: ScanReport,
    duplicates: HashSet<PathBuf>,
    /// Card path → the damaged library copy found for it.
    damaged: HashMap<PathBuf, PathBuf>,
}

/// Scan `root` (a mounted card or any folder) read-only. With
/// `global_index`, candidates the library already holds are marked
/// `already_imported` (an unreadable index just means no marks).
#[uniffi::export]
pub async fn scan_card(
    root: String,
    global_index: Option<String>,
    token: Arc<CancelToken>,
) -> Result<Arc<CardScan>, DsError> {
    run_job_on_thread("lt-import-scan", token, move |cancelled| {
        scan_blocking(
            Path::new(&root),
            global_index.as_deref().map(Path::new),
            cancelled,
        )
        .map(Arc::new)
    })
    .await
}

fn scan_blocking(
    root: &Path,
    global_index: Option<&Path>,
    cancelled: &dyn Fn() -> bool,
) -> Result<CardScan, DsError> {
    let report = lt_import::scan_volume(root)?;
    let volume_id = volume_id_of(root);
    let dedupe = global_index.and_then(|p| GlobalDedupe::open(p).ok());
    let mut duplicates = HashSet::new();
    let mut damaged = HashMap::new();
    if let Some(d) = &dedupe {
        for c in &report.candidates {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            if let Ok(Some(_)) = find_duplicate(&c.primary.path, c.primary.size, d, &[]) {
                duplicates.insert(c.primary.path.clone());
            } else if let Some(bad) = d.damaged_copy(&c.primary.path, c.primary.size) {
                damaged.insert(c.primary.path.clone(), bad);
            }
        }
    }
    Ok(CardScan {
        root: root.to_path_buf(),
        volume_id,
        report,
        duplicates,
        damaged,
    })
}

impl CardScan {
    fn summary_record(&self) -> CardScanSummary {
        let candidates: Vec<ImportCandidateRecord> = self
            .report
            .candidates
            .iter()
            .map(|c| ImportCandidateRecord {
                path: c.primary.path.to_string_lossy().into_owned(),
                size: c.primary.size,
                mtime_ms: ms(c.primary.mtime),
                kind: c.primary.kind.into(),
                sidecars: c
                    .sidecars
                    .iter()
                    .map(|s| s.path.to_string_lossy().into_owned())
                    .collect(),
                total_bytes: c.total_bytes(),
                already_imported: self.duplicates.contains(&c.primary.path),
                damaged_library_copy: self
                    .damaged
                    .get(&c.primary.path)
                    .map(|p| p.to_string_lossy().into_owned()),
            })
            .collect();
        CardScanSummary {
            root: self.root.to_string_lossy().into_owned(),
            volume_id: self.volume_id.clone(),
            total_bytes: candidates.iter().map(|c| c.total_bytes).sum(),
            candidates,
            scan_errors: self
                .report
                .errors
                .iter()
                .map(|e| issue(&e.path, &e.reason))
                .collect(),
            skipped_unknown: self
                .report
                .skipped_unknown
                .iter()
                .map(|s| issue(&s.path, &s.reason))
                .collect(),
        }
    }

    /// The scan with only the chosen candidates (errors and skipped files
    /// stay: they are about the whole card).
    fn selected(&self, include: Option<&[String]>) -> ScanReport {
        let mut r = self.report.clone();
        if let Some(inc) = include {
            let keep: HashSet<&str> = inc.iter().map(String::as_str).collect();
            r.candidates
                .retain(|c| keep.contains(c.primary.path.to_string_lossy().as_ref()));
        }
        r
    }

    /// The card in the reader is still the one scanned: same volume, and
    /// every file of `selected` with its scanned size and mtime.
    fn check_same_card(&self, selected: &ScanReport) -> Result<(), DsError> {
        let changed = |why: String| DsError::InvalidArgument {
            message: format!("{CARD_CHANGED}: {why}"),
        };
        let now = volume_id_of(&self.root);
        if now != self.volume_id {
            return Err(changed(format!(
                "another card is in the reader ({now}, scanned {})",
                self.volume_id
            )));
        }
        for f in selected
            .candidates
            .iter()
            .flat_map(lt_import::Candidate::files)
        {
            let same = fs::symlink_metadata(&f.path)
                .is_ok_and(|m| m.len() == f.size && m.modified().is_ok_and(|t| t == f.mtime));
            if !same {
                let name = f.path.file_name().unwrap_or_default().to_string_lossy();
                return Err(changed(format!("{name} is not the file that was scanned")));
            }
        }
        Ok(())
    }
}

#[uniffi::export]
impl CardScan {
    /// What the scan found (for the import sheet).
    pub fn summary(&self) -> Result<CardScanSummary, DsError> {
        guard(|| Ok(self.summary_record()))
    }

    /// Copy the chosen candidates into the session (see the module docs).
    /// Progress: `"import"` (bytes) and `"import.files"` (files).
    pub async fn import(
        self: Arc<Self>,
        options: ImportOptions,
        listener: Option<Arc<dyn ProgressListener>>,
        token: Arc<CancelToken>,
    ) -> Result<ImportResult, DsError> {
        let events = Arc::new(Dispatcher::new());
        events.set_listener(listener);
        let ev = events.clone();
        let watch = token.clone();
        let r = run_job_on_thread("lt-import", token, move |_| {
            run_import(&self, &options, &ev, &watch)
        })
        .await;
        events.flush(Duration::from_secs(5));
        r
    }
}

// ------------------------------------------------------------------ import ---

/// A session, in the current layout or one from before the rename (`Session::open` moves it).
fn is_session(dir: &Path) -> bool {
    lt_session::is_session_dir(dir)
}

fn open_destination(o: &ImportOptions) -> Result<Session, DsError> {
    let dir = Path::new(&o.session_dir);
    // An interrupted import already created it: resume into it.
    let mut s = if o.create_session && !is_session(dir) {
        Session::create(dir, &o.session_name)?
    } else {
        Session::open(dir)?
    };
    if s.is_external() {
        return Err(DsError::Unsupported {
            message: format!(
                "{} is a folder opened in place; import into a session instead",
                dir.display()
            ),
        });
    }
    // the session's preset follows the setting (off clears it).
    let preset = o
        .auto_apply
        .as_ref()
        .map(AutoApplyPreset::to_preset)
        .transpose()?;
    s.set_auto_apply_preset(preset)?;
    s.ensure_builtin_albums()?;
    Ok(s)
}

/// Slow card reads for demos, screenshots and cancel tests.
struct Throttle(Duration);

impl FaultInjector for Throttle {
    fn before_read(&self, _src: &Path, _offset: u64) -> std::io::Result<()> {
        std::thread::sleep(self.0);
        Ok(())
    }
}

/// Registers each verified primary file in the session as it lands.
struct Register<'a> {
    session: &'a Mutex<Session>,
    root: PathBuf,
    primaries: HashSet<PathBuf>,
    registered: AtomicU32,
    preset: Option<IptcMeta>,
    /// Card folders listed for XMP sidecars (each once per import).
    dirs: Mutex<Listings>,
}

/// Session metadata for a copied file: from the RAW when it parses, else
/// (JPEG/HEIF the RAW reader doesn't take, videos) the file time; and the IPTC
/// fields the file already has (camera credits, its own XMP, then the XMP sidecar
/// beside the card's `src`, which is always there, unlike its copy; none for clips).
/// `src`'s folder must be listed in `dirs`.
fn meta_for(src: &Path, dest: &Path, kind: FileKind, dirs: &Listings) -> (ImageMetaLite, IptcMeta) {
    if kind == FileKind::Video {
        return (mtime_meta(src, dest), IptcMeta::default());
    }
    let (lite, camera) = if let Some((lite, raw)) = read_meta(dest) {
        (lite, raw_credits(&raw))
    } else {
        // JPEG/HEIF: the credits from its own EXIF.
        let e = crate::export::exif_block(dest).map(|b| b.meta);
        let camera = e.as_ref().map_or_else(IptcMeta::default, |m| {
            credits(m.artist.as_deref(), m.copyright.as_deref())
        });
        (mtime_meta(src, dest), camera)
    };
    (lite, iptc_of(src, dest, camera, dirs))
}

/// What a copied file is registered with: its metadata, the IPTC fields it
/// already has, and the import's metadata preset over them (photos only).
fn prepare(
    src: &Path,
    dest: &Path,
    kind: FileKind,
    preset: Option<&IptcMeta>,
    dirs: &Listings,
) -> (ImageMetaLite, IptcMeta) {
    let (meta, mut iptc) = meta_for(src, dest, kind, dirs);
    if let Some(p) = preset.filter(|_| kind != FileKind::Video) {
        iptc = iptc.overlaid(p);
    }
    (meta, iptc)
}

fn mtime_meta(src: &Path, dest: &Path) -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: fs::metadata(src)
            .or_else(|_| fs::metadata(dest))
            .and_then(|m| m.modified())
            .ok()
            .map(ms),
        ..ImageMetaLite::default()
    }
}

/// Add one original to `session` (clips without the auto-apply or metadata
/// preset). A file the session already has counts as added.
fn register(
    session: &mut Session,
    root: &Path,
    dest: &Path,
    kind: FileKind,
    hash: FileHash,
    (meta, iptc): (ImageMetaLite, IptcMeta),
) -> Result<bool, String> {
    let rel = dest
        .strip_prefix(root)
        .map_err(|_| "copied outside the session".to_owned())?
        .to_string_lossy()
        .into_owned();
    let item = NewOriginal {
        rel,
        hash,
        content_hash: None,
        meta,
        replaced: Vec::new(),
        iptc,
    };
    match session.add_new_originals_with(vec![item], kind != FileKind::Video) {
        Ok(_) => Ok(true),
        // A resumed run, or recovered on open: already in the session.
        Err(SessionError::DuplicatePath(_)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

impl SessionSink for Register<'_> {
    fn imported(&self, f: &ImportedFile) -> Result<(), String> {
        if !self.primaries.contains(&f.src) {
            return Ok(()); // sidecars travel with their photo; not photos themselves
        }
        // Read the files before taking the session lock.
        let prepared = {
            let mut dirs = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
            dirs.prefill([f.src.as_path()]);
            prepare(&f.src, &f.dest, f.kind, self.preset.as_ref(), &dirs)
        };
        let mut s = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        if register(&mut s, &self.root, &f.dest, f.kind, f.hash, prepared)? {
            self.registered.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn emit(ev: &Dispatcher, kind: &str, phase: TaskPhase, done: u64, total: u64) {
    ev.emit(ProgressEvent {
        task_id: 1,
        kind: kind.into(),
        phase,
        completed: done.min(total),
        total,
    });
}

/// Journal entries from earlier runs into `capture` (any card).
fn prior_entries(capture: &Path) -> HashMap<SourceKey, EntryState> {
    Journal::load_all(capture)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|(_, j)| j.entries)
        .map(|e| (e.key, e.state))
        .collect()
}

/// Files an earlier run already verified must still be the same bytes on
/// the card (their journal's partial hash): the same card, not a look-alike.
fn check_against_journal(
    plan: &ImportPlan,
    prior: &HashMap<SourceKey, EntryState>,
) -> Result<(), DsError> {
    for f in plan.items.iter().flat_map(|i| &i.files) {
        if let Some(EntryState::Done { partial, .. }) = prior.get(&f.key)
            && let Some(want) = FileHash::from_hex(partial)
        {
            let got = partial_hash(&f.src.path).ok().map(|p| p.0);
            if got != Some(want.0) {
                let name = f.src.path.file_name().unwrap_or_default().to_string_lossy();
                return Err(DsError::InvalidArgument {
                    message: format!(
                        "{CARD_CHANGED}: {name} differs from the copy an earlier run of this import made"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Bytes this plan still has to write (files not already done by an earlier
/// run), checked against free space before anything is created.
fn preflight(
    plan: &ImportPlan,
    prior: &HashMap<SourceKey, EntryState>,
    session_dir: &Path,
    backup: Option<&Path>,
) -> Result<(), DsError> {
    let need: u64 = plan
        .items
        .iter()
        .flat_map(|i| &i.files)
        .filter(|f| {
            !matches!(
                prior.get(&f.key),
                Some(EntryState::Done { .. } | EntryState::Duplicate { .. })
            )
        })
        .map(|f| f.src.size)
        .sum();
    for root in std::iter::once(session_dir).chain(backup) {
        if let Some(free) = free_bytes_at(root)
            && free < need.saturating_add(SPACE_MARGIN)
        {
            return Err(DsError::Io {
                message: format!(
                    "{NOT_ENOUGH_SPACE} on the volume of {}: this import needs {need} bytes, {free} are free",
                    root.display()
                ),
            });
        }
    }
    Ok(())
}

/// What the import left only on the card, from a fresh scan of the whole
/// card: each media file (primary or sidecar) is covered by a verified,
/// registered copy from this run, a verified duplicate this run found, or a
/// library copy re-verified now (full hash of both). Anything else is listed.
fn card_only(
    scan: &CardScan,
    report: &lt_import::ImportReport,
    included: &HashSet<PathBuf>,
    index: Option<&Path>,
) -> (Vec<CardIssue>, ScanReport) {
    let fresh = lt_import::scan_volume(&scan.root).unwrap_or_else(|_| scan.report.clone());
    let copied: HashSet<&PathBuf> = report.copied.iter().map(|c| &c.src).collect();
    let dups: HashSet<&PathBuf> = report.skipped_duplicates.iter().map(|d| &d.src).collect();
    let failed: HashMap<&PathBuf, &str> = report
        .failed
        .iter()
        .map(|f| (&f.src, f.reason.as_str()))
        .collect();
    let dedupe = index.and_then(|p| GlobalDedupe::open(p).ok());
    let mut out = Vec::new();
    for f in fresh
        .candidates
        .iter()
        .flat_map(lt_import::Candidate::files)
    {
        if copied.contains(&f.path) || dups.contains(&f.path) {
            continue;
        }
        let in_library = dedupe
            .as_ref()
            .is_some_and(|d| matches!(find_duplicate(&f.path, f.size, d, &[]), Ok(Some(_))));
        if in_library {
            continue;
        }
        let reason = if let Some(r) = failed.get(&f.path) {
            (*r).to_owned()
        } else if !included.contains(&f.path) {
            "not selected for import".to_owned()
        } else if report.cancelled || report.source_lost.is_some() {
            "not copied: the import stopped".to_owned()
        } else {
            "not copied".to_owned()
        };
        out.push(issue(&f.path, &reason));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    (out, fresh)
}

#[allow(clippy::too_many_lines)]
fn run_import(
    scan: &CardScan,
    o: &ImportOptions,
    ev: &Dispatcher,
    token: &Arc<CancelToken>,
) -> Result<ImportResult, DsError> {
    let selected = scan.selected(o.include.as_deref());
    // never mix two cards in one run.
    scan.check_same_card(&selected)?;
    let session_dir = PathBuf::from(&o.session_dir);
    let capture = session_dir.join("Capture");
    let mut opts = PlanOptions::new(&capture);
    opts.subfolder = o.subfolder.clone().filter(|s| !s.trim().is_empty());
    opts.rename = o.rename.clone().filter(|s| !s.trim().is_empty());
    opts.backup_dest = o.backup_dest.as_ref().map(PathBuf::from);
    opts.verify_source_reread = o.verify_source_reread;
    if o.skip_duplicates
        && let Some(p) = &o.global_index
    {
        opts.dedupe = GlobalDedupe::open(Path::new(p))
            .ok()
            .map(|d| Arc::new(d) as Arc<dyn DedupeLookup>);
    }
    let plan = ImportPlan::from_scan(&selected, opts, &lt_import::raw::RawMeta)?;
    let prior = prior_entries(&capture);
    check_against_journal(&plan, &prior)?;
    // nothing is created when the import can't fit.
    preflight(&plan, &prior, &session_dir, plan.backup_dest.as_deref())?;
    let session = open_destination(o)?;
    let root = session.root().to_path_buf();
    let primaries: HashSet<PathBuf> = plan
        .items
        .iter()
        .filter_map(|i| i.files.first().map(|f| f.src.path.clone()))
        .collect();
    let included: HashSet<PathBuf> = plan
        .items
        .iter()
        .flat_map(|i| &i.files)
        .map(|f| f.src.path.clone())
        .collect();
    let session = Mutex::new(session);
    let sink = Register {
        session: &session,
        root: root.clone(),
        primaries,
        registered: AtomicU32::new(0),
        preset: o.metadata_preset.clone().map(IptcMeta::from),
        dirs: Mutex::new(Listings::default()),
    };
    let throttle = Throttle(Duration::from_millis(u64::from(o.throttle_ms)));
    let hooks = RunHooks {
        sink: Some(&sink),
        faults: (o.throttle_ms > 0).then_some(&throttle as &dyn FaultInjector),
    };

    // Bridge the token to lt-import's flag (it polls between chunks).
    let cancel = Arc::new(AtomicBool::new(token.is_cancelled()));
    let finished = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (cancel, finished, token) = (cancel.clone(), finished.clone(), token.clone());
        std::thread::Builder::new()
            .name("lt-import-cancel".into())
            .spawn(move || {
                while !finished.load(Ordering::Acquire) {
                    if token.is_cancelled() {
                        cancel.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
            .ok()
    };
    let (bytes_total, files_total) = (plan.total_bytes(), plan.file_count() as u64);
    emit(ev, "import", TaskPhase::Started, 0, bytes_total);
    let progress = |p: &lt_import::ImportProgress| {
        emit(
            ev,
            "import",
            TaskPhase::Running,
            p.bytes_done,
            p.bytes_total,
        );
        emit(
            ev,
            "import.files",
            TaskPhase::Running,
            p.files_done as u64,
            p.files_total as u64,
        );
    };
    let run = lt_import::run_with(&plan, hooks, &progress, &cancel);
    finished.store(true, Ordering::Release);
    if let Some(w) = watcher {
        let _ = w.join();
    }
    let report = match run {
        Ok(r) => r,
        Err(e) => {
            emit(ev, "import", TaskPhase::Failed, 0, bytes_total);
            return Err(e.into());
        }
    };
    emit(
        ev,
        "import.files",
        TaskPhase::Running,
        files_total,
        files_total,
    );
    let phase = if report.cancelled {
        TaskPhase::Cancelled
    } else if report.is_complete() {
        TaskPhase::Finished
    } else {
        TaskPhase::Failed
    };
    emit(ev, "import", phase, report.bytes_copied, bytes_total);

    let registered = sink.registered.load(Ordering::Relaxed);
    let session = session.into_inner().unwrap_or_else(PoisonError::into_inner);
    let index_error = o.global_index.as_ref().and_then(|p| {
        GlobalIndex::open(Path::new(p))
            .and_then(|mut g| g.sync_session(&session))
            .err()
            .map(|e| e.to_string())
    });
    let (card_only, fresh) = card_only(
        scan,
        &report,
        &included,
        o.global_index.as_deref().map(Path::new),
    );
    let (register_errors, failed): (Vec<_>, Vec<_>) = report
        .failed
        .iter()
        .partition(|f| f.reason.starts_with(NOT_REGISTERED));
    let fresh_skipped: Vec<CardIssue> = fresh
        .skipped_unknown
        .iter()
        .map(|s| issue(&s.path, &s.reason))
        .collect();
    let safe_to_remove = card_only.is_empty()
        && fresh.errors.is_empty()
        && report.scan_errors.is_empty()
        && fresh_skipped.iter().all(is_camera_database)
        && !report.cancelled
        && report.source_lost.is_none()
        && report.durability_error.is_none();
    let damaged_library_copies = report
        .copied
        .iter()
        .filter_map(|c| {
            scan.damaged.get(&c.src).map(|bad| {
                issue(
                    &c.src,
                    &format!(
                        "the library copy {} was damaged; imported again",
                        bad.display()
                    ),
                )
            })
        })
        .collect();
    Ok(ImportResult {
        session_dir: root.to_string_lossy().into_owned(),
        session_name: session.name().to_owned(),
        volume_id: scan.volume_id.clone(),
        files_planned: count(plan.file_count()),
        copied: report
            .copied
            .iter()
            .map(|f| ImportedRecord {
                src: f.src.to_string_lossy().into_owned(),
                dest: f.dest.to_string_lossy().into_owned(),
                backup: f.backup.as_ref().map(|b| b.to_string_lossy().into_owned()),
                size: f.size,
                kind: f.kind.into(),
                resumed: f.resumed,
            })
            .collect(),
        skipped_duplicates: report
            .skipped_duplicates
            .iter()
            .map(|d| d.src.to_string_lossy().into_owned())
            .collect(),
        failed: failed.iter().map(|f| issue(&f.src, &f.reason)).collect(),
        backup_failed: report
            .backup_failed
            .iter()
            .map(|f| issue(&f.src, &f.reason))
            .collect(),
        quarantined: report
            .quarantined
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        cancelled: report.cancelled,
        source_lost: report.source_lost.clone(),
        durability_error: report.durability_error.clone(),
        scan_errors: report
            .scan_errors
            .iter()
            .map(|e| issue(&e.path, &e.reason))
            .collect(),
        skipped_unknown: report
            .skipped_unknown
            .iter()
            .map(|s| issue(&s.path, &s.reason))
            .collect(),
        bytes_copied: report.bytes_copied,
        elapsed_ms: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        is_complete: report.is_complete(),
        eject_needs_confirmation: report.eject_needs_confirmation(),
        registered,
        register_errors: register_errors
            .iter()
            .map(|f| {
                let why = f
                    .reason
                    .strip_prefix(NOT_REGISTERED)
                    .map_or(f.reason.as_str(), |r| r.trim_start_matches(": "));
                issue(&f.src, why)
            })
            .collect(),
        card_only,
        safe_to_remove,
        damaged_library_copies,
        index_error,
    })
}

// -------------------------------------------------------------- recovery ---

/// What [`recover_unregistered`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub added: u64,
    pub failed: Vec<String>,
}

/// Register originals an import copied into `Capture/` but could not add
/// to the session (the sink failed, or the app died between copy and
/// registration). Grouped like a card (a RAW+JPG pair is one photo; sidecars
/// and dot-files such as `.lt/` journals and `.partial`s are never photos),
/// hashed in full, and added (clips without the auto-apply preset).
pub(crate) fn recover_unregistered(session: &mut Session) -> Recovered {
    let mut out = Recovered::default();
    if session.is_external() {
        return out;
    }
    let root = session.root().to_path_buf();
    let capture = root.join("Capture");
    if !capture.is_dir() {
        return out;
    }
    let Ok(scan) = lt_import::scan_volume(&capture) else {
        return out;
    };
    let Ok(images) = session.images() else {
        return out;
    };
    let known: HashSet<String> = images
        .iter()
        .map(|i| i.rel_path.as_str().to_owned())
        .collect();
    let mut dirs = Listings::default();
    for c in &scan.candidates {
        let p = &c.primary.path;
        let Ok(rel) = p.strip_prefix(&root) else {
            continue;
        };
        if known.contains(rel.to_string_lossy().as_ref()) {
            continue;
        }
        let r = full_hash(p).map_err(|e| e.to_string()).and_then(|h| {
            dirs.prefill([p.as_path()]);
            let prepared = prepare(p, p, c.primary.kind, None, &dirs);
            register(session, &root, p, c.primary.kind, h, prepared)
        });
        match r {
            Ok(true) => out.added += 1,
            Ok(false) => {}
            Err(e) => out.failed.push(format!("{}: {e}", rel.to_string_lossy())),
        }
    }
    out
}

// ------------------------------------------------------------------ resume ---

fn pending_in(session_dir: &Path) -> Result<Option<PendingImport>, DsError> {
    Ok(
        lt_import::pending_resume(&session_dir.join("Capture"))?.map(|i| PendingImport {
            session_dir: session_dir.to_string_lossy().into_owned(),
            done: count(i.done),
            pending: count(i.pending),
            failed: count(i.failed),
            volumes: i.volumes,
            backup_dests: i
                .backup_dests
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        }),
    )
}

/// Unfinished imports into sessions under each of `roots` (one level down)
/// and in each of `sessions`, for the "resume import?" prompt and for
/// resuming a re-inserted card. Duplicates and unreadable folders are skipped.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn pending_imports(
    roots: Vec<String>,
    sessions: Vec<String>,
) -> Result<Vec<PendingImport>, DsError> {
    guard(|| {
        let mut seen = HashSet::new();
        let mut dirs: Vec<PathBuf> = roots
            .iter()
            .filter_map(|r| fs::read_dir(r).ok())
            .flat_map(|rd| rd.flatten().map(|e| e.path()))
            .chain(sessions.iter().map(PathBuf::from))
            .filter(|p| p.join("Capture").is_dir())
            .filter(|p| seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
            .collect();
        dirs.sort();
        Ok(dirs
            .iter()
            .filter_map(|d| pending_in(d).ok().flatten())
            .collect())
    })
}

/// The unfinished import into one session, if any.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn pending_import(session_dir: String) -> Result<Option<PendingImport>, DsError> {
    guard(|| pending_in(Path::new(&session_dir)))
}

/// After the app's support folder was copied to its new name (Darkslide → Luxtable):
/// point the sessions the global index at `index` registered under `from` (stores the
/// app keeps for folders that can't hold their own) at the same place under `to`.
/// Returns how many moved. A missing index is nothing to do.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn relocate_global_index(index: String, from: String, to: String) -> Result<u64, DsError> {
    guard(|| {
        let path = Path::new(&index);
        if !path.is_file() {
            return Ok(0);
        }
        let mut g = GlobalIndex::open(path)?;
        Ok(g.relocate_roots(Path::new(&from), Path::new(&to))? as u64)
    })
}

/// Abandon an unfinished import: removes its journals and `.partial` files
/// (in the session, in `backup_dest` and in the backup root the journal
/// recorded) only, never a finished copy, never anything on the card.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn discard_pending_import(
    session_dir: String,
    backup_dest: Option<String>,
) -> Result<(), DsError> {
    guard(|| {
        lt_import::discard_journal(
            &Path::new(&session_dir).join("Capture"),
            backup_dest.as_deref().map(Path::new),
        )?;
        Ok(())
    })
}

/// Free bytes on the volume holding `path` (or its nearest existing
/// ancestor); `None` when unknown. For the import sheet's space line.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn free_space(path: String) -> Option<u64> {
    free_bytes_at(Path::new(&path))
}

/// Camera and capture time of a card file as import's templates see them
/// (the RAW's EXIF; empty for other files), for the rename preview.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn card_file_meta(path: String) -> CardFileMeta {
    // The RAW parser can panic on a damaged file; this export is not `async`
    // and must not unwind into Swift, so such a file simply has no metadata.
    let m = std::panic::catch_unwind(|| lt_import::raw::RawMeta.capture_meta(Path::new(&path)));
    match m {
        Ok(m) => CardFileMeta {
            camera: m.camera,
            capture_ms: m.captured_at.map(|t| t.and_utc().timestamp_millis()),
        },
        Err(_) => CardFileMeta {
            camera: None,
            capture_ms: None,
        },
    }
}

// --------------------------------------------------------------- templates ---

/// Render a rename (`subfolder: false`) or subfolder template for a sample
/// file, as import would. `capture_ms` is the camera clock as UTC (like
/// `capture_time_ms`). Errors name what is wrong with the template.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn render_import_template(
    template: String,
    file_name: String,
    capture_ms: Option<i64>,
    camera: Option<String>,
    seq: u64,
    subfolder: bool,
) -> Result<String, DsError> {
    guard(|| {
        let t = Template::parse(&template).map_err(|e| DsError::invalid(e.to_string()))?;
        let p = Path::new(&file_name);
        let name = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = p
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let vars = Vars {
            name: &name,
            seq,
            datetime: capture_ms
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| d.naive_utc()),
            camera: camera.as_deref(),
            ext: &ext,
        };
        if subfolder {
            let p = t
                .render_subfolder(&vars)
                .map_err(|e| DsError::invalid(e.to_string()))?;
            Ok(p.to_string_lossy().into_owned())
        } else {
            Ok(t.render_file_name(&vars))
        }
    })
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "import_review_tests.rs"]
mod review_tests;
