//! Background work that must never slow culling down: hashing originals of
//! an in-place session on its own two utility-QoS threads (not the rayon pool
//! every preview and save runs on), reading with `F_NOCACHE` (see
//! `ds_session::identity`), and pausing while the user is active: the app
//! calls [`note_user_activity`] on every navigation and rating, and workers
//! wait until [`IDLE`] has passed without one.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::DsError;
use crate::jobs::guard;

/// Files read at once (bounded I/O: a card reader or a NAS is not saturated).
pub(crate) const READERS: usize = 2;
/// How long after the last user action background work resumes.
pub(crate) const IDLE: Duration = Duration::from_millis(500);

/// Milliseconds since the epoch of the last user action (0: none yet).
static LAST_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The user just did something (navigated, rated): background hashing
/// pauses until [`IDLE`] passes without another call.
#[uniffi::export]
pub fn note_user_activity() -> Result<(), DsError> {
    guard(|| {
        mark_activity();
        Ok(())
    })
}

pub(crate) fn mark_activity() {
    LAST_ACTIVITY_MS.store(now_ms(), Ordering::Release);
}

/// Whether the user acted within the last [`IDLE`].
pub(crate) fn user_active() -> bool {
    let last = LAST_ACTIVITY_MS.load(Ordering::Acquire);
    last != 0 && now_ms().saturating_sub(last) < IDLE.as_millis() as u64
}

/// Block while the user is active, unless `stop` is set.
pub(crate) fn wait_idle(stop: &AtomicBool) {
    while user_active() && !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Lower the calling thread to utility `QoS` (CPU and I/O priority).
#[allow(unsafe_code)]
fn lower_priority() {
    // SAFETY: sets a scheduling attribute of the calling thread only; no
    // pointers are passed and failure is harmless (the thread keeps its QoS).
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

/// Run `work` over `items` on [`READERS`] dedicated low-priority threads;
/// results reach `sink` on the calling thread as they complete (so it can
/// apply them in batches). `cancelled` is polled by the caller at least every
/// 50 ms; workers then stop between files, and `work` gets the `stop` flag
/// to check inside long reads. Returns `false` when stopped early.
pub(crate) fn run_pool<T, R>(
    items: &[T],
    cancelled: &dyn Fn() -> bool,
    work: impl Fn(&T, &AtomicBool) -> R + Sync,
    mut sink: impl FnMut(usize, R),
) -> bool
where
    T: Sync,
    R: Send,
{
    if items.is_empty() {
        return true;
    }
    if cancelled() {
        return false;
    }
    let stop = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, R)>();
    std::thread::scope(|s| {
        for _ in 0..READERS.min(items.len()) {
            let tx = tx.clone();
            let (stop, next, work) = (&stop, &next, &work);
            s.spawn(move || {
                lower_priority();
                loop {
                    wait_idle(stop);
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::AcqRel);
                    if i >= items.len() {
                        break;
                    }
                    if tx.send((i, work(&items[i], stop))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut received = 0;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((i, r)) => {
                    sink(i, r);
                    received += 1;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !stop.load(Ordering::Acquire) && cancelled() {
                stop.store(true, Ordering::Release);
            }
        }
        received == items.len()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    /// Serializes tests that touch the process-wide activity clock.
    pub(crate) static ACTIVITY: Mutex<()> = Mutex::new(());

    #[test]
    fn pool_runs_every_item_off_the_calling_thread() {
        let items: Vec<u32> = (0..50).collect();
        let caller = std::thread::current().id();
        let mut got = vec![0u32; 50];
        let ok = run_pool(
            &items,
            &|| false,
            |x, _| (*x * 2, std::thread::current().id()),
            |i, (v, t)| {
                assert_ne!(t, caller, "work runs on the pool threads");
                got[i] = v;
            },
        );
        assert!(ok);
        assert_eq!(got, items.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert!(run_pool(&[] as &[u8], &|| true, |_, _| (), |_, ()| {}));
    }

    #[test]
    fn pool_stops_when_cancelled() {
        let items: Vec<u32> = (0..10_000).collect();
        let t = Instant::now();
        let mut n = 0;
        let ok = run_pool(
            &items,
            &|| t.elapsed() > Duration::from_millis(60),
            |_, _| std::thread::sleep(Duration::from_millis(2)),
            |_, ()| n += 1,
        );
        assert!(!ok);
        assert!(n < items.len(), "stopped early ({n})");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn activity_pauses_workers_until_idle() {
        let _g = ACTIVITY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        note_user_activity().unwrap();
        assert!(user_active());
        let t = Instant::now();
        let stop = AtomicBool::new(false);
        wait_idle(&stop);
        let waited = t.elapsed();
        assert!(waited + Duration::from_millis(30) >= IDLE, "{waited:?}");
        assert!(!user_active());
        // A stop request ends the wait at once.
        mark_activity();
        stop.store(true, Ordering::Release);
        let t = Instant::now();
        wait_idle(&stop);
        assert!(t.elapsed() < Duration::from_millis(100));
        LAST_ACTIVITY_MS.store(0, Ordering::Release);
    }
}
