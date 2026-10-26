//! `MemLru` hot-path costs: hit (get + promote), insert with eviction.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_cache::MemLru;

fn bench_lru(c: &mut Criterion) {
    // 2,000 cached previews of 1 KB each (weights are what matter, not bytes).
    let n = 2_000u64;
    let mut lru: MemLru<u64, Arc<Vec<u8>>> = MemLru::new(n as usize * 1024);
    let val = Arc::new(vec![0u8; 1024]);
    for k in 0..n {
        lru.insert(k, Arc::clone(&val));
    }
    let mut i = 0u64;
    c.bench_function("lru_get_hit_2000", |b| {
        b.iter(|| {
            i = (i + 7919) % n;
            black_box(lru.get(&i).is_some())
        });
    });
    let mut next = n;
    c.bench_function("lru_insert_evict_2000", |b| {
        b.iter(|| {
            next += 1;
            black_box(lru.insert(next, Arc::clone(&val)).len())
        });
    });
}

criterion_group!(benches, bench_lru);
criterion_main!(benches);
