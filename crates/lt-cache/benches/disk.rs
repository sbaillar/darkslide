//! `DiskCache::get` hit cost (a 60 KB thumbnail) versus a plain file read.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_cache::{CacheKey, DiskCache, Kind, adjustments_hash};
use lt_core::{Adjustments, FileHash};

fn bench_disk(c: &mut Criterion) {
    let d = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(d.path(), 1 << 30).unwrap();
    let adj = adjustments_hash(&Adjustments::default());
    let key = |v: u64| CacheKey::new(v % 2000, FileHash([1; 32]), adj, Kind::Thumb);
    let jpeg = vec![0u8; 60_000];
    for v in 0..2000 {
        cache.put(&key(v), &jpeg).unwrap();
    }
    let mut i = 0u64;
    c.bench_function("disk_file_read_60k", |b| {
        b.iter(|| {
            i += 7;
            black_box(std::fs::read(cache.path_for(&key(i))).unwrap())
        });
    });
    c.bench_function("disk_get_hit_60k", |b| {
        b.iter(|| {
            i += 7;
            black_box(cache.get(&key(i)).unwrap())
        });
    });
}

criterion_group!(benches, bench_disk);
criterion_main!(benches);
