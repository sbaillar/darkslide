//! Session hot paths: filtering a 2,000-image session (budget < 5 ms), batch
//! metadata writes, a single slider save, and opening.
#![allow(missing_docs)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::geom::Orientation;
use lt_core::{Adjustments, ColorTag, FileHash, Rating};
use lt_session::{Filter, ImageMetaLite, Session, Sort, SortKey, VariantId};

fn build(root: &Path, n: usize) -> (Session, Vec<VariantId>) {
    let mut s = Session::create(root, "bench").unwrap();
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let rel = format!("Capture/DSC{i:05}.ARW");
        fs::write(root.join(&rel), i.to_le_bytes()).unwrap();
        let meta = ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + i as i64 * 250),
            camera: Some("ILCE-9".into()),
            lens: Some(
                if i % 2 == 0 {
                    "FE 24-70mm F2.8 GM"
                } else {
                    "FE 100-400mm GM"
                }
                .into(),
            ),
            iso: Some(100 << (i % 7)),
            focal_length_mm: Some(50.0),
            width: 6000,
            height: 4000,
            orientation: Orientation::Normal,
            as_shot_wb: None,
        };
        items.push((
            rel,
            FileHash(*blake3::hash(&i.to_le_bytes()).as_bytes()),
            meta,
        ));
    }
    s.add_originals(items).unwrap();
    let vs = s.query(&Filter::default(), Sort::default()).unwrap();
    for (k, chunk) in vs.chunks(400).enumerate() {
        s.set_rating(chunk, Rating::new(k as u8 % 6).unwrap())
            .unwrap();
    }
    s.set_color_tag(&vs[..300], ColorTag::Red).unwrap();
    (s, vs)
}

fn benches(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bench");
    let (mut s, vs) = build(&root, 2000);

    let rated = Filter {
        min_rating: Rating::new(3),
        ..Filter::default()
    };
    let complex = Filter {
        min_rating: Rating::new(1),
        tags: vec![ColorTag::Red, ColorTag::None],
        text: Some("dsc01".into()),
        lens: Some("gm".into()),
        iso_min: Some(200),
        iso_max: Some(3200),
        ..Filter::default()
    };
    let mut g = c.benchmark_group("filter_2000");
    g.bench_function("all_by_time", |b| {
        b.iter(|| s.query(&Filter::default(), Sort::default()).unwrap());
    });
    g.bench_function("rating_ge3_by_name", |b| {
        b.iter(|| s.query(&rated, Sort::by(SortKey::Name)).unwrap());
    });
    g.bench_function("complex_by_rating", |b| {
        b.iter(|| s.query(&complex, Sort::by(SortKey::Rating)).unwrap());
    });
    g.finish();

    let mut g = c.benchmark_group("writes");
    g.sample_size(10).measurement_time(Duration::from_secs(10));
    let mut k = 0u8;
    g.bench_function("set_rating_500", |b| {
        b.iter(|| {
            k = (k + 1) % 6;
            s.set_rating(&vs[..500], Rating::new(k).unwrap()).unwrap();
        });
    });
    let mut ev = 0.0f32;
    g.bench_function("save_adjustments_1", |b| {
        b.iter(|| {
            ev += 0.01;
            let mut a = Adjustments::default();
            a.exposure.exposure_ev = ev;
            s.save_adjustments(vs[0], &a).unwrap();
        });
    });
    g.bench_function("open_2000", |b| b.iter(|| Session::open(&root).unwrap()));
    g.finish();
}

criterion_group!(session, benches);
criterion_main!(session);
