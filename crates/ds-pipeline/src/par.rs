//! The pipeline's own thread pool for the data-parallel CPU work of GPU renders (planning:
//! noise measurement, lens/vignetting maps; mask rasterization).
//!
//! GPU renders wait for their turn and their class's slot (`gpu::Gate`), and the render
//! holding a slot runs parallel loops. On the caller's pool that deadlocks when every
//! worker of it waits for the slot (the holder's loop never gets a worker), and a worker
//! holding a slot must not steal its pool's jobs while its loop runs (a stolen render
//! would wait for the slot its own outer frame holds). So these loops run here, on
//! workers that never render, and callers block without stealing.

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, OnceLock};

thread_local! {
    /// Pipeline locks this thread holds (see [`hold`]).
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// A pipeline mutex guard that counts as held for [`run`]'s check. Every ds-pipeline
/// cache/renderer lock is taken through [`hold`]: a thread must never enter a thread pool
/// (or run parallel iterators) while holding one, since a worker waiting for its parallel
/// work can steal another render's job that blocks on the same lock.
pub(crate) struct Held<'a, T>(MutexGuard<'a, T>);

impl<T> std::ops::Deref for Held<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get().saturating_sub(1)));
    }
}

/// Lock a pipeline mutex (poisoning ignored), counted for [`run`]'s check.
pub(crate) fn hold<T>(m: &Mutex<T>) -> Held<'_, T> {
    let g = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    HELD.with(|h| h.set(h.get() + 1));
    Held(g)
}

/// Pipeline locks held by this thread.
pub(crate) fn held() -> u32 {
    HELD.with(Cell::get)
}

fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("ds-pipeline-{i}"))
            .build()
            // Thread creation failing is fatal elsewhere too; one thread still works.
            .unwrap_or_else(|_| pool_of_one())
    })
}

/// Half the cores, for non-interactive CPU renders (exports, thumbnails): an interactive
/// render on the global pool always has cores left.
fn low_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        rayon::ThreadPoolBuilder::new()
            .num_threads((n / 2).max(1))
            .thread_name(|i| format!("ds-pipeline-low-{i}"))
            .build()
            .unwrap_or_else(|_| pool_of_one())
    })
}

fn pool_of_one() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("a one-thread pool")
}

/// Run `f` (whose parallel iterators then use the pipeline pool) and wait for it without
/// running anyone else's jobs meanwhile.
pub(crate) fn run<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    run_on(pool(), f)
}

/// [`run`] on the low-priority pool.
pub(crate) fn run_low<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    run_on(low_pool(), f)
}

/// [`run`] for interactive work, [`run_low`] for everything else: a background render's
/// CPU work (a full-sensor noise scan, a RAW staging copy, tile planning) never queues
/// ahead of the viewer's planning on the main pool.
pub(crate) fn run_for<R: Send>(p: crate::Priority, f: impl FnOnce() -> R + Send) -> R {
    if p == crate::Priority::Interactive {
        run(f)
    } else {
        run_low(f)
    }
}

/// [`run`] for CPU work done while holding the GPU renderer's own lock (`gpu::Inner`,
/// e.g. converting a linear image into its BASE). That lock is held across the work, so
/// it must never run on a pool whose workers render: rayon's global pool (or the
/// caller's), whose workers may all be blocked on that lock or on the GPU gate slot this
/// render holds. The pipeline pool's jobs never take the renderer's lock, so entering it
/// with the lock held is safe (the only exception to [`run`]'s no-lock check).
pub(crate) fn run_holding_renderer<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    debug_assert_eq!(held(), 1, "only the renderer's own lock may be held here");
    enter(pool(), f)
}

fn run_on<R: Send>(pool: &rayon::ThreadPool, f: impl FnOnce() -> R + Send) -> R {
    debug_assert_eq!(
        held(),
        0,
        "a ds-pipeline lock is held while entering a thread pool (deadlock hazard)"
    );
    enter(pool, f)
}

fn enter<R: Send>(pool: &rayon::ThreadPool, f: impl FnOnce() -> R + Send) -> R {
    if pool.current_thread_index().is_some() {
        return f();
    }
    if rayon::current_thread_index().is_some() {
        // A worker of another pool would keep busy with its own pool's jobs inside
        // `install`: hop to a plain thread, which blocks.
        return std::thread::scope(|s| {
            s.spawn(|| pool.install(f))
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
        });
    }
    pool.install(f)
}

#[cfg(test)]
mod tests {
    use rayon::prelude::*;

    #[test]
    fn held_locks_are_counted() {
        let m = std::sync::Mutex::new(1);
        assert_eq!(super::held(), 0);
        {
            let mut g = super::hold(&m);
            *g += 1;
            assert_eq!((*g, super::held()), (2, 1));
        }
        assert_eq!(super::held(), 0);
    }

    /// Entering a pool with a pipeline lock held is the deadlock pattern: caught in debug.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "held while entering a thread pool")]
    fn entering_a_pool_with_a_lock_held_is_caught() {
        let m = std::sync::Mutex::new(0);
        let _g = super::hold(&m);
        super::run(|| ());
    }

    /// Background CPU work (a noise scan, a staging copy) runs on the low pool: an
    /// interactive run never queues behind it, even when it keeps every low worker busy.
    #[test]
    fn background_work_never_delays_interactive_runs() {
        use crate::Priority;
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let bg = {
            let busy = busy.clone();
            std::thread::spawn(move || {
                super::run_for(Priority::Background, || {
                    (0..64).into_par_iter().for_each(|_| {
                        let t = std::time::Instant::now();
                        while busy.load(std::sync::atomic::Ordering::Relaxed)
                            && t.elapsed() < std::time::Duration::from_secs(2)
                        {
                            std::hint::spin_loop();
                        }
                    });
                    super::low_pool().current_thread_index().is_some()
                })
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..20 {
            let t = std::time::Instant::now();
            let on_main = super::run_for(Priority::Interactive, || {
                (0..1000).into_par_iter().sum::<u64>();
                super::pool().current_thread_index().is_some()
            });
            assert!(on_main);
            worst = worst.max(t.elapsed());
        }
        busy.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(bg.join().unwrap(), "background work ran on the low pool");
        assert!(
            worst < std::time::Duration::from_millis(500),
            "an interactive run waited {worst:?} behind background work"
        );
    }

    #[test]
    fn runs_on_the_pipeline_pool_from_any_thread() {
        let on_pool = || super::pool().current_thread_index().is_some();
        assert!(super::run(on_pool));
        // From a worker of the global pool, and nested.
        let r = rayon::join(
            || super::run(on_pool),
            || super::run(|| super::run(on_pool)),
        );
        assert_eq!(r, (true, true));
        let sum: u64 = super::run(|| (0..1000u64).into_par_iter().sum());
        assert_eq!(sum, 499_500);
        let low = super::run_low(|| {
            (
                super::low_pool().current_thread_index().is_some(),
                rayon::current_num_threads(),
            )
        });
        assert!(low.0);
        assert!(low.1 >= 1 && low.1 <= super::pool().current_num_threads());
        assert_eq!(super::pool_of_one().current_num_threads(), 1);
        let caught =
            std::panic::catch_unwind(|| rayon::join(|| super::run(|| panic!("boom")), || ()));
        assert!(caught.is_err(), "a panic inside propagates");
    }
}
