//! Find across 50,000 photos in 25 sessions (budget ≤ 300 ms per query):
//! structured (person + place + date), visual (person + text + a soft
//! place), text only, and the first query after a session changed (which
//! reloads the find rows).
#![allow(missing_docs)]

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "../tests/support/find_fixture.rs"]
mod fixture;

fn benches(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let mut g = fixture::build(dir.path());
    let (structured, visual, text) = (
        fixture::structured(),
        fixture::visual(),
        fixture::text_only(),
    );
    assert!(g.find(&structured).unwrap().total > 0);

    let mut grp = c.benchmark_group("find_50000");
    grp.measurement_time(Duration::from_secs(8));
    grp.bench_function("structured_warm", |b| {
        b.iter(|| g.find(&structured).unwrap());
    });
    grp.bench_function("visual_warm", |b| {
        b.iter(|| g.find(&visual).unwrap());
    });
    grp.bench_function("text_only_warm", |b| {
        b.iter(|| g.find(&text).unwrap());
    });
    grp.sample_size(10);
    grp.bench_function("first_after_a_change", |b| {
        let mut n = 0u8;
        let root = fixture::root(0);
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let mut records = fixture::records(0);
        b.iter(|| {
            n = n.wrapping_add(1);
            records[0].rating = n % 6;
            g.sync_find_items(&root, &name, &records).unwrap();
            g.find(&structured).unwrap()
        });
    });
    grp.finish();
}

criterion_group!(find, benches);
criterion_main!(find);
