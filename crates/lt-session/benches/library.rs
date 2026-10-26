//! The Library across 100,000 photos in 50 sessions (ARCHITECTURE §Budgets):
//! a cold open to the first screen (open the index, load the rows, the
//! default query, the first screen's items), filter and sort changes, and an
//! incremental sync of one changed 2,000-photo session (its records read
//! from a real session, one rating changed) with the first query after it.
#![allow(missing_docs)]

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::{FileHash, Rating};
use lt_session::{GlobalIndex, LibraryQuery, LibrarySource, Session, index_stamp};

#[path = "../tests/support/library_fixture.rs"]
mod fixture;

fn benches(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("global.db");
    drop(fixture::build(dir.path()));

    let mut grp = c.benchmark_group("library_100000");
    grp.sample_size(20);
    grp.measurement_time(Duration::from_secs(10));
    grp.bench_function("cold_open_first_screen", |b| {
        b.iter(|| {
            let mut g = GlobalIndex::open(&path).unwrap();
            let ids = g.library_query(&LibraryQuery::default()).unwrap();
            assert_eq!(ids.len(), fixture::N);
            g.library_items(&ids[..fixture::FIRST_SCREEN]).unwrap()
        });
    });
    let mut g = GlobalIndex::open(&path).unwrap();
    for (name, q) in fixture::queries() {
        g.library_query(&q).unwrap();
        grp.bench_function(name, |b| b.iter(|| g.library_query(&q).unwrap()));
    }
    grp.bench_function("page_of_200_items", |b| {
        let ids = g.library_query(&LibraryQuery::default()).unwrap();
        b.iter(|| g.library_items(&ids[50_000..50_200]).unwrap());
    });

    // One real 2,000-photo session among the 100,000.
    let sdir = dir.path().join("Changed");
    let mut s = Session::create(&sdir, "Changed").unwrap();
    let mut ids = Vec::new();
    for i in 0..fixture::PER {
        let rel = format!("Capture/IMG{i:05}.ARW");
        std::fs::create_dir_all(sdir.join("Capture")).unwrap();
        std::fs::write(sdir.join(&rel), rel.as_bytes()).unwrap();
        let h = FileHash(*blake3::hash(rel.as_bytes()).as_bytes());
        let id = s
            .add_original(&rel, h, lt_session::ImageMetaLite::default())
            .unwrap();
        ids.push(s.variants(id).unwrap()[0]);
    }
    let sync = |g: &mut GlobalIndex, s: &Session| {
        let known = g.library_known(s.root()).unwrap();
        let recs = s.library_records(&known).unwrap();
        let src = LibrarySource {
            store: s.root().to_owned(),
            external: None,
            stamp: index_stamp(s.root()).unwrap(),
        };
        g.sync_library(s.root(), s.name(), &src, &recs).unwrap()
    };
    sync(&mut g, &s);
    // A rating changes before each timed sync (untimed): one row differs.
    let mut n = 0u8;
    let mut change = |s: &mut Session| {
        n = (n + 1) % 6;
        s.set_rating(&ids[..1], Rating::new(n).unwrap()).unwrap();
    };
    grp.bench_function("sync_one_changed_session", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                change(&mut s);
                let t = std::time::Instant::now();
                assert!(sync(&mut g, &s));
                total += t.elapsed();
            }
            total
        });
    });
    grp.bench_function("first_query_after_a_sync", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                change(&mut s);
                assert!(sync(&mut g, &s));
                let t = std::time::Instant::now();
                g.library_query(&LibraryQuery::default()).unwrap();
                total += t.elapsed();
            }
            total
        });
    });
    grp.finish();
}

criterion_group!(library, benches);
criterion_main!(library);
