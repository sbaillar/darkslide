//! Import copy throughput vs a fair plain-copy baseline.
//!
//! Source: the real A9 ARWs in `samples/` when present (else 8 synthetic
//! 48 MiB files), read into the page cache first (ARCHITECTURE budget: "file
//! in page cache").
//!
//! * `baseline_3x4MiB_fsync`: 3 threads, 4 MiB buffers, read/write, `fsync`
//!   per file and one `F_FULLFSYNC` at the end: the same durability as the
//!   import, without hashing or verification. Import target: ≥ 90% of this.
//! * `lt_import_c1` / `lt_import_c3`: full import (single read pass + BLAKE3
//!   + fsync + uncached re-read verify + no-replace rename + checkpoints).
//! * `card_90MBps/*`: the same two, with the source rate-limited to 90 MB/s
//!   in total (a UHS-I card reader), showing the import is card-bound.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use lt_import::{FaultInjector, ImportPlan, NoMeta, PlanOptions, RunHooks, run_with, scan_volume};

const CHUNK: usize = 4 << 20;

fn source_card(root: &Path) -> Vec<PathBuf> {
    let dir = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dir).unwrap();
    let samples = lt_testdata::samples_with_ext("arw");
    let mut out = Vec::new();
    if samples.is_empty() {
        for i in 0..8u8 {
            let p = dir.join(format!("SYN{i:05}.ARW"));
            let data: Vec<u8> = (0..48usize << 20)
                .map(|j| (j as u8).wrapping_mul(37) ^ i)
                .collect();
            fs::write(&p, data).unwrap();
            out.push(p);
        }
    } else {
        for s in samples {
            let p = dir.join(s.file_name().unwrap());
            std::io::copy(
                &mut fs::File::open(&s).unwrap(),
                &mut fs::File::create(&p).unwrap(),
            )
            .unwrap();
            out.push(p);
        }
    }
    out
}

fn warm(files: &[PathBuf]) {
    for f in files {
        let _ = fs::read(f).unwrap();
    }
}

/// Shared token bucket: all readers together get at most `bps` bytes/s.
struct CardReader {
    bps: f64,
    state: Mutex<(Instant, f64)>,
}

impl CardReader {
    fn new(bps: f64) -> Self {
        Self {
            bps,
            state: Mutex::new((Instant::now(), 0.0)),
        }
    }
    fn take(&self, bytes: usize) {
        let wait = {
            let mut s = self.state.lock().unwrap();
            let (start, sent) = *s;
            s.1 = sent + bytes as f64;
            let due = start + Duration::from_secs_f64(s.1 / self.bps);
            due.saturating_duration_since(Instant::now())
        };
        std::thread::sleep(wait);
    }
}

impl FaultInjector for CardReader {
    fn before_read(&self, _: &Path, _: u64) -> std::io::Result<()> {
        self.take(CHUNK);
        Ok(())
    }
}

/// 3 threads × 4 MiB buffers, fsync per file, one `F_FULLFSYNC`.
fn baseline(files: &[PathBuf], out: &Path, card: Option<&CardReader>) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                let mut buf = vec![0u8; CHUNK];
                while let Some(f) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut src = fs::File::open(f).unwrap();
                    let mut dst = fs::File::create(out.join(f.file_name().unwrap())).unwrap();
                    loop {
                        if let Some(c) = card {
                            c.take(CHUNK);
                        }
                        let n = src.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        dst.write_all(&buf[..n]).unwrap();
                    }
                    lt_fsutil::sync_file(&dst).unwrap();
                }
            });
        }
    });
    lt_fsutil::flush_device(out).unwrap();
}

fn import(candidates: &[lt_import::Candidate], out: &Path, conc: usize, card: Option<&CardReader>) {
    let mut opts = PlanOptions::new(out);
    opts.concurrency = conc;
    let plan = ImportPlan::new(candidates, opts, &NoMeta).unwrap();
    let hooks = RunHooks {
        faults: card.map(|c| c as &dyn FaultInjector),
        sink: None,
    };
    let r = run_with(&plan, hooks, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert!(r.is_complete());
}

fn bench(c: &mut Criterion) {
    let card = tempfile::tempdir().unwrap();
    let files = source_card(card.path());
    let total: u64 = files.iter().map(|f| fs::metadata(f).unwrap().len()).sum();
    let candidates = scan_volume(card.path()).unwrap().candidates;
    let out_root = tempfile::tempdir().unwrap();
    let fresh = || tempfile::tempdir_in(out_root.path()).unwrap();

    let mut g = c.benchmark_group("import_copy");
    g.throughput(Throughput::Bytes(total));
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(20));
    warm(&files);
    g.bench_function("baseline_3x4MiB_fsync", |b| {
        b.iter_batched(
            fresh,
            |d| {
                baseline(&files, d.path(), None);
                d
            },
            BatchSize::PerIteration,
        );
    });
    for conc in [1usize, 3] {
        warm(&files);
        g.bench_function(format!("lt_import_c{conc}"), |b| {
            b.iter_batched(
                fresh,
                |d| {
                    import(&candidates, d.path(), conc, None);
                    d
                },
                BatchSize::PerIteration,
            );
        });
    }
    g.finish();

    // Card-bound: 90 MB/s total from the "card".
    let mut g = c.benchmark_group("card_90MBps");
    g.throughput(Throughput::Bytes(total));
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(45));
    g.bench_function("baseline_3x4MiB_fsync", |b| {
        b.iter_batched(
            fresh,
            |d| {
                baseline(&files, d.path(), Some(&CardReader::new(90e6)));
                d
            },
            BatchSize::PerIteration,
        );
    });
    g.bench_function("lt_import_c3", |b| {
        b.iter_batched(
            fresh,
            |d| {
                import(&candidates, d.path(), 3, Some(&CardReader::new(90e6)));
                d
            },
            BatchSize::PerIteration,
        );
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
