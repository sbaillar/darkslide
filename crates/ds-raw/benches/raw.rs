//! Budgets (ARCHITECTURE.md, release, file in page cache):
//! `read_metadata` ≤ 5 ms, `embedded_preview` ≤ 10 ms, `decode` ≤ 120 ms.
//! Skips (benchmarks nothing) when `samples/` is absent.
//!
//! With `DS_RAWCOMPAT_DIR` (see `scripts/fetch-rawcompat.sh`), also
//! `embedded_preview` for every camera there (`compat/<file>`).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

/// The rawler header path on a big non-TIFF file (RAF signature, sparse
/// 64 MiB): measures how much of the file `read_metadata` touches before
/// rawler gets going. The parse itself fails; only the read cost matters.
fn rawler_path(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.raf");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(64 << 20).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, b"FUJIFILMCCD-RAW ", 0).unwrap();
    let mut g = c.benchmark_group("rawler_path_64mib");
    g.bench_function("read_metadata", |b| {
        b.iter(|| black_box(ds_raw::read_metadata(&path).is_err()));
    });
    g.finish();
}

/// The ≤ 10 ms preview budget, per format.
fn compat(c: &mut Criterion) {
    let Some(dir) = std::env::var_os("DS_RAWCOMPAT_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| ds_raw::has_raw_extension(p))
        .collect();
    files.sort();
    let mut g = c.benchmark_group("compat");
    for p in files {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if ds_raw::embedded_preview(&p).is_err() {
            eprintln!("ds-raw benches: no preview from {name}, skipping");
            continue;
        }
        g.bench_function(name, |b| {
            b.iter(|| black_box(ds_raw::embedded_preview(&p).unwrap()));
        });
    }
    g.finish();
}

fn benches(c: &mut Criterion) {
    rawler_path(c);
    let Some(path) = ds_testdata::sample_arw() else {
        eprintln!("ds-raw benches: no samples/ directory, skipping");
        return;
    };
    let mut g = c.benchmark_group("a9_arw");
    g.bench_function("read_metadata", |b| {
        b.iter(|| black_box(ds_raw::read_metadata(&path).unwrap()));
    });
    g.bench_function("embedded_preview", |b| {
        b.iter(|| black_box(ds_raw::embedded_preview(&path).unwrap()));
    });
    g.sample_size(20);
    g.bench_function("decode", |b| {
        b.iter(|| black_box(ds_raw::decode(&path).unwrap()));
    });
    // Baseline for comparison: stock rawler full decode (no crop/convert).
    g.bench_function("rawler_decode_baseline", |b| {
        b.iter(|| black_box(rawler::decode_file(&path).unwrap()));
    });
    g.finish();
}

criterion_group!(raw, benches, compat);
criterion_main!(raw);
