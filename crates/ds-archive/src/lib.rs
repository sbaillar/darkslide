//! Session archiving: rsync to a mounted target (the NAS at `/Volumes/pics`)
//! on a schedule, plus the Amazon Photos folder hand-off for originals.
//!
//! Invariants: never `--delete`; replaced files go to
//! `.ds-archive-trash/<timestamp>` on the target; nothing is written unless
//! the target is mounted and writable; the live SQLite WAL is never copied
//! (a consistent snapshot from the injected [`Snapshotter`] is).

use std::io;
use std::path::{Path, PathBuf};

pub mod amazon;
pub mod archiver;
pub mod rsync;
pub mod scheduler;
pub mod target;

pub use amazon::{AmazonBackup, MirrorReport, Original, amazon_app_installed};
pub use archiver::{ArchiveOutcome, Archiver, SNAPSHOT_REL, Snapshotter, TRASH_DIR, prune_trash};
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
