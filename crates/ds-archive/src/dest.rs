//! Where sessions are archived: one [`Destination`] trait, two backends. The
//! scheduler, the status, the Immich hook and the FFI work through the trait and
//! don't care which one is in use.
//!
//! - [`FolderDestination`]: rsync into a folder ([`ArchiveTarget`]): a mounted volume
//!   (a NAS share, an external disk) or a local folder.
//! - [`crate::object::ObjectDestination`]: S3-compatible object storage.
//!
//! Every backend keeps the same promises: nothing is ever deleted from the archive,
//! a replaced file's previous version goes to `.ds-archive-trash/<timestamp>/`, the
//! live SQLite files never leave the Mac (a consistent snapshot does), `.ds/cache`
//! and `*.partial` stay out, and nothing is written unless the destination answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::ArchiveError;
use crate::archiver::{ArchiveOutcome, Archiver, Snapshotter};
use crate::rsync::{RsyncProgress, RsyncRunner};
use crate::target::{ArchiveTarget, ClaimedDest};

/// What kind of place a destination is (for the settings and the status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    /// A volume that must be mounted: under `/Volumes`, or a network share.
    MountedVolume,
    /// A folder on a local disk: it must exist and be writable.
    LocalFolder,
    /// S3-compatible object storage.
    ObjectStorage,
}

impl DestinationKind {
    /// `volume`, `local`, `object` (the FFI's word for it).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MountedVolume => "volume",
            Self::LocalFolder => "local",
            Self::ObjectStorage => "object",
        }
    }
}

/// One session to archive.
#[derive(Clone)]
pub struct SessionSource {
    /// The session folder.
    pub dir: PathBuf,
    /// Extra rsync-style exclude patterns, anchored at `dir` (rejected photos).
    pub excludes: Vec<String>,
    /// Session folders whose moves the archive mirrors (see
    /// [`ArchiveTarget::mirror_folders`]). Object storage can't rename without
    /// deleting, so there a moved file is simply archived at its new path too.
    pub mirror_folders: Vec<String>,
    /// Writes the consistent DB snapshot.
    pub snapshotter: Option<Arc<dyn Snapshotter>>,
}

impl std::fmt::Debug for SessionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSource")
            .field("dir", &self.dir)
            .field("excludes", &self.excludes.len())
            .field("mirror_folders", &self.mirror_folders)
            .field("snapshotter", &self.snapshotter.is_some())
            .finish()
    }
}

impl SessionSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            excludes: Vec::new(),
            mirror_folders: Vec::new(),
            snapshotter: None,
        }
    }
}

/// An archive destination.
pub trait Destination: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> DestinationKind;

    /// Where it is, for people and for the saved state: the folder path, or
    /// `s3://bucket/prefix` (with the endpoint's host when it isn't AWS).
    fn location(&self) -> String;

    /// Two runs with the same key never archive at the same time (see
    /// [`crate::scheduler::TargetLock`]).
    fn lock_key(&self) -> PathBuf;

    /// Can sessions be archived there right now? `Ok(detail)`: the file system type,
    /// or the storage service. [`ArchiveError::TargetUnavailable`] means "offline,
    /// try later"; [`ArchiveError::TargetNotUsable`] needs the user. Writes nothing.
    fn check(&self, cancel: &AtomicBool) -> Result<Option<String>, ArchiveError>;

    /// The session's own folder there, claimed with an owner marker (see
    /// [`ArchiveTarget::claim_dest`]).
    fn claim(
        &self,
        session_dir: &Path,
        owner: &str,
        recorded: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ClaimedDest, ArchiveError>;

    /// Archive `source` into `claimed`.
    fn archive(
        &self,
        source: &SessionSource,
        claimed: &ClaimedDest,
        now: SystemTime,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<ArchiveOutcome, ArchiveError>;

    /// How many of `originals` (under `source`) `claimed` verifiably lacks, after a
    /// failed run; `None` when that can't be told cheaply.
    fn missing(
        &self,
        _source: &Path,
        _claimed: &ClaimedDest,
        _originals: &[PathBuf],
    ) -> Option<u64> {
        None
    }
}

/// Network file systems: a folder on one is on a mounted volume wherever it is.
const NETWORK_FS: &[&str] = &["smbfs", "nfs", "afpfs", "webdav", "cifs", "ftp"];

/// rsync into a folder: the archive as it always was.
#[derive(Debug, Clone)]
pub struct FolderDestination {
    target: ArchiveTarget,
    rsync: Option<RsyncRunner>,
    /// Folders the target must not be inside (Darkslide's support folder).
    protected: Vec<PathBuf>,
}

impl FolderDestination {
    /// `rsync`: `None` when none was found ([`Destination::archive`] then fails with
    /// [`ArchiveError::RsyncNotFound`]; checks work without it).
    pub fn new(target: ArchiveTarget, rsync: Option<RsyncRunner>) -> Self {
        Self {
            target,
            rsync,
            protected: Vec::new(),
        }
    }

    /// The target must not be inside any of `folders`.
    #[must_use]
    pub fn protecting(mut self, folders: Vec<PathBuf>) -> Self {
        self.protected = folders;
        self
    }

    pub fn target(&self) -> &ArchiveTarget {
        &self.target
    }

    /// The target, or why it can't be one: inside a protected folder, or a local
    /// folder that doesn't exist (only a mounted volume's folder is ever created).
    fn validate(&self) -> Result<(), ArchiveError> {
        let not_usable = |reason: String| ArchiveError::TargetNotUsable {
            root: self.target.root.clone(),
            reason,
        };
        let canon = std::fs::canonicalize(&self.target.root).ok();
        for p in &self.protected {
            let pc = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if self.target.root.starts_with(p) || canon.as_ref().is_some_and(|c| c.starts_with(&pc))
            {
                return Err(not_usable(format!(
                    "it is inside Darkslide's own folder ({})",
                    p.display()
                )));
            }
        }
        if self.kind() == DestinationKind::LocalFolder
            && self.target.root.is_absolute()
            && !self.target.root.exists()
        {
            return Err(not_usable("the folder doesn't exist".into()));
        }
        Ok(())
    }
}

/// Is the target on a volume that must be mounted (rather than a local disk)?
fn mounted_volume(target: &ArchiveTarget) -> bool {
    target.mount_point.is_some()
        || target.volume.as_ref().is_some_and(|v| {
            v.mounted_on.starts_with("/Volumes/") || NETWORK_FS.contains(&v.fs_type.as_str())
        })
}

impl Destination for FolderDestination {
    fn kind(&self) -> DestinationKind {
        if mounted_volume(&self.target) {
            DestinationKind::MountedVolume
        } else {
            DestinationKind::LocalFolder
        }
    }

    fn location(&self) -> String {
        self.target.root.to_string_lossy().into_owned()
    }

    fn lock_key(&self) -> PathBuf {
        self.target.root.clone()
    }

    fn check(&self, _cancel: &AtomicBool) -> Result<Option<String>, ArchiveError> {
        self.validate()?;
        self.target.check_available()?;
        Ok(crate::target::fs_type(&self.target.root))
    }

    fn claim(
        &self,
        session_dir: &Path,
        owner: &str,
        recorded: Option<&str>,
        _cancel: &AtomicBool,
    ) -> Result<ClaimedDest, ArchiveError> {
        self.validate()?;
        self.target.claim_dest(session_dir, owner, recorded)
    }

    fn archive(
        &self,
        source: &SessionSource,
        claimed: &ClaimedDest,
        now: SystemTime,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        self.validate()?;
        if self.target.root.starts_with(&source.dir) {
            return Err(ArchiveError::TargetNotUsable {
                root: self.target.root.clone(),
                reason: "it is inside the session being archived".into(),
            });
        }
        let rsync = self.rsync.clone().ok_or(ArchiveError::RsyncNotFound)?;
        let mut target = self.target.clone();
        target.excludes.extend(source.excludes.iter().cloned());
        target.mirror_folders.clone_from(&source.mirror_folders);
        Archiver::new(rsync, source.snapshotter.clone()).archive_to(
            &source.dir,
            &target,
            &claimed.dest,
            now,
            progress,
            cancel,
        )
    }

    /// Originals the target doesn't hold with the same size (stats the target: call
    /// it on the run's worker thread).
    fn missing(&self, source: &Path, claimed: &ClaimedDest, originals: &[PathBuf]) -> Option<u64> {
        let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
        Some(
            originals
                .iter()
                .filter(|p| {
                    let Ok(rel) = p.strip_prefix(source) else {
                        return false;
                    };
                    let there = size(&claimed.dest.join(rel));
                    there.is_none() || there != size(p)
                })
                .count() as u64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::VolumeId;

    #[test]
    fn kinds_and_validation() {
        let t = tempfile::tempdir().unwrap();
        let none = AtomicBool::new(false);
        // A local folder must exist.
        let local = FolderDestination::new(ArchiveTarget::new(t.path().join("missing")), None);
        assert_eq!(local.kind(), DestinationKind::LocalFolder);
        assert_eq!(local.kind().as_str(), "local");
        let err = local.check(&none).unwrap_err();
        assert!(err.to_string().contains("doesn't exist"), "{err}");
        // Inside the support folder: refused (also through a symlink).
        let support = t.path().join("Support");
        std::fs::create_dir_all(support.join("inner")).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&support, &link).unwrap();
        for root in [support.join("inner"), link.join("inner")] {
            let d = FolderDestination::new(ArchiveTarget::new(&root), None)
                .protecting(vec![support.clone()]);
            let err = d.check(&none).unwrap_err();
            assert!(err.to_string().contains("Darkslide's own folder"), "{err}");
            assert!(d.claim(t.path(), "abc", None, &none).is_err());
        }
        // An existing local folder is fine; its location and lock key are the path.
        let ok = FolderDestination::new(ArchiveTarget::new(t.path()), None)
            .protecting(vec![support.clone()]);
        assert!(ok.check(&none).is_ok());
        assert_eq!(ok.location(), t.path().to_string_lossy());
        assert_eq!(ok.lock_key(), t.path());
        assert_eq!(ok.target().root, t.path());
        // /Volumes paths and network shares are mounted volumes.
        let vol = FolderDestination::new(ArchiveTarget::new("/Volumes/pics/Darkslide"), None);
        assert_eq!(vol.kind(), DestinationKind::MountedVolume);
        let mut share = ArchiveTarget::new(t.path().join("nas"));
        share.volume = Some(VolumeId {
            mounted_on: t.path().to_string_lossy().into_owned(),
            source: "//nas/pics".into(),
            fs_type: "smbfs".into(),
        });
        assert_eq!(
            FolderDestination::new(share, None).kind(),
            DestinationKind::MountedVolume
        );
        assert_eq!(DestinationKind::ObjectStorage.as_str(), "object");
        assert_eq!(DestinationKind::MountedVolume.as_str(), "volume");
    }

    #[test]
    fn archive_refuses_a_target_inside_the_session_and_needs_rsync() {
        let t = tempfile::tempdir().unwrap();
        let session = t.path().join("sess");
        std::fs::create_dir_all(session.join("Archive")).unwrap();
        let none = AtomicBool::new(false);
        let inside = FolderDestination::new(ArchiveTarget::new(session.join("Archive")), None);
        let claimed = ClaimedDest {
            dest: session.join("Archive/x"),
            folder: "x".into(),
            adopted_legacy: false,
        };
        let src = SessionSource::new(&session);
        assert!(format!("{src:?}").contains("sess"));
        let err = inside
            .archive(&src, &claimed, SystemTime::now(), &mut |_| {}, &none)
            .unwrap_err();
        assert!(err.to_string().contains("inside the session"), "{err}");
        let target = t.path().join("archive");
        std::fs::create_dir(&target).unwrap();
        let no_rsync = FolderDestination::new(ArchiveTarget::new(&target), None);
        assert!(matches!(
            no_rsync.archive(&src, &claimed, SystemTime::now(), &mut |_| {}, &none),
            Err(ArchiveError::RsyncNotFound)
        ));
        // What a failed run left out: compared by size.
        std::fs::write(session.join("a.ARW"), b"1234").unwrap();
        std::fs::write(session.join("b.ARW"), b"12").unwrap();
        let dest = target.join("sess");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("a.ARW"), b"1234").unwrap();
        std::fs::write(dest.join("b.ARW"), b"1").unwrap();
        let claimed = ClaimedDest {
            dest,
            folder: "sess".into(),
            adopted_legacy: false,
        };
        let originals = [
            session.join("a.ARW"),
            session.join("b.ARW"),
            session.join("c.ARW"),
            PathBuf::from("/elsewhere/d.ARW"),
        ];
        assert_eq!(no_rsync.missing(&session, &claimed, &originals), Some(2));
    }
}
