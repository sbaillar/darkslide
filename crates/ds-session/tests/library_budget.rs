//! Library budgets on 100,000 photos (release builds; debug and coverage
//! builds only check the results): cold open to the first screen ≤ 300 ms,
//! a filter or sort change ≤ 50 ms, an incremental sync of one changed
//! session ≤ 100 ms.

use std::time::Instant;

use ds_session::{GlobalIndex, LibraryQuery};

#[path = "support/library_fixture.rs"]
mod fixture;

fn load_factor() -> f64 {
    let load = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().find_map(|t| t.parse::<f64>().ok()))
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
    (load / cores).max(1.0)
}

fn median_ms(mut f: impl FnMut()) -> f64 {
    let mut t: Vec<f64> = (0..7)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    t.sort_by(f64::total_cmp);
    t[t.len() / 2]
}

#[test]
fn library_of_100000_photos_is_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    drop(fixture::build(dir.path()));
    let path = dir.path().join("global.db");
    let load = load_factor();
    let mut report = Vec::new();
    let check = |report: &mut Vec<String>, name: &str, ms: f64, budget: f64| {
        report.push(format!("{name} {ms:.1} ms"));
        if !cfg!(debug_assertions) {
            assert!(
                ms <= budget * load,
                "{name}: median {ms:.1} ms > {budget:.0} ms × {load:.1}"
            );
        }
    };

    let ms = median_ms(|| {
        let mut g = GlobalIndex::open(&path).unwrap();
        let ids = g.library_query(&LibraryQuery::default()).unwrap();
        assert_eq!(ids.len(), fixture::N);
        assert_eq!(
            g.library_items(&ids[..fixture::FIRST_SCREEN])
                .unwrap()
                .len(),
            fixture::FIRST_SCREEN
        );
    });
    check(&mut report, "cold open", ms, 300.0);

    // The first sort by name, and the first text, after a load build their data.
    let by_name = LibraryQuery {
        sort: ds_session::LibrarySort::Name,
        ..LibraryQuery::default()
    };
    let text = LibraryQuery {
        text: "lake".into(),
        ..LibraryQuery::default()
    };
    for (name, q) in [("first sort by name", &by_name), ("first text", &text)] {
        let mut first: Vec<f64> = (0..7)
            .map(|_| {
                let mut g = GlobalIndex::open(&path).unwrap();
                g.library_query(&LibraryQuery::default()).unwrap();
                let t = Instant::now();
                g.library_query(q).unwrap();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        first.sort_by(f64::total_cmp);
        check(&mut report, name, first[3], 50.0);
    }

    let mut g = GlobalIndex::open(&path).unwrap();
    for (name, q) in fixture::queries() {
        let n = g.library_query(&q).unwrap().len();
        assert!(n > 0 && n <= fixture::N, "{name}: {n}");
        let ms = median_ms(|| {
            g.library_query(&q).unwrap();
        });
        check(&mut report, name, ms, 50.0);
    }

    let root = fixture::root(0);
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut records = fixture::records(0);
    let mut n = 0u8;
    let ms = median_ms(|| {
        n += 1;
        records[0].rating = n % 6;
        assert!(
            g.sync_library(&root, &name, &fixture::source(0), &records)
                .unwrap()
        );
    });
    check(&mut report, "sync one changed session", ms, 100.0);
    println!("library_100000: {}", report.join(", "));
}
