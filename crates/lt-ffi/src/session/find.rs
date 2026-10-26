//! Find across sessions: "pics of alex in iceland last summer" (see `docs/FFI.md`).
//!
//! 1. [`SessionHandle::sync_find`]: the open session's photos, people and
//!    faces reach the global index (nothing is written when unchanged), and
//!    face suggestions are refreshed for every session when the people store
//!    or the faces changed.
//! 2. [`find_prompts`]: what the app should embed with the image-text model
//!    (the description, and each place term for "looks like").
//! 3. [`find_photos`]: parse (`lt_ai::find`, rules), resolve people against the
//!    people store and places against the library, filter and rank
//!    (`lt_session::GlobalIndex::find`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use lt_ai::find::{
    CivilDate, FindBackend, FindQuery, FlagQuery, QueryContext, RuleBasedFindParser,
};
use lt_core::PickState;
use lt_session::{
    FindOptions, FindOutcome, FindSpec, MatchReason, PeopleStore, PersonQuery, PlaceSource,
    SessionReason,
};

use super::people::centroids;
use super::search::with_global;
use super::{CancelToken, DsError, PickFlag, SessionHandle, lock, run_job};

/// Today, for "last summer" (the app's calendar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FindDay {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// A place term's embedding.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FindPlaceVector {
    pub term: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct FindOptionsRecord {
    pub limit: u32,
    /// Floor and window on the description's (else the places') similarity.
    pub min_score: f32,
    pub window: f32,
    /// How much a "looks like" place adds under a description.
    pub place_weight: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PlaceSourceRecord {
    Location { value: String },
    SessionName { name: String },
    Folder { name: String },
    Keyword { keyword: String },
    LooksLike,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum MatchReasonRecord {
    Person {
        name: String,
        suggested: bool,
    },
    Place {
        term: String,
        source: PlaceSourceRecord,
    },
    Date,
    Rating {
        stars: u8,
    },
    Flag {
        pick: PickFlag,
    },
    Visual {
        score: f32,
    },
    Text {
        term: String,
    },
    /// The description in its title or caption (the one given).
    Title {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FindPhotoRecord {
    pub session_root: String,
    pub session_name: String,
    pub path: String,
    pub score: f32,
    pub capture_time_ms: Option<i64>,
    pub reasons: Vec<MatchReasonRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SessionReasonRecord {
    Name { term: String },
    Folder { term: String, folder: String },
    Keyword { term: String, keyword: String },
    ManyHits,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FindSessionRecord {
    pub root: String,
    pub name: String,
    /// The folder is reachable now.
    pub available: bool,
    pub reason: SessionReasonRecord,
    pub hits: u64,
    pub best_score: f32,
}

/// A person term and who it means.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FindPersonRecord {
    pub term: String,
    /// Known people it matched, by their current names (empty: nobody by
    /// that name yet).
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FindPlaceRecord {
    pub term: String,
    /// What matched in the library (empty with no "looks like" either:
    /// nothing called that, and nothing to look for).
    pub sources: Vec<PlaceSourceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FindDateRecord {
    pub phrase: String,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// Bit `m - 1` for month `m`; 0 = any.
    pub months: u16,
}

/// How the query was understood.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FindQueryRecord {
    pub people: Vec<FindPersonRecord>,
    pub places: Vec<FindPlaceRecord>,
    pub date: Option<FindDateRecord>,
    pub min_rating: Option<u8>,
    pub max_rating: Option<u8>,
    pub flag: Option<PickFlag>,
    /// The descriptive rest ("beach at sunset"), for the image-text model.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FindResults {
    pub query: FindQueryRecord,
    /// Sessions named after a place or the description, with a matching
    /// folder or keyword, or holding many of the photos.
    pub sessions: Vec<FindSessionRecord>,
    /// Ranked across all sessions, best first.
    pub photos: Vec<FindPhotoRecord>,
    /// Photos matching before the limit.
    pub total: u64,
}

impl From<FindOptionsRecord> for FindOptions {
    fn from(o: FindOptionsRecord) -> Self {
        FindOptions {
            limit: o.limit as usize,
            min_score: o.min_score,
            window: o.window,
            place_weight: o.place_weight,
        }
    }
}

fn source(s: PlaceSource) -> PlaceSourceRecord {
    match s {
        PlaceSource::Location(value) => PlaceSourceRecord::Location { value },
        PlaceSource::SessionName(name) => PlaceSourceRecord::SessionName { name },
        PlaceSource::Folder(name) => PlaceSourceRecord::Folder { name },
        PlaceSource::Keyword(keyword) => PlaceSourceRecord::Keyword { keyword },
        PlaceSource::LooksLike => PlaceSourceRecord::LooksLike,
    }
}

fn reason(r: MatchReason) -> MatchReasonRecord {
    match r {
        MatchReason::Person { name, suggested } => MatchReasonRecord::Person { name, suggested },
        MatchReason::Place { term, source: s } => MatchReasonRecord::Place {
            term,
            source: source(s),
        },
        MatchReason::Date => MatchReasonRecord::Date,
        MatchReason::Rating(stars) => MatchReasonRecord::Rating { stars },
        MatchReason::Flag(p) => MatchReasonRecord::Flag { pick: p.into() },
        MatchReason::Visual { score } => MatchReasonRecord::Visual { score },
        MatchReason::Text { term } => MatchReasonRecord::Text { term },
        MatchReason::Title { text } => MatchReasonRecord::Title { text },
    }
}

fn session_reason(r: SessionReason) -> SessionReasonRecord {
    match r {
        SessionReason::Name { term } => SessionReasonRecord::Name { term },
        SessionReason::Folder { term, folder } => SessionReasonRecord::Folder { term, folder },
        SessionReason::Keyword { term, keyword } => SessionReasonRecord::Keyword { term, keyword },
        SessionReason::ManyHits => SessionReasonRecord::ManyHits,
    }
}

/// When suggestions were last refreshed for (global index, store, face model):
/// the store file's modification time and size then.
type Stamp = Option<(SystemTime, u64)>;
/// (global index, people store, face model).
type RefreshKey = (PathBuf, PathBuf, String);
static REFRESHED: LazyLock<Mutex<HashMap<RefreshKey, Stamp>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn stamp(path: &Path) -> Stamp {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

impl SessionHandle {
    pub(crate) fn sync_find_with(
        &self,
        global: &Path,
        face_model: &str,
        people_store: &Path,
        suggest_threshold: f32,
    ) -> Result<bool, DsError> {
        let (root, name, records, faces) = {
            let s = lock(&self.session);
            let faces: Vec<_> = s
                .faces(face_model)?
                .into_iter()
                .map(|f| (f.at.key, f.at.idx, f.vector))
                .collect();
            (
                self.search_root(s.root()),
                s.name().to_owned(),
                s.find_records()?,
                faces,
            )
        };
        let id = (
            global.to_owned(),
            people_store.to_owned(),
            face_model.to_owned(),
        );
        let now = stamp(people_store);
        let stale = lock(&REFRESHED).get(&id) != Some(&now);
        let changed = with_global(global, |g| {
            let items = g.sync_find_items(&root, &name, &records)?;
            let faces = g.sync_faces(face_model, &faces)?;
            if items || faces || stale {
                let people = centroids(people_store, face_model)?;
                g.refresh_suggestions(face_model, &people, suggest_threshold)?;
            }
            Ok(items || faces)
        })?;
        lock(&REFRESHED).insert(id, now);
        Ok(changed)
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Make this session findable from every session (see the module docs).
    /// Returns whether the global index changed.
    pub async fn sync_find(
        self: Arc<Self>,
        global_index: String,
        face_model: String,
        people_store: String,
        suggest_threshold: f32,
        token: Arc<CancelToken>,
    ) -> Result<bool, DsError> {
        run_job(token, move |_| {
            self.sync_find_with(
                Path::new(&global_index),
                &face_model,
                Path::new(&people_store),
                suggest_threshold,
            )
        })
        .await
    }
}

/// The parsed query, with the people store and the library's places as context.
fn parse(
    text: &str,
    global: &Path,
    people_store: &Path,
    today: FindDay,
    model: &str,
) -> Result<(FindQuery, Vec<lt_session::Person>), DsError> {
    let people = PeopleStore::new(people_store).load()?.people;
    let places = with_global(global, |g| Ok(g.find_places(model)?))?;
    let mut known_people: Vec<String> = Vec::new();
    for p in &people {
        known_people.push(p.name.clone());
        known_people.extend(p.aliases.iter().cloned());
    }
    let ctx = QueryContext {
        known_people,
        known_places: places,
        today: Some(CivilDate::new(today.year, today.month, today.day)),
    };
    Ok((RuleBasedFindParser.parse(text, &ctx), people))
}

/// What to embed for a query: its description and its place terms.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FindPrompts {
    /// The descriptive rest (`None` when the query is all people, places, dates…).
    pub text: Option<String>,
    pub places: Vec<String>,
}

/// What to embed for `text`, as the query parser understood it.
#[uniffi::export]
pub async fn find_prompts(
    text: String,
    global_index: String,
    people_store: String,
    today: FindDay,
    model: String,
    token: Arc<CancelToken>,
) -> Result<FindPrompts, DsError> {
    run_job(token, move |_| {
        let (q, _) = parse(
            &text,
            Path::new(&global_index),
            Path::new(&people_store),
            today,
            &model,
        )?;
        Ok(FindPrompts {
            text: (!q.text.is_empty()).then_some(q.text),
            places: q.places,
        })
    })
    .await
}

/// Who each person term means: the current name of every matched person
/// (shown), and every name they go by, earlier ones too (matched: photos of
/// sessions not synced since a rename still carry them).
fn persons(q: &FindQuery, people: &[lt_session::Person]) -> Vec<(PersonQuery, Vec<String>)> {
    q.people
        .iter()
        .map(|t| {
            let (mut shown, mut names): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
            for m in &t.matches {
                let p = people
                    .iter()
                    .find(|p| p.name == m.name || p.aliases.contains(&m.name));
                let (current, all): (String, Vec<String>) = match p {
                    Some(p) => (
                        p.name.clone(),
                        std::iter::once(p.name.clone())
                            .chain(p.aliases.iter().cloned())
                            .collect(),
                    ),
                    None => (m.name.clone(), vec![m.name.clone()]),
                };
                if !shown.contains(&current) {
                    shown.push(current);
                }
                for n in all {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
            let query = PersonQuery {
                term: t.term.clone(),
                names,
            };
            (query, shown)
        })
        .collect()
}

fn spec(
    q: &FindQuery,
    persons: Vec<PersonQuery>,
    model: String,
    text_vector: Option<Vec<f32>>,
    place_vectors: Vec<FindPlaceVector>,
    options: FindOptionsRecord,
) -> FindSpec {
    let rating = q.rating.unwrap_or_default();
    FindSpec {
        people: persons,
        places: q.places.clone(),
        date: q.date.as_ref().map(|d| d.filter),
        min_rating: rating.min,
        max_rating: rating.max,
        pick: q.flag.map(|f| match f {
            FlagQuery::Picked => PickState::Picked,
            FlagQuery::Rejected => PickState::Rejected,
            FlagQuery::Unflagged => PickState::Unflagged,
        }),
        text: q.text.clone(),
        model,
        text_vector,
        place_vectors: place_vectors
            .into_iter()
            .map(|p| (p.term, p.vector))
            .collect(),
        options: options.into(),
    }
}

fn results(
    q: &FindQuery,
    spec: &FindSpec,
    shown: Vec<Vec<String>>,
    out: FindOutcome,
) -> FindResults {
    let query = FindQueryRecord {
        people: spec
            .people
            .iter()
            .zip(shown)
            .map(|(p, names)| FindPersonRecord {
                term: p.term.clone(),
                names,
            })
            .collect(),
        places: out
            .places
            .into_iter()
            .map(|p| FindPlaceRecord {
                term: p.term,
                sources: p.sources.into_iter().map(source).collect(),
            })
            .collect(),
        date: q.date.as_ref().map(|d| FindDateRecord {
            phrase: d.phrase.clone(),
            from_ms: d.filter.from_ms,
            to_ms: d.filter.to_ms,
            months: d.filter.months,
        }),
        min_rating: spec.min_rating,
        max_rating: spec.max_rating,
        flag: spec.pick.map(PickFlag::from),
        text: q.text.clone(),
    };
    FindResults {
        query,
        sessions: out
            .sessions
            .into_iter()
            .map(|s| FindSessionRecord {
                available: s.root.is_dir(),
                root: s.root.to_string_lossy().into_owned(),
                name: s.name,
                reason: session_reason(s.reason),
                hits: s.hits as u64,
                best_score: s.best_score,
            })
            .collect(),
        photos: out
            .photos
            .into_iter()
            .map(|p| FindPhotoRecord {
                session_root: p.session_root.to_string_lossy().into_owned(),
                session_name: p.session_name,
                path: p.path.to_string_lossy().into_owned(),
                score: p.score,
                capture_time_ms: p.capture_ms,
                reasons: p.reasons.into_iter().map(reason).collect(),
            })
            .collect(),
        total: out.total as u64,
    }
}

/// Find photos and sessions for `text` (see the module docs). The vectors are
/// the image-text model's for [`find_prompts`]' strings (`None` / empty
/// without the model: people, places, dates and ratings still work).
#[allow(clippy::too_many_arguments)]
#[uniffi::export]
pub async fn find_photos(
    text: String,
    global_index: String,
    people_store: String,
    today: FindDay,
    model: String,
    text_vector: Option<Vec<f32>>,
    place_vectors: Vec<FindPlaceVector>,
    options: FindOptionsRecord,
    token: Arc<CancelToken>,
) -> Result<FindResults, DsError> {
    run_job(token, move |_| {
        let global = Path::new(&global_index);
        let (q, people) = parse(&text, global, Path::new(&people_store), today, &model)?;
        let (persons, shown): (Vec<PersonQuery>, Vec<Vec<String>>) =
            persons(&q, &people).into_iter().unzip();
        let spec = spec(&q, persons, model, text_vector, place_vectors, options);
        let out = with_global(global, |g| Ok(g.find(&spec)?))?;
        Ok(results(&q, &spec, shown, out))
    })
    .await
}

#[cfg(test)]
#[path = "find_tests.rs"]
mod tests;
