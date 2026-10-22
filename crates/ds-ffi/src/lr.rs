//! Lightroom import (`ds-lr`) through the FFI: a catalog or a folder of XMP
//! sidecars is read once into a [`LightroomSource`] (a multi-GB catalog is
//! copied and read a single time), dry runs and the import run against that
//! object, and presets are mapped to Darkslide adjustments for the app's own
//! preset store. Lightroom's files are only ever read.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ds_core::geom::Orientation;
use ds_lr::{Category, Fidelity, ImportOptions, ImportReport, LrError, LrLibrary, RevertReport};
use ds_session::ImageMetaLite;

use crate::clipboard::AdjustGroup;
use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::session::SessionHandle;

impl From<LrError> for DsError {
    fn from(e: LrError) -> Self {
        let message = e.to_string();
        match e {
            LrError::Session(s) => s.into(),
            LrError::Io { .. }
            | LrError::Db { .. }
            | LrError::Xmp { .. }
            | LrError::Preset { .. } => Self::Io { message },
            LrError::NotACatalog { .. } => Self::InvalidArgument { message },
            LrError::Cancelled => Self::Cancelled,
        }
    }
}

/// Most unmatched photos listed individually; the count keeps the true number.
const MAX_NOT_FOUND: usize = 500;
/// Entries looked at when guessing what a folder holds.
const KIND_SCAN_LIMIT: usize = 2000;
const KIND_SCAN_DEPTH: usize = 3;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn n(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// The first bytes of an XMP file say whether it is a preset: a
/// `crs:PresetType` attribute or element (the word in a caption doesn't count).
fn xmp_is_preset(p: &Path) -> bool {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = fs::File::open(p).and_then(|f| f.take(256 * 1024).read_to_end(&mut buf));
    let text = String::from_utf8_lossy(&buf);
    text.contains("crs:PresetType=") || text.contains("<crs:PresetType>")
}

/// Extensions of the files a sidecar sits next to.
const SIBLINGS: [&str; 13] = [
    "arw", "cr2", "cr3", "nef", "dng", "raf", "orf", "rw2", "pef", "srw", "jpg", "jpeg", "tif",
];

/// A file of the same name with a photo's extension is beside `xmp`: it is a
/// sidecar, whatever its text says.
fn has_photo_sibling(xmp: &Path) -> bool {
    SIBLINGS.iter().any(|e| {
        xmp.with_extension(e).is_file() || xmp.with_extension(e.to_ascii_uppercase()).is_file()
    })
}

fn is_preset_file(p: &Path) -> bool {
    has_ext(p, &["lrtemplate"])
        || (has_ext(p, &["xmp"]) && xmp_is_preset(p) && !has_photo_sibling(p))
}

/// What a path is, without reading it fully: `catalog` (`.lrcat`), `sidecars`
/// (a folder of `.xmp` / `.dng` files), `presets` (a preset `.xmp`, a
/// `.lrtemplate`, or a folder of them) or `unknown`.
pub fn source_kind(path: &Path) -> &'static str {
    let Ok(md) = fs::metadata(path) else {
        return "unknown";
    };
    if md.is_file() {
        return if has_ext(path, &["lrcat"]) {
            "catalog"
        } else if is_preset_file(path) {
            "presets"
        } else {
            "unknown"
        };
    }
    let (mut sidecars, mut presets, mut seen) = (false, false, 0usize);
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > KIND_SCAN_LIMIT {
                return kind_of(sidecars, presets);
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() && depth < KIND_SCAN_DEPTH {
                stack.push((p, depth + 1));
            } else if ft.is_file() {
                if is_preset_file(&p) {
                    presets = true;
                } else {
                    sidecars |= has_ext(&p, &["xmp", "dng"]);
                }
            }
        }
    }
    kind_of(sidecars, presets)
}

/// A folder with sidecars is a sidecar folder even if it also holds a preset.
fn kind_of(sidecars: bool, presets: bool) -> &'static str {
    if sidecars {
        "sidecars"
    } else if presets {
        "presets"
    } else {
        "unknown"
    }
}

/// Whether `path` is a Lightroom catalog, a folder of sidecars, presets, or none.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned strings
pub fn lightroom_source_kind(path: String) -> Result<String, DsError> {
    guard(|| Ok(source_kind(Path::new(&path)).to_owned()))
}

/// What a loaded source holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrSourceSummary {
    /// `catalog` or `sidecars`.
    pub kind: String,
    /// File or folder name.
    pub origin: String,
    pub written_by: Option<String>,
    pub photos: u32,
    pub collections: u32,
    pub smart_collections: u32,
    pub warnings: Vec<String>,
}

/// A catalog or a folder of sidecars, read once.
#[derive(Debug, uniffi::Object)]
pub struct LightroomSource {
    lib: LrLibrary,
}

impl LightroomSource {
    pub(crate) fn new(lib: LrLibrary) -> Arc<Self> {
        Arc::new(Self { lib })
    }

    pub(crate) fn load(
        path: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Arc<Self>, DsError> {
        let p = Path::new(path);
        let lib = match source_kind(p) {
            "catalog" => ds_lr::read_catalog(p, cancelled)?,
            "sidecars" => ds_lr::read_sidecars(p, cancelled)?,
            "presets" => {
                return Err(DsError::invalid(
                    "that is a preset file: import presets instead",
                ));
            }
            _ => {
                return Err(DsError::invalid(
                    "not a Lightroom catalog (.lrcat) or a folder of XMP sidecars",
                ));
            }
        };
        Ok(Self::new(lib))
    }
}

#[uniffi::export]
impl LightroomSource {
    pub fn summary(&self) -> LrSourceSummary {
        let l = &self.lib;
        LrSourceSummary {
            kind: match l.kind {
                ds_lr::SourceKind::Catalog => "catalog",
                ds_lr::SourceKind::Sidecars => "sidecars",
            }
            .to_owned(),
            origin: l
                .origin
                .file_name()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            written_by: l.written_by.clone(),
            photos: n(l.photos.len()),
            collections: n(l.collections.iter().filter(|c| !c.smart).count()),
            smart_collections: n(l.collections.iter().filter(|c| c.smart).count()),
            warnings: l.warnings.clone(),
        }
    }
}

/// Read a Lightroom catalog or sidecar folder (never in place: a catalog is
/// copied and the copy read). Cancellable.
#[uniffi::export]
pub async fn load_lightroom_source(
    path: String,
    token: Arc<CancelToken>,
) -> Result<Arc<LightroomSource>, DsError> {
    // The catalog reader polls from a watcher thread, so it needs a `Sync` check.
    let watch = Arc::clone(&token);
    run_job_on_thread("ds-lr-read", token, move |cancelled| {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        LightroomSource::load(&path, &|| watch.is_cancelled())
    })
    .await
}

/// A photo with no Darkslide photo in the session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrNotFoundRecord {
    pub path: String,
    pub folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrFolderRecord {
    pub folder: String,
    pub photos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrChangedRecord {
    /// A [`Category::key`].
    pub category: String,
    pub count: u32,
}

/// A mapped setting and how many variants have it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrMappedRecord {
    pub key: String,
    pub approximate: bool,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrDroppedRecord {
    pub key: String,
    pub reason: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrLabelRecord {
    pub name: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrConflictRecord {
    pub variant_id: i64,
    pub file: String,
    /// `rating`, `title`, `caption`, `copyright` or a [`Category::key`].
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrSkippedRecord {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrNoteRecord {
    pub file: String,
    pub notes: Vec<String>,
}

/// What a Lightroom import did, or (dry run) would do. See `ds_lr::ImportReport`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrImportReportRecord {
    pub dry_run: bool,
    pub written_by: Option<String>,
    pub photos: u32,
    pub matched: u32,
    pub matched_by_path: u32,
    pub matched_by_hash: u32,
    pub matched_by_name: u32,
    pub ambiguous: Vec<String>,
    /// The first 500 unmatched photos.
    pub not_found: Vec<LrNotFoundRecord>,
    /// All unmatched photos.
    pub not_found_count: u32,
    pub not_found_folders: Vec<LrFolderRecord>,
    pub variants: u32,
    pub variants_created: u32,
    /// Photos written per category, most first.
    pub changed: Vec<LrChangedRecord>,
    /// Most frequent first.
    pub mapped: Vec<LrMappedRecord>,
    /// Most frequent first.
    pub dropped: Vec<LrDroppedRecord>,
    pub with_dropped: u32,
    pub unmapped_labels: Vec<LrLabelRecord>,
    pub in_sync: u32,
    pub kept_darkslide: u32,
    pub deleted_in_darkslide: u32,
    pub conflicts: Vec<LrConflictRecord>,
    pub overwritten: u32,
    pub read_only: u32,
    pub albums_created: u32,
    pub albums_updated: u32,
    pub collections_skipped: Vec<LrSkippedRecord>,
    pub notes: Vec<LrNoteRecord>,
    pub warnings: Vec<String>,
}

fn by_count<T, K: Ord>(v: &mut [T], count: impl Fn(&T) -> u32, key: impl Fn(&T) -> K) {
    v.sort_by(|a, b| count(b).cmp(&count(a)).then_with(|| key(a).cmp(&key(b))));
}

#[allow(clippy::too_many_lines)] // one field-by-field mirror of the report
impl From<ImportReport> for LrImportReportRecord {
    fn from(r: ImportReport) -> Self {
        let mut changed: Vec<LrChangedRecord> = r
            .changed
            .into_iter()
            .map(|(c, count)| LrChangedRecord {
                category: c.key().to_owned(),
                count: n(count),
            })
            .collect();
        by_count(&mut changed, |c| c.count, |c| c.category.clone());
        let mut mapped: Vec<LrMappedRecord> = r
            .mapped_keys
            .into_iter()
            .map(|(key, (f, count))| LrMappedRecord {
                key,
                approximate: f == Fidelity::Approximate,
                count: n(count),
            })
            .collect();
        by_count(&mut mapped, |m| m.count, |m| m.key.clone());
        let mut dropped: Vec<LrDroppedRecord> = r
            .dropped
            .into_iter()
            .map(|(key, (reason, count))| LrDroppedRecord {
                key,
                reason,
                count: n(count),
            })
            .collect();
        by_count(&mut dropped, |d| d.count, |d| d.key.clone());
        let mut unmapped_labels: Vec<LrLabelRecord> = r
            .unmapped_labels
            .into_iter()
            .map(|(name, count)| LrLabelRecord {
                name,
                count: n(count),
            })
            .collect();
        by_count(&mut unmapped_labels, |l| l.count, |l| l.name.clone());
        Self {
            dry_run: r.dry_run,
            written_by: r.written_by,
            photos: n(r.photos),
            matched: n(r.matched),
            matched_by_path: n(r.matched_by_path),
            matched_by_hash: n(r.matched_by_hash),
            matched_by_name: n(r.matched_by_name),
            ambiguous: r.ambiguous,
            not_found_count: n(r.not_found.len()),
            not_found: r
                .not_found
                .into_iter()
                .take(MAX_NOT_FOUND)
                .map(|f| LrNotFoundRecord {
                    path: f.path,
                    folder: f.folder,
                })
                .collect(),
            not_found_folders: r
                .not_found_folders
                .into_iter()
                .map(|f| LrFolderRecord {
                    folder: f.folder,
                    photos: n(f.photos),
                })
                .collect(),
            variants: n(r.variants),
            variants_created: n(r.variants_created),
            changed,
            mapped,
            dropped,
            with_dropped: n(r.with_dropped),
            unmapped_labels,
            in_sync: n(r.in_sync),
            kept_darkslide: n(r.kept_darkslide),
            deleted_in_darkslide: n(r.deleted_in_darkslide),
            conflicts: r
                .conflicts
                .into_iter()
                .map(|c| LrConflictRecord {
                    variant_id: c.variant.0,
                    file: c.file,
                    field: c.field,
                })
                .collect(),
            overwritten: n(r.overwritten),
            read_only: n(r.read_only),
            albums_created: n(r.albums_created),
            albums_updated: n(r.albums_updated),
            collections_skipped: r
                .collections_skipped
                .into_iter()
                .map(|s| LrSkippedRecord {
                    name: s.name,
                    reason: s.reason,
                })
                .collect(),
            notes: r
                .notes
                .into_iter()
                .map(|p| LrNoteRecord {
                    file: p.file,
                    notes: p.notes,
                })
                .collect(),
            warnings: r.warnings,
        }
    }
}

/// What "Revert Lightroom Import" did. See `ds_lr::RevertReport`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrRevertRecord {
    pub restored: u32,
    pub kept: u32,
    pub variants_removed: u32,
    pub albums_removed: u32,
    pub albums_trimmed: u32,
}

impl From<RevertReport> for LrRevertRecord {
    fn from(r: RevertReport) -> Self {
        Self {
            restored: n(r.restored),
            kept: n(r.kept),
            variants_removed: n(r.variants_removed),
            albums_removed: n(r.albums_removed),
            albums_trimmed: n(r.albums_trimmed),
        }
    }
}

/// Categories by their keys; an unknown key is an error, an empty list means none.
fn parse_categories(keys: &[String]) -> Result<std::collections::BTreeSet<Category>, DsError> {
    keys.iter()
        .map(|k| {
            Category::from_key(k).ok_or_else(|| DsError::invalid(format!("unknown category {k}")))
        })
        .collect()
}

impl SessionHandle {
    /// The import, on a source already loaded (tests call this directly).
    pub(crate) fn import_lightroom_with(
        &self,
        source: &LightroomSource,
        opts: &ImportOptions,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<LrImportReportRecord, DsError> {
        self.check_folder()?;
        let mut session = lock(&self.session);
        let report = ds_lr::import(&mut session, &source.lib, opts, cancelled)?;
        Ok(report.into())
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Bring the ratings, labels, flags, keywords, titles, mapped edits and
    /// collections of a loaded Lightroom source (only the listed `categories`,
    /// by [`Category::key`]) into this session. Re-running updates instead of
    /// duplicating; a field changed in Darkslide since the last import is a
    /// conflict, kept unless `overwrite_edits`. `dry_run` writes nothing.
    pub async fn import_lightroom(
        self: Arc<Self>,
        source: Arc<LightroomSource>,
        categories: Vec<String>,
        dry_run: bool,
        overwrite_edits: bool,
        token: Arc<CancelToken>,
    ) -> Result<LrImportReportRecord, DsError> {
        let opts = ImportOptions {
            dry_run,
            overwrite_edits,
            categories: parse_categories(&categories)?,
        };
        run_job_on_thread("ds-lr-import", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            self.import_lightroom_with(&source, &opts, cancelled)
        })
        .await
    }

    /// There is a Lightroom import to revert.
    pub fn can_revert_lightroom_import(&self) -> bool {
        ds_lr::can_revert(&lock(&self.session))
    }

    /// Take back the Lightroom imports since the last revert; `None`: nothing to revert.
    pub async fn revert_lightroom_import(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<Option<LrRevertRecord>, DsError> {
        run_job_on_thread("ds-lr-revert", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            let mut session = lock(&self.session);
            Ok(ds_lr::revert(&mut session)?.map(LrRevertRecord::from))
        })
        .await
    }
}

/// A Lightroom preset mapped to Darkslide.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LrPresetRecord {
    pub name: String,
    pub group: Option<String>,
    /// `ds_core::Adjustments` JSON: defaults plus the preset's mapped settings.
    pub adjustments_json: String,
    /// The adjustment groups the preset sets.
    pub groups: Vec<AdjustGroup>,
    /// What is approximate or left out, one line each.
    pub notes: Vec<String>,
}

fn groups_of(cats: &std::collections::BTreeSet<Category>) -> Vec<AdjustGroup> {
    let mut out: Vec<AdjustGroup> = Vec::new();
    for c in cats {
        let gs: &[AdjustGroup] = match c {
            Category::Crop => &[AdjustGroup::Crop],
            Category::Tone => &[AdjustGroup::Exposure, AdjustGroup::Hdr, AdjustGroup::Curve],
            Category::WhiteBalance => &[AdjustGroup::WhiteBalance],
            Category::Color => &[
                AdjustGroup::Exposure,
                AdjustGroup::Presence,
                AdjustGroup::ColorEditor,
                AdjustGroup::ColorBalance,
            ],
            Category::Detail => &[
                AdjustGroup::Presence,
                AdjustGroup::Detail,
                AdjustGroup::Noise,
            ],
            Category::Effects => &[AdjustGroup::Vignette, AdjustGroup::Effects],
            Category::Lens => &[AdjustGroup::Lens],
            Category::Masks => &[AdjustGroup::Layers],
            _ => &[],
        };
        for g in gs {
            if !out.contains(g) {
                out.push(*g);
            }
        }
    }
    out
}

/// A neutral photo for mapping a preset, which belongs to no photo.
fn neutral_meta() -> ImageMetaLite {
    ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    }
}

pub(crate) fn presets_at(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<LrPresetRecord>, DsError> {
    let meta = neutral_meta();
    let ctx = ds_lr::MapContext {
        meta: &meta,
        is_raw: true,
        lr_orientation: None,
    };
    ds_lr::read_presets(path, cancelled)?
        .into_iter()
        .map(|p| {
            let m = ds_lr::map_develop(&p.develop, &ctx);
            let mut notes: Vec<String> = m
                .mapped
                .iter()
                .filter(|(_, f)| *f == Fidelity::Approximate)
                .map(|(k, _)| format!("{k}: approximate"))
                .collect();
            notes.extend(m.dropped.iter().map(|(k, why)| format!("{k}: {why}")));
            Ok(LrPresetRecord {
                name: p.name,
                group: p.group,
                adjustments_json: SessionHandle::to_json(&m.adjustments.sanitized())?,
                groups: groups_of(&m.categories),
                notes,
            })
        })
        .collect()
}

/// Read Lightroom presets (`.xmp` or `.lrtemplate`, one file or a folder) and
/// map each to Darkslide adjustments. Nothing is written.
#[uniffi::export]
pub async fn read_lightroom_presets(
    path: String,
    token: Arc<CancelToken>,
) -> Result<Vec<LrPresetRecord>, DsError> {
    run_job_on_thread("ds-lr-presets", token, move |cancelled| {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        presets_at(Path::new(&path), cancelled)
    })
    .await
}

#[cfg(test)]
#[path = "lr_tests.rs"]
mod tests;
