//! S3-compatible object storage as an archive destination (AWS S3, Backblaze B2,
//! Wasabi, Cloudflare R2, `MinIO`): [`ObjectDestination`].
//!
//! **Layout.** Each session lands under `<prefix><session folder>/<relative path>`
//! (`luxtable/Iceland 2026/Capture/DSC0001.ARW`), the folder claimed with an owner
//! marker (`.lt-archive-owner`) exactly as on a folder target. A replaced object's
//! previous version is copied to `<prefix>.lt-archive-trash/<UTC timestamp>/<session
//! folder>/<relative path>` first. Settings ▸ Archive's connection test leaves one
//! small object at `<prefix>.lt-archive-test/connection-test.txt`.
//!
//! **Never deletes.** The client has no `DeleteObject`; an object is only ever
//! overwritten after its previous content was copied into the trash (and that copy
//! checked). The DB snapshot is the exception the folder backend makes too: it is a
//! derived copy rewritten every run, replaced in place.
//!
//! **Unchanged files are skipped.** Each object carries the file's BLAKE3 as user
//! metadata (`x-amz-meta-lt-blake3`). A run lists the session's keys once; a file whose
//! size and mtime match what the last run uploaded (a local cache in `.lt/cache`) and
//! whose object is listed with that size is skipped without a request. Any other file
//! is hashed and compared with `HeadObject`'s metadata: the hash decides.
//!
//! **Integrity.** Every request is signed with the body's SHA-256
//! (`x-amz-content-sha256`), which the service checks, and uploads carry
//! `Content-MD5` (per part for multipart), which every S3-compatible service tested
//! for honours (`x-amz-checksum-*` is not portable: B2 and R2 reject or ignore some
//! of them). The answer is verified: a single-part `ETag` must be the MD5, a multipart
//! one `MD5(part MD5s)-N` (when the service uses those forms), and a final
//! `HeadObject` must show the right size and BLAKE3.
//!
//! **Large files** are uploaded in parts of [`PART_SIZE`] (more for files over
//! 80 GB, to stay under 10 000 parts), read from disk one part at a time, so memory
//! stays bounded. A failed or cancelled upload is aborted (`AbortMultipartUpload`
//! drops the parts; there is no object yet, so nothing the user had is touched).
//!
//! Requests go through the [`http::Transport`] seam (`URLSession` in the app).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ArchiveError;
use crate::archiver::{ArchiveOutcome, SNAPSHOT_REL, TRASH_DIR, take_snapshot, trash_stamp};
use crate::dest::{Destination, DestinationKind, SessionSource};
use crate::exclude::Excludes;
use crate::rsync::{LEGACY_PARTIAL_DIR, PARTIAL_DIR, RsyncProgress};
use crate::target::{
    ARCHIVE_OWNER_FILE, ClaimedDest, DEFAULT_EXCLUDES, LEGACY_ARCHIVE_OWNER_FILE, plain_component,
};

pub mod client;
pub mod http;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod sigv4;

pub use client::{
    Client, Endpoint, META_BLAKE3, ObjectError, ObjectHead, PutIfAbsent, RetryPolicy,
};
pub use http::{Request, Response, Transport};
pub use sigv4::Credentials;

/// Upload part size, and the largest file sent in one `PutObject`. Small enough that
/// a part goes through a slow uplink well within the archive watchdog's patience.
pub const PART_SIZE: usize = 8 * 1024 * 1024;
/// S3's limit on parts per upload.
const MAX_PARTS: u64 = 10_000;
/// The largest object a single `CopyObject` can copy.
pub const MAX_COPY: u64 = 5 * 1024 * 1024 * 1024;
/// Under the prefix: where the connection test writes.
pub const TEST_DIR: &str = ".lt-archive-test";
const TEST_NAME: &str = "connection-test.txt";
/// Mtime (seconds) metadata, for people browsing the bucket.
const META_MTIME: &str = "x-amz-meta-lt-mtime";

/// Object storage settings (everything but the keys, which come from the Keychain).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectStoreConfig {
    /// `https://s3.eu-central-003.backblazeb2.com`, `http://127.0.0.1:9000`, … Empty:
    /// AWS S3 in `region`.
    pub endpoint: String,
    /// `us-east-1`, `eu-central-003`, `auto` (R2). Empty: `us-east-1`.
    pub region: String,
    pub bucket: String,
    /// Key prefix ("path"): `luxtable/` puts sessions at `luxtable/<session>/…`.
    pub prefix: String,
    /// Path-style URLs (`https://host/bucket/key`), for `MinIO` and some others.
    pub path_style: bool,
}

impl ObjectStoreConfig {
    /// The region, defaulted.
    pub fn region(&self) -> String {
        let r = self.region.trim();
        if r.is_empty() {
            "us-east-1".into()
        } else {
            r.to_owned()
        }
    }

    /// The prefix as keys use it: no leading `/`, single `/`s, ending in `/` (or
    /// empty). `Err` for `.`/`..` segments.
    pub fn normalized_prefix(&self) -> Result<String, String> {
        let segments: Vec<&str> = self
            .prefix
            .trim()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segments.iter().any(|s| *s == "." || *s == "..") {
            return Err("the path can't contain . or .. folders".into());
        }
        Ok(segments.iter().fold(String::new(), |mut p, s| {
            p.push_str(s);
            p.push('/');
            p
        }))
    }

    /// `s3://bucket/prefix`, with ` at <host>` for a service other than AWS (also for
    /// settings that don't validate yet: then as typed).
    pub fn location(&self) -> String {
        let prefix = self
            .normalized_prefix()
            .unwrap_or_else(|_| self.prefix.trim().to_owned());
        let mut s = format!("s3://{}/{prefix}", self.bucket.trim());
        if !self.endpoint.trim().is_empty() {
            let host = self
                .endpoint()
                .map_or_else(|_| self.endpoint.trim().to_owned(), |e| e.host);
            let _ = write!(s, " at {host}");
        }
        s
    }

    /// Identifies the archive across settings changes and processes (lock files, the
    /// saved state): `s3://<host>/<bucket>/<prefix>`.
    pub fn lock_key(&self) -> String {
        let host = self
            .endpoint()
            .map_or_else(|_| self.endpoint.trim().to_owned(), |e| e.host);
        let prefix = self
            .normalized_prefix()
            .unwrap_or_else(|_| self.prefix.trim().to_owned());
        format!("s3://{host}/{}/{prefix}", self.bucket.trim())
    }

    /// Where requests go, or what is wrong with the settings (in words).
    pub fn endpoint(&self) -> Result<Endpoint, String> {
        let bucket = self.bucket.trim();
        let valid_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
        if !(3..=63).contains(&bucket.len())
            || !bucket.chars().all(valid_char)
            || bucket.starts_with(['.', '-'])
            || bucket.ends_with(['.', '-'])
        {
            return Err("enter a bucket name (3–63 letters, digits, dots or hyphens)".into());
        }
        let region = self.region();
        if !region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err("the region is letters, digits and hyphens (e.g. us-east-1)".into());
        }
        let raw = self.endpoint.trim().trim_end_matches('/');
        let (scheme, host) = if raw.is_empty() {
            ("https".to_owned(), format!("s3.{region}.amazonaws.com"))
        } else {
            let (scheme, rest) = match raw.split_once("://") {
                Some((s, r)) => (s.to_ascii_lowercase(), r),
                None => ("https".to_owned(), raw),
            };
            if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
                return Err(
                    "the endpoint is a server address only, e.g. https://s3.example.com".into(),
                );
            }
            let host = rest.to_ascii_lowercase();
            let (name, port) = host.rsplit_once(':').unwrap_or((&host, ""));
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
                || (host.contains(':')
                    && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit())))
            {
                return Err(
                    "the endpoint is a server address only, e.g. https://s3.example.com".into(),
                );
            }
            let host = match (scheme.as_str(), host.rsplit_once(':')) {
                ("https", Some((h, "443"))) | ("http", Some((h, "80"))) => h.to_owned(),
                _ => host,
            };
            if let Some(why) = client::scheme_problem(&scheme, &host) {
                return Err(why);
            }
            (scheme, host)
        };
        if !self.path_style && scheme == "https" && bucket.contains('.') {
            return Err("a bucket name with dots needs path-style addressing".into());
        }
        Ok(Endpoint {
            scheme,
            host,
            bucket: bucket.to_owned(),
            region,
            path_style: self.path_style,
        })
    }
}

/// What the connection test did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionReport {
    /// The small object it left (and overwrites on the next test).
    pub test_key: String,
    pub location: String,
}

/// Why one file wasn't archived: a problem for that file, or a reason to stop the run.
enum FileError {
    /// Listed as not archived; the run goes on.
    Problem(String),
    Stop(ObjectError),
}

impl From<ObjectError> for FileError {
    fn from(e: ObjectError) -> Self {
        match e {
            ObjectError::Http { .. } | ObjectError::Integrity(_) | ObjectError::Protocol(_) => {
                Self::Problem(e.to_string())
            }
            e => Self::Stop(e),
        }
    }
}

/// What the last run uploaded, per session-relative path (the fast pre-check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Uploaded {
    size: u64,
    mtime: (i64, u32),
    blake3: String,
}

/// A session file to archive.
#[derive(Debug, Clone)]
struct LocalFile {
    rel: String,
    path: PathBuf,
    size: u64,
    mtime: (i64, u32),
}

fn mtime_of(m: &std::fs::Metadata) -> (i64, u32) {
    m.modified()
        .ok()
        .map_or((0, 0), |t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => (-(e.duration().as_secs() as i64), 0),
        })
}

/// The session's files that are archived (excludes applied, `.lt-archive-*` and
/// symbolic links left out), sorted. Unreadable folders and names that aren't UTF-8
/// are problems; links are warnings.
fn walk(
    root: &Path,
    rel: &str,
    ex: &Excludes,
    out: &mut Vec<LocalFile>,
    problems: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let dir = root.join(rel);
    let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(e) => {
            problems.push(format!("{}: {e}", if rel.is_empty() { "." } else { rel }));
            return;
        }
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            problems.push(format!(
                "{}: the name isn't valid UTF-8",
                Path::new(rel).join(&name).display()
            ));
            continue;
        };
        let child = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(ty) = e.file_type() else { continue };
        if ty.is_symlink() {
            warnings.push(format!(
                "{child}: symbolic link not archived to object storage"
            ));
        } else if ty.is_dir() {
            if !ex.excludes(&child, true) {
                walk(root, &child, ex, out, problems, warnings);
            }
        } else if ty.is_file() && !ex.excludes(&child, false) {
            match e.metadata() {
                Ok(m) => out.push(LocalFile {
                    rel: child,
                    path: e.path(),
                    size: m.len(),
                    mtime: mtime_of(&m),
                }),
                Err(err) => problems.push(format!("{child}: {err}")),
            }
        }
    }
}

/// Bytes sent of the bytes to send, as rsync-like progress.
struct Tracker {
    total: u64,
    done: u64,
    files: u64,
    count: u64,
    started: Instant,
}

impl Tracker {
    fn progress(&self) -> RsyncProgress {
        let secs = self.started.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            self.done as f64 / secs
        } else {
            0.0
        };
        let left = self.total.saturating_sub(self.done);
        RsyncProgress {
            bytes: self.done,
            percent: (self.done.min(self.total) * 100)
                .checked_div(self.total)
                .map_or(100, |p| p as u8),
            bytes_per_sec: rate,
            eta: (rate > 0.0).then(|| std::time::Duration::from_secs_f64(left as f64 / rate)),
            files_transferred: Some(self.files),
            to_check: Some((self.count.saturating_sub(self.files), self.count)),
        }
    }
}

/// Part size for a file of `size` bytes: [`PART_SIZE`], or more (whole MiB) to stay
/// within S3's 10 000 parts.
fn part_size_for(size: u64) -> usize {
    let min = size.div_ceil(MAX_PARTS);
    let mib = 1024 * 1024;
    (PART_SIZE as u64).max(min.div_ceil(mib) * mib) as usize
}

/// Read up to `buf.len()` bytes (fewer only at the end of the file).
fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// An `ETag` that is a plain MD5 (32 hex digits): comparable with ours.
fn md5_etag(etag: &str) -> bool {
    etag.len() == 32 && etag.chars().all(|c| c.is_ascii_hexdigit())
}

/// The archive in a bucket.
#[derive(Debug, Clone)]
pub struct ObjectDestination {
    cfg: ObjectStoreConfig,
    prefix: String,
    client: Client,
}

impl ObjectDestination {
    /// Settings that make no sense, or missing keys, are [`ArchiveError::TargetNotUsable`].
    pub fn new(
        cfg: ObjectStoreConfig,
        transport: Arc<dyn Transport>,
        creds: Credentials,
    ) -> Result<Self, ArchiveError> {
        let location = PathBuf::from(format!("s3://{}", cfg.bucket.trim()));
        let not_usable = |reason: String| ArchiveError::TargetNotUsable {
            root: location.clone(),
            reason,
        };
        let endpoint = cfg.endpoint().map_err(not_usable)?;
        let prefix = cfg.normalized_prefix().map_err(not_usable)?;
        if creds.access_key_id.trim().is_empty() || creds.secret_access_key.is_empty() {
            return Err(not_usable("the access key isn't set".into()));
        }
        Ok(Self {
            cfg,
            prefix,
            client: Client::new(transport, creds, endpoint),
        })
    }

    /// The same destination with another retry policy (tests).
    #[must_use]
    pub fn with_client(mut self, f: impl FnOnce(Client) -> Client) -> Self {
        self.client = f(self.client);
        self
    }

    pub fn config(&self) -> &ObjectStoreConfig {
        &self.cfg
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// An object-storage error as an archive error: no answer is "offline"
    /// (retried), refused keys, a missing bucket, a skewed clock or wrong settings need
    /// the user.
    fn err(&self, e: ObjectError) -> ArchiveError {
        let root = PathBuf::from(self.location());
        match e {
            ObjectError::Cancelled => ArchiveError::Cancelled,
            ObjectError::Network(_) => ArchiveError::TargetUnavailable {
                root,
                reason: e.to_string(),
            },
            ObjectError::Auth(_)
            | ObjectError::ClockSkew(_)
            | ObjectError::NoSuchBucket(_)
            | ObjectError::Settings(_) => ArchiveError::TargetNotUsable {
                root,
                reason: e.to_string(),
            },
            e => ArchiveError::Storage(e.to_string()),
        }
    }

    fn folder_prefix(&self, folder: &str) -> String {
        format!("{}{folder}/", self.prefix)
    }

    /// The owner in `folder`'s marker named `file`, if any.
    fn marker(
        &self,
        folder: &str,
        file: &str,
        cancel: &AtomicBool,
    ) -> Result<Option<String>, ObjectError> {
        let key = format!("{}{file}", self.folder_prefix(folder));
        Ok(self
            .client
            .get_small(&key, cancel)?
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned()))
    }

    /// The owner recorded in `folder`'s marker, if any (the old marker's when
    /// there is no new one: a folder Darkslide 0.1 claimed is still ours).
    fn owner_of(&self, folder: &str, cancel: &AtomicBool) -> Result<Option<String>, ObjectError> {
        match self.marker(folder, ARCHIVE_OWNER_FILE, cancel)? {
            Some(o) => Ok(Some(o)),
            None => self.marker(folder, LEGACY_ARCHIVE_OWNER_FILE, cancel),
        }
    }

    /// Write `owner`'s marker in `folder`, which had none; whether `owner` holds it.
    /// The marker is created with `If-None-Match: *`, so of two sessions claiming the
    /// folder at once only one gets it. Where the service has no conditional writes
    /// (501) it is written plainly; either way it is read back, which also catches a
    /// service that ignored the condition and a later writer that replaced it.
    fn write_owner(
        &self,
        folder: &str,
        owner: &str,
        cancel: &AtomicBool,
    ) -> Result<bool, ObjectError> {
        let key = format!("{}{ARCHIVE_OWNER_FILE}", self.folder_prefix(folder));
        if self.client.put_if_absent(&key, owner.as_bytes(), cancel)? == PutIfAbsent::Unsupported {
            self.client
                .put_object(&key, owner.as_bytes(), &[], cancel)?;
        }
        Ok(self.owner_of(folder, cancel)?.as_deref() == Some(owner))
    }

    /// `folder` holds no object at all.
    fn is_empty(&self, folder: &str, cancel: &AtomicBool) -> Result<bool, ObjectError> {
        Ok(self
            .client
            .list(&self.folder_prefix(folder), cancel)?
            .is_empty())
    }

    /// `HeadBucket`, a small `PutObject` under `<prefix>.lt-archive-test/`, and a
    /// `HeadObject` of it (size and BLAKE3). The test object stays.
    pub fn test_connection(
        &self,
        now: SystemTime,
        cancel: &AtomicBool,
    ) -> Result<ConnectionReport, ArchiveError> {
        self.client.head_bucket(cancel).map_err(|e| self.err(e))?;
        let key = format!("{}{TEST_DIR}/{TEST_NAME}", self.prefix);
        let when = chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339();
        let body = format!(
            "Luxtable archive connection test ({when}). Safe to delete; the next test writes it again.\n"
        );
        let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
        let etag = self
            .client
            .put_object(
                &key,
                body.as_bytes(),
                &[(META_BLAKE3, hash.clone())],
                cancel,
            )
            .map_err(|e| self.err(e))?;
        let (_, md5_hex) = client::md5(body.as_bytes());
        if md5_etag(&etag) && !etag.eq_ignore_ascii_case(&md5_hex) {
            return Err(self.err(ObjectError::Integrity(format!(
                "the test object's ETag is {etag}, expected {md5_hex}"
            ))));
        }
        self.verify(&key, body.len() as u64, &hash, cancel)
            .map_err(|e| self.err(e))?;
        Ok(ConnectionReport {
            test_key: key,
            location: self.location(),
        })
    }

    /// `HeadObject` shows `size` and `hash`.
    fn verify(
        &self,
        key: &str,
        size: u64,
        hash: &str,
        cancel: &AtomicBool,
    ) -> Result<(), ObjectError> {
        match self.client.head_object(key, cancel)? {
            Some(h) if h.size == size && h.blake3.as_deref() == Some(hash) => Ok(()),
            Some(h) => Err(ObjectError::Integrity(format!(
                "{key}: stored {} bytes with hash {}, sent {size} bytes with hash {hash}",
                h.size,
                h.blake3.as_deref().unwrap_or("none")
            ))),
            None => Err(ObjectError::Integrity(format!(
                "{key}: not found after the upload"
            ))),
        }
    }

    /// Copy the current object at `key` into the trash before it is replaced, and
    /// check the copy. A copy that can't be made means the object is not replaced.
    fn keep_previous(
        &self,
        key: &str,
        head: &ObjectHead,
        stamp: &str,
        cancel: &AtomicBool,
    ) -> Result<(), FileError> {
        if head.size > MAX_COPY {
            return Err(FileError::Problem(
                "its archived copy is over 5 GB and can't be kept in the archive trash, so it was not replaced".into(),
            ));
        }
        let rest = key.strip_prefix(&self.prefix).unwrap_or(key);
        let trash = format!("{}{TRASH_DIR}/{stamp}/{rest}", self.prefix);
        self.client.copy_object(key, &trash, cancel)?;
        match self.client.head_object(&trash, cancel)? {
            Some(t) if t.size == head.size => Ok(()),
            _ => Err(FileError::Problem(
                "the previous version couldn't be kept in the archive trash, so it was not replaced".into(),
            )),
        }
    }

    /// Archive one file at `key`: skipped when the object has the same BLAKE3,
    /// otherwise its previous version is kept (unless `replace_in_place`) and the
    /// file uploaded and verified. Returns the file's hash.
    #[allow(clippy::too_many_arguments)]
    fn put_file(
        &self,
        f: &LocalFile,
        key: &str,
        listed: bool,
        replace_in_place: bool,
        stamp: &str,
        sent: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<String, FileError> {
        let local = |e: std::io::Error| FileError::Problem(e.to_string());
        let meta = |hash: &str| {
            vec![
                (META_BLAKE3, hash.to_owned()),
                (META_MTIME, f.mtime.0.to_string()),
            ]
        };
        // Small files are read once: what is hashed is exactly what is sent.
        let small = if f.size <= PART_SIZE as u64 {
            Some(std::fs::read(&f.path).map_err(local)?)
        } else {
            None
        };
        let hash = match &small {
            Some(body) => blake3::hash(body).to_hex().to_string(),
            None => hash_file(&f.path, cancel)?,
        };
        let size = small.as_ref().map_or(f.size, |b| b.len() as u64);
        if listed && let Some(head) = self.client.head_object(key, cancel)? {
            if head.size == size && head.blake3.as_deref() == Some(hash.as_str()) {
                sent(size);
                return Ok(hash);
            }
            if !replace_in_place {
                self.keep_previous(key, &head, stamp, cancel)?;
            }
        }
        if let Some(body) = small {
            let etag = self.client.put_object(key, &body, &meta(&hash), cancel)?;
            sent(size);
            let (_, md5_hex) = client::md5(&body);
            if md5_etag(&etag) && !etag.eq_ignore_ascii_case(&md5_hex) {
                return Err(
                    ObjectError::Integrity(format!("ETag {etag}, expected {md5_hex}")).into(),
                );
            }
        } else {
            self.multipart(f, key, &hash, &meta(&hash), sent, cancel)?;
        }
        self.verify(key, size, &hash, cancel)?;
        Ok(hash)
    }

    /// Upload a large file in parts, read one at a time; aborted on any failure.
    fn multipart(
        &self,
        f: &LocalFile,
        key: &str,
        hash: &str,
        meta: &[(&str, String)],
        sent: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), FileError> {
        let mut file =
            std::fs::File::open(&f.path).map_err(|e| FileError::Problem(e.to_string()))?;
        let id = self.client.create_multipart(key, meta, cancel)?;
        let r = (|| -> Result<(), FileError> {
            let part_size = part_size_for(f.size);
            let mut buf = vec![0u8; part_size];
            let mut etags = Vec::new();
            let mut md5s = Vec::new();
            let mut hasher = blake3::Hasher::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(FileError::Stop(ObjectError::Cancelled));
                }
                let n = read_full(&mut file, &mut buf)
                    .map_err(|e| FileError::Problem(e.to_string()))?;
                if n == 0 {
                    break;
                }
                let part = &buf[..n];
                hasher.update(part);
                let (_, md5_hex) = client::md5(part);
                let number = etags.len() as u32 + 1;
                let etag = self.client.upload_part(key, &id, number, part, cancel)?;
                if md5_etag(&etag) && !etag.eq_ignore_ascii_case(&md5_hex) {
                    return Err(ObjectError::Integrity(format!(
                        "part {number}: ETag {etag}, expected {md5_hex}"
                    ))
                    .into());
                }
                md5s.extend_from_slice(&hex_bytes(&md5_hex));
                etags.push(etag);
                sent(n as u64);
                if n < part_size {
                    break;
                }
            }
            if hasher.finalize().to_hex().as_str() != hash {
                return Err(FileError::Problem(
                    "it changed while it was being archived; the next run archives it".into(),
                ));
            }
            let etag = self.client.complete_multipart(key, &id, &etags, cancel)?;
            let (_, combined) = client::md5(&md5s);
            let expected = format!("{combined}-{}", etags.len());
            if etag.contains('-') && !etag.eq_ignore_ascii_case(&expected) {
                return Err(
                    ObjectError::Integrity(format!("ETag {etag}, expected {expected}")).into(),
                );
            }
            Ok(())
        })();
        if r.is_err()
            && let Err(e) = self.client.abort_multipart(key, &id)
        {
            tracing::warn!(%e, key, "object storage: multipart upload not aborted");
        }
        r
    }

    /// The fast pre-check's file for this destination and session folder.
    fn cache_path(&self, session: &Path, folder: &str) -> PathBuf {
        let mut h = blake3::Hasher::new();
        h.update(self.lock_key().as_os_str().as_encoded_bytes());
        h.update(b"\0");
        h.update(folder.as_bytes());
        let key = h.finalize().to_hex();
        session
            .join(".lt/cache")
            .join(format!("archive-object-{}.json", &key[..16]))
    }
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// BLAKE3 of a file, streamed.
fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, FileError> {
    let mut f = std::fs::File::open(path).map_err(|e| FileError::Problem(e.to_string()))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(FileError::Stop(ObjectError::Cancelled));
        }
        let n = read_full(&mut f, &mut buf).map_err(|e| FileError::Problem(e.to_string()))?;
        h.update(&buf[..n]);
        if n < buf.len() {
            return Ok(h.finalize().to_hex().to_string());
        }
    }
}

impl Destination for ObjectDestination {
    fn kind(&self) -> DestinationKind {
        DestinationKind::ObjectStorage
    }

    /// `s3://bucket/prefix`, with ` at <host>` for a service other than AWS.
    fn location(&self) -> String {
        self.cfg.location()
    }

    fn lock_key(&self) -> PathBuf {
        PathBuf::from(self.cfg.lock_key())
    }

    /// One `HeadBucket`, not retried: the caller checks again on its own schedule.
    fn check(&self, cancel: &AtomicBool) -> Result<Option<String>, ArchiveError> {
        let once = self.client.clone().with_retry(RetryPolicy {
            attempts: 1,
            ..RetryPolicy::default()
        });
        once.head_bucket(cancel).map_err(|e| self.err(e))?;
        Ok(Some("object storage".into()))
    }

    /// The recorded folder unless another session owns it; else `<name>` when it is
    /// ours or empty; else `<name> (<id>)`. Only the owner marker is written.
    fn claim(
        &self,
        session_dir: &Path,
        owner: &str,
        recorded: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ClaimedDest, ArchiveError> {
        let invalid = || ArchiveError::InvalidSession(session_dir.to_path_buf());
        let base = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(invalid)?
            .to_owned();
        if !plain_component(owner) || !plain_component(&base) {
            return Err(invalid());
        }
        let e = |e| self.err(e);
        // `None`: another session's marker won the race for `folder`.
        let claim =
            |folder: &str, current: Option<String>| -> Result<Option<ClaimedDest>, ArchiveError> {
                // Claimed under the old marker only: the new one is written beside it.
                let marked = current.as_deref() == Some(owner)
                    && self
                        .marker(folder, ARCHIVE_OWNER_FILE, cancel)
                        .map_err(e)?
                        .is_some();
                if !marked && !self.write_owner(folder, owner, cancel).map_err(e)? {
                    return Ok(None);
                }
                Ok(Some(ClaimedDest {
                    dest: PathBuf::from(format!("{}{folder}", self.location_root())),
                    folder: folder.to_owned(),
                    adopted_legacy: false,
                }))
            };
        if let Some(name) = recorded.filter(|n| plain_component(n)) {
            let current = self.owner_of(name, cancel).map_err(e)?;
            if current.as_deref().is_none_or(|o| o == owner)
                && let Some(c) = claim(name, current)?
            {
                return Ok(c);
            }
        }
        let current = self.owner_of(&base, cancel).map_err(e)?;
        let free = match current.as_deref() {
            Some(o) => o == owner,
            None => self.is_empty(&base, cancel).map_err(e)?,
        };
        if free && let Some(c) = claim(&base, current)? {
            return Ok(c);
        }
        let short = &owner[..owner.len().min(8)];
        for folder in [format!("{base} ({short})"), format!("{base} ({owner})")] {
            let current = self.owner_of(&folder, cancel).map_err(e)?;
            if current.as_deref().is_none_or(|o| o == owner)
                && let Some(c) = claim(&folder, current)?
            {
                return Ok(c);
            }
        }
        Err(ArchiveError::TargetNotUsable {
            root: PathBuf::from(self.location()),
            reason: format!("every archive folder name for {base} is claimed by another session"),
        })
    }

    #[allow(clippy::too_many_lines)] // one pass: check, snapshot, walk, plan, upload
    fn archive(
        &self,
        source: &SessionSource,
        claimed: &ClaimedDest,
        now: SystemTime,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<ArchiveOutcome, ArchiveError> {
        let dir = &source.dir;
        if !dir.is_absolute() || !dir.is_dir() {
            return Err(ArchiveError::InvalidSession(dir.clone()));
        }
        // Nothing is written unless the bucket answers.
        self.client.head_bucket(cancel).map_err(|e| self.err(e))?;
        let mut problems = Vec::new();
        let mut warnings = Vec::new();
        let mut excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| (*s).to_owned()).collect();
        excludes.extend(
            [
                ".lt/cache/",
                "*.partial",
                &format!("{PARTIAL_DIR}/"),
                // Migration source: an old `.ds` left next to `.lt`, an old partial folder.
                ".ds/cache/",
                &format!("{LEGACY_PARTIAL_DIR}/"),
            ]
            .iter()
            .map(|s| (*s).to_owned()),
        );
        excludes.extend(source.excludes.iter().cloned());
        let snapshot = take_snapshot(
            source.snapshotter.as_deref(),
            dir,
            &mut excludes,
            &mut problems,
        );
        let mut files = Vec::new();
        walk(
            dir,
            "",
            &Excludes::new(&excludes),
            &mut files,
            &mut problems,
            &mut warnings,
        );

        let folder_prefix = self.folder_prefix(&claimed.folder);
        let listed: HashMap<String, u64> = self
            .client
            .list(&folder_prefix, cancel)
            .map_err(|e| self.err(e))?
            .into_iter()
            .collect();
        let cache_path = self.cache_path(dir, &claimed.folder);
        let mut cache: BTreeMap<String, Uploaded> = std::fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let todo: Vec<&LocalFile> = files
            .iter()
            .filter(|f| {
                let unchanged = cache
                    .get(&f.rel)
                    .is_some_and(|u| u.size == f.size && u.mtime == f.mtime)
                    && listed.get(&format!("{folder_prefix}{}", f.rel)) == Some(&f.size);
                !unchanged
            })
            .collect();
        let mut t = Tracker {
            total: todo.iter().map(|f| f.size).sum(),
            done: 0,
            files: 0,
            count: todo.len() as u64,
            started: Instant::now(),
        };
        progress(&t.progress());
        let stamp = trash_stamp(now);
        let save = |cache: &BTreeMap<String, Uploaded>| {
            if let Ok(b) = serde_json::to_vec(cache) {
                let _ = lt_fsutil::write_atomic(&cache_path, &b);
            }
        };
        for f in todo {
            if cancel.load(Ordering::Relaxed) {
                save(&cache);
                return Err(ArchiveError::Cancelled);
            }
            let key = format!("{folder_prefix}{}", f.rel);
            let before = t.done;
            let r = self.put_file(
                f,
                &key,
                listed.contains_key(&key),
                f.rel == SNAPSHOT_REL,
                &stamp,
                &mut |n| {
                    t.done += n;
                    progress(&t.progress());
                },
                cancel,
            );
            match r {
                Ok(blake3) => {
                    cache.insert(
                        f.rel.clone(),
                        Uploaded {
                            size: f.size,
                            mtime: f.mtime,
                            blake3,
                        },
                    );
                }
                Err(FileError::Problem(why)) => {
                    cache.remove(&f.rel);
                    problems.push(format!("{}: {why}", f.rel));
                }
                Err(FileError::Stop(e)) => {
                    save(&cache);
                    return Err(self.err(e));
                }
            }
            t.done = t.done.max(before + f.size).min(t.total);
            t.files += 1;
            progress(&t.progress());
        }
        save(&cache);
        Ok(ArchiveOutcome {
            dest: claimed.dest.clone(),
            backup_dir: format!("{TRASH_DIR}/{stamp}"),
            snapshot,
            warnings,
            problems,
        })
    }
}

impl ObjectDestination {
    /// `s3://bucket/prefix` (a claimed folder's `dest` is this plus its name).
    fn location_root(&self) -> String {
        format!("s3://{}/{}", self.client.endpoint().bucket, self.prefix)
    }
}

#[cfg(test)]
mod tests;
