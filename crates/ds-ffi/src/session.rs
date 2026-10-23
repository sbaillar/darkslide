//! Sessions (`ds-session`) across the FFI: open a session or a plain folder
//! in place, list variants, write ratings/tags/picks, load and save
//! adjustments.
//!
//! **Sidecars are the source of truth.** Every write goes through
//! `ds_session::Session`, which writes the sidecar atomically and updates the
//! index; ids ([`VariantRecord::variant_id`]) are the sidecar's stable ids, so
//! they survive reopen, index rebuilds, and app restarts.
//!
//! **Open folder in place** (Capture One "open folder as session") uses a
//! session with *external originals* (`ds_session::Originals::External`): the
//! photos are the user's own files, which are only ever read. The session
//! (index + sidecars) lives in `<folder>/.ds/session/` when the folder's
//! volume is a local, writable APFS/HFS+ disk, and otherwise (memory cards
//! on FAT/exFAT, read-only media, network shares) in a directory the app
//! chooses, e.g. `~/Library/Application Support/Darkslide/InPlace/<volume
//! uuid>/<path hash>/`, with **zero writes to the volume**. The volume is
//! checked before anything is created; a failed first open removes what it
//! created. Old hard-link sessions (`.ds/session/Capture/` links) are
//! migrated on open: the links are removed after checking they are the same
//! inode as the original.
//!
//! Identity (see `ds_session::external`): an open only looks at `(size,
//! mtime, inode)` and never waits for hashing: renames are followed by stat,
//! files whose stat changed are kept provisionally, and files that could be
//! a missing photo are held back. [`SessionHandle::hash_pending`] then works
//! in the background (two utility-QoS threads, paused while the user is
//! active; see [`crate::background`]): a whole-file or sensor-data match keeps
//! everything (metadata-only rewrites by exiftool or geotaggers included), a
//! real change sets the old metadata aside ("file changed", persistent,
//! restorable with [`SessionHandle::restore_previous`]), held files are
//! relinked to missing photos by content (ambiguous duplicates are reported,
//! never guessed), and an unreadable file is simply retried on the next open.
//! Missing originals are listed with `missing = true`
//! ([`SessionHandle::remove_missing`], [`SessionHandle::locate`]).
//!
//! Adjustments cross the FFI as `ds_core::Adjustments` JSON (the sidecar
//! schema). Swift overlays the fields it models onto the JSON it read, so
//! fields it doesn't model are preserved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ds_core::{Adjustments, ColorTag, FileHash, PickState, Rating};
use ds_session::identity::{self, Hashes, Verdict};
use ds_session::{
    AlbumId, AlbumKind, Filter, Found, ImageId, ImageInfo, ImageMetaLite, NewOriginal, Preset,
    RelPath, Session, SessionError, Sort, ToVerify, UNHASHED, VariantId, VariantInfo, Verified,
    original_stat,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::DsError;
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::jobs::{CancelToken, guard, run_job};
use crate::raw::{GeoPosition, RawMetadata};

/// Where an in-place session lives inside the user's folder (local writable volumes).
pub const IN_PLACE_DIR: &str = ".ds/session";
/// Originals are registered in batches of this size (one index transaction each).
const ADD_BATCH: usize = 256;
/// Verified hashes recorded per index/sidecar batch by `hash_pending`.
const HASH_BATCH: usize = 32;
/// ... or at least this often while hashing runs.
const HASH_FLUSH: std::time::Duration = std::time::Duration::from_millis(250);
/// Metadata cache file under the session root (disposable).
const META_CACHE: &str = ".ds/cache/raw-meta.json";

impl From<SessionError> for DsError {
    fn from(e: SessionError) -> Self {
        let message = e.to_string();
        match e {
            SessionError::Io { .. } | SessionError::Sqlite(_) | SessionError::Trash(_) => {
                Self::Io { message }
            }
            SessionError::NotASession(_)
            | SessionError::SidecarTooNew { .. }
            | SessionError::NoMigration { .. }
            | SessionError::IndexTooNew { .. }
            | SessionError::ExternalOriginals(_)
            | SessionError::ExternalOnly(_) => Self::Unsupported { message },
            SessionError::Json { .. } => Self::Io { message },
            _ => Self::InvalidArgument { message },
        }
    }
}

/// Capture One color labels (all of `ds_core::ColorTag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ColorLabel {
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Pink,
    Purple,
}

impl From<ColorTag> for ColorLabel {
    fn from(t: ColorTag) -> Self {
        match t {
            ColorTag::None => Self::None,
            ColorTag::Red => Self::Red,
            ColorTag::Orange => Self::Orange,
            ColorTag::Yellow => Self::Yellow,
            ColorTag::Green => Self::Green,
            ColorTag::Blue => Self::Blue,
            ColorTag::Pink => Self::Pink,
            ColorTag::Purple => Self::Purple,
        }
    }
}

impl From<ColorLabel> for ColorTag {
    fn from(t: ColorLabel) -> Self {
        match t {
            ColorLabel::None => Self::None,
            ColorLabel::Red => Self::Red,
            ColorLabel::Orange => Self::Orange,
            ColorLabel::Yellow => Self::Yellow,
            ColorLabel::Green => Self::Green,
            ColorLabel::Blue => Self::Blue,
            ColorLabel::Pink => Self::Pink,
            ColorLabel::Purple => Self::Purple,
        }
    }
}

/// Pick flag (`ds_core::PickState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PickFlag {
    Unflagged,
    Picked,
    Rejected,
}

impl From<PickState> for PickFlag {
    fn from(p: PickState) -> Self {
        match p {
            PickState::Unflagged => Self::Unflagged,
            PickState::Picked => Self::Picked,
            PickState::Rejected => Self::Rejected,
        }
    }
}

impl From<PickFlag> for PickState {
    fn from(p: PickFlag) -> Self {
        match p {
            PickFlag::Unflagged => Self::Unflagged,
            PickFlag::Picked => Self::Picked,
            PickFlag::Rejected => Self::Rejected,
        }
    }
}

/// One variant as the browser needs it.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
#[allow(clippy::struct_excessive_bools)] // a flat FFI record
pub struct VariantRecord {
    /// Stable across reopen (the sidecar's id).
    pub variant_id: i64,
    pub image_id: i64,
    /// The user's file.
    pub original_path: String,
    pub file_name: String,
    /// Variant name ("" for the first variant).
    pub name: String,
    pub rating: u8,
    pub color_tag: ColorLabel,
    pub pick: PickFlag,
    pub edited: bool,
    /// The sidecar is from a newer build: show, don't write.
    pub read_only: bool,
    /// The original is gone (deleted in Finder, or not found on sync).
    pub missing: bool,
    /// The file at this path replaced a different photo: it started fresh (no
    /// metadata carried over). Persistent until restored or dismissed
    /// ([`SessionHandle::restore_previous`], [`SessionHandle::dismiss_file_changed`]).
    pub file_changed: bool,
    /// Missing from the folder, but its only copy is still inside the store
    /// (left there by the migration of an old session):
    /// [`SessionHandle::restore_to_folder`] puts it back.
    pub kept_in_store: bool,
    pub file_size: u64,
    /// Trailing digits of the file name (burst tie-breaker).
    pub file_sequence: Option<u64>,
    /// Camera clock as ms since the epoch (treated as UTC), from the sidecar.
    pub capture_time_ms: Option<i64>,
    /// Display-upright size from the sidecar.
    pub width: u32,
    pub height: u32,
    /// Full capture metadata (cached in the session; read once per file).
    pub meta: Option<RawMetadata>,
    /// In the session's `Trash/` folder (moved there by "Move Rejected to Trash";
    /// recoverable until "Delete Rejected…" empties it).
    #[uniffi(default = false)]
    pub in_trash: bool,
    /// In the session's `Selects/` folder.
    #[uniffi(default = false)]
    pub in_selects: bool,
    /// The variant's keywords (sidecar), in order.
    #[uniffi(default)]
    pub keywords: Vec<String>,
    /// People tagged on it (sidecar), in order.
    #[uniffi(default)]
    pub people: Vec<String>,
    /// Title, caption, credits, location and rights (sidecar); `None` when none is set.
    #[uniffi(default = None)]
    pub metadata: Option<metadata::PhotoMetadata>,
    /// Where it was taken, set by hand on the map (sidecar; wins over the
    /// file's GPS in `meta`).
    #[uniffi(default = None)]
    pub geotag: Option<GeoPosition>,
    /// Where the file says it was taken, as the places scan read it (EXIF,
    /// then XMP); `None` until scanned (then `meta.gps` is the EXIF position).
    #[uniffi(default = None)]
    pub file_gps: Option<GeoPosition>,
    /// Why AI Cull rejected it (sidecar `cull_note`); `None` once the flag
    /// changed or when it was never culled.
    #[uniffi(default = None)]
    pub cull_note: Option<String>,
}

/// A session folder originals can be moved between (sessions created by import).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SessionFolder {
    Capture,
    Selects,
    Trash,
}

impl From<SessionFolder> for ds_session::Folder {
    fn from(f: SessionFolder) -> Self {
        match f {
            SessionFolder::Capture => Self::Capture,
            SessionFolder::Selects => Self::Selects,
            SessionFolder::Trash => Self::Trash,
        }
    }
}

/// One original moved between session folders (paths relative to the
/// session), named by one of its variants: what undo needs to put it back.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderMoveRecord {
    pub variant_id: i64,
    pub from: String,
    pub to: String,
}

/// What "Move Rejected to Trash" did ([`SessionHandle::trash_rejected`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TrashRejectedReport {
    /// Originals whose every variant is rejected, now in `Trash/`.
    pub moved: Vec<FolderMoveRecord>,
    /// Rejected variants of photos with a kept variant: deleted (their
    /// sidecars set aside; [`SessionHandle::move_back`] restores them).
    pub hidden: Vec<i64>,
    /// It stopped part-way: why (`moved` and `hidden` are what was done).
    pub error: Option<String>,
}

/// Undo records of moved originals, each named by its first variant.
fn move_records(
    s: &Session,
    moved: Vec<ds_session::Moved>,
) -> Result<Vec<FolderMoveRecord>, DsError> {
    moved
        .into_iter()
        .map(|m| {
            Ok(FolderMoveRecord {
                variant_id: s.variants(m.image)?[0].0,
                from: m.from.as_str().to_owned(),
                to: m.to.as_str().to_owned(),
            })
        })
        .collect()
}

/// One variant's adjustments as `ds_core::Adjustments` JSON.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AdjustmentsJson {
    pub variant_id: i64,
    pub json: String,
}

/// Every non-default variant's adjustments (`load_all_adjustments_json`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AllAdjustmentsJson {
    /// Variants with real edits, each read from its sidecar.
    pub edits: Vec<AdjustmentsJson>,
    /// Variants whose adjustments are exactly `neutral_json` (an untouched sidecar from
    /// before profiles), known from the index without reading their sidecars.
    pub neutral_ids: Vec<i64>,
    /// The default adjustments with the Neutral profile.
    pub neutral_json: String,
    /// The device-write generation this snapshot includes: pass it to
    /// `remote_mark_seen` once these edits are in the app's model.
    pub device_generation: u64,
}

/// What opening a folder in place did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OpenReport {
    /// Originals registered by this open (new files).
    pub added: u64,
    /// Supported files already in the session.
    pub existing: u64,
    /// Files that could not be added: "path: reason".
    pub failed: Vec<String>,
    /// Originals followed to a new name (Finder rename) by content hash.
    pub relinked: u64,
    /// Originals not found (listed with `missing = true`).
    pub missing: u64,
    /// Originals whose stat changed, kept provisionally until
    /// [`SessionHandle::hash_pending`] checks them.
    pub to_verify: u64,
    /// New files held back until hashed (they may be a missing photo).
    pub held: u64,
    /// Files seen in the folder when the walk hit its limit (else `None`).
    pub truncated_at: Option<u64>,
    /// Where the index and sidecars are.
    pub store: String,
    /// `true` when the store is inside the folder (`<folder>/.ds/session`).
    pub store_on_volume: bool,
    /// Hard links removed while migrating an old in-place session.
    pub links_removed: u64,
    /// Plain copies removed by that migration (identical file in the folder).
    pub duplicates_removed: u64,
    /// Files the migration kept in the store because they are the only copy
    /// (`<rel>` inside the folder); see [`VariantRecord::kept_in_store`].
    pub kept_in_store: Vec<String>,
}

impl OpenReport {
    fn empty(store: &Path) -> Self {
        Self {
            added: 0,
            existing: 0,
            failed: Vec::new(),
            relinked: 0,
            missing: 0,
            to_verify: 0,
            held: 0,
            truncated_at: None,
            store: store.to_string_lossy().into_owned(),
            store_on_volume: false,
            links_removed: 0,
            duplicates_removed: 0,
            kept_in_store: Vec::new(),
        }
    }
}

/// A preset applied to every photo when it is first registered.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AutoApplyPreset {
    pub name: String,
    /// `ds_core::Adjustments` JSON; only `groups` are taken from it.
    pub adjustments_json: String,
    pub groups: Vec<crate::clipboard::AdjustGroup>,
    /// Groups applied as deltas from the default.
    pub relative: Vec<crate::clipboard::AdjustGroup>,
}

impl AutoApplyPreset {
    pub(crate) fn to_preset(&self) -> Result<Preset, DsError> {
        Ok(Preset {
            schema_version: ds_session::PRESET_SCHEMA_VERSION,
            name: self.name.clone(),
            adjustments: SessionHandle::parse(&self.adjustments_json)?,
            groups: self.groups.iter().map(|&g| g.into()).collect(),
            relative: self.relative.iter().map(|&g| g.into()).collect(),
        })
    }
}

/// What [`SessionHandle::hash_pending`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct HashReport {
    /// Files hashed and recorded.
    pub hashed: u64,
    /// Files that changed while being hashed (looked at again on the next open).
    pub skipped: u64,
    /// Files that could not be read ("path: reason"): kept as they were,
    /// retried on the next open.
    pub failed: Vec<String>,
    /// Re-checked originals that are byte-identical.
    pub verified: u64,
    /// Re-checked originals whose metadata was rewritten (same sensor data):
    /// ratings and edits kept.
    pub metadata_edited: u64,
    /// Originals replaced by a different photo ("file changed", restorable).
    pub changed: u64,
    /// Held files matched to missing photos by content.
    pub relinked: u64,
    /// Held files registered as new photos.
    pub registered: u64,
    /// Paths of missing photos with identical-content candidates that names
    /// could not tell apart (left for the user; never guessed).
    pub ambiguous: Vec<String>,
    /// The photo list changed (relinked, changed or new photos): list again.
    pub listing_changed: bool,
}

/// One file for the background hashing pass.
enum Job {
    /// Present, stat changed since verified.
    Verify(ToVerify),
    /// New file that could be a missing photo (`Capture/<rel>`, path).
    Held(String, PathBuf),
    /// Registered by stat, never hashed.
    Unhashed(ImageId, PathBuf),
}

impl Job {
    fn path(&self) -> &Path {
        match self {
            Self::Verify(v) => &v.path,
            Self::Held(_, p) | Self::Unhashed(_, p) => p,
        }
    }
}

/// The session's originals and variants as `list` needs them.
#[derive(Default)]
struct Listing {
    root: PathBuf,
    rows: Vec<(ImageInfo, VariantInfo)>,
    paths: HashMap<ImageId, PathBuf>,
    kept: HashSet<ImageId>,
    /// The places scan's position (EXIF, else XMP) per original.
    places: HashMap<ImageId, (f64, f64)>,
}

/// Cached capture metadata of one original (keyed by its session path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedMeta {
    size: u64,
    mtime: i64,
    meta: RawMetadata,
    /// [`META_VERSION`] when read: older entries lack fields and are read again.
    #[serde(default)]
    version: u32,
}

/// Bump when [`RawMetadata`] gains a field older cache entries can't have.
/// 1: GPS (and the artist and copyright metadata added alongside).
const META_VERSION: u32 = 1;

/// An open session. Thread-safe (one writer at a time).
#[derive(uniffi::Object)]
pub struct SessionHandle {
    pub(crate) session: Mutex<Session>,
    /// `Some(folder)` for an in-place session: originals are that folder's files.
    pub(crate) in_place: Option<PathBuf>,
    /// The session folder of a plain session (created by import): renamed or
    /// moved in Finder while open, it is reported as `FolderMoved` before any
    /// write (so no sidecar lands in a recreated ghost folder).
    plain_root: Option<PathBuf>,
    report: OpenReport,
    /// Background work the last open left for [`SessionHandle::hash_pending`].
    pending: Mutex<PendingWork>,
    meta: Mutex<HashMap<String, CachedMeta>>,
}

/// What an open could not decide by stat alone.
#[derive(Debug, Default, Clone)]
struct PendingWork {
    verify: Vec<ToVerify>,
    held: Vec<String>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("in_place", &self.in_place)
            .finish_non_exhaustive()
    }
}

/// Byte ranges `(offset, len)` of a file.
type Ranges = Vec<(u64, u64)>;

/// How files are recognised and described, and whether a volume may hold a
/// session. Real exports use `ds_raw` and `statfs`; tests inject fakes.
pub(crate) struct Probe {
    pub is_supported: fn(&Path) -> bool,
    /// One metadata read: the sidecar summary plus the full record for the browser.
    pub read: fn(&Path) -> Option<(ImageMetaLite, RawMetadata)>,
    /// `Ok` when sidecars may be written into this folder (local, writable APFS/HFS+).
    pub volume_ok: fn(&Path) -> Result<(), String>,
    /// Sensor-data byte ranges (content identity); `None`: the whole file.
    pub ranges: fn(&Path) -> Option<Ranges>,
    /// The volume's stat can't prove identity (FAT/exFAT reuse inodes):
    /// unchanged files get a sample check in the background.
    pub untrusted_stat: fn(&Path) -> bool,
    /// The name is a RAW type (an unreadable one is reported, not skipped).
    pub is_raw_name: fn(&Path) -> bool,
}

impl Probe {
    pub(crate) const RAW: Probe = Probe {
        is_supported: ds_raw::is_supported,
        read: read_meta,
        volume_ok: sidecar_volume_ok,
        ranges: |p| ds_raw::image_data_ranges(p).ok().flatten(),
        untrusted_stat: fat_volume,
        is_raw_name: ds_raw::has_raw_extension,
    };
}

fn fs_type_name(st: &rustix::fs::StatFs) -> String {
    st.f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8 as char)
        .collect()
}

fn fat_volume(folder: &Path) -> bool {
    rustix::fs::statfs(folder)
        .is_ok_and(|st| matches!(fs_type_name(&st).as_str(), "msdos" | "exfat"))
}

pub(crate) fn read_meta(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    let m = ds_raw::read_metadata(p).ok()?;
    let size = m.dimensions.unwrap_or(ds_core::Size::new(0, 0));
    let lite = ImageMetaLite {
        capture_time_ms: m.capture_time.as_ref().map(capture_ms),
        camera: Some(format!("{} {}", m.make, m.model).trim().to_owned()).filter(|s| !s.is_empty()),
        lens: m.lens.clone(),
        iso: m.iso,
        focal_length_mm: m.focal_length,
        width: size.width,
        height: size.height,
        orientation: m.orientation,
        as_shot_wb: None,
    };
    Some((lite, m.into()))
}

/// Why a Capture One session folder holds no Darkslide store.
const CAPTURE_ONE_FOLDER: &str = "a Capture One session folder is never written to";

/// File systems where the app may create `<folder>/.ds/session`.
const SIDECAR_FS: [&str; 2] = ["apfs", "hfs"];
const MNT_RDONLY: u32 = 0x0000_0001;
const MNT_LOCAL: u32 = 0x0000_1000;

/// Decide from `statfs` flags and type whether sidecars may live in the folder.
pub(crate) fn volume_verdict(fs_type: &str, flags: u32, writable: bool) -> Result<(), String> {
    if flags & MNT_RDONLY != 0 || !writable {
        return Err("the volume is read-only".into());
    }
    if flags & MNT_LOCAL == 0 {
        return Err(format!("{fs_type} is a network volume"));
    }
    if !SIDECAR_FS.contains(&fs_type) {
        return Err(format!(
            "{fs_type} volumes (memory cards) are never written to"
        ));
    }
    Ok(())
}

pub(crate) fn sidecar_volume_ok(folder: &Path) -> Result<(), String> {
    let st = rustix::fs::statfs(folder).map_err(|e| e.to_string())?;
    let name = fs_type_name(&st);
    let writable = rustix::fs::access(folder, rustix::fs::Access::WRITE_OK).is_ok();
    volume_verdict(&name, st.f_flags, writable)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Camera wall-clock time as ms since the epoch, treated as UTC (the
/// `ImageMetaLite::capture_time_ms` convention).
pub(crate) fn capture_ms(t: &ds_raw::CaptureTime) -> i64 {
    let days = days_from_civil(i64::from(t.year), i64::from(t.month), i64::from(t.day));
    let secs =
        days * 86_400 + i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
    secs * 1000 + i64::from(t.nanos / 1_000_000)
}

/// A session name from a folder name (ds-session forbids `/ : NUL`, a leading
/// dot, and more than 200 bytes).
pub(crate) fn session_name(folder: &Path) -> String {
    let raw = folder
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Session");
    let mut s: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '/' | ':' | '\0') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = s.trim().trim_start_matches('.').trim().to_owned();
    s = trimmed;
    while s.len() > 200 {
        s.pop();
    }
    if s.is_empty() { "Session".into() } else { s }
}

/// The identity hashes of an original (whole file, sensor data, sample).
fn hash_with(probe: &Probe, p: &Path, cancelled: &dyn Fn() -> bool) -> io::Result<Hashes> {
    let ranges = (probe.ranges)(p);
    identity::hash_original(p, ranges.as_deref(), cancelled)
}

/// What identifies the volume a folder is on when it has no volume UUID
/// (network shares, some cards): the file system type and what is mounted
/// (`//user@server/share`, `server:/export`, `/dev/disk4s1`). The app keys
/// fallback stores by it, so two shares never share a store.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn volume_identity(folder: String) -> Result<String, DsError> {
    guard(|| {
        let st = rustix::fs::statfs(Path::new(&folder))
            .map_err(|e| DsError::invalid(format!("{folder}: {e}")))?;
        let from: String = st
            .f_mntfromname
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8 as char)
            .collect();
        let kind = if st.f_flags & MNT_LOCAL == 0 {
            "net"
        } else {
            "dev"
        };
        Ok(format!("{kind}:{}:{from}", fs_type_name(&st)))
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Progress stages of an open: `(stage, done, total)` → events with
/// `kind` = "open.scan" / "open.register".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Scanning,
    Registering,
}

fn emit(d: &Dispatcher, id: u64, kind: &str, phase: TaskPhase, completed: u64, total: u64) {
    d.emit(ProgressEvent {
        task_id: id,
        kind: kind.into(),
        phase,
        completed,
        total,
    });
}

fn has_session(dir: &Path) -> bool {
    dir.join(".ds/settings/session.json").is_file()
}

/// Removes a freshly created store (and the `.ds` it made in the user's
/// folder) unless disarmed: a failed first open leaves nothing behind.
struct Cleanup {
    root: Option<PathBuf>,
    made_ds: Option<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(root) = self.root.take() {
            let _ = fs::remove_dir_all(&root);
            // Only the (now empty) `.ds` this open created; never anything else.
            if let Some(ds) = &self.made_ds {
                let _ = fs::remove_dir(ds);
            }
        }
    }
}

/// Create the store at `root`; if another process created it meanwhile,
/// open theirs instead, and then it is not ours to clean up.
fn create_or_join(
    root: &Path,
    name: &str,
    folder: &Path,
    cleanup: &mut Cleanup,
) -> Result<Session, DsError> {
    match Session::create_external(root, name, folder) {
        Err(SessionError::AlreadyExists(_)) => {
            cleanup.root = None;
            cleanup.made_ds = None;
            for _ in 0..100 {
                if has_session(root) {
                    return Ok(Session::open(root)?);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(DsError::Io {
                message: format!("{} is being created by another process", root.display()),
            })
        }
        r => Ok(r?),
    }
}

/// Who is opening a folder: the app's identity for it (volume identity plus
/// path; `None`: not checked) and whether the folder was proven moved (a
/// bookmark resolved to it), which is the only time a fallback store may be
/// re-pointed at a folder it wasn't made for.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenOptions {
    pub identity: Option<String>,
    pub allow_move: bool,
}

/// Whether the fallback store `s` belongs to `folder`: a store is
/// never silently taken over by another folder that maps to the same place.
fn belongs(s: &Session, folder: &Path, opts: &OpenOptions) -> bool {
    if opts.allow_move {
        return true;
    }
    match (s.folder_identity(), opts.identity.as_deref()) {
        (Some(have), Some(want)) => have == want,
        (None, Some(_)) => s.external_root() == Some(folder),
        (_, None) => true,
    }
}

/// The session for `folder`: the one in the folder (an old hard-link one is
/// migrated first: links verified, then removed), else the one at
/// `fallback` if it belongs to this folder (else a separate one next to it),
/// else a new one in the folder when its volume can hold it, or at
/// `fallback`. Returns `(root, session, migration)`; arms `cleanup` for what
/// it creates.
fn store_for(
    folder: &Path,
    fallback: Option<&Path>,
    opts: &OpenOptions,
    probe: &Probe,
    cleanup: &mut Cleanup,
) -> Result<(PathBuf, Session, ds_session::Migrated), DsError> {
    let on_volume = folder.join(IN_PLACE_DIR);
    // Only write to the folder's volume when it may hold the store. A session found
    // there on a volume that can't (card, read-only, network) is left untouched: no
    // migration, no index writes; the store goes to `fallback` instead.
    let mut volume_ok = (probe.volume_ok)(folder);
    // A Capture One session folder is Capture One's: a new store never goes into it
    // (its sync, iCloud or backups would pick it up). One an earlier
    // version made there is kept, so no edits are lost.
    if volume_ok.is_ok() && !has_session(&on_volume) && ds_session::is_capture_one_session(folder) {
        volume_ok = Err(CAPTURE_ONE_FOLDER.into());
    }
    if volume_ok.is_ok() && has_session(&on_volume) {
        let migrated = Session::migrate_to_external(&on_volume, folder)?;
        let s = Session::open(&on_volume)?;
        return Ok((on_volume, s, migrated));
    }
    let mut fallback = fallback.map(Path::to_path_buf);
    if let Some(fb) = fallback.clone().filter(|d| has_session(d)) {
        let s = Session::open(&fb)?;
        if belongs(&s, folder, opts) {
            return Ok((fb, s, ds_session::Migrated::default()));
        }
        // Another folder's store: never take it over; use one of our own.
        let key = opts
            .identity
            .clone()
            .unwrap_or_else(|| folder.to_string_lossy().into_owned());
        let tag = blake3::hash(key.as_bytes()).to_hex();
        let name = fb
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let alt = fb.with_file_name(format!("{name}-{}", &tag[..12]));
        if has_session(&alt) {
            let s = Session::open(&alt)?;
            if !belongs(&s, folder, opts) {
                return Err(DsError::Io {
                    message: format!("{} belongs to another folder", alt.display()),
                });
            }
            return Ok((alt, s, ds_session::Migrated::default()));
        }
        fallback = Some(alt);
    }
    let root = match volume_ok {
        Ok(()) => on_volume,
        Err(why) => fallback.ok_or_else(|| DsError::Unsupported {
            message: format!(
                "{} can't hold Darkslide's sidecars ({why}) and no other place was given",
                folder.display()
            ),
        })?,
    };
    if root.starts_with(folder) && !folder.join(".ds").exists() {
        cleanup.made_ds = Some(folder.join(".ds"));
    }
    if !root.exists() {
        cleanup.root = Some(root.clone());
    }
    let s = create_or_join(&root, &session_name(folder), folder, cleanup)?;
    Ok((root, s, ds_session::Migrated::default()))
}

// ------------------------------------------------------------ archiving ---

/// What [`crate::archive`] needs from a session (fields stay private here).
impl SessionHandle {
    /// The folder rsync archives, or why this session can't be archived: an
    /// imported session's own folder, or an in-place session's folder when its
    /// store lives there (the DB snapshot is written into `<folder>/.ds/`). A
    /// folder on a card or read-only volume (store elsewhere) is never written.
    pub(crate) fn archive_source(&self) -> Result<PathBuf, String> {
        match &self.in_place {
            None => Ok(lock(&self.session).root().to_path_buf()),
            Some(folder) if self.report.store_on_volume => Ok(folder.clone()),
            Some(_) => Err(
                "on a card, a read-only volume or in a Capture One session, where Darkslide never writes: import it to archive it"
                    .into(),
            ),
        }
    }

    /// Session folders whose moves the archive mirrors (see
    /// `ds_archive::ArchiveTarget::mirror_folders`): an imported session's `Capture`
    /// and `Selects`, so Move to Selects never leaves a second copy on the archive.
    /// None for a folder opened in place (nothing moves there).
    pub(crate) fn archive_mirror_folders(&self) -> Vec<String> {
        if self.in_place.is_some() {
            return Vec::new();
        }
        [ds_session::Folder::Capture, ds_session::Folder::Selects]
            .map(|f| f.dir_name().to_owned())
            .to_vec()
    }

    /// An imported session (its originals are its own copies under
    /// `Capture/`), as opposed to a folder opened in place. Only imported
    /// sessions are handed to Amazon Photos.
    pub(crate) fn is_imported(&self) -> bool {
        self.in_place.is_none()
    }

    /// Consistent copy of the session index at `dest` (SQLite backup API).
    pub(crate) fn snapshot_index(&self, dest: &Path) -> Result<(), String> {
        lock(&self.session)
            .snapshot_to(dest)
            .map_err(|e| e.to_string())
    }

    /// Originals with a verified whole-file hash (for the Amazon hand-off),
    /// plus how many are not hashed yet or missing (handed over later).
    /// Rejected photos are left out unless `include_rejected`;
    /// the last list is their hashes (unhashed ones as `UNHASHED`).
    pub(crate) fn backup_originals(
        &self,
        include_rejected: bool,
    ) -> Result<(HashedOriginals, u64, Vec<FileHash>), DsError> {
        let s = lock(&self.session);
        let mut out = Vec::new();
        let mut later = 0;
        let mut excluded = Vec::new();
        for img in s.images()? {
            if !include_rejected && Self::is_rejected(&s, &img)? {
                excluded.push(img.hash);
                continue;
            }
            if img.missing || img.hash == UNHASHED {
                later += 1;
            } else {
                out.push((s.original_path(img.rel_path.as_str()), img.hash));
            }
        }
        Ok((out, later, excluded))
    }

    /// The originals an archive run copies (rejected ones only when
    /// `include_rejected`), for counting what the target is missing.
    pub(crate) fn archive_originals(
        &self,
        include_rejected: bool,
    ) -> Result<Vec<PathBuf>, DsError> {
        let s = lock(&self.session);
        let mut out = Vec::new();
        for img in s.images()? {
            if include_rejected || !Self::is_rejected(&s, &img)? {
                out.push(s.original_path(img.rel_path.as_str()));
            }
        }
        Ok(out)
    }

    /// How many originals the session has, and how many of them are rejected
    /// (for honest archive counts: "5 archived, 3 rejects excluded").
    pub(crate) fn original_counts(&self) -> Result<(u64, u64), DsError> {
        let s = lock(&self.session);
        let (mut total, mut rejected) = (0, 0);
        for img in s.images()? {
            total += 1;
            if Self::is_rejected(&s, &img)? {
                rejected += 1;
            }
        }
        Ok((total, rejected))
    }

    /// A rejected photo: every variant is rejected, or it was moved to the
    /// session's `Trash/` (the same rule as "Move Rejected to Trash").
    fn is_rejected(s: &Session, img: &ds_session::ImageInfo) -> Result<bool, DsError> {
        Ok(s.is_rejected(img)?)
    }

    /// rsync `--exclude` patterns (anchored at the archive source) that keep
    /// rejected photos out of the archive: each rejected
    /// original, and the session's `Trash/` folder. Nothing is removed from an
    /// archive that already has them (the archive never deletes).
    pub(crate) fn reject_excludes(&self) -> Result<Vec<String>, DsError> {
        let source = self
            .archive_source()
            .map_err(|m| DsError::Unsupported { message: m })?;
        let s = lock(&self.session);
        let mut out = Vec::new();
        if self.in_place.is_none() {
            out.push("/Trash/".to_owned());
        }
        for img in s.images()? {
            if img.rel_path.folder() == ds_session::Folder::Trash || !Self::is_rejected(&s, &img)? {
                continue;
            }
            let path = s.original_path(img.rel_path.as_str());
            if let Ok(rel) = path.strip_prefix(&source) {
                out.push(format!("/{}", rsync_escape(&rel.to_string_lossy())));
            }
        }
        Ok(out)
    }
}

/// Originals and their verified whole-file hashes.
pub(crate) type HashedOriginals = Vec<(PathBuf, FileHash)>;

/// Open (or create) the in-place session for `folder` and register new files
/// (with `auto_apply` applied to them; it is stored as the session's
/// auto-apply preset). `fallback` is where the store goes when the folder's
/// volume can't hold it. Identity is not checked (tests; see [`open_in_place_with`]).
#[cfg(test)]
pub(crate) fn open_in_place(
    folder: &Path,
    fallback: Option<&Path>,
    auto_apply: Option<Preset>,
    probe: &Probe,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Stage, u64, u64),
) -> Result<SessionHandle, DsError> {
    open_in_place_with(
        folder,
        fallback,
        &OpenOptions::default(),
        auto_apply,
        probe,
        cancelled,
        progress,
    )
}

/// [`open_in_place`] with the app's folder identity. Only stat work
/// happens here: what needs hashing is left to
/// [`SessionHandle::hash_pending`].
#[allow(clippy::too_many_lines)]
pub(crate) fn open_in_place_with(
    folder: &Path,
    fallback: Option<&Path>,
    opts: &OpenOptions,
    auto_apply: Option<Preset>,
    probe: &Probe,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(Stage, u64, u64),
) -> Result<SessionHandle, DsError> {
    let folder = folder.canonicalize()?;
    if !folder.is_dir() {
        return Err(DsError::invalid(format!(
            "{} is not a folder",
            folder.display()
        )));
    }
    progress(Stage::Scanning, 0, 0);
    let mut cleanup = Cleanup {
        root: None,
        made_ds: None,
    };
    let (root, mut session, migrated) = store_for(&folder, fallback, opts, probe, &mut cleanup)?;
    if session.external_root() != Some(folder.as_path()) {
        session.set_external_root(&folder)?;
    }
    if let Some(id) = &opts.identity
        && session.folder_identity() != Some(id.as_str())
    {
        session.set_folder_identity(id)?;
    }
    if session.auto_apply_preset() != auto_apply.as_ref() {
        session.set_auto_apply_preset(auto_apply)?;
    }
    let sync = session.sync_external_with((probe.untrusted_stat)(&folder))?;
    if cancelled() {
        return Err(DsError::Cancelled);
    }
    let mut failed = Vec::new();
    let mut new = Vec::new();
    for rel in &sync.untracked {
        let p = session.original_path(rel);
        if (probe.is_supported)(&p) {
            new.push(rel.clone());
        } else if (probe.is_raw_name)(&p) {
            // a RAW that can't be read is reported, not silently skipped; so is
            // one that opens but isn't a readable RAW (0 bytes, cut short).
            let why = match fs::File::open(&p) {
                Err(e) => format!("unreadable ({e})"),
                Ok(_) => "damaged or incomplete".to_owned(),
            };
            failed.push(format!("{rel}: {why}"));
        }
    }
    let existing = session.image_count()? as u64;
    let total = new.len() as u64;
    let mut added = 0u64;
    let mut cache = load_meta_cache(&root);
    progress(Stage::Registering, 0, total);
    for chunk in new.chunks(ADD_BATCH) {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        let batch = read_batch(&session, chunk, probe, &mut cache, &mut failed);
        let ids = session.add_new_originals(batch)?;
        added += ids.len() as u64;
        progress(
            Stage::Registering,
            (added + failed.len() as u64).min(total),
            total,
        );
    }
    save_meta_cache(&root, &cache);
    // Smart folders: built-ins exist from the first open (and come back if deleted).
    session.ensure_builtin_albums()?;
    let store_on_volume = root.starts_with(&folder);
    cleanup.root = None; // success: keep the store
    Ok(SessionHandle {
        session: Mutex::new(session),
        in_place: Some(folder),
        plain_root: None,
        report: OpenReport {
            added,
            existing,
            failed,
            relinked: sync.relinked.len() as u64,
            missing: sync.missing.len() as u64,
            to_verify: sync.verify.len() as u64,
            held: sync.held.len() as u64,
            truncated_at: (sync.seen > ds_session::MAX_EXTERNAL_FILES).then_some(sync.seen as u64),
            store: root.to_string_lossy().into_owned(),
            store_on_volume,
            links_removed: migrated.links_removed as u64,
            duplicates_removed: migrated.duplicates_removed as u64,
            kept_in_store: migrated.kept,
        },
        pending: Mutex::new(PendingWork {
            verify: sync.verify,
            held: sync.held,
        }),
        meta: Mutex::new(cache),
    })
}

/// Read the metadata of `rels` in parallel (caching the full record); files
/// that can't be read go to `failed`.
fn read_batch(
    session: &Session,
    rels: &[String],
    probe: &Probe,
    cache: &mut HashMap<String, CachedMeta>,
    failed: &mut Vec<String>,
) -> Vec<NewOriginal> {
    let paths: Vec<(String, PathBuf)> = rels
        .iter()
        .map(|rel| (rel.clone(), session.original_path(rel)))
        .collect();
    // Each folder is listed once for the XMP sidecars, then read in parallel.
    let mut dirs = places::Listings::default();
    dirs.prefill(paths.iter().map(|(_, p)| p.as_path()));
    let read: Vec<_> = paths
        .into_par_iter()
        .map(|(rel, p)| {
            let r = (probe.read)(&p).map(|(lite, meta)| {
                let iptc = metadata::iptc_of(&p, &p, metadata::raw_credits(&meta), &dirs);
                (lite, meta, iptc)
            });
            (rel, original_stat(&p), r)
        })
        .collect();
    let mut batch = Vec::with_capacity(read.len());
    for (rel, stat, r) in read {
        let Some((lite, meta, iptc)) = r else {
            failed.push(format!("{rel}: unreadable metadata"));
            continue;
        };
        if let Some((size, mtime, _)) = stat {
            cache.insert(
                rel.clone(),
                CachedMeta {
                    size,
                    mtime,
                    meta,
                    version: META_VERSION,
                },
            );
        }
        batch.push(NewOriginal {
            rel,
            hash: UNHASHED,
            content_hash: None,
            meta: lite,
            replaced: Vec::new(),
            iptc,
        });
    }
    batch
}

fn load_meta_cache(root: &Path) -> HashMap<String, CachedMeta> {
    fs::read(root.join(META_CACHE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// A literal path as an rsync/openrsync exclude pattern: wildcard characters
/// are escaped (a backslash escapes them in a pattern that has wildcards).
pub(crate) fn rsync_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn save_meta_cache(root: &Path, cache: &HashMap<String, CachedMeta>) {
    // Disposable cache: a failed write only costs a re-read next time.
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let p = root.join(META_CACHE);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = ds_session::write_file_atomic(&p, &bytes);
    }
}

impl SessionHandle {
    pub(crate) fn plain(session: Session) -> Self {
        let root = session.root().to_path_buf();
        Self {
            meta: Mutex::new(load_meta_cache(&root)),
            session: Mutex::new(session),
            in_place: None,
            plain_root: Some(root.clone()),
            report: OpenReport::empty(&root),
            pending: Mutex::new(PendingWork::default()),
        }
    }

    fn empty_trash_with(&self, trasher: &dyn ds_session::Trasher) -> Result<u64, DsError> {
        self.check_folder()?;
        Ok(lock(&self.session).empty_trash(trasher)? as u64)
    }

    /// The folder must still be where it was: the user's folder of an in-place
    /// session, the session folder of a plain one.
    pub(crate) fn check_folder(&self) -> Result<(), DsError> {
        match self.in_place.as_ref().or(self.plain_root.as_ref()) {
            Some(f) if !f.is_dir() => Err(DsError::FolderMoved {
                message: f.to_string_lossy().into_owned(),
            }),
            _ => Ok(()),
        }
    }

    /// What `list` reads under the session lock.
    fn listing(&self) -> Result<Listing, DsError> {
        let s = lock(&self.session);
        let mut out = Listing {
            root: s.root().to_path_buf(),
            ..Listing::default()
        };
        for img in s.images()? {
            out.paths
                .insert(img.id, s.original_path(img.rel_path.as_str()));
            if img.missing && s.kept_copy(img.id).is_some() {
                out.kept.insert(img.id);
            }
            if let Some(key) = ds_session::embedding_key(&img)
                && let Some(gps) = s.location(&key)?.and_then(|p| p.gps)
            {
                out.places.insert(img.id, gps);
            }
            for v in s.variants(img.id)? {
                out.rows.push((img.clone(), s.variant(v)?));
            }
        }
        Ok(out)
    }

    pub(crate) fn list(
        &self,
        probe: &Probe,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<VariantRecord>, DsError> {
        self.check_folder()?;
        let Listing {
            root,
            rows,
            paths,
            kept,
            places,
        } = self.listing()?;
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        let cache = lock(&self.meta).clone();
        let out: Vec<(VariantRecord, Option<(String, CachedMeta)>)> = rows
            .into_par_iter()
            .map(|(img, v)| {
                let original = paths[&img.id].clone();
                let stat = original_stat(&original);
                let rel = img.rel_path.as_str().to_owned();
                let cached = cache
                    .get(&rel)
                    .filter(|c| {
                        c.version == META_VERSION
                            && stat.is_some_and(|(sz, mt, _)| c.size == sz && c.mtime == mt)
                    })
                    .map(|c| c.meta.clone());
                let (meta, fresh) = match (cached, stat) {
                    (Some(m), _) => (Some(m), None),
                    (None, Some((size, mtime, _))) => match (probe.read)(&original) {
                        Some((_, full)) => (
                            Some(full.clone()),
                            Some((
                                rel,
                                CachedMeta {
                                    size,
                                    mtime,
                                    meta: full,
                                    version: META_VERSION,
                                },
                            )),
                        ),
                        None => (None, None),
                    },
                    (None, None) => (None, None),
                };
                let size = img.meta.oriented_size();
                let rec = VariantRecord {
                    variant_id: v.id.0,
                    image_id: img.id.0,
                    file_name: original
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    missing: img.missing || stat.is_none(),
                    file_changed: img.file_changed,
                    kept_in_store: kept.contains(&img.id),
                    meta,
                    original_path: original.to_string_lossy().into_owned(),
                    name: v.name,
                    rating: v.rating.stars(),
                    color_tag: v.color_tag.into(),
                    pick: v.pick.into(),
                    edited: v.edited,
                    read_only: v.read_only,
                    file_size: img.file_size,
                    file_sequence: ds_raw::filename_sequence(&original),
                    capture_time_ms: img.meta.capture_time_ms,
                    width: size.width,
                    height: size.height,
                    in_trash: img.rel_path.folder() == ds_session::Folder::Trash,
                    in_selects: img.rel_path.folder() == ds_session::Folder::Selects,
                    keywords: v.keywords,
                    people: v.people,
                    metadata: (!v.iptc.is_empty()).then(|| v.iptc.into()),
                    geotag: v.geotag.map(GeoPosition::from),
                    cull_note: v.cull_note,
                    file_gps: places
                        .get(&img.id)
                        .map(|&(latitude, longitude)| GeoPosition {
                            latitude,
                            longitude,
                            altitude_m: None,
                        }),
                };
                (rec, fresh)
            })
            .collect();
        let mut records = Vec::with_capacity(out.len());
        let mut updates = Vec::new();
        for (rec, fresh) in out {
            records.push(rec);
            updates.extend(fresh);
        }
        if !updates.is_empty() {
            let mut c = lock(&self.meta);
            c.extend(updates);
            save_meta_cache(&root, &c);
        }
        Ok(records)
    }

    /// Background identity work left by the open (see the module docs):
    /// sample checks (FAT/exFAT), then verification of changed files, held
    /// files and first hashes, all on [`crate::background`]'s threads. The
    /// session lock is only taken to record results, in batches.
    pub(crate) fn hash_pending_with(
        &self,
        probe: &Probe,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<HashReport, DsError> {
        self.check_folder()?;
        self.requeue_on_error(|work| self.hash_pending_run(work, probe, cancelled, progress))
    }

    /// Run `f` on the pending work. If it fails for any reason but a
    /// cancel (which puts back exactly what is left), everything it was
    /// given is queued again: held files and changed-file checks aren't
    /// rediscovered until the next open, so an error must not lose them.
    fn requeue_on_error<T>(
        &self,
        f: impl FnOnce(PendingWork) -> Result<T, DsError>,
    ) -> Result<T, DsError> {
        let work = std::mem::take(&mut *lock(&self.pending));
        let backup = work.clone();
        let out = f(work);
        if matches!(&out, Err(e) if *e != DsError::Cancelled) {
            let mut p = lock(&self.pending);
            p.verify.extend(backup.verify);
            p.held.extend(backup.held);
        }
        out
    }

    #[allow(clippy::too_many_lines)]
    fn hash_pending_run(
        &self,
        work: PendingWork,
        probe: &Probe,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<HashReport, DsError> {
        let mut report = HashReport::default();
        let (samples, mut full): (Vec<ToVerify>, Vec<ToVerify>) =
            work.verify.into_iter().partition(|v| v.sample_only);
        let in_verify: HashSet<ImageId> = full.iter().map(|v| v.image).collect();
        let unhashed: Vec<(ImageId, PathBuf)> = lock(&self.session)
            .unhashed_images()?
            .into_iter()
            .filter(|(id, _)| !in_verify.contains(id))
            .collect();
        let mut total = (samples.len() + full.len() + work.held.len() + unhashed.len()) as u64;
        let mut done = 0u64;
        progress(0, total);

        // 1. Sample checks: two small reads per file.
        let mut sampled = Vec::new();
        let mut sample_done = vec![false; samples.len()];
        let complete = crate::background::run_pool(
            &samples,
            cancelled,
            |v, _| identity::sample_hash(&v.path),
            |i, r| {
                sample_done[i] = true;
                done += 1;
                progress(done, total);
                match r {
                    Ok(h) => sampled.push((samples[i].image, h)),
                    Err(e) => report
                        .failed
                        .push(format!("{}: {e}", samples[i].path.display())),
                }
            },
        );
        if !complete {
            let mut p = lock(&self.pending);
            p.verify.extend(
                samples
                    .into_iter()
                    .zip(sample_done)
                    .filter(|(_, d)| !d)
                    .map(|(v, _)| v),
            );
            p.verify.extend(full);
            p.held.extend(work.held);
            return Err(DsError::Cancelled);
        }
        let differ = lock(&self.session).check_samples(&sampled)?;
        for v in samples.into_iter().filter(|v| differ.contains(&v.image)) {
            full.push(ToVerify {
                sample_only: false,
                ..v
            });
            total += 1;
        }

        // 2. Full hashes: changed files, held files, first hashes.
        let jobs: Vec<Job> = full
            .into_iter()
            .map(Job::Verify)
            .chain(work.held.into_iter().map(|rel| {
                let p = lock(&self.session).original_path(&rel);
                Job::Held(rel, p)
            }))
            .chain(unhashed.into_iter().map(|(id, p)| Job::Unhashed(id, p)))
            .collect();
        let mut accept: Vec<Verified> = Vec::new();
        let mut changed: Vec<(ToVerify, ds_session::Stat, Hashes)> = Vec::new();
        let mut found: Vec<Found> = Vec::new();
        let mut last_flush = std::time::Instant::now();
        let mut first_err: Option<DsError> = None;
        let mut job_done = vec![false; jobs.len()];
        let complete = crate::background::run_pool(
            &jobs,
            cancelled,
            |job, stop| {
                let path = job.path();
                let st = original_stat(path);
                let never_mind = || {
                    crate::background::wait_idle(stop);
                    stop.load(std::sync::atomic::Ordering::Acquire)
                };
                (st, hash_with(probe, path, &never_mind))
            },
            |i, (st, h)| {
                job_done[i] = true;
                done += 1;
                progress(done, total);
                let job = &jobs[i];
                let (Some(st), Ok(h)) = (st, h.as_ref().map(|h| *h)) else {
                    match h {
                        Err(e) if e.kind() != io::ErrorKind::Interrupted => {
                            report.failed.push(format!("{}: {e}", job.path().display()));
                        }
                        Err(_) => {}
                        Ok(_) => report.skipped += 1,
                    }
                    return;
                };
                report.hashed += 1;
                match job {
                    Job::Verify(v) => match identity::verdict(v.hash, v.content, &h) {
                        Verdict::Same => {
                            report.verified += 1;
                            accept.push(Verified {
                                image: v.image,
                                stat: st,
                                hashes: h,
                            });
                        }
                        Verdict::MetadataEdited => {
                            report.metadata_edited += 1;
                            accept.push(Verified {
                                image: v.image,
                                stat: st,
                                hashes: h,
                            });
                        }
                        Verdict::Changed => changed.push((v.clone(), st, h)),
                    },
                    Job::Held(rel, _) => found.push(Found {
                        rel: rel.clone(),
                        stat: st,
                        hashes: h,
                    }),
                    Job::Unhashed(id, _) => accept.push(Verified {
                        image: *id,
                        stat: st,
                        hashes: h,
                    }),
                }
                if first_err.is_none()
                    && (accept.len() >= HASH_BATCH || last_flush.elapsed() >= HASH_FLUSH)
                {
                    if let Err(e) = self.record_verified(&mut accept, &mut report) {
                        first_err = Some(e);
                    }
                    last_flush = std::time::Instant::now();
                }
            },
        );
        if let Some(e) = first_err {
            return Err(e);
        }
        self.record_verified(&mut accept, &mut report)?;
        if !complete {
            let mut p = lock(&self.pending);
            for (job, d) in jobs.into_iter().zip(job_done) {
                match job {
                    Job::Verify(v) if !d => p.verify.push(v),
                    Job::Held(rel, _) if !d => p.held.push(rel),
                    _ => {}
                }
            }
            p.held.extend(found.into_iter().map(|f| f.rel));
            return Err(DsError::Cancelled);
        }

        // 3. Different photos: their old metadata is set aside ("file changed").
        for (v, st, h) in changed {
            let now = (probe.read)(&v.path).map(|(lite, _)| lite);
            let mut s = lock(&self.session);
            // Registered by stat and never hashed: nothing to compare the
            // bytes with. The same camera and capture time is the same shot
            // (its metadata was rewritten before the first hash).
            let stored = s.image(v.image).ok().map(|i| i.meta);
            let same_shot = v.hash == UNHASHED
                && now.as_ref().zip(stored.as_ref()).is_some_and(|(a, b)| {
                    a.capture_time_ms.is_some()
                        && a.capture_time_ms == b.capture_time_ms
                        && a.camera == b.camera
                });
            if same_shot {
                report.metadata_edited += 1;
                s.accept_verified(&[Verified {
                    image: v.image,
                    stat: st,
                    hashes: h,
                }])?;
                continue;
            }
            if s.replace_image(v.image, st, Some(h), now.unwrap_or_default())?
                .is_some()
            {
                report.changed += 1;
            } else {
                report.skipped += 1;
            }
        }

        // 4. Held files: relink missing photos by content, register the rest.
        let hashes: HashMap<String, Hashes> =
            found.iter().map(|f| (f.rel.clone(), f.hashes)).collect();
        let relink = lock(&self.session).relink_found(&found)?;
        report.relinked = relink.relinked.len() as u64;
        report.ambiguous = relink.ambiguous;
        let mut fresh = Vec::new();
        let mut dirs = places::Listings::default();
        for rel in relink.unmatched {
            let p = lock(&self.session).original_path(&rel);
            if !(probe.is_supported)(&p) {
                continue;
            }
            let Some((lite, raw)) = (probe.read)(&p) else {
                report.failed.push(format!("{rel}: unreadable metadata"));
                continue;
            };
            let h = hashes[&rel];
            fresh.push(NewOriginal {
                rel,
                hash: h.file,
                content_hash: h.content,
                meta: lite,
                replaced: Vec::new(),
                iptc: {
                    dirs.prefill([p.as_path()]);
                    metadata::iptc_of(&p, &p, metadata::raw_credits(&raw), &dirs)
                },
            });
        }
        if !fresh.is_empty() {
            report.registered = lock(&self.session).add_new_originals(fresh)?.len() as u64;
        }
        report.listing_changed = report.changed + report.relinked + report.registered > 0;
        progress(total, total);
        Ok(report)
    }

    /// Record a batch of verified hashes (one sidecar batch, one index transaction).
    fn record_verified(
        &self,
        batch: &mut Vec<Verified>,
        report: &mut HashReport,
    ) -> Result<(), DsError> {
        if batch.is_empty() {
            return Ok(());
        }
        let recorded = lock(&self.session).accept_verified(batch)?.len();
        report.skipped += (batch.len() - recorded) as u64;
        batch.clear();
        Ok(())
    }

    fn ids(ids: &[i64]) -> Vec<VariantId> {
        ids.iter().copied().map(VariantId).collect()
    }

    fn parse(json: &str) -> Result<Adjustments, DsError> {
        parse_adjustments(json)
    }

    pub(crate) fn to_json(a: &Adjustments) -> Result<String, DsError> {
        serde_json::to_string(a).map_err(|e| DsError::Internal {
            message: format!("adjustments JSON: {e}"),
        })
    }

    fn edited_json(&self) -> Result<AllAdjustmentsJson, DsError> {
        let s = lock(&self.session);
        // Every non-default edit, not only the badged ones: an untouched Neutral edit
        // must reach the app, or it would render as the default (Standard). Those are
        // a legacy session's every photo, so they go as ids, not sidecar reads.
        let edits = s
            .variants_with_adjustments()?
            .into_iter()
            .map(|v| {
                Ok(AdjustmentsJson {
                    variant_id: v.0,
                    json: Self::to_json(&s.load_adjustments(v)?)?,
                })
            })
            .collect::<Result<_, DsError>>()?;
        Ok(AllAdjustmentsJson {
            edits,
            neutral_ids: s
                .neutral_default_variants()?
                .into_iter()
                .map(|v| v.0)
                .collect(),
            neutral_json: Self::to_json(&ds_session::neutral_default())?,
            device_generation: crate::remote::device_generation(), // under the session lock
        })
    }

    /// Image ids behind variant ids (unknown variants are skipped).
    fn images_of(&self, ids: &[i64]) -> Vec<ImageId> {
        let s = lock(&self.session);
        let mut out = Vec::new();
        for v in Self::ids(ids) {
            if let Ok(info) = s.variant(v)
                && !out.contains(&info.image)
            {
                out.push(info.image);
            }
        }
        out
    }

    pub(crate) fn locate_with(
        &self,
        probe: &Probe,
        variant_id: i64,
        path: &Path,
    ) -> Result<(), DsError> {
        let folder = self
            .in_place
            .as_ref()
            .ok_or_else(|| DsError::invalid("locate only applies to folders opened in place"))?;
        let path = path.canonicalize()?;
        let rest = path.strip_prefix(folder).map_err(|_| {
            DsError::invalid(format!(
                "{} is not inside {}",
                path.display(),
                folder.display()
            ))
        })?;
        let rel = RelPath::new(&format!("Capture/{}", rest.to_string_lossy()))?;
        let image = self
            .images_of(&[variant_id])
            .first()
            .copied()
            .ok_or_else(|| DsError::invalid(format!("unknown variant {variant_id}")))?;
        let info = lock(&self.session).image(image)?;
        if info.hash == UNHASHED {
            return Err(DsError::Unsupported {
                message: "this photo was never verified, so its file can't be recognised; \
                          remove it and let the new file be added"
                    .into(),
            });
        }
        let now = hash_with(probe, &path, &|| false)?;
        if identity::verdict(info.hash, info.content_hash, &now) == Verdict::Changed {
            return Err(DsError::invalid(format!(
                "{} is a different photo",
                path.display()
            )));
        }
        let mut s = lock(&self.session);
        s.relink_external(image, &rel)?;
        if let Some(stat) = original_stat(&path) {
            // A metadata-edited file: its new whole-file hash is the identity now.
            s.accept_verified(&[Verified {
                image,
                stat,
                hashes: now,
            }])?;
        }
        Ok(())
    }

    /// The image behind a variant id.
    fn image_of(&self, variant_id: i64) -> Result<ImageId, DsError> {
        self.images_of(&[variant_id])
            .first()
            .copied()
            .ok_or_else(|| DsError::invalid(format!("unknown variant {variant_id}")))
    }
}

/// `ds_core::Adjustments::default()` as JSON: the base the app overlays edits
/// onto when it hasn't read a variant's JSON yet (and a schema fixture for tests).
#[uniffi::export]
pub fn default_adjustments_json() -> Result<String, DsError> {
    guard(|| SessionHandle::to_json(&Adjustments::default()))
}

/// Round-trip `json` through `ds_core::Adjustments` (sanitized, unknown fields
/// dropped): what a sidecar would store. Lets the app verify its codec.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned strings
pub fn normalize_adjustments_json(json: String) -> Result<String, DsError> {
    guard(|| SessionHandle::to_json(&SessionHandle::parse(&json)?))
}

/// `ds_core::Adjustments` from the app's JSON (sidecar form).
pub(crate) fn parse_adjustments(json: &str) -> Result<Adjustments, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("adjustments JSON: {e}")))
}

/// Whether sidecars may be written into `folder` (a local, writable APFS/HFS+
/// volume). The app uses this to explain where an in-place session is stored.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn folder_can_hold_session(folder: String) -> Result<bool, DsError> {
    guard(|| Ok(sidecar_volume_ok(Path::new(&folder)).is_ok()))
}

/// Open an existing session folder (created by import or `Session::create`).
#[uniffi::export]
pub async fn open_session(
    dir: String,
    token: Arc<CancelToken>,
) -> Result<Arc<SessionHandle>, DsError> {
    run_job(token, move |_| {
        let mut s = Session::open(Path::new(&dir))?;
        s.ensure_builtin_albums()?;
        // originals an import copied but couldn't add are added now.
        let recovered = crate::import::recover_unregistered(&mut s);
        let mut h = SessionHandle::plain(s);
        h.report.added = recovered.added;
        h.report.failed = recovered.failed;
        Ok(Arc::new(h))
    })
    .await
}

/// Create an empty session at `dir` (Capture/, Selects/, Output/, Trash/ and its
/// settings) named `name`, and open it. `dir` must not exist or be empty
/// (`InvalidArgument` otherwise, as for a bad name); nothing is ever overwritten.
/// Missing parent folders are created.
#[uniffi::export]
pub async fn create_session(
    dir: String,
    name: String,
    token: Arc<CancelToken>,
) -> Result<Arc<SessionHandle>, DsError> {
    run_job(token, move |_| {
        let mut s = Session::create(Path::new(&dir), &name)?;
        s.ensure_builtin_albums()?;
        Ok(Arc::new(SessionHandle::plain(s)))
    })
    .await
}

/// Open `folder` as a session in place (see the module docs). `fallback_store`
/// is where the index and sidecars go when the folder's volume can't hold them
/// (cards, read-only media, network shares); without it such folders fail
/// with `Unsupported` and nothing is written anywhere. `folder_identity` is
/// the app's identity for the folder (volume identity plus path): a fallback
/// store made for another folder is never taken over (a separate store is
/// used), unless `moved` says a bookmark proved this is that folder, moved.
/// Progress events: `kind` "open.scan" then "open.register".
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn open_folder_in_place(
    folder: String,
    fallback_store: Option<String>,
    folder_identity: Option<String>,
    moved: bool,
    auto_apply: Option<AutoApplyPreset>,
    listener: Option<Arc<dyn ProgressListener>>,
    token: Arc<CancelToken>,
) -> Result<Arc<SessionHandle>, DsError> {
    let events = Arc::new(Dispatcher::new());
    events.set_listener(listener);
    let ev = events.clone();
    let r = run_job(token, move |cancelled| {
        let preset = auto_apply
            .as_ref()
            .map(AutoApplyPreset::to_preset)
            .transpose()?;
        emit(&ev, 1, "open", TaskPhase::Started, 0, 0);
        let mut last = (0, 0);
        let fb = fallback_store.as_deref().map(Path::new);
        let opts = OpenOptions {
            identity: folder_identity,
            allow_move: moved,
        };
        let r = open_in_place_with(
            Path::new(&folder),
            fb,
            &opts,
            preset,
            &Probe::RAW,
            cancelled,
            &mut |st, d, t| {
                last = (d, t);
                let kind = match st {
                    Stage::Scanning => "open.scan",
                    Stage::Registering => "open.register",
                };
                emit(&ev, 1, kind, TaskPhase::Running, d, t);
            },
        );
        let phase = match &r {
            Ok(_) => TaskPhase::Finished,
            Err(DsError::Cancelled) => TaskPhase::Cancelled,
            Err(_) => TaskPhase::Failed,
        };
        emit(&ev, 1, "open", phase, last.0, last.1);
        r.map(Arc::new)
    })
    .await;
    events.flush(std::time::Duration::from_secs(5));
    r
}

#[uniffi::export]
impl SessionHandle {
    /// Session name.
    pub fn name(&self) -> Result<String, DsError> {
        guard(|| Ok(lock(&self.session).name().to_owned()))
    }

    /// Where the index and sidecars are.
    pub fn root(&self) -> Result<String, DsError> {
        guard(|| Ok(lock(&self.session).root().to_string_lossy().into_owned()))
    }

    /// The user's folder for an in-place session.
    pub fn in_place_folder(&self) -> Result<Option<String>, DsError> {
        guard(|| {
            Ok(self
                .in_place
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()))
        })
    }

    /// What the open that produced this handle did.
    pub fn open_report(&self) -> Result<OpenReport, DsError> {
        guard(|| Ok(self.report.clone()))
    }

    /// Every variant (missing originals included, flagged), sorted by capture
    /// time then file name. Metadata comes from the session's cache.
    pub async fn variants(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<Vec<VariantRecord>, DsError> {
        run_job(token, move |cancelled| {
            let mut v = self.list(&Probe::RAW, cancelled)?;
            v.sort_by(|a, b| {
                (a.capture_time_ms, &a.file_name, a.variant_id).cmp(&(
                    b.capture_time_ms,
                    &b.file_name,
                    b.variant_id,
                ))
            });
            Ok(v)
        })
        .await
    }

    /// The background identity pass after an open (see the module docs):
    /// runs on its own low-priority threads, pauses while the user is active
    /// ([`crate::note_user_activity`]), and reports what changed
    /// (`listing_changed`: list the photos again). Progress events `kind` =
    /// "hash" (determinate). Cancelling keeps the remaining work for the next call.
    pub async fn hash_pending(
        self: Arc<Self>,
        listener: Option<Arc<dyn ProgressListener>>,
        token: Arc<CancelToken>,
    ) -> Result<HashReport, DsError> {
        let events = Arc::new(Dispatcher::new());
        events.set_listener(listener);
        let ev = events.clone();
        let r = crate::jobs::run_job_on_thread("ds-identity", token, move |cancelled| {
            self.hash_pending_with(&Probe::RAW, cancelled, &mut |d, t| {
                emit(&ev, 2, "hash", TaskPhase::Running, d, t);
            })
        })
        .await;
        events.flush(std::time::Duration::from_secs(5));
        r
    }

    /// Drop variants' images from the session (only their sidecars and index
    /// rows; the files, if any, are never touched). Photos whose file is back
    /// on disk are kept.
    pub async fn remove_missing(
        self: Arc<Self>,
        ids: Vec<i64>,
        token: Arc<CancelToken>,
    ) -> Result<u64, DsError> {
        run_job(token, move |_| {
            self.check_folder()?; // a drive that dropped off: nothing is forgotten
            let images = self.images_of(&ids);
            Ok(lock(&self.session).forget_missing(&images)? as u64)
        })
        .await
    }

    /// "File changed": put back the metadata (ratings, tags, edits, ids) of the
    /// photo this file replaced; it now describes this file.
    pub async fn restore_previous(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let image = self.image_of(variant_id)?;
            lock(&self.session).restore_replaced(image)?;
            Ok(())
        })
        .await
    }

    /// "File changed": keep it as the new photo it is (clears the flag).
    pub async fn dismiss_file_changed(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let image = self.image_of(variant_id)?;
            Ok(lock(&self.session).dismiss_file_changed(image)?)
        })
        .await
    }

    /// Move a missing photo's only copy (kept in the store by a migration)
    /// back into the folder; never overwrites a file that is there.
    pub async fn restore_to_folder(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let image = self.image_of(variant_id)?;
            Ok(lock(&self.session).restore_kept_copy(image)?)
        })
        .await
    }

    /// Point a missing photo at `path` (a file in the folder with the same content).
    pub async fn locate(
        self: Arc<Self>,
        variant_id: i64,
        path: String,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.locate_with(&Probe::RAW, variant_id, Path::new(&path))
        })
        .await
    }

    /// Set (or with `None`, clear) the preset applied to every file registered
    /// from now on (by the next open of an in-place session).
    pub async fn set_auto_apply_preset_json(
        self: Arc<Self>,
        preset: Option<AutoApplyPreset>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            let preset = preset
                .as_ref()
                .map(AutoApplyPreset::to_preset)
                .transpose()?;
            Ok(lock(&self.session).set_auto_apply_preset(preset)?)
        })
        .await
    }

    pub async fn set_rating(
        self: Arc<Self>,
        ids: Vec<i64>,
        rating: u8,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        let r = Rating::new(rating)
            .ok_or_else(|| DsError::invalid(format!("rating {rating} out of 0..=5")))?;
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_rating(&Self::ids(&ids), r)?)
        })
        .await
    }

    pub async fn set_color_tag(
        self: Arc<Self>,
        ids: Vec<i64>,
        tag: ColorLabel,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_color_tag(&Self::ids(&ids), tag.into())?)
        })
        .await
    }

    pub async fn set_pick(
        self: Arc<Self>,
        ids: Vec<i64>,
        pick: PickFlag,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_pick(&Self::ids(&ids), pick.into())?)
        })
        .await
    }

    /// Store why AI Cull rejected `ids` (`None` clears it). Kept only on rejected
    /// variants; `set_pick` clears it. Not an undo step.
    pub async fn set_cull_note(
        self: Arc<Self>,
        ids: Vec<i64>,
        note: Option<String>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_cull_note(&Self::ids(&ids), note.as_deref())?)
        })
        .await
    }

    /// Replace the keywords of `ids` (trimmed, de-duplicated, order kept; undoable).
    pub async fn set_keywords(
        self: Arc<Self>,
        ids: Vec<i64>,
        keywords: Vec<String>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_keywords(&Self::ids(&ids), &keywords)?)
        })
        .await
    }

    /// Keep `report_json` (an AI Optimize report, see `optimize_apply`) as the last
    /// applied report of `ids`; `None` clears it. Not an undo step.
    pub async fn set_optimize_report(
        self: Arc<Self>,
        ids: Vec<i64>,
        report_json: Option<String>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let report = report_json
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .map_err(|e| DsError::invalid(format!("optimize report JSON: {e}")))?;
            Ok(lock(&self.session).set_optimize_report(&Self::ids(&ids), report.as_ref())?)
        })
        .await
    }

    /// The last applied AI Optimize report of a variant (JSON), if any.
    pub async fn optimize_report(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<Option<String>, DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session)
                .optimize_report(VariantId(variant_id))?
                .map(|v| v.to_string()))
        })
        .await
    }

    /// Keep `changes_json` (the app's record of the rows AI edits changed and their
    /// values before) for `ids`; `None` clears it. Not an undo step.
    pub async fn set_ai_changes(
        self: Arc<Self>,
        ids: Vec<i64>,
        changes_json: Option<String>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let changes = changes_json
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .map_err(|e| DsError::invalid(format!("AI changes JSON: {e}")))?;
            Ok(lock(&self.session).set_ai_changes(&Self::ids(&ids), changes.as_ref())?)
        })
        .await
    }

    /// The AI changes kept for a variant (JSON), if any.
    pub async fn ai_changes(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<Option<String>, DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session)
                .ai_changes(VariantId(variant_id))?
                .map(|v| v.to_string()))
        })
        .await
    }

    /// Whether originals can be moved between the session's folders (Capture,
    /// Selects, Trash) and the Trash emptied: sessions created by import only.
    /// A folder opened in place holds the user's own files: never moved.
    pub fn can_move_originals(&self) -> bool {
        self.in_place.is_none()
    }

    /// Move the originals of `ids` (with all their variants' sidecars) into
    /// `folder` of the session: "Move Picked to Selects",
    /// "Restore from Session Trash". Nothing is ever overwritten or deleted; a
    /// failure rolls every rename back. Returns the originals moved (for undo,
    /// [`Self::move_back`]). Refused for folders opened in place.
    pub async fn move_to_folder(
        self: Arc<Self>,
        ids: Vec<i64>,
        folder: SessionFolder,
        token: Arc<CancelToken>,
    ) -> Result<Vec<FolderMoveRecord>, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let mut s = lock(&self.session);
            let moved = s.move_to(&Self::ids(&ids), folder.into())?;
            move_records(&s, moved)
        })
        .await
    }

    /// "Move Rejected to Trash": an original goes to the session's
    /// `Trash/` only when every variant of it is rejected (the archive's rule);
    /// a photo with a kept variant stays where it is and only its rejected
    /// variants are deleted (recoverable). Refused for folders opened in place.
    pub async fn trash_rejected(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<TrashRejectedReport, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let mut s = lock(&self.session);
            let done = s.trash_rejected()?;
            Ok(TrashRejectedReport {
                moved: move_records(&s, done.moved)?,
                hidden: done.hidden.iter().map(|v| v.0).collect(),
                error: done.error.map(|e| e.to_string()),
            })
        })
        .await
    }

    /// Undo of a move: each original goes back to the path it came
    /// from (a free `-2` name there if it was taken meanwhile), and the deleted
    /// variants in `hidden` come back. Returns how many originals moved.
    pub async fn move_back(
        self: Arc<Self>,
        moves: Vec<FolderMoveRecord>,
        hidden: Vec<i64>,
        token: Arc<CancelToken>,
    ) -> Result<u64, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let back = moves
                .iter()
                .map(|m| Ok((VariantId(m.variant_id), RelPath::new(&m.from)?)))
                .collect::<Result<Vec<_>, ds_session::SessionError>>()?;
            let mut s = lock(&self.session);
            let n = s.move_back(&back)?.len();
            s.restore_variants(&Self::ids(&hidden))?;
            Ok(n as u64)
        })
        .await
    }

    /// "Restore from Session Trash", variants part: every variant
    /// set aside in `Trash/.ds/deleted/` comes back next to its original (found
    /// on disk, so it works after a relaunch). Returns how many came back.
    pub async fn restore_set_aside_variants(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<u64, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let mut s = lock(&self.session);
            let aside = s.set_aside_variants()?;
            Ok(s.restore_variants(&aside)? as u64)
        })
        .await
    }

    /// "Delete Rejected…": hand everything in the session's `Trash/` to the macOS
    /// Trash (`NSFileManager`, recoverable) and drop it from the session. Returns
    /// how many originals left. A volume without a Trash refuses and nothing is
    /// deleted. Refused for folders opened in place.
    pub async fn empty_trash(self: Arc<Self>, token: Arc<CancelToken>) -> Result<u64, DsError> {
        run_job(token, move |_| {
            self.empty_trash_with(&ds_session::SystemTrash)
        })
        .await
    }

    /// A new variant of the photo behind `variant_id` (its edits copied, culling
    /// metadata fresh). Returns the new variant's id.
    pub async fn new_variant(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<i64, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let mut s = lock(&self.session);
            let image = s.variant(VariantId(variant_id))?.image;
            Ok(s.new_variant(image)?.0)
        })
        .await
    }

    /// `ds_core::Adjustments` JSON of one variant.
    pub async fn load_adjustments_json(
        self: Arc<Self>,
        variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<String, DsError> {
        run_job(token, move |_| {
            Self::to_json(&lock(&self.session).load_adjustments(VariantId(variant_id))?)
        })
        .await
    }

    /// Adjustments of every variant not at the default: the edits as JSON, and the
    /// variants at the default with the Neutral profile (sidecars from before profiles)
    /// as ids sharing one JSON. Variants at the default are omitted.
    pub async fn load_all_adjustments_json(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<AllAdjustmentsJson, DsError> {
        run_job(token, move |_| self.edited_json()).await
    }

    /// Save one variant's adjustments (atomic sidecar write; coalesced undo).
    pub async fn save_adjustments_json(
        self: Arc<Self>,
        variant_id: i64,
        json: String,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let adj = Self::parse(&json)?;
            let mut s = lock(&self.session);
            let v = VariantId(variant_id);
            crate::remote::keep_unseen_device_edits(&mut s, &[(v, &adj)])?; // [remote]
            Ok(s.save_adjustments(v, &adj)?)
        })
        .await
    }

    /// Save several variants' adjustments as one undo step, all-or-nothing
    /// across crashes (ds-session journals multi-sidecar batches). Returns
    /// whether anything changed.
    pub async fn apply_batch_json(
        self: Arc<Self>,
        edits: Vec<AdjustmentsJson>,
        token: Arc<CancelToken>,
    ) -> Result<bool, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let parsed = edits
                .iter()
                .map(|e| Ok((VariantId(e.variant_id), Self::parse(&e.json)?)))
                .collect::<Result<Vec<_>, DsError>>()?;
            let unique: HashSet<VariantId> = parsed.iter().map(|(v, _)| *v).collect();
            if unique.len() != parsed.len() {
                return Err(DsError::invalid("duplicate variant id in batch"));
            }
            let mut s = lock(&self.session);
            let edits: Vec<_> = parsed.iter().map(|(v, a)| (*v, a)).collect();
            crate::remote::keep_unseen_device_edits(&mut s, &edits)?; // [remote]
            Ok(s.apply_batch(&parsed)?.is_some())
        })
        .await
    }
}

// ------------------------------------------------------------ smart folders ---

/// A smart-folder definition (`ds_session::Filter`). Ratings are 0...5 stars;
/// `min_rating == max_rating` is an exact rating, `max_rating == 0` = unrated.
#[derive(Debug, Clone, PartialEq, Default, uniffi::Record)]
pub struct SmartFilterRecord {
    pub min_rating: Option<u8>,
    pub max_rating: Option<u8>,
    /// Any of these labels (empty = any).
    pub tags: Vec<ColorLabel>,
    pub pick: Option<PickFlag>,
    pub text: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub date_from_ms: Option<i64>,
    pub date_to_ms: Option<i64>,
    pub iso_min: Option<u32>,
    pub iso_max: Option<u32>,
    pub edited: Option<bool>,
    /// All of these keywords (whole keyword, case-insensitive).
    pub keywords: Vec<String>,
}

impl SmartFilterRecord {
    fn to_filter(&self) -> Result<Filter, DsError> {
        let rating = |r: Option<u8>| {
            r.map(|n| {
                Rating::new(n).ok_or_else(|| DsError::invalid(format!("rating {n} out of 0..=5")))
            })
            .transpose()
        };
        Ok(Filter {
            min_rating: rating(self.min_rating)?,
            max_rating: rating(self.max_rating)?,
            tags: self.tags.iter().map(|t| (*t).into()).collect(),
            pick: self.pick.map(Into::into),
            text: self.text.clone(),
            camera: self.camera.clone(),
            lens: self.lens.clone(),
            date_from_ms: self.date_from_ms,
            date_to_ms: self.date_to_ms,
            iso_min: self.iso_min,
            iso_max: self.iso_max,
            edited: self.edited,
            folder: None,
            keywords: self.keywords.clone(),
        })
    }

    fn from_filter(f: &Filter) -> Self {
        Self {
            min_rating: f.min_rating.map(Rating::stars),
            max_rating: f.max_rating.map(Rating::stars),
            tags: f.tags.iter().map(|t| (*t).into()).collect(),
            pick: f.pick.map(Into::into),
            text: f.text.clone(),
            camera: f.camera.clone(),
            lens: f.lens.clone(),
            date_from_ms: f.date_from_ms,
            date_to_ms: f.date_to_ms,
            iso_min: f.iso_min,
            iso_max: f.iso_max,
            edited: f.edited,
            keywords: f.keywords.clone(),
        }
    }
}

/// One smart folder with its current member count.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SmartFolderRecord {
    pub id: i64,
    pub name: String,
    /// Stable key of a built-in (`"rating:5"`, `"unrated"`, `"picked"`,
    /// `"rejected"`, `"tag:red"`); `None` for the user's own folders.
    pub builtin: Option<String>,
    pub mirror: bool,
    pub filter: SmartFilterRecord,
    pub count: u64,
}

#[uniffi::export]
impl SessionHandle {
    /// Every smart folder (built-ins first, in their canonical order, then the
    /// user's), with counts. Creates missing built-ins first (a newly used color
    /// tag gets its folder here).
    pub async fn smart_folders(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<Vec<SmartFolderRecord>, DsError> {
        run_job(token, move |_| {
            let mut s = lock(&self.session);
            s.ensure_builtin_albums()?;
            let order: Vec<String> = ds_session::builtin_smart_folders(&s.tags_in_use()?)
                .into_iter()
                .map(|b| b.0)
                .collect();
            let mut out = Vec::new();
            for a in s.albums() {
                let AlbumKind::Smart { filter, .. } = &a.kind else {
                    continue;
                };
                out.push(SmartFolderRecord {
                    id: a.id.0,
                    name: a.name.clone(),
                    builtin: a.builtin.clone(),
                    mirror: a.mirror,
                    filter: SmartFilterRecord::from_filter(filter),
                    count: s.query(filter, Sort::default())?.len() as u64,
                });
            }
            let rank = |r: &SmartFolderRecord| {
                r.builtin
                    .as_ref()
                    .and_then(|k| order.iter().position(|o| o == k))
                    .unwrap_or(usize::MAX)
            };
            out.sort_by_key(|r| rank(r));
            Ok(out)
        })
        .await
    }

    /// Variant ids in a smart folder, in capture order.
    pub async fn smart_folder_members(
        self: Arc<Self>,
        id: i64,
        token: Arc<CancelToken>,
    ) -> Result<Vec<i64>, DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session)
                .album_variants(AlbumId(id))?
                .into_iter()
                .map(|v| v.0)
                .collect())
        })
        .await
    }

    /// Variant ids matching an ad-hoc filter (the app asks only for criteria it
    /// can't evaluate itself, e.g. keywords and text).
    pub async fn query_filter(
        self: Arc<Self>,
        filter: SmartFilterRecord,
        token: Arc<CancelToken>,
    ) -> Result<Vec<i64>, DsError> {
        run_job(token, move |_| {
            let f = filter.to_filter()?;
            Ok(lock(&self.session)
                .query(&f, Sort::default())?
                .into_iter()
                .map(|v| v.0)
                .collect())
        })
        .await
    }

    /// Save a new user smart folder; returns its id.
    pub async fn create_smart_folder(
        self: Arc<Self>,
        name: String,
        filter: SmartFilterRecord,
        token: Arc<CancelToken>,
    ) -> Result<i64, DsError> {
        run_job(token, move |_| {
            let f = filter.to_filter()?;
            Ok(lock(&self.session)
                .create_smart_album(&name, f, Sort::default())?
                .0)
        })
        .await
    }

    /// Rename / redefine a user smart folder (built-ins refuse).
    pub async fn update_smart_folder(
        self: Arc<Self>,
        id: i64,
        name: String,
        filter: SmartFilterRecord,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            let f = filter.to_filter()?;
            Ok(lock(&self.session).update_smart_album(AlbumId(id), &name, f)?)
        })
        .await
    }

    /// Delete a user smart folder (built-ins refuse).
    pub async fn delete_smart_folder(
        self: Arc<Self>,
        id: i64,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session).delete_album(AlbumId(id))?)
        })
        .await
    }

    /// Turn "Mirror to Finder" on/off (the app maintains the symlink folder).
    pub async fn set_smart_folder_mirror(
        self: Arc<Self>,
        id: i64,
        on: bool,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session).set_album_mirror(AlbumId(id), on)?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fake_supported(p: &Path) -> bool {
        p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
    }

    fn fake_read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
        let bytes = fs::read(p).ok()?;
        if bytes.starts_with(b"BAD") {
            return None;
        }
        let lite = ImageMetaLite {
            capture_time_ms: Some(i64::from(bytes.len() as u32)),
            camera: Some("SONY ILCE-9".into()),
            width: 6000,
            height: 4000,
            ..Default::default()
        };
        let full = RawMetadata {
            model: format!("len{}", bytes.len()),
            ..RawMetadata::default()
        };
        Some((lite, full))
    }

    const FAKE: Probe = Probe {
        is_supported: fake_supported,
        read: fake_read,
        volume_ok: |_| Ok(()),
        ranges: |_| None,
        untrusted_stat: |_| false,
        is_raw_name: fake_supported,
    };

    /// Whole-file BLAKE3 (what the tests' sessions record).
    fn hash_file(p: &Path) -> io::Result<FileHash> {
        identity::hash_original(p, None, &|| false).map(|h| h.file)
    }
    const CARD: Probe = Probe {
        volume_ok: |_| Err("exfat volumes (memory cards) are never written to".into()),
        ..FAKE
    };

    pub(super) fn folder(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, bytes) in files {
            let p = d.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
        d
    }

    pub(super) fn open(dir: &Path) -> SessionHandle {
        open_in_place(dir, None, None, &FAKE, &|| false, &mut |_, _, _| {}).unwrap()
    }

    pub(super) fn list(h: &SessionHandle) -> Vec<VariantRecord> {
        h.list(&FAKE, &|| false).unwrap()
    }

    pub(super) fn by_name<'a>(v: &'a [VariantRecord], n: &str) -> &'a VariantRecord {
        v.iter().find(|r| r.file_name == n).unwrap()
    }

    /// Every file under `dir` (relative path → bytes).
    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push((
                        p.strip_prefix(dir).unwrap().to_owned(),
                        fs::read(&p).unwrap(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// A session left on a volume that may not be written (card, read-only, network)
    /// is never migrated or updated there: the store goes to the fallback and the
    /// folder, including its existing `.ds`, stays byte-identical.
    #[test]
    fn existing_session_on_unwritable_volume_is_left_untouched() {
        let d = folder(&[("a.ARW", b"aaaa"), ("b.ARW", b"bb")]);
        drop(open(d.path())); // an older in-place session exists on the volume
        let before = snapshot(d.path());
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("store");
        let h = open_in_place(
            d.path(),
            Some(&fb),
            None,
            &CARD,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap();
        let v = h.list(&CARD, &|| false).unwrap();
        assert_eq!(v.len(), 2);
        block_on(Arc::new(h).set_rating(vec![v[0].variant_id], 4, CancelToken::new())).unwrap();
        assert!(has_session(&fb), "store written to the fallback");
        assert_eq!(snapshot(d.path()), before, "nothing on the volume changed");
    }

    /// A Capture One session folder never gets a new store (nor
    /// anything else) written into it; it goes to the fallback. A store an earlier
    /// version made in one keeps being used (its edits aren't lost).
    #[test]
    fn capture_one_session_folders_are_never_written() {
        let d = folder(&[
            ("Shoot.cosessiondb", b"db"),
            ("Capture/a.ARW", b"aaaa"),
            ("CaptureOne/Settings153/a.ARW.cos", b"c1"),
        ]);
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("InPlace/UUID/hash");
        let before = snapshot(d.path());
        let refused = open_in_place(d.path(), None, None, &FAKE, &|| false, &mut |_, _, _| {});
        assert!(
            matches!(&refused, Err(DsError::Unsupported { message }) if message.contains("Capture One")),
            "{refused:?}"
        );
        let h = open_in_place(
            d.path(),
            Some(&fb),
            None,
            &FAKE,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap();
        assert!(!h.report.store_on_volume);
        assert_eq!(PathBuf::from(&h.report.store), fb);
        let v = list(&h);
        assert_eq!(v.len(), 1, "only the photo under Capture/");
        block_on(Arc::new(h).set_rating(vec![v[0].variant_id], 4, CancelToken::new())).unwrap();
        assert_eq!(
            snapshot(d.path()),
            before,
            "nothing in the Capture One folder changed"
        );
        // An existing in-folder store (made before this rule) is kept.
        let old = folder(&[("Shoot.cosessiondb", b"db"), ("Capture/b.ARW", b"bb")]);
        fs::remove_file(old.path().join("Shoot.cosessiondb")).unwrap();
        drop(open(old.path()));
        fs::write(old.path().join("Shoot.cosessiondb"), b"db").unwrap();
        let again = open(old.path());
        assert!(
            again.report.store_on_volume,
            "the earlier store keeps its edits"
        );
    }

    #[test]
    fn opens_in_place_without_touching_the_photos() {
        let d = folder(&[
            ("a.ARW", b"aaaa"),
            ("b.arw", b"bb"),
            ("notes.txt", b"x"),
            (".hidden.ARW", b"h"),
            ("DCIM/100MSDCF/c.ARW", b"ccc"),
        ]);
        let before = fs::read(d.path().join("a.ARW")).unwrap();
        let h = open(d.path());
        assert_eq!((h.report.added, h.report.existing), (3, 0));
        assert!(h.report.store_on_volume);
        assert!(h.report.store.ends_with(".ds/session"));
        let v = list(&h);
        let mut names: Vec<_> = v.iter().map(|r| r.file_name.clone()).collect();
        names.sort();
        assert_eq!(names, ["a.ARW", "b.arw", "c.ARW"]);
        let a = by_name(&v, "a.ARW");
        let canon = d.path().canonicalize().unwrap();
        assert_eq!(PathBuf::from(&a.original_path), canon.join("a.ARW"));
        assert!(!a.missing && !a.file_changed && a.width == 6000);
        assert_eq!(a.capture_time_ms, Some(4));
        assert_eq!(a.meta.as_ref().unwrap().model, "len4");
        assert!(
            !d.path().join(".ds/session/Capture/a.ARW").exists(),
            "no links or copies of originals"
        );
        block_on(Arc::new(h).set_rating(vec![a.variant_id], 4, CancelToken::new())).unwrap();
        let again = open(d.path());
        assert_eq!((again.report.added, again.report.existing), (0, 3));
        let a2 = list(&again)
            .into_iter()
            .find(|r| r.file_name == "a.ARW")
            .unwrap();
        assert_eq!((a2.variant_id, a2.rating), (a.variant_id, 4));
        assert_eq!(fs::read(d.path().join("a.ARW")).unwrap(), before);
    }

    #[test]
    fn cards_keep_the_session_elsewhere_with_zero_writes() {
        let d = folder(&[("DCIM/a.ARW", b"aaaa"), ("DCIM/b.ARW", b"bb")]);
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("InPlace/UUID/hash");
        let before = snapshot(d.path());
        let refused = open_in_place(d.path(), None, None, &CARD, &|| false, &mut |_, _, _| {});
        assert!(
            matches!(refused, Err(DsError::Unsupported { .. })),
            "{refused:?}"
        );
        let h = Arc::new(
            open_in_place(
                d.path(),
                Some(&fb),
                None,
                &CARD,
                &|| false,
                &mut |_, _, _| {},
            )
            .unwrap(),
        );
        assert!(!h.report.store_on_volume);
        assert_eq!(PathBuf::from(&h.report.store), fb);
        let id = by_name(&list(&h), "a.ARW").variant_id;
        block_on(h.clone().set_rating(vec![id], 5, CancelToken::new())).unwrap();
        block_on(
            h.clone()
                .set_color_tag(vec![id], ColorLabel::Pink, CancelToken::new()),
        )
        .unwrap();
        drop(h);
        assert_eq!(snapshot(d.path()), before, "the card is byte-identical");
        assert!(!d.path().join(".ds").exists());
        // Reopen through the fallback: ratings persisted there.
        let again = open_in_place(
            d.path(),
            Some(&fb),
            None,
            &CARD,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap();
        let a = list(&again)
            .into_iter()
            .find(|r| r.file_name == "a.ARW")
            .unwrap();
        assert_eq!((a.rating, a.color_tag), (5, ColorLabel::Pink));
        assert_eq!(again.report.existing, 2);
    }

    #[test]
    fn a_failed_or_cancelled_first_open_leaves_no_debris() {
        let d = folder(&[("a.ARW", b"a")]);
        let before = snapshot(d.path());
        let r = open_in_place(d.path(), None, None, &FAKE, &|| true, &mut |_, _, _| {});
        assert_eq!(r.err(), Some(DsError::Cancelled));
        assert_eq!(snapshot(d.path()), before);
        assert!(!d.path().join(".ds").exists());
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("fb");
        let r = open_in_place(
            d.path(),
            Some(&fb),
            None,
            &CARD,
            &|| true,
            &mut |_, _, _| {},
        );
        assert_eq!(r.err(), Some(DsError::Cancelled));
        assert!(!fb.exists(), "fallback store removed too");
        // An existing `.ds` of the user's is never removed.
        fs::create_dir(d.path().join(".ds")).unwrap();
        fs::write(d.path().join(".ds/keep"), b"k").unwrap();
        let r = open_in_place(d.path(), None, None, &FAKE, &|| true, &mut |_, _, _| {});
        assert!(r.is_err());
        assert!(d.path().join(".ds/keep").is_file());
        assert!(!d.path().join(".ds/session").exists());
    }

    #[test]
    fn progress_and_cancellation_after_the_first_batch() {
        let files: Vec<(String, Vec<u8>)> = (0..(ADD_BATCH + 3))
            .map(|i| (format!("f{i:03}.ARW"), vec![1; i + 1]))
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let d = folder(&refs);
        let mut seen = Vec::new();
        let calls = AtomicUsize::new(0);
        let h0 = open_in_place(d.path(), None, None, &FAKE, &|| false, &mut |s, a, b| {
            seen.push((s, a, b));
        })
        .unwrap();
        assert_eq!(seen.first(), Some(&(Stage::Scanning, 0, 0)));
        assert_eq!(seen.last(), Some(&(Stage::Registering, 259, 259)));
        assert_eq!(h0.report.added, 259);
        drop(h0);
        for i in 0..(ADD_BATCH + 3) {
            fs::write(d.path().join(format!("g{i:03}.ARW")), vec![2; i + 1]).unwrap();
        }
        // A later open (the store exists) cancelled after its first batch
        // keeps what it registered.
        let r = open_in_place(
            d.path(),
            None,
            None,
            &FAKE,
            &|| calls.fetch_add(1, Ordering::SeqCst) >= 2,
            &mut |_, _, _| {},
        );
        assert_eq!(r.err(), Some(DsError::Cancelled));
        let h = open(d.path());
        assert_eq!(
            (h.report.added, h.report.existing),
            (3, 259 + 256),
            "resumes"
        );
    }

    static COUNTED: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn metadata_is_cached_not_reread_per_listing() {
        const COUNTING: Probe = Probe {
            read: |p| {
                COUNTED.fetch_add(1, Ordering::SeqCst);
                fake_read(p)
            },
            ..FAKE
        };
        let open_c =
            |d: &Path| open_in_place(d, None, None, &COUNTING, &|| false, &mut |_, _, _| {});
        let d = folder(&[("a.ARW", b"aa"), ("b.ARW", b"bbb")]);
        let h = open_c(d.path()).unwrap();
        assert_eq!(
            COUNTED.load(Ordering::SeqCst),
            2,
            "one read per file at registration"
        );
        assert_eq!(h.list(&COUNTING, &|| false).unwrap().len(), 2);
        let h2 = open_c(d.path()).unwrap();
        let v = h2.list(&COUNTING, &|| false).unwrap();
        assert_eq!(
            COUNTED.load(Ordering::SeqCst),
            2,
            "listing and reopening read nothing"
        );
        assert_eq!(by_name(&v, "b.ARW").meta.as_ref().unwrap().model, "len3");
        // A lost cache is rebuilt by the next listing; unreadable files get no metadata.
        fs::remove_file(Path::new(&h2.report.store).join(META_CACHE)).unwrap();
        let h3 = open_c(d.path()).unwrap();
        let none = Probe {
            read: |_| None,
            ..FAKE
        };
        assert!(
            h3.list(&none, &|| false)
                .unwrap()
                .iter()
                .all(|r| r.meta.is_none())
        );
        let v = h3.list(&COUNTING, &|| false).unwrap();
        assert_eq!(COUNTED.load(Ordering::SeqCst), 4);
        assert!(v.iter().all(|r| r.meta.is_some()));
        assert_eq!(load_meta_cache(Path::new(&h3.report.store)).len(), 2);
        // A changed file is re-read with fresh metadata once the background
        // pass decides it is a different photo (never hashed, other capture
        // time); an unreadable replacement is listed without metadata.
        fs::write(d.path().join("a.ARW"), b"aaaaaa").unwrap();
        fs::write(d.path().join("b.ARW"), b"BAD, changed").unwrap();
        let h4 = open(d.path());
        assert_eq!((h4.report.to_verify, h4.report.added), (2, 0));
        let r = h4
            .hash_pending_with(&FAKE, &|| false, &mut |_, _| {})
            .unwrap();
        assert_eq!(r.changed, 2, "{r:?}");
        assert!(r.listing_changed);
        let v = list(&h4);
        assert_eq!(v.len(), 2);
        assert_eq!(by_name(&v, "a.ARW").meta.as_ref().unwrap().model, "len6");
        assert!(by_name(&v, "a.ARW").file_changed);
        assert!(by_name(&v, "b.ARW").meta.is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one Finder story, end to end
    fn identity_rename_replace_delete_and_locate() {
        let d = folder(&[
            ("a.ARW", b"alpha"),
            ("b.ARW", b"bravo"),
            ("c.ARW", b"charlie"),
        ]);
        let h = Arc::new(open(d.path()));
        let mut hashed = Vec::new();
        let rep = h
            .hash_pending_with(&FAKE, &|| false, &mut |a, b| hashed.push((a, b)))
            .unwrap();
        assert_eq!((rep.hashed, rep.skipped), (3, 0));
        assert_eq!(hashed.last(), Some(&(3, 3)));
        let v = list(&h);
        let (a, b, c) = (
            by_name(&v, "a.ARW").clone(),
            by_name(&v, "b.ARW").clone(),
            by_name(&v, "c.ARW").clone(),
        );
        for id in [a.variant_id, b.variant_id] {
            block_on(h.clone().set_rating(vec![id], 3, CancelToken::new())).unwrap();
        }
        drop(h);
        // Finder: rename a, replace b with different content, delete c.
        fs::rename(d.path().join("a.ARW"), d.path().join("a2.ARW")).unwrap();
        fs::remove_file(d.path().join("b.ARW")).unwrap();
        fs::write(d.path().join("b.ARW"), b"something else").unwrap();
        let away = tempfile::tempdir().unwrap();
        fs::rename(d.path().join("c.ARW"), away.path().join("c.ARW")).unwrap();
        let h = Arc::new(open(d.path()));
        assert_eq!(
            (h.report.relinked, h.report.to_verify, h.report.missing),
            (1, 1, 1),
            "decided by stat; b is checked in the background"
        );
        let v = list(&h);
        assert_eq!(by_name(&v, "b.ARW").rating, 3, "provisional until hashed");
        let r = h
            .hash_pending_with(&FAKE, &|| false, &mut |_, _| {})
            .unwrap();
        assert_eq!((r.changed, r.verified), (1, 0), "{r:?}");
        let v = list(&h);
        let a2 = by_name(&v, "a2.ARW");
        assert_eq!(
            (a2.variant_id, a2.rating),
            (a.variant_id, 3),
            "rename followed"
        );
        let nb = by_name(&v, "b.ARW").clone();
        assert!(nb.file_changed && nb.rating == 0 && nb.variant_id != b.variant_id);
        // The badge persists across a reopen; "restore" brings b's metadata back.
        let h2 = Arc::new(open(d.path()));
        assert!(by_name(&list(&h2), "b.ARW").file_changed);
        block_on(
            h2.clone()
                .restore_previous(nb.variant_id, CancelToken::new()),
        )
        .unwrap();
        let rb = by_name(&list(&h2), "b.ARW").clone();
        assert_eq!(
            (rb.variant_id, rb.rating, rb.file_changed),
            (b.variant_id, 3, false)
        );
        let e = block_on(
            h2.clone()
                .restore_previous(rb.variant_id, CancelToken::new()),
        );
        assert!(e.is_err(), "nothing left to restore");
        drop(h2);
        let gone = by_name(&v, "c.ARW");
        assert!(gone.missing);
        // Locate: wrong file refused, right one relinked.
        let wrong = block_on(h.clone().locate(
            c.variant_id,
            d.path().join("a2.ARW").to_string_lossy().into(),
            CancelToken::new(),
        ));
        assert!(
            matches!(wrong, Err(DsError::InvalidArgument { .. })),
            "{wrong:?}"
        );
        let outside = tempfile::NamedTempFile::new().unwrap();
        let e = block_on(h.clone().locate(
            c.variant_id,
            outside.path().to_string_lossy().into(),
            CancelToken::new(),
        ));
        assert!(matches!(e, Err(DsError::InvalidArgument { .. })));
        fs::rename(away.path().join("c.ARW"), d.path().join("c-found.ARW")).unwrap();
        // (a supported file would be auto-relinked on reopen; locate does it now)
        block_on(h.clone().locate(
            c.variant_id,
            d.path().join("c-found.ARW").to_string_lossy().into(),
            CancelToken::new(),
        ))
        .unwrap();
        let v = list(&h);
        let found = v.iter().find(|r| r.variant_id == c.variant_id).unwrap();
        assert_eq!(
            (found.file_name.as_str(), found.missing),
            ("c-found.ARW", false)
        );
        // Remove missing: drops the session's record only.
        fs::remove_file(d.path().join("c-found.ARW")).unwrap();
        assert_eq!(
            block_on(
                h.clone()
                    .remove_missing(vec![c.variant_id, 999], CancelToken::new())
            )
            .unwrap(),
            1
        );
        assert!(list(&h).iter().all(|r| r.variant_id != c.variant_id));
    }

    #[test]
    fn locate_needs_a_hash_and_an_in_place_session() {
        let d = folder(&[("a.ARW", b"alpha")]);
        let h = Arc::new(open(d.path()));
        let id = list(&h)[0].variant_id;
        let p = d.path().join("a.ARW").to_string_lossy().into_owned();
        let e = block_on(h.clone().locate(id, p.clone(), CancelToken::new()));
        assert!(matches!(e, Err(DsError::Unsupported { .. })), "{e:?}");
        let e = block_on(h.clone().locate(424_242, p, CancelToken::new()));
        assert!(matches!(e, Err(DsError::InvalidArgument { .. })));
        let s = tempfile::tempdir().unwrap();
        let plain = SessionHandle::plain(Session::create(&s.path().join("S"), "S").unwrap());
        assert!(plain.locate_with(&FAKE, 1, d.path()).is_err());
    }

    #[test]
    fn an_error_in_the_background_pass_keeps_the_pending_work() {
        let d = folder(&[("a.ARW", b"alpha")]);
        let h = open(d.path());
        lock(&h.pending).held.push("Capture/held.ARW".into());
        let before = lock(&h.pending).clone();
        h.requeue_on_error(|work| {
            assert_eq!(work.held, ["Capture/held.ARW"]);
            Err::<(), _>(DsError::invalid("database is busy"))
        })
        .unwrap_err();
        let after = lock(&h.pending).clone();
        assert_eq!(
            (after.verify, after.held),
            (before.verify, before.held),
            "nothing was lost to the failed pass"
        );
        // A cancel is the pass's own business: it puts back what is left, so
        // the wrapper adds no second copy.
        h.requeue_on_error(|_| Err::<(), _>(DsError::Cancelled))
            .unwrap_err();
        assert_eq!(lock(&h.pending).held.len(), 0);
    }

    #[test]
    fn hash_pending_skips_files_changing_underneath_and_cancels() {
        let d = folder(&[("a.ARW", b"alpha"), ("b.ARW", b"b")]);
        let h = open(d.path());
        assert_eq!(
            h.hash_pending_with(&FAKE, &|| true, &mut |_, _| {}).err(),
            Some(DsError::Cancelled)
        );
        fs::remove_file(d.path().join("b.ARW")).unwrap();
        let r = h
            .hash_pending_with(&FAKE, &|| false, &mut |_, _| {})
            .unwrap();
        assert_eq!(r.hashed, 1);
        assert_eq!(r.skipped + r.failed.len() as u64, 1);
        let again = h
            .hash_pending_with(&FAKE, &|| false, &mut |_, _| {})
            .unwrap();
        assert_eq!(again.hashed, 0);
    }

    #[test]
    fn folder_moved_while_open_is_reported_then_reopened() {
        let parent = tempfile::tempdir().unwrap();
        let f = parent.path().join("Shoot");
        fs::create_dir(&f).unwrap();
        fs::write(f.join("a.ARW"), b"alpha").unwrap();
        let h = Arc::new(open(&f));
        let id = list(&h)[0].variant_id;
        let moved = parent.path().join("Shoot renamed");
        fs::rename(&f, &moved).unwrap();
        let e = block_on(h.clone().set_rating(vec![id], 2, CancelToken::new()));
        assert!(matches!(e, Err(DsError::FolderMoved { .. })), "{e:?}");
        assert!(matches!(
            h.list(&FAKE, &|| false),
            Err(DsError::FolderMoved { .. })
        ));
        // Gone folder (a drive that dropped off): Remove forgets nothing.
        let e = block_on(h.clone().remove_missing(vec![id], CancelToken::new()));
        assert!(matches!(e, Err(DsError::FolderMoved { .. })), "{e:?}");
        // The app re-resolves its bookmark and reopens at the new place.
        let h = Arc::new(open(&moved));
        block_on(h.clone().set_rating(vec![id], 2, CancelToken::new())).unwrap();
        assert_eq!(list(&h)[0].rating, 2);
        assert_eq!(h.report.missing, 0);
        // A card session (store elsewhere) follows the folder too.
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("fb");
        drop(open_in_place(&moved, Some(&fb), None, &CARD, &|| false, &mut |_, _, _| {}).unwrap());
        let m2 = parent.path().join("again");
        fs::rename(&moved, &m2).unwrap();
        fs::remove_dir_all(m2.join(".ds")).unwrap();
        let h = open_in_place(&m2, Some(&fb), None, &CARD, &|| false, &mut |_, _, _| {}).unwrap();
        assert_eq!(h.report.missing, 0);
        assert_eq!(list(&h).len(), 1);
    }

    #[test]
    fn old_hard_link_sessions_are_migrated() {
        let d = folder(&[("a.ARW", b"alpha"), ("sub/b.ARW", b"bravo")]);
        let canon = d.path().canonicalize().unwrap();
        // Build a round-3 style session: Capture/ holds hard links.
        let root = canon.join(IN_PLACE_DIR);
        let mut s = Session::create(&root, "old").unwrap();
        fs::create_dir_all(root.join("Capture/sub")).unwrap();
        fs::hard_link(canon.join("a.ARW"), root.join("Capture/a.ARW")).unwrap();
        fs::hard_link(canon.join("sub/b.ARW"), root.join("Capture/sub/b.ARW")).unwrap();
        let meta = ImageMetaLite::default();
        let ids = s
            .add_originals(vec![
                (
                    "Capture/a.ARW".into(),
                    hash_file(&canon.join("a.ARW")).unwrap(),
                    meta.clone(),
                ),
                (
                    "Capture/sub/b.ARW".into(),
                    hash_file(&canon.join("sub/b.ARW")).unwrap(),
                    meta,
                ),
            ])
            .unwrap();
        let vid = s.variants(ids[0]).unwrap()[0];
        s.set_rating(&[vid], Rating::new(4).unwrap()).unwrap();
        drop(s);
        let h = open(d.path());
        assert_eq!(h.report.links_removed, 2);
        assert_eq!((h.report.added, h.report.missing), (0, 0));
        assert!(!root.join("Capture/a.ARW").exists());
        assert_eq!(
            fs::read(canon.join("a.ARW")).unwrap(),
            b"alpha",
            "original kept"
        );
        let v = list(&h);
        assert_eq!(v.len(), 2);
        assert_eq!(by_name(&v, "a.ARW").rating, 4);
        assert_eq!(by_name(&v, "a.ARW").variant_id, vid.0);
    }

    #[test]
    fn a_racing_creator_is_joined() {
        let d = folder(&[("a.ARW", b"a")]);
        let canon = d.path().canonicalize().unwrap();
        let root = canon.join(IN_PLACE_DIR);
        drop(Session::create_external(&root, "other", &canon).unwrap());
        // We armed cleanup for a store we thought we'd create...
        let mut cleanup = Cleanup {
            root: Some(root.clone()),
            made_ds: Some(canon.join(".ds")),
        };
        let s = create_or_join(&root, "mine", &canon, &mut cleanup).unwrap();
        assert_eq!(s.name(), "other");
        // ...but joined another process's store: never ours to remove.
        assert!(cleanup.root.is_none() && cleanup.made_ds.is_none());
        drop(cleanup);
        assert!(has_session(&root), "the other process's store survives");
        // A store that never finishes appearing times out instead of hanging.
        let half = canon.join("half");
        fs::create_dir_all(half.join("junk")).unwrap();
        let mut c2 = Cleanup {
            root: None,
            made_ds: None,
        };
        assert!(matches!(
            create_or_join(&half, "x", &canon, &mut c2),
            Err(DsError::Io { .. })
        ));
    }

    #[test]
    fn auto_apply_on_first_registration() {
        use crate::clipboard::AdjustGroup as G;
        let mut adj = Adjustments::default();
        adj.exposure.exposure_ev = 0.7;
        adj.white_balance = ds_core::adjust::WhiteBalance::Custom {
            temp_k: 3200.0,
            tint: 0.0,
        };
        let aa = AutoApplyPreset {
            name: "Warm".into(),
            adjustments_json: serde_json::to_string(&adj).unwrap(),
            groups: vec![G::Exposure],
            relative: vec![],
        };
        let preset = aa.to_preset().unwrap();
        let d = folder(&[("a.ARW", b"a")]);
        let with = |d: &Path, p: Option<Preset>| {
            open_in_place(d, None, p, &FAKE, &|| false, &mut |_, _, _| {}).unwrap()
        };
        let h = Arc::new(with(d.path(), Some(preset.clone())));
        let a = &list(&h)[0];
        assert!(a.edited, "applied when first registered");
        let got = SessionHandle::parse(
            &block_on(
                h.clone()
                    .load_adjustments_json(a.variant_id, CancelToken::new()),
            )
            .unwrap(),
        )
        .unwrap();
        assert!((got.exposure.exposure_ev - 0.7).abs() < 1e-6);
        assert_eq!(
            got.white_balance,
            Adjustments::default().white_balance,
            "only the chosen groups"
        );
        // Reset the photo; a reopen with the preset doesn't apply it again.
        block_on(h.clone().save_adjustments_json(
            a.variant_id,
            default_adjustments_json().unwrap(),
            CancelToken::new(),
        ))
        .unwrap();
        drop(h);
        fs::write(d.path().join("b.ARW"), b"bb").unwrap();
        let h = with(d.path(), None);
        let v = list(&h);
        assert!(!by_name(&v, "a.ARW").edited && !by_name(&v, "b.ARW").edited);
        let h = Arc::new(h);
        block_on(
            h.clone()
                .set_auto_apply_preset_json(Some(aa.clone()), CancelToken::new()),
        )
        .unwrap();
        let bad = AutoApplyPreset {
            adjustments_json: "{nope".into(),
            ..aa
        };
        let e = block_on(
            h.clone()
                .set_auto_apply_preset_json(Some(bad), CancelToken::new()),
        );
        assert!(matches!(e, Err(DsError::InvalidArgument { .. })));
        drop(h);
        fs::write(d.path().join("c.ARW"), b"ccc").unwrap();
        let s = Session::open(&d.path().canonicalize().unwrap().join(IN_PLACE_DIR)).unwrap();
        assert_eq!(s.auto_apply_preset(), Some(&preset), "stored by the method");
        drop(s);
        let h = with(d.path(), Some(preset));
        let v = list(&h);
        assert!(by_name(&v, "c.ARW").edited && !by_name(&v, "b.ARW").edited);
    }

    /// Sidecars under `dir` (any depth).
    fn sidecar_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(sidecar_files(&p));
            } else if fs::read_to_string(&p).is_ok_and(|t| t.contains("\"adjustments\"")) {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn untouched_neutral_edits_are_loaded_but_not_edited() {
        // Neutral on otherwise default adjustments isn't "edited" (no badge) but
        // isn't the default either (it renders unlike Standard): the app must get it.
        let d = folder(&[("a.ARW", b"a"), ("b.ARW", b"b")]);
        let h = Arc::new(open(d.path()));
        let ids: Vec<i64> = list(&h).iter().map(|r| r.variant_id).collect();
        let neutral = Adjustments {
            profile: ds_core::adjust::Profile::Neutral,
            ..Adjustments::default()
        };
        let json = serde_json::to_string(&neutral).unwrap();
        block_on(
            h.clone()
                .save_adjustments_json(ids[0], json, CancelToken::new()),
        )
        .unwrap();
        drop(h);
        let h = Arc::new(open(d.path()));
        let all = h.edited_json().unwrap();
        assert_eq!(all.edits.len(), 0);
        assert_eq!(all.neutral_ids, vec![ids[0]]);
        assert_eq!(SessionHandle::parse(&all.neutral_json).unwrap(), neutral);
        assert!(list(&h).iter().all(|r| !r.edited), "no edited badge");
        // A sidecar written before profiles (no `profile` key) is Neutral and loaded
        // too, once the index re-reads it.
        drop(h);
        let files: Vec<_> = sidecar_files(d.path())
            .into_iter()
            .filter(|p| fs::read_to_string(p).unwrap().contains("\"neutral\""))
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let mut v: serde_json::Value =
            serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
        v["adjustments"]
            .as_object_mut()
            .unwrap()
            .remove("profile")
            .unwrap();
        fs::write(&files[0], serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        let h = Arc::new(open(d.path()));
        let all = h.edited_json().unwrap();
        assert_eq!(all.edits.len(), 0);
        assert_eq!(all.neutral_ids, vec![ids[0]]);
        assert!(list(&h).iter().all(|r| !r.edited));
        // Neutral with an edit on top is an edit, sent in full.
        let mut edited = neutral.clone();
        edited.exposure.exposure_ev = 0.5;
        let json = serde_json::to_string(&edited).unwrap();
        block_on(
            h.clone()
                .save_adjustments_json(ids[1], json, CancelToken::new()),
        )
        .unwrap();
        let all = h.edited_json().unwrap();
        assert_eq!(all.neutral_ids, vec![ids[0]]);
        assert_eq!(all.edits.len(), 1);
        assert_eq!(all.edits[0].variant_id, ids[1]);
        assert_eq!(SessionHandle::parse(&all.edits[0].json).unwrap(), edited);
    }

    #[test]
    fn a_legacy_session_loads_its_edits_without_reading_sidecars() {
        use std::os::unix::fs::PermissionsExt;
        // Every photo of a session written before profiles reads as Neutral: listing
        // them must cost index rows, not one sidecar read and full JSON each.
        const N: usize = 200;
        let names: Vec<String> = (0..N).map(|i| format!("p{i:03}.ARW")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let d = folder(&files);
        drop(open(d.path()));
        let sidecars = sidecar_files(d.path());
        assert_eq!(sidecars.len(), N);
        for p in &sidecars {
            let mut v: serde_json::Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
            v["adjustments"].as_object_mut().unwrap().remove("profile");
            fs::write(p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        }
        let h = Arc::new(open(d.path()));
        // Unreadable now: any sidecar read below would fail the call.
        for p in &sidecars {
            fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let all = block_on(h.clone().load_all_adjustments_json(CancelToken::new()));
        for p in &sidecars {
            fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let all = all.unwrap();
        assert_eq!(all.edits.len(), 0);
        assert_eq!(all.neutral_ids.len(), N);
        let mut expected: Vec<i64> = list(&h).iter().map(|r| r.variant_id).collect();
        expected.sort_unstable();
        assert_eq!(all.neutral_ids, expected);
        assert_eq!(
            SessionHandle::parse(&all.neutral_json).unwrap(),
            ds_session::neutral_default()
        );
        // One shared JSON, not N of them.
        assert!(all.neutral_json.len() < 4096, "{}", all.neutral_json.len());
    }

    #[test]
    fn adjustments_round_trip_as_json_and_batch_is_one_step() {
        let d = folder(&[("a.ARW", b"a"), ("b.ARW", b"b")]);
        let h = Arc::new(open(d.path()));
        let ids: Vec<i64> = list(&h).iter().map(|r| r.variant_id).collect();
        assert_eq!(h.edited_json().unwrap().edits.len(), 0);
        let mut adj = Adjustments::default();
        adj.exposure.exposure_ev = 1.5;
        let json = serde_json::to_string(&adj).unwrap();
        block_on(
            h.clone()
                .save_adjustments_json(ids[0], json.clone(), CancelToken::new()),
        )
        .unwrap();
        let back = block_on(h.clone().load_adjustments_json(ids[0], CancelToken::new())).unwrap();
        assert_eq!(SessionHandle::parse(&back).unwrap(), adj);
        let all = block_on(h.clone().load_all_adjustments_json(CancelToken::new())).unwrap();
        assert_eq!((all.edits.len(), all.edits[0].variant_id), (1, ids[0]));
        assert_eq!(all.neutral_ids.len(), 0);
        let batch = ids
            .iter()
            .map(|&v| AdjustmentsJson {
                variant_id: v,
                json: json.clone(),
            })
            .collect();
        assert!(block_on(h.clone().apply_batch_json(batch, CancelToken::new())).unwrap());
        assert_eq!(h.edited_json().unwrap().edits.len(), 2);
        let dup = vec![
            AdjustmentsJson {
                variant_id: ids[0],
                json: json.clone(),
            },
            AdjustmentsJson {
                variant_id: ids[0],
                json,
            },
        ];
        assert!(matches!(
            block_on(h.clone().apply_batch_json(dup, CancelToken::new())),
            Err(DsError::InvalidArgument { .. })
        ));
        let bad = block_on(h.clone().save_adjustments_json(
            ids[0],
            "{nope".into(),
            CancelToken::new(),
        ));
        assert!(matches!(bad, Err(DsError::InvalidArgument { .. })));
        let unknown = block_on(h.clone().load_adjustments_json(424_242, CancelToken::new()));
        assert!(
            matches!(unknown, Err(DsError::InvalidArgument { .. })),
            "{unknown:?}"
        );
    }

    #[test]
    fn tags_picks_and_rating_validation() {
        let d = folder(&[("a.ARW", b"a"), ("bad.ARW", b"BAD!")]);
        let h = Arc::new(open(d.path()));
        assert_eq!(h.report.failed.len(), 1);
        assert!(h.report.failed[0].starts_with("Capture/bad.ARW"));
        let id = list(&h)[0].variant_id;
        block_on(
            h.clone()
                .set_color_tag(vec![id], ColorLabel::Orange, CancelToken::new()),
        )
        .unwrap();
        block_on(
            h.clone()
                .set_pick(vec![id], PickFlag::Rejected, CancelToken::new()),
        )
        .unwrap();
        let r = &list(&h)[0];
        assert_eq!(
            (r.color_tag, r.pick),
            (ColorLabel::Orange, PickFlag::Rejected)
        );
        assert!(matches!(
            block_on(h.clone().set_rating(vec![id], 9, CancelToken::new())),
            Err(DsError::InvalidArgument { .. })
        ));
        assert_eq!(
            h.name().unwrap(),
            session_name(&d.path().canonicalize().unwrap())
        );
        assert!(h.root().unwrap().ends_with(".ds/session"));
        assert!(h.in_place_folder().unwrap().is_some());
        assert_eq!(h.open_report().unwrap().added, 1);
        assert!(format!("{h:?}").starts_with("SessionHandle"));
    }

    /// AI Cull's reject note is listed with the variant until the flag changes.
    #[test]
    fn cull_notes_are_set_listed_and_cleared_by_a_new_flag() {
        let d = folder(&[("a.ARW", b"a")]);
        let h = Arc::new(open(d.path()));
        let id = list(&h)[0].variant_id;
        assert_eq!(list(&h)[0].cull_note, None);
        let pick = |p| block_on(h.clone().set_pick(vec![id], p, CancelToken::new())).unwrap();
        let note = |n: Option<&str>| {
            block_on(
                h.clone()
                    .set_cull_note(vec![id], n.map(Into::into), CancelToken::new()),
            )
            .unwrap();
        };
        pick(PickFlag::Rejected);
        note(Some("Eyes closed"));
        assert_eq!(list(&h)[0].cull_note.as_deref(), Some("Eyes closed"));
        pick(PickFlag::Unflagged);
        assert_eq!(list(&h)[0].cull_note, None);
        note(Some("Eyes closed"));
        assert_eq!(
            list(&h)[0].cull_note,
            None,
            "only a rejected photo keeps one"
        );
    }

    /// Keywords round-trip through the FFI (sidecars), trimmed and de-duplicated, and
    /// show on the listed variant.
    #[test]
    fn keywords_are_set_and_listed() {
        let d = folder(&[("a.ARW", b"a")]);
        let h = Arc::new(open(d.path()));
        let id = list(&h)[0].variant_id;
        assert_eq!(list(&h)[0].keywords.len(), 0);
        block_on(h.clone().set_keywords(
            vec![id],
            vec![" beach ".into(), "Crete".into(), "beach".into()],
            CancelToken::new(),
        ))
        .unwrap();
        assert_eq!(list(&h)[0].keywords, ["beach", "Crete"]);
        block_on(h.clone().set_keywords(vec![id], vec![], CancelToken::new())).unwrap();
        assert_eq!(list(&h)[0].keywords.len(), 0);
    }

    /// AI Optimize reports round-trip through the sidecar; bad JSON is refused.
    #[test]
    fn optimize_reports_are_stored_per_variant() {
        let d = folder(&[("a.ARW", b"a")]);
        let h = Arc::new(open(d.path()));
        let id = list(&h)[0].variant_id;
        let get = || block_on(h.clone().optimize_report(id, CancelToken::new())).unwrap();
        assert_eq!(get(), None);
        let json = r#"{"version":1,"summary":"brightens the image"}"#;
        block_on(
            h.clone()
                .set_optimize_report(vec![id], Some(json.into()), CancelToken::new()),
        )
        .unwrap();
        let back: serde_json::Value = serde_json::from_str(&get().unwrap()).unwrap();
        assert_eq!(back["summary"], "brightens the image");
        assert!(matches!(
            block_on(
                h.clone()
                    .set_optimize_report(vec![id], Some("{".into()), CancelToken::new())
            ),
            Err(DsError::InvalidArgument { .. })
        ));
        block_on(
            h.clone()
                .set_optimize_report(vec![id], None, CancelToken::new()),
        )
        .unwrap();
        assert_eq!(get(), None);
    }

    /// AI changes round-trip through the sidecar; bad JSON is refused.
    #[test]
    fn ai_changes_are_stored_per_variant() {
        let d = folder(&[("a.ARW", b"a")]);
        let h = Arc::new(open(d.path()));
        let id = list(&h)[0].variant_id;
        let get = || block_on(h.clone().ai_changes(id, CancelToken::new())).unwrap();
        assert_eq!(get(), None);
        let json = r#"{"version":1,"steps":[]}"#;
        block_on(
            h.clone()
                .set_ai_changes(vec![id], Some(json.into()), CancelToken::new()),
        )
        .unwrap();
        let back: serde_json::Value = serde_json::from_str(&get().unwrap()).unwrap();
        assert_eq!(back["version"], 1);
        assert!(matches!(
            block_on(
                h.clone()
                    .set_ai_changes(vec![id], Some("{".into()), CancelToken::new())
            ),
            Err(DsError::InvalidArgument { .. })
        ));
        block_on(h.clone().set_ai_changes(vec![id], None, CancelToken::new())).unwrap();
        assert_eq!(get(), None);
    }

    /// An imported session renamed in Finder while open is
    /// reported (`FolderMoved`) before any write, so no ghost folder is created
    /// at the old path; reopened at the new path, writes go on.
    #[test]
    fn plain_session_renamed_while_open_is_reported_and_leaves_no_ghost() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("2026-09-26 Beach");
        let mut s = Session::create(&root, "Beach").unwrap();
        fs::create_dir_all(root.join("Capture")).unwrap();
        fs::write(root.join("Capture/a.ARW"), b"alpha").unwrap();
        s.add_originals(vec![(
            "Capture/a.ARW".into(),
            hash_file(&root.join("Capture/a.ARW")).unwrap(),
            ImageMetaLite::default(),
        )])
        .unwrap();
        drop(s);
        let h = block_on(open_session(
            root.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap();
        let id = list(&h)[0].variant_id;
        block_on(h.clone().set_rating(vec![id], 3, CancelToken::new())).unwrap();
        let moved = d.path().join("Beach RENAMED");
        fs::rename(&root, &moved).unwrap();
        let e = block_on(h.clone().set_rating(vec![id], 5, CancelToken::new()));
        assert!(
            matches!(e, Err(DsError::FolderMoved { ref message }) if message.ends_with("2026-09-26 Beach")),
            "{e:?}"
        );
        let json = default_adjustments_json().unwrap();
        let e = block_on(
            h.clone()
                .save_adjustments_json(id, json, CancelToken::new()),
        );
        assert!(matches!(e, Err(DsError::FolderMoved { .. })), "{e:?}");
        assert!(!root.exists(), "no ghost folder at the old path");
        let h = block_on(open_session(
            moved.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap();
        assert_eq!(
            list(&h)[0].rating,
            3,
            "what was saved before the rename is there"
        );
        block_on(h.clone().set_rating(vec![id], 5, CancelToken::new())).unwrap();
        assert_eq!(list(&h)[0].rating, 5);
        assert!(!root.exists());
    }

    /// Finishing a cull in an imported session. Rejects go to
    /// the session Trash (listed `in_trash`, recoverable by moving them back),
    /// picks to Selects; emptying the Trash hands the files to a trasher; a new
    /// variant is listed; a folder opened in place refuses moves.
    #[test]
    fn finishing_a_cull_moves_trashes_and_makes_variants() {
        struct Fake(Mutex<Vec<PathBuf>>);
        impl ds_session::Trasher for Fake {
            fn trash(&self, paths: &[PathBuf]) -> Result<(), String> {
                lock(&self.0).extend_from_slice(paths);
                Ok(())
            }
        }
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Cull");
        let mut s = Session::create(&root, "Cull").unwrap();
        fs::create_dir_all(root.join("Capture")).unwrap();
        let mut items = Vec::new();
        for n in ["a", "b", "c"] {
            let rel = format!("Capture/{n}.ARW");
            fs::write(root.join(&rel), n.as_bytes()).unwrap();
            items.push((
                rel.clone(),
                hash_file(&root.join(&rel)).unwrap(),
                ImageMetaLite::default(),
            ));
        }
        s.add_originals(items).unwrap();
        drop(s);
        let h = block_on(open_session(
            root.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap();
        assert!(h.can_move_originals());
        let v = list(&h);
        let (a, b, c) = (
            by_name(&v, "a.ARW").variant_id,
            by_name(&v, "b.ARW").variant_id,
            by_name(&v, "c.ARW").variant_id,
        );
        let mv = |ids: Vec<i64>, f| {
            block_on(h.clone().move_to_folder(ids, f, CancelToken::new())).unwrap()
        };
        assert_eq!(mv(vec![a], SessionFolder::Trash).len(), 1);
        assert_eq!(mv(vec![b], SessionFolder::Selects).len(), 1);
        let v = list(&h);
        assert!(by_name(&v, "a.ARW").in_trash && !by_name(&v, "a.ARW").in_selects);
        assert!(by_name(&v, "b.ARW").in_selects && !by_name(&v, "c.ARW").in_trash);
        assert!(root.join("Trash/a.ARW").is_file() && root.join("Selects/b.ARW").is_file());
        // Undo: back to Capture.
        assert_eq!(mv(vec![a], SessionFolder::Capture).len(), 1);
        assert!(root.join("Capture/a.ARW").is_file() && !by_name(&list(&h), "a.ARW").in_trash);
        assert_eq!(mv(vec![a], SessionFolder::Trash).len(), 1);
        // Empty the Trash: the files go to the trasher, the photo leaves the session.
        let fake = Fake(Mutex::new(Vec::new()));
        assert_eq!(h.empty_trash_with(&fake).unwrap(), 1);
        assert!(lock(&fake.0).iter().any(|p| p.ends_with("Trash/a.ARW")));
        assert_eq!(list(&h).len(), 2);
        // A new variant of c.
        let nv = block_on(h.clone().new_variant(c, CancelToken::new())).unwrap();
        assert!(
            list(&h)
                .iter()
                .any(|r| r.variant_id == nv && r.file_name == "c.ARW")
        );
        // A folder opened in place never moves the user's files.
        let f = folder(&[("x.ARW", b"xx")]);
        let ip = Arc::new(open(f.path()));
        assert!(!ip.can_move_originals());
        let x = list(&ip)[0].variant_id;
        let e = block_on(ip.clone().move_to_folder(
            vec![x],
            SessionFolder::Trash,
            CancelToken::new(),
        ));
        assert!(e.is_err(), "{e:?}");
        assert!(f.path().join("x.ARW").is_file());
        assert!(ip.empty_trash_with(&fake).is_err());
    }

    /// "Move Rejected to Trash" moves a file only when every
    /// variant is rejected (a picked sibling keeps it, and it stays archived);
    /// the rejected variant is hidden. Undo returns a Select to Selects and
    /// brings the variant back.
    #[test]
    fn trash_rejected_keeps_picked_siblings_and_undo_restores_folders() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Cull");
        let mut s = Session::create(&root, "Cull").unwrap();
        fs::create_dir_all(root.join("Capture")).unwrap();
        let mut items = Vec::new();
        for n in ["a", "b"] {
            let rel = format!("Capture/{n}.ARW");
            fs::write(root.join(&rel), n.as_bytes()).unwrap();
            items.push((
                rel.clone(),
                hash_file(&root.join(&rel)).unwrap(),
                ImageMetaLite::default(),
            ));
        }
        s.add_originals(items).unwrap();
        drop(s);
        let h = block_on(open_session(
            root.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap();
        let v = list(&h);
        let (a, b) = (
            by_name(&v, "a.ARW").variant_id,
            by_name(&v, "b.ARW").variant_id,
        );
        let a2 = block_on(h.clone().new_variant(a, CancelToken::new())).unwrap();
        let pick =
            |ids: Vec<i64>, p| block_on(h.clone().set_pick(ids, p, CancelToken::new())).unwrap();
        pick(vec![a2], PickFlag::Picked);
        pick(vec![a], PickFlag::Rejected);
        // b was a Select, then rejected.
        let sel = block_on(h.clone().move_to_folder(
            vec![b],
            SessionFolder::Selects,
            CancelToken::new(),
        ))
        .unwrap();
        assert_eq!(sel[0].from, "Capture/b.ARW");
        assert_eq!(sel[0].to, "Selects/b.ARW");
        pick(vec![b], PickFlag::Rejected);

        let rep = block_on(h.clone().trash_rejected(CancelToken::new())).unwrap();
        assert_eq!(rep.error, None);
        assert_eq!(rep.hidden, vec![a]);
        assert_eq!(
            rep.moved,
            vec![FolderMoveRecord {
                variant_id: b,
                from: "Selects/b.ARW".into(),
                to: "Trash/b.ARW".into()
            }]
        );
        let v = list(&h);
        let kept = v.iter().find(|r| r.variant_id == a2).unwrap();
        assert!(!kept.in_trash && kept.pick == PickFlag::Picked);
        assert!(
            !v.iter().any(|r| r.variant_id == a),
            "the rejected variant is hidden"
        );
        assert!(root.join("Capture/a.ARW").is_file());
        {
            let s = lock(&h.session);
            let img = s
                .images()
                .unwrap()
                .into_iter()
                .find(|i| i.rel_path.as_str() == "Capture/a.ARW");
            assert!(
                !SessionHandle::is_rejected(&s, &img.unwrap()).unwrap(),
                "still archived"
            );
        }

        let n = block_on(
            h.clone()
                .move_back(rep.moved, rep.hidden, CancelToken::new()),
        )
        .unwrap();
        assert_eq!(n, 1);
        let v = list(&h);
        assert!(by_name(&v, "b.ARW").in_selects && root.join("Selects/b.ARW").is_file());
        let back = v.iter().find(|r| r.variant_id == a).unwrap();
        assert_eq!(back.pick, PickFlag::Rejected);
        // A bad path is refused before anything moves.
        let bad = FolderMoveRecord {
            variant_id: b,
            from: "../escape.ARW".into(),
            to: String::new(),
        };
        assert!(block_on(h.clone().move_back(vec![bad], vec![], CancelToken::new())).is_err());
        assert!(root.join("Selects/b.ARW").is_file());
    }

    /// A rejected variant hidden by "Move Rejected to Trash" comes
    /// back with "Restore from Session Trash" after a relaunch (no undo record).
    #[test]
    fn restore_set_aside_variants_after_a_relaunch() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Cull");
        let mut s = Session::create(&root, "Cull").unwrap();
        fs::create_dir_all(root.join("Capture")).unwrap();
        fs::write(root.join("Capture/a.ARW"), b"a").unwrap();
        let hash = hash_file(&root.join("Capture/a.ARW")).unwrap();
        s.add_originals(vec![(
            "Capture/a.ARW".into(),
            hash,
            ImageMetaLite::default(),
        )])
        .unwrap();
        drop(s);
        let reopen = || {
            block_on(open_session(
                root.to_string_lossy().into_owned(),
                CancelToken::new(),
            ))
            .unwrap()
        };
        let h = reopen();
        let a = list(&h)[0].variant_id;
        let a2 = block_on(h.clone().new_variant(a, CancelToken::new())).unwrap();
        block_on(
            h.clone()
                .set_pick(vec![a2], PickFlag::Rejected, CancelToken::new()),
        )
        .unwrap();
        let rep = block_on(h.clone().trash_rejected(CancelToken::new())).unwrap();
        assert!(rep.moved.is_empty() && rep.hidden == vec![a2]);
        drop(h);
        let h = reopen();
        assert!(!list(&h).iter().any(|r| r.variant_id == a2));
        let restore =
            || block_on(h.clone().restore_set_aside_variants(CancelToken::new())).unwrap();
        assert_eq!(restore(), 1);
        let back = list(&h).into_iter().find(|r| r.variant_id == a2).unwrap();
        assert_eq!(back.pick, PickFlag::Rejected);
        assert_eq!(restore(), 0);
        // A folder opened in place has nothing set aside: nothing happens.
        let f = folder(&[("x.ARW", b"xx")]);
        let ip = Arc::new(open(f.path()));
        assert_eq!(
            block_on(ip.restore_set_aside_variants(CancelToken::new())).unwrap(),
            0
        );
        assert!(f.path().join("x.ARW").is_file());
    }

    /// A 0-byte (or otherwise unreadable-as-RAW) file is named
    /// in the open report instead of silently not appearing.
    #[test]
    fn empty_and_damaged_raws_are_named_in_the_open_report() {
        let probe = Probe {
            is_supported: |p| fake_supported(p) && fs::metadata(p).is_ok_and(|m| m.len() > 0),
            is_raw_name: fake_supported,
            ..FAKE
        };
        let d = folder(&[
            ("good.ARW", b"good"),
            ("EMPTY.ARW", b""),
            ("notes.txt", b"x"),
        ]);
        let h = open_in_place(d.path(), None, None, &probe, &|| false, &mut |_, _, _| {}).unwrap();
        assert_eq!(h.report.added, 1);
        assert_eq!(
            h.report.failed,
            vec!["Capture/EMPTY.ARW: damaged or incomplete".to_owned()]
        );
    }

    #[test]
    fn rsync_patterns_escape_wildcards() {
        assert_eq!(
            rsync_escape("Capture/a[1]*?.ARW"),
            "Capture/a\\[1\\]\\*\\?.ARW"
        );
        assert_eq!(rsync_escape("plain.ARW"), "plain.ARW");
    }

    #[test]
    fn create_session_makes_an_empty_session_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Library").join("Session 2026-10-03");
        let path = root.to_string_lossy().into_owned();
        let h = block_on(create_session(
            path.clone(),
            "Session 2026-10-03".into(),
            CancelToken::new(),
        ))
        .unwrap();
        assert_eq!(h.name().unwrap(), "Session 2026-10-03");
        assert!(h.in_place_folder().unwrap().is_none());
        assert_eq!(
            block_on(h.clone().variants(CancelToken::new()))
                .unwrap()
                .len(),
            0
        );
        for sub in ["Capture", "Selects", "Output", "Trash"] {
            assert!(root.join(sub).is_dir(), "{sub}");
        }
        drop(h);
        // It opens like any session made by import.
        assert!(block_on(open_session(path.clone(), CancelToken::new())).is_ok());
        // An existing session (a non-empty folder) is refused, and left as it was.
        let again = block_on(create_session(path, "Other".into(), CancelToken::new()));
        assert!(
            matches!(again, Err(DsError::InvalidArgument { .. })),
            "{again:?}"
        );
        assert!(root.join(".ds/settings/session.json").is_file());
        // So are names a folder can't have.
        let bad = block_on(create_session(
            d.path().join("b").to_string_lossy().into_owned(),
            "a/b".into(),
            CancelToken::new(),
        ));
        assert!(
            matches!(bad, Err(DsError::InvalidArgument { .. })),
            "{bad:?}"
        );
        assert!(!d.path().join("b").exists());
    }

    #[test]
    fn open_session_and_errors() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("S");
        drop(Session::create(&root, "S").unwrap());
        let h = block_on(open_session(
            root.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap();
        assert!(h.in_place_folder().unwrap().is_none());
        assert_eq!(h.open_report().unwrap().added, 0);
        assert_eq!(
            block_on(h.clone().variants(CancelToken::new()))
                .unwrap()
                .len(),
            0
        );
        let r = block_on(h.clone().hash_pending(None, CancelToken::new())).unwrap();
        assert_eq!(r.hashed, 0);
        let e = block_on(open_session(
            d.path().join("nope").to_string_lossy().into_owned(),
            CancelToken::new(),
        ));
        assert!(e.is_err());
        let f = block_on(open_folder_in_place(
            d.path().join("missing").to_string_lossy().into_owned(),
            None,
            None,
            false,
            None,
            None,
            CancelToken::new(),
        ));
        assert!(matches!(f, Err(DsError::Io { .. })), "{f:?}");
        let file = d.path().join("x.txt");
        fs::write(&file, b"x").unwrap();
        let r = open_in_place(&file, None, None, &FAKE, &|| false, &mut |_, _, _| {});
        assert!(matches!(r, Err(DsError::InvalidArgument { .. })));
    }

    #[test]
    fn volume_verdicts() {
        assert!(volume_verdict("apfs", MNT_LOCAL, true).is_ok());
        assert!(volume_verdict("hfs", MNT_LOCAL, true).is_ok());
        assert!(
            volume_verdict("exfat", MNT_LOCAL, true)
                .unwrap_err()
                .contains("never written")
        );
        assert!(volume_verdict("msdos", MNT_LOCAL, true).is_err());
        assert!(
            volume_verdict("smbfs", 0, true)
                .unwrap_err()
                .contains("network")
        );
        assert!(
            volume_verdict("apfs", MNT_LOCAL | MNT_RDONLY, true)
                .unwrap_err()
                .contains("read-only")
        );
        assert!(volume_verdict("apfs", MNT_LOCAL, false).is_err());
        let d = tempfile::tempdir().unwrap();
        assert!(folder_can_hold_session(d.path().to_string_lossy().into()).unwrap());
        assert!(!folder_can_hold_session("/nonexistent/x".into()).unwrap());
    }

    #[test]
    fn names_and_dates() {
        assert_eq!(session_name(Path::new("/x/2026: Trip")), "2026- Trip");
        assert_eq!(session_name(Path::new("/x/..hidden")), "hidden");
        assert_eq!(session_name(Path::new("/")), "Session");
        assert_eq!(
            session_name(Path::new(&format!("/x/{}", "a".repeat(300)))).len(),
            200
        );
        let t = ds_raw::CaptureTime {
            year: 2026,
            month: 8,
            day: 8,
            hour: 17,
            minute: 25,
            second: 40,
            nanos: 120_000_000,
            utc_offset_minutes: None,
        };
        assert_eq!(capture_ms(&t), 1_786_209_940_120);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn default_and_normalized_json() {
        let d = default_adjustments_json().unwrap();
        assert_eq!(SessionHandle::parse(&d).unwrap(), Adjustments::default());
        let n = normalize_adjustments_json(
            r#"{"exposure":{"exposure_ev":1.5},"future_field":3}"#.into(),
        )
        .unwrap();
        let a = SessionHandle::parse(&n).unwrap();
        assert!((a.exposure.exposure_ev - 1.5).abs() < f32::EPSILON);
        assert!(!n.contains("future_field"));
        assert!(normalize_adjustments_json("[1]".into()).is_err());
    }

    #[test]
    fn session_errors_map_by_action() {
        let io: DsError = SessionError::NotASession(PathBuf::from("/x")).into();
        assert!(matches!(io, DsError::Unsupported { .. }));
        let ext: DsError = SessionError::ExternalOriginals("move_to").into();
        assert!(matches!(ext, DsError::Unsupported { .. }));
        let only: DsError = SessionError::ExternalOnly("x").into();
        assert!(matches!(only, DsError::Unsupported { .. }));
        let bad: DsError = SessionError::InvalidName("x".into()).into();
        assert!(matches!(bad, DsError::InvalidArgument { .. }));
        let json: DsError = SessionError::Json {
            path: PathBuf::from("/x"),
            message: "m".into(),
        }
        .into();
        assert!(matches!(json, DsError::Io { .. }));
        for (t, l) in [
            (ColorTag::None, ColorLabel::None),
            (ColorTag::Red, ColorLabel::Red),
            (ColorTag::Orange, ColorLabel::Orange),
            (ColorTag::Yellow, ColorLabel::Yellow),
            (ColorTag::Green, ColorLabel::Green),
            (ColorTag::Blue, ColorLabel::Blue),
            (ColorTag::Pink, ColorLabel::Pink),
            (ColorTag::Purple, ColorLabel::Purple),
        ] {
            assert_eq!(ColorLabel::from(t), l);
            assert_eq!(ColorTag::from(l), t);
        }
        for (p, f) in [
            (PickState::Unflagged, PickFlag::Unflagged),
            (PickState::Picked, PickFlag::Picked),
            (PickState::Rejected, PickFlag::Rejected),
        ] {
            assert_eq!(PickFlag::from(p), f);
            assert_eq!(PickState::from(f), p);
        }
    }

    #[test]
    fn real_samples_open_in_place_with_metadata() {
        let Some(sample) = ds_testdata::sample_arw() else {
            eprintln!("no samples/ — skipping");
            return;
        };
        let d = tempfile::tempdir().unwrap();
        fs::copy(&sample, d.path().join(sample.file_name().unwrap())).unwrap();
        let events = Arc::new(Recorder::default());
        let h = block_on(open_folder_in_place(
            d.path().to_string_lossy().into_owned(),
            None,
            None,
            false,
            None,
            Some(events.clone()),
            CancelToken::new(),
        ))
        .unwrap();
        let kinds = lock(&events.0).clone();
        assert!(kinds.iter().any(|k| k == "open.register"), "{kinds:?}");
        let v = block_on(h.clone().variants(CancelToken::new())).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].meta.as_ref().unwrap().model, "ILCE-9");
        assert!(v[0].capture_time_ms.is_some());
        assert_eq!(v[0].width.max(v[0].height), 6000);
        let r = block_on(
            h.clone()
                .hash_pending(Some(events.clone()), CancelToken::new()),
        )
        .unwrap();
        assert_eq!(r.hashed, 1);
        assert!(lock(&events.0).iter().any(|k| k == "hash"));
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);
    impl ProgressListener for Recorder {
        fn on_progress(&self, e: ProgressEvent) -> Result<(), crate::error::DsError> {
            lock(&self.0).push(e.kind);
            Ok(())
        }
    }

    /// 1,757-file first open (R3 perf target: grid ≤ 1 s warm). Needs the
    /// read-only `big1757` fixture: `DS_BIG_FOLDER=<dir> cargo test --release -- --ignored`.
    #[test]
    #[ignore = "needs DS_BIG_FOLDER"]
    fn big_folder_first_open_timing() {
        let Some(src) = std::env::var_os("DS_BIG_FOLDER") else {
            return;
        };
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("store");
        // CARD probe with the real reader: store outside, zero writes to the fixture.
        let probe = Probe {
            volume_ok: |_| Err("fixture is read-only".into()),
            ..Probe::RAW
        };
        let t = std::time::Instant::now();
        let h = open_in_place(
            Path::new(&src),
            Some(&fb),
            None,
            &probe,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap();
        let v = h.list(&probe, &|| false).unwrap();
        let first = t.elapsed();
        let t = std::time::Instant::now();
        let h2 = open_in_place(
            Path::new(&src),
            Some(&fb),
            None,
            &probe,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap();
        let v2 = h2.list(&probe, &|| false).unwrap();
        let reopen = t.elapsed();
        eprintln!(
            "BIG: {} files, first open+list {first:?}, reopen+list {reopen:?}",
            v.len()
        );
        assert_eq!(v.len(), v2.len());
        let t = std::time::Instant::now();
        let r = h2
            .hash_pending_with(&probe, &|| false, &mut |_, _| {})
            .unwrap();
        eprintln!(
            "BIG: background hash of {} files {:?}",
            r.hashed,
            t.elapsed()
        );
    }
}

/// Real volumes (R3 item 7): FAT32/exFAT memory-card images and a read-only
/// image, made with `hdiutil`. Every hdiutil call has a timeout; a test whose
/// image can't be made or mounted in time is skipped (reported), and images
/// are always detached.
#[cfg(all(test, target_os = "macos"))]
mod volume_tests {
    use super::*;
    use futures_executor::block_on;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Run `hdiutil args`, killed after `secs`. `None` on failure or timeout.
    fn hdiutil(args: &[&str], secs: u64) -> Option<()> {
        let mut child = Command::new("/usr/bin/hdiutil")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => return st.success().then_some(()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("hdiutil {args:?} timed out");
                    return None;
                }
            }
        }
    }

    /// A mounted image; detached on drop.
    struct Mounted {
        mount: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            let m = self.mount.to_string_lossy().into_owned();
            if hdiutil(&["detach", &m], 30).is_none() {
                let _ = hdiutil(&["detach", &m, "-force"], 30);
            }
        }
    }

    fn attach(dir: tempfile::TempDir, dmg: &Path, read_only: bool) -> Option<Mounted> {
        let mount = dir.path().join("mnt");
        fs::create_dir(&mount).ok()?;
        let (d, m) = (dmg.to_string_lossy(), mount.to_string_lossy());
        let mut args = vec!["attach", "-nobrowse", "-noverify", "-mountpoint", &m, &d];
        if read_only {
            args.push("-readonly");
        }
        hdiutil(&args, 60)?;
        let img = Mounted { mount, _dir: dir };
        img.mount.join(".").is_dir().then_some(img)
    }

    /// A blank card image formatted `fs_name` (`ExFAT`, `MS-DOS FAT32`).
    fn card(fs_name: &str) -> Option<Mounted> {
        let dir = tempfile::tempdir().ok()?;
        let dmg = dir.path().join("card.dmg");
        let d = dmg.to_string_lossy().into_owned();
        hdiutil(
            &[
                "create", "-size", "64m", "-fs", fs_name, "-volname", "DSCARD", &d,
            ],
            90,
        )?;
        attach(dir, &dmg, false)
    }

    /// Files under `dir` that the test put there (OS bookkeeping like
    /// `.fseventsd` and `.Spotlight-V100` is not ours and is ignored).
    fn user_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if matches!(
                    name.as_str(),
                    ".fseventsd" | ".Spotlight-V100" | ".Trashes" | ".TemporaryItems"
                ) || name.starts_with("._")
                {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dir).unwrap().to_owned();
                    out.push((rel, fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn fake_read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
        let bytes = fs::read(p).ok()?;
        let lite = ImageMetaLite {
            capture_time_ms: Some(bytes.len() as i64),
            width: 6000,
            height: 4000,
            ..Default::default()
        };
        let full = RawMetadata {
            model: "ILCE-9".into(),
            ..RawMetadata::default()
        };
        Some((lite, full))
    }

    /// The production volume check with a fake RAW reader.
    const REAL_VOLUME: Probe = Probe {
        is_supported: |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")),
        read: fake_read,
        volume_ok: sidecar_volume_ok,
        ranges: |_| None,
        untrusted_stat: fat_volume,
        is_raw_name: |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")),
    };

    fn open_card(dir: &Path, fb: &Path) -> Result<SessionHandle, DsError> {
        open_in_place(
            dir,
            Some(fb),
            None,
            &REAL_VOLUME,
            &|| false,
            &mut |_, _, _| {},
        )
    }

    fn card_round_trip(fs_name: &str) {
        let Some(img) = card(fs_name) else {
            eprintln!("SKIPPED: could not create/mount a {fs_name} image in time");
            return;
        };
        let dcim = img.mount.join("DCIM/100MSDCF");
        fs::create_dir_all(&dcim).unwrap();
        for (n, b) in [
            ("A.ARW", &b"alpha"[..]),
            ("B.ARW", b"bravo!"),
            ("C.ARW", b"charlie"),
        ] {
            fs::write(dcim.join(n), b).unwrap();
        }
        let why = sidecar_volume_ok(&img.mount).unwrap_err();
        eprintln!("{fs_name}: {why}");
        let before = user_files(&img.mount);
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("InPlace/vol/hash");
        let refused = open_in_place(
            &img.mount,
            None,
            None,
            &REAL_VOLUME,
            &|| false,
            &mut |_, _, _| {},
        );
        assert!(
            matches!(refused, Err(DsError::Unsupported { .. })),
            "{refused:?}"
        );
        let h = Arc::new(open_card(&img.mount, &fb).unwrap());
        assert!(!h.report.store_on_volume);
        let v = h.list(&REAL_VOLUME, &|| false).unwrap();
        assert_eq!(v.len(), 3);
        assert!(v.iter().all(|r| r.meta.as_ref().unwrap().model == "ILCE-9"));
        let a = v.iter().find(|r| r.file_name == "A.ARW").unwrap().clone();
        block_on(
            h.clone()
                .set_rating(vec![a.variant_id], 5, CancelToken::new()),
        )
        .unwrap();
        assert_eq!(
            h.hash_pending_with(&REAL_VOLUME, &|| false, &mut |_, _| {})
                .unwrap()
                .hashed,
            3
        );
        drop(h);
        assert_eq!(
            user_files(&img.mount),
            before,
            "{fs_name}: card byte-identical"
        );
        assert!(!img.mount.join(".ds").exists());
        let again = open_card(&img.mount, &fb).unwrap();
        let v = again.list(&REAL_VOLUME, &|| false).unwrap();
        let a2 = v.iter().find(|r| r.file_name == "A.ARW").unwrap();
        assert_eq!(
            (a2.variant_id, a2.rating),
            (a.variant_id, 5),
            "rating persisted in the store"
        );
        drop(again);
        // R3-H3 Finder operations on the card: rename, delete, replace (same name).
        fs::rename(dcim.join("A.ARW"), dcim.join("A-renamed.ARW")).unwrap();
        fs::remove_file(dcim.join("C.ARW")).unwrap();
        fs::remove_file(dcim.join("B.ARW")).unwrap();
        fs::write(dcim.join("B.ARW"), b"a different photo").unwrap();
        let h = open_card(&img.mount, &fb).unwrap();
        // The rename is followed (by stat, or by content in the background if
        // the FAT "inode" changed with it); B is a different photo.
        let r = h
            .hash_pending_with(&REAL_VOLUME, &|| false, &mut |_, _| {})
            .unwrap();
        eprintln!("{fs_name}: open {:?} background {r:?}", h.report);
        assert_eq!(h.report.relinked + r.relinked, 1, "{fs_name}");
        assert_eq!(h.report.missing, 1, "{fs_name}: C");
        assert_eq!(r.changed, 1, "{fs_name}");
        let v = h.list(&REAL_VOLUME, &|| false).unwrap();
        let ren = v.iter().find(|r| r.file_name == "A-renamed.ARW").unwrap();
        assert_eq!((ren.variant_id, ren.rating), (a.variant_id, 5));
        let b = v.iter().find(|r| r.file_name == "B.ARW").unwrap();
        assert!(b.file_changed && b.rating == 0);
        assert!(v.iter().find(|r| r.file_name == "C.ARW").unwrap().missing);
        drop(h);
        assert!(
            !img.mount.join(".ds").exists(),
            "{fs_name}: still nothing written"
        );
    }

    #[test]
    fn exfat_card_is_never_written() {
        card_round_trip("ExFAT");
    }

    #[test]
    fn fat32_card_is_never_written() {
        card_round_trip("MS-DOS FAT32");
    }

    #[test]
    fn read_only_image_opens_with_the_store_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("DCIM")).unwrap();
        fs::write(src.join("DCIM/A.ARW"), b"alpha").unwrap();
        let dmg = dir.path().join("ro.dmg");
        let (s, d) = (
            src.to_string_lossy().into_owned(),
            dmg.to_string_lossy().into_owned(),
        );
        let made = hdiutil(
            &[
                "create",
                "-srcfolder",
                &s,
                "-format",
                "UDRO",
                "-volname",
                "DSRO",
                &d,
            ],
            90,
        );
        let Some(img) = made.and_then(|()| attach(dir, &dmg, true)) else {
            eprintln!("SKIPPED: could not create/mount a read-only image in time");
            return;
        };
        assert!(
            sidecar_volume_ok(&img.mount)
                .unwrap_err()
                .contains("read-only")
        );
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("fb");
        let h = Arc::new(open_card(&img.mount, &fb).unwrap());
        let id = h.list(&REAL_VOLUME, &|| false).unwrap()[0].variant_id;
        block_on(
            h.clone()
                .set_color_tag(vec![id], ColorLabel::Orange, CancelToken::new()),
        )
        .unwrap();
        drop(h);
        let again = open_card(&img.mount, &fb).unwrap();
        assert_eq!(
            again.list(&REAL_VOLUME, &|| false).unwrap()[0].color_tag,
            ColorLabel::Orange
        );
        assert!(!img.mount.join(".ds").exists());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn smart_folders_exact_counts_crud_and_persistence() {
        let d = super::tests::folder(&[
            ("a.ARW", b"alpha"),
            ("b.ARW", b"bravo"),
            ("c.ARW", b"charlie"),
        ]);
        let h = Arc::new(super::tests::open(d.path()));
        let sf =
            |h: &Arc<SessionHandle>| block_on(h.clone().smart_folders(CancelToken::new())).unwrap();
        let folders = sf(&h);
        let keys: Vec<_> = folders.iter().map(|f| f.builtin.clone().unwrap()).collect();
        assert_eq!(
            keys,
            [
                "rating:5", "rating:4", "rating:3", "rating:2", "rating:1", "unrated", "picked",
                "rejected"
            ]
        );
        let count = |h: &Arc<SessionHandle>, k: &str| {
            sf(h)
                .into_iter()
                .find(|f| f.builtin.as_deref() == Some(k))
                .map(|f| f.count)
        };
        assert_eq!(count(&h, "unrated"), Some(3));
        let v = super::tests::list(&h);
        let a = super::tests::by_name(&v, "a.ARW").variant_id;
        let b = super::tests::by_name(&v, "b.ARW").variant_id;
        block_on(h.clone().set_rating(vec![a], 3, CancelToken::new())).unwrap();
        assert_eq!(count(&h, "rating:3"), Some(1));
        assert_eq!(count(&h, "rating:4"), Some(0));
        block_on(h.clone().set_rating(vec![a], 4, CancelToken::new())).unwrap();
        assert_eq!(
            (count(&h, "rating:3"), count(&h, "rating:4")),
            (Some(0), Some(1)),
            "moves, exact"
        );
        block_on(
            h.clone()
                .set_color_tag(vec![b], ColorLabel::Green, CancelToken::new()),
        )
        .unwrap();
        assert_eq!(
            count(&h, "tag:green"),
            Some(1),
            "tag folder appears once in use"
        );
        let r4 = sf(&h)
            .into_iter()
            .find(|f| f.builtin.as_deref() == Some("rating:4"))
            .unwrap();
        assert_eq!(
            block_on(h.clone().smart_folder_members(r4.id, CancelToken::new())).unwrap(),
            vec![a]
        );
        assert_eq!(r4.filter.min_rating, Some(4));
        assert_eq!(r4.filter.max_rating, Some(4));
        // Built-ins are protected; mirror toggles.
        assert!(block_on(h.clone().delete_smart_folder(r4.id, CancelToken::new())).is_err());
        block_on(
            h.clone()
                .set_smart_folder_mirror(r4.id, true, CancelToken::new()),
        )
        .unwrap();
        // User folder CRUD.
        let filter = SmartFilterRecord {
            min_rating: Some(1),
            tags: vec![ColorLabel::Green],
            ..SmartFilterRecord::default()
        };
        let id = block_on(h.clone().create_smart_folder(
            "Keepers".into(),
            filter.clone(),
            CancelToken::new(),
        ))
        .unwrap();
        let bad = SmartFilterRecord {
            max_rating: Some(9),
            ..SmartFilterRecord::default()
        };
        assert!(
            block_on(
                h.clone()
                    .create_smart_folder("Bad".into(), bad, CancelToken::new())
            )
            .is_err()
        );
        block_on(h.clone().set_rating(vec![b], 1, CancelToken::new())).unwrap();
        let user = sf(&h).into_iter().find(|f| f.id == id).unwrap();
        assert_eq!(
            (user.name.as_str(), user.count, user.builtin.clone()),
            ("Keepers", 1, None)
        );
        assert_eq!(user.filter, filter);
        let wider = SmartFilterRecord {
            min_rating: Some(1),
            ..SmartFilterRecord::default()
        };
        block_on(
            h.clone()
                .update_smart_folder(id, "Rated".into(), wider, CancelToken::new()),
        )
        .unwrap();
        assert_eq!(sf(&h).into_iter().find(|f| f.id == id).unwrap().count, 2);
        let unrated = SmartFilterRecord {
            max_rating: Some(0),
            ..SmartFilterRecord::default()
        };
        let q = block_on(h.clone().query_filter(unrated, CancelToken::new())).unwrap();
        assert_eq!(q.len(), 1);
        assert!(!q.contains(&a) && !q.contains(&b));
        // Relaunch: same counts, mirror kept.
        let before: Vec<_> = sf(&h)
            .into_iter()
            .map(|f| (f.id, f.count, f.mirror))
            .collect();
        drop(h);
        let h = Arc::new(super::tests::open(d.path()));
        let after: Vec<_> = sf(&h)
            .into_iter()
            .map(|f| (f.id, f.count, f.mirror))
            .collect();
        assert_eq!(before, after);
        block_on(h.clone().delete_smart_folder(id, CancelToken::new())).unwrap();
        assert!(sf(&h).iter().all(|f| f.id != id));
        // open_session also ensures the built-ins.
        let store = h.root().unwrap();
        drop(h);
        let plain = block_on(open_session(store, CancelToken::new())).unwrap();
        assert!(sf(&plain).len() >= 9);
    }
}

#[cfg(test)]
#[path = "ident_tests.rs"]
mod ident_tests;

pub mod find;
pub mod library;
#[cfg(test)]
mod library_tests;
pub mod map;
pub mod merge;
pub mod metadata;
pub mod people;
pub mod places;
pub mod search;
