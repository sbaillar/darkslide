//! Natural-language search over 20,000 photos (budget ≤ 100 ms per query):
//! a session query (warm, with a filter, and the first one after a write, which
//! reloads the vectors) and a global query across four sessions.
#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::embedding::RankOptions;
use lt_core::geom::Orientation;
use lt_core::{FileHash, Rating};
use lt_session::{Filter, GlobalIndex, ImageMetaLite, Session, Sort};

const N: usize = 20_000;
const DIM: usize = 512;
const MODEL: &str = "mobileclip-s2";

/// Deterministic pseudo-random vector (xorshift).
fn vector(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..DIM)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 2001) as f32 / 1000.0 - 1.0
        })
        .collect()
}

fn key(i: usize) -> FileHash {
    FileHash(*blake3::hash(&i.to_le_bytes()).as_bytes())
}

fn build(root: &Path) -> Session {
    let mut s = Session::create(root, "search").unwrap();
    let mut items = Vec::with_capacity(N);
    for i in 0..N {
        let rel = format!("Capture/DSC{i:05}.ARW");
        fs::write(root.join(&rel), i.to_le_bytes()).unwrap();
        let meta = ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + i as i64 * 250),
            width: 6000,
            height: 4000,
            orientation: Orientation::Normal,
            ..ImageMetaLite::default()
        };
        items.push((rel, key(i), meta));
    }
    s.add_originals(items).unwrap();
    let vs = s.query(&Filter::default(), Sort::default()).unwrap();
    for (k, chunk) in vs.chunks(2000).enumerate() {
        s.set_rating(chunk, Rating::new(k as u8 % 6).unwrap())
            .unwrap();
    }
    let vectors: Vec<(FileHash, Vec<f32>)> = (0..N).map(|i| (key(i), vector(i as u64))).collect();
    s.store_embeddings(MODEL, &vectors).unwrap();
    s
}

fn benches(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = build(&dir.path().join("search"));
    let q = vector(424_242);
    let opts = RankOptions {
        limit: 1000,
        min_score: 0.0,
        window: 1.0,
    };
    let rated = Filter {
        min_rating: Rating::new(3),
        ..Filter::default()
    };
    // Warm the cache once.
    assert_ne!(
        s.search_embeddings(MODEL, &q, &Filter::default(), opts)
            .unwrap(),
        [] as [lt_session::SearchHit; 0]
    );

    let mut g = c.benchmark_group("search_20000");
    g.measurement_time(Duration::from_secs(8));
    g.bench_function("session_warm", |b| {
        b.iter(|| {
            s.search_embeddings(MODEL, &q, &Filter::default(), opts)
                .unwrap()
        });
    });
    g.bench_function("session_warm_rating_ge3", |b| {
        b.iter(|| s.search_embeddings(MODEL, &q, &rated, opts).unwrap());
    });
    g.bench_function("session_after_write", |b| {
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            s.store_embeddings(MODEL, &[(key(0), vector(i))]).unwrap();
            s.search_embeddings(MODEL, &q, &Filter::default(), opts)
                .unwrap()
        });
    });

    // Global: four sessions of 5,000 photos each.
    let mut gi = GlobalIndex::open(&dir.path().join("global.db")).unwrap();
    for sidx in 0..4 {
        let root = PathBuf::from(format!("/Volumes/pics/Session {sidx}"));
        let items: Vec<(PathBuf, FileHash)> = (sidx * 5000..(sidx + 1) * 5000)
            .map(|i| (root.join(format!("DSC{i:05}.ARW")), key(i)))
            .collect();
        gi.sync_search_items(&root, &format!("Session {sidx}"), &items)
            .unwrap();
    }
    let vectors: Vec<(FileHash, Vec<f32>)> = (0..N).map(|i| (key(i), vector(i as u64))).collect();
    gi.store_embeddings(MODEL, &vectors).unwrap();
    assert_ne!(
        gi.search(MODEL, &q, opts).unwrap(),
        [] as [lt_session::GlobalSearchHit; 0]
    );
    g.bench_function("global_warm", |b| {
        b.iter(|| gi.search(MODEL, &q, opts).unwrap());
    });
    g.finish();
}

criterion_group!(search, benches);
criterion_main!(search);
