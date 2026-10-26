//! When to archive: interval, debounce after changes, on demand, on session
//! close; exponential backoff on failure; one run at a time per target.
//!
//! Scheduling uses a monotonic clock only, so wall-clock jumps (NTP, sleep,
//! the user changing the time zone) cannot stall or storm the archive. Wall
//! time is used only for the status shown to the user.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::ArchiveError;

/// Time source. [`SystemClock`] in production, [`ManualClock`] in tests.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time, for display only.
    fn wall(&self) -> SystemTime;
    fn sleep(&self, d: Duration);
}

#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Deterministic clock: `sleep` advances time instantly.
#[derive(Debug)]
pub struct ManualClock {
    state: Mutex<(Duration, SystemTime)>,
}

impl ManualClock {
    pub fn new(wall: SystemTime) -> Self {
        Self {
            state: Mutex::new((Duration::ZERO, wall)),
        }
    }
    /// Advance both clocks.
    pub fn advance(&self, d: Duration) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.0 += d;
        s.1 += d;
    }
    /// Jump the wall clock only (monotonic time is unaffected).
    pub fn set_wall(&self, wall: SystemTime) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = wall;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).0
    }
    fn wall(&self) -> SystemTime {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Periodic check (default 10 min). It only runs rsync when there are
    /// pending changes, or when the periodic verify is due.
    pub interval: Duration,
    /// Run even without pending changes at least this often (default 24 h),
    /// to catch changes nobody reported.
    pub verify_interval: Duration,
    /// Directory for cross-process lock files (one per target). Default
    /// `~/Library/Application Support/Luxtable/locks`; `None` = in-process
    /// locking only.
    pub lock_dir: Option<PathBuf>,
    /// Quiet time after the last change before running (default 60 s).
    pub debounce: Duration,
    /// First retry delay after a failure (default 30 s), doubled per failure…
    pub backoff_base: Duration,
    /// …up to this (default 30 min).
    pub backoff_max: Duration,
    /// Longest sleep in [`Scheduler::drive`], bounding on-demand latency.
    pub poll: Duration,
    /// After another run holds the target (Busy), wait this long before
    /// trying again, even for "Sync now" (default 5 s): no busy loop.
    pub busy_backoff: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(600),
            verify_interval: Duration::from_secs(24 * 3600),
            lock_dir: default_lock_dir(),
            debounce: Duration::from_secs(60),
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(1800),
            poll: Duration::from_secs(1),
            busy_backoff: Duration::from_secs(5),
        }
    }
}

/// `~/Library/Application Support/Luxtable/locks` (from `$HOME`).
pub fn default_lock_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/Luxtable/locks"))
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Interval,
    Debounce,
    OnDemand,
    SessionClosed,
}

/// What [`Scheduler::run_if_due`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRecord {
    Succeeded(Trigger),
    Failed(Trigger, String),
    /// Target not mounted/reachable; retried with backoff.
    Unavailable(Trigger, String),
    /// Another run for the same target is in progress (this or another
    /// session, or another process).
    Busy(Trigger),
    /// The user cancelled: not a failure, no backoff, changes stay pending.
    Cancelled(Trigger),
    /// The pass ran, but these items are not in the archive (unreadable
    /// originals, a failed DB snapshot): a failure with backoff; the changes
    /// stay pending and the problems are shown until a clean run.
    Incomplete(Trigger, Vec<String>),
}

/// Status for the UI ("archive behind by N changes since …").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveStatus {
    pub last_success: Option<SystemTime>,
    pub last_error: Option<String>,
    pub pending_changes: u64,
    /// When the oldest un-archived change happened.
    pub behind_since: Option<SystemTime>,
    pub running: bool,
    pub consecutive_failures: u32,
    /// Monotonic delay until the next attempt is allowed (backoff), if any.
    pub retry_in: Option<Duration>,
    /// What the last run could not archive (`"<path>: <reason>"`); empty
    /// after a clean run.
    pub problems: Vec<String>,
}

/// A started run (see [`Scheduler::start`]); hand it back to
/// [`Scheduler::finish`] or [`Scheduler::mark_busy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    pub trigger: Trigger,
    pending_at_start: u64,
}

#[derive(Debug, Default)]
struct State {
    pending: u64,
    last_change: Option<Duration>,
    behind_since: Option<SystemTime>,
    /// Changes that arrived while a run was in progress.
    changes_during_run: Option<SystemTime>,
    last_attempt: Duration,
    last_success: Option<SystemTime>,
    last_success_mono: Option<Duration>,
    last_error: Option<String>,
    failures: u32,
    retry_at: Option<Duration>,
    /// Another run held the target: don't retry before this.
    busy_until: Option<Duration>,
    forced: Option<Trigger>,
    running: bool,
    problems: Vec<String>,
}

/// Archive scheduler for one (session, target) pair.
pub struct Scheduler {
    cfg: Mutex<SchedulerConfig>,
    clock: Arc<dyn Clock>,
    target_root: Mutex<PathBuf>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("cfg", &self.cfg)
            .field("target_root", &self.target_root)
            .finish_non_exhaustive()
    }
}

impl Scheduler {
    /// The first interval run is one interval from now.
    pub fn new(
        cfg: SchedulerConfig,
        clock: Arc<dyn Clock>,
        target_root: impl Into<PathBuf>,
    ) -> Self {
        let state = State {
            last_attempt: clock.monotonic(),
            ..State::default()
        };
        Self {
            cfg: Mutex::new(cfg),
            clock,
            target_root: Mutex::new(target_root.into()),
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cfg(&self) -> SchedulerConfig {
        self.cfg
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn target_root(&self) -> PathBuf {
        self.target_root
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// New settings (interval, target, …). Everything the scheduler knows
    /// stays: pending changes, last success, the last error, failures and
    /// backoff, a run in progress.
    pub fn reconfigure(&self, cfg: SchedulerConfig, target_root: impl Into<PathBuf>) {
        *self.cfg.lock().unwrap_or_else(PoisonError::into_inner) = cfg;
        *self
            .target_root
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = target_root.into();
    }

    /// Record `n` changed files (import, edit, sidecar write).
    pub fn note_changes(&self, n: u64) {
        if n == 0 {
            return;
        }
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        let mut s = self.lock();
        s.pending += n;
        s.last_change = Some(now);
        s.behind_since.get_or_insert(wall);
        if s.running {
            s.changes_during_run.get_or_insert(wall);
        }
    }

    /// Run as soon as possible, ignoring debounce and backoff.
    pub fn request_now(&self) {
        self.lock().forced.get_or_insert(Trigger::OnDemand);
    }

    /// The session is closing: archive now.
    pub fn session_closed(&self) {
        self.lock().forced = Some(Trigger::SessionClosed);
    }

    /// The trigger that would start a run now, if any.
    pub fn due(&self) -> Option<Trigger> {
        due(&self.cfg(), &self.lock(), self.clock.monotonic())
    }

    /// Time until something may become due (0 if due now).
    pub fn next_wakeup(&self) -> Duration {
        let cfg = self.cfg();
        let s = self.lock();
        let now = self.clock.monotonic();
        if let Some(b) = s.busy_until.filter(|b| *b > now) {
            return b.saturating_sub(now);
        }
        if s.forced.is_some() {
            return Duration::ZERO;
        }
        let interval_at = s.last_attempt + cfg.interval;
        let mut at = match s.last_change.filter(|_| s.pending > 0) {
            Some(c) => interval_at.min(c + cfg.debounce),
            // Nothing pending: next is the periodic verify.
            None => interval_at.max(
                s.last_success_mono
                    .map_or(Duration::ZERO, |t| t + cfg.verify_interval),
            ),
        };
        if let Some(r) = s.retry_at {
            at = at.max(r);
        }
        at.saturating_sub(now)
    }

    pub fn status(&self) -> ArchiveStatus {
        let s = self.lock();
        let now = self.clock.monotonic();
        ArchiveStatus {
            last_success: s.last_success,
            last_error: s.last_error.clone(),
            pending_changes: s.pending,
            behind_since: s.behind_since,
            running: s.running,
            consecutive_failures: s.failures,
            retry_in: s
                .retry_at
                .filter(|r| *r > now)
                .map(|r| r.saturating_sub(now)),
            problems: s.problems.clone(),
        }
    }

    /// Start a due run (marks it running), or `None`. The caller runs the
    /// job (anywhere, e.g. on a worker thread) and reports with
    /// [`Self::finish`], or [`Self::mark_busy`] when the target is taken.
    pub fn start(&self) -> Option<Ticket> {
        let cfg = self.cfg();
        let mut s = self.lock();
        let trigger = due(&cfg, &s, self.clock.monotonic())?;
        s.running = true;
        s.forced = None;
        s.last_attempt = self.clock.monotonic();
        s.changes_during_run = None;
        Some(Ticket {
            trigger,
            pending_at_start: s.pending,
        })
    }

    /// Another run holds the target: not a failure. A forced trigger is kept,
    /// and nothing retries for [`SchedulerConfig::busy_backoff`].
    pub fn mark_busy(&self, ticket: Ticket) -> RunRecord {
        let backoff = self.cfg().busy_backoff;
        let now = self.clock.monotonic();
        let mut s = self.lock();
        s.running = false;
        if matches!(ticket.trigger, Trigger::OnDemand | Trigger::SessionClosed) {
            s.forced.get_or_insert(ticket.trigger);
        }
        s.busy_until = Some(now + backoff);
        RunRecord::Busy(ticket.trigger)
    }

    /// Record how a started run ended.
    pub fn finish(&self, ticket: Ticket, result: Result<(), ArchiveError>) -> RunRecord {
        let cfg = self.cfg();
        let Ticket {
            trigger,
            pending_at_start,
        } = ticket;
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        let mut s = self.lock();
        s.running = false;
        s.busy_until = None;
        match result {
            Ok(()) => {
                s.last_success = Some(wall);
                s.last_success_mono = Some(now);
                s.last_error = None;
                s.problems.clear();
                s.failures = 0;
                s.retry_at = None;
                s.pending = s.pending.saturating_sub(pending_at_start);
                s.behind_since = if s.pending == 0 {
                    None
                } else {
                    s.changes_during_run.or(s.behind_since)
                };
                RunRecord::Succeeded(trigger)
            }
            Err(ArchiveError::Cancelled) => RunRecord::Cancelled(trigger),
            Err(e) => {
                s.failures = s.failures.saturating_add(1);
                let exp = s.failures.saturating_sub(1).min(20);
                let backoff = cfg
                    .backoff_base
                    .saturating_mul(1 << exp)
                    .min(cfg.backoff_max);
                s.retry_at = Some(now + backoff);
                s.last_error = Some(e.to_string());
                match e {
                    ArchiveError::TargetUnavailable { .. } => {
                        RunRecord::Unavailable(trigger, e.to_string())
                    }
                    ArchiveError::Incomplete { problems } => {
                        // The changes stay pending: this is retried until clean.
                        s.pending = s.pending.max(1);
                        s.last_change.get_or_insert(now);
                        s.behind_since.get_or_insert(wall);
                        s.problems.clone_from(&problems);
                        RunRecord::Incomplete(trigger, problems)
                    }
                    e => RunRecord::Failed(trigger, e.to_string()),
                }
            }
        }
    }

    /// Run `job` if a trigger is due. Holds a process-wide lock on the target
    /// root for the duration, so two sessions never rsync into the same
    /// target at once.
    pub fn run_if_due(
        &self,
        job: &mut dyn FnMut(Trigger) -> Result<(), ArchiveError>,
    ) -> Option<RunRecord> {
        let ticket = self.start()?;
        let lock_dir = self.cfg().lock_dir;
        let Some(_guard) = TargetLock::try_acquire(&self.target_root(), lock_dir.as_deref()) else {
            return Some(self.mark_busy(ticket));
        };
        let result = job(ticket.trigger);
        Some(self.finish(ticket, result))
    }

    /// Loop until `stop` is set: run when due, then sleep until the next
    /// deadline (at most `cfg.poll`). Run it on a background thread.
    pub fn drive(
        &self,
        job: &mut dyn FnMut(Trigger) -> Result<(), ArchiveError>,
        stop: &AtomicBool,
    ) {
        while !stop.load(Ordering::Relaxed) {
            if let Some(rec) = self.run_if_due(job) {
                tracing::debug!(?rec, "archive run");
            }
            let wait = self
                .next_wakeup()
                .min(self.cfg().poll)
                .max(Duration::from_millis(1));
            self.clock.sleep(wait);
        }
    }
}

fn due(cfg: &SchedulerConfig, s: &State, now: Duration) -> Option<Trigger> {
    if s.running || s.busy_until.is_some_and(|b| now < b) {
        return None;
    }
    if let Some(t) = s.forced {
        return Some(t);
    }
    if s.retry_at.is_some_and(|r| now < r) {
        return None;
    }
    if s.pending > 0
        && s.last_change
            .is_some_and(|c| now.saturating_sub(c) >= cfg.debounce)
    {
        return Some(Trigger::Debounce);
    }
    let verify_due = s
        .last_success_mono
        .is_none_or(|t| now.saturating_sub(t) >= cfg.verify_interval);
    if now.saturating_sub(s.last_attempt) >= cfg.interval && (s.pending > 0 || verify_due) {
        return Some(Trigger::Interval);
    }
    None
}

/// Single-flight lock per target root: a process-wide registry plus, when a
/// lock directory is given, an `flock`ed file so other processes (a second
/// app instance, a CLI) are excluded too.
#[derive(Debug)]
pub struct TargetLock {
    key: PathBuf,
    /// Held for its `Drop` (releases the flock).
    file: Option<lt_fsutil::DirLock>,
}

fn registry() -> &'static Mutex<HashSet<PathBuf>> {
    static LOCKS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    LOCKS.get_or_init(Mutex::default)
}

/// `root` with `.` and `..` resolved lexically. No file-system call: a lock
/// must never block on a stale network mount.
fn lexical_key(root: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in root.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

impl TargetLock {
    /// Take the target's lock, or `None` when another run holds it. Only the
    /// (local) lock directory is touched, never the target itself.
    pub fn try_acquire(root: &Path, lock_dir: Option<&Path>) -> Option<Self> {
        let key = lexical_key(root);
        // Not `then_some(Self { .. })`: that builds (and on failure drops) a
        // guard eagerly, whose Drop would release the *other* holder's lock.
        let inserted = registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone());
        if !inserted {
            return None;
        }
        let mut guard = Self { key, file: None };
        if let Some(dir) = lock_dir {
            let name = blake3::hash(guard.key.as_os_str().as_encoded_bytes()).to_hex();
            let path = dir.join(format!("{}.lock", &name[..16]));
            match lt_fsutil::DirLock::try_lock(&path) {
                Ok(Some(l)) => guard.file = Some(l),
                Ok(None) => return None, // another process; `guard` drop frees the registry
                // An unusable lock dir must not stop archiving forever.
                Err(e) => {
                    tracing::warn!(%e, ?path, "archive lock file unavailable; in-process lock only");
                }
            }
        }
        Some(guard)
    }
}

impl Drop for TargetLock {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn setup(root: &str) -> (Arc<ManualClock>, Scheduler) {
        let clock = Arc::new(ManualClock::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        ));
        let s = Scheduler::new(test_cfg(), clock.clone(), format!("/nonexistent/{root}"));
        (clock, s)
    }

    fn test_lock_dir() -> &'static Path {
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
    }

    fn test_cfg() -> SchedulerConfig {
        SchedulerConfig {
            lock_dir: Some(test_lock_dir().to_path_buf()),
            ..SchedulerConfig::default()
        }
    }

    #[allow(clippy::unnecessary_wraps)] // must match the job signature
    fn ok(_: Trigger) -> Result<(), ArchiveError> {
        Ok(())
    }

    fn unavailable(_: Trigger) -> Result<(), ArchiveError> {
        Err(ArchiveError::TargetUnavailable {
            root: "/Volumes/pics".into(),
            reason: "not mounted".into(),
        })
    }

    #[test]
    fn interval_fires_after_ten_minutes() {
        let (clock, s) = setup("interval");
        assert_eq!(s.due(), None);
        assert_eq!(s.next_wakeup(), 10 * MIN);
        clock.advance((10 * MIN).saturating_sub(Duration::from_secs(1)));
        assert_eq!(s.run_if_due(&mut ok), None);
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Interval))
        );
        assert_eq!(s.due(), None, "interval restarts from the attempt");
        assert!(s.status().last_success.is_some());
    }

    #[test]
    fn debounce_waits_for_quiet_and_clears_behind() {
        let (clock, s) = setup("debounce");
        s.note_changes(0);
        assert_eq!(s.status().pending_changes, 0);
        s.note_changes(3);
        let t0 = clock.wall();
        clock.advance(Duration::from_secs(50));
        s.note_changes(2); // keeps resetting the debounce
        assert_eq!(s.next_wakeup(), MIN);
        clock.advance(Duration::from_secs(59));
        assert_eq!(s.due(), None);
        clock.advance(Duration::from_secs(1));
        let st = s.status();
        assert_eq!((st.pending_changes, st.behind_since), (5, Some(t0)));
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
        let st = s.status();
        assert_eq!(
            (st.pending_changes, st.behind_since, st.last_error),
            (0, None, None)
        );
    }

    #[test]
    fn changes_during_a_run_stay_pending() {
        let (clock, s) = setup("during");
        s.note_changes(4);
        clock.advance(MIN);
        let mid = clock.wall();
        let mut job = |_| {
            s.note_changes(1);
            assert!(s.status().running);
            assert_eq!(s.due(), None, "no re-entry while running");
            Ok(())
        };
        assert_eq!(
            s.run_if_due(&mut job),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
        let st = s.status();
        assert_eq!(st.pending_changes, 1);
        assert_eq!(st.behind_since, Some(mid));
    }

    #[test]
    fn on_demand_and_close_bypass_backoff() {
        let (_clock, s) = setup("demand");
        s.request_now();
        assert_eq!(s.next_wakeup(), Duration::ZERO);
        assert_eq!(
            s.run_if_due(&mut unavailable).unwrap(),
            RunRecord::Unavailable(
                Trigger::OnDemand,
                "archive target /Volumes/pics unavailable: not mounted".into()
            )
        );
        assert!(s.status().retry_in.is_some());
        assert_eq!(s.due(), None, "in backoff");
        s.session_closed();
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::SessionClosed))
        );
        assert_eq!(s.status().consecutive_failures, 0);
    }

    #[test]
    fn exponential_backoff_is_capped() {
        let (clock, s) = setup("backoff");
        let cfg = SchedulerConfig::default();
        s.note_changes(1);
        clock.advance(MIN);
        let mut fail = |_| {
            Err(ArchiveError::Rsync {
                code: Some(23),
                stderr: "partial".into(),
            })
        };
        let mut delays = vec![];
        for _ in 0..8 {
            let rec = s.run_if_due(&mut fail).expect("due");
            assert!(
                matches!(rec, RunRecord::Failed(Trigger::Debounce, _)),
                "{rec:?}"
            );
            let wait = s.status().retry_in.unwrap();
            delays.push(wait.as_secs());
            assert_eq!(s.next_wakeup(), wait);
            clock.advance(wait.saturating_sub(Duration::from_secs(1)));
            assert_eq!(s.due(), None);
            clock.advance(Duration::from_secs(1));
        }
        assert_eq!(delays, [30, 60, 120, 240, 480, 960, 1800, 1800]);
        assert!(
            delays
                .iter()
                .all(|d| Duration::from_secs(*d) <= cfg.backoff_max)
        );
        let st = s.status();
        assert_eq!(st.consecutive_failures, 8);
        assert!(st.last_error.unwrap().contains("23"));
        assert_eq!(st.pending_changes, 1, "failures keep changes pending");
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
    }

    #[test]
    fn wall_clock_jumps_do_not_affect_scheduling() {
        let (clock, s) = setup("jump");
        s.note_changes(1);
        clock.set_wall(SystemTime::UNIX_EPOCH); // jump back ~57 years
        clock.advance(30 * Duration::from_secs(1));
        assert_eq!(s.due(), None);
        clock.set_wall(SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000)); // far future
        clock.advance(30 * Duration::from_secs(1));
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
        assert_eq!(s.status().last_success, Some(clock.wall()));
    }

    #[test]
    fn single_flight_per_target() {
        let (_c1, a) = setup("shared");
        let (c2, b) = setup("shared");
        a.request_now();
        b.request_now();
        let mut inner = None;
        let mut job = |_| {
            inner = b.run_if_due(&mut ok);
            Ok(())
        };
        assert_eq!(
            a.run_if_due(&mut job),
            Some(RunRecord::Succeeded(Trigger::OnDemand))
        );
        assert_eq!(inner, Some(RunRecord::Busy(Trigger::OnDemand)));
        assert!(!b.status().running);
        assert_eq!(b.due(), None, "busy backoff");
        c2.advance(Duration::from_secs(5));
        assert_eq!(
            b.due(),
            Some(Trigger::OnDemand),
            "on-demand request survives Busy"
        );
        assert_eq!(
            b.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::OnDemand))
        );
        // Different targets do not block each other.
        let t = TargetLock::try_acquire(Path::new("/nonexistent/x"), None).unwrap();
        assert!(TargetLock::try_acquire(Path::new("/nonexistent/y"), None).is_some());
        assert!(TargetLock::try_acquire(Path::new("/nonexistent/x"), None).is_none());
        drop(t);
        assert!(TargetLock::try_acquire(Path::new("/nonexistent/x"), None).is_some());
    }

    #[test]
    fn lock_file_excludes_other_processes() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/nonexistent/xproc");
        let a = TargetLock::try_acquire(root, Some(dir.path())).unwrap();
        // Simulate another process: same lock file, independent open → flock refuses.
        let lock_file = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(lt_fsutil::DirLock::try_lock(&lock_file).unwrap().is_none());
        drop(a);
        let other = lt_fsutil::DirLock::try_lock(&lock_file).unwrap().unwrap();
        // While the "other process" holds it, we are Busy, and the in-process
        // registry entry is released again.
        assert!(TargetLock::try_acquire(root, Some(dir.path())).is_none());
        drop(other);
        assert!(TargetLock::try_acquire(root, Some(dir.path())).is_some());
        // An unusable lock dir falls back to in-process locking.
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(TargetLock::try_acquire(root, Some(&file)).is_some());
        assert!(default_lock_dir().is_none_or(|d| d.ends_with("Luxtable/locks")));
    }

    #[test]
    fn cancel_is_not_a_failure() {
        let (_c, s) = setup("cancel");
        s.note_changes(2);
        s.request_now();
        let mut cancelled = |_| Err(ArchiveError::Cancelled);
        assert_eq!(
            s.run_if_due(&mut cancelled),
            Some(RunRecord::Cancelled(Trigger::OnDemand))
        );
        let st = s.status();
        assert_eq!(
            (st.consecutive_failures, st.retry_in, st.last_error),
            (0, None, None)
        );
        assert_eq!(st.pending_changes, 2);
    }

    #[test]
    fn nothing_pending_skips_runs_until_the_periodic_verify() {
        let (clock, s) = setup("idle");
        // First interval: never verified yet, so it runs.
        clock.advance(10 * MIN);
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Interval))
        );
        // Subsequent intervals with no changes do nothing…
        for _ in 0..10 {
            clock.advance(10 * MIN);
            assert_eq!(s.run_if_due(&mut ok), None);
        }
        assert!(s.next_wakeup() > Duration::ZERO, "no busy loop while idle");
        // …until a change arrives, or 24 h pass.
        s.note_changes(1);
        clock.advance(MIN);
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
        clock.advance(Duration::from_secs(24 * 3600));
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Interval))
        );
    }

    #[test]
    fn drive_loop_is_deterministic_with_manual_clock() {
        let clock = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        let cfg = SchedulerConfig {
            verify_interval: 20 * MIN,
            ..test_cfg()
        };
        let s = Scheduler::new(cfg, clock.clone(), "/nonexistent/drive");
        let stop = AtomicBool::new(false);
        let mut runs = vec![];
        s.note_changes(1);
        let mut job = |t| {
            runs.push((t, clock.monotonic()));
            if runs.len() == 3 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        };
        s.drive(&mut job, &stop);
        let times: Vec<_> = runs.iter().map(|(t, at)| (*t, at.as_secs())).collect();
        assert_eq!(
            times,
            [
                (Trigger::Debounce, 60),
                // Idle intervals at 660 and 1260 are skipped; verify is due
                // 20 min after the last success.
                (Trigger::Interval, 1260),
                (Trigger::Interval, 2460)
            ]
        );
    }

    /// New settings keep the failure state, pending changes and a run
    /// in progress.
    #[test]
    fn reconfigure_keeps_the_state() {
        let (clock, s) = setup("reconf");
        s.note_changes(2);
        clock.advance(MIN);
        let mut fail = |_| {
            Err(ArchiveError::Rsync {
                code: Some(11),
                stderr: "No space left on device".into(),
            })
        };
        assert!(matches!(
            s.run_if_due(&mut fail),
            Some(RunRecord::Failed(..))
        ));
        let before = s.status();
        s.reconfigure(
            SchedulerConfig {
                interval: 30 * MIN,
                ..test_cfg()
            },
            "/nonexistent/reconf-2",
        );
        assert_eq!(s.status(), before);
        assert!(before.last_error.is_some() && before.consecutive_failures == 1);
        // A run in progress stays running across a reconfigure.
        s.request_now();
        let t = s.start().unwrap();
        s.reconfigure(test_cfg(), "/nonexistent/reconf");
        assert!(s.status().running);
        assert_eq!(s.finish(t, Ok(())), RunRecord::Succeeded(Trigger::OnDemand));
        assert!(!s.status().running && s.status().last_error.is_none());
    }

    /// An incomplete pass is a failure: the problems are kept (shown),
    /// changes stay pending, it is retried with backoff; a clean run clears it.
    #[test]
    fn incomplete_runs_keep_problems_and_pending() {
        let (clock, s) = setup("incomplete");
        s.request_now();
        let mut partial = |_| {
            Err(ArchiveError::Incomplete {
                problems: vec!["Capture/a.ARW: Permission denied".into()],
            })
        };
        let rec = s.run_if_due(&mut partial).unwrap();
        assert_eq!(
            rec,
            RunRecord::Incomplete(
                Trigger::OnDemand,
                vec!["Capture/a.ARW: Permission denied".into()]
            )
        );
        let st = s.status();
        assert_eq!(st.problems, ["Capture/a.ARW: Permission denied"]);
        assert!(st.pending_changes >= 1 && st.behind_since.is_some());
        assert!(st.retry_in.is_some() && st.last_success.is_none());
        assert!(st.last_error.unwrap().contains("1 item not archived"));
        clock.advance(st.retry_in.unwrap() + MIN);
        assert_eq!(
            s.run_if_due(&mut ok),
            Some(RunRecord::Succeeded(Trigger::Debounce))
        );
        let st = s.status();
        assert!(st.problems.is_empty() && st.pending_changes == 0 && st.last_error.is_none());
        assert!(
            ArchiveError::Incomplete {
                problems: vec!["a".into(), "b".into()]
            }
            .to_string()
            .starts_with("2 items not archived")
        );
    }

    /// When the target is busy (another run, another process), even a
    /// forced run waits the busy backoff instead of spinning.
    #[test]
    fn busy_backs_off_even_for_forced_runs() {
        let (clock, s) = setup("busy");
        s.request_now();
        let t = s.start().unwrap();
        assert_eq!(s.mark_busy(t), RunRecord::Busy(Trigger::OnDemand));
        assert_eq!(s.due(), None, "no immediate retry");
        assert_eq!(s.next_wakeup(), Duration::from_secs(5));
        clock.advance(Duration::from_secs(5));
        assert_eq!(s.due(), Some(Trigger::OnDemand), "the forced run survives");
        assert_eq!(
            lexical_key(Path::new("/Volumes/pics/./x/../Luxtable")),
            PathBuf::from("/Volumes/pics/Luxtable")
        );
    }

    #[test]
    fn system_clock_and_debug() {
        let c = SystemClock::default();
        let a = c.monotonic();
        c.sleep(Duration::from_millis(1));
        assert!(c.monotonic() > a);
        assert!(c.wall() > SystemTime::UNIX_EPOCH);
        let (_, s) = setup("dbg");
        assert!(format!("{s:?}").contains("target_root"));
    }
}
