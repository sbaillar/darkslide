//! Locate the real sample RAW files used by integration tests and benches.
//!
//! Samples live in the git-ignored `samples/` directory at the workspace root
//! (or `$DS_SAMPLES_DIR`). Every helper returns `None`/empty when they are
//! absent so tests can skip instead of failing on machines without them.

use std::path::{Path, PathBuf};

/// Directory holding sample RAWs, if present: `$DS_SAMPLES_DIR`, else the nearest
/// `samples/` above this crate. Walking up also finds the main checkout's samples from a
/// worktree nested inside it (`<repo>/.claude/worktrees/<name>/`), where the ignored
/// folder doesn't exist; real-sample tests used to skip silently there.
pub fn samples_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DS_SAMPLES_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    find_samples(Path::new(env!("CARGO_MANIFEST_DIR")))
}

fn find_samples(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|a| a.join("samples"))
        .find(|d| d.is_dir())
}

/// All sample files with the given extension (case-insensitive), sorted.
pub fn samples_with_ext(ext: &str) -> Vec<PathBuf> {
    let Some(dir) = samples_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)))
        .collect();
    out.sort();
    out
}

/// First Sony A9 ARW sample, if any.
pub fn sample_arw() -> Option<PathBuf> {
    samples_with_ext("arw").into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_found_from_nested_folders() {
        let t = tempfile_dir();
        let nested = t.join(".claude/worktrees/w/crates/x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(t.join("samples")).unwrap();
        assert_eq!(find_samples(&nested), Some(t.join("samples")));
        std::fs::remove_dir_all(&t).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let t = std::env::temp_dir().join(format!("ds-testdata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        t
    }

    #[test]
    fn helpers_are_consistent() {
        let all = samples_with_ext("ARW");
        assert_eq!(sample_arw(), all.first().cloned());
        if samples_dir().is_none() {
            assert_eq!(all, [] as [std::path::PathBuf; 0]);
        }
    }
}
