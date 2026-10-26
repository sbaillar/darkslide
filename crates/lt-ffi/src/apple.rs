//! Aperture and iPhoto library import (`lt-apple`) through the FFI. A library
//! is read once into an [`AppleLibrarySource`] (its databases copied and the
//! copies read); dry runs and the import run against that object through
//! lt-lr's engine, so the report is a [`LrImportReportRecord`] like Lightroom's.
//! The library is only ever read.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use lt_apple::{AppleError, AppleLibrary};
use lt_lr::ImportOptions;

use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::lr::{LrImportReportRecord, LrRevertRecord, parse_categories};
use crate::session::SessionHandle;

impl From<AppleError> for DsError {
    fn from(e: AppleError) -> Self {
        let message = e.to_string();
        match e {
            AppleError::Io { .. } | AppleError::Db { .. } | AppleError::Xml { .. } => {
                Self::Io { message }
            }
            AppleError::NotALibrary { .. } => Self::InvalidArgument { message },
            AppleError::Unsupported { .. } => Self::Unsupported { message },
            AppleError::Cancelled => Self::Cancelled,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn n(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// What a path is, by its files only: `aperture`, `iphoto` (9.3 and later),
/// `iphoto-xml` (6 to 9.2), `photos` (a Photos library: not supported),
/// `old-aperture` (Aperture 1 or 2: not supported) or `unknown`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned strings
pub fn apple_library_kind(path: String) -> Result<String, DsError> {
    guard(|| Ok(lt_apple::detect(Path::new(&path)).key().to_owned()))
}

/// What a loaded library holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AppleLibrarySummary {
    /// See [`apple_library_kind`].
    pub format: String,
    /// The library's name.
    pub origin: String,
    pub written_by: Option<String>,
    /// Photos, every version counted.
    pub photos: u32,
    /// Projects, events and albums that become Luxtable albums.
    pub albums: u32,
    /// Smart albums, books, slideshows…: listed, not imported.
    pub skipped_albums: u32,
    pub warnings: Vec<String>,
}

/// An Aperture or iPhoto library, read once.
#[derive(Debug, uniffi::Object)]
pub struct AppleLibrarySource {
    lib: AppleLibrary,
}

impl AppleLibrarySource {
    pub(crate) fn load(
        path: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Arc<Self>, DsError> {
        let lib = lt_apple::read_library(Path::new(path), cancelled)?;
        Ok(Arc::new(Self { lib }))
    }
}

#[uniffi::export]
impl AppleLibrarySource {
    pub fn summary(&self) -> AppleLibrarySummary {
        let l = &self.lib.lib;
        let skipped = l
            .collections
            .iter()
            .filter(|c| c.smart || c.unsupported.is_some())
            .count();
        AppleLibrarySummary {
            format: self.lib.format.key().to_owned(),
            origin: l
                .origin
                .file_name()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            written_by: l.written_by.clone(),
            photos: n(l.photos.len()),
            albums: n(l.collections.len() - skipped),
            skipped_albums: n(skipped),
            warnings: l.warnings.clone(),
        }
    }
}

/// Read an Aperture or iPhoto library (never in place: its databases are
/// copied and the copies read). A Photos library is refused (`Unsupported`).
/// Cancellable.
#[uniffi::export]
pub async fn load_apple_library(
    path: String,
    token: Arc<CancelToken>,
) -> Result<Arc<AppleLibrarySource>, DsError> {
    // The database reader polls from a watcher thread, so it needs a `Sync` check.
    let watch = Arc::clone(&token);
    run_job_on_thread("lt-apple-read", token, move |cancelled| {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        AppleLibrarySource::load(&path, &|| watch.is_cancelled())
    })
    .await
}

impl SessionHandle {
    /// The import, on a library already loaded (tests call this directly).
    pub(crate) fn import_apple_with(
        &self,
        source: &AppleLibrarySource,
        opts: &ImportOptions,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<LrImportReportRecord, DsError> {
        self.check_folder()?;
        let mut session = lock(&self.session);
        let report = lt_lr::import(&mut session, &source.lib.lib, opts, cancelled)?;
        Ok(report.into())
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Bring the ratings, labels, flags, keywords, titles, captions, positions,
    /// mapped adjustments and albums of a loaded Aperture or iPhoto library
    /// (only the listed `categories`, by `lt_lr::Category::key`) into this
    /// session. Like the Lightroom import: re-running updates, a field changed
    /// in Luxtable since is a conflict kept unless `overwrite_edits`, and
    /// `dry_run` writes nothing.
    pub async fn import_apple_library(
        self: Arc<Self>,
        source: Arc<AppleLibrarySource>,
        categories: Vec<String>,
        dry_run: bool,
        overwrite_edits: bool,
        token: Arc<CancelToken>,
    ) -> Result<LrImportReportRecord, DsError> {
        let opts = ImportOptions {
            dry_run,
            overwrite_edits,
            categories: parse_categories(&categories)?,
            flavor: lt_apple::FLAVOR,
        };
        run_job_on_thread("lt-apple-import", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            self.import_apple_with(&source, &opts, cancelled)
        })
        .await
    }

    /// There is an Aperture / iPhoto import to revert.
    pub fn can_revert_apple_import(&self) -> bool {
        lt_lr::can_revert_as(&lock(&self.session), &lt_apple::FLAVOR)
    }

    /// Take back the Aperture / iPhoto imports since the last revert; `None`: nothing to revert.
    pub async fn revert_apple_import(
        self: Arc<Self>,
        token: Arc<CancelToken>,
    ) -> Result<Option<LrRevertRecord>, DsError> {
        run_job_on_thread("lt-apple-revert", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            let mut session = lock(&self.session);
            Ok(lt_lr::revert_as(&mut session, &lt_apple::FLAVOR)?.map(LrRevertRecord::from))
        })
        .await
    }
}

/// Whether a folder about to be opened in place is inside an Apple photo
/// library (its session's store then goes elsewhere: the library is never written).
pub(crate) fn inside_library(folder: &Path) -> bool {
    lt_apple::is_inside_library(folder)
}

#[cfg(test)]
#[path = "apple_tests.rs"]
mod tests;
