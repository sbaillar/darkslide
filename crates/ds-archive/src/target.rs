//! Archive targets and the "is it safe to write there right now?" check.

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::ArchiveError;

/// Excludes added by [`ArchiveTarget::new`].
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // The live SQLite index and its side files: only the consistent snapshot
    // (`.ds/index-snapshot.db`) is archived.
    "*.dsession",
    "*.dsession-wal",
    "*.dsession-shm",
    "*.dsession-journal",
    // In-flight temp files (ds-fsutil temp names) and our lock files.
    ".ds-*.tmp",
    ".ds/*.lock",
    ".DS_Store",
];

/// Written into each session's archive folder: the archive id of the session
/// that owns it (see [`ArchiveTarget::claim_dest`]).
pub const ARCHIVE_OWNER_FILE: &str = ".ds-archive-owner";

/// A session's own folder on the target, from [`ArchiveTarget::claim_dest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedDest {
    pub dest: PathBuf,
    /// Folder name under the target root; record it in the session so a
    /// later rename of the session keeps archiving into the same folder.
    pub folder: String,
    /// An archive folder made before per-session folders (`<name>`) was
    /// taken over because it holds this session's files.
    pub adopted_legacy: bool,
}

/// The file system a path lives on, recorded when the user confirms an archive target
/// so a later run can tell "the share is mounted there" from "the share is gone and
/// the path now resolves to the boot disk": the mount point, what is
/// mounted there (a device or a share URL) and the file system type, of the path's
/// nearest existing ancestor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeId {
    pub mounted_on: String,
    pub source: String,
    pub fs_type: String,
}

impl VolumeId {
    /// The volume `path` (or its nearest existing ancestor) is on right now; `None`
    /// where the platform can't say.
    pub fn of(path: &Path) -> Option<Self> {
        let mut p = path;
        while std::fs::symlink_metadata(p).is_err() {
            p = p.parent()?;
        }
        let canon = std::fs::canonicalize(p).ok()?;
        statfs_id(&canon)
    }

    /// A string form for settings.
    pub fn to_token(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_token(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    /// The same volume as `recorded`: same mount point and file system type, and the
    /// same source for a share. A local disk's device node (`/dev/diskNsM`) is
    /// renumbered when disks are plugged in a different order, so it isn't compared.
    pub fn matches(&self, recorded: &VolumeId) -> bool {
        self.mounted_on == recorded.mounted_on
            && self.fs_type == recorded.fs_type
            && (self.source == recorded.source
                || (self.source.starts_with("/dev/") && recorded.source.starts_with("/dev/")))
    }
}

#[cfg(target_vendor = "apple")]
fn statfs_id(path: &Path) -> Option<VolumeId> {
    let st = rustix::fs::statfs(path).ok()?;
    let text = |b: &[i8]| -> String {
        let v: Vec<u8> = b
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&v).into_owned()
    };
    Some(VolumeId {
        mounted_on: text(&st.f_mntonname),
        source: text(&st.f_mntfromname),
        fs_type: text(&st.f_fstypename),
    })
}

#[cfg(not(target_vendor = "apple"))]
fn statfs_id(_: &Path) -> Option<VolumeId> {
    None
}

/// Where sessions are archived, e.g. `/Volumes/pics/Darkslide`. Each session
/// lands in its own claimed folder (`root/<name> (<id>)/`, see
/// [`Self::claim_dest`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveTarget {
    pub root: PathBuf,
    /// Extra rsync `--exclude` patterns (on top of `.ds/cache/` and `*.partial`).
    pub excludes: Vec<String>,
    /// If set, this directory must be a mount point (a different device from
    /// its parent). Guards against writing into a stale, empty `/Volumes/pics`
    /// directory on the boot disk when the NAS is not mounted.
    pub mount_point: Option<PathBuf>,
    /// rsync `--modify-window` (seconds) for this target. `None` = detect
    /// from the target file system (see [`mtime_window_for`]).
    pub modify_window: Option<u32>,
    /// Session folders whose moves the archive mirrors (a managed session's
    /// `Capture` and `Selects`): a file archived under one of them that the session
    /// now has only under another is moved there on the target (or, when the target
    /// has it there already, into the archive trash) before rsync, so the archive
    /// keeps the session's current layout without duplicates. Nothing is deleted.
    /// Empty (the default): nothing on the target is moved.
    pub mirror_folders: Vec<String>,
    /// The volume the user confirmed the target on ([`VolumeId::of`] then). When set,
    /// every check and create requires the target (or its nearest existing ancestor)
    /// to be on that volume now; otherwise the target is offline and nothing is
    /// created. When unset, a missing root is only ever created under a mounted
    /// `/Volumes/<name>` ([`Self::mount_point`]); elsewhere it must exist.
    pub volume: Option<VolumeId>,
}

impl ArchiveTarget {
    /// Target with [`DEFAULT_EXCLUDES`]; for paths under `/Volumes/<name>`,
    /// `/Volumes/<name>` is required to be a mount point.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mount_point = volume_of(&root);
        Self {
            root,
            excludes: DEFAULT_EXCLUDES.iter().map(|s| (*s).to_owned()).collect(),
            mount_point,
            modify_window: None,
            mirror_folders: Vec::new(),
            volume: None,
        }
    }

    /// Mtime tolerance for rsync's quick check against this target.
    pub fn modify_window(&self) -> u32 {
        self.modify_window
            .unwrap_or_else(|| mtime_window_for(&self.root))
    }

    /// Destination directory for a session folder.
    pub fn dest_for(&self, session_dir: &Path) -> Result<PathBuf, ArchiveError> {
        let name = session_dir
            .file_name()
            .ok_or_else(|| ArchiveError::InvalidSession(session_dir.to_path_buf()))?;
        Ok(self.root.join(name))
    }

    /// The session's own archive folder, claimed with an owner marker
    /// ([`ARCHIVE_OWNER_FILE`]) so two sessions with the same folder name
    /// never share (and overwrite) one archive folder.
    ///
    /// - `recorded`: the folder an earlier run stored in the session; used as
    ///   long as no other session claims it (so renames keep the folder).
    /// - A pre-existing `<name>` folder (archives made before per-session
    ///   folders) is adopted when its marker is `owner`, or when it has no
    ///   marker and holds this session's files (same relative path, size
    ///   and mtime for every sampled file present there).
    /// - Otherwise the folder is `<name> (<first 8 of owner>)`.
    ///
    /// Nothing on the target is ever moved or deleted; only the folder and
    /// its marker may be created. `owner` must be a plain token (the app uses
    /// a hex id).
    pub fn claim_dest(
        &self,
        session_dir: &Path,
        owner: &str,
        recorded: Option<&str>,
    ) -> Result<ClaimedDest, ArchiveError> {
        let base = session_dir
            .file_name()
            .ok_or_else(|| ArchiveError::InvalidSession(session_dir.to_path_buf()))?
            .to_string_lossy()
            .into_owned();
        if owner.is_empty() || !plain_component(owner) {
            return Err(ArchiveError::InvalidSession(session_dir.to_path_buf()));
        }
        self.ensure_root()?;
        let owner_of = |dir: &Path| {
            std::fs::read_to_string(dir.join(ARCHIVE_OWNER_FILE))
                .ok()
                .map(|s| s.trim().to_owned())
        };
        let claim = |folder: String, adopted_legacy: bool| -> Result<ClaimedDest, ArchiveError> {
            let dest = self.root.join(&folder);
            self.check_mounted()?;
            std::fs::create_dir_all(&dest).map_err(|e| ArchiveError::io(&dest, e))?;
            if owner_of(&dest).as_deref() != Some(owner) {
                let marker = dest.join(ARCHIVE_OWNER_FILE);
                ds_fsutil::write_atomic(&marker, owner.as_bytes())
                    .map_err(|e| ArchiveError::io(&marker, e))?;
            }
            Ok(ClaimedDest {
                dest,
                folder,
                adopted_legacy,
            })
        };
        if let Some(name) = recorded.filter(|n| plain_component(n)) {
            match owner_of(&self.root.join(name)) {
                Some(o) if o != owner => {} // someone else's now: claim a new one
                _ => return claim(name.to_owned(), false),
            }
        }
        let legacy = self.root.join(&base);
        if legacy.is_dir() {
            match owner_of(&legacy) {
                Some(o) if o == owner => return claim(base, false),
                None if holds_same_files(&legacy, session_dir) => return claim(base, true),
                _ => {}
            }
        }
        let short = &owner[..owner.len().min(8)];
        for folder in [format!("{base} ({short})"), format!("{base} ({owner})")] {
            match owner_of(&self.root.join(&folder)) {
                Some(o) if o != owner => {}
                _ => return claim(folder, false),
            }
        }
        Err(ArchiveError::io(
            &self.root.join(&base),
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "every archive folder name for this session is claimed by another session",
            ),
        ))
    }

    /// Ready to receive files: the volume (if configured) is mounted, and the root is a
    /// writable directory, or doesn't exist yet but can be created below its nearest
    /// existing ancestor on that volume (see [`Self::ensure_root`]).
    ///
    /// Errors: [`ArchiveError::TargetUnavailable`] only when the volume is not mounted
    /// ("offline"); [`ArchiveError::TargetNotUsable`] for permission problems, a file in
    /// the way, or a relative path. Nothing is created here.
    pub fn check_available(&self) -> Result<(), ArchiveError> {
        self.creatable_from().map(|_| ())
    }

    /// [`Self::check_available`], then create the root if it doesn't exist yet: one
    /// directory at a time, only below an existing, writable directory on the mounted
    /// volume. The mount point itself is never created.
    pub fn ensure_root(&self) -> Result<(), ArchiveError> {
        let Some(base) = self.creatable_from()? else {
            return Ok(());
        };
        let rest = self.root.strip_prefix(&base).unwrap_or(Path::new(""));
        let mut dir = base;
        for c in rest.components() {
            dir.push(c);
            // Re-checked right before each mkdir: an unmount in between must not create
            // folders on the boot disk.
            self.check_mounted()?;
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
                Err(e) => {
                    return Err(self.not_usable(format!(
                        "couldn't create {}: {}",
                        dir.display(),
                        plain_io(&e)
                    )));
                }
            }
        }
        Ok(())
    }

    /// The configured volume is still mounted (re-checked right before anything is
    /// created on the target, so an unmount never leaves folders on the boot disk).
    pub fn check_mounted(&self) -> Result<(), ArchiveError> {
        if let Some(mp) = &self.mount_point
            && !is_mount_point(mp)
        {
            return Err(self.unavailable(format!("{} is not mounted", mp.display())));
        }
        self.check_volume()
    }

    /// The target is on its recorded volume (if one is recorded).
    fn check_volume(&self) -> Result<(), ArchiveError> {
        let Some(want) = &self.volume else {
            return Ok(());
        };
        match VolumeId::of(&self.root) {
            Some(now) if now.matches(want) => Ok(()),
            Some(now) => Err(self.unavailable(format!(
                "the archive volume ({} on {}) is not mounted; the path is now on {}",
                want.source, want.mounted_on, now.mounted_on
            ))),
            None => Err(self.unavailable("the archive volume can't be identified".into())),
        }
    }

    fn unavailable(&self, reason: String) -> ArchiveError {
        ArchiveError::TargetUnavailable {
            root: self.root.clone(),
            reason,
        }
    }

    fn not_usable(&self, reason: String) -> ArchiveError {
        ArchiveError::TargetNotUsable {
            root: self.root.clone(),
            reason,
        }
    }

    /// `Ok(None)`: the root exists and is usable. `Ok(Some(dir))`: the root is missing
    /// and would be created below `dir` (existing, writable, on the mounted volume).
    fn creatable_from(&self) -> Result<Option<PathBuf>, ArchiveError> {
        if !self.root.is_absolute() {
            return Err(self.not_usable("the archive folder must be an absolute path".into()));
        }
        self.check_mounted()?;
        let writable = |dir: &Path| {
            rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).map_err(|e| {
                self.not_usable(format!("no permission to write to {}: {e}", dir.display()))
            })
        };
        match std::fs::metadata(&self.root) {
            Ok(meta) if meta.is_dir() => {
                writable(&self.root)?;
                return Ok(None);
            }
            Ok(_) => {
                return Err(self.not_usable("not a directory (a file is in the way)".into()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotADirectory | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Err(self.not_usable(plain_io(&e)));
            }
            // Anything else (I/O error, stale network mount): not reachable right now.
            Err(e) => return Err(self.unavailable(plain_io(&e))),
        }
        // The nearest existing ancestor: it must be a writable directory on the mounted
        // volume (never the mount point's parent).
        let mut base = self.root.as_path();
        while let Some(p) = base.parent() {
            base = p;
            if std::fs::symlink_metadata(base).is_ok() {
                break;
            }
        }
        if let Some(mp) = &self.mount_point
            && !base.starts_with(mp)
        {
            return Err(self.unavailable(format!("{} is not mounted", mp.display())));
        }
        // Without a recorded volume, a missing root outside /Volumes is never created:
        // a share mounted elsewhere that has gone away leaves an empty directory on the
        // boot disk that would take it.
        if self.mount_point.is_none() && self.volume.is_none() {
            return Err(self.unavailable(format!(
                "{} doesn't exist (its volume may not be mounted)",
                self.root.display()
            )));
        }
        if !base.is_dir() {
            return Err(self.not_usable(format!("{} is not a directory", base.display())));
        }
        writable(base)?;
        Ok(Some(base.to_path_buf()))
    }
}

/// An I/O error's message without the "(os error N)" suffix.
fn plain_io(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

/// Name of the file system holding `path` (`apfs`, `msdos`, `exfat`, `nfs`,
/// …), if the platform reports one.
pub fn fs_type(path: &Path) -> Option<String> {
    #[cfg(target_vendor = "apple")]
    {
        let st = rustix::fs::statfs(path).ok()?;
        let name: String = st
            .f_fstypename
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| char::from(*c as u8))
            .collect();
        Some(name)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = path;
        None
    }
}

/// `--modify-window` needed so unchanged files are not re-sent (and their
/// old copies not moved to the archive trash) on every run.
///
/// rsync compares whole seconds by default, which is right for APFS, HFS+,
/// NFS and exFAT (10 ms resolution keeps the seconds). FAT stores mtimes in
/// 2 s steps, so an odd-second source file never matches: allow 1 s there.
/// A negative window (nanoseconds) is never used: it makes every file look
/// changed on any target coarser than the source.
pub fn mtime_window_for(path: &Path) -> u32 {
    match fs_type(path).as_deref() {
        Some("msdos") => 1,
        _ => 0,
    }
}

/// One path component that is not `.`/`..` and holds no separator.
pub(crate) fn plain_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\0')
}

/// Does `archived` (an unclaimed archive folder) hold `session_dir`'s files?
/// Up to 5 files of the session (sorted, hidden entries skipped) are looked
/// up at the same relative path: at least one must be there, and every one
/// that is there must have the same size and mtime (±2 s for FAT targets).
fn holds_same_files(archived: &Path, session_dir: &Path) -> bool {
    let mut sample = Vec::new();
    let mut stack = vec![session_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries.into_iter().rev() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => sample.push(e.path()),
                _ => {}
            }
        }
        if sample.len() >= 5 {
            break;
        }
    }
    sample.sort();
    sample.truncate(5);
    let mut matched = 0;
    for f in &sample {
        let Ok(rel) = f.strip_prefix(session_dir) else {
            continue;
        };
        let (Ok(ours), Ok(theirs)) = (std::fs::metadata(f), std::fs::metadata(archived.join(rel)))
        else {
            continue;
        };
        if ours.len() != theirs.len() || (ours.mtime() - theirs.mtime()).abs() > 2 {
            return false;
        }
        matched += 1;
    }
    matched > 0
}

/// `/Volumes/<name>` for paths below it (`Volumes` compared case-insensitively, as
/// APFS does: `/volumes/pics/x` is on the `pics` volume too).
fn volume_of(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    match (comps.next(), comps.next(), comps.next()) {
        (Some(Component::RootDir), Some(Component::Normal(v)), Some(Component::Normal(name)))
            if v.eq_ignore_ascii_case("Volumes") =>
        {
            Some(Path::new("/Volumes").join(name))
        }
        _ => None,
    }
}

/// True when `dir` is a mounted volume's root: on macOS, the file system holding it
/// (`statfs`) is mounted exactly at its canonical path, which also accepts
/// `/Volumes/Macintosh HD` (a link to `/`); elsewhere, a different device from its
/// parent.
pub fn is_mount_point(dir: &Path) -> bool {
    let Ok(canon) = std::fs::canonicalize(dir) else {
        return false;
    };
    if !canon.is_dir() {
        return false;
    }
    #[cfg(target_vendor = "apple")]
    {
        let Ok(st) = rustix::fs::statfs(&canon) else {
            return false;
        };
        let on: Vec<u8> = st
            .f_mntonname
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        std::str::from_utf8(&on).is_ok_and(|on| Path::new(on) == canon)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let Some(parent) = canon.parent() else {
            return true;
        }; // "/" is always mounted
        match (std::fs::metadata(&canon), std::fs::metadata(parent)) {
            (Ok(d), Ok(p)) => d.dev() != p.dev(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_paths_require_a_mount() {
        let t = ArchiveTarget::new("/Volumes/pics/Darkslide");
        assert_eq!(t.mount_point, Some(PathBuf::from("/Volumes/pics")));
        for live in [
            "*.dsession",
            "*.dsession-wal",
            "*.dsession-shm",
            "*.dsession-journal",
            ".ds-*.tmp",
        ] {
            assert!(t.excludes.contains(&live.to_owned()), "{live}");
        }
        assert_eq!(
            ArchiveTarget::new("/volumes/pics/Darkslide").mount_point,
            Some(PathBuf::from("/Volumes/pics")),
            "APFS is case-insensitive"
        );
        assert_eq!(ArchiveTarget::new("/Users/x/Archive").mount_point, None);
        assert_eq!(ArchiveTarget::new("/Volumes").mount_point, None);
        assert_eq!(ArchiveTarget::new("relative/x").mount_point, None);
    }

    #[test]
    fn modify_window_detection_and_override() {
        let t = tempfile::tempdir().unwrap();
        let mut target = ArchiveTarget::new(t.path());
        if cfg!(target_vendor = "apple") {
            assert_eq!(fs_type(t.path()).as_deref(), Some("apfs"));
        }
        assert_eq!(target.modify_window(), 0);
        target.modify_window = Some(1);
        assert_eq!(target.modify_window(), 1);
        assert_eq!(fs_type(&t.path().join("missing")), None);
        assert_eq!(mtime_window_for(&t.path().join("missing")), 0);
    }

    #[test]
    fn mount_point_detection() {
        assert!(is_mount_point(Path::new("/")));
        // /dev is devfs on macOS and Linux.
        assert!(is_mount_point(Path::new("/dev")));
        let t = tempfile::tempdir().unwrap();
        assert!(
            !is_mount_point(t.path()),
            "a plain directory is not a mount"
        );
        assert!(!is_mount_point(&t.path().join("missing")));
        // A link to a mount point (like /Volumes/Macintosh HD -> /) is mounted.
        let link = t.path().join("boot");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(is_mount_point(&link));
        // A link to a plain directory is not.
        let plain = t.path().join("plain");
        std::os::unix::fs::symlink(t.path(), &plain).unwrap();
        assert!(!is_mount_point(&plain));
        // A stale mount point: the configured volume is a plain directory.
        let mut stale = ArchiveTarget::new(t.path().join("x"));
        stale.mount_point = Some(t.path().to_path_buf());
        assert!(matches!(
            stale.check_mounted(),
            Err(ArchiveError::TargetUnavailable { .. })
        ));
        assert!(ArchiveTarget::new(t.path()).check_mounted().is_ok());
    }

    #[test]
    fn availability_checks() {
        let t = tempfile::tempdir().unwrap();
        let ok = ArchiveTarget::new(t.path());
        ok.check_available().unwrap();
        ok.ensure_root().unwrap();

        let file = t.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let err = ArchiveTarget::new(&file).check_available().unwrap_err();
        assert!(matches!(err, ArchiveError::TargetNotUsable { .. }), "{err}");
        assert!(err.to_string().contains("not a directory"), "{err}");
        // A file where a parent folder should be.
        let err = ArchiveTarget::new(file.join("sub"))
            .ensure_root()
            .unwrap_err();
        assert!(matches!(err, ArchiveError::TargetNotUsable { .. }), "{err}");

        let err = ArchiveTarget::new("relative/x")
            .check_available()
            .unwrap_err();
        assert!(err.to_string().contains("absolute path"), "{err}");

        // A stale mount directory (plain dir) is refused even though it exists, and a
        // missing root below it is never created: "offline".
        let mut stale = ArchiveTarget::new(t.path().join("Darkslide"));
        stale.mount_point = Some(t.path().to_path_buf());
        for err in [
            stale.check_available().unwrap_err(),
            stale.ensure_root().unwrap_err(),
        ] {
            assert!(
                matches!(err, ArchiveError::TargetUnavailable { .. }),
                "{err}"
            );
            assert!(err.to_string().contains("not mounted"), "{err}");
        }
        assert!(!t.path().join("Darkslide").exists());
    }

    /// `/Volumes/pics/Darkslide` on a mounted, writable volume where the
    /// folder doesn't exist yet is available and created on the first run (every missing
    /// level, only below the existing directory).
    #[test]
    fn a_missing_root_on_a_mounted_volume_is_created() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Darkslide").join("Archive");
        // Outside /Volumes the volume is recorded when the user confirms the target.
        let mut target = ArchiveTarget::new(&root);
        target.volume = VolumeId::of(&root);
        target.check_available().unwrap();
        assert!(!root.exists(), "the check creates nothing");
        target.ensure_root().unwrap();
        assert!(root.is_dir());
        target.ensure_root().unwrap();
        // claim_dest creates it too (a scheduled run).
        let root2 = t.path().join("Other");
        let session = t.path().join("sess");
        std::fs::create_dir(&session).unwrap();
        let mut other = ArchiveTarget::new(&root2);
        other.volume = VolumeId::of(&root2);
        let claimed = other.claim_dest(&session, "abcdef0123", None).unwrap();
        assert!(claimed.dest.starts_with(&root2) && claimed.dest.is_dir());
        // The mount point is on another device than the tree below it only for real
        // mounts; an ancestor outside the mount point is refused as offline.
        let mut elsewhere = ArchiveTarget::new(t.path().join("x"));
        elsewhere.mount_point = Some(PathBuf::from("/dev"));
        let err = elsewhere.ensure_root().unwrap_err();
        assert!(
            matches!(err, ArchiveError::TargetUnavailable { .. }),
            "{err}"
        );
        assert!(!t.path().join("x").exists());
    }

    /// A target outside /Volumes (a share mounted at `~/NAS`, say) whose
    /// share has gone away now resolves to the boot disk. Recorded on the share's
    /// volume, it is offline and nothing is created; without a recorded volume, a
    /// missing root there is never created either. A target the user picked on the boot
    /// disk (recorded there) stays creatable.
    #[test]
    fn a_share_that_went_away_is_offline_and_never_recreated() {
        let t = tempfile::tempdir().unwrap();
        let nas = t.path().join("NAS");
        std::fs::create_dir(&nas).unwrap();
        let root = nas.join("Darkslide");
        let share = VolumeId {
            mounted_on: nas.to_string_lossy().into_owned(),
            source: "//me@nas.local/pics".into(),
            fs_type: "smbfs".into(),
        };
        let mut gone = ArchiveTarget::new(&root);
        gone.volume = Some(share.clone());
        for err in [
            gone.check_available().unwrap_err(),
            gone.ensure_root().unwrap_err(),
        ] {
            assert!(
                matches!(err, ArchiveError::TargetUnavailable { .. }),
                "{err}"
            );
            assert!(err.to_string().contains("is not mounted"), "{err}");
        }
        let session = t.path().join("sess");
        std::fs::create_dir(&session).unwrap();
        assert!(gone.claim_dest(&session, "abcdef0123", None).is_err());
        // An existing folder left on the boot disk is not written into either.
        std::fs::create_dir(&root).unwrap();
        assert!(matches!(
            gone.check_available(),
            Err(ArchiveError::TargetUnavailable { .. })
        ));
        std::fs::remove_dir(&root).unwrap();
        // Unrecorded: offline, not created.
        let unrecorded = ArchiveTarget::new(&root);
        assert!(matches!(
            unrecorded.ensure_root(),
            Err(ArchiveError::TargetUnavailable { .. })
        ));
        assert!(!root.exists());
        // Deliberately on the boot disk: recorded there, so it is created.
        let mut local = ArchiveTarget::new(&root);
        local.volume = VolumeId::of(&root);
        local.ensure_root().unwrap();
        assert!(root.is_dir());
        // A local disk renumbered by the OS is still the same volume; a different share
        // at the same mount point is not.
        let disk = |source: &str| VolumeId {
            mounted_on: "/Volumes/Pics".into(),
            source: source.into(),
            fs_type: "apfs".into(),
        };
        assert!(disk("/dev/disk5s1").matches(&disk("/dev/disk4s1")));
        let other_share = VolumeId {
            source: "//me@nas.local/other".into(),
            ..share.clone()
        };
        assert!(!other_share.matches(&share));
        // Tokens round-trip for settings.
        assert_eq!(VolumeId::from_token(&share.to_token()), Some(share));
        assert_eq!(VolumeId::from_token("nonsense"), None);
    }

    /// The same inside /Volumes: a recorded volume that isn't the one mounted at
    /// `/Volumes/<name>` now (another disk with the same name) is offline too.
    #[test]
    fn a_different_volume_under_the_same_volumes_name_is_offline() {
        let mut t = ArchiveTarget::new("/Volumes/Macintosh HD/Users/Shared/DarkslideArchive");
        assert!(t.mount_point.is_some());
        t.volume = Some(VolumeId {
            mounted_on: "/Volumes/Macintosh HD".into(),
            source: "//me@nas.local/pics".into(),
            fs_type: "smbfs".into(),
        });
        let err = t.check_mounted().unwrap_err();
        assert!(
            matches!(err, ArchiveError::TargetUnavailable { .. }),
            "{err}"
        );
        // The real volume there is identified.
        let now = VolumeId::of(Path::new("/Volumes/Macintosh HD")).unwrap();
        assert!(!now.source.is_empty() && !now.fs_type.is_empty());
    }

    #[test]
    fn unwritable_root_is_a_permission_problem_not_offline() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let ro = t.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let existing = ArchiveTarget::new(&ro).check_available();
        let mut below = ArchiveTarget::new(ro.join("Darkslide"));
        below.volume = VolumeId::of(&ro);
        let below = below.ensure_root();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        if rustix::process::geteuid().is_root() {
            return; // root can write anywhere
        }
        for err in [existing.unwrap_err(), below.unwrap_err()] {
            assert!(matches!(err, ArchiveError::TargetNotUsable { .. }), "{err}");
            assert!(err.to_string().contains("no permission to write"), "{err}");
        }
        assert!(!ro.join("Darkslide").exists());
        let e = std::io::Error::from_raw_os_error(13);
        assert!(!plain_io(&e).contains("os error"));
        assert_eq!(plain_io(&std::io::Error::other("x")), "x");
    }

    fn set_mtime(p: &Path, secs: i64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.unsigned_abs());
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn session_with(dir: &Path, files: &[(&str, &[u8])]) {
        for (rel, bytes) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            set_mtime(&p, 1_790_000_000);
        }
    }

    /// Two sessions with the same folder name get their own archive
    /// folders; a recorded folder is kept; markers decide ownership.
    #[test]
    fn claim_dest_gives_each_session_its_own_folder() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        std::fs::create_dir(&root).unwrap();
        let target = ArchiveTarget::new(&root);
        let (a, b) = (t.path().join("p1/Trip"), t.path().join("p2/Trip"));
        session_with(&a, &[("Capture/S0.ARW", b"aaaa")]);
        session_with(&b, &[("Capture/S0.ARW", b"different")]);
        let ca = target.claim_dest(&a, "aaaaaaaa1111", None).unwrap();
        let cb = target.claim_dest(&b, "bbbbbbbb2222", None).unwrap();
        assert_eq!(ca.folder, "Trip (aaaaaaaa)");
        assert_eq!(cb.folder, "Trip (bbbbbbbb)");
        assert_ne!(ca.dest, cb.dest);
        assert_eq!(
            std::fs::read_to_string(ca.dest.join(ARCHIVE_OWNER_FILE)).unwrap(),
            "aaaaaaaa1111"
        );
        // The session was renamed: its recorded folder is kept.
        let renamed = t.path().join("p1/Trip renamed");
        std::fs::rename(&a, &renamed).unwrap();
        let again = target
            .claim_dest(&renamed, "aaaaaaaa1111", Some(&ca.folder))
            .unwrap();
        assert_eq!(again.folder, ca.folder);
        // A recorded folder that another session owns is not taken over.
        let stolen = target
            .claim_dest(&renamed, "cccccccc3333", Some(&ca.folder))
            .unwrap();
        assert_eq!(stolen.folder, "Trip renamed (cccccccc)");
        // Same 8-character prefix: the full id is used instead.
        let clash = target.claim_dest(&b, "bbbbbbbb9999", None).unwrap();
        assert_eq!(clash.folder, "Trip (bbbbbbbb9999)");
        // Bad inputs.
        assert!(target.claim_dest(Path::new("/"), "x", None).is_err());
        assert!(target.claim_dest(&b, "", None).is_err());
        assert!(target.claim_dest(&b, "../x", None).is_err());
        // A recorded name that isn't a plain component is ignored.
        let c = target
            .claim_dest(&b, "bbbbbbbb2222", Some("../evil"))
            .unwrap();
        assert_eq!(c.folder, "Trip (bbbbbbbb)");
    }

    /// Migration: an archive folder made before per-session folders is
    /// adopted only when it holds this session's files; nothing is deleted.
    #[test]
    fn claim_dest_adopts_a_matching_legacy_folder_only() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let target = ArchiveTarget::new(&root);
        let a = t.path().join("p1/Trip");
        session_with(
            &a,
            &[("Capture/S0.ARW", b"aaaa"), ("Capture/S1.ARW", b"bb")],
        );
        // The legacy archive of this session (rsync keeps mtimes).
        session_with(
            &root.join("Trip"),
            &[("Capture/S0.ARW", b"aaaa"), ("old.txt", b"x")],
        );
        let c = target.claim_dest(&a, "aaaaaaaa1111", None).unwrap();
        assert_eq!((c.folder.as_str(), c.adopted_legacy), ("Trip", true));
        assert!(root.join("Trip/old.txt").exists(), "nothing deleted");
        // From now on the marker says whose it is.
        let again = target.claim_dest(&a, "aaaaaaaa1111", None).unwrap();
        assert_eq!(
            (again.folder.as_str(), again.adopted_legacy),
            ("Trip", false)
        );
        // Another session with the same name but other files: its own folder.
        let b = t.path().join("p2/Trip");
        session_with(&b, &[("Capture/S0.ARW", b"different")]);
        let cb = target.claim_dest(&b, "bbbbbbbb2222", None).unwrap();
        assert_eq!(cb.folder, "Trip (bbbbbbbb)");
        // An unclaimed legacy folder holding other files is not adopted.
        let root2 = t.path().join("archive2");
        session_with(&root2.join("Trip"), &[("Capture/S0.ARW", b"zzz")]);
        let t2 = ArchiveTarget::new(&root2);
        assert_eq!(
            t2.claim_dest(&a, "aaaaaaaa1111", None).unwrap().folder,
            "Trip (aaaaaaaa)"
        );
        // …nor one that shares no file with the session.
        let root3 = t.path().join("archive3");
        session_with(&root3.join("Trip"), &[("other.ARW", b"q")]);
        let t3 = ArchiveTarget::new(&root3);
        assert_eq!(
            t3.claim_dest(&a, "aaaaaaaa1111", None).unwrap().folder,
            "Trip (aaaaaaaa)"
        );
        // A claim into an unwritable root is an error, not a panic.
        let file = t.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            ArchiveTarget::new(&file)
                .claim_dest(&a, "aaaaaaaa1111", None)
                .is_err()
        );
    }

    #[test]
    fn dest_for_uses_session_folder_name() {
        let t = ArchiveTarget::new("/Volumes/pics/Darkslide");
        assert_eq!(
            t.dest_for(Path::new("/Users/me/Pictures/2026-09-25 Hike"))
                .unwrap(),
            PathBuf::from("/Volumes/pics/Darkslide/2026-09-25 Hike")
        );
        assert!(matches!(
            t.dest_for(Path::new("/")),
            Err(ArchiveError::InvalidSession(_))
        ));
    }
}
