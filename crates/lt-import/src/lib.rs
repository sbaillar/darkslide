//! Import from memory cards: scan, plan (rename/subfolder/dedupe/backup),
//! and a verified, resumable, single-read-pass copy.
//!
//! ```no_run
//! # use std::sync::atomic::AtomicBool;
//! # use lt_import::*;
//! let candidates = scan_volume("/Volumes/SONY".as_ref())?.candidates;
//! let mut opts = PlanOptions::new("/Users/me/Pictures/Luxtable/Trip/Capture");
//! opts.rename = Some("{date:%Y%m%d}_{seq:4}".into());
//! let plan = ImportPlan::new(&candidates, opts, &NoMeta)?;
//! let report = run(&plan, &|p| println!("{}/{}", p.files_done, p.files_total), &AtomicBool::new(false))?;
//! assert!(report.is_complete());
//! # Ok::<(), ImportError>(())
//! ```
//!
//! Safety properties:
//! * The source is only ever opened `O_RDONLY | O_NOFOLLOW`; nothing is
//!   written to, renamed on, or deleted from the card.
//! * Nothing in the destination is ever overwritten: names are resolved at
//!   plan time and placed with a no-replace rename.
//! * A file is reported as copied only after its bytes were re-read from the
//!   destination (page cache bypassed) and matched the source BLAKE3.

use std::io;
use std::path::{Path, PathBuf};

pub mod dedupe;
pub mod fsio;
pub mod journal;
pub mod plan;
pub mod raw;
pub mod run;
pub mod scan;

pub use dedupe::{
    DedupeLookup, KnownCopy, MemoryDedupe, PartialHash, find_duplicate, full_hash, partial_hash,
};
pub use journal::{
    ResumeInfo, discard as discard_journal, journal_path, journal_paths, pending_resume,
};
pub use plan::{
    CaptureMeta, ImportPlan, MetaProvider, NoMeta, PlanOptions, PlannedFile, PlannedItem,
};
pub use raw::{RawMeta, structurally_valid};
pub use run::{
    FailedFile, FaultInjector, ImportProgress, ImportReport, ImportedFile, NOT_REGISTERED,
    RunHooks, SessionSink, SkippedDuplicate, run, run_with, verify_hash,
};
pub use scan::{Candidate, FileKind, ScanError, ScanReport, SkippedFile, SourceFile, scan_volume};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("template: {0}")]
    Template(#[from] lt_template::TemplateError),
    #[error("import journal: {0}")]
    Journal(String),
    /// A journal of another format version: older ones are set aside, newer
    /// ones (written by a later Luxtable) are left alone and never rewritten.
    #[error("import journal {}: format version {version}, this build reads {}", path.display(), journal::JOURNAL_VERSION)]
    JournalVersion { path: PathBuf, version: u32 },
    #[error("another import into {} is running", .0.display())]
    Busy(PathBuf),
}

impl ImportError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
