//! Find budget: a query across 50,000 indexed photos returns in ≤ 300 ms
//! (release builds; debug and coverage builds only check the results).

use std::time::Instant;

#[path = "support/find_fixture.rs"]
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
fn find_across_50000_photos_is_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = fixture::build(dir.path());
    let structured = fixture::structured();
    let first = g.find(&structured).unwrap();
    // Alex (tagged on 1 in 20 by i % 40 ∈ {0, 1}, suggested on 1 in 50) in a
    // Iceland session or with an Iceland location, in 2024.
    assert!(first.total > 0);
    assert!(first.photos.iter().all(|p| {
        p.reasons
            .iter()
            .any(|r| matches!(r, ds_session::MatchReason::Person { .. }))
            && p.reasons
                .iter()
                .any(|r| matches!(r, ds_session::MatchReason::Place { .. }))
    }));
    let visual = fixture::visual();
    let text = fixture::text_only();
    assert_eq!(g.find(&text).unwrap().total, fixture::N);
    assert!(g.find(&visual).unwrap().total > 0);

    let budget = 300.0 * load_factor();
    let mut report = Vec::new();
    for (name, spec) in [
        ("structured", &structured),
        ("visual", &visual),
        ("text", &text),
    ] {
        let ms = median_ms(|| {
            g.find(spec).unwrap();
        });
        report.push(format!("{name} {ms:.1} ms"));
        if !cfg!(debug_assertions) {
            assert!(ms <= budget, "{name}: median {ms:.1} ms > {budget:.0} ms");
        }
    }
    // The first query after a session changed reloads the rows.
    let root = fixture::root(0);
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut records = fixture::records(0);
    let mut n = 0u8;
    let ms = median_ms(|| {
        n += 1;
        records[0].rating = n % 6;
        g.sync_find_items(&root, &name, &records).unwrap();
        g.find(&structured).unwrap();
    });
    report.push(format!("after a change {ms:.1} ms"));
    println!("find_50000: {}", report.join(", "));
    if !cfg!(debug_assertions) {
        assert!(ms <= 3.0 * budget, "after a change: median {ms:.1} ms");
    }
}
