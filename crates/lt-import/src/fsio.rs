//! Import-specific file helpers. Generic primitives live in `lt-fsutil`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Open a source file strictly read-only (`O_RDONLY | O_NOFOLLOW`). This is
/// the only way the importer opens anything on the card, so the source can
/// never be written, truncated or have its times changed.
pub fn open_source(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// In-progress name for a copy: `.<16 hex of BLAKE3(final name)>.partial` in
/// the same directory. Short (a 255-byte final name still works), hidden,
/// and deterministic so a later run can clean up after a crash.
pub fn partial_path(final_path: &Path) -> PathBuf {
    let name = final_path.file_name().unwrap_or_default();
    let h = blake3::hash(name.as_encoded_bytes()).to_hex();
    final_path.with_file_name(format!(".{}.partial", &h[..16]))
}

/// Remove `p` if it is an import partial (`.<16 hex>.partial`) that nothing
/// has written to for [`lt_fsutil::STALE_TEMP_AGE`]: a crash left it and its
/// journal is gone or was set aside. A partial is never a finished file (the
/// card still holds the original), so clearing it loses nothing. Anything
/// else, including a partial a live import is still writing, is kept.
pub fn remove_stale_partial(p: &Path) -> bool {
    let is_partial = p
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix('.')?.strip_suffix(".partial"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    let old = fs::symlink_metadata(p)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= lt_fsutil::STALE_TEMP_AGE);
    is_partial && old && fs::remove_file(p).is_ok()
}

/// Create the partial `p` (`create_new`). A crash can leave a partial no
/// journal knows about: a [stale](remove_stale_partial) one is cleared and the
/// create retried instead of blocking the name; a fresh one is `AlreadyExists`.
pub fn create_partial(p: &Path) -> io::Result<File> {
    let open = || OpenOptions::new().write(true).create_new(true).open(p);
    open().or_else(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists && remove_stale_partial(p) {
            open()
        } else {
            Err(e)
        }
    })
}

/// Move a file we own but that failed verification (a torn copy from a crash)
/// to `<root>/.lt/quarantine/<stamp>/<rel>`: never deleted, never in the way.
pub fn quarantine(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let dest = root.join(".lt/quarantine").join(stamp).join(rel);
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    lt_fsutil::rename_noreplace(&root.join(rel), &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn source_open_is_read_only_and_refuses_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a");
        fs::write(&p, b"x").unwrap();
        let mut f = open_source(&p).unwrap();
        assert!(
            f.write_all(b"y").is_err(),
            "source handle must not be writable"
        );
        let link = t.path().join("l");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(open_source(&link).is_err());
    }

    #[test]
    fn partial_names_are_short_stable_and_distinct() {
        let a = partial_path(Path::new("/a/B.ARW"));
        assert_eq!(a, partial_path(Path::new("/a/B.ARW")));
        assert_ne!(a, partial_path(Path::new("/a/B.JPG")));
        assert_eq!(a.parent(), Some(Path::new("/a")));
        let long = partial_path(&Path::new("/a").join("x".repeat(255)));
        assert!(long.file_name().unwrap().len() < 30);
        assert!(long.to_string_lossy().ends_with(".partial"));
    }

    #[test]
    fn quarantine_moves_never_deletes() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("sub")).unwrap();
        fs::write(t.path().join("sub/A.ARW"), b"torn").unwrap();
        let q = quarantine(t.path(), Path::new("sub/A.ARW")).unwrap();
        assert!(!t.path().join("sub/A.ARW").exists());
        assert_eq!(fs::read(&q).unwrap(), b"torn");
        assert!(q.starts_with(t.path().join(".lt/quarantine")));
        assert!(
            quarantine(t.path(), Path::new("sub/A.ARW")).is_err(),
            "nothing to move"
        );
    }
}
