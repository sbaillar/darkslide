//! Immich (`ds-immich`): show Darkslide's work in a self-hosted Immich server
//! that indexes the archive (the NAS folder `ds-archive` rsyncs sessions to) as
//! a read-only external library. Nothing is uploaded; Immich reads files.
//!
//! - **Developed copies**: the photos the user chooses get a full-size JPEG with
//!   every edit in `Developed/` inside the archived folder
//!   ([`ds_immich::publish`]). The app renders them through its export (which
//!   has the AI masks); this module says which are missing or out of date
//!   ([`SessionHandle::immich_develop_plan`]) and puts each render in place
//!   ([`SessionHandle::immich_developed_rendered`]).
//! - **XMP sidecars** (`IMG_0001.ARW.xmp`) with rating, title, caption, tags
//!   and position, so undeveloped RAWs show them too; refreshed before every
//!   archive run ([`before_archive`]); never over a sidecar another app wrote.
//! - **Albums**: rules ("★★★★★" → "Darkslide ★★★★★") kept in sync after archive
//!   runs ([`immich_sync_albums`]); only album membership and library scans are
//!   ever written to Immich.
//!
//! Requests go through the app's [`ImmichHttp`] (`URLSession`); the API key
//! lives in the Keychain and is passed per call ([`ImmichServer`]).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ds_core::{ColorTag, PickState, Rating};
use ds_immich::publish::{self, PublishError, PublishState, SidecarOutcome};
use ds_immich::{Client, ImmichError, PathMapping, Source, SyncOptions, SyncState, Wanted};
use ds_session::{Filter, ImageInfo, Session, Sort, VariantId, VariantInfo};

use crate::archive::ArchiveManager;
use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::session::{ColorLabel, PickFlag, SessionHandle};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One read-modify-write of a session's publish state at a time (renders
/// finish in parallel with archive runs).
static STATE: Mutex<()> = Mutex::new(());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

// --------------------------------------------------------------- the seam ---

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<ImmichHeader>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichHttpResponse {
    pub status: u16,
    pub headers: Vec<ImmichHeader>,
    pub body: Vec<u8>,
}

/// Implemented in Swift with `URLSession`. Called on a worker thread, one
/// request at a time; blocks until the answer.
#[uniffi::export(with_foreign)]
pub trait ImmichHttp: Send + Sync {
    /// Any HTTP answer is `Ok` (whatever its status); `Err` means no answer
    /// (offline, timeout).
    fn send(&self, request: ImmichHttpRequest) -> Result<ImmichHttpResponse, DsError>;
}

struct Bridge(Arc<dyn ImmichHttp>);

impl ds_immich::Transport for Bridge {
    fn send(&self, r: ds_immich::Request) -> Result<ds_immich::Response, String> {
        let answer = self
            .0
            .send(ImmichHttpRequest {
                method: r.method,
                url: r.url,
                headers: r
                    .headers
                    .into_iter()
                    .map(|(name, value)| ImmichHeader { name, value })
                    .collect(),
                body: r.body,
            })
            .map_err(|e| e.to_string())?;
        Ok(ds_immich::Response {
            status: answer.status,
            body: answer.body,
        })
    }
}

/// The server and the API key (from the Keychain).
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichServer {
    /// e.g. `https://immich.example.org` (`/api` is added).
    pub url: String,
    pub api_key: String,
}

impl std::fmt::Debug for ImmichServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImmichServer")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// `ds-immich` errors as the app sees them: a refused key is `InvalidArgument`
/// ("Immich refused the API key: …"), no answer is `Io` ("Immich server not
/// reachable: …").
fn ds_error(e: ImmichError) -> DsError {
    match e {
        ImmichError::Cancelled => DsError::Cancelled,
        ImmichError::Auth(_) | ImmichError::InvalidServer(_) => DsError::InvalidArgument {
            message: e.to_string(),
        },
        e => DsError::Io {
            message: e.to_string(),
        },
    }
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(publish_error)`
fn publish_error(e: PublishError) -> DsError {
    match e {
        PublishError::NotOurs(_) | PublishError::Taken(_) => DsError::Unsupported {
            message: e.to_string(),
        },
        PublishError::Io { .. } => DsError::Io {
            message: e.to_string(),
        },
    }
}

// ------------------------------------------------------------- connection ---

/// One library (external libraries index their import paths).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichLibrary {
    pub id: String,
    pub name: String,
    pub import_paths: Vec<String>,
    pub exclusion_patterns: Vec<String>,
}

/// What the connection test found.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichConnection {
    pub user_name: String,
    pub user_email: String,
    pub is_admin: bool,
    /// The libraries (admin keys only).
    pub libraries: Vec<ImmichLibrary>,
    /// Why they couldn't be listed (a non-admin key), if so.
    pub libraries_note: Option<String>,
}

/// Check the server answers and accepts the key; list its libraries.
#[uniffi::export]
pub async fn immich_test_connection(
    http: Arc<dyn ImmichHttp>,
    server: ImmichServer,
    token: Arc<CancelToken>,
) -> Result<ImmichConnection, DsError> {
    run_job_on_thread("ds-immich", token, move |cancelled| {
        let bridge = Bridge(http);
        let c = Client::new(&bridge, &server.url, &server.api_key)
            .map_err(ds_error)?
            .with_cancel(cancelled);
        c.ping().map_err(ds_error)?;
        let me = c.me().map_err(ds_error)?;
        let (libraries, libraries_note) = match c.libraries() {
            Ok(l) => (l, None),
            Err(ImmichError::Forbidden(_)) => (
                Vec::new(),
                Some(
                    "Only an admin's API key can see the libraries: set the path mapping by hand."
                        .to_owned(),
                ),
            ),
            Err(e) => return Err(ds_error(e)),
        };
        Ok(ImmichConnection {
            user_name: me.name,
            user_email: me.email,
            is_admin: me.is_admin,
            libraries: libraries
                .into_iter()
                .map(|l| ImmichLibrary {
                    id: l.id,
                    name: l.name,
                    import_paths: l.import_paths,
                    exclusion_patterns: l.exclusion_patterns,
                })
                .collect(),
            libraries_note,
        })
    })
    .await
}

/// Where Immich sees the archive: `immich_root` (on the server) is `local_root`
/// (on this Mac), e.g. `/mnt/photos` ↔ `/Volumes/photos`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichPathMapping {
    pub immich_root: String,
    pub local_root: String,
}

impl ImmichPathMapping {
    fn mapping(&self) -> Result<PathMapping, DsError> {
        let (i, l) = (self.immich_root.trim(), self.local_root.trim());
        if !i.starts_with('/') || !Path::new(l).is_absolute() {
            return Err(DsError::invalid(
                "the path mapping needs two absolute folders (as Immich sees it, and on this Mac)",
            ));
        }
        Ok(PathMapping {
            immich_root: i.trim_end_matches('/').to_owned(),
            local_root: PathBuf::from(l),
        })
    }
}

/// A mapping found by [`immich_detect_mapping`], and how.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichDetectedMapping {
    pub mapping: ImmichPathMapping,
    /// "asset": an archived photo was found in Immich; "library": a library's
    /// import path is named like a folder of the archive's path.
    pub how: String,
}

/// Work out how Immich sees `archive_root` (the archive target): from an
/// archived file Immich already indexed (`known_file`, a path under the target),
/// else from the libraries' import paths. `None` when neither tells.
#[uniffi::export]
pub async fn immich_detect_mapping(
    http: Arc<dyn ImmichHttp>,
    server: ImmichServer,
    archive_root: String,
    known_file: Option<String>,
    token: Arc<CancelToken>,
) -> Result<Option<ImmichDetectedMapping>, DsError> {
    run_job_on_thread("ds-immich", token, move |cancelled| {
        let bridge = Bridge(http);
        let c = Client::new(&bridge, &server.url, &server.api_key)
            .map_err(ds_error)?
            .with_cancel(cancelled);
        let found = ds_immich::detect(
            &c,
            known_file.as_deref().map(Path::new),
            Path::new(&archive_root),
        )
        .map_err(ds_error)?;
        Ok(found.map(|d| ImmichDetectedMapping {
            mapping: ImmichPathMapping {
                immich_root: d.mapping.immich_root,
                local_root: d.mapping.local_root.to_string_lossy().into_owned(),
            },
            how: match d.how {
                ds_immich::Detection::Asset => "asset",
                ds_immich::Detection::LibraryPath => "library",
            }
            .to_owned(),
        }))
    })
    .await
}

// ------------------------------------------------------- developed copies ---

/// How developed copies are made (part of what makes a copy current: changing
/// them renders the copies again).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ImmichDevelopOptions {
    /// JPEG quality 1–100.
    pub quality: u8,
    /// Long edge in pixels; `None` = full size.
    pub long_edge: Option<u32>,
    /// Colour label and pick as `Darkslide/…` tags in the copy.
    pub label_tags: bool,
}

/// A developed copy's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ImmichCopyState {
    /// Marked; not rendered yet (or the file is gone).
    Pending,
    /// Rendered from the photo as it is now.
    Current,
    /// The photo changed since (edit, rating, keywords…): rendered again soon.
    OutOfDate,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichDeveloped {
    pub variant_id: i64,
    pub state: ImmichCopyState,
    /// The copy, once rendered.
    pub path: Option<String>,
}

/// One copy to render: export `original_path` with the photo's edits as a JPEG,
/// then hand the file to [`SessionHandle::immich_developed_rendered`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichDevelopJob {
    pub variant_id: i64,
    pub original_path: String,
    /// Where it goes, relative to the archived folder (`Developed/…`).
    pub rel: String,
    /// What it is rendered from (pass it back).
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichDevelopPlan {
    /// Render into this folder (inside the session's cache; never archived).
    pub staging_dir: String,
    pub jobs: Vec<ImmichDevelopJob>,
}

/// What marking or unmarking did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichMarkReport {
    pub changed: u32,
    /// Copies of unmarked photos, left where they are (and in the archive).
    pub kept_files: Vec<String>,
}

/// One photo as the publish step needs it.
struct Photo {
    variant: VariantInfo,
    image: ImageInfo,
    original: PathBuf,
    /// The original relative to the archived folder (`/`-separated).
    rel: String,
}

/// Every photo (variant) of `ids` (all when `None`) outside the Trash whose
/// original is in the archived folder `source`.
fn photos(s: &Session, source: &Path, ids: Option<&[i64]>) -> Result<Vec<Photo>, DsError> {
    let variants = match ids {
        Some(ids) => ids.iter().map(|i| VariantId(*i)).collect(),
        None => s.all_variants()?,
    };
    let mut images: HashMap<i64, ImageInfo> = HashMap::new();
    let mut out = Vec::new();
    for v in variants {
        let Ok(variant) = s.variant(v) else { continue };
        let image = if let Some(i) = images.get(&variant.image.0) {
            i.clone()
        } else {
            let i = s.image(variant.image)?;
            images.insert(variant.image.0, i.clone());
            i
        };
        if image.folder() == ds_session::Folder::Trash {
            continue;
        }
        let original = s.original_path(image.rel_path.as_str());
        let Some(rel) = original
            .strip_prefix(source)
            .ok()
            .and_then(|r| r.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        out.push(Photo {
            variant,
            image,
            original,
            rel,
        });
    }
    out.sort_by_key(|p| p.variant.id);
    Ok(out)
}

/// File names of the session's originals per folder (relative to `source`), for
/// [`publish::developed_rel`]'s collisions.
fn siblings(s: &Session, source: &Path) -> Result<HashMap<String, Vec<String>>, DsError> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for i in s.images()? {
        let p = s.original_path(i.rel_path.as_str());
        if let Ok(rel) = p.strip_prefix(source) {
            let dir = rel.parent().map(|d| d.to_string_lossy().into_owned());
            let name = rel.file_name().map(|n| n.to_string_lossy().into_owned());
            if let (Some(dir), Some(name)) = (dir, name) {
                out.entry(dir).or_default().push(name);
            }
        }
    }
    Ok(out)
}

/// Whether `staged` names a regular file inside `staging`: below it by plain
/// names only (no `..`, `.` or root), so a lexical prefix can't be escaped.
fn staged_render_ok(staged: &Path, staging: &Path) -> bool {
    staged.strip_prefix(staging).is_ok_and(|rest| {
        rest.components().next().is_some()
            && rest
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
    }) && std::fs::symlink_metadata(staged).is_ok_and(|m| m.is_file())
}

/// Whether `rel` is a copy's path as [`publish::developed_rel`] makes them:
/// relative, plain names only, a file under `Developed/`.
fn developed_rel_ok(rel: &str) -> bool {
    let mut parts = Path::new(rel).components();
    parts.next()
        == Some(std::path::Component::Normal(
            publish::DEVELOPED_DIR.as_ref(),
        ))
        && parts.next().is_some()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn developed_rel(p: &Photo, siblings: &HashMap<String, Vec<String>>) -> String {
    let dir = p.rel.rsplit_once('/').map_or("", |(d, _)| d);
    let none = Vec::new();
    publish::developed_rel(
        &p.rel,
        siblings.get(dir).unwrap_or(&none),
        p.variant.ordinal,
    )
}

fn tag_code(t: ColorTag) -> u8 {
    ColorLabel::from(t) as u8
}

fn pick_code(p: PickState) -> u8 {
    PickFlag::from(p) as u8
}

/// What a developed copy of `p` is rendered from.
fn fingerprint(
    s: &Session,
    p: &Photo,
    rel: &str,
    o: ImmichDevelopOptions,
) -> Result<String, DsError> {
    let adjustments = s.load_adjustments(p.variant.id)?;
    let v = &p.variant;
    let options = format!("{}:{:?}:{}", o.quality, o.long_edge, o.label_tags);
    let organize = [v.rating.stars(), tag_code(v.color_tag), pick_code(v.pick)];
    Ok(publish::fingerprint(&[
        b"darkslide-developed/1",
        rel.as_bytes(),
        &p.image.hash.0,
        &json(&adjustments),
        &organize,
        &json(&v.keywords),
        &json(&v.iptc),
        &json(&v.geotag),
        options.as_bytes(),
    ]))
}

/// Plain data as JSON, for a fingerprint.
fn json<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

impl SessionHandle {
    /// The archived folder, or why this session can't publish.
    fn publish_source(&self) -> Result<PathBuf, DsError> {
        self.archive_source()
            .map_err(|message| DsError::Unsupported { message })
    }

    fn store(&self) -> PathBuf {
        lock(&self.session).root().to_path_buf()
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Mark (`on`) or unmark photos for a developed copy. Marking needs an
    /// archivable session with room for `Developed/`; unmarking leaves the
    /// copies where they are (nothing deletes from the archive).
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn immich_mark_developed(
        &self,
        ids: Vec<i64>,
        on: bool,
    ) -> Result<ImmichMarkReport, DsError> {
        guard(|| {
            self.check_folder()?;
            let source = self.publish_source()?;
            if on {
                publish::prepare_developed_dir(&source).map_err(publish_error)?;
            }
            let store = self.store();
            let _g = lock(&STATE);
            let mut state = PublishState::load(&store).map_err(publish_error)?;
            let m = state.mark(&ids, on);
            if m.changed > 0 {
                state.save(&store).map_err(publish_error)?;
            }
            Ok(ImmichMarkReport {
                changed: m.changed as u32,
                kept_files: m
                    .kept
                    .iter()
                    .map(|r| source.join(r).to_string_lossy().into_owned())
                    .collect(),
            })
        })
    }

    /// The marked photos and their copies' states (for badges).
    pub fn immich_developed(
        &self,
        options: ImmichDevelopOptions,
    ) -> Result<Vec<ImmichDeveloped>, DsError> {
        guard(|| {
            let Ok(source) = self.archive_source() else {
                return Ok(Vec::new());
            };
            let state = PublishState::load(&self.store()).map_err(publish_error)?;
            if state.developed.is_empty() {
                return Ok(Vec::new());
            }
            let ids: Vec<i64> = state.developed.keys().copied().collect();
            let s = lock(&self.session);
            let sib = siblings(&s, &source)?;
            let mut out = Vec::new();
            for p in photos(&s, &source, Some(&ids))? {
                let rel = developed_rel(&p, &sib);
                let fp = fingerprint(&s, &p, &rel, options)?;
                let d = &state.developed[&p.variant.id.0];
                let path = d.file.as_ref().map(|f| source.join(f));
                let on_disk = path.as_ref().is_some_and(|p| p.is_file());
                let state = if !on_disk {
                    ImmichCopyState::Pending
                } else if publish::is_stale(d, &fp) || d.file.as_deref() != Some(rel.as_str()) {
                    ImmichCopyState::OutOfDate
                } else {
                    ImmichCopyState::Current
                };
                out.push(ImmichDeveloped {
                    variant_id: p.variant.id.0,
                    state,
                    path: path
                        .filter(|_| on_disk)
                        .map(|p| p.to_string_lossy().into_owned()),
                });
            }
            Ok(out)
        })
    }

    /// The copies to render now: marked photos never rendered, out of date, or
    /// whose file is gone. Prepares `Developed/` and the staging folder.
    pub fn immich_develop_plan(
        &self,
        options: ImmichDevelopOptions,
    ) -> Result<ImmichDevelopPlan, DsError> {
        guard(|| {
            let source = self.publish_source()?;
            let store = self.store();
            let staging = store.join(".ds/cache/immich-staging");
            let state = PublishState::load(&store).map_err(publish_error)?;
            let mut jobs = Vec::new();
            if !state.developed.is_empty() {
                publish::prepare_developed_dir(&source).map_err(publish_error)?;
                std::fs::create_dir_all(&staging)?;
                let ids: Vec<i64> = state.developed.keys().copied().collect();
                let s = lock(&self.session);
                let sib = siblings(&s, &source)?;
                for p in photos(&s, &source, Some(&ids))? {
                    if p.image.missing {
                        continue;
                    }
                    let rel = developed_rel(&p, &sib);
                    let fp = fingerprint(&s, &p, &rel, options)?;
                    let d = &state.developed[&p.variant.id.0];
                    let current = !publish::is_stale(d, &fp)
                        && d.file.as_deref() == Some(rel.as_str())
                        && source.join(&rel).is_file();
                    if !current {
                        jobs.push(ImmichDevelopJob {
                            variant_id: p.variant.id.0,
                            original_path: p.original.to_string_lossy().into_owned(),
                            rel,
                            fingerprint: fp,
                        });
                    }
                }
            }
            Ok(ImmichDevelopPlan {
                staging_dir: staging.to_string_lossy().into_owned(),
                jobs,
            })
        })
    }

    /// Put a rendered copy (`staged`, written by the export into the staging
    /// folder) in place for `job`, replacing the photo's previous copy. A photo
    /// unmarked meanwhile is skipped (the render is discarded); a file Darkslide
    /// didn't make at that path is never replaced. Returns the copy's path.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn immich_developed_rendered(
        &self,
        job: ImmichDevelopJob,
        staged: String,
    ) -> Result<Option<String>, DsError> {
        guard(|| {
            self.check_folder()?;
            let source = self.publish_source()?;
            let store = self.store();
            let staged = PathBuf::from(staged);
            let staging = store.join(".ds/cache/immich-staging");
            // Both paths come from the app: check them before anything is
            // removed or renamed.
            if !staged_render_ok(&staged, &staging) {
                return Err(DsError::invalid("the render isn't in the staging folder"));
            }
            if !developed_rel_ok(&job.rel) {
                return Err(DsError::invalid(
                    "the copy's path isn't in the Developed folder",
                ));
            }
            let _g = lock(&STATE);
            let mut state = PublishState::load(&store).map_err(publish_error)?;
            if !state.is_marked(job.variant_id) {
                let _ = std::fs::remove_file(&staged);
                return Ok(None);
            }
            let owned = state.owns_copy(&job.rel);
            let dest = publish::place_developed(&source, &job.rel, &staged, owned)
                .map_err(publish_error)?;
            state.rendered(job.variant_id, &job.rel, &job.fingerprint, now_ms());
            state.save(&store).map_err(publish_error)?;
            Ok(Some(dest.to_string_lossy().into_owned()))
        })
    }

    /// Write or refresh the XMP sidecars next to the originals (see
    /// [`refresh_sidecars`]).
    pub fn immich_refresh_sidecars(
        &self,
        label_tags: bool,
    ) -> Result<ImmichSidecarReport, DsError> {
        guard(|| {
            self.check_folder()?;
            refresh_sidecars(self, label_tags)
        })
    }
}

// ------------------------------------------------------------ XMP sidecars ---

/// What a sidecar refresh did.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct ImmichSidecarReport {
    pub written: u32,
    pub unchanged: u32,
    /// Sidecars another app wrote (or that changed since Darkslide wrote them):
    /// left alone.
    pub foreign: Vec<String>,
    /// Couldn't be written (why).
    pub failed: Vec<String>,
}

/// The sidecar of an image: its first variant's rating, label/pick tags,
/// keywords, IPTC fields and position.
fn sidecar_bytes(v: &VariantInfo, label_tags: bool) -> Vec<u8> {
    let org = ds_export::xmp::Organize {
        rating: Some(v.rating.stars()),
        tags: if label_tags {
            ds_export::xmp::label_pick_tags(v.color_tag, v.pick)
        } else {
            Vec::new()
        },
    };
    ds_export::xmp::sidecar(&v.keywords, &v.iptc, &org, v.geotag)
}

/// Write `IMG_0001.ARW.xmp` next to every original of the session (outside the
/// Trash) that has none, or whose sidecar is Darkslide's and unchanged since it
/// was written; another app's sidecar is never touched.
pub(crate) fn refresh_sidecars(
    h: &SessionHandle,
    label_tags: bool,
) -> Result<ImmichSidecarReport, DsError> {
    let source = h.publish_source()?;
    let store = h.store();
    let wanted: Vec<(String, PathBuf, Vec<u8>)> = {
        let s = lock(&h.session);
        let mut seen = BTreeSet::new();
        photos(&s, &source, None)?
            .into_iter()
            .filter(|p| !p.image.missing && seen.insert(p.image.id))
            .map(|p| {
                let path = publish::sidecar_path(&p.original);
                (
                    format!("{}.xmp", p.rel),
                    path,
                    sidecar_bytes(&p.variant, label_tags),
                )
            })
            .collect()
    };
    let _g = lock(&STATE);
    let mut state = PublishState::load(&store).map_err(publish_error)?;
    let before = state.sidecars.clone();
    let mut report = ImmichSidecarReport::default();
    for (rel, path, bytes) in wanted {
        let recorded = state.sidecars.get(&rel).map(|r| r.hash.clone());
        match publish::write_sidecar(
            &path,
            &bytes,
            recorded.as_deref(),
            ds_export::xmp::is_darkslide_sidecar,
        ) {
            Ok(SidecarOutcome::Written(hash)) => {
                report.written += 1;
                state.sidecars.insert(rel, publish::SidecarRecord { hash });
            }
            Ok(SidecarOutcome::Unchanged(hash)) => {
                report.unchanged += 1;
                state.sidecars.insert(rel, publish::SidecarRecord { hash });
            }
            Ok(SidecarOutcome::Foreign) => {
                state.sidecars.remove(&rel);
                report.foreign.push(rel);
            }
            Err(e) => report.failed.push(format!("{rel}: {e}")),
        }
    }
    if state.sidecars != before {
        state.save(&store).map_err(publish_error)?;
    }
    Ok(report)
}

/// What the archive does before each run while Immich publishing is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchivePublish {
    pub xmp_sidecars: bool,
    pub label_tags: bool,
}

/// Before an archive run: refresh the sidecars (so they match the photos in
/// this run). Problems come back as the run's warnings; they never stop it.
pub(crate) fn before_archive(h: &SessionHandle, p: &ArchivePublish) -> Vec<String> {
    if !p.xmp_sidecars {
        return Vec::new();
    }
    match refresh_sidecars(h, p.label_tags) {
        Ok(r) => r
            .failed
            .into_iter()
            .map(|f| format!("XMP sidecar not written: {f}"))
            .collect(),
        Err(e) => vec![format!("XMP sidecars not refreshed: {e}")],
    }
}

/// Immich publishing settings the archive applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ImmichPublishConfig {
    /// Write XMP sidecars next to the originals before each archive run.
    pub xmp_sidecars: bool,
    /// Colour label and pick as `Darkslide/…` tags in them.
    pub label_tags: bool,
}

#[uniffi::export]
impl ArchiveManager {
    /// Turn Immich publishing on (`Some`: sidecars before each run, sessions
    /// queued for the album sync after each run) or off.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn set_immich(&self, config: Option<ImmichPublishConfig>) -> Result<(), DsError> {
        guard(|| {
            self.set_immich_publish(config.map(|c| ArchivePublish {
                xmp_sidecars: c.xmp_sidecars,
                label_tags: c.label_tags,
            }));
            Ok(())
        })
    }

    /// Sessions archived since the last album sync.
    pub fn immich_pending(&self) -> Result<u32, DsError> {
        guard(|| Ok(self.immich_queued() as u32))
    }
}

// ------------------------------------------------------------------ albums ---

/// An album kept in sync with a rule. Rules apply to every archived session;
/// the album collects the matching photos of all of them.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichRule {
    /// The Immich album, e.g. "Darkslide ★★★★★".
    pub album: String,
    /// At least this many stars (0: any).
    pub min_rating: u8,
    /// Only this pick flag.
    pub pick: Option<PickFlag>,
    /// Any of these colour labels (empty: any).
    pub labels: Vec<ColorLabel>,
    /// All of these keywords (whole keyword, any case).
    pub keywords: Vec<String>,
}

impl ImmichRule {
    /// The session filter, or `None` for a rule without any criterion (it would
    /// put every photo in the album).
    fn filter(&self) -> Option<Filter> {
        let keywords: Vec<String> = self
            .keywords
            .iter()
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
            .collect();
        let f = Filter {
            min_rating: Rating::new(self.min_rating.min(5)).filter(|r| r.stars() > 0),
            pick: self.pick.map(Into::into),
            tags: self
                .labels
                .iter()
                .filter(|l| **l != ColorLabel::None)
                .map(|l| (*l).into())
                .collect(),
            keywords,
            ..Filter::default()
        };
        let any = f.min_rating.is_some()
            || f.pick.is_some()
            || !f.tags.is_empty()
            || !f.keywords.is_empty();
        any.then_some(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichAlbumResult {
    pub album: String,
    pub created: bool,
    pub added: u32,
    pub removed: u32,
    /// Photos that belong in it but that Immich hasn't indexed yet.
    pub missing: u32,
}

/// What an album sync did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImmichSyncReport {
    pub finished_ms: i64,
    pub sessions: u32,
    pub albums: Vec<ImmichAlbumResult>,
    /// Photos not in Immich yet (not scanned yet, or left out by the library's
    /// exclusion patterns): tried again by the next sync.
    pub missing: u32,
    pub scanned_libraries: u32,
    pub scan_note: Option<String>,
    /// What failed; the rest went on.
    pub failures: Vec<String>,
    /// Nothing was synced: why.
    pub error: Option<String>,
    /// The server didn't answer (the sessions are tried again).
    pub offline: bool,
    /// Immich refused the API key.
    pub key_refused: bool,
}

impl ImmichSyncReport {
    fn new(sessions: usize) -> Self {
        Self {
            finished_ms: now_ms(),
            sessions: sessions as u32,
            albums: Vec::new(),
            missing: 0,
            scanned_libraries: 0,
            scan_note: None,
            failures: Vec::new(),
            error: None,
            offline: false,
            key_refused: false,
        }
    }
}

/// One session as the sync sees it.
fn source_of(
    h: &SessionHandle,
    source: &Path,
    target: &Path,
    mapping: &PathMapping,
    rules: &[(String, Filter)],
) -> Result<Source, String> {
    let store = h.store();
    let (key, folder) =
        crate::archive::archived_as(&store).ok_or_else(|| "not archived yet".to_owned())?;
    let archived = target.join(&folder);
    let scope = mapping.to_immich(&archived).ok_or_else(|| {
        format!(
            "{} isn't inside {} (the path mapping)",
            archived.display(),
            mapping.local_root.display()
        )
    })?;
    let state = PublishState::load(&store).map_err(|e| e.to_string())?;
    let s = lock(&h.session);
    let mut wanted = Vec::new();
    for (album, filter) in rules {
        let ids = s
            .query(filter, Sort::default())
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = ids.iter().map(|v| v.0).collect();
        let mut paths = BTreeSet::new();
        for p in photos(&s, source, Some(&ids)).map_err(|e| e.to_string())? {
            // The developed copy when there is one, else the original.
            let developed = state
                .developed
                .get(&p.variant.id.0)
                .and_then(|d| d.file.clone())
                .filter(|f| source.join(f).is_file());
            let rel = developed.unwrap_or(p.rel);
            if let Some(i) = mapping.to_immich(&archived.join(&rel)) {
                paths.insert(i);
            }
        }
        wanted.push(Wanted {
            album: album.clone(),
            paths: paths.into_iter().collect(),
        });
    }
    Ok(Source { key, scope, wanted })
}

/// Bring the rule albums up to date for the sessions archived since the last
/// sync (`every_session`: every session the archive knows): ask Immich to rescan
/// first (`scan`), then add the matching photos (their developed copies when
/// there are) and remove those Darkslide added that no longer match. Photos the
/// user put in an album by hand are never removed; nothing is ever deleted.
/// `state_path` keeps what Darkslide added (the app's support folder).
/// Why albums aren't synced while the archive goes to object storage.
pub const OBJECT_STORAGE_NOTE: &str =
    "The archive goes to object storage, which Immich can't index: album sync is skipped.";

#[uniffi::export]
#[allow(clippy::too_many_arguments)] // one flat call across the FFI
pub async fn immich_sync_albums(
    http: Arc<dyn ImmichHttp>,
    server: ImmichServer,
    mapping: ImmichPathMapping,
    rules: Vec<ImmichRule>,
    manager: Arc<ArchiveManager>,
    state_path: String,
    scan: bool,
    every_session: bool,
    token: Arc<CancelToken>,
) -> Result<ImmichSyncReport, DsError> {
    let mapping = mapping.mapping()?;
    run_job_on_thread("ds-immich", token, move |cancelled| {
        let (target, sessions) = manager.immich_sessions(every_session);
        let mut report = ImmichSyncReport::new(sessions.len());
        if !manager.archives_to_folder() {
            report.scan_note = Some(OBJECT_STORAGE_NOTE.into());
            return Ok(report);
        }
        let mut filters = Vec::new();
        for r in &rules {
            let album = r.album.trim();
            match (album.is_empty(), r.filter()) {
                (true, _) => report
                    .failures
                    .push("a rule without an album name was skipped".into()),
                (false, None) => report.failures.push(format!(
                    "“{album}” has no criterion (it would hold every photo): skipped"
                )),
                (false, Some(f)) => filters.push((album.to_owned(), f)),
            }
        }
        let mut sources = Vec::new();
        for (h, source) in &sessions {
            match source_of(h, source, &target, &mapping, &filters) {
                Ok(s) => sources.push(s),
                Err(why) => {
                    let name = h.name().unwrap_or_default();
                    report.failures.push(format!("{name}: {why}"));
                }
            }
        }
        if sources.is_empty() {
            return Ok(report);
        }
        let bridge = Bridge(http);
        let client = match Client::new(&bridge, &server.url, &server.api_key) {
            Ok(c) => c.with_cancel(cancelled),
            Err(e) => {
                report.key_refused = matches!(e, ImmichError::Auth(_));
                report.error = Some(e.to_string());
                return Ok(report);
            }
        };
        let state_path = PathBuf::from(state_path);
        let mut state = SyncState::load(&state_path).map_err(ds_error)?;
        let opts = SyncOptions {
            scan,
            album_description: "Kept in sync by Darkslide.".into(),
        };
        let result = ds_immich::sync(&client, &mut state, &sources, &opts);
        state.save(&state_path).map_err(ds_error)?;
        match result {
            Ok(r) => {
                report.scanned_libraries = r.scanned as u32;
                report.scan_note = r.scan_note;
                report.missing = r.missing as u32;
                report.failures.extend(r.failures);
                report.albums = r
                    .albums
                    .into_iter()
                    .map(|a| ImmichAlbumResult {
                        album: a.name,
                        created: a.created,
                        added: a.added as u32,
                        removed: a.removed as u32,
                        missing: a.missing as u32,
                    })
                    .collect();
            }
            Err(ImmichError::Cancelled) => return Err(DsError::Cancelled),
            Err(e) => {
                report.offline = e.is_offline();
                report.key_refused = matches!(e, ImmichError::Auth(_));
                report.error = Some(e.to_string());
                // Not synced: these sessions are tried again next time.
                let handles: Vec<_> = sessions.iter().map(|(h, _)| h.clone()).collect();
                manager.immich_requeue(&handles);
            }
        }
        report.finished_ms = now_ms();
        Ok(report)
    })
    .await
}

#[cfg(test)]
mod tests;
