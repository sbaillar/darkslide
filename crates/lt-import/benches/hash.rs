//! Is import hashing I/O-bound? BLAKE3 as the import runs it (one
//! `blake3::Hasher` per file on a helper thread, fed 4 MiB chunks; three
//! files at once with `concurrency: 3`) against sequential 4 MiB reads of
//! the same bytes from the internal SSD with the page cache bypassed
//! (`F_NOCACHE`, as the verify pass reads).
//!
//! Data: six real A9 ARWs from `samples/` when present, else 6 × 48 MiB of
//! synthetic bytes (6 files, so three hashers get two each).

use std::fs::{self, File};
use std::hint::black_box;
use std::io::{Read, Write};
use std::path::Path;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const CHUNK: usize = 4 << 20;

fn source() -> Vec<Vec<u8>> {
    let samples = lt_testdata::samples_with_ext("arw");
    if samples.is_empty() {
        (0..6u8)
            .map(|i| {
                (0..48usize << 20)
                    .map(|j| (j as u8).wrapping_mul(37) ^ i)
                    .collect()
            })
            .collect()
    } else {
        samples
            .iter()
            .take(6)
            .map(|p| fs::read(p).unwrap())
            .collect()
    }
}

fn hash(data: &[u8]) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    for c in data.chunks(CHUNK) {
        h.update(c);
    }
    h.finalize()
}

/// Write `data` to `path` without leaving it in the page cache.
fn write_uncached(path: &Path, data: &[u8]) {
    let mut f = File::create(path).unwrap();
    lt_fsutil::set_nocache(&f);
    f.write_all(data).unwrap();
    f.sync_all().unwrap();
}

fn read_uncached(path: &Path, buf: &mut [u8]) -> usize {
    let mut f = File::open(path).unwrap();
    lt_fsutil::set_nocache(&f);
    let mut total = 0;
    loop {
        let n = f.read(buf).unwrap();
        if n == 0 {
            return total;
        }
        total += n;
    }
}

fn bench(c: &mut Criterion) {
    let files = source();
    let bytes: u64 = files.iter().map(|f| f.len() as u64).sum();
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let p = dir.path().join(format!("f{i}.arw"));
            write_uncached(&p, f);
            p
        })
        .collect();

    let mut g = c.benchmark_group("import_hash_vs_ssd");
    g.sample_size(10);
    g.throughput(Throughput::Bytes(bytes));
    g.bench_function("blake3_1_thread", |b| {
        b.iter(|| files.iter().map(|f| hash(black_box(f))).collect::<Vec<_>>());
    });
    g.bench_function("blake3_3_files_at_once", |b| {
        b.iter(|| {
            std::thread::scope(|s| {
                for part in files.chunks(files.len().div_ceil(3)) {
                    s.spawn(move || part.iter().map(|f| hash(black_box(f))).collect::<Vec<_>>());
                }
            });
        });
    });
    g.bench_function("ssd_read_uncached_4MiB", |b| {
        let mut buf = vec![0u8; CHUNK];
        b.iter(|| {
            paths
                .iter()
                .map(|p| read_uncached(p, &mut buf))
                .sum::<usize>()
        });
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
