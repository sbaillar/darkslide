//! Adversarial-review regression tests: scheduler stress and `DiskCache::get` cost.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lt_cache::*;
use lt_core::FileHash;

fn rng(seed: &AtomicU64) -> u64 {
    let mut x = seed.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^ (x >> 33)
}

/// Many threads submit/dedupe/cancel/reprioritize random keys. Every waiter
/// must be called exactly once; no `Done` for a waiter whose `cancel()` was
/// observed before completion; `wait_idle` must return (no lost wakeup).
#[test]
fn scheduler_stress() {
    for round in 0..20 {
        let s: Arc<JobScheduler<u32, u32>> = Arc::new(JobScheduler::new(4));
        let calls: Arc<Mutex<HashMap<u64, u32>>> = Arc::default();
        let next_id = Arc::new(AtomicU64::new(0));
        let seed = Arc::new(AtomicU64::new(round));
        std::thread::scope(|sc| {
            for _ in 0..8 {
                let (s, calls, next_id, seed) = (&s, &calls, &next_id, &seed);
                sc.spawn(move || {
                    for _ in 0..3000 {
                        let r = rng(seed);
                        let key = (r % 64) as u32;
                        match r % 10 {
                            0..=5 => {
                                let id = next_id.fetch_add(1, Ordering::Relaxed);
                                calls.lock().unwrap().insert(id, 0);
                                let c = Arc::clone(calls);
                                let prio =
                                    [Priority::Background, Priority::Prefetch, Priority::Visible]
                                        [(r as usize >> 8) % 3];
                                s.submit(
                                    key,
                                    prio,
                                    move |t| {
                                        let end = Instant::now() + Duration::from_micros(50);
                                        while Instant::now() < end && !t.is_cancelled() {}
                                        key
                                    },
                                    move |res| {
                                        if let JobResult::Done(v) = res {
                                            assert_eq!(v, key);
                                        }
                                        *c.lock().unwrap().get_mut(&id).unwrap() += 1;
                                    },
                                );
                            }
                            6 => {
                                s.cancel(&key);
                            }
                            7 => {
                                s.reprioritize(&key, Priority::Background);
                            }
                            8 => {
                                s.reprioritize_where(|k, _| {
                                    (k % 3 == 0).then_some(Priority::Visible)
                                });
                            }
                            _ => {
                                s.cancel_where(|k, p| k % 7 == 0 && p == Priority::Background);
                            }
                        }
                    }
                });
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let s2 = Arc::clone(&s);
        std::thread::spawn(move || {
            s2.wait_idle();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("wait_idle deadlocked");
        drop(s);
        let c = calls.lock().unwrap();
        let bad: Vec<_> = c.iter().filter(|(_, n)| **n != 1).collect();
        assert!(
            bad.is_empty(),
            "round {round}: {} waiters called != 1 time (of {})",
            bad.len(),
            c.len()
        );
    }
}

const ORIG: FileHash = FileHash([3; 32]);

fn filled(n: u64) -> (tempfile::TempDir, DiskCache, AdjHash) {
    let d = tempfile::tempdir().unwrap();
    let c = DiskCache::open(d.path(), 1 << 30).unwrap();
    let adj = adjustments_hash(&lt_core::Adjustments::default());
    let jpeg = vec![0u8; 60_000];
    for v in 0..n {
        c.put(&CacheKey::new(v, ORIG, adj, Kind::Thumb), &jpeg)
            .unwrap();
    }
    (d, c, adj)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

/// A grid scrolling at 60 fps does ~2,400 `get`s per second. The old
/// implementation committed a SQLite UPDATE per call (~18 µs of index work
/// holding the cache mutex); `get` must now cost about a file read.
#[test]
fn cache_get_costs_about_a_file_read() {
    let (_d, c, adj) = filled(2000);
    let key = |i: u64| CacheKey::new(i % 2000, ORIG, adj, Kind::Thumb);
    // Interleave a plain read and a get of the same entry so machine noise
    // (parallel tests) hits both; compare medians of the per-pair difference.
    let mut diffs = Vec::new();
    let (mut reads, mut gets) = (Vec::new(), Vec::new());
    for i in 0..10_000u64 {
        let k = key(i * 7);
        std::hint::black_box(std::fs::read(c.path_for(&k)).unwrap()); // warm both equally
        let t = Instant::now();
        std::hint::black_box(std::fs::read(c.path_for(&k)).unwrap());
        let read = t.elapsed();
        let t = Instant::now();
        assert!(c.get(&k).unwrap().is_some());
        let get = t.elapsed();
        diffs.push(get.saturating_sub(read));
        reads.push(read);
        gets.push(get);
    }
    let (read, get, overhead) = (median(reads), median(gets), median(diffs));
    eprintln!("file read {read:?}, get {get:?}, median overhead {overhead:?}");
    // The old per-get UPDATE + commit cost ~18 µs over a ~16 µs read here. A slower
    // machine (CI: 52 µs reads) scales both, so bound the overhead by the read too.
    let bound = Duration::from_micros(6).max(read / 4);
    assert!(
        overhead < bound,
        "index overhead per get {overhead:?} (bound {bound:?})"
    );
}

/// `contains` and repeated gets of the hottest entry touch no SQLite at all.
#[test]
fn hot_gets_do_not_grow_the_wal() {
    let (d, c, adj) = filled(50);
    c.flush().unwrap();
    let k = CacheKey::new(49, ORIG, adj, Kind::Thumb);
    let wal = || std::fs::metadata(d.path().join("index.db-wal")).map_or(0, |m| m.len());
    let before = wal();
    for _ in 0..10_000 {
        assert!(c.get(&k).unwrap().is_some());
        assert!(c.contains(&k).unwrap());
    }
    assert_eq!(wal(), before);
}
