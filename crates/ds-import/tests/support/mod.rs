//! Shared helpers for ds-import integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ds_import::{ImportPlan, ImportProgress, NoMeta, PlanOptions, scan_volume};

pub fn bytes(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A card with `n` RAW+JPG pairs, mtimes two seconds apart (FAT resolution).
pub fn make_card(root: &Path, n: usize, size: usize) {
    let dir = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000 + 2 * i as u64);
        for (ext, len) in [("ARW", size), ("JPG", size / 4 + 1)] {
            let p = dir.join(format!("DSC{i:05}.{ext}"));
            fs::write(&p, bytes(i as u8, len)).unwrap();
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(base)
                .unwrap();
        }
    }
}

/// Every file under `root` (recursive), relative, sorted.
pub fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    out.sort();
    out
}

pub fn partials_under(root: &Path) -> Vec<PathBuf> {
    files_under(root)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".partial"))
        .collect()
}

pub fn plan_for(card: &Path, dest: &Path) -> ImportPlan {
    ImportPlan::new(
        &scan_volume(card).unwrap().candidates,
        PlanOptions::new(dest),
        &NoMeta,
    )
    .unwrap()
}

pub fn no_progress(_: &ImportProgress) {}

/// Assert every planned destination equals its card file byte-for-byte.
pub fn assert_all_imported(plan: &ImportPlan, root: &Path) {
    for f in plan.items.iter().flat_map(|i| &i.files) {
        assert_eq!(
            fs::read(root.join(&f.rel)).unwrap(),
            fs::read(&f.src.path).unwrap(),
            "{} differs from its source",
            f.rel.display()
        );
    }
}
