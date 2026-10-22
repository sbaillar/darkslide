//! `JobScheduler` overhead: submit → run trivial closure → callback, per job.

use std::hint::black_box;
use std::sync::mpsc::channel;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ds_cache::{JobScheduler, Priority};

fn bench_scheduler(c: &mut Criterion) {
    let mut g = c.benchmark_group("scheduler");
    let jobs = 1_000u64;
    g.throughput(Throughput::Elements(jobs));
    for workers in [1usize, 4] {
        let s: JobScheduler<u64, u64> = JobScheduler::new(workers);
        g.bench_function(format!("roundtrip_1000_jobs_{workers}w"), |b| {
            b.iter(|| {
                let (tx, rx) = channel();
                for k in 0..jobs {
                    let tx = tx.clone();
                    let p = if k % 2 == 0 {
                        Priority::Visible
                    } else {
                        Priority::Prefetch
                    };
                    s.submit(k, p, move |_| k, move |r| tx.send(r).unwrap());
                }
                drop(tx);
                black_box(rx.iter().count())
            });
        });
    }
    // Queue-side cost only: submit then cancel everything (no job runs).
    let s: JobScheduler<u64, u64> = JobScheduler::new(1);
    let (gate_tx, gate_rx) = channel::<()>();
    s.submit(
        u64::MAX,
        Priority::Visible,
        move |_| gate_rx.recv().map_or(0, |()| 0),
        |_| {},
    );
    g.bench_function("submit_cancel_1000", |b| {
        b.iter(|| {
            for k in 0..jobs {
                s.submit(k, Priority::Background, move |_| k, |_| {});
            }
            black_box(s.cancel_where(|k, _| *k != u64::MAX))
        });
    });
    g.finish();
    drop(gate_tx);
}

criterion_group!(benches, bench_scheduler);
criterion_main!(benches);
