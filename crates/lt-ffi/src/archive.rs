//! Archive (`lt-archive`) and the Amazon Photos folder hand-off, for the app.
//!
//! - **Destinations.** [`ArchiveConfig::object_store`] picks one: `None` archives
//!   with rsync into [`ArchiveConfig::target`], a folder (a mounted volume such as
//!   the NAS at `/Volumes/pics`, or a local folder); `Some` archives to
//!   S3-compatible object storage through the app's [`ArchiveHttp`], with the keys
//!   from [`ArchiveManager::set_object_store`] (the app keeps them in the Keychain).
//!   Everything below works the same for both, through `lt_archive::Destination`.
//!   Switching never moves or deletes what is already archived.
//! - [`ArchiveManager`] owns one [`Scheduler`] per registered session and a
//!   driver thread that decides what runs: after an import, every interval
//!   when changes are pending, on session close, on "Sync now". Swift only
//!   reads [`ArchiveManager::status`], which never touches the file system:
//!   it returns what the driver and the runs last recorded.
//! - **The driver never touches the target itself.** Its only file-system
//!   work is the bounded target check ([`check_with`]: a detached thread,
//!   waited for with a timeout) and the local lock directory. Each archive
//!   run (identity file, claim of the session's archive folder, DB snapshot,
//!   rsync) happens on its own worker thread. A watchdog notices a run that
//!   stops showing signs of life: it re-checks the target (bounded) and, if
//!   the target doesn't answer, or after a hard limit, marks the run "not
//!   responding", cancels it (rsync is killed if it can be) and **detaches**
//!   it, so other sessions keep archiving and quitting stays prompt.
//! - While the target is offline the driver re-checks it with backoff (5 s
//!   doubling to 30 s) whatever is waiting, and wakes early on "Sync now" or
//!   a mount change ([`ArchiveManager::volumes_changed`]); when idle it checks
//!   every 15 s. No spinning.
//! - Each session archives into its own folder on the target
//!   (`<name> (<id>)`, claimed with a marker, recorded in the session store as
//!   `.lt/archive.json`), so two sessions with the same folder name never
//!   overwrite each other; an existing `<name>` folder holding the session's
//!   files is adopted (archives made before this rule).
//! - An unreadable original or a failed DB snapshot makes a run
//!   **incomplete** (listed per file, retried with backoff), never "archived".
//! - What can be archived: an imported session, or a folder opened in place
//!   whose store lives in that folder (its volume is local and writable:
//!   APFS/HFS). A folder on an exFAT/FAT card, a read-only or network volume
//!   (store elsewhere) is never archived, since nothing may be written there,
//!   not even the DB snapshot. The card watcher's notion of a "card" (DCIM on
//!   an ejectable volume) is about offering an import; an APFS-formatted
//!   card opened in place already holds its store, so it archives like any
//!   local folder.
//! - The Amazon Photos hand-off runs on its own worker, one mirror at a
//!   time per process (the manifest also has a cross-process lock): after an
//!   import, on "Sync now", when a session is first registered (launch
//!   catch-up) and every 15 minutes, for imported sessions only. The chosen
//!   folder is never created: a missing one is reported per session.
//! - [`ArchiveManager::shutdown`] cancels a running rsync (the child is
//!   killed) and the hand-off, and waits a bounded time.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lt_archive::object::{Credentials, ObjectDestination, ObjectStoreConfig, Transport};
use lt_archive::scheduler::TargetLock;
use lt_archive::{
    AmazonBackup, ArchiveError, ArchiveTarget, Clock, CommandOutput, CommandRunner, CommandSpec,
    Destination, DestinationKind, FolderDestination, Original, RsyncProgress, RsyncRunner,
    RunRecord, Scheduler, SchedulerConfig, SessionSource, Snapshotter, SystemClock, SystemRunner,
    Trigger, VolumeId,
};
use serde::{Deserialize, Serialize};

use crate::error::DsError;
use crate::jobs::{CancelToken, guard};
use crate::session::SessionHandle;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn dur_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ------------------------------------------------------------ target check ---

/// Result of a bounded availability check of an archive target.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TargetCheck {
    pub path: String,
    /// Mounted (when under `/Volumes/<name>`), an existing writable directory.
    pub available: bool,
    /// The check did not answer in time (e.g. a stale network mount).
    pub timed_out: bool,
    /// Why it isn't available.
    pub reason: Option<String>,
    /// File system of the target (`apfs`, `nfs`, `smbfs`, `exfat`, …), or
    /// `object storage`.
    pub fs_type: Option<String>,
    /// `volume` (must be mounted), `local` (a folder on this Mac) or `object`.
    #[uniffi(default = None)]
    pub kind: Option<String>,
}

/// The file-system work of a check (a seam: tests block it to simulate a hung mount),
/// for a target and the volume it was confirmed on.
type CheckFn = dyn Fn(&Path, Option<&VolumeId>) -> Result<Option<String>, String> + Send + Sync;

fn real_check(root: &Path, volume: Option<&VolumeId>) -> Result<Option<String>, String> {
    folder_destination(root, volume.cloned(), None, protected())
        .check(&AtomicBool::new(false))
        .map_err(reason)
}

/// Why a destination isn't available, in words (an offline reason as is).
fn reason(e: ArchiveError) -> String {
    match e {
        ArchiveError::TargetUnavailable { reason, .. } => reason,
        other => other.to_string(),
    }
}

/// The folder destination for `root` on `volume`.
fn folder_destination(
    root: &Path,
    volume: Option<VolumeId>,
    rsync: Option<RsyncRunner>,
    protected: Vec<PathBuf>,
) -> FolderDestination {
    let mut target = ArchiveTarget::new(root);
    target.volume = volume;
    FolderDestination::new(target, rsync).protecting(protected)
}

/// Luxtable's support folder (`~/Library/Application Support/Luxtable`): never
/// an archive target.
fn protected() -> Vec<PathBuf> {
    lt_archive::scheduler::default_lock_dir()
        .and_then(|d| d.parent().map(Path::to_path_buf))
        .into_iter()
        .collect()
}

/// `volume` or `local` for a folder target (no file-system call).
fn folder_kind(root: &Path, volume: Option<VolumeId>) -> String {
    folder_destination(root, volume, None, Vec::new())
        .kind()
        .as_str()
        .to_owned()
}

/// Paths whose check thread has not come back yet.
fn in_flight() -> &'static Mutex<HashSet<PathBuf>> {
    static S: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// Check `root` on a detached thread, waiting at most `timeout`. A check that
/// is still stuck from an earlier call is not duplicated (reported as timed out).
fn check_with(
    root: &Path,
    volume: Option<VolumeId>,
    timeout: Duration,
    check: Arc<CheckFn>,
) -> TargetCheck {
    let kind = folder_kind(root, volume.clone());
    let mut c = check_bounded(root, timeout, move |key| check(key, volume.as_ref()));
    c.kind = Some(kind);
    c
}

/// Run `check` for the destination keyed `root` on a detached thread, waiting at
/// most `timeout`. A check that is still stuck from an earlier call is not
/// duplicated (reported as timed out).
fn check_bounded(
    root: &Path,
    timeout: Duration,
    check: impl FnOnce(&Path) -> Result<Option<String>, String> + Send + 'static,
) -> TargetCheck {
    let path = root.to_string_lossy().into_owned();
    let timed_out = |reason: &str| TargetCheck {
        path: path.clone(),
        available: false,
        timed_out: true,
        reason: Some(reason.to_owned()),
        fs_type: None,
        kind: None,
    };
    if root.as_os_str().is_empty() {
        return TargetCheck {
            path,
            available: false,
            timed_out: false,
            reason: Some("no archive target set".into()),
            fs_type: None,
            kind: None,
        };
    }
    if !lock(in_flight()).insert(root.to_path_buf()) {
        return timed_out("the volume is not responding (an earlier check is still waiting)");
    }
    let (tx, rx) = mpsc::channel();
    let key = root.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("lt-archive-check".into())
        .spawn(move || {
            let r = check(&key);
            lock(in_flight()).remove(&key);
            let _ = tx.send(r);
        });
    if spawned.is_err() {
        lock(in_flight()).remove(root);
        return timed_out("could not start the check");
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(fs_type)) => TargetCheck {
            path,
            available: true,
            timed_out: false,
            reason: None,
            fs_type,
            kind: None,
        },
        Ok(Err(reason)) => TargetCheck {
            path,
            available: false,
            timed_out: false,
            reason: Some(reason),
            fs_type: None,
            kind: None,
        },
        Err(_) => timed_out("the volume did not respond in time"),
    }
}

/// Check an archive target without ever blocking the caller for longer than
/// `timeout_ms` (the waiting happens on a worker thread; the file-system
/// call on a detached one).
#[uniffi::export]
pub async fn check_archive_target(
    path: String,
    timeout_ms: u32,
    token: Arc<CancelToken>,
) -> Result<TargetCheck, DsError> {
    crate::jobs::run_job_on_thread("lt-archive-wait", token, move |_| {
        // Confirming a target: the volume it is on now is the one it will be recorded
        // on (`archive_volume_identity`), so a missing folder there is creatable.
        let volume = VolumeId::of(Path::new(&path));
        Ok(check_with(
            Path::new(&path),
            volume,
            Duration::from_millis(u64::from(timeout_ms)),
            Arc::new(real_check),
        ))
    })
    .await
}

/// The volume `path` is on right now, as an opaque token for
/// [`ArchiveConfig::target_volume`]: record it when the user confirms the archive
/// target, so a share that later goes away (and leaves the path on the boot disk) is
/// reported offline instead of written to (review N3). `None` if it can't be told.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn archive_volume_identity(path: String) -> Option<String> {
    VolumeId::of(Path::new(&path)).map(|v| v.to_token())
}

/// Is the official Amazon Photos app in `/Applications`? (The app also asks
/// Launch Services by bundle id.)
#[uniffi::export]
pub fn amazon_photos_app_installed() -> Result<bool, DsError> {
    guard(|| Ok(lt_archive::amazon_app_installed()))
}

// ---------------------------------------------------------- object storage ---

/// S3-compatible object storage settings (Settings ▸ Archive). The keys are not
/// here: they live in the Keychain and reach Rust as [`ObjectCredentials`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObjectStoreSettings {
    /// `https://s3.eu-central-003.backblazeb2.com`, … Empty: AWS S3 in `region`.
    pub endpoint: String,
    /// `us-east-1`, `auto` (R2), … Empty: `us-east-1`.
    pub region: String,
    pub bucket: String,
    /// Key prefix ("path"): `luxtable/` → `luxtable/<session>/<file>`.
    pub prefix: String,
    /// `https://host/bucket/key` instead of `https://bucket.host/key`.
    pub path_style: bool,
}

impl ObjectStoreSettings {
    fn config(&self) -> ObjectStoreConfig {
        ObjectStoreConfig {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            path_style: self.path_style,
        }
    }
}

/// An access key pair, from the Keychain. The secret never appears in `Debug`.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObjectCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for ObjectCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectCredentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

impl ObjectCredentials {
    fn credentials(&self) -> Credentials {
        Credentials {
            access_key_id: self.access_key_id.trim().to_owned(),
            secret_access_key: self.secret_access_key.trim().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<ArchiveHeader>,
    /// At most one upload part (8 MB, more only for files over 80 GB).
    pub body: Vec<u8>,
}

impl std::fmt::Debug for ArchiveHttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveHttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveHttpResponse {
    pub status: u16,
    /// Every header (`ETag`, `Content-Length`, `x-amz-meta-*`, `Date`, …).
    pub headers: Vec<ArchiveHeader>,
    pub body: Vec<u8>,
}

/// Implemented in Swift with `URLSession`. Called on a worker thread, one request
/// at a time; blocks until the answer. Must not follow redirects.
#[uniffi::export(with_foreign)]
pub trait ArchiveHttp: Send + Sync {
    /// Any HTTP answer is `Ok` (whatever its status); `Err` means no answer.
    fn send(&self, request: ArchiveHttpRequest) -> Result<ArchiveHttpResponse, DsError>;
}

/// [`ArchiveHttp`] as lt-archive's transport.
struct HttpBridge(Arc<dyn ArchiveHttp>);

impl Transport for HttpBridge {
    fn send(&self, r: lt_archive::object::Request) -> Result<lt_archive::object::Response, String> {
        let header = |(name, value)| ArchiveHeader { name, value };
        let answer = self
            .0
            .send(ArchiveHttpRequest {
                method: r.method,
                url: r.url,
                headers: r.headers.into_iter().map(header).collect(),
                body: r.body,
            })
            .map_err(|e| e.to_string())?;
        Ok(lt_archive::object::Response {
            status: answer.status,
            headers: answer
                .headers
                .into_iter()
                .map(|h| (h.name, h.value))
                .collect(),
            body: answer.body,
        })
    }
}

/// What is wrong with `settings`, in words (`None`: they can be used): the bucket
/// name, region, endpoint address and path, checked as the archive will use them.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn archive_object_settings_problem(settings: ObjectStoreSettings) -> Option<String> {
    let c = settings.config();
    c.endpoint().and(c.normalized_prefix()).err()
}

/// What "Test Connection" did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObjectConnectionTest {
    /// The small test object it left (the next test overwrites it).
    pub test_key: String,
    /// `s3://bucket/prefix` (and the host).
    pub location: String,
}

/// Settings ▸ Archive ▸ Test Connection: `HeadBucket`, then a small `PutObject` under
/// `<prefix>.lt-archive-test/` and a `HeadObject` of it. The test object stays.
/// Wrong keys, a missing bucket, a skewed clock or bad settings are
/// `InvalidArgument`; no answer is `Io`.
#[uniffi::export]
pub async fn archive_object_test_connection(
    http: Arc<dyn ArchiveHttp>,
    settings: ObjectStoreSettings,
    credentials: ObjectCredentials,
    token: Arc<CancelToken>,
) -> Result<ObjectConnectionTest, DsError> {
    crate::jobs::run_job_on_thread("lt-archive-test", token, move |cancelled| {
        test_object_store(
            Arc::new(HttpBridge(http)),
            &settings,
            &credentials,
            cancelled,
        )
    })
    .await
}

fn test_object_store(
    transport: Arc<dyn Transport>,
    settings: &ObjectStoreSettings,
    credentials: &ObjectCredentials,
    cancelled: &dyn Fn() -> bool,
) -> Result<ObjectConnectionTest, DsError> {
    let invalid = |e: ArchiveError| match e {
        ArchiveError::TargetNotUsable { reason, .. } => {
            DsError::InvalidArgument { message: reason }
        }
        ArchiveError::TargetUnavailable { reason, .. } => DsError::Io { message: reason },
        ArchiveError::Cancelled => DsError::Cancelled,
        e => DsError::Io {
            message: e.to_string(),
        },
    };
    let dest = ObjectDestination::new(settings.config(), transport, credentials.credentials())
        .map_err(invalid)?;
    let cancel = AtomicBool::new(cancelled());
    let r = dest
        .test_connection(SystemTime::now(), &cancel)
        .map_err(invalid)?;
    Ok(ObjectConnectionTest {
        test_key: r.test_key,
        location: r.location,
    })
}

// ----------------------------------------------------------------- records ---

/// Archive settings.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveConfig {
    pub enabled: bool,
    /// Target root, e.g. `/Volumes/pics/Luxtable` (each session gets its own folder there).
    pub target: String,
    /// Periodic check; rsync runs only when changes are pending (or daily).
    pub interval_secs: u64,
    /// Quiet time after the last change before archiving.
    pub debounce_secs: u64,
    /// Lock files for single-flight per target across processes (`None` = default dir).
    pub lock_dir: Option<String>,
    /// Timeout of each target check.
    pub check_timeout_ms: u32,
    /// Use this rsync binary (`None`: the first of Homebrew rsync 3, then the
    /// system openrsync). Read when the manager starts.
    pub rsync_path: Option<String>,
    /// Archive rejected photos (and the session's `Trash/`) too. On by
    /// default: the archive is the backup (final-journeys2 N2).
    #[uniffi(default = true)]
    pub archive_rejected: bool,
    /// Hand rejected photos to Amazon Photos too. Off by default.
    #[uniffi(default = false)]
    pub amazon_rejected: bool,
    /// The volume the target was confirmed on ([`archive_volume_identity`]). Runs and
    /// checks require the target to be on it; without it, a missing target folder
    /// outside `/Volumes` is never created.
    #[uniffi(default = None)]
    pub target_volume: Option<String>,
    /// Archive to S3-compatible object storage instead of `target` (the keys come
    /// from [`ArchiveManager::set_object_store`]).
    #[uniffi(default = None)]
    pub object_store: Option<ObjectStoreSettings>,
}

impl ArchiveConfig {
    /// What identifies the destination (lock files, saved state, the schedulers):
    /// the folder path, or `s3://<host>/<bucket>/<prefix>`.
    fn target_key(&self) -> String {
        match &self.object_store {
            Some(o) => o.config().lock_key(),
            None => self.target.clone(),
        }
    }
}

/// One session's archive state.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionArchiveStatus {
    pub name: String,
    /// The folder archived (or the session folder, when it can't be).
    pub source: String,
    /// `None` when the session can't be archived.
    pub not_archivable: Option<String>,
    /// This session's folder under the target (once claimed by a run).
    pub archive_folder: Option<String>,
    /// The last successful run, this launch or (from `.lt/archive-state.json`)
    /// an earlier one.
    pub last_success_ms: Option<i64>,
    /// Why the last run failed, in words (e.g. "The archive disk is full
    /// (1.2 GB free) — 16 files not archived"); kept across launches until a
    /// run succeeds.
    pub last_error: Option<String>,
    /// The raw error of that run (rsync's stderr), for a "Details" disclosure.
    #[uniffi(default = None)]
    pub last_error_detail: Option<String>,
    /// What the last run could not archive (`"<path>: <reason>"`): an
    /// unreadable original, a failed DB snapshot. Empty after a clean run.
    pub problems: Vec<String>,
    /// Originals in the session at the last run (0: no run yet).
    #[uniffi(default = 0)]
    pub originals: u64,
    /// Rejected originals the last run left out (the setting is off).
    #[uniffi(default = 0)]
    pub rejects_excluded: u64,
    pub pending_changes: u64,
    pub behind_since_ms: Option<i64>,
    pub running: bool,
    /// Its run stopped responding (e.g. a stale network mount) and was
    /// detached; it is retried once the target answers again.
    pub not_responding: bool,
    /// rsync 3 only (`--info=progress2`); openrsync reports no progress.
    pub percent: Option<u8>,
    pub bytes_per_sec: Option<f64>,
    pub eta_secs: Option<u64>,
    pub consecutive_failures: u32,
    pub retry_in_ms: Option<u64>,
    /// This session's last Amazon Photos hand-off, if any.
    pub amazon: Option<AmazonStatus>,
}

/// One archive attempt (most recent first in [`ArchiveOverview::runs`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveRun {
    pub session: String,
    /// "interval" | "changes" | "on demand" | "session closed"
    pub trigger: String,
    /// "succeeded" | "incomplete" | "failed" | "offline" | "busy" |
    /// "cancelled" | "not responding"
    pub outcome: String,
    /// In words for a failure (never raw rsync output: see `detail`).
    pub message: Option<String>,
    /// The raw error (rsync's stderr), for a "Details" disclosure.
    #[uniffi(default = None)]
    pub detail: Option<String>,
    pub started_ms: i64,
    pub duration_ms: u64,
    pub warnings: Vec<String>,
    /// Items not archived by this run (`"<path>: <reason>"`).
    pub problems: Vec<String>,
}

/// An Amazon Photos hand-off, as last run.
#[derive(Debug, Clone, PartialEq, Eq, Default, uniffi::Record)]
pub struct AmazonStatus {
    pub folder: Option<String>,
    pub running: bool,
    pub last_run_ms: Option<i64>,
    /// Placed by the last run (APFS clones + verified copies).
    pub placed: u64,
    pub cloned: u64,
    pub already_mirrored: u64,
    /// Not RAW/JPEG/HEIC (Amazon gets originals only).
    pub skipped_unsupported: u64,
    /// Not hashed yet or missing: handed over by a later run.
    pub waiting: u64,
    pub failed: Vec<String>,
    pub error: Option<String>,
    /// The chosen folder is gone (moved, deleted, disk unplugged). It is
    /// never recreated; choose it again in Settings.
    pub folder_missing: bool,
    /// Which session this was (the overall status: the last one that ran).
    pub session: Option<String>,
    /// Rejected originals left out (rejects aren't handed over unless set).
    #[uniffi(default = 0)]
    pub rejects_excluded: u64,
}

/// Everything the archive badge and popover show.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ArchiveOverview {
    pub enabled: bool,
    pub target: TargetCheck,
    /// e.g. "/opt/homebrew/bin/rsync (rsync 3.4.1)"; `None` until detected.
    pub rsync: Option<String>,
    pub rsync_error: Option<String>,
    pub sessions: Vec<SessionArchiveStatus>,
    pub runs: Vec<ArchiveRun>,
    /// The last Amazon hand-off (any session); per session in `sessions`.
    pub amazon: AmazonStatus,
}

// ---------------------------------------------------------- session identity ---

/// A session's archive identity, kept in its store (`.lt/archive.json`):
/// a random id (the owner of its archive folder) and that folder's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveIdentity {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

const IDENTITY_REL: &str = ".lt/archive.json";

fn new_archive_id(store: &Path) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = blake3::Hasher::new();
    h.update(store.as_os_str().as_encoded_bytes());
    h.update(
        &SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    h.update(&std::process::id().to_le_bytes());
    h.update(&N.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    h.finalize().to_hex()[..16].to_owned()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The identity in `store`, created (and saved) on first use.
fn load_identity(store: &Path) -> io::Result<ArchiveIdentity> {
    let path = store.join(IDENTITY_REL);
    match std::fs::read(&path) {
        Ok(b) => {
            if let Ok(id) = serde_json::from_slice::<ArchiveIdentity>(&b)
                && !id.id.is_empty()
                && id.id.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Ok(id);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let id = ArchiveIdentity {
        id: new_archive_id(store),
        folder: None,
    };
    save_identity(store, &id)?;
    Ok(id)
}

/// A session's archive id and the folder its runs claimed under the target, once
/// a run has (`None` before, or when the identity can't be read).
pub(crate) fn archived_as(store: &Path) -> Option<(String, String)> {
    let b = std::fs::read(store.join(IDENTITY_REL)).ok()?;
    let id = serde_json::from_slice::<ArchiveIdentity>(&b).ok()?;
    Some((id.id, id.folder?))
}

fn save_identity(store: &Path, id: &ArchiveIdentity) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(id).map_err(io::Error::other)?;
    write_atomic(&store.join(IDENTITY_REL), &bytes)
}

// ------------------------------------------------------ last result, saved ---

/// What a session's archive runs last left, kept in its store
/// (`.lt/archive-state.json`) so a relaunch never claims "up to date" after
/// a failed run or with changes that were never archived (final-journeys2 N1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SavedState {
    /// The target this state is about (another target: nothing is known).
    #[serde(default)]
    target: String,
    #[serde(default)]
    last_success_ms: Option<i64>,
    /// In words, and the raw error.
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    last_error_detail: Option<String>,
    /// Changes not archived yet.
    #[serde(default)]
    pending: u64,
    #[serde(default)]
    originals: u64,
    #[serde(default)]
    rejects_excluded: u64,
}

const STATE_REL: &str = ".lt/archive-state.json";

impl SavedState {
    /// The saved state of `store` for `target` (default: nothing known).
    fn load(store: &Path, target: &str) -> Self {
        let saved = std::fs::read(store.join(STATE_REL))
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default();
        if saved.target == target {
            saved
        } else {
            Self {
                target: target.to_owned(),
                // Changes not archived anywhere yet are still not archived.
                pending: saved.pending,
                ..Self::default()
            }
        }
    }

    /// Best effort: a store that can't be written only loses this hint.
    fn save(&self, store: &Path) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = write_atomic(&store.join(STATE_REL), &bytes);
        }
    }

    /// The last run succeeded and nothing changed since.
    fn clean(&self) -> bool {
        self.last_success_ms.is_some() && self.last_error.is_none() && self.pending == 0
    }
}

/// Free space on the file system holding `path` (bytes).
fn free_space(path: &Path) -> Option<u64> {
    rustix::fs::statvfs(path)
        .ok()
        .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
}

/// "1.2 GB", "850 MB" (decimal units, like Finder).
fn human_bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let f = n as f64;
    match n {
        0..1_000_000 => format!("{} KB", n / 1000),
        1_000_000..1_000_000_000 => format!("{:.0} MB", f / 1e6),
        1_000_000_000..1_000_000_000_000 => format!("{:.1} GB", f / 1e9),
        _ => format!("{:.1} TB", f / 1e12),
    }
}

/// Why a run failed, in words: never rsync's raw stderr (final-journeys2
/// N1c; the raw text goes behind "Details"). `missing`: originals verified
/// missing on the target (`None`: not known, so no number is given); `free`:
/// free space on the target, asked for only when it's full (the target just
/// answered, so the call doesn't hang).
fn human_error(
    e: &ArchiveError,
    missing: Option<u64>,
    free: impl FnOnce() -> Option<u64>,
) -> String {
    let files = |n: u64| format!("{n} file{}", if n == 1 { "" } else { "s" });
    match e {
        ArchiveError::RsyncNotFound => return "No rsync was found on this Mac.".into(),
        ArchiveError::TargetUnavailable { .. } => return "The archive disk is not mounted.".into(),
        ArchiveError::TargetNotUsable { reason, .. } => {
            return format!("The archive folder can't be used: {reason}.");
        }
        ArchiveError::Cancelled => return "Cancelled.".into(),
        ArchiveError::Incomplete { problems } => {
            return format!("{} not archived.", files(problems.len() as u64));
        }
        ArchiveError::Storage(message) => {
            return format!("The storage service refused the upload: {message}.");
        }
        _ => {}
    }
    let raw = e.to_string().to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| raw.contains(n));
    let not_archived = match missing {
        Some(n) if n > 0 => format!(" — {} not archived", files(n)),
        _ => String::new(),
    };
    if has(&[
        "no space left on device",
        "quota exceeded",
        "file too large",
    ]) {
        let free = free().map_or(String::new(), |b| format!(" ({} free)", human_bytes(b)));
        format!("The archive disk is full{free}{not_archived}.")
    } else if has(&["read-only file system"]) {
        format!("The archive disk is read-only{not_archived}.")
    } else if has(&["permission denied", "operation not permitted"]) {
        format!("Luxtable isn't allowed to write to the archive folder{not_archived}.")
    } else if has(&[
        "broken pipe",
        "connection reset",
        "connection unexpectedly closed",
        "socket is not connected",
        "host is down",
        "stale nfs",
        "timed out",
        "network is unreachable",
        "no route to host",
        "input/output error",
    ]) {
        format!("The connection to the archive was lost{not_archived}.")
    } else {
        format!("The archive copy failed{not_archived}.")
    }
}

/// An object-storage failure in words (`None`: the same words as a folder's).
fn object_error(e: &ArchiveError) -> Option<String> {
    match e {
        ArchiveError::TargetUnavailable { .. } => {
            Some("The storage service can't be reached.".into())
        }
        ArchiveError::TargetNotUsable { reason, .. } => {
            Some(format!("The bucket can't be used: {reason}."))
        }
        _ => None,
    }
}

// ------------------------------------------------------------ run liveness ---

/// Wraps the command runner: every line rsync prints is a sign of life for
/// the watchdog (one run at a time per manager, so one clock suffices).
struct ActivityRunner {
    inner: Arc<dyn CommandRunner>,
    last: Arc<AtomicU64>,
    clock: Arc<dyn Clock>,
}

impl CommandRunner for ActivityRunner {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        let (last, clock) = (self.last.clone(), self.clock.clone());
        self.inner.run(
            cmd,
            &mut |l| {
                last.store(dur_ms(clock.monotonic()), Ordering::Relaxed);
                on_line(l);
            },
            cancel,
        )
    }
}

/// Watchdog and backoff timings (tests shorten them).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// Target re-check while it is offline: starts here, doubles…
    pub offline_base: Duration,
    /// …up to this.
    pub offline_max: Duration,
    /// Target re-check while nothing is due (A-L4).
    pub idle_check: Duration,
    /// A run silent this long gets a (bounded) target check; if the target
    /// doesn't answer, the run is declared not responding.
    pub stall_probe: Duration,
    /// A silent run is declared not responding after this, whatever the
    /// target says, when rsync reports progress (rsync 3 prints every second)…
    pub stall_hard_progress: Duration,
    /// …or after this when it doesn't (openrsync is silent while copying).
    pub stall_hard_quiet: Duration,
    /// Amazon hand-off catch-up for every imported session.
    pub amazon_interval: Duration,
    /// How long [`ArchiveManager::shutdown`] waits for a hand-off to stop.
    pub amazon_shutdown_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            offline_base: Duration::from_secs(5),
            offline_max: Duration::from_secs(30),
            idle_check: Duration::from_secs(15),
            stall_probe: Duration::from_secs(90),
            stall_hard_progress: Duration::from_secs(180),
            stall_hard_quiet: Duration::from_secs(1800),
            amazon_interval: Duration::from_secs(900),
            amazon_shutdown_wait: Duration::from_millis(500),
        }
    }
}

// ----------------------------------------------------------------- manager ---

struct Entry {
    /// The session folder (one entry per session, whichever handle is newest).
    key: PathBuf,
    name: String,
    source: Result<PathBuf, String>,
    handle: Arc<SessionHandle>,
    scheduler: Arc<Scheduler>,
    progress: Arc<Mutex<Option<RsyncProgress>>>,
    /// This session's archive folder name (from its identity, once known).
    archive_folder: Arc<Mutex<Option<String>>>,
    /// Closed sessions leave once their final archive ran.
    closed: bool,
    /// Its run was detached by the watchdog and hasn't returned yet.
    not_responding: Arc<AtomicBool>,
    /// Imported session, or one an import went into (only those are
    /// handed to Amazon Photos; a folder browsed in place is not).
    imported: bool,
    /// Wants an Amazon hand-off.
    amazon_due: bool,
    amazon: Option<AmazonStatus>,
    /// The session's store (where [`SavedState`] lives).
    store: PathBuf,
    /// The last result, as saved in the store (archivable sessions only).
    saved: Arc<Mutex<SavedState>>,
}

impl Entry {
    /// Save the changes not archived yet when they differ from what's saved
    /// (N1: a relaunch still shows them).
    fn save_pending(&self) {
        if self.source.is_err() {
            return;
        }
        let pending = self.scheduler.status().pending_changes;
        let mut saved = lock(&self.saved);
        if saved.pending != pending {
            saved.pending = pending;
            saved.save(&self.store);
        }
    }

    /// Something changed: saved once, when the session goes from archived to
    /// behind (not on every change; quitting saves the count).
    fn note_behind(&self) {
        if lock(&self.saved).pending == 0 {
            self.save_pending();
        }
    }
}

/// The archive run in progress (at most one per manager: one target).
struct Active {
    name: String,
    trigger: Trigger,
    started: Duration,
    started_wall: SystemTime,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    not_responding: Arc<AtomicBool>,
    progress_capable: bool,
    probed_at: Option<Duration>,
    /// Released when the run ends or is detached.
    _lock: TargetLock,
}

#[derive(Default)]
struct TargetState {
    check: Option<TargetCheck>,
    checked_at: Option<Duration>,
    offline_backoff: Duration,
}

struct Inner {
    cfg: Mutex<ArchiveConfig>,
    /// Settings ▸ Plugins: the archive, Amazon and Immich features may be turned off.
    features: crate::features::FeatureGate,
    entries: Mutex<Vec<Entry>>,
    runs: Mutex<VecDeque<ArchiveRun>>,
    target: Mutex<TargetState>,
    rsync: Mutex<Option<Result<RsyncRunner, String>>>,
    active: Mutex<Option<Active>>,
    amazon: Mutex<AmazonStatus>,
    amazon_cfg: Mutex<Option<(PathBuf, PathBuf)>>,
    /// One hand-off at a time in this process (the manifest lock covers others).
    amazon_serial: Mutex<()>,
    amazon_worker: Mutex<Option<Arc<AtomicBool>>>,
    amazon_swept_at: Mutex<Option<Duration>>,
    /// Immich publishing (`crate::immich`): sidecars refreshed before each run;
    /// `None` while Immich isn't set up.
    immich: Mutex<Option<crate::immich::ArchivePublish>>,
    /// Sessions archived since the last Immich album sync (only while publishing).
    immich_archived: Mutex<Vec<Arc<SessionHandle>>>,
    /// Object storage: the app's HTTP and the keys (from the Keychain).
    object_http: Mutex<Option<Arc<dyn Transport>>>,
    object_creds: Mutex<Option<Credentials>>,
    stop: AtomicBool,
    /// Cancels the rsync and the hand-off in flight (quit).
    cancel: AtomicBool,
    recheck: AtomicBool,
    wake: (Mutex<bool>, Condvar),
    check: Arc<CheckFn>,
    runner: Arc<dyn CommandRunner>,
    activity: Arc<AtomicU64>,
    rsync_paths: Vec<PathBuf>,
    clock: Arc<dyn Clock>,
    timing: Timing,
}

const MAX_RUNS: usize = 20;

/// Background archiving for the app (see the module docs).
#[derive(uniffi::Object)]
pub struct ArchiveManager {
    inner: Arc<Inner>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ArchiveManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveManager").finish_non_exhaustive()
    }
}

fn rsync_label(r: &RsyncRunner) -> String {
    let kind = match r.flavor() {
        lt_archive::RsyncFlavor::Rsync3 { version } => format!("rsync {version}"),
        lt_archive::RsyncFlavor::Legacy { version } => format!("rsync {version} (legacy)"),
        lt_archive::RsyncFlavor::Openrsync => "openrsync, no progress".into(),
    };
    format!("{} ({kind})", r.program().display())
}

fn trigger_name(t: Trigger) -> &'static str {
    match t {
        Trigger::Interval => "interval",
        Trigger::Debounce => "changes",
        Trigger::OnDemand => "on demand",
        Trigger::SessionClosed => "session closed",
    }
}

/// What identifies a session across handles: its archived folder, or its store.
fn entry_key(handle: &SessionHandle) -> PathBuf {
    handle
        .archive_source()
        .unwrap_or_else(|_| PathBuf::from(handle.root().unwrap_or_default()))
}

/// Snapshots through the registered session's own handle.
struct HandleSnapshot(Arc<SessionHandle>);

impl Snapshotter for HandleSnapshot {
    fn snapshot(&self, _session_dir: &Path, dest: &Path) -> Result<(), String> {
        self.0.snapshot_index(dest)
    }
}

/// Everything a run needs, moved onto its worker thread.
struct RunJob {
    name: String,
    source: PathBuf,
    handle: Arc<SessionHandle>,
    scheduler: Arc<Scheduler>,
    progress: Arc<Mutex<Option<RsyncProgress>>>,
    archive_folder: Arc<Mutex<Option<String>>>,
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    /// Cleared when the run returns (it may have been detached as stuck).
    not_responding: Arc<AtomicBool>,
    store: PathBuf,
    saved: Arc<Mutex<SavedState>>,
}

impl RunJob {
    /// Keep what a run left in the session's store, for the next launch (N1).
    fn save_result(
        &self,
        rec: &RunRecord,
        error: Option<(String, String)>,
        (originals, rejects_excluded): (u64, u64),
    ) {
        let st = self.scheduler.status();
        let mut saved = lock(&self.saved);
        if !matches!(rec, RunRecord::Busy(_) | RunRecord::Cancelled(_)) {
            (saved.last_error, saved.last_error_detail) = error.unzip();
        }
        if matches!(rec, RunRecord::Succeeded(_)) {
            saved.last_success_ms = st.last_success.map(ms);
        }
        if matches!(rec, RunRecord::Succeeded(_) | RunRecord::Incomplete(..)) {
            saved.originals = originals;
            saved.rejects_excluded = rejects_excluded;
        }
        saved.target = self.root.to_string_lossy().into_owned();
        saved.pending = st.pending_changes;
        saved.save(&self.store);
    }
}

impl ArchiveManager {
    fn with(
        config: ArchiveConfig,
        check: Arc<CheckFn>,
        runner: Arc<dyn CommandRunner>,
        rsync_paths: Vec<PathBuf>,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, DsError> {
        Self::with_timing(config, check, runner, rsync_paths, clock, Timing::default())
    }

    fn with_timing(
        config: ArchiveConfig,
        check: Arc<CheckFn>,
        runner: Arc<dyn CommandRunner>,
        rsync_paths: Vec<PathBuf>,
        clock: Arc<dyn Clock>,
        timing: Timing,
    ) -> Result<Arc<Self>, DsError> {
        let activity = Arc::new(AtomicU64::new(0));
        let runner: Arc<dyn CommandRunner> = Arc::new(ActivityRunner {
            inner: runner,
            last: activity.clone(),
            clock: clock.clone(),
        });
        let inner = Arc::new(Inner {
            cfg: Mutex::new(config),
            features: crate::features::FeatureGate::default(),
            entries: Mutex::default(),
            runs: Mutex::default(),
            target: Mutex::new(TargetState {
                offline_backoff: timing.offline_base,
                ..TargetState::default()
            }),
            rsync: Mutex::default(),
            active: Mutex::default(),
            amazon: Mutex::default(),
            amazon_cfg: Mutex::default(),
            amazon_serial: Mutex::default(),
            amazon_worker: Mutex::default(),
            amazon_swept_at: Mutex::default(),
            immich: Mutex::default(),
            immich_archived: Mutex::default(),
            object_http: Mutex::default(),
            object_creds: Mutex::default(),
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            recheck: AtomicBool::new(true),
            wake: (Mutex::new(false), Condvar::new()),
            check,
            runner,
            activity,
            rsync_paths,
            clock,
            timing,
        });
        let driver_inner = inner.clone();
        let driver = std::thread::Builder::new()
            .name("lt-archive".into())
            .spawn(move || driver_inner.drive())
            .map_err(DsError::from)?;
        Ok(Arc::new(Self {
            inner,
            driver: Mutex::new(Some(driver)),
        }))
    }

    fn entry_index(&self, handle: &Arc<SessionHandle>) -> Option<usize> {
        let key = entry_key(handle);
        lock(&self.inner.entries).iter().position(|e| e.key == key)
    }

    /// The entry of `handle`'s session (one per session folder: a reopened
    /// session's new handle takes over its schedule), created if needed.
    fn with_entry<T>(&self, handle: &Arc<SessionHandle>, f: impl FnOnce(&mut Entry) -> T) -> T {
        let key = entry_key(handle);
        let mut entries = lock(&self.inner.entries);
        let i = if let Some(i) = entries.iter().position(|e| e.key == key) {
            entries[i].handle = handle.clone();
            i
        } else {
            entries.push(self.inner.new_entry(handle.clone(), key));
            self.inner.poke();
            entries.len() - 1
        };
        f(&mut entries[i])
    }

    pub(crate) fn feature_gate(&self) -> &crate::features::FeatureGate {
        &self.inner.features
    }

    /// Turn Immich publishing on (sidecars before each run, album sync queue)
    /// or off (the queue is dropped).
    pub(crate) fn set_immich_publish(&self, publish: Option<crate::immich::ArchivePublish>) {
        if publish.is_none() {
            lock(&self.inner.immich_archived).clear();
        }
        *lock(&self.inner.immich) = publish;
    }

    /// The archive target's root, and the sessions to sync albums for: those
    /// archived since the last sync (taken off the queue), plus with `every` all
    /// registered archivable ones. Each with the folder it archives.
    pub(crate) fn immich_sessions(
        &self,
        every: bool,
    ) -> (PathBuf, Vec<(Arc<SessionHandle>, PathBuf)>) {
        if !self.archives_to_folder() {
            // Immich indexes a folder: object storage gives it nothing to read.
            lock(&self.inner.immich_archived).clear();
            return (PathBuf::new(), Vec::new());
        }
        let root = PathBuf::from(&lock(&self.inner.cfg).target);
        let mut handles: Vec<Arc<SessionHandle>> =
            std::mem::take(&mut *lock(&self.inner.immich_archived));
        if every {
            for e in lock(&self.inner.entries).iter() {
                if !handles.iter().any(|h| Arc::ptr_eq(h, &e.handle)) {
                    handles.push(e.handle.clone());
                }
            }
        }
        let sessions = handles
            .into_iter()
            .filter_map(|h| h.archive_source().ok().map(|s| (h, s)))
            .collect();
        (root, sessions)
    }

    /// Put sessions back on the album sync queue (a sync that couldn't reach
    /// Immich tries them again).
    pub(crate) fn immich_requeue(&self, handles: &[Arc<SessionHandle>]) {
        for h in handles {
            self.inner.queue_album_sync(h);
        }
    }

    /// Sessions waiting for an album sync.
    pub(crate) fn immich_queued(&self) -> usize {
        lock(&self.inner.immich_archived).len()
    }

    /// The archive goes to a folder (which Immich can index), not to object storage.
    pub(crate) fn archives_to_folder(&self) -> bool {
        lock(&self.inner.cfg).object_store.is_none()
    }
}

impl Inner {
    fn scheduler_config(&self) -> SchedulerConfig {
        let c = lock(&self.cfg);
        SchedulerConfig {
            interval: Duration::from_secs(c.interval_secs.max(1)),
            debounce: Duration::from_secs(c.debounce_secs),
            lock_dir: c
                .lock_dir
                .as_ref()
                .map(PathBuf::from)
                .or_else(lt_archive::scheduler::default_lock_dir),
            poll: Duration::from_millis(250),
            ..SchedulerConfig::default()
        }
    }

    fn new_entry(&self, handle: Arc<SessionHandle>, key: PathBuf) -> Entry {
        let source = handle.archive_source();
        let name = source.as_ref().map_or_else(
            |_| handle.root().unwrap_or_default(),
            |p| p.to_string_lossy().into_owned(),
        );
        let name = Path::new(&name)
            .file_name()
            .map_or(name.clone(), |n| n.to_string_lossy().into_owned());
        let target = lock(&self.cfg).target_key();
        let imported = handle.is_imported();
        let store = PathBuf::from(handle.root().unwrap_or_default());
        let scheduler = Arc::new(Scheduler::new(
            self.scheduler_config(),
            self.clock.clone(),
            target.clone(),
        ));
        let saved = if source.is_ok() {
            // N1: what an earlier launch left. Changes it never archived stay
            // pending; a failed last run, changes left, or an archive made
            // before this state was saved get a catch-up run as soon as the
            // target answers. (A session never archived simply says so.)
            let saved = SavedState::load(&store, &target);
            scheduler.note_changes(saved.pending);
            let archived_before = store.join(IDENTITY_REL).is_file();
            if !saved.clean()
                && (saved.last_error.is_some() || saved.pending > 0 || archived_before)
            {
                scheduler.request_now();
            }
            saved
        } else {
            SavedState::default()
        };
        Entry {
            key,
            name,
            source,
            handle,
            scheduler,
            progress: Arc::default(),
            archive_folder: Arc::default(),
            closed: false,
            not_responding: Arc::default(),
            imported,
            // Launch catch-up: a newly registered imported session gets a hand-off pass.
            amazon_due: imported,
            amazon: None,
            store,
            saved: Arc::new(Mutex::new(saved)),
        }
    }

    /// The configured destination (for a run). A folder needs rsync (detected on first
    /// use); object storage needs the app's HTTP and the keys.
    fn destination(&self) -> Result<Arc<dyn Destination>, ArchiveError> {
        let cfg = lock(&self.cfg).clone();
        match &cfg.object_store {
            Some(o) => {
                let not_usable = |reason: &str| ArchiveError::TargetNotUsable {
                    root: PathBuf::from(o.config().location()),
                    reason: reason.into(),
                };
                let http = lock(&self.object_http)
                    .clone()
                    .ok_or_else(|| not_usable("object storage isn't connected yet"))?;
                let creds = lock(&self.object_creds)
                    .clone()
                    .ok_or_else(|| not_usable("the access key isn't set"))?;
                Ok(Arc::new(ObjectDestination::new(o.config(), http, creds)?))
            }
            // No rsync: the run fails before anything is claimed on the target.
            None => Ok(Arc::new(folder_destination(
                Path::new(&cfg.target),
                self.target_volume(),
                Some(self.rsync().map_err(|_| ArchiveError::RsyncNotFound)?),
                protected(),
            ))),
        }
    }

    /// Check the destination keyed `root` without waiting longer than `timeout`.
    fn check_target(&self, root: &Path, timeout: Duration) -> TargetCheck {
        if lock(&self.cfg).object_store.is_none() {
            return check_with(root, self.target_volume(), timeout, self.check.clone());
        }
        let dest = self.destination();
        let mut c = check_bounded(root, timeout, move |_| {
            dest.and_then(|d| d.check(&AtomicBool::new(false)))
                .map_err(reason)
        });
        c.kind = Some(DestinationKind::ObjectStorage.as_str().into());
        c
    }

    fn poke(&self) {
        let (m, cv) = &self.wake;
        *lock(m) = true;
        cv.notify_all();
    }

    fn sleep(&self, d: Duration) {
        let (m, cv) = &self.wake;
        let mut woken = lock(m);
        if !*woken {
            woken = cv
                .wait_timeout(woken, d)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *woken = false;
    }

    /// Add a run to the history; a repeated "busy" for the same session is
    /// not recorded again (A-L1: it would flush real failures out of the list).
    fn record(&self, run: ArchiveRun) {
        let mut runs = lock(&self.runs);
        if run.outcome == "busy"
            && runs
                .front()
                .is_some_and(|r| r.outcome == "busy" && r.session == run.session)
        {
            return;
        }
        runs.push_front(run);
        runs.truncate(MAX_RUNS);
    }

    fn rsync(&self) -> Result<RsyncRunner, String> {
        let mut r = lock(&self.rsync);
        if r.is_none() {
            *r = Some(
                RsyncRunner::detect_from(self.runner.clone(), &self.rsync_paths)
                    .map_err(|e| e.to_string()),
            );
        }
        r.clone().unwrap_or_else(|| Err("no rsync".into()))
    }

    fn target_available(&self) -> bool {
        lock(&self.target)
            .check
            .as_ref()
            .is_some_and(|c| c.available)
    }

    /// Driver loop: watch the run in flight, check the target when needed,
    /// start what is due, hand off to Amazon, sleep.
    fn drive(self: &Arc<Self>) {
        while !self.stop.load(Ordering::Relaxed) {
            let (enabled, root, timeout) = {
                let c = lock(&self.cfg);
                (
                    c.enabled && self.features.allows(crate::features::ARCHIVE),
                    PathBuf::from(c.target_key()),
                    Duration::from_millis(u64::from(c.check_timeout_ms)),
                )
            };
            self.watch_active(&root, timeout);
            if enabled {
                self.maybe_check(&root, timeout);
                if self.target_available() && lock(&self.active).is_none() {
                    self.start_due(&root);
                }
                self.drop_closed();
            }
            if self.features.allows(crate::features::AMAZON_PHOTOS) {
                self.maybe_amazon();
            }
            self.sleep(self.wait_time(enabled));
        }
    }

    /// Check the target now? Offline: with backoff, whatever is due (A-M4).
    /// Something due: at most every second. Idle: every `idle_check` (A-L4).
    /// "Sync now", a mount change or new settings check at once.
    fn maybe_check(&self, root: &Path, timeout: Duration) {
        let now = self.clock.monotonic();
        let due_any = lock(&self.entries)
            .iter()
            .any(|e| e.source.is_ok() && e.scheduler.due().is_some());
        let forced = self.recheck.swap(false, Ordering::Relaxed);
        let (last, available, backoff) = {
            let t = lock(&self.target);
            (
                t.checked_at,
                t.check.as_ref().is_some_and(|c| c.available),
                t.offline_backoff,
            )
        };
        let since = last.map(|l| now.saturating_sub(l));
        let wanted = forced
            || since.is_none_or(|s| {
                if !available {
                    s >= backoff
                } else if due_any && lock(&self.active).is_none() {
                    s >= Duration::from_secs(1)
                } else {
                    s >= self.timing.idle_check
                }
            });
        if !wanted {
            return;
        }
        let check = self.check_target(root, timeout);
        let mut t = lock(&self.target);
        t.offline_backoff = if check.available {
            self.timing.offline_base
        } else if t.check.as_ref().is_some_and(|c| !c.available) {
            (t.offline_backoff * 2).min(self.timing.offline_max)
        } else {
            t.offline_backoff
        };
        t.check = Some(check);
        t.checked_at = Some(self.clock.monotonic());
    }

    /// A run of `handle`'s session archived it: its albums are synced next
    /// (only while Immich publishing is on, so no closed session is kept alive).
    fn queue_album_sync(&self, handle: &Arc<SessionHandle>) {
        // With object storage Immich has no folder to index: no album sync.
        if lock(&self.immich).is_none()
            || !self.features.allows(crate::features::IMMICH)
            || lock(&self.cfg).object_store.is_some()
        {
            return;
        }
        let mut q = lock(&self.immich_archived);
        if !q.iter().any(|h| Arc::ptr_eq(h, handle)) {
            q.push(handle.clone());
        }
    }

    /// The configured target's recorded volume.
    fn target_volume(&self) -> Option<VolumeId> {
        lock(&self.cfg)
            .target_volume
            .as_deref()
            .and_then(VolumeId::from_token)
    }

    /// How long the driver sleeps: no spinning while offline or busy.
    fn wait_time(&self, enabled: bool) -> Duration {
        let now = self.clock.monotonic();
        let mut wait = self.timing.idle_check;
        if lock(&self.active).is_some() || lock(&self.amazon_worker).is_some() {
            wait = wait.min(Duration::from_millis(500));
        }
        if enabled {
            let t = lock(&self.target);
            let checked = t.checked_at.unwrap_or_default();
            if t.check.as_ref().is_some_and(|c| c.available) {
                let next_run = lock(&self.entries)
                    .iter()
                    .filter(|e| e.source.is_ok())
                    .map(|e| e.scheduler.next_wakeup())
                    .min()
                    .unwrap_or(self.timing.idle_check);
                wait = wait
                    .min(next_run)
                    .min((checked + self.timing.idle_check).saturating_sub(now));
            } else {
                wait = wait.min((checked + t.offline_backoff).saturating_sub(now));
            }
        }
        if let Some(at) = *lock(&self.amazon_swept_at) {
            wait = wait.min((at + self.timing.amazon_interval).saturating_sub(now));
        }
        wait.max(Duration::from_millis(50))
    }

    /// Start the first due session's run on its own worker thread.
    fn start_due(self: &Arc<Self>, root: &Path) {
        let lock_dir = self.scheduler_config().lock_dir;
        let mut entries = lock(&self.entries);
        let Some(e) = entries
            .iter_mut()
            .find(|e| e.source.is_ok() && e.scheduler.due().is_some())
        else {
            return;
        };
        let target_lock = TargetLock::try_acquire(root, lock_dir.as_deref());
        let Some(ticket) = e.scheduler.start() else {
            return;
        };
        let Some(target_lock) = target_lock else {
            let rec = e.scheduler.mark_busy(ticket);
            let name = e.name.clone();
            drop(entries);
            self.record(ArchiveRun {
                session: name,
                trigger: trigger_name(match rec {
                    RunRecord::Busy(t) => t,
                    _ => Trigger::OnDemand,
                })
                .into(),
                outcome: "busy".into(),
                message: Some("another archive run is using the target".into()),
                detail: None,
                started_ms: ms(self.clock.wall()),
                duration_ms: 0,
                warnings: vec![],
                problems: vec![],
            });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(self.cancel.load(Ordering::Relaxed)));
        let done = Arc::new(AtomicBool::new(false));
        let now = self.clock.monotonic();
        self.activity.store(dur_ms(now), Ordering::Relaxed);
        let job = RunJob {
            name: e.name.clone(),
            source: e.source.clone().unwrap_or_default(),
            handle: e.handle.clone(),
            scheduler: e.scheduler.clone(),
            progress: e.progress.clone(),
            archive_folder: e.archive_folder.clone(),
            root: root.to_path_buf(),
            cancel: cancel.clone(),
            not_responding: e.not_responding.clone(),
            store: e.store.clone(),
            saved: e.saved.clone(),
        };
        // rsync is detected once (a local `rsync --version`), so the watchdog
        // knows whether silence means trouble.
        // Object storage reports progress after every request.
        let progress_capable = lock(&self.cfg).object_store.is_some()
            || self.rsync().is_ok_and(|r| r.flavor().supports_progress2());
        *lock(&self.active) = Some(Active {
            name: e.name.clone(),
            trigger: ticket.trigger,
            started: now,
            started_wall: self.clock.wall(),
            cancel,
            done: done.clone(),
            not_responding: e.not_responding.clone(),
            progress_capable,
            probed_at: None,
            _lock: target_lock,
        });
        drop(entries);
        let inner = self.clone();
        let spawned = std::thread::Builder::new()
            .name("lt-archive-run".into())
            .spawn(move || {
                inner.run_job(job, ticket);
                done.store(true, Ordering::Release);
                inner.poke();
            });
        if spawned.is_err() {
            // The ticket went with the closure: nothing ran. Forget the run.
            lock(&self.active).take();
        }
    }

    /// One archive run (on its worker thread): identity, claimed folder,
    /// snapshot, rsync. Records the outcome.
    #[allow(clippy::too_many_lines)] // one run, start to record
    fn run_job(&self, job: RunJob, ticket: lt_archive::Ticket) {
        let started = self.clock.wall();
        let t0 = self.clock.monotonic();
        let trigger = ticket.trigger;
        let mut warnings = Vec::new();
        let archive_rejected = lock(&self.cfg).archive_rejected;
        let (originals, rejected) = job.handle.original_counts().unwrap_or_default();
        let excluded = if archive_rejected { 0 } else { rejected };
        let mut claimed_at: Option<(Arc<dyn Destination>, lt_archive::ClaimedDest)> = None;
        let mut result = (|| -> Result<(), ArchiveError> {
            let dest = self.destination()?;
            let store = PathBuf::from(job.handle.root().unwrap_or_default());
            let io_err = |e| ArchiveError::Io {
                path: store.join(IDENTITY_REL),
                source: e,
            };
            let mut identity = load_identity(&store).map_err(io_err)?;
            dest.check(&job.cancel)?;
            // Immich: the sidecars match the photos in this very run.
            let publish = lock(&self.immich).clone();
            if let Some(publish) = publish.filter(|_| self.features.allows(crate::features::IMMICH))
            {
                warnings.extend(crate::immich::before_archive(&job.handle, &publish));
            }
            let claimed = dest.claim(
                &job.source,
                &identity.id,
                identity.folder.as_deref(),
                &job.cancel,
            )?;
            if identity.folder.as_deref() != Some(claimed.folder.as_str()) {
                identity.folder = Some(claimed.folder.clone());
                save_identity(&store, &identity).map_err(io_err)?;
            }
            *lock(&job.archive_folder) = Some(claimed.folder.clone());
            claimed_at = Some((dest.clone(), claimed.clone()));
            let source = SessionSource {
                dir: job.source.clone(),
                // H7: rejected photos and the session Trash stay out of the archive.
                excludes: if archive_rejected {
                    Vec::new()
                } else {
                    job.handle.reject_excludes().unwrap_or_default()
                },
                // The archive keeps the session's current layout (no duplicate after a
                // Move to Selects; the displaced copy goes to the archive trash).
                mirror_folders: job.handle.archive_mirror_folders(),
                snapshotter: Some(Arc::new(HandleSnapshot(job.handle.clone()))),
            };
            let activity = self.activity.clone();
            let clock = self.clock.clone();
            let out = dest.archive(
                &source,
                &claimed,
                self.clock.wall(),
                &mut |p| {
                    activity.store(dur_ms(clock.monotonic()), Ordering::Relaxed);
                    *lock(&job.progress) = Some(p.clone());
                },
                &job.cancel,
            );
            *lock(&job.progress) = None;
            let out = out?;
            warnings = out.warnings;
            if out.problems.is_empty() {
                Ok(())
            } else {
                Err(ArchiveError::Incomplete {
                    problems: out.problems,
                })
            }
        })();
        if job.cancel.load(Ordering::Relaxed) && result.is_err() {
            result = Err(ArchiveError::Cancelled);
        }
        let object = lock(&self.cfg).object_store.is_some();
        // N1c: the error in words (the raw text stays available as detail),
        // with the originals the target verifiably lacks, when it answered.
        let error = result.as_ref().err().map(|e| {
            let missing = match (e, &claimed_at) {
                (
                    ArchiveError::Rsync { .. } | ArchiveError::Io { .. } | ArchiveError::Storage(_),
                    Some((dest, claimed)),
                ) => job
                    .handle
                    .archive_originals(archive_rejected)
                    .ok()
                    .and_then(|o| dest.missing(&job.source, claimed, &o)),
                _ => None,
            };
            let words = if object {
                object_error(e).unwrap_or_else(|| human_error(e, missing, || None))
            } else {
                human_error(e, missing, || free_space(&job.root))
            };
            (words, e.to_string())
        });
        let rec = job.scheduler.finish(ticket, result);
        if matches!(rec, RunRecord::Succeeded(_) | RunRecord::Incomplete(..)) {
            self.queue_album_sync(&job.handle);
        }
        job.not_responding.store(false, Ordering::Relaxed);
        job.save_result(&rec, error.clone(), (originals, excluded));
        let (message, detail) = error.map_or((None, None), |(h, raw)| (Some(h), Some(raw)));
        let (outcome, message, detail, problems) = match rec {
            RunRecord::Succeeded(_) => ("succeeded", None, None, vec![]),
            RunRecord::Incomplete(_, p) => ("incomplete", None, None, p),
            RunRecord::Failed(..) => ("failed", message, detail, vec![]),
            RunRecord::Unavailable(..) => ("offline", message, detail, vec![]),
            RunRecord::Busy(_) => ("busy", None, None, vec![]),
            RunRecord::Cancelled(_) => ("cancelled", None, None, vec![]),
        };
        self.record(ArchiveRun {
            session: job.name,
            trigger: trigger_name(trigger).into(),
            outcome: outcome.into(),
            message,
            detail,
            started_ms: ms(started),
            duration_ms: dur_ms(self.clock.monotonic().saturating_sub(t0)),
            warnings,
            problems,
        });
    }

    /// The watchdog: reap a finished run; detach one that stopped responding
    /// (A-H2) so the driver and the other sessions go on.
    fn watch_active(&self, root: &Path, timeout: Duration) {
        let now = self.clock.monotonic();
        let (silent, hard, probe) = {
            let mut active = lock(&self.active);
            let Some(a) = active.as_mut() else {
                return;
            };
            if a.done.load(Ordering::Acquire) {
                active.take();
                // The next due session may start right away (the target was just used).
                self.recheck.store(true, Ordering::Relaxed);
                return;
            }
            let last = Duration::from_millis(self.activity.load(Ordering::Relaxed)).max(a.started);
            let silent = now.saturating_sub(last);
            let hard = if a.progress_capable {
                self.timing.stall_hard_progress
            } else {
                self.timing.stall_hard_quiet
            };
            let probe = silent >= self.timing.stall_probe
                && a.probed_at
                    .is_none_or(|p| now.saturating_sub(p) >= self.timing.stall_probe);
            if probe {
                a.probed_at = Some(now);
            }
            (silent, hard, probe)
        };
        let reason = if silent >= hard {
            Some(format!(
                "no sign of life for {} s; the run was stopped and will be retried",
                silent.as_secs()
            ))
        } else if probe {
            // Bounded: never holds a lock while the check waits.
            let check = self.check_target(root, timeout);
            let bad = (!check.available).then(|| {
                format!(
                    "the archive target is not responding ({}); the run was stopped and will be retried",
                    check.reason.clone().unwrap_or_default()
                )
            });
            let mut t = lock(&self.target);
            t.check = Some(check);
            t.checked_at = Some(now);
            bad
        } else {
            None
        };
        let Some(reason) = reason else {
            return;
        };
        let Some(a) = lock(&self.active).take() else {
            return;
        };
        if a.done.load(Ordering::Acquire) {
            return; // it just finished after all
        }
        // Detached: the worker keeps its handles; when (if) it returns, it
        // records its own outcome and clears `not_responding`.
        a.cancel.store(true, Ordering::Relaxed);
        a.not_responding.store(true, Ordering::Relaxed);
        self.record(ArchiveRun {
            session: a.name,
            trigger: trigger_name(a.trigger).into(),
            outcome: "not responding".into(),
            message: Some(reason),
            detail: None,
            started_ms: ms(a.started_wall),
            duration_ms: dur_ms(now.saturating_sub(a.started)),
            warnings: vec![],
            problems: vec![],
        });
    }

    /// Closed sessions leave once nothing is pending (their final archive ran).
    fn drop_closed(&self) {
        lock(&self.entries).retain(|e| {
            let st = e.scheduler.status();
            !(e.closed && !st.running && e.scheduler.due().is_none() && st.pending_changes == 0)
        });
    }

    // ------------------------------------------------------------- amazon ---

    /// Start a hand-off pass for the imported sessions that want one (after
    /// an import, "Sync now", launch) or every `amazon_interval` for all.
    fn maybe_amazon(self: &Arc<Self>) {
        let Some((folder, manifest)) = lock(&self.amazon_cfg).clone() else {
            return;
        };
        {
            let mut w = lock(&self.amazon_worker);
            if w.as_ref().is_some_and(|d| !d.load(Ordering::Acquire)) {
                return; // one pass at a time
            }
            *w = None;
        }
        let now = self.clock.monotonic();
        let sweep = lock(&self.amazon_swept_at)
            .is_none_or(|at| now.saturating_sub(at) >= self.timing.amazon_interval);
        let jobs: Vec<(PathBuf, String, Arc<SessionHandle>)> = {
            let mut entries = lock(&self.entries);
            entries
                .iter_mut()
                .filter(|e| e.imported && (e.amazon_due || sweep))
                .map(|e| {
                    e.amazon_due = false;
                    (e.key.clone(), e.name.clone(), e.handle.clone())
                })
                .collect()
        };
        if sweep {
            *lock(&self.amazon_swept_at) = Some(now);
        }
        if jobs.is_empty() || self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let done = Arc::new(AtomicBool::new(false));
        *lock(&self.amazon_worker) = Some(done.clone());
        let inner = self.clone();
        let spawned = std::thread::Builder::new()
            .name("lt-amazon".into())
            .spawn(move || {
                for (key, name, handle) in jobs {
                    if inner.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let cancelled = || inner.cancel.load(Ordering::Relaxed);
                    let _ =
                        inner.mirror_session(&key, &name, &handle, &folder, &manifest, &cancelled);
                }
                done.store(true, Ordering::Release);
                inner.poke();
            });
        if spawned.is_err() {
            lock(&self.amazon_worker).take();
        }
    }

    /// Hand one session's verified originals to the Amazon Photos folder,
    /// serialized with every other hand-off in this process (A-M1). Records
    /// the status overall and for the session (A-L5).
    fn mirror_session(
        &self,
        key: &Path,
        album: &str,
        handle: &SessionHandle,
        folder: &Path,
        manifest: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<AmazonStatus, DsError> {
        let running = AmazonStatus {
            folder: Some(folder.to_string_lossy().into_owned()),
            running: true,
            session: Some(album.to_owned()),
            ..AmazonStatus::default()
        };
        self.set_amazon_status(key, &running);
        let _serial = lock(&self.amazon_serial);
        let r = self.mirror(handle, folder, manifest, album, cancelled);
        let status = match &r {
            Ok(s) => s.clone(),
            Err(e) => AmazonStatus {
                folder: Some(folder.to_string_lossy().into_owned()),
                running: false,
                last_run_ms: Some(ms(self.clock.wall())),
                error: Some(e.to_string()),
                folder_missing: !folder.is_dir(),
                session: Some(album.to_owned()),
                ..AmazonStatus::default()
            },
        };
        self.set_amazon_status(key, &status);
        r
    }

    fn set_amazon_status(&self, key: &Path, s: &AmazonStatus) {
        *lock(&self.amazon) = s.clone();
        if let Some(e) = lock(&self.entries).iter_mut().find(|e| e.key == key) {
            e.amazon = Some(s.clone());
        }
    }

    fn mirror(
        &self,
        handle: &SessionHandle,
        folder: &Path,
        manifest: &Path,
        album: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<AmazonStatus, DsError> {
        let include_rejected = lock(&self.cfg).amazon_rejected;
        let (originals, waiting, rejected) = handle.backup_originals(include_rejected)?;
        let originals: Vec<Original> = originals
            .into_iter()
            .map(|(path, hash)| Original { path, hash })
            .collect();
        let mut backup = AmazonBackup::open(folder, manifest).map_err(|e| DsError::Io {
            message: e.to_string(),
        })?;
        let r = backup
            .mirror_with(album, &originals, cancelled)
            .map_err(|e| DsError::Io {
                message: e.to_string(),
            })?;
        // "Excluded" counts only rejects the app doesn't have: one handed over before
        // it was rejected stays there (the hand-off never removes anything).
        let rejects_excluded = rejected
            .iter()
            .filter(|h| **h == lt_session::UNHASHED || backup.mirrored_path(h).is_none())
            .count() as u64;
        Ok(AmazonStatus {
            folder: Some(folder.to_string_lossy().into_owned()),
            running: false,
            last_run_ms: Some(ms(self.clock.wall())),
            placed: (r.cloned.len() + r.copied.len()) as u64,
            cloned: r.cloned.len() as u64,
            already_mirrored: r.already_mirrored as u64,
            skipped_unsupported: r.skipped_unsupported.len() as u64,
            waiting,
            failed: r
                .failed
                .iter()
                .map(|(p, e)| format!("{}: {e}", p.display()))
                .collect(),
            error: r.cancelled.then(|| "cancelled".into()),
            folder_missing: false,
            session: Some(album.to_owned()),
            rejects_excluded,
        })
    }

    fn overview(&self) -> ArchiveOverview {
        let enabled = lock(&self.cfg).enabled;
        let (rsync, rsync_error) = match &*lock(&self.rsync) {
            Some(Ok(r)) => (Some(rsync_label(r)), None),
            Some(Err(e)) => (None, Some(e.clone())),
            None => (None, None),
        };
        let target_path = lock(&self.cfg).target_key();
        let target = lock(&self.target).check.clone().unwrap_or(TargetCheck {
            path: target_path,
            available: false,
            timed_out: false,
            reason: Some("checking…".into()),
            fs_type: None,
            kind: None,
        });
        let sessions = lock(&self.entries)
            .iter()
            .map(|e| {
                let st = e.scheduler.status();
                let p = lock(&e.progress).clone();
                let saved = lock(&e.saved).clone();
                SessionArchiveStatus {
                    name: e.name.clone(),
                    source: e.source.as_ref().map_or_else(
                        |_| e.handle.root().unwrap_or_default(),
                        |p| p.to_string_lossy().into_owned(),
                    ),
                    not_archivable: e.source.as_ref().err().cloned(),
                    archive_folder: lock(&e.archive_folder).clone(),
                    // N1: this launch's runs, else what the last launch saved.
                    last_success_ms: st.last_success.map(ms).or(saved.last_success_ms),
                    last_error: saved.last_error,
                    last_error_detail: saved.last_error_detail,
                    problems: st.problems,
                    originals: saved.originals,
                    rejects_excluded: saved.rejects_excluded,
                    pending_changes: st.pending_changes,
                    behind_since_ms: st.behind_since.map(ms),
                    running: st.running,
                    not_responding: e.not_responding.load(Ordering::Relaxed),
                    percent: p.as_ref().map(|p| p.percent),
                    bytes_per_sec: p.as_ref().map(|p| p.bytes_per_sec),
                    eta_secs: p.as_ref().and_then(|p| p.eta).map(|d| d.as_secs()),
                    consecutive_failures: st.consecutive_failures,
                    retry_in_ms: st.retry_in.map(dur_ms),
                    amazon: e.amazon.clone(),
                }
            })
            .collect();
        ArchiveOverview {
            enabled,
            target,
            rsync,
            rsync_error,
            sessions,
            runs: lock(&self.runs).iter().cloned().collect(),
            amazon: lock(&self.amazon).clone(),
        }
    }
}

#[uniffi::export]
impl ArchiveManager {
    /// Start background archiving with `config` (the driver thread starts now).
    #[uniffi::constructor(name = "new")]
    pub fn ffi_new(config: ArchiveConfig) -> Result<Arc<Self>, DsError> {
        guard(|| {
            let rsync_paths = match &config.rsync_path {
                Some(p) => vec![PathBuf::from(p)],
                None => lt_archive::rsync::DEFAULT_RSYNC_PATHS
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            };
            Self::with(
                config,
                Arc::new(real_check),
                Arc::new(SystemRunner),
                rsync_paths,
                Arc::new(SystemClock::default()),
            )
        })
    }

    /// New settings. Only a change of the target or the schedule touches the
    /// schedulers, and even then everything they know is kept: pending
    /// changes, last success, the last error and its backoff, a run in
    /// progress (A-H4).
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn set_config(&self, config: ArchiveConfig) -> Result<(), DsError> {
        guard(|| {
            let old = std::mem::replace(&mut *lock(&self.inner.cfg), config.clone());
            let schedule_changed = old.target_key() != config.target_key()
                || old.interval_secs != config.interval_secs
                || old.debounce_secs != config.debounce_secs
                || old.lock_dir != config.lock_dir;
            if schedule_changed {
                let cfg = self.inner.scheduler_config();
                for e in lock(&self.inner.entries).iter() {
                    e.scheduler.reconfigure(cfg.clone(), config.target_key());
                }
            }
            if old.target_key() != config.target_key()
                || old.object_store != config.object_store
                || old.enabled != config.enabled
            {
                let mut t = lock(&self.inner.target);
                t.check = None;
                t.checked_at = None;
                t.offline_backoff = self.inner.timing.offline_base;
            }
            self.inner.recheck.store(true, Ordering::Relaxed);
            self.inner.poke();
            Ok(())
        })
    }

    /// Where the Amazon Photos hand-off goes (`None`: off) and where its
    /// manifest lives (outside that folder). The folder is never created.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn set_amazon(&self, folder: Option<String>, manifest: String) -> Result<(), DsError> {
        guard(|| {
            let next = folder
                .filter(|f| !f.is_empty())
                .map(|f| (PathBuf::from(f), PathBuf::from(&manifest)));
            let mut cur = lock(&self.inner.amazon_cfg);
            if *cur != next {
                *cur = next;
                drop(cur);
                // A new folder: catch up every imported session.
                *lock(&self.inner.amazon_swept_at) = None;
                self.inner.poke();
            }
            Ok(())
        })
    }

    /// Object storage: the app's HTTP and the keys from the Keychain (`None`: not
    /// set; runs then say so). Used while [`ArchiveConfig::object_store`] is set.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn set_object_store(
        &self,
        http: Arc<dyn ArchiveHttp>,
        credentials: Option<ObjectCredentials>,
    ) -> Result<(), DsError> {
        guard(|| {
            let creds = credentials.as_ref().map(ObjectCredentials::credentials);
            let changed = *lock(&self.inner.object_creds) != creds;
            *lock(&self.inner.object_http) = Some(Arc::new(HttpBridge(http)));
            *lock(&self.inner.object_creds) = creds;
            if changed {
                self.inner.recheck.store(true, Ordering::Relaxed);
                self.inner.poke();
            }
            Ok(())
        })
    }

    /// A volume was mounted or unmounted: check the target again now.
    pub fn volumes_changed(&self) -> Result<(), DsError> {
        guard(|| {
            self.inner.recheck.store(true, Ordering::Relaxed);
            self.inner.poke();
            Ok(())
        })
    }

    /// Start scheduling `session` (idempotent). Returns whether it can be
    /// archived (a session on a card or read-only volume can't).
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn register_session(&self, session: Arc<SessionHandle>) -> Result<bool, DsError> {
        guard(|| Ok(self.with_entry(&session, |e| e.source.is_ok())))
    }

    /// `n` files changed in `session` (ratings, edits, imports).
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn note_changes(&self, session: Arc<SessionHandle>, n: u64) -> Result<(), DsError> {
        guard(|| {
            self.with_entry(&session, |e| {
                e.closed = false;
                if e.source.is_ok() {
                    e.scheduler.note_changes(n);
                    e.note_behind();
                }
            });
            self.inner.poke();
            Ok(())
        })
    }

    /// An import into `session` finished: archive it now and hand its
    /// originals to Amazon Photos (when a folder is set).
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn after_import(&self, session: Arc<SessionHandle>, files: u64) -> Result<(), DsError> {
        guard(|| {
            self.with_entry(&session, |e| {
                e.closed = false;
                // Imported into: its originals are handed to Amazon from now on.
                e.imported = true;
                e.amazon_due = true;
                if e.source.is_ok() {
                    e.scheduler.note_changes(files.max(1));
                    e.scheduler.request_now();
                    e.note_behind();
                }
            });
            self.inner.poke();
            Ok(())
        })
    }

    /// The session is closing: archive it now, then stop scheduling it.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn session_closed(&self, session: Arc<SessionHandle>) -> Result<(), DsError> {
        guard(|| {
            if self.entry_index(&session).is_some() {
                self.with_entry(&session, |e| {
                    e.closed = true;
                    if e.source.is_ok() {
                        e.scheduler.session_closed();
                    }
                });
            }
            self.inner.poke();
            Ok(())
        })
    }

    /// "Sync now": check the target, archive every registered session
    /// (ignoring debounce and backoff) and catch up the Amazon hand-off.
    pub fn sync_now(&self) -> Result<(), DsError> {
        guard(|| {
            for e in lock(&self.inner.entries).iter_mut() {
                e.amazon_due = e.imported;
                if e.source.is_ok() {
                    e.scheduler.request_now();
                }
            }
            self.inner.recheck.store(true, Ordering::Relaxed);
            self.inner.poke();
            Ok(())
        })
    }

    /// What the badge and popover show. Never touches the file system.
    pub fn status(&self) -> Result<ArchiveOverview, DsError> {
        guard(|| Ok(self.inner.overview()))
    }

    /// Mirror `session`'s originals into the Amazon Photos folder now
    /// (`<folder>/<album>/`; manifest kept outside it) and wait for it.
    /// Files are placed once per content hash, ever. Serialized with the
    /// background hand-off; `token` cancels it between files.
    pub async fn mirror_to_amazon(
        self: Arc<Self>,
        session: Arc<SessionHandle>,
        folder: String,
        manifest: String,
        album: String,
        token: Arc<CancelToken>,
    ) -> Result<AmazonStatus, DsError> {
        if !self.inner.features.allows(crate::features::AMAZON_PHOTOS) {
            return Err(DsError::invalid(
                "Amazon Photos is turned off (Settings ▸ Plugins)",
            ));
        }
        let inner = self.inner.clone();
        crate::jobs::run_job_on_thread("lt-amazon", token, move |cancelled| {
            let key = entry_key(&session);
            let quit = || cancelled() || inner.cancel.load(Ordering::Relaxed);
            inner.mirror_session(
                &key,
                &album,
                &session,
                Path::new(&folder),
                Path::new(&manifest),
                &quit,
            )
        })
        .await
    }

    /// Stop: cancel a running rsync (the process is killed) and the Amazon
    /// hand-off, then wait up to `timeout_ms` in total for the driver, the
    /// run (a run stuck in a hung mount is left behind) and, briefly, the
    /// hand-off. Returns whether everything stopped in time.
    pub fn shutdown(&self, timeout_ms: u32) -> Result<bool, DsError> {
        guard(|| {
            let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
            self.inner.stop.store(true, Ordering::Relaxed);
            self.inner.cancel.store(true, Ordering::Relaxed);
            // N1: what isn't archived is known at the next launch.
            for e in lock(&self.inner.entries).iter() {
                e.save_pending();
            }
            let run_done = lock(&self.inner.active).as_ref().map(|a| {
                a.cancel.store(true, Ordering::Relaxed);
                a.done.clone()
            });
            self.inner.poke();
            let mut stopped = true;
            if let Some(driver) = lock(&self.driver).take() {
                let (tx, rx) = mpsc::channel();
                let _ = std::thread::Builder::new()
                    .name("lt-archive-join".into())
                    .spawn(move || {
                        let _ = driver.join();
                        let _ = tx.send(());
                    });
                stopped = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .is_ok();
            }
            let wait_for = |done: &AtomicBool, until: Instant| {
                while !done.load(Ordering::Acquire) {
                    if Instant::now() >= until {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                true
            };
            if let Some(done) = run_done {
                stopped &= wait_for(&done, deadline);
            }
            let amazon = lock(&self.inner.amazon_worker).clone();
            if let Some(done) = amazon {
                let until = deadline.min(Instant::now() + self.inner.timing.amazon_shutdown_wait);
                stopped &= wait_for(&done, until);
            }
            Ok(stopped)
        })
    }
}

impl Drop for ArchiveManager {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.cancel.store(true, Ordering::Relaxed);
        if let Some(a) = lock(&self.inner.active).as_ref() {
            a.cancel.store(true, Ordering::Relaxed);
        }
        self.inner.poke();
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod feature_tests;
