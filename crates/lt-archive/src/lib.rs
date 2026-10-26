//! Session archiving on a schedule, to one of two kinds of [`Destination`]:
//!
//! - a **folder** ([`FolderDestination`]): rsync to a mounted volume (the NAS at
//!   `/Volumes/pics`, an external disk) or a local folder;
//! - **S3-compatible object storage** ([`object::ObjectDestination`]): AWS S3,
//!   Backblaze B2, Wasabi, Cloudflare R2, `MinIO`.
//!
//! Plus the Amazon Photos folder hand-off for originals ([`amazon`]), which doesn't
//! depend on the destination.
//!
//! Invariants, whichever the destination: nothing is ever deleted from the archive
//! (never `--delete`; no `DeleteObject`); a replaced file's previous version goes to
//! `.lt-archive-trash/<timestamp>` first; nothing is written unless the destination
//! answers (a folder's volume mounted and writable, a bucket reachable with the
//! keys); the live SQLite WAL is never copied (a consistent snapshot from the
//! injected [`Snapshotter`] is); `.lt/cache` and `*.partial` stay out.

use std::io;
use std::path::{Path, PathBuf};

pub mod amazon;
pub mod archiver;
pub mod dest;
pub mod exclude;
pub mod object;
pub mod rsync;
pub mod scheduler;
pub mod target;

pub use amazon::{AmazonBackup, MirrorReport, Original, amazon_app_installed};
pub use archiver::{ArchiveOutcome, Archiver, SNAPSHOT_REL, Snapshotter, TRASH_DIR, prune_trash};
pub use dest::{Destination, DestinationKind, FolderDestination, SessionSource};
pub use object::{ObjectDestination, ObjectStoreConfig};
pub use rsync::{
    CommandOutput, CommandRunner, CommandSpec, RsyncFlavor, RsyncProgress, RsyncRunner,
    SystemRunner,
};
pub use scheduler::{
    ArchiveStatus, Clock, ManualClock, RunRecord, Scheduler, SchedulerConfig, SystemClock, Ticket,
    Trigger,
};
pub use target::ArchiveTarget;
pub use target::{ARCHIVE_OWNER_FILE, ClaimedDest, VolumeId};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("no rsync found (install Homebrew rsync or use /usr/bin/rsync)")]
    RsyncNotFound,
    /// The target's volume is not mounted ("offline"): changes stay queued.
    #[error("archive target {} unavailable: {reason}", root.display())]
    TargetUnavailable { root: PathBuf, reason: String },
    /// The volume is there but the target can't be used: no permission to write (or to
    /// create the folder), the path is a file, or it isn't absolute. Needs the user.
    #[error("archive target {} can't be used: {reason}", root.display())]
    TargetNotUsable { root: PathBuf, reason: String },
    #[error("not a session folder: {}", .0.display())]
    InvalidSession(PathBuf),
    #[error("rsync failed (exit {code:?}): {stderr}")]
    Rsync { code: Option<i32>, stderr: String },
    #[error("cancelled")]
    Cancelled,
    /// Object storage answered, but refused or garbled what was sent (after retries).
    #[error("object storage: {0}")]
    Storage(String),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("amazon manifest: {0}")]
    Manifest(String),
    /// The run finished, but these files (`"<path>: <reason>"`) are not in
    /// the archive (unreadable originals, a failed DB snapshot). Not a
    /// success: the scheduler keeps the changes pending and retries.
    #[error("{} not archived: {}", count_files(.problems.len()), .problems.join("; "))]
    Incomplete { problems: Vec<String> },
    /// The Amazon Photos folder chosen in Settings is gone (moved, deleted,
    /// or its disk is not connected). It is never recreated.
    #[error("Amazon Photos folder missing: {}", .0.display())]
    AmazonFolderMissing(PathBuf),
}

fn count_files(n: usize) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{n} items")
    }
}

impl ArchiveError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
