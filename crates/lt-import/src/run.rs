//! Execute an [`ImportPlan`]: verified, resumable, bounded-parallel copy.
//!
//! Per file:
//! 1. **Resume first.** Every destination root (primary, backup) is checked
//!    on its own: a present file must re-hash to the source's BLAKE3. Our own
//!    torn copies (from a crash) are moved to `.lt/quarantine/`, never
//!    trusted and never deleted; a stranger's file is never touched.
//! 2. **Dedupe** (per file, sidecars included) only for files no earlier run
//!    started, and only against a library copy that exists and re-hashes.
//! 3. **Copy** to the roots that still need it, in one read pass:
//!    read chunk → write `.partial`s → BLAKE3 (on a helper thread); set mtime,
//!    fsync, re-read each partial with the page cache bypassed, compare, and
//!    rename into place without replacing anything. A backup failure never
//!    fails the primary.
//! 4. **Checkpoint.** Every ~500 ms (and at the end): `F_FULLFSYNC` the
//!    destination devices, *then* mark the files `Done` in the journal, *then*
//!    tell the [`SessionSink`]. Nothing is reported before it is durable.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use lt_core::FileHash;

use crate::ImportError;
use crate::dedupe::{PartialHash, PartialHasher, find_duplicate, full_hash};
use crate::fsio;
use crate::journal::{EntryState, Journal, JournalEntry, SourceKey, journal_path, lt_dir};
use crate::plan::{ImportPlan, PlannedFile};
use crate::scan::FileKind;

/// Minimum interval between progress callbacks for in-file byte updates.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Minimum interval between durability checkpoints while copying.
const CHECKPOINT_EVERY: Duration = Duration::from_millis(500);
/// Kept free on a destination volume beyond the files in flight (file-system
/// metadata, the journal).
const SPACE_MARGIN: u64 = 1 << 20;
/// How long a busy destination lock is retried before the run reports
/// [`ImportError::Busy`]. A process spawned at the wrong moment (this app runs
/// rsync, hdiutil…) shares a just-released lock's open file description until
/// it execs, so a lock can look held for a moment after its holder let go.
const LOCK_GRACE: Duration = Duration::from_secs(1);
/// Start of the reason for a file the [`SessionSink`] refused.
pub const NOT_REGISTERED: &str = "copied and verified, but not added to the session";

/// A snapshot of import progress.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: usize,
    pub files_total: usize,
    /// Source file that produced this event.
    pub current: Option<PathBuf>,
    pub bytes_per_sec: f64,
    pub eta: Option<Duration>,
}

/// A file that is durably in the destination, hash-verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedFile {
    pub src: PathBuf,
    pub dest: PathBuf,
    /// The backup copy, if one was requested *and* succeeded.
    pub backup: Option<PathBuf>,
    pub size: u64,
    pub kind: FileKind,
    pub hash: FileHash,
    pub partial: PartialHash,
    /// The primary copy was already in place from an earlier run.
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDuplicate {
    pub src: PathBuf,
    pub existing: FileHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedFile {
    pub src: PathBuf,
    pub reason: String,
}

/// Outcome of [`run`]. `copied` holds the verified hashes for the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub copied: Vec<ImportedFile>,
    pub skipped_duplicates: Vec<SkippedDuplicate>,
    pub failed: Vec<FailedFile>,
    /// Primary copy fine, backup copy failed (retried on resume).
    pub backup_failed: Vec<FailedFile>,
    /// Our own torn files moved aside before re-copying.
    pub quarantined: Vec<PathBuf>,
    pub cancelled: bool,
    /// The source volume disappeared (card pulled). Resume later.
    pub source_lost: Option<String>,
    /// The final durability flush failed; affected files are in `failed`.
    pub durability_error: Option<String>,
    /// Parts of the card the scan could not read (from [`ImportPlan::scan_errors`]).
    pub scan_errors: Vec<crate::scan::ScanError>,
    /// Card files deliberately not imported (from [`ImportPlan::skipped`]).
    pub skipped_unknown: Vec<crate::scan::SkippedFile>,
    pub bytes_copied: u64,
    pub elapsed: Duration,
}

impl ImportReport {
    /// Every file verified and durable, nothing left to resume: safe to
    /// offer "eject card".
    pub fn is_complete(&self) -> bool {
        !self.cancelled
            && self.failed.is_empty()
            && self.backup_failed.is_empty()
            && self.source_lost.is_none()
            && self.durability_error.is_none()
            && self.scan_errors.is_empty()
    }

    /// The UI must show [`Self::scan_errors`] and [`Self::skipped_unknown`]
    /// and get an explicit "I understand" before offering to eject the card.
    pub fn eject_needs_confirmation(&self) -> bool {
        !self.is_complete() || !self.skipped_unknown.is_empty()
    }
}

/// Receives each verified, durable file (e.g. the session adds the original
/// to its index). Called from worker threads. A file counts as imported only
/// once the sink took it: `Err(reason)` (or a panic) reports the file failed,
/// so the import is not complete and its journal stays; a resume delivers it
/// again (as `resumed`).
pub trait SessionSink: Send + Sync {
    fn imported(&self, file: &ImportedFile) -> Result<(), String>;
}

/// Test seam for I/O faults (card pulled, disk full, bit rot) and for
/// simulating slow cards. Every hook defaults to "no fault".
pub trait FaultInjector: Send + Sync {
    /// Before reading the source at `offset`.
    fn before_read(&self, _src: &Path, _offset: u64) -> io::Result<()> {
        Ok(())
    }
    /// Before writing a chunk to a destination partial at `offset`.
    fn before_write(&self, _partial: &Path, _offset: u64) -> io::Result<()> {
        Ok(())
    }
    /// After fsync, before verification re-reads `partial`.
    fn before_verify(&self, _partial: &Path) {}
    /// Before a durability checkpoint (`F_FULLFSYNC`).
    fn before_flush(&self) -> io::Result<()> {
        Ok(())
    }
    /// Free bytes on the volume of a destination root, already net of any
    /// safety margin (`None`: ask the file system).
    fn free_bytes(&self, _root: &Path) -> Option<io::Result<u64>> {
        None
    }
}

/// Optional collaborators for [`run_with`].
#[derive(Default, Clone, Copy)]
pub struct RunHooks<'a> {
    pub sink: Option<&'a dyn SessionSink>,
    pub faults: Option<&'a dyn FaultInjector>,
}

impl std::fmt::Debug for RunHooks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHooks")
            .field("sink", &self.sink.is_some())
            .field("faults", &self.faults.is_some())
            .finish()
    }
}

/// Copy everything in `plan`. See [`run_with`].
pub fn run(
    plan: &ImportPlan,
    progress: &(dyn Fn(&ImportProgress) + Sync),
    cancel: &AtomicBool,
) -> Result<ImportReport, ImportError> {
    run_with(plan, RunHooks::default(), progress, cancel)
}

/// Lock the destination `root` for this run, retrying a busy lock for up to
/// `grace` (see [`LOCK_GRACE`]) unless `cancel` is set; still busy after that
/// is [`ImportError::Busy`]. `on_busy` runs before each wait.
fn lock_destination(
    root: &Path,
    grace: Duration,
    cancel: &AtomicBool,
    mut on_busy: impl FnMut(),
) -> Result<lt_fsutil::DirLock, ImportError> {
    let deadline = Instant::now() + grace;
    loop {
        match lt_fsutil::DirLock::try_lock_dir(root, "import")
            .map_err(|e| ImportError::io(root, e))?
        {
            Some(l) => return Ok(l),
            None if Instant::now() < deadline && !cancel.load(Ordering::Relaxed) => {
                on_busy();
                std::thread::sleep(Duration::from_millis(10));
            }
            None => return Err(ImportError::Busy(root.to_path_buf())),
        }
    }
}

/// Copy everything in `plan` with bounded concurrency.
///
/// Per-file failures are reported in the [`ImportReport`] and never abort
/// other files. A full destination stops scheduling new files; a removed card
/// stops the run with one [`ImportReport::source_lost`] message (the rest
/// stays pending for resume). `Err` is reserved for the destination itself:
/// it cannot be created, another import holds its lock ([`ImportError::Busy`]),
/// or the journal cannot be read/written before copying starts.
pub fn run_with(
    plan: &ImportPlan,
    hooks: RunHooks<'_>,
    progress: &(dyn Fn(&ImportProgress) + Sync),
    cancel: &AtomicBool,
) -> Result<ImportReport, ImportError> {
    let start = Instant::now();
    // Each destination root: must be on a mounted volume (a missing backup
    // drive must not turn into a folder on the internal disk), locked for
    // this run (directory flock: no lock file to delete or leave behind),
    // and held open so the durability flush goes to *that* volume.
    let mut locks = Vec::new();
    let mut roots = Vec::new();
    for root in std::iter::once(&plan.dest).chain(&plan.backup_dest) {
        lt_fsutil::require_mounted(root).map_err(|e| ImportError::io(root, e))?;
        fs::create_dir_all(root).map_err(|e| ImportError::io(root, e))?;
        let mut waits = 0;
        let l = lock_destination(root, LOCK_GRACE, cancel, || waits += 1)?;
        if waits > 0 {
            tracing::debug!(?root, waits, "destination lock was busy for a moment");
        }
        locks.push(l);
        roots.push(lt_fsutil::RootHandle::open(root).map_err(|e| ImportError::io(root, e))?);
    }

    // What earlier runs did (any card), before this run changes anything.
    let mut prior: HashMap<SourceKey, EntryState> = HashMap::new();
    for (_, j) in Journal::load_all(&plan.dest)? {
        for e in j.entries {
            prior.insert(e.key, e.state);
        }
    }
    // Record the whole plan before copying the first byte.
    let jpath = journal_path(&plan.dest, &plan.journal_id);
    let mut journal = Journal::load(&jpath)?.unwrap_or_default();
    journal.backup_dest.clone_from(&plan.backup_dest);
    for f in plan.items.iter().flat_map(|i| &i.files) {
        match journal.entries.iter_mut().find(|e| e.key == f.key) {
            Some(e) => e.dest_rel.clone_from(&f.rel),
            None => journal.entries.push(JournalEntry {
                key: f.key.clone(),
                dest_rel: f.rel.clone(),
                state: prior.get(&f.key).cloned().unwrap_or(EntryState::Pending),
            }),
        }
    }
    journal.save(&jpath)?;

    let state = RunState {
        plan,
        hooks,
        progress,
        cancel,
        start,
        prior,
        roots,
        checkpointing: Mutex::new(()),
        ledger: Mutex::new(Ledger {
            journal,
            path: jpath.clone(),
            queued: Vec::new(),
            last: Instant::now(),
        }),
        report: Mutex::new(ImportReport::default()),
        next: AtomicUsize::new(0),
        bytes_done: AtomicU64::new(0),
        files_done: AtomicUsize::new(0),
        bytes_total: plan.total_bytes(),
        files_total: plan.file_count(),
        last_progress_ms: AtomicU64::new(0),
        dest_full: AtomicBool::new(false),
        source_lost: Mutex::new(None),
        space: Mutex::new(Space::default()),
        space_freed: Condvar::new(),
    };

    std::thread::scope(|s| {
        for _ in 0..plan.concurrency.min(plan.items.len()).max(1) {
            s.spawn(|| state.worker());
        }
    });

    // Final checkpoint: nothing is reported before the drive caches are flushed.
    let final_flush = state.checkpoint(true);
    let RunState {
        report,
        source_lost,
        ledger,
        ..
    } = state;
    let mut report = report.into_inner().unwrap_or_else(PoisonError::into_inner);
    report.cancelled = cancel.load(Ordering::Relaxed);
    report.source_lost = source_lost
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    report.scan_errors.clone_from(&plan.scan_errors);
    report.skipped_unknown.clone_from(&plan.skipped);
    if let Err(e) = final_flush {
        let ledger = ledger.into_inner().unwrap_or_else(PoisonError::into_inner);
        for q in ledger.queued {
            report.failed.push(FailedFile {
                src: q.file.src,
                reason: format!("copied and verified but not confirmed durable: {e}"),
            });
        }
        report.durability_error = Some(e);
    }
    if report.is_complete() {
        lt_fsutil::remove_if_exists(&jpath).map_err(|e| ImportError::io(&jpath, e))?;
        // Leave no stray `.lt/` behind (only removed when empty).
        let _ = fs::remove_dir(lt_dir(&plan.dest));
    }
    drop(locks);
    report.elapsed = start.elapsed();
    Ok(report)
}

/// A verified copy waiting for the next durability checkpoint.
struct Queued {
    key: SourceKey,
    state: EntryState,
    file: ImportedFile,
    copied_bytes: u64,
}

struct Ledger {
    journal: Journal,
    path: PathBuf,
    queued: Vec<Queued>,
    last: Instant,
}

impl Ledger {
    fn set(&mut self, key: &SourceKey, state: EntryState) {
        if let Some(e) = self.journal.entries.iter_mut().find(|e| &e.key == key) {
            e.state = state;
        }
    }
}

struct RunState<'a> {
    plan: &'a ImportPlan,
    hooks: RunHooks<'a>,
    progress: &'a (dyn Fn(&ImportProgress) + Sync),
    cancel: &'a AtomicBool,
    start: Instant,
    prior: HashMap<SourceKey, EntryState>,
    ledger: Mutex<Ledger>,
    /// Serializes durability checkpoints (see [`RunState::checkpoint`]).
    checkpointing: Mutex<()>,
    report: Mutex<ImportReport>,
    next: AtomicUsize,
    bytes_done: AtomicU64,
    files_done: AtomicUsize,
    bytes_total: u64,
    files_total: usize,
    last_progress_ms: AtomicU64,
    dest_full: AtomicBool,
    source_lost: Mutex<Option<String>>,
    roots: Vec<lt_fsutil::RootHandle>,
    space: Mutex<Space>,
    space_freed: Condvar,
}

/// Bytes promised to files in flight, per destination root (admission control).
#[derive(Default)]
struct Space {
    reserved: HashMap<PathBuf, u64>,
    in_flight: usize,
}

/// Whether a file may start.
enum Admit {
    /// Reserved these bytes on these roots (released after the copy).
    Go(Vec<(PathBuf, u64)>),
    Stopped,
    NoSpace(String),
}

/// Why a single file did not copy.
enum CopyFail {
    Cancelled,
    SourceLost(String),
    Failed(String, io::ErrorKind),
}

/// A destination root for one file.
#[derive(Clone)]
struct Target {
    root: PathBuf,
    fin: PathBuf,
    primary: bool,
}

impl RunState<'_> {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || lock(&self.source_lost).is_some()
    }

    fn worker(&self) {
        loop {
            if self.stopped() {
                return;
            }
            let idx = self.next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = self.plan.items.get(idx) else {
                return;
            };
            for f in &item.files {
                if self.stopped() {
                    return;
                }
                if self.dest_full.load(Ordering::Relaxed) {
                    self.fail(f, "not attempted: destination is full".into());
                    continue;
                }
                let reserved = match self.admit(f) {
                    Admit::Go(r) => r,
                    Admit::Stopped => return,
                    Admit::NoSpace(reason) => {
                        self.fail(f, reason);
                        continue;
                    }
                };
                let result = self.import_file(f);
                self.release(&reserved);
                match result {
                    Ok(()) | Err(CopyFail::Cancelled) => {}
                    Err(CopyFail::SourceLost(msg)) => {
                        lock(&self.source_lost).get_or_insert(msg);
                    }
                    Err(CopyFail::Failed(reason, kind)) => {
                        if matches!(
                            kind,
                            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
                        ) {
                            self.dest_full.store(true, Ordering::Relaxed);
                        }
                        self.fail(f, reason);
                    }
                }
            }
            if lock(&self.ledger).last.elapsed() >= CHECKPOINT_EVERY {
                // A failed intermediate checkpoint is retried later; the final
                // one reports it.
                if let Err(e) = self.checkpoint(false) {
                    tracing::warn!(%e, "import checkpoint failed");
                }
            }
        }
    }

    /// Free bytes on `root`'s volume (after [`SPACE_MARGIN`]); `None` when unknown.
    fn free_bytes(&self, root: &Path) -> Option<u64> {
        if let Some(r) = self.hooks.faults.and_then(|fi| fi.free_bytes(root)) {
            return r.ok();
        }
        let st = rustix::fs::statvfs(root).ok()?;
        Some(
            st.f_bavail
                .saturating_mul(st.f_frsize)
                .saturating_sub(SPACE_MARGIN),
        )
    }

    /// Start `f` only while every root that still needs it has room for it on
    /// top of the files in flight; otherwise wait for those to finish. With
    /// nothing in flight and still no room, the file fails (smaller files may
    /// still fit). Concurrent copies therefore never starve each other.
    fn admit(&self, f: &PlannedFile) -> Admit {
        let need: Vec<PathBuf> = std::iter::once(&self.plan.dest)
            .chain(&self.plan.backup_dest)
            .filter(|root| fs::symlink_metadata(root.join(&f.rel)).is_err())
            .cloned()
            .collect();
        let size = f.src.size;
        let mut s = lock(&self.space);
        loop {
            if self.stopped() {
                return Admit::Stopped;
            }
            let short = need.iter().find_map(|root| {
                let free = self.free_bytes(root)?;
                let want = s.reserved.get(root).copied().unwrap_or(0) + size;
                (free < want).then(|| (root.clone(), free))
            });
            match short {
                None => {
                    for root in &need {
                        *s.reserved.entry(root.clone()).or_default() += size;
                    }
                    s.in_flight += 1;
                    return Admit::Go(need.into_iter().map(|r| (r, size)).collect());
                }
                Some((root, free)) if s.in_flight == 0 => {
                    return Admit::NoSpace(format!(
                        "not enough free space on {}: needs {size} bytes, {free} free",
                        root.display()
                    ));
                }
                Some(_) => {
                    s = self
                        .space_freed
                        .wait_timeout(s, Duration::from_millis(250))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    fn release(&self, reserved: &[(PathBuf, u64)]) {
        let mut s = lock(&self.space);
        for (root, n) in reserved {
            if let Some(r) = s.reserved.get_mut(root) {
                *r = r.saturating_sub(*n);
            }
        }
        s.in_flight = s.in_flight.saturating_sub(1);
        drop(s);
        self.space_freed.notify_all();
    }

    fn import_file(&self, f: &PlannedFile) -> Result<(), CopyFail> {
        let targets: Vec<Target> = std::iter::once((&self.plan.dest, true))
            .chain(self.plan.backup_dest.iter().map(|b| (b, false)))
            .map(|(root, primary)| Target {
                root: root.clone(),
                fin: root.join(&f.rel),
                primary,
            })
            .collect();
        let prior = self.prior.get(&f.key);
        // Ours: an earlier run of ours planned this file (and may have left a
        // placed-but-failed or torn copy at its name).
        let ours = matches!(
            prior,
            Some(EntryState::Pending | EntryState::Done { .. } | EntryState::Failed { .. })
        );
        // Partials are removed up front only when our journal recorded this
        // file. Any other partial at that name is left alone unless it is
        // stale (untouched for `STALE_TEMP_AGE`, see `copy_verified`); a fresh
        // one fails the copy instead of being touched.
        if ours {
            for t in &targets {
                let p = fsio::partial_path(&t.fin);
                lt_fsutil::remove_if_exists(&p).map_err(|e| io_fail("removing stale", &p, &e))?;
            }
        }
        let mut known: Option<(FileHash, PartialHash)> = match prior {
            Some(EntryState::Done { hash, partial, .. }) => FileHash::from_hex(hash)
                .zip(FileHash::from_hex(partial))
                .map(|(h, p)| (h, PartialHash(p.0))),
            _ => None,
        };

        // 1. Resume: verify what is already in place, per root.
        let (need, backup_skip) = self.verify_existing(f, &targets, ours, &mut known)?;

        // 2. Dedupe: only files no earlier run started, only when the primary
        //    copy is still needed, only against a verifiable library copy.
        let primary_needed = need.iter().any(|t| t.primary);
        if primary_needed && !ours && self.is_duplicate(f, &targets)? {
            return Ok(());
        }

        // 3. Copy to the roots that need it.
        let mut backup_err = backup_skip;
        let (hash, partial) = if need.is_empty() {
            known.expect("present roots were verified against a hash")
        } else {
            let (h, p, errs) = self.copy_verified(f, &need)?;
            if let Some((kh, _)) = known
                && kh != h
            {
                return Err(CopyFail::Failed(
                    format!(
                        "source {} changed since it was first imported",
                        f.src.path.display()
                    ),
                    io::ErrorKind::InvalidData,
                ));
            }
            if let Some(e) = errs.into_iter().flatten().next() {
                backup_err = Some(e);
            }
            self.post_copy_checks(f, &need, h)?;
            (h, p)
        };
        let copied_bytes = if primary_needed { f.src.size } else { 0 };
        // A backup-only copy already counted its bytes in the copy loop.
        if need.is_empty() {
            self.bytes_done.fetch_add(f.src.size, Ordering::Relaxed);
        }
        let backup = targets.iter().find(|t| !t.primary);
        if let Some(reason) = &backup_err {
            lock(&self.report).backup_failed.push(FailedFile {
                src: f.src.path.clone(),
                reason: reason.clone(),
            });
        }
        let file = ImportedFile {
            src: f.src.path.clone(),
            dest: targets[0].fin.clone(),
            backup: backup
                .filter(|_| backup_err.is_none())
                .map(|t| t.fin.clone()),
            size: f.src.size,
            kind: f.src.kind,
            hash,
            partial,
            resumed: !primary_needed,
        };
        let state = EntryState::Done {
            hash: hash.to_hex(),
            partial: partial.to_hex(),
            backup_ok: backup.is_none() || backup_err.is_none(),
        };
        lock(&self.ledger).queued.push(Queued {
            key: f.key.clone(),
            state,
            file,
            copied_bytes,
        });
        self.account(0, 1, &f.src.path);
        Ok(())
    }

    /// Check each root's existing file against the source hash. Returns the
    /// roots that still need a copy, plus a reason if a stranger's file
    /// blocks the backup.
    fn verify_existing(
        &self,
        f: &PlannedFile,
        targets: &[Target],
        ours: bool,
        known: &mut Option<(FileHash, PartialHash)>,
    ) -> Result<(Vec<Target>, Option<String>), CopyFail> {
        let mut need = Vec::new();
        let mut backup_skip = None;
        for t in targets {
            let Ok(meta) = fs::symlink_metadata(&t.fin) else {
                need.push(t.clone());
                continue;
            };
            let src_hash = if let Some((h, _)) = *known {
                h
            } else {
                let k = self.hash_source(f)?;
                *known = Some(k);
                k.0
            };
            let matches = meta.is_file()
                && meta.len() == f.src.size
                && full_hash(&t.fin).map_err(|e| io_fail("verifying existing", &t.fin, &e))?
                    == src_hash;
            if matches {
                continue;
            }
            if ours {
                // Our own torn copy (crash/power loss): move aside, copy again.
                let q = fsio::quarantine(&t.root, &f.rel)
                    .map_err(|e| io_fail("quarantining", &t.fin, &e))?;
                lock(&self.report).quarantined.push(q);
                need.push(t.clone());
            } else if t.primary {
                return Err(CopyFail::Failed(
                    format!(
                        "destination {} exists with different content; not overwritten",
                        t.fin.display()
                    ),
                    io::ErrorKind::AlreadyExists,
                ));
            } else {
                backup_skip = Some(format!(
                    "backup {} exists with different content; not overwritten",
                    t.fin.display()
                ));
            }
        }
        Ok((need, backup_skip))
    }

    /// Dedupe one file (sidecars are judged on their own bytes). Records the
    /// skip and returns `true` when the library verifiably holds it.
    fn is_duplicate(&self, f: &PlannedFile, targets: &[Target]) -> Result<bool, CopyFail> {
        let Some(lookup) = &self.plan.dedupe else {
            return Ok(false);
        };
        if let Some(fi) = self.hooks.faults {
            fi.before_read(&f.src.path, 0)
                .map_err(|e| source_err(f, "reading for duplicate check", &e))?;
        }
        let ignore: Vec<PathBuf> = targets.iter().map(|t| t.fin.clone()).collect();
        match find_duplicate(&f.src.path, f.src.size, lookup.as_ref(), &ignore) {
            Ok(Some(existing)) => {
                lock(&self.ledger).set(
                    &f.key,
                    EntryState::Duplicate {
                        existing: existing.to_hex(),
                    },
                );
                lock(&self.report)
                    .skipped_duplicates
                    .push(SkippedDuplicate {
                        src: f.src.path.clone(),
                        existing,
                    });
                self.account(f.src.size, 1, &f.src.path);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(source_err(f, "reading for duplicate check", &e)),
        }
    }

    /// Durability checkpoint: flush devices, record `Done`, then notify the
    /// sink and the report. On flush failure the queue is kept (and the
    /// journal still says `Pending` for those files).
    ///
    /// The ledger lock is held only to move data in and out; the flush and
    /// the journal write (both slow on removable media) run without it, so
    /// workers finishing files are never stalled behind `F_FULLFSYNC`. The
    /// `checkpointing` lock keeps checkpoints (and journal writes) in order.
    fn checkpoint(&self, final_: bool) -> Result<(), String> {
        let _one_at_a_time = if final_ {
            self.checkpointing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        } else {
            match self.checkpointing.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                // Another worker is already flushing.
                Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            }
        };
        let taken = {
            let mut l = lock(&self.ledger);
            l.last = Instant::now();
            std::mem::take(&mut l.queued)
        };
        if !taken.is_empty()
            && let Err(e) = self.flush_devices()
        {
            // Persist what we know (failures, duplicates); the queued files
            // stay `Pending` in the journal and are re-verified on resume.
            let snapshot = {
                let mut l = lock(&self.ledger);
                l.queued.splice(0..0, taken);
                final_.then(|| (l.journal.clone(), l.path.clone()))
            };
            if let Some((journal, path)) = snapshot
                && let Err(je) = journal.save(&path)
            {
                tracing::warn!(%je, "journal save failed");
            }
            return Err(e);
        }
        let snapshot = {
            let mut l = lock(&self.ledger);
            for q in &taken {
                l.set(&q.key, q.state.clone());
            }
            (!taken.is_empty() || final_).then(|| (l.journal.clone(), l.path.clone()))
        };
        if let Some((journal, path)) = snapshot
            && let Err(e) = journal.save(&path)
        {
            // Data is durable; the journal just lags (resume re-verifies).
            tracing::warn!(%e, "journal save failed");
        }
        let delivered = taken;
        for q in delivered {
            let sink_result = match self.hooks.sink {
                Some(sink) => lt_fsutil::catch_panic(|| sink.imported(&q.file)),
                None => Ok(Ok(())),
            };
            let mut r = lock(&self.report);
            match sink_result {
                Ok(Ok(())) => {
                    r.bytes_copied += q.copied_bytes;
                    r.copied.push(q.file);
                }
                Ok(Err(e)) => r.failed.push(FailedFile {
                    src: q.file.src,
                    reason: format!("{NOT_REGISTERED}: {e}"),
                }),
                Err(p) => r.failed.push(FailedFile {
                    src: q.file.src,
                    reason: format!("{NOT_REGISTERED}: the session sink panicked: {p}"),
                }),
            }
        }
        Ok(())
    }

    /// `F_FULLFSYNC` every destination root through the handle opened at
    /// the start. A root that is gone, replaced or fails to flush is an
    /// error: nothing is marked done, copied or backed up.
    fn flush_devices(&self) -> Result<(), String> {
        if let Some(fi) = self.hooks.faults {
            fi.before_flush().map_err(|e| e.to_string())?;
        }
        for h in &self.roots {
            h.flush()
                .map_err(|e| format!("{}: {e}", h.path().display()))?;
        }
        Ok(())
    }

    /// Optional paranoia after a copy landed:
    /// * `verify_source_reread`: read the card again with the cache bypassed;
    ///   a different hash means the reader returned bad bytes at least once.
    /// * RAWs: if the card file parses (container, metadata, embedded
    ///   preview), the copy must parse too.
    fn post_copy_checks(
        &self,
        f: &PlannedFile,
        targets: &[Target],
        hash: FileHash,
    ) -> Result<(), CopyFail> {
        if self.plan.verify_source_reread {
            let mut src = fsio::open_source(&f.src.path)
                .map_err(|e| source_err(f, "re-reading source", &e))?;
            lt_fsutil::set_nocache(&src);
            let (again, _, _) = stream_hashed(
                &mut src,
                f.src.size,
                self.plan.chunk_size,
                |_| Ok(()),
                |_, _| Ok(()),
                |e| source_err(f, "re-reading source", &e),
            )?;
            if again != hash {
                return Err(CopyFail::Failed(
                    format!(
                        "card returned different bytes on a second read of {}; not trusted",
                        f.src.path.display()
                    ),
                    io::ErrorKind::InvalidData,
                ));
            }
        }
        if f.src.kind == crate::scan::FileKind::Raw && crate::raw::structurally_valid(&f.src.path) {
            for t in targets.iter().filter(|t| t.primary) {
                if !crate::raw::structurally_valid(&t.fin) {
                    return Err(CopyFail::Failed(
                        format!(
                            "copy {} does not parse as a RAW although the card file does",
                            t.fin.display()
                        ),
                        io::ErrorKind::InvalidData,
                    ));
                }
            }
        }
        Ok(())
    }

    fn hash_source(&self, f: &PlannedFile) -> Result<(FileHash, PartialHash), CopyFail> {
        let mut src =
            fsio::open_source(&f.src.path).map_err(|e| source_err(f, "opening source", &e))?;
        let (h, p, n) = stream_hashed(
            &mut src,
            f.src.size,
            self.plan.chunk_size,
            |off| match self.hooks.faults {
                Some(fi) => fi
                    .before_read(&f.src.path, off)
                    .map_err(|e| source_err(f, "reading", &e)),
                None => Ok(()),
            },
            |_, _| Ok(()),
            |e| source_err(f, "reading", &e),
        )?;
        if n != f.src.size {
            return Err(CopyFail::Failed(
                format!(
                    "source {} changed size since the scan",
                    f.src.path.display()
                ),
                io::ErrorKind::UnexpectedEof,
            ));
        }
        Ok((h, p))
    }

    /// Single-pass copy to `targets`. A primary failure fails the file; a
    /// backup failure is returned per target (index-aligned) and the primary
    /// continues.
    fn copy_verified(
        &self,
        f: &PlannedFile,
        targets: &[Target],
    ) -> Result<(FileHash, PartialHash, Vec<Option<String>>), CopyFail> {
        let partials: Vec<PathBuf> = targets.iter().map(|t| fsio::partial_path(&t.fin)).collect();
        // Only partials this call created are removed on failure. One that
        // already exists (not in our journal) is kept while fresh (another
        // import's?) and cleared once stale: see `fsio::remove_stale_partial`.
        let mut guard = PartialGuard { paths: Vec::new() };
        let mut errs: Vec<Option<String>> = vec![None; targets.len()];
        // Primary errors abort; backup errors disable that target.
        let settle =
            |errs: &mut Vec<Option<String>>, i: usize, err: CopyFail| -> Result<(), CopyFail> {
                if targets[i].primary {
                    return Err(err);
                }
                if let CopyFail::Failed(msg, _) = err {
                    errs[i] = Some(msg);
                }
                Ok(())
            };

        let mut src =
            fsio::open_source(&f.src.path).map_err(|e| source_err(f, "opening source", &e))?;
        let mut outs: Vec<Option<File>> = Vec::with_capacity(targets.len());
        for (i, p) in partials.iter().enumerate() {
            let opened = p
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .map_err(|e| io_fail("creating", p, &e))
                .and_then(|()| fsio::create_partial(p).map_err(|e| io_fail("creating", p, &e)));
            match opened {
                Ok(out) => {
                    guard.paths.push(p.clone());
                    lt_fsutil::set_nocache(&out);
                    outs.push(Some(out));
                }
                Err(e) => {
                    settle(&mut errs, i, e)?;
                    outs.push(None);
                }
            }
        }

        let (hash, partial, copied) = stream_hashed(
            &mut src,
            f.src.size,
            self.plan.chunk_size,
            |offset| {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(CopyFail::Cancelled);
                }
                match self.hooks.faults {
                    Some(fi) => fi
                        .before_read(&f.src.path, offset)
                        .map_err(|e| source_err(f, "reading", &e)),
                    None => Ok(()),
                }
            },
            |offset, chunk| {
                for i in 0..outs.len() {
                    let Some(out) = outs[i].as_mut() else {
                        continue;
                    };
                    let res = match self.hooks.faults {
                        Some(fi) => fi.before_write(&partials[i], offset),
                        None => Ok(()),
                    }
                    .and_then(|()| out.write_all(chunk));
                    if let Err(e) = res {
                        outs[i] = None;
                        settle(&mut errs, i, io_fail("writing", &partials[i], &e))?;
                    }
                }
                self.bytes_done
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                self.maybe_progress(&f.src.path);
                Ok(())
            },
            |e| source_err(f, "reading", &e),
        )?;
        if copied != f.src.size {
            return Err(CopyFail::Failed(
                format!(
                    "source {} changed size during import ({} → {copied} bytes)",
                    f.src.path.display(),
                    f.src.size
                ),
                io::ErrorKind::UnexpectedEof,
            ));
        }

        for i in 0..outs.len() {
            let Some(out) = outs[i].take() else { continue };
            if let Err(e) = self.finish_target(f, out, &partials[i], &targets[i].fin, hash) {
                settle(&mut errs, i, e)?;
            }
        }
        Ok((hash, partial, errs))
    }

    /// mtime → fsync → uncached verify → no-replace rename, for one target.
    fn finish_target(
        &self,
        f: &PlannedFile,
        out: File,
        partial: &Path,
        fin: &Path,
        hash: FileHash,
    ) -> Result<(), CopyFail> {
        out.set_modified(f.src.mtime)
            .map_err(|e| io_fail("setting mtime on", partial, &e))?;
        lt_fsutil::sync_file(&out).map_err(|e| io_fail("syncing", partial, &e))?;
        drop(out);
        if let Some(fi) = self.hooks.faults {
            fi.before_verify(partial);
        }
        let got = verify_hash(partial, f.src.size, self.plan.chunk_size)
            .map_err(|e| io_fail("verifying", partial, &e))?;
        if got != hash {
            return Err(CopyFail::Failed(
                format!(
                    "verification failed for {}: written bytes differ from source",
                    partial.display()
                ),
                io::ErrorKind::InvalidData,
            ));
        }
        lt_fsutil::rename_noreplace(partial, fin).map_err(|e| io_fail("placing", fin, &e))
    }

    fn fail(&self, f: &PlannedFile, reason: String) {
        tracing::warn!(src = ?f.src.path, %reason, "import failed");
        lock(&self.ledger).set(
            &f.key,
            EntryState::Failed {
                reason: reason.clone(),
            },
        );
        lock(&self.report).failed.push(FailedFile {
            src: f.src.path.clone(),
            reason,
        });
        self.account(0, 1, &f.src.path);
    }

    /// Count finished files (and bytes not streamed through the copy loop).
    fn account(&self, bytes: u64, files: usize, current: &Path) {
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
        self.files_done.fetch_add(files, Ordering::Relaxed);
        self.emit(current);
    }

    fn maybe_progress(&self, current: &Path) {
        let now_ms = self.start.elapsed().as_millis() as u64;
        let last = self.last_progress_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= PROGRESS_EVERY.as_millis() as u64
            && self
                .last_progress_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.emit(current);
        }
    }

    fn emit(&self, current: &Path) {
        let bytes_done = self.bytes_done.load(Ordering::Relaxed);
        let secs = self.start.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            bytes_done as f64 / secs
        } else {
            0.0
        };
        let remaining = self.bytes_total.saturating_sub(bytes_done);
        let eta = (rate > 0.0).then(|| Duration::from_secs_f64(remaining as f64 / rate));
        let event = ImportProgress {
            bytes_done,
            bytes_total: self.bytes_total,
            files_done: self.files_done.load(Ordering::Relaxed),
            files_total: self.files_total,
            current: Some(current.to_path_buf()),
            bytes_per_sec: rate,
            eta,
        };
        // A panicking progress callback must not kill the import.
        if let Err(p) = lt_fsutil::catch_panic(|| (self.progress)(&event)) {
            tracing::warn!(panic = %p, "progress callback panicked");
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Classify a source-side I/O error: card gone, or this file is bad.
fn source_err(f: &PlannedFile, what: &str, e: &io::Error) -> CopyFail {
    let dir_gone = f.src.path.parent().is_some_and(|d| !d.exists());
    let unmounted =
        !f.mount.exists() || lt_fsutil::mount_point_of(&f.mount).ok().as_ref() != Some(&f.mount);
    if dir_gone || unmounted || e.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error()) {
        CopyFail::SourceLost(format!(
            "source volume {} was removed: {e}",
            f.mount.display()
        ))
    } else {
        CopyFail::Failed(format!("{what} {}: {e}", f.src.path.display()), e.kind())
    }
}

fn io_fail(what: &str, p: &Path, e: &io::Error) -> CopyFail {
    CopyFail::Failed(format!("{what} {}: {e}", p.display()), e.kind())
}

/// Removes this call's `.partial` files on every exit path (after a
/// successful rename they no longer exist).
struct PartialGuard {
    paths: Vec<PathBuf>,
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = fs::remove_file(p);
        }
    }
}

/// Fill `buf` as far as possible (fewer, larger writes); 0 means EOF.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Re-read a written file with the page cache bypassed and hash it.
///
/// Reads in `chunk`-sized pieces: with `F_NOCACHE` every read goes to the
/// device synchronously, so small reads (e.g. `Hasher::update_reader`'s)
/// cut verify throughput by more than half.
pub fn verify_hash(path: &Path, size: u64, chunk: usize) -> io::Result<FileHash> {
    let mut f = File::open(path)?;
    lt_fsutil::set_nocache(&f);
    stream_hashed(&mut f, size, chunk, |_| Ok(()), |_, _| Ok(()), |e| e).map(|(h, _, _)| h)
}

/// Read `r` to EOF in `chunk`-sized pieces on this thread, passing each chunk
/// to `sink` (e.g. destination writes) while a helper thread hashes the same
/// chunks (full BLAKE3 + [`PartialHash`]). Two buffers ping-pong between the
/// threads, so a worker runs at max(I/O, hashing) instead of their sum.
/// Returns (hash, partial hash, bytes read). `before_read`/`sink` errors and
/// mapped read errors abort the stream.
fn stream_hashed<E>(
    r: &mut impl Read,
    size_hint: u64,
    chunk: usize,
    mut before_read: impl FnMut(u64) -> Result<(), E>,
    mut sink: impl FnMut(u64, &[u8]) -> Result<(), E>,
    read_err: impl Fn(io::Error) -> E,
) -> Result<(FileHash, PartialHash, u64), E> {
    let (full_tx, full_rx) = mpsc::sync_channel::<(Vec<u8>, usize, u64)>(1);
    let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(2);
    for _ in 0..2 {
        free_tx.send(vec![0u8; chunk]).expect("receiver alive");
    }
    std::thread::scope(|s| {
        let hasher = s.spawn(move || {
            let mut h = blake3::Hasher::new();
            let mut p = PartialHasher::new(size_hint);
            for (buf, n, off) in full_rx {
                h.update(&buf[..n]);
                p.update(off, &buf[..n]);
                // The reader may already be gone (error path); that's fine.
                let _ = free_tx.send(buf);
            }
            (FileHash(*h.finalize().as_bytes()), p.finish())
        });
        // Moves `full_tx` in, so it drops (ending the hasher) on every path.
        let io = (move || -> Result<u64, E> {
            let mut off = 0u64;
            loop {
                let mut buf = free_rx.recv().expect("hasher thread returns buffers");
                before_read(off)?;
                let n = read_full(r, &mut buf).map_err(&read_err)?;
                if n == 0 {
                    return Ok(off);
                }
                sink(off, &buf[..n])?;
                full_tx.send((buf, n, off)).expect("hasher thread alive");
                off += n as u64;
            }
        })();
        let (hash, partial) = hasher.join().expect("hasher thread panicked");
        io.map(|len| (hash, partial, len))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(dir: &Path) -> lt_fsutil::DirLock {
        lt_fsutil::DirLock::try_lock_dir(dir, "import")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_busy_lock_released_during_the_grace_is_taken() {
        let t = tempfile::tempdir().unwrap();
        let mut holder = Some(held(t.path()));
        let mut waits = 0;
        // Released only once an attempt found it busy: the retry is what takes it.
        let l = lock_destination(
            t.path(),
            Duration::from_secs(5),
            &AtomicBool::new(false),
            || {
                waits += 1;
                drop(holder.take());
            },
        );
        assert!(l.is_ok());
        assert!(waits >= 1);
    }

    #[test]
    fn a_lock_held_past_the_grace_is_busy_and_a_cancel_stops_waiting() {
        let t = tempfile::tempdir().unwrap();
        let _held = held(t.path());
        let mut waits = 0;
        let r = lock_destination(
            t.path(),
            Duration::from_millis(30),
            &AtomicBool::new(false),
            || waits += 1,
        );
        assert!(matches!(r, Err(ImportError::Busy(p)) if p == t.path()));
        assert!(waits >= 1);
        let r = lock_destination(
            t.path(),
            Duration::from_secs(60),
            &AtomicBool::new(true),
            || {
                panic!("cancelled: no wait");
            },
        );
        assert!(matches!(r, Err(ImportError::Busy(_))));
    }
}
