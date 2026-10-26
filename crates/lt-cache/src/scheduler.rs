//! Prioritized, cancellable, deduplicating worker pool.
//!
//! The browser grid scrolls fast: requests for cells that scrolled away must
//! be cheap to demote or cancel, and the same thumbnail requested twice must
//! run once. Jobs are caller-supplied closures keyed by `K`; every waiter on a
//! key receives the result through its own callback (wrap a channel sender to
//! get channel delivery). Workers sleep on a `Condvar`; nothing spins.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

/// Job priority; higher runs first. Within a priority, submission order (FIFO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Background,
    Prefetch,
    Visible,
}

/// Cooperative cancellation flag handed to running jobs.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// What each waiter receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult<T> {
    Done(T),
    /// Cancelled before or while running, or the scheduler shut down.
    Cancelled,
    /// The job closure panicked.
    Panicked,
}

/// Outcome of [`JobScheduler::submit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// A new job was queued.
    New,
    /// An identical key was already queued or running; the callback was attached to it.
    Deduped,
    /// The scheduler is shutting down; the callback already received `Cancelled`.
    Rejected,
}

type Job<T> = Box<dyn FnOnce(&CancelToken) -> T + Send>;
type Callback<T> = Box<dyn FnOnce(JobResult<T>) + Send>;

struct Entry<T> {
    generation: u64,
    prio: Priority,
    seq: u64,
    /// `Some` while queued, `None` once a worker took it.
    job: Option<Job<T>>,
    token: CancelToken,
    waiters: Vec<Callback<T>>,
}

struct State<K, T> {
    queue: BTreeMap<(Reverse<Priority>, u64), K>,
    entries: HashMap<K, Entry<T>>,
    next_seq: u64,
    running: usize,
    /// Callers delivering `Cancelled` outside the lock (see `cancel_where`).
    delivering: usize,
    shutdown: bool,
}

impl<K, T> State<K, T> {
    /// Nothing queued, running, or still being delivered to waiters.
    fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.running == 0 && self.delivering == 0
    }
}

struct Shared<K, T> {
    state: Mutex<State<K, T>>,
    work: Condvar,
    idle: Condvar,
}

impl<K, T> Shared<K, T> {
    fn lock(&self) -> MutexGuard<'_, State<K, T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Bounded pool of worker threads running keyed jobs by priority.
pub struct JobScheduler<K, T> {
    shared: Arc<Shared<K, T>>,
    workers: Vec<JoinHandle<()>>,
}

impl<K, T> fmt::Debug for JobScheduler<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobScheduler")
            .field("workers", &self.workers.len())
            .finish_non_exhaustive()
    }
}

/// Number of performance cores (Apple silicon `hw.perflevel0.physicalcpu`),
/// falling back to the available parallelism.
pub fn perf_cores() -> usize {
    sysctl_perf_cores()
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, std::num::NonZero::get))
}

// iOS can't start processes: there the pool is sized by `available_parallelism` alone.
#[cfg(target_os = "macos")]
fn sysctl_perf_cores() -> Option<usize> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.perflevel0.physicalcpu"])
        .output()
        .ok()?;
    parse_core_count(&out.stdout)
}

#[cfg(not(target_os = "macos"))]
fn sysctl_perf_cores() -> Option<usize> {
    None
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_core_count(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&n| n > 0)
}

fn deliver<T: Clone>(mut waiters: Vec<Callback<T>>, result: &JobResult<T>) {
    // Clone for all but the last waiter; a panicking callback must not kill the worker.
    let last = waiters.pop();
    for cb in waiters {
        let r = result.clone();
        let _ = catch_unwind(AssertUnwindSafe(move || cb(r)));
    }
    if let Some(cb) = last {
        let r = result.clone();
        let _ = catch_unwind(AssertUnwindSafe(move || cb(r)));
    }
}

impl<K, T> JobScheduler<K, T>
where
    K: Hash + Eq + Clone + Send + 'static,
    T: Clone + Send + 'static,
{
    /// Pool sized to the performance cores.
    pub fn with_default_workers() -> Self {
        Self::new(perf_cores())
    }

    /// Pool with `workers` threads (at least one).
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: BTreeMap::new(),
                entries: HashMap::new(),
                next_seq: 0,
                running: 0,
                delivering: 0,
                shutdown: false,
            }),
            work: Condvar::new(),
            idle: Condvar::new(),
        });
        let workers = (0..workers.max(1))
            .map(|i| {
                let s = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name(format!("lt-cache-worker-{i}"))
                    .spawn(move || worker_loop(&s))
                    .expect("spawn cache worker")
            })
            .collect();
        Self { shared, workers }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Queue `job` under `key`. If `key` is already queued or running, `job` is
    /// dropped, `on_done` joins the existing waiters, and a queued job's
    /// priority is raised to `prio` if higher.
    pub fn submit<F, C>(&self, key: K, prio: Priority, job: F, on_done: C) -> Submitted
    where
        F: FnOnce(&CancelToken) -> T + Send + 'static,
        C: FnOnce(JobResult<T>) + Send + 'static,
    {
        let mut st = self.shared.lock();
        if st.shutdown {
            drop(st);
            on_done(JobResult::Cancelled);
            return Submitted::Rejected;
        }
        if let Some(e) = st.entries.get_mut(&key) {
            e.waiters.push(Box::new(on_done));
            if e.job.is_some() && prio > e.prio {
                let (old, seq) = (e.prio, e.seq);
                e.prio = prio;
                st.queue.remove(&(Reverse(old), seq));
                st.queue.insert((Reverse(prio), seq), key);
            }
            return Submitted::Deduped;
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        st.entries.insert(
            key.clone(),
            Entry {
                generation: seq,
                prio,
                seq,
                job: Some(Box::new(job)),
                token: CancelToken::default(),
                waiters: vec![Box::new(on_done)],
            },
        );
        st.queue.insert((Reverse(prio), seq), key);
        drop(st);
        self.shared.work.notify_one();
        Submitted::New
    }

    /// Change a queued job's priority (up or down). Returns false if `key` is
    /// not queued (unknown or already running).
    pub fn reprioritize(&self, key: &K, prio: Priority) -> bool {
        let mut st = self.shared.lock();
        let State { entries, queue, .. } = &mut *st;
        match entries.get_mut(key) {
            Some(e) if e.job.is_some() => {
                if let Some(k) = queue.remove(&(Reverse(e.prio), e.seq)) {
                    e.prio = prio;
                    queue.insert((Reverse(prio), e.seq), k);
                }
                true
            }
            _ => false,
        }
    }

    /// Re-prioritize every queued job for which `f` returns a new priority
    /// (e.g. demote all `Visible` cells that scrolled out of view).
    pub fn reprioritize_where(&self, mut f: impl FnMut(&K, Priority) -> Option<Priority>) -> usize {
        let mut st = self.shared.lock();
        let State { entries, queue, .. } = &mut *st;
        let mut changed = 0;
        for (k, e) in entries.iter_mut().filter(|(_, e)| e.job.is_some()) {
            if let Some(p) = f(k, e.prio).filter(|p| *p != e.prio) {
                queue.remove(&(Reverse(e.prio), e.seq));
                e.prio = p;
                queue.insert((Reverse(p), e.seq), k.clone());
                changed += 1;
            }
        }
        changed
    }

    /// Cancel `key`: a queued job never runs; a running job's token is set and
    /// its eventual result is discarded. All waiters get `Cancelled` now.
    pub fn cancel(&self, key: &K) -> bool {
        self.cancel_where(|k, _| k == key) > 0
    }

    /// Cancel every queued or running job matching `pred`.
    pub fn cancel_where(&self, mut pred: impl FnMut(&K, Priority) -> bool) -> usize {
        let mut st = self.shared.lock();
        let doomed: Vec<K> = st
            .entries
            .iter()
            .filter(|(k, e)| pred(k, e.prio))
            .map(|(k, _)| k.clone())
            .collect();
        if doomed.is_empty() {
            return 0;
        }
        let mut waiters = Vec::new();
        for k in &doomed {
            let e = st.entries.remove(k).expect("collected above");
            st.queue.remove(&(Reverse(e.prio), e.seq));
            e.token.cancel();
            waiters.extend(e.waiters);
        }
        // Idle is only signalled once the waiters have been answered.
        st.delivering += 1;
        drop(st);
        deliver(waiters, &JobResult::Cancelled);
        let mut st = self.shared.lock();
        st.delivering -= 1;
        let idle = st.is_idle();
        drop(st);
        if idle {
            self.shared.idle.notify_all();
        }
        doomed.len()
    }

    /// Current priority of a queued job.
    pub fn queued_priority(&self, key: &K) -> Option<Priority> {
        self.shared
            .lock()
            .entries
            .get(key)
            .filter(|e| e.job.is_some())
            .map(|e| e.prio)
    }

    /// True while `key` is queued or running (and not cancelled).
    pub fn is_pending(&self, key: &K) -> bool {
        self.shared.lock().entries.contains_key(key)
    }

    /// Jobs waiting for a worker.
    pub fn queued(&self) -> usize {
        self.shared.lock().queue.len()
    }

    /// Block until nothing is queued or running and every waiter of the
    /// finished or cancelled jobs has returned.
    pub fn wait_idle(&self) {
        let mut st = self.shared.lock();
        while !st.is_idle() {
            st = self
                .shared
                .idle
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn worker_loop<K: Hash + Eq + Clone, T: Clone>(shared: &Shared<K, T>) {
    loop {
        let (key, generation, job, token) = {
            let mut st = shared.lock();
            let key = loop {
                if st.shutdown {
                    return;
                }
                if let Some((_, k)) = st.queue.pop_first() {
                    break k;
                }
                st = shared.work.wait(st).unwrap_or_else(PoisonError::into_inner);
            };
            st.running += 1;
            let e = st.entries.get_mut(&key).expect("queued key has an entry");
            let job = e.job.take().expect("queued entry has a job");
            (key, e.generation, job, e.token.clone())
        };
        let out = catch_unwind(AssertUnwindSafe(|| job(&token)));
        let waiters = {
            let mut st = shared.lock();
            // A cancelled entry was already removed (and maybe resubmitted under
            // a new generation); its waiters were told `Cancelled`.
            match st.entries.get(&key) {
                Some(e) if e.generation == generation => st
                    .entries
                    .remove(&key)
                    .map(|e| e.waiters)
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        let result = match out {
            Ok(_) if token.is_cancelled() => JobResult::Cancelled,
            Ok(v) => JobResult::Done(v),
            Err(_) => JobResult::Panicked,
        };
        deliver(waiters, &result);
        // Still counted as running until its waiters have returned, so
        // `wait_idle` cannot wake while results are in flight.
        let idle = {
            let mut st = shared.lock();
            st.running -= 1;
            st.is_idle()
        };
        if idle {
            shared.idle.notify_all();
        }
    }
}

impl<K, T> Drop for JobScheduler<K, T> {
    fn drop(&mut self) {
        let waiters: Vec<Callback<T>> = {
            let mut st = self.shared.lock();
            st.shutdown = true;
            st.queue.clear();
            st.entries
                .drain()
                .flat_map(|(_, e)| {
                    e.token.cancel();
                    e.waiters
                })
                .collect()
        };
        self.shared.work.notify_all();
        self.shared.idle.notify_all();
        for cb in waiters {
            let _ = catch_unwind(AssertUnwindSafe(move || cb(JobResult::Cancelled)));
        }
        // The last `Arc<JobScheduler>` can be dropped by a job's own closure,
        // i.e. on a worker thread: joining itself would deadlock, so that
        // worker is left to exit on its own (shutdown is already set).
        let me = std::thread::current().id();
        for w in self.workers.drain(..) {
            if w.thread().id() != me {
                let _ = w.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::mpsc::{Receiver, Sender, channel};

    type Sched = JobScheduler<u32, u32>;
    type Rec = (u32, JobResult<u32>);

    /// Occupy the single worker until the returned sender is used (or dropped).
    fn block_worker(s: &Sched, key: u32) -> Sender<()> {
        let (release_tx, release_rx) = channel::<()>();
        let (started_tx, started_rx) = channel::<()>();
        s.submit(
            key,
            Priority::Visible,
            move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                key
            },
            |_| {},
        );
        started_rx.recv().unwrap();
        release_tx
    }

    fn collector() -> (Sender<Rec>, Receiver<Rec>) {
        channel()
    }

    fn submit_rec(
        s: &Sched,
        key: u32,
        p: Priority,
        tx: &Sender<(u32, JobResult<u32>)>,
    ) -> Submitted {
        let tx = tx.clone();
        s.submit(
            key,
            p,
            move |_| key * 10,
            move |r| tx.send((key, r)).unwrap(),
        )
    }

    #[test]
    fn runs_by_priority_then_fifo() {
        let s = Sched::new(1);
        let gate = block_worker(&s, 0);
        let (tx, rx) = collector();
        submit_rec(&s, 1, Priority::Background, &tx);
        submit_rec(&s, 2, Priority::Prefetch, &tx);
        submit_rec(&s, 3, Priority::Visible, &tx);
        submit_rec(&s, 4, Priority::Prefetch, &tx);
        submit_rec(&s, 5, Priority::Visible, &tx);
        assert_eq!(s.queued(), 5);
        gate.send(()).unwrap();
        let order: Vec<u32> = (0..5).map(|_| rx.recv().unwrap().0).collect();
        assert_eq!(order, vec![3, 5, 2, 4, 1]);
        s.wait_idle();
        assert_eq!(s.queued(), 0);
    }

    #[test]
    fn delivers_results_and_dedupes_waiters() {
        let s = Sched::new(1);
        let gate = block_worker(&s, 0);
        let runs = Arc::new(Mutex::new(0));
        let (tx, rx) = collector();
        for _ in 0..3 {
            let runs = Arc::clone(&runs);
            let tx = tx.clone();
            let r = s.submit(
                7,
                Priority::Background,
                move |_| {
                    *runs.lock().unwrap() += 1;
                    70
                },
                move |r| tx.send((7, r)).unwrap(),
            );
            assert!(matches!(r, Submitted::New | Submitted::Deduped));
        }
        assert_eq!(s.queued(), 1);
        gate.send(()).unwrap();
        for _ in 0..3 {
            assert_eq!(rx.recv().unwrap(), (7, JobResult::Done(70)));
        }
        s.wait_idle();
        assert_eq!(*runs.lock().unwrap(), 1, "identical keys run once");
        assert!(!s.is_pending(&7));
    }

    #[test]
    fn dedupe_raises_priority_but_not_lowers() {
        let s = Sched::new(1);
        let gate = block_worker(&s, 0);
        let (tx, rx) = collector();
        submit_rec(&s, 1, Priority::Prefetch, &tx);
        submit_rec(&s, 2, Priority::Background, &tx);
        assert_eq!(
            submit_rec(&s, 2, Priority::Visible, &tx),
            Submitted::Deduped
        );
        assert_eq!(s.queued_priority(&2), Some(Priority::Visible));
        assert_eq!(
            submit_rec(&s, 2, Priority::Background, &tx),
            Submitted::Deduped
        );
        assert_eq!(s.queued_priority(&2), Some(Priority::Visible));
        gate.send(()).unwrap();
        let order: Vec<u32> = (0..4).map(|_| rx.recv().unwrap().0).collect();
        assert_eq!(order, vec![2, 2, 2, 1]);
    }

    #[test]
    fn dedupes_onto_running_job() {
        let s = Sched::new(1);
        let (release_tx, release_rx) = channel::<()>();
        let (started_tx, started_rx) = channel::<()>();
        let (tx, rx) = collector();
        let tx1 = tx.clone();
        s.submit(
            9,
            Priority::Visible,
            move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                90
            },
            move |r| tx1.send((9, r)).unwrap(),
        );
        started_rx.recv().unwrap();
        assert_eq!(s.queued_priority(&9), None, "running, not queued");
        assert!(!s.reprioritize(&9, Priority::Background));
        assert_eq!(
            submit_rec(&s, 9, Priority::Visible, &tx),
            Submitted::Deduped
        );
        release_tx.send(()).unwrap();
        assert_eq!(rx.recv().unwrap(), (9, JobResult::Done(90)));
        assert_eq!(rx.recv().unwrap(), (9, JobResult::Done(90)));
    }

    #[test]
    fn reprioritize_moves_queued_jobs() {
        let s = Sched::new(1);
        let gate = block_worker(&s, 0);
        let (tx, rx) = collector();
        for k in 1..=4 {
            submit_rec(&s, k, Priority::Visible, &tx);
        }
        // Scrolled away: 1 and 2 are no longer visible.
        let n = s.reprioritize_where(|k, p| {
            (p == Priority::Visible && *k <= 2).then_some(Priority::Background)
        });
        assert_eq!(n, 2);
        assert!(s.reprioritize(&4, Priority::Prefetch));
        assert!(!s.reprioritize(&99, Priority::Visible));
        // Same priority is not counted as a change.
        assert_eq!(s.reprioritize_where(|_, p| Some(p)), 0);
        gate.send(()).unwrap();
        let order: Vec<u32> = (0..4).map(|_| rx.recv().unwrap().0).collect();
        assert_eq!(order, vec![3, 4, 1, 2]);
    }

    #[test]
    fn cancel_queued_never_runs() {
        let s = Sched::new(1);
        let gate = block_worker(&s, 0);
        let (tx, rx) = collector();
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = Arc::clone(&ran);
        let tx1 = tx.clone();
        s.submit(
            1,
            Priority::Visible,
            move |_| {
                ran2.store(true, Ordering::SeqCst);
                1
            },
            move |r| tx1.send((1, r)).unwrap(),
        );
        submit_rec(&s, 2, Priority::Visible, &tx);
        submit_rec(&s, 3, Priority::Background, &tx);
        assert!(s.cancel(&1));
        assert!(!s.cancel(&1));
        assert_eq!(rx.recv().unwrap(), (1, JobResult::Cancelled));
        assert_eq!(s.cancel_where(|_, p| p == Priority::Background), 1);
        assert_eq!(rx.recv().unwrap(), (3, JobResult::Cancelled));
        gate.send(()).unwrap();
        assert_eq!(rx.recv().unwrap(), (2, JobResult::Done(20)));
        s.wait_idle();
        assert!(!ran.load(Ordering::SeqCst));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cancel_running_sets_token_and_discards_result() {
        let s = Sched::new(1);
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let (seen_tx, seen_rx) = channel::<bool>();
        let (tx, rx) = collector();
        let tx1 = tx.clone();
        s.submit(
            5,
            Priority::Visible,
            move |tok| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                seen_tx.send(tok.is_cancelled()).unwrap();
                50
            },
            move |r| tx1.send((5, r)).unwrap(),
        );
        started_rx.recv().unwrap();
        assert!(s.cancel(&5));
        assert_eq!(rx.recv().unwrap(), (5, JobResult::Cancelled));
        // Resubmitting the same key while the old one is still running creates a fresh job.
        let (tx2, rx2) = collector();
        assert_eq!(submit_rec(&s, 5, Priority::Visible, &tx2), Submitted::New);
        release_tx.send(()).unwrap();
        assert!(seen_rx.recv().unwrap(), "job observed cancellation");
        assert_eq!(rx2.recv().unwrap(), (5, JobResult::Done(50)));
        s.wait_idle();
        assert!(rx.try_recv().is_err(), "stale result discarded");
    }

    #[test]
    fn job_ignoring_cancel_still_reports_cancelled() {
        // A cancelled job that finishes is reported Cancelled to anyone who
        // attached before the cancel, even via the worker path.
        let s = Sched::new(1);
        let (tx, rx) = channel();
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        s.submit(
            1,
            Priority::Visible,
            move |tok: &CancelToken| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                // Simulate the token being tripped by shutdown/cancel.
                tok.cancel();
                1
            },
            move |r| tx.send(r).unwrap(),
        );
        started_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(rx.recv().unwrap(), JobResult::Cancelled);
    }

    #[test]
    fn panicking_job_and_callback_do_not_kill_worker() {
        let s = Sched::new(1);
        let (tx, rx) = collector();
        let tx1 = tx.clone();
        s.submit(
            1,
            Priority::Visible,
            |_| panic!("boom"),
            move |r| tx1.send((1, r)).unwrap(),
        );
        assert_eq!(rx.recv().unwrap(), (1, JobResult::Panicked));
        s.submit(2, Priority::Visible, |_| 2, |_| panic!("callback boom"));
        s.submit(3, Priority::Visible, |_| 2, |_| {});
        submit_rec(&s, 4, Priority::Visible, &tx);
        assert_eq!(rx.recv().unwrap(), (4, JobResult::Done(40)));
    }

    #[test]
    fn many_workers_run_in_parallel() {
        // All 4 jobs must be running at once to pass the barrier.
        let s = Sched::new(4);
        assert_eq!(s.worker_count(), 4);
        let barrier = Arc::new(Barrier::new(4));
        let (tx, rx) = collector();
        for k in 0..4 {
            let b = Arc::clone(&barrier);
            let tx = tx.clone();
            s.submit(
                k,
                Priority::Prefetch,
                move |_| {
                    b.wait();
                    k
                },
                move |r| tx.send((k, r)).unwrap(),
            );
        }
        let mut got: Vec<u32> = (0..4).map(|_| rx.recv().unwrap().0).collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2, 3]);
        s.wait_idle();
    }

    #[test]
    fn drop_cancels_queued_and_joins() {
        let (tx, rx) = collector();
        let s = Sched::new(1);
        // The running job is released by its own Cancelled callback, which Drop
        // delivers after marking shutdown and before joining the worker.
        let (release_tx, release_rx) = channel::<()>();
        let (started_tx, started_rx) = channel::<()>();
        s.submit(
            0,
            Priority::Visible,
            move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                0
            },
            move |r| {
                assert_eq!(r, JobResult::Cancelled);
                release_tx.send(()).unwrap();
            },
        );
        started_rx.recv().unwrap();
        submit_rec(&s, 1, Priority::Visible, &tx);
        submit_rec(&s, 2, Priority::Background, &tx);
        drop(s);
        let mut got: Vec<(u32, JobResult<u32>)> = rx.try_iter().collect();
        got.sort_by_key(|(k, _)| *k);
        assert_eq!(
            got,
            vec![(1, JobResult::Cancelled), (2, JobResult::Cancelled)]
        );
    }

    #[test]
    fn dropping_the_last_handle_from_a_job_does_not_self_join() {
        let s = Arc::new(Sched::new(1));
        let (gate_tx, gate_rx) = channel::<()>();
        let (done_tx, done_rx) = channel::<()>();
        let held = Arc::clone(&s);
        s.submit(
            0,
            Priority::Visible,
            move |_| {
                gate_rx.recv().unwrap();
                let _last = held; // dropped on the worker once main lets go
                7
            },
            move |r| {
                // Dropping the scheduler cancels the job that is dropping it.
                assert_eq!(r, JobResult::Cancelled);
                done_tx.send(()).unwrap();
            },
        );
        drop(s);
        gate_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker deadlocked joining itself");
    }

    #[test]
    fn submit_after_shutdown_is_rejected() {
        let s = Sched::new(1);
        s.shared.lock().shutdown = true;
        let (tx, rx) = collector();
        assert_eq!(
            submit_rec(&s, 1, Priority::Visible, &tx),
            Submitted::Rejected
        );
        assert_eq!(rx.recv().unwrap(), (1, JobResult::Cancelled));
    }

    #[test]
    fn wait_idle_returns_when_empty_and_after_cancel() {
        let s = Sched::new(1);
        s.wait_idle();
        let gate = block_worker(&s, 0);
        let (tx, _rx) = collector();
        submit_rec(&s, 1, Priority::Visible, &tx);
        assert!(s.cancel(&0));
        s.cancel(&1);
        gate.send(()).unwrap();
        s.wait_idle();
        assert_eq!(s.queued(), 0);
        assert!(format!("{s:?}").contains("workers: 1"));
    }

    /// A waiter callback is running and blocks until `release` is used. Until
    /// then `wait_idle` must not return: idle means every waiter has been
    /// answered, so a caller may inspect what the callbacks recorded.
    fn assert_wait_idle_waits_for_callback(s: &Arc<Sched>, release: &Sender<()>) {
        let (idle_tx, idle_rx) = channel::<()>();
        let s2 = Arc::clone(s);
        std::thread::spawn(move || {
            s2.wait_idle();
            idle_tx.send(()).unwrap();
        });
        assert!(
            idle_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "wait_idle returned while a waiter callback was still running"
        );
        release.send(()).unwrap();
        idle_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("wait_idle returns once the callback finishes");
    }

    fn blocking_callback() -> (Receiver<()>, Sender<()>, Callback<u32>) {
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let cb: Callback<u32> = Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        (started_rx, release_tx, cb)
    }

    #[test]
    fn wait_idle_waits_for_completed_job_callbacks() {
        let s = Arc::new(Sched::new(1));
        let (started, release, cb) = blocking_callback();
        s.submit(1, Priority::Visible, |_| 1, cb);
        started.recv().unwrap();
        assert_wait_idle_waits_for_callback(&s, &release);
    }

    #[test]
    fn wait_idle_waits_for_cancelled_callbacks() {
        let s = Arc::new(Sched::new(1));
        let gate = block_worker(&s, 0);
        let (started, release, cb) = blocking_callback();
        s.submit(1, Priority::Visible, |_| 1, cb);
        let s2 = Arc::clone(&s);
        let canceller = std::thread::spawn(move || s2.cancel(&1));
        // Job 1 was cancelled while queued; now let the worker go idle so only
        // the canceller's delivery is left.
        started.recv().unwrap();
        drop(gate);
        assert_wait_idle_waits_for_callback(&s, &release);
        assert!(canceller.join().unwrap());
    }

    #[test]
    fn core_count_helpers() {
        assert!(perf_cores() >= 1);
        assert_eq!(parse_core_count(b"10\n"), Some(10));
        assert_eq!(parse_core_count(b"0"), None);
        assert_eq!(parse_core_count(b"x"), None);
        assert_eq!(parse_core_count(&[0xff]), None);
        let s: JobScheduler<u8, u8> = JobScheduler::with_default_workers();
        assert_eq!(s.worker_count(), perf_cores());
    }

    #[test]
    fn stress_many_keys_all_waiters_answered() {
        let s: JobScheduler<u32, u32> = JobScheduler::new(4);
        let (tx, rx) = collector();
        let mut expected = 0;
        for i in 0..2_000u32 {
            let key = i % 300;
            let p = match i % 3 {
                0 => Priority::Background,
                1 => Priority::Prefetch,
                _ => Priority::Visible,
            };
            submit_rec(&s, key, p, &tx);
            expected += 1;
            if i % 7 == 0 {
                s.cancel(&((i * 13) % 300));
            }
        }
        s.wait_idle();
        drop(s);
        drop(tx);
        let got: Vec<_> = rx.iter().collect();
        assert_eq!(got.len(), expected, "every waiter gets exactly one answer");
        assert!(
            got.iter()
                .all(|(k, r)| matches!(r, JobResult::Cancelled) || *r == JobResult::Done(k * 10))
        );
    }
}
