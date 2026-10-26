//! Progress events, the callback interface Swift implements, and the
//! dispatcher that delivers them without ever blocking a worker.
//!
//! Threading contract (also in docs/FFI.md):
//! - Workers never call Swift. They enqueue events; one dedicated delivery
//!   thread per `Engine` calls `on_progress`, in order, never holding a lock.
//! - `Running` events are dropped (not queued) while more than
//!   [`MAX_PENDING_RUNNING`] are waiting, so a slow listener can't grow memory
//!   or slow a render. Lifecycle events (`Started` and the terminal phase) are
//!   never dropped, so every task visibly ends.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Backlog of undelivered `Running` events before new ones are dropped.
pub const MAX_PENDING_RUNNING: usize = 64;

/// Which stage a task is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TaskPhase {
    Started,
    Running,
    Finished,
    Cancelled,
    Failed,
}

/// One progress update. `completed <= total` always holds.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ProgressEvent {
    /// Engine-unique, monotonically increasing per started task.
    pub task_id: u64,
    /// Short task kind, e.g. `"render"`, later `"import"`, `"archive"`.
    pub kind: String,
    pub phase: TaskPhase,
    pub completed: u64,
    pub total: u64,
}

impl ProgressEvent {
    /// 0.0..=1.0; an empty task counts as done.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.completed as f64 / self.total as f64
        }
    }
}

/// Implemented in Swift. Called on the engine's delivery thread (never a
/// worker, never the main thread); must be `Send + Sync` and cheap.
#[uniffi::export(with_foreign)]
pub trait ProgressListener: Send + Sync {
    /// Errors (thrown from Swift) are ignored by the dispatcher; the signature
    /// throws so no generated Swift call site uses `try!`.
    fn on_progress(&self, event: ProgressEvent) -> Result<(), crate::error::DsError>;
}

type Slot = Arc<Mutex<Option<Arc<dyn ProgressListener>>>>;

enum Msg {
    Event(ProgressEvent),
    Flush(mpsc::SyncSender<()>),
}

/// Queue + delivery thread. Dropping it ends the thread once the queue drains.
pub(crate) struct Dispatcher {
    tx: mpsc::Sender<Msg>,
    /// The queue's receiving end until the delivery thread takes it.
    rx: Mutex<Option<mpsc::Receiver<Msg>>>,
    listener: Slot,
    pending_running: Arc<AtomicUsize>,
    dropped: AtomicU64,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl Dispatcher {
    /// No thread yet: most calls pass no listener, so it starts with the first
    /// [`Dispatcher::set_listener`].
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            listener: Arc::new(Mutex::new(None)),
            pending_running: Arc::new(AtomicUsize::new(0)),
            dropped: AtomicU64::new(0),
        }
    }

    fn start(&self, rx: mpsc::Receiver<Msg>) {
        let (slot, pending) = (self.listener.clone(), self.pending_running.clone());
        // If the OS refuses a thread, degrade to "no delivery" (events are
        // dropped with the receiver) instead of panicking inside an export.
        let _ = std::thread::Builder::new()
            .name("lt-progress".into())
            .spawn(move || {
                for msg in rx {
                    match msg {
                        Msg::Event(e) => {
                            if e.phase == TaskPhase::Running {
                                pending.fetch_sub(1, Ordering::AcqRel);
                            }
                            // Snapshot, then call without holding the lock.
                            let l = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
                            if let Some(l) = l {
                                // A panicking listener must not kill delivery.
                                let _ = catch_unwind(AssertUnwindSafe(|| l.on_progress(e)));
                            }
                        }
                        Msg::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            });
    }

    pub(crate) fn set_listener(&self, l: Option<Arc<dyn ProgressListener>>) {
        let some = l.is_some();
        *self.listener.lock().unwrap_or_else(PoisonError::into_inner) = l;
        if some
            && let Some(rx) = self
                .rx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            self.start(rx);
        }
    }

    fn has_listener(&self) -> bool {
        self.listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Enqueue without blocking. `Running` events may be dropped under backlog.
    pub(crate) fn emit(&self, e: ProgressEvent) {
        if !self.has_listener() {
            return;
        }
        if e.phase == TaskPhase::Running {
            // Check-and-reserve atomically so concurrent emitters can't overshoot the cap.
            // `fetch_update` (deprecated for `try_update`, which needs Rust 1.95; MSRV 1.90).
            #[allow(deprecated)]
            let reserved =
                self.pending_running
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n < MAX_PENDING_RUNNING).then_some(n + 1)
                    });
            if reserved.is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let _ = self.tx.send(Msg::Event(e));
    }

    /// Block until everything queued so far was delivered (tests, teardown).
    /// Returns false on timeout.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        // Without a listener nothing was ever queued (and no thread runs).
        if !self.has_listener() {
            return true;
        }
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(ack_tx)).is_err() {
            return false;
        }
        ack_rx.recv_timeout(timeout).is_ok()
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    fn ev(id: u64, phase: TaskPhase, done: u64) -> ProgressEvent {
        ProgressEvent {
            task_id: id,
            kind: "t".into(),
            phase,
            completed: done,
            total: 10,
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ProgressEvent>>);
    impl ProgressListener for Recorder {
        fn on_progress(&self, event: ProgressEvent) -> Result<(), crate::error::DsError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[test]
    fn fraction_handles_empty_and_partial() {
        let mut e = ev(1, TaskPhase::Running, 0);
        e.total = 0;
        assert!((e.fraction() - 1.0).abs() < f64::EPSILON);
        e.total = 4;
        e.completed = 1;
        assert!((e.fraction() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn delivers_in_order_on_another_thread() {
        let d = Dispatcher::new();
        let rec = Arc::new(Recorder::default());
        d.set_listener(Some(rec.clone()));
        for i in 0..5 {
            d.emit(ev(1, TaskPhase::Running, i));
        }
        d.emit(ev(1, TaskPhase::Finished, 5));
        assert!(d.flush(Duration::from_secs(5)));
        let got: Vec<_> = rec.0.lock().unwrap().iter().map(|e| e.completed).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(d.dropped(), 0);
    }

    #[test]
    fn nothing_is_queued_without_a_listener() {
        let d = Dispatcher::new();
        d.emit(ev(1, TaskPhase::Running, 0));
        assert!(d.flush(Duration::from_secs(5)));
        assert_eq!(d.pending_running.load(Ordering::Acquire), 0);
    }

    #[test]
    fn the_delivery_thread_starts_with_the_first_listener_only() {
        let d = Dispatcher::new();
        assert!(
            d.rx.lock().unwrap().is_some(),
            "no thread without a listener"
        );
        let rec = Arc::new(Recorder::default());
        d.set_listener(Some(rec.clone()));
        assert!(d.rx.lock().unwrap().is_none(), "the thread took the queue");
        d.set_listener(Some(rec.clone()));
        d.set_listener(None);
        d.set_listener(Some(rec.clone()));
        d.emit(ev(1, TaskPhase::Finished, 1));
        assert!(d.flush(Duration::from_secs(5)));
        assert_eq!(rec.0.lock().unwrap().len(), 1);
    }

    /// Blocks inside `on_progress` until released; counts deliveries.
    struct Stuck {
        gate: Mutex<bool>,
        cv: Condvar,
        seen: Mutex<Vec<TaskPhase>>,
    }
    impl ProgressListener for Stuck {
        fn on_progress(&self, event: ProgressEvent) -> Result<(), crate::error::DsError> {
            let mut open = self.gate.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
            self.seen.lock().unwrap().push(event.phase);
            Ok(())
        }
    }

    #[test]
    fn slow_listener_never_blocks_emitters_and_keeps_terminal_events() {
        let d = Dispatcher::new();
        let l = Arc::new(Stuck {
            gate: Mutex::new(false),
            cv: Condvar::new(),
            seen: Mutex::new(Vec::new()),
        });
        d.set_listener(Some(l.clone()));
        let start = std::time::Instant::now();
        d.emit(ev(1, TaskPhase::Started, 0));
        for i in 0..10_000 {
            d.emit(ev(1, TaskPhase::Running, i));
        }
        d.emit(ev(1, TaskPhase::Finished, 10));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "emit never blocks"
        );
        assert!(d.dropped() >= 10_000 - MAX_PENDING_RUNNING as u64);
        *l.gate.lock().unwrap() = true;
        l.cv.notify_all();
        assert!(d.flush(Duration::from_secs(5)));
        let seen = l.seen.lock().unwrap().clone();
        assert_eq!(seen.first(), Some(&TaskPhase::Started));
        assert_eq!(seen.last(), Some(&TaskPhase::Finished));
        assert!(seen.len() <= MAX_PENDING_RUNNING + 2);
    }

    struct Panicky;
    impl ProgressListener for Panicky {
        fn on_progress(&self, _: ProgressEvent) -> Result<(), crate::error::DsError> {
            panic!("listener bug");
        }
    }

    #[test]
    fn a_panicking_listener_does_not_stop_delivery() {
        let d = Dispatcher::new();
        d.set_listener(Some(Arc::new(Panicky)));
        d.emit(ev(1, TaskPhase::Started, 0));
        assert!(d.flush(Duration::from_secs(5)));
        let rec = Arc::new(Recorder::default());
        d.set_listener(Some(rec.clone()));
        d.emit(ev(2, TaskPhase::Started, 0));
        assert!(d.flush(Duration::from_secs(5)));
        assert_eq!(rec.0.lock().unwrap().len(), 1);
        assert!(format!("{d:?}").starts_with("Dispatcher"));
    }

    #[test]
    fn concurrent_emitters_never_exceed_the_cap() {
        let d = Arc::new(Dispatcher::new());
        let l = Arc::new(Stuck {
            gate: Mutex::new(false),
            cv: Condvar::new(),
            seen: Mutex::new(Vec::new()),
        });
        d.set_listener(Some(l.clone()));
        // Park the delivery thread on a first event so nothing drains.
        d.emit(ev(0, TaskPhase::Started, 0));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for i in 0..2_000 {
                        d.emit(ev(t, TaskPhase::Running, i));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(d.pending_running.load(Ordering::Acquire) <= MAX_PENDING_RUNNING);
        assert_eq!(
            d.dropped() + d.pending_running.load(Ordering::Acquire) as u64,
            16_000
        );
        *l.gate.lock().unwrap() = true;
        l.cv.notify_all();
        assert!(d.flush(Duration::from_secs(5)));
    }
}
