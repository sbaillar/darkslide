//! The Library (see `docs/FFI.md`): every session's photos in one place.
//!
//! 1. [`SessionHandle::sync_library`]: the open session's photos reach the
//!    Library (only changed rows are written).
//! 2. [`library_refresh`]: every other registered session whose index changed
//!    since its last sync is opened from its store and synced, in the
//!    background; the global index is only locked to read what it knows and
//!    to write, never while a session is read.
//! 3. [`library_query`] / [`library_items`]: the ids a filter keeps, in order,
//!    with the first ones' details; the app pages the rest in.
//! 4. [`library_overview`]: sessions, days, cameras, keywords, collections.
//! 5. [`library_edit`]: a rating, label or flag written through to each
//!    owning session's sidecars (atomic, one undo step there), then to the rows.
//! 6. Collections across sessions ([`library_create_collection`] …).
//! 7. [`DevelopThumbnails::cached_thumbnail`]: a photo's developed thumbnail
//!    straight from the disk cache, when it has one for the edit.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ds_core::{ColorTag, Rating};
use ds_session::{
    LibraryEdit, LibraryItem, LibraryQuery, LibrarySort, LibrarySource, PickFilter, Session,
    apply_library_edit, index_stamp,
};

use super::search::with_global;
use super::{
    CancelToken, ColorLabel, DsError, PickFlag, SessionHandle, has_session, lock, run_job,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LibraryPickFilter {
    All,
    Picked,
    Unflagged,
    Unrejected,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LibrarySortKey {
    /// Capture time (undated last), then file.
    Captured,
    /// Most stars first, then capture time.
    Rating,
    /// File name (numbers as numbers).
    Name,
    /// Session name, then capture time.
    Session,
}

/// What the Library shows: every criterion holds (empty lists: any).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryQueryRecord {
    pub sessions: Vec<i64>,
    /// Capture time in `[from_ms, to_ms)`; undated photos never match a date.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub min_rating: u8,
    pub unrated: bool,
    /// Any of these labels.
    pub labels: Vec<ColorLabel>,
    pub pick: LibraryPickFilter,
    /// Any of these cameras.
    pub cameras: Vec<String>,
    /// Every one of these keywords (any case).
    pub keywords: Vec<String>,
    /// Every word starts a word of the file name, title, keywords, camera,
    /// lens or session name (accents and case folded).
    pub text: String,
    pub collection: Option<i64>,
    pub sort: LibrarySortKey,
    pub descending: bool,
}

/// One photo as the Library grid shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryItemRecord {
    pub id: i64,
    pub session_id: i64,
    /// What to open for its session (the in-place folder or the session).
    pub session_root: String,
    pub session_name: String,
    /// The original.
    pub path: String,
    /// 1 for the master, 2… for the other variants.
    pub ordinal: u32,
    /// The session's variant id (the thumbnail cache's key).
    pub variant: i64,
    pub capture_time_ms: Option<i64>,
    pub rating: u8,
    pub label: ColorLabel,
    pub pick: PickFlag,
    pub camera: String,
    pub lens: String,
    pub keywords: Vec<String>,
    pub title: String,
    pub has_location: bool,
    /// Hex hash of its edit when edited (for [`DevelopThumbnails::cached_thumbnail`]).
    pub edit_hash: Option<String>,
}

/// The ids a query keeps, in order, and the first ones' details.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryPage {
    pub ids: Vec<i64>,
    pub first: Vec<LibraryItemRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibrarySessionRecord {
    pub id: i64,
    pub root: String,
    pub name: String,
    pub count: u64,
    /// The folder is reachable now.
    pub available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct LibraryDayRecord {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryNameCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryCollectionRecord {
    pub id: i64,
    pub name: String,
    /// A smart collection's rules.
    pub rules: Option<LibraryQueryRecord>,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryOverviewRecord {
    pub total: u64,
    pub sessions: Vec<LibrarySessionRecord>,
    /// Days with photos, oldest first.
    pub days: Vec<LibraryDayRecord>,
    pub undated: u64,
    pub cameras: Vec<LibraryNameCount>,
    pub keywords: Vec<LibraryNameCount>,
    pub collections: Vec<LibraryCollectionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LibraryEditRecord {
    Rating { stars: u8 },
    Label { label: ColorLabel },
    Flag { pick: PickFlag },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibraryEditReport {
    /// Photos whose sidecar now says so.
    pub changed: Vec<i64>,
    /// Sessions that couldn't be written ("name: why").
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LibrarySyncReport {
    /// Sessions whose rows changed.
    pub synced: u32,
    pub unchanged: u32,
    /// Offline, or never opened since the Library existed and not found.
    pub unavailable: u32,
    /// Sessions that couldn't be read ("name: why").
    pub failed: Vec<String>,
}

impl From<LibraryQueryRecord> for LibraryQuery {
    fn from(q: LibraryQueryRecord) -> Self {
        LibraryQuery {
            sessions: q.sessions,
            from_ms: q.from_ms,
            to_ms: q.to_ms,
            min_rating: q.min_rating,
            unrated: q.unrated,
            labels: q.labels.into_iter().map(ColorTag::from).collect(),
            pick: match q.pick {
                LibraryPickFilter::All => PickFilter::All,
                LibraryPickFilter::Picked => PickFilter::Picked,
                LibraryPickFilter::Unflagged => PickFilter::Unflagged,
                LibraryPickFilter::Unrejected => PickFilter::Unrejected,
                LibraryPickFilter::Rejected => PickFilter::Rejected,
            },
            cameras: q.cameras,
            keywords: q.keywords,
            text: q.text,
            collection: q.collection,
            sort: match q.sort {
                LibrarySortKey::Captured => LibrarySort::Captured,
                LibrarySortKey::Rating => LibrarySort::Rating,
                LibrarySortKey::Name => LibrarySort::Name,
                LibrarySortKey::Session => LibrarySort::Session,
            },
            descending: q.descending,
        }
    }
}

impl From<LibraryQuery> for LibraryQueryRecord {
    fn from(q: LibraryQuery) -> Self {
        LibraryQueryRecord {
            sessions: q.sessions,
            from_ms: q.from_ms,
            to_ms: q.to_ms,
            min_rating: q.min_rating,
            unrated: q.unrated,
            labels: q.labels.into_iter().map(ColorLabel::from).collect(),
            pick: match q.pick {
                PickFilter::All => LibraryPickFilter::All,
                PickFilter::Picked => LibraryPickFilter::Picked,
                PickFilter::Unflagged => LibraryPickFilter::Unflagged,
                PickFilter::Unrejected => LibraryPickFilter::Unrejected,
                PickFilter::Rejected => LibraryPickFilter::Rejected,
            },
            cameras: q.cameras,
            keywords: q.keywords,
            text: q.text,
            collection: q.collection,
            sort: match q.sort {
                LibrarySort::Captured => LibrarySortKey::Captured,
                LibrarySort::Rating => LibrarySortKey::Rating,
                LibrarySort::Name => LibrarySortKey::Name,
                LibrarySort::Session => LibrarySortKey::Session,
            },
            descending: q.descending,
        }
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*bytes).to_hex().to_string()
}

fn item(i: LibraryItem) -> LibraryItemRecord {
    LibraryItemRecord {
        id: i.id,
        session_id: i.session_id,
        session_root: i.session_root.to_string_lossy().into_owned(),
        session_name: i.session_name,
        path: i.path.to_string_lossy().into_owned(),
        ordinal: i.ordinal,
        variant: i.variant,
        capture_time_ms: i.capture_ms,
        rating: i.rating,
        label: i.label.into(),
        pick: i.pick.into(),
        camera: i.camera,
        lens: i.lens,
        keywords: i.keywords,
        title: i.title,
        has_location: i.gps,
        edit_hash: i.edit.as_ref().map(hex),
    }
}

impl LibraryEditRecord {
    fn edit(self) -> Result<LibraryEdit, DsError> {
        Ok(match self {
            Self::Rating { stars } => LibraryEdit::Rating(
                Rating::new(stars).ok_or_else(|| DsError::invalid(format!("rating {stars}")))?,
            ),
            Self::Label { label } => LibraryEdit::Label(label.into()),
            Self::Flag { pick } => LibraryEdit::Pick(pick.into()),
        })
    }
}

/// Sync `session` (opened from `source.store`) into the Library at `global`
/// as `root`: what the index knows is read, then the session (unlocked), then
/// the changed rows written.
fn sync_into(
    global: &Path,
    root: &Path,
    session: &Session,
    source: &LibrarySource,
) -> Result<bool, DsError> {
    let known = with_global(global, |g| Ok(g.library_known(root)?))?;
    let records = session.library_records(&known)?;
    with_global(global, |g| {
        Ok(g.sync_library(root, session.name(), source, &records)?)
    })
}

impl SessionHandle {
    pub(crate) fn sync_library_with(&self, global: &Path) -> Result<bool, DsError> {
        let s = lock(&self.session);
        let root = self.search_root(s.root());
        let source = LibrarySource {
            store: s.root().to_owned(),
            external: s.external_root().map(Path::to_path_buf),
            stamp: index_stamp(s.root()).unwrap_or(0),
        };
        sync_into(global, &root, &s, &source)
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Bring this session's photos into the Library. Returns whether any changed.
    pub async fn sync_library(
        self: Arc<Self>,
        global_index: String,
        token: Arc<CancelToken>,
    ) -> Result<bool, DsError> {
        run_job(token, move |_| {
            self.sync_library_with(Path::new(&global_index))
        })
        .await
    }
}

/// Where a registered session's store is: as last synced, else found at its
/// root (a session folder, or an in-place folder's `.ds`).
fn store_of(root: &Path, known: Option<&LibrarySource>) -> Option<PathBuf> {
    if let Some(s) = known {
        return Some(s.store.clone());
    }
    [root.to_path_buf(), root.join(super::IN_PLACE_DIR)]
        .into_iter()
        .find(|d| has_session(d))
}

pub(crate) fn refresh(
    global: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<LibrarySyncReport, DsError> {
    let sessions = with_global(global, |g| Ok(g.library_sessions()?))?;
    let mut report = LibrarySyncReport {
        synced: 0,
        unchanged: 0,
        unavailable: 0,
        failed: Vec::new(),
    };
    for s in sessions {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        let Some(store) = store_of(&s.root, s.source.as_ref()) else {
            report.unavailable += 1;
            continue;
        };
        let Some(stamp) = index_stamp(&store) else {
            report.unavailable += 1;
            continue;
        };
        if s.source.as_ref().is_some_and(|k| k.stamp == stamp) {
            report.unchanged += 1;
            continue;
        }
        let synced = Session::open(&store)
            .map_err(DsError::from)
            .and_then(|session| {
                let source = LibrarySource {
                    store: store.clone(),
                    external: session.external_root().map(Path::to_path_buf),
                    stamp,
                };
                sync_into(global, &s.root, &session, &source)
            });
        match synced {
            Ok(true) => report.synced += 1,
            Ok(false) => report.unchanged += 1,
            Err(e) => report.failed.push(format!("{}: {e}", s.name)),
        }
    }
    Ok(report)
}

/// Sync every registered session whose index changed since its last sync
/// (see the module docs). Sessions offline are skipped; their rows stay.
#[uniffi::export]
pub async fn library_refresh(
    global_index: String,
    token: Arc<CancelToken>,
) -> Result<LibrarySyncReport, DsError> {
    run_job(token, move |cancelled| {
        refresh(Path::new(&global_index), cancelled)
    })
    .await
}

/// The ids `query` keeps, in its order, with the first `first` ones' details.
#[uniffi::export]
pub async fn library_query(
    global_index: String,
    query: LibraryQueryRecord,
    first: u32,
    token: Arc<CancelToken>,
) -> Result<LibraryPage, DsError> {
    run_job(token, move |_| {
        with_global(Path::new(&global_index), |g| {
            let ids = g.library_query(&query.into())?;
            let head = &ids[..ids.len().min(first as usize)];
            let first = g.library_items(head)?.into_iter().map(item).collect();
            Ok(LibraryPage { ids, first })
        })
    })
    .await
}

/// These photos' details, in this order (ids no longer in the Library are left out).
#[uniffi::export]
pub async fn library_items(
    global_index: String,
    ids: Vec<i64>,
    token: Arc<CancelToken>,
) -> Result<Vec<LibraryItemRecord>, DsError> {
    run_job(token, move |_| {
        with_global(Path::new(&global_index), |g| {
            Ok(g.library_items(&ids)?.into_iter().map(item).collect())
        })
    })
    .await
}

/// Sessions (and whether each is reachable now), days, cameras, keywords and
/// collections, with counts.
#[uniffi::export]
pub async fn library_overview(
    global_index: String,
    token: Arc<CancelToken>,
) -> Result<LibraryOverviewRecord, DsError> {
    run_job(token, move |_| {
        let o = with_global(Path::new(&global_index), |g| Ok(g.library_overview()?))?;
        let named = |v: Vec<(String, u64)>| {
            v.into_iter()
                .map(|(name, count)| LibraryNameCount { name, count })
                .collect()
        };
        Ok(LibraryOverviewRecord {
            total: o.total,
            sessions: o
                .sessions
                .into_iter()
                .map(|s| LibrarySessionRecord {
                    id: s.id,
                    available: s.root.is_dir(),
                    root: s.root.to_string_lossy().into_owned(),
                    name: s.name,
                    count: s.count,
                })
                .collect(),
            days: o
                .days
                .into_iter()
                .map(|d| LibraryDayRecord {
                    year: d.year,
                    month: d.month,
                    day: d.day,
                    count: d.count,
                })
                .collect(),
            undated: o.undated,
            cameras: named(o.cameras),
            keywords: named(o.keywords),
            collections: o
                .collections
                .into_iter()
                .map(|c| LibraryCollectionRecord {
                    id: c.id,
                    name: c.name,
                    rules: c.rules.map(LibraryQueryRecord::from),
                    count: c.count,
                })
                .collect(),
        })
    })
    .await
}

pub(crate) fn edit(
    global: &Path,
    ids: &[i64],
    edit: LibraryEdit,
) -> Result<LibraryEditReport, DsError> {
    let targets = with_global(global, |g| Ok(g.library_targets(ids)?))?;
    let mut report = LibraryEditReport {
        changed: Vec::new(),
        failed: Vec::new(),
    };
    for t in targets {
        let rels: Vec<(String, u32)> = t.photos.iter().map(|p| p.1.clone()).collect();
        let written =
            Session::open(&t.store).and_then(|mut s| apply_library_edit(&mut s, &rels, edit));
        match written {
            Ok(done) => {
                let done: Vec<i64> = done.into_iter().map(|i| t.photos[i].0).collect();
                with_global(global, |g| Ok(g.library_patch(&done, edit)?))?;
                report.changed.extend(done);
            }
            Err(e) => report
                .failed
                .push(format!("{}: {}", t.session_name, DsError::from(e))),
        }
    }
    Ok(report)
}

/// Rate, label or flag these photos in their own sessions (see the module docs).
#[uniffi::export]
pub async fn library_edit(
    global_index: String,
    ids: Vec<i64>,
    change: LibraryEditRecord,
    token: Arc<CancelToken>,
) -> Result<LibraryEditReport, DsError> {
    let change = change.edit()?;
    run_job(token, move |_| edit(Path::new(&global_index), &ids, change)).await
}

/// A new collection: with `rules` a smart one, else holding `ids`.
#[uniffi::export]
pub async fn library_create_collection(
    global_index: String,
    name: String,
    rules: Option<LibraryQueryRecord>,
    ids: Vec<i64>,
    token: Arc<CancelToken>,
) -> Result<i64, DsError> {
    run_job(token, move |_| {
        let rules = rules.map(LibraryQuery::from);
        with_global(Path::new(&global_index), |g| {
            Ok(g.create_collection(&name, rules.as_ref(), &ids)?)
        })
    })
    .await
}

/// Rename a collection (and change a smart one's rules). Returns whether it exists.
#[uniffi::export]
pub async fn library_update_collection(
    global_index: String,
    id: i64,
    name: String,
    rules: Option<LibraryQueryRecord>,
    token: Arc<CancelToken>,
) -> Result<bool, DsError> {
    run_job(token, move |_| {
        let rules = rules.map(LibraryQuery::from);
        with_global(Path::new(&global_index), |g| {
            Ok(g.update_collection(id, &name, rules.as_ref())?)
        })
    })
    .await
}

#[uniffi::export]
pub async fn library_delete_collection(
    global_index: String,
    id: i64,
    token: Arc<CancelToken>,
) -> Result<bool, DsError> {
    run_job(token, move |_| {
        with_global(Path::new(&global_index), |g| Ok(g.delete_collection(id)?))
    })
    .await
}

/// Add photos to (or with `remove`, take them out of) a plain collection.
/// Returns how many changed.
#[uniffi::export]
pub async fn library_collection_members(
    global_index: String,
    id: i64,
    ids: Vec<i64>,
    remove: bool,
    token: Arc<CancelToken>,
) -> Result<u64, DsError> {
    run_job(token, move |_| {
        with_global(Path::new(&global_index), |g| {
            let n = if remove {
                g.remove_from_collection(id, &ids)?
            } else {
                g.add_to_collection(id, &ids)?
            };
            Ok(n as u64)
        })
    })
    .await
}
