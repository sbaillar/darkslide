//! Walk a memory card (or any folder) and group files into import candidates.
//!
//! **Nothing but hidden entries is silently ignored.** Entries whose names
//! start with `.` (OS metadata: `._*`, `.Trashes`, `.Spotlight-V100`,
//! `.fseventsd`) are skipped without a report. Every other regular file is either
//! imported (RAW, JPEG/HEIF, video, and their sidecars) or listed in
//! [`ScanReport::skipped_unknown`] with a reason; every directory that could
//! not be read is in [`ScanReport::errors`], and an import is only complete
//! when that list is empty.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ImportError;

/// Directory depth limit. Cards are shallow (`DCIM/100MSDCF/x.ARW`,
/// `PRIVATE/M4ROOT/CLIP/C0001.MP4`); this only guards pathological trees.
const MAX_DEPTH: usize = 16;

const RAW_EXTS: &[&str] = &[
    "arw", "arq", "srf", "sr2", "cr2", "cr3", "crw", "nef", "nrw", "raf", "orf", "rw2", "rwl",
    "dng", "pef", "srw", "3fr", "fff", "iiq", "erf", "mos", "mrw", "kdc", "dcr", "x3f", "raw",
];
const VIDEO_EXTS: &[&str] = &["mp4", "mts", "m2ts", "mov", "mxf", "m4v", "avi"];
/// Camera bookkeeping that is safe to leave behind; listed, not imported.
const CAMERA_DB: &[&str] = &[
    "bin", "bdm", "cpi", "mpl", "bnp", "dat", "inp", "int", "idx", "ctg", "log", "sbd",
];

/// What a file is, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Raw,
    Jpeg,
    Heif,
    /// Video clip: imported as a non-editable item.
    Video,
    /// XMP metadata sidecar.
    Xmp,
    /// Video sidecar (Sony `C0001M01.XML`, `THM` thumbnails).
    VideoSidecar,
}

impl FileKind {
    /// Classify by extension (case-insensitive). `None` for everything else.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            e if RAW_EXTS.contains(&e) => Some(Self::Raw),
            e if VIDEO_EXTS.contains(&e) => Some(Self::Video),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "heic" | "heif" | "hif" => Some(Self::Heif),
            "xmp" => Some(Self::Xmp),
            "xml" | "thm" => Some(Self::VideoSidecar),
            _ => None,
        }
    }

    /// Photo or video that can stand on its own.
    pub fn is_primary(self) -> bool {
        matches!(self, Self::Raw | Self::Jpeg | Self::Heif | Self::Video)
    }

    /// Pairing priority when several files share a stem: RAW leads.
    fn rank(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Video => 1,
            Self::Jpeg => 2,
            Self::Heif => 3,
            Self::Xmp => 4,
            Self::VideoSidecar => 5,
        }
    }
}

/// One file on the source volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
    pub kind: FileKind,
}

/// A photo or clip to import: the primary file plus same-stem sidecars
/// that travel with it and share its new name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub primary: SourceFile,
    pub sidecars: Vec<SourceFile>,
}

impl Candidate {
    /// Primary first, then sidecars.
    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        std::iter::once(&self.primary).chain(&self.sidecars)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files().map(|f| f.size).sum()
    }

    /// The primary's file stem (`DSC01234`).
    pub fn stem(&self) -> &str {
        self.primary
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
    }
}

/// A file on the card that will not be imported, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub reason: String,
}

/// A part of the card that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub reason: String,
}

/// Result of [`scan_volume`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub candidates: Vec<Candidate>,
    /// Unreadable directories/files. Non-empty ⇒ the import can never be complete.
    pub errors: Vec<ScanError>,
    /// Files deliberately not imported (camera databases, unknown types,
    /// orphan sidecars, symlinks). The UI must show these and ask for an
    /// explicit acknowledgement before offering "eject".
    pub skipped_unknown: Vec<SkippedFile>,
}

impl ScanReport {
    /// Every part of the card was readable.
    pub fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Scan a volume for importable media.
///
/// Walks the whole volume (DCIM photos, `PRIVATE/M4ROOT/CLIP` and AVCHD
/// video, anything else), never following symlinks (so loops are
/// impossible) and skipping dot-entries (`._*`, `.Trashes`,
/// `.Spotlight-V100`, …). File contents are never opened: sizes and mtimes
/// come from `lstat`. An unreadable `root` is an error; anything unreadable
/// below it is reported in [`ScanReport::errors`]. Candidates are sorted by
/// (mtime, path), i.e. shooting order.
pub fn scan_volume(root: &Path) -> Result<ScanReport, ImportError> {
    fs::read_dir(root).map_err(|e| ImportError::io(root, e))?;
    let mut report = ScanReport::default();
    let mut files = Vec::new();
    walk(root, 0, &mut files, &mut report);
    report.candidates = group(files, &mut report.skipped_unknown);
    report.skipped_unknown.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(report)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<SourceFile>, report: &mut ScanReport) {
    if depth > MAX_DEPTH {
        report.errors.push(ScanError {
            path: dir.to_path_buf(),
            reason: format!("deeper than {MAX_DEPTH} levels; not scanned"),
        });
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            report.errors.push(ScanError {
                path: dir.to_path_buf(),
                reason: format!("unreadable directory: {err}"),
            });
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                report.errors.push(ScanError {
                    path: dir.to_path_buf(),
                    reason: format!("listing failed: {err}"),
                });
                continue;
            }
        };
        let name = entry.file_name();
        if name.as_encoded_bytes().first() == Some(&b'.') {
            continue; // OS metadata (._*, .Trashes, .Spotlight-V100, .fseventsd)
        }
        let path = entry.path();
        // `DirEntry::file_type`/`metadata` do not follow symlinks.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(err) => {
                report.errors.push(ScanError {
                    path,
                    reason: format!("stat failed: {err}"),
                });
                continue;
            }
        };
        if ft.is_dir() {
            walk(&path, depth + 1, out, report);
        } else if ft.is_symlink() {
            report.skipped_unknown.push(SkippedFile {
                path,
                reason: "symbolic link (not followed)".into(),
            });
        } else if ft.is_file() {
            let Some(kind) = FileKind::from_path(&path) else {
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                let reason = if CAMERA_DB.contains(&ext.as_str()) || is_camera_index(&path) {
                    "camera database/index file (not media)"
                } else {
                    "unknown file type"
                };
                report.skipped_unknown.push(SkippedFile {
                    path,
                    reason: reason.into(),
                });
                continue;
            };
            match entry.metadata() {
                Ok(meta) => out.push(SourceFile {
                    path,
                    size: meta.len(),
                    mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    kind,
                }),
                Err(err) => report.errors.push(ScanError {
                    path,
                    reason: format!("stat failed: {err}"),
                }),
            }
        } else {
            report.skipped_unknown.push(SkippedFile {
                path,
                reason: "not a regular file".into(),
            });
        }
    }
}

/// Sony/AVCHD index files that use a media-looking extension (`MEDIAPRO.XML`,
/// `STATUS.XML`, `CUEUP.XML`), and the `AVF_INFO` database directory.
fn is_camera_index(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(name.as_str(), "MEDIAPRO.XML" | "STATUS.XML" | "CUEUP.XML")
        || path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("AVF_INFO"))
}

/// Pair files sharing (directory, case-insensitive stem). Video sidecars pair
/// by prefix (`C0001M01.XML` belongs to `C0001.MP4`).
fn group(files: Vec<SourceFile>, skipped: &mut Vec<SkippedFile>) -> Vec<Candidate> {
    let mut by_stem: HashMap<(PathBuf, String), Vec<SourceFile>> = HashMap::new();
    let mut video_sidecars = Vec::new();
    for f in files {
        if f.kind == FileKind::VideoSidecar {
            if is_camera_index(&f.path) {
                skipped.push(SkippedFile {
                    path: f.path,
                    reason: "camera database/index file (not media)".into(),
                });
            } else {
                video_sidecars.push(f);
            }
            continue;
        }
        let dir = f.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = f
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        by_stem.entry((dir, stem)).or_default().push(f);
    }
    // Attach each video sidecar to the video whose stem prefixes its own.
    for f in video_sidecars {
        let dir = f.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = f
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let owner = by_stem
            .iter_mut()
            .filter(|((d, s), g)| {
                *d == dir
                    && stem.starts_with(s.as_str())
                    && g.iter().any(|x| x.kind == FileKind::Video)
            })
            .max_by_key(|((_, s), _)| s.len());
        match owner {
            Some((_, g)) => g.push(f),
            None => skipped.push(SkippedFile {
                path: f.path,
                reason: "sidecar without its media file".into(),
            }),
        }
    }
    let mut out = Vec::new();
    for (_, mut group) in by_stem {
        group.sort_by(|a, b| {
            a.kind
                .rank()
                .cmp(&b.kind.rank())
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut rest = group.into_iter();
        let Some(primary) = rest.next() else { continue };
        if !primary.kind.is_primary() {
            skipped.push(SkippedFile {
                path: primary.path,
                reason: "sidecar without its media file".into(),
            });
            for f in rest {
                skipped.push(SkippedFile {
                    path: f.path,
                    reason: "sidecar without its media file".into(),
                });
            }
            continue;
        }
        let mut sidecars = Vec::new();
        for f in rest {
            if matches!(f.kind, FileKind::Raw | FileKind::Video) {
                // Two RAWs (or clips) with one stem: each is its own item.
                out.push(Candidate {
                    primary: f,
                    sidecars: Vec::new(),
                });
            } else {
                sidecars.push(f);
            }
        }
        out.push(Candidate { primary, sidecars });
    }
    out.sort_by(|a, b| {
        a.primary
            .mtime
            .cmp(&b.primary.mtime)
            .then_with(|| a.primary.path.cmp(&b.primary.path))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn touch(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; len]).unwrap();
    }

    fn skipped_names(r: &ScanReport) -> Vec<String> {
        r.skipped_unknown
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn classifies_extensions() {
        assert_eq!(FileKind::from_path(Path::new("a.ARW")), Some(FileKind::Raw));
        assert_eq!(FileKind::from_path(Path::new("a.ARQ")), Some(FileKind::Raw));
        assert_eq!(FileKind::from_path(Path::new("a.3fr")), Some(FileKind::Raw));
        assert_eq!(FileKind::from_path(Path::new("a.IIQ")), Some(FileKind::Raw));
        assert_eq!(
            FileKind::from_path(Path::new("a.JPeG")),
            Some(FileKind::Jpeg)
        );
        assert_eq!(
            FileKind::from_path(Path::new("a.HIF")),
            Some(FileKind::Heif)
        );
        assert_eq!(FileKind::from_path(Path::new("a.xmp")), Some(FileKind::Xmp));
        assert_eq!(
            FileKind::from_path(Path::new("a.MP4")),
            Some(FileKind::Video)
        );
        assert_eq!(
            FileKind::from_path(Path::new("a.MTS")),
            Some(FileKind::Video)
        );
        assert_eq!(
            FileKind::from_path(Path::new("a.XML")),
            Some(FileKind::VideoSidecar)
        );
        assert_eq!(FileKind::from_path(Path::new("a.BIN")), None);
        assert_eq!(FileKind::from_path(Path::new("noext")), None);
        assert!(FileKind::Video.is_primary() && !FileKind::Xmp.is_primary());
    }

    #[test]
    fn walks_whole_card_imports_all_media_and_lists_the_rest() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        touch(&r.join("DCIM/100MSDCF/DSC00001.ARW"), 10);
        touch(&r.join("DCIM/100MSDCF/DSC00001.JPG"), 3);
        touch(&r.join("DCIM/100MSDCF/dsc00001.xmp"), 1);
        touch(&r.join("DCIM/100MSDCF/DSC00002.ARQ"), 11);
        touch(&r.join("DCIM/100MSDCF/._DSC00002.ARQ"), 4);
        touch(&r.join("DCIM/100MSDCF/DSC00003.JPG"), 5); // standalone JPEG
        touch(&r.join("DCIM/100MSDCF/ORPHAN.XMP"), 1);
        touch(&r.join("PRIVATE/M4ROOT/CLIP/C0001.MP4"), 9);
        touch(&r.join("PRIVATE/M4ROOT/CLIP/C0001M01.XML"), 2);
        touch(&r.join("PRIVATE/M4ROOT/CLIP/C0009M01.XML"), 2); // clip deleted in camera
        touch(&r.join("PRIVATE/M4ROOT/MEDIAPRO.XML"), 2);
        touch(&r.join("PRIVATE/M4ROOT/STATUS.XML"), 2);
        touch(&r.join("PRIVATE/AVCHD/BDMV/STREAM/00000.MTS"), 3);
        touch(&r.join("PRIVATE/AVCHD/BDMV/INDEX.BDM"), 3);
        touch(&r.join("AVF_INFO/AVIN0001.BNP"), 3);
        touch(&r.join("AVF_INFO/PRV00001.XML"), 3);
        touch(&r.join("notes.txt"), 1);
        touch(&r.join(".Trashes/DSC09999.ARW"), 1);
        touch(&r.join(".Spotlight-V100/x.ARW"), 1);

        let s = scan_volume(r).unwrap();
        assert!(s.is_complete(), "{:?}", s.errors);
        let prim: Vec<_> = s
            .candidates
            .iter()
            .map(|c| c.primary.path.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(prim.len(), 5, "{prim:?}");
        let one = s
            .candidates
            .iter()
            .find(|c| c.stem() == "DSC00001")
            .unwrap();
        assert_eq!(one.sidecars.len(), 2);
        assert_eq!(one.total_bytes(), 14);
        let clip = s.candidates.iter().find(|c| c.stem() == "C0001").unwrap();
        assert_eq!(clip.primary.kind, FileKind::Video);
        assert_eq!(clip.sidecars.len(), 1, "C0001M01.XML travels with its clip");
        assert!(
            s.candidates
                .iter()
                .any(|c| c.stem() == "00000" && c.primary.kind == FileKind::Video)
        );
        assert!(
            s.candidates
                .iter()
                .any(|c| c.primary.kind == FileKind::Raw && c.stem() == "DSC00002")
        );
        let skipped = skipped_names(&s);
        for n in [
            "ORPHAN.XMP",
            "C0009M01.XML",
            "MEDIAPRO.XML",
            "STATUS.XML",
            "INDEX.BDM",
            "AVIN0001.BNP",
            "PRV00001.XML",
            "notes.txt",
        ] {
            assert!(
                skipped.iter().any(|x| x == n),
                "{n} not listed in {skipped:?}"
            );
        }
        let reason = |n: &str| {
            s.skipped_unknown
                .iter()
                .find(|x| x.path.ends_with(n))
                .unwrap()
                .reason
                .clone()
        };
        assert!(reason("MEDIAPRO.XML").contains("camera database"));
        assert!(reason("notes.txt").contains("unknown"));
        assert!(reason("ORPHAN.XMP").contains("without its media"));
        assert!(
            !skipped
                .iter()
                .any(|x| x.starts_with("._") || x == "DSC09999.ARW")
        );
    }

    #[test]
    fn two_raws_with_one_stem_split() {
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("a/IMG.ARW"), 1);
        touch(&t.path().join("a/IMG.DNG"), 2);
        let c = scan_volume(t.path()).unwrap().candidates;
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|c| c.sidecars.is_empty()));
    }

    #[test]
    fn sorted_by_mtime_then_path() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("DCIM/B.ARW"), t.path().join("DCIM/A.ARW"));
        touch(&a, 1);
        touch(&b, 1);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let c = scan_volume(t.path()).unwrap().candidates;
        assert_eq!(c[0].primary.path, a);
        assert_eq!(c[0].primary.mtime, old);
    }

    #[test]
    fn symlinks_are_listed_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let dcim = t.path().join("DCIM");
        touch(&dcim.join("100/X.ARW"), 1);
        std::os::unix::fs::symlink(&dcim, dcim.join("100/loop")).unwrap();
        std::os::unix::fs::symlink(dcim.join("100/X.ARW"), dcim.join("100/Y.ARW")).unwrap();
        let s = scan_volume(t.path()).unwrap();
        assert_eq!(s.candidates.len(), 1);
        assert_eq!(s.skipped_unknown.len(), 2);
        assert!(
            s.skipped_unknown
                .iter()
                .all(|x| x.reason.contains("symbolic link"))
        );
    }

    #[test]
    fn missing_root_is_error_and_depth_limit_is_a_scan_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(scan_volume(&t.path().join("nope")).is_err());
        let mut deep = t.path().join("DCIM");
        for i in 0..(MAX_DEPTH + 3) {
            deep.push(format!("d{i}"));
        }
        touch(&deep.join("DEEP.ARW"), 1);
        touch(&t.path().join("DCIM/d0/SHALLOW.ARW"), 1);
        let s = scan_volume(t.path()).unwrap();
        assert_eq!(s.candidates.len(), 1);
        assert_eq!(s.candidates[0].stem(), "SHALLOW");
        assert!(!s.is_complete());
        assert!(s.errors[0].reason.contains("deeper"));
    }

    #[test]
    fn unreadable_subdir_is_a_scan_error() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("DCIM/ok/A.ARW"), 1);
        touch(&t.path().join("DCIM/locked/B.ARW"), 1);
        let locked = t.path().join("DCIM/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let s = scan_volume(t.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let s = s.unwrap();
        assert!(s.candidates.iter().any(|c| c.stem() == "A"));
        if !rustix::process::geteuid().is_root() {
            assert_eq!(s.errors.len(), 1);
            assert!(
                s.errors[0].path.ends_with("locked") && s.errors[0].reason.contains("unreadable")
            );
        }
    }

    #[test]
    fn fifo_is_listed() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("pipe");
        let st = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        let s = scan_volume(t.path()).unwrap();
        assert_eq!(s.skipped_unknown.len(), 1);
        assert!(s.skipped_unknown[0].reason.contains("not a regular file"));
    }
}
