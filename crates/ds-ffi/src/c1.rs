//! Capture One session import (`ds-c1`) for an in-place session of a C1
//! session folder. The C1 folder is only read (see `ds_c1`); everything is
//! written to the Darkslide session's own store.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ds_c1::settings::Cancel;
use ds_c1::{C1Error, Field, ImportOptions, ImportReport, RevertReport, WbConverter};
use ds_core::adjust::WhiteBalance;
use ds_pipeline::color::{CameraColor, multipliers_to_wb, normalize_mults};
use ds_session::Session;

use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::session::SessionHandle;

impl From<C1Error> for DsError {
    fn from(e: C1Error) -> Self {
        let message = e.to_string();
        match e {
            C1Error::Session(s) => s.into(),
            C1Error::Io { .. } | C1Error::Db { .. } | C1Error::Cos { .. } => Self::Io { message },
            C1Error::NotASession(_) | C1Error::WrongSession(_) => Self::InvalidArgument { message },
            C1Error::Cancelled => Self::Cancelled,
        }
    }
}

/// White balance through the original's camera matrix: C1's edit is applied
/// as a ratio to C1's as-shot multipliers, on top of the camera's as-shot.
struct RawWb;

impl WbConverter for RawWb {
    fn convert(
        &self,
        original: &Path,
        c1_default: Option<[f64; 3]>,
        c1_user: [f64; 3],
    ) -> Option<WhiteBalance> {
        let info = ds_raw::color_info(original).ok()?;
        Some(wb_from(&info, c1_default, c1_user))
    }
}

fn wb_from(
    info: &ds_raw::ColorInfo,
    c1_default: Option<[f64; 3]>,
    c1_user: [f64; 3],
) -> WhiteBalance {
    let mults: [f32; 3] = std::array::from_fn(|c| match c1_default {
        Some(d) if d[c] > 0.0 => (f64::from(info.wb_as_shot[c]) * c1_user[c] / d[c]) as f32,
        _ => c1_user[c] as f32,
    });
    let cam = CameraColor {
        cam_to_xyz: info.cam_to_xyz,
        daylight: normalize_mults(info.wb_daylight),
    };
    multipliers_to_wb(mults, &cam)
}

/// An edit with no Darkslide translation, how many variants have it, and why.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1UnmappedRecord {
    pub key: String,
    pub reason: String,
    pub count: u32,
}

/// A mapped edit and how many variants have it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1MappedRecord {
    pub key: String,
    pub count: u32,
}

/// A field both Darkslide and Capture One changed since the last import.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1ConflictRecord {
    pub variant_id: i64,
    /// Path relative to the C1 session folder.
    pub file: String,
    /// `rating`, `color_tag`, `pick`, `keywords` or `adjustments`.
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1SkippedRecord {
    pub name: String,
    pub reason: String,
}

/// What a Capture One import did, or (dry run) would do. See `ds_c1::ImportReport`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1ImportReportRecord {
    pub dry_run: bool,
    /// The Capture One build that last wrote the session.
    pub written_by: Option<String>,
    pub images: u32,
    pub matched: u32,
    pub variants: u32,
    pub variants_created: u32,
    pub not_found: Vec<String>,
    pub outside_folder: u32,
    pub in_trash: u32,
    pub in_output: u32,
    pub in_selects: u32,
    pub rated: u32,
    pub tagged: u32,
    pub picked: u32,
    pub keyworded: u32,
    pub edits_mapped: u32,
    pub edited_in_c1: u32,
    pub with_unmapped: u32,
    /// Mapped edit keys, most frequent first.
    pub mapped: Vec<C1MappedRecord>,
    /// Most frequent first.
    pub unmapped: Vec<C1UnmappedRecord>,
    pub in_sync: u32,
    pub kept_darkslide: u32,
    pub deleted_in_darkslide: u32,
    pub conflicts: Vec<C1ConflictRecord>,
    pub overwritten: u32,
    pub read_only: u32,
    pub albums_created: u32,
    pub albums_updated: u32,
    pub collections_skipped: Vec<C1SkippedRecord>,
    pub warnings: Vec<String>,
}

fn n(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn field_name(f: Field) -> &'static str {
    match f {
        Field::Rating => "rating",
        Field::ColorTag => "color_tag",
        Field::Pick => "pick",
        Field::Keywords => "keywords",
        Field::Adjustments => "adjustments",
    }
}

impl C1ImportReportRecord {
    fn from(r: ImportReport, written_by: Option<String>) -> Self {
        let mut unmapped: Vec<C1UnmappedRecord> = r
            .unmapped
            .into_iter()
            .map(|(key, (reason, count))| C1UnmappedRecord {
                key,
                reason,
                count: n(count),
            })
            .collect();
        unmapped.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        let mut mapped: Vec<C1MappedRecord> = r
            .mapped_keys
            .into_iter()
            .map(|(key, count)| C1MappedRecord {
                key,
                count: n(count),
            })
            .collect();
        mapped.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        Self {
            dry_run: r.dry_run,
            written_by,
            images: n(r.images),
            matched: n(r.matched),
            variants: n(r.variants),
            variants_created: n(r.variants_created),
            not_found: r.not_found,
            outside_folder: n(r.outside_folder),
            in_trash: n(r.in_trash),
            in_output: n(r.in_output),
            in_selects: n(r.in_selects),
            rated: n(r.rated),
            tagged: n(r.tagged),
            picked: n(r.picked),
            keyworded: n(r.keyworded),
            edits_mapped: n(r.edits_mapped),
            edited_in_c1: n(r.edited_in_c1),
            with_unmapped: n(r.with_unmapped),
            mapped,
            unmapped,
            in_sync: n(r.in_sync),
            kept_darkslide: n(r.kept_darkslide),
            deleted_in_darkslide: n(r.deleted_in_darkslide),
            conflicts: r
                .conflicts
                .into_iter()
                .map(|c| C1ConflictRecord {
                    variant_id: c.variant.0,
                    file: c.file,
                    field: field_name(c.field).to_owned(),
                })
                .collect(),
            overwritten: n(r.overwritten),
            read_only: n(r.read_only),
            albums_created: n(r.albums_created),
            albums_updated: n(r.albums_updated),
            collections_skipped: r
                .collections_skipped
                .into_iter()
                .map(|s| C1SkippedRecord {
                    name: s.name,
                    reason: s.reason,
                })
                .collect(),
            warnings: r.warnings,
        }
    }
}

/// What "Revert Capture One Import" did. See `ds_c1::RevertReport`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1RevertRecord {
    /// Variants with at least one field put back.
    pub restored: u32,
    /// Fields, variants and albums changed in Darkslide since the import (left alone).
    pub kept: u32,
    pub variants_removed: u32,
    pub albums_removed: u32,
    pub albums_trimmed: u32,
}

impl From<RevertReport> for C1RevertRecord {
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

/// Whether `folder` is a Capture One session folder (a `*.cosessiondb` at its top).
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned strings
pub fn is_capture_one_folder(folder: String) -> Result<bool, DsError> {
    guard(|| Ok(ds_session::is_capture_one_session(Path::new(&folder))))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionHandle {
    /// The import, with the white-balance conversion injected (tests).
    pub(crate) fn import_capture_one_with(
        &self,
        wb: &dyn WbConverter,
        opts: ImportOptions,
        cancel: &Cancel,
    ) -> Result<C1ImportReportRecord, DsError> {
        let folder = self.in_place.clone().ok_or_else(|| {
            DsError::invalid("only a folder opened in place can import its Capture One session")
        })?;
        self.check_folder()?;
        // Read (and copy the database) before taking the session lock.
        let c1 = ds_c1::read_session_cancellable(&folder, cancel)?;
        let mut session: MutexGuard<'_, Session> = lock(&self.session);
        let report = ds_c1::import_cancellable(&mut session, &c1, wb, opts, &|| cancel())?;
        Ok(C1ImportReportRecord::from(report, c1.written_by))
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Bring the ratings, colour tags, keywords, Selects (as picked), mapped
    /// edits and albums of the folder's Capture One session into this
    /// in-place session. Re-running updates instead of duplicating; a field
    /// changed in Darkslide since the last import is reported as a conflict
    /// and kept unless `overwrite_edits`. `dry_run` writes nothing.
    pub async fn import_capture_one(
        self: Arc<Self>,
        dry_run: bool,
        overwrite_edits: bool,
        token: Arc<CancelToken>,
    ) -> Result<C1ImportReportRecord, DsError> {
        // The database read polls this from inside its queries.
        let probe = token.clone();
        let cancel: Cancel = Arc::new(move || probe.is_cancelled());
        // Seconds of file reads: a thread of its own, not a render pool slot.
        run_job_on_thread("ds-c1-import", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            self.import_capture_one_with(
                &RawWb,
                ImportOptions {
                    dry_run,
                    overwrite_edits,
                },
                &cancel,
            )
        })
        .await
    }

    /// There is a Capture One import to revert (one wrote something since the last revert).
    pub fn can_revert_capture_one_import(&self) -> bool {
        ds_c1::can_revert(&lock(&self.session))
    }

    /// Take back the Capture One imports since the last revert: fields still
    /// holding what an import wrote go back to their earlier values, and the
    /// variants and albums it created go away unless changed since. `None`:
    /// nothing to revert.
    pub async fn revert_capture_one_import(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<Option<C1RevertRecord>, DsError> {
        run_job_on_thread("ds-c1-revert", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            let mut session = lock(&self.session);
            Ok(ds_c1::revert(&mut session)?.map(C1RevertRecord::from))
        })
        .await
    }
}

#[cfg(test)]
#[path = "c1_tests.rs"]
mod tests;
