//! People: face indexing storage, clustering, naming (see `docs/FFI.md`).
//!
//! The app detects faces and embeds aligned crops (Apple Vision); Rust stores
//! them per session ([`SessionHandle::store_faces`]), clusters the unnamed ones
//! ([`SessionHandle::people_overview`], `lt_ai::faces`), suggests known people
//! from the app-level people store, and writes the user's answers to the
//! sidecars (names, "not this person") and the store (names, confirmed faces).
//! [`SessionHandle::sync_find`] makes the session's photos, people and faces
//! findable from every session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use lt_ai::faces::{ClusterConfig, Exemplars, Links, cluster, suggest};
use lt_core::FileHash;
use lt_core::geom::NormRect;
use lt_session::{
    Exemplar, Face, FaceRef, NewFace, PeopleStore, PersonCentroid, PersonTag, PhotoPeople,
};

use super::search::{hex, unhex};
use super::{CancelToken, DsError, SessionHandle, lock, run_job};

/// A face box, normalized to the upright photo (origin top left).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct FaceRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl From<FaceRect> for NormRect {
    fn from(r: FaceRect) -> Self {
        NormRect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

impl From<NormRect> for FaceRect {
    fn from(r: NormRect) -> Self {
        FaceRect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

/// A face the app found on a photo.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DetectedFace {
    pub rect: FaceRect,
    /// 0–1 (Vision face capture quality).
    pub quality: f32,
    pub vector: Vec<f32>,
}

/// A photo to scan for faces.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FaceJobRecord {
    pub key: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FaceStatus {
    pub scanned: u64,
    pub total: u64,
    pub waiting: u64,
    pub faces: u64,
}

/// One face: its photo's content key and its index there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct FaceId {
    pub key: String,
    pub idx: u32,
}

/// A face to show.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FaceRecord {
    pub id: FaceId,
    pub rect: FaceRect,
    pub quality: f32,
    /// The original (to crop the face from its preview).
    pub path: String,
    /// Its first variant (to show the photo).
    pub variant_id: i64,
}

/// A named person's faces in this session.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PersonGroup {
    /// The people store's id (`None`: a tag whose name isn't in the store).
    pub person_id: Option<String>,
    pub name: String,
    pub faces: Vec<FaceRecord>,
    /// Photos tagged with them (some without a matching face box).
    pub photos: u64,
    /// Every variant of those photos (the browser's people filter).
    pub variant_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PersonSuggestion {
    pub person_id: String,
    pub name: String,
    pub similarity: f32,
}

/// Faces that look like one person nobody has named.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FaceCluster {
    pub faces: Vec<FaceRecord>,
    /// "Is this Alex?"
    pub suggestion: Option<PersonSuggestion>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PeopleOverview {
    pub people: Vec<PersonGroup>,
    /// Largest first.
    pub unnamed: Vec<FaceCluster>,
    pub status: FaceStatus,
}

/// Clustering and suggestion thresholds (mean cosine similarity).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PeopleOptions {
    pub cluster_threshold: f32,
    pub suggest_threshold: f32,
}

/// A person in the app's people store.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct KnownPerson {
    pub id: String,
    pub name: String,
    /// Earlier names.
    pub aliases: Vec<String>,
}

/// Serializes read-modify-write of people files (one per path).
static STORE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn store(path: &str) -> PeopleStore {
    PeopleStore::new(PathBuf::from(path))
}

fn face_ref(id: &FaceId) -> Result<FaceRef, DsError> {
    Ok(FaceRef {
        key: unhex(&id.key)?,
        idx: id.idx,
    })
}

fn face_id(r: FaceRef) -> FaceId {
    FaceId {
        key: hex(&r.key),
        idx: r.idx,
    }
}

/// What the session knows about each face: its photo and who it is.
struct FaceIndex {
    faces: Vec<Face>,
    /// Content key → photos (the first one is shown).
    photos: HashMap<FileHash, Vec<PhotoPeople>>,
}

impl FaceIndex {
    fn photo(&self, key: &FileHash) -> Option<&PhotoPeople> {
        self.photos.get(key).and_then(|p| p.first())
    }

    /// Who the user said `face` is.
    fn tag(&self, face: &Face) -> Option<&PersonTag> {
        self.photos
            .get(&face.at.key)?
            .iter()
            .flat_map(|p| &p.tags)
            .find(|t| t.same_region(Some(face.rect)))
    }

    /// People the user said `face` isn't.
    fn rejected(&self, face: &Face) -> Vec<&PersonTag> {
        self.photos
            .get(&face.at.key)
            .into_iter()
            .flatten()
            .flat_map(|p| &p.rejected)
            .filter(|t| t.same_region(Some(face.rect)))
            .collect()
    }

    fn record(&self, face: &Face, paths: &HashMap<i64, PathBuf>) -> Option<FaceRecord> {
        let photo = self.photo(&face.at.key)?;
        Some(FaceRecord {
            id: face_id(face.at),
            rect: face.rect.into(),
            quality: face.quality,
            path: paths.get(&photo.image.0)?.to_string_lossy().into_owned(),
            variant_id: photo.variants.first()?.0,
        })
    }
}

/// Store id → that person's confirmed face vectors in this session.
type Confirmed = HashMap<String, Vec<Vec<f32>>>;

/// Named faces grouped by person (by name), the confirmed vectors per store
/// id, and the faces nobody named.
fn named_groups<'a>(
    ix: &'a FaceIndex,
    known: &lt_session::People,
    paths: &HashMap<i64, PathBuf>,
) -> (Vec<PersonGroup>, Confirmed, Vec<&'a Face>) {
    let mut groups: Vec<PersonGroup> = Vec::new();
    let mut confirmed: Confirmed = HashMap::new();
    let mut unnamed: Vec<&Face> = Vec::new();
    for face in &ix.faces {
        let Some(tag) = ix.tag(face) else {
            unnamed.push(face);
            continue;
        };
        let (id, name) = match tag.id.as_deref().and_then(|id| known.by_id(id)) {
            Some(p) => (Some(p.id.clone()), p.name.clone()),
            None => (tag.id.clone(), tag.name.clone()),
        };
        if let Some(id) = &id {
            confirmed
                .entry(id.clone())
                .or_default()
                .push(face.vector.clone());
        }
        let at = groups
            .iter()
            .position(|g| g.person_id == id && g.name.to_lowercase() == name.to_lowercase())
            .unwrap_or_else(|| {
                groups.push(PersonGroup {
                    person_id: id,
                    name,
                    faces: Vec::new(),
                    photos: 0,
                    variant_ids: Vec::new(),
                });
                groups.len() - 1
            });
        if let Some(r) = ix.record(face, paths) {
            groups[at].faces.push(r);
        }
    }
    // Everyone tagged, also without a face box: their photos and variants.
    let mut photos: Vec<&PhotoPeople> = ix.photos.values().flatten().collect();
    photos.sort_by_key(|p| p.image);
    for p in photos {
        for t in &p.tags {
            let (id, name) = match t.id.as_deref().and_then(|id| known.by_id(id)) {
                Some(k) => (Some(k.id.clone()), k.name.clone()),
                None => (t.id.clone(), t.name.clone()),
            };
            let at = groups
                .iter()
                .position(|g| g.person_id == id && g.name.to_lowercase() == name.to_lowercase())
                .unwrap_or_else(|| {
                    groups.push(PersonGroup {
                        person_id: id,
                        name,
                        faces: Vec::new(),
                        photos: 0,
                        variant_ids: Vec::new(),
                    });
                    groups.len() - 1
                });
            let g = &mut groups[at];
            if !p.variants.iter().all(|v| g.variant_ids.contains(&v.0)) {
                g.photos += 1;
                g.variant_ids.extend(p.variants.iter().map(|v| v.0));
            }
        }
    }
    groups.sort_by_key(|g| g.name.to_lowercase());
    (groups, confirmed, unnamed)
}

/// Unnamed faces clustered under the user's links, each with the known person
/// it resembles (store faces plus this session's confirmed ones), unless
/// someone said a face of it isn't them.
#[allow(clippy::too_many_arguments)]
fn unnamed_clusters(
    ix: &FaceIndex,
    paths: &HashMap<i64, PathBuf>,
    unnamed: &[&Face],
    stored_links: &[(FaceRef, FaceRef, bool)],
    known: &lt_session::People,
    confirmed: &Confirmed,
    model: &str,
    opts: PeopleOptions,
) -> Vec<FaceCluster> {
    let at: HashMap<FaceRef, usize> = unnamed.iter().enumerate().map(|(i, f)| (f.at, i)).collect();
    let mut links = Links::default();
    for (a, b, must) in stored_links {
        if let (Some(&i), Some(&j)) = (at.get(a), at.get(b)) {
            if *must {
                links.must.push((i, j));
            } else {
                links.cannot.push((i, j));
            }
        }
    }
    let vectors: Vec<Vec<f32>> = unnamed.iter().map(|f| f.vector.clone()).collect();
    let config = ClusterConfig {
        threshold: opts.cluster_threshold,
    };
    let candidates: Vec<(&lt_session::Person, Exemplars)> = known
        .people
        .iter()
        .filter_map(|p| {
            let mut v = p.vectors(model);
            v.extend(confirmed.get(&p.id).into_iter().flatten().cloned());
            Some((p, Exemplars::of(&v)?))
        })
        .collect();
    cluster(&vectors, &links, config)
        .into_iter()
        .map(|members| {
            let faces: Vec<&Face> = members.iter().map(|&i| unnamed[i]).collect();
            let rejected: HashSet<String> = faces
                .iter()
                .flat_map(|f| ix.rejected(f))
                .flat_map(|t| [t.id.clone().unwrap_or_default(), t.name.to_lowercase()])
                .collect();
            let allowed: Vec<&(&lt_session::Person, Exemplars)> = candidates
                .iter()
                .filter(|(p, _)| {
                    !rejected.contains(&p.id) && !rejected.contains(&p.name.to_lowercase())
                })
                .collect();
            let group = Exemplars::of(&faces.iter().map(|f| f.vector.clone()).collect::<Vec<_>>());
            let suggestion = group.and_then(|g| {
                let ex: Vec<Exemplars> = allowed.iter().map(|c| c.1.clone()).collect();
                let (i, s) = suggest(&g, &ex, opts.suggest_threshold)?;
                Some(PersonSuggestion {
                    person_id: allowed[i].0.id.clone(),
                    name: allowed[i].0.name.clone(),
                    similarity: s,
                })
            });
            FaceCluster {
                faces: faces.iter().filter_map(|f| ix.record(f, paths)).collect(),
                suggestion,
            }
        })
        .filter(|c| !c.faces.is_empty())
        .collect()
}

impl SessionHandle {
    fn face_index(&self, model: &str) -> Result<(FaceIndex, HashMap<i64, PathBuf>), DsError> {
        let s = lock(&self.session);
        let faces = s.faces(model)?;
        let mut photos: HashMap<FileHash, Vec<PhotoPeople>> = HashMap::new();
        let mut paths = HashMap::new();
        let people = s.photo_people()?;
        for img in s.images()? {
            paths.insert(img.id.0, s.original_path(img.rel_path.as_str()));
        }
        for p in people {
            if let Some(k) = p.key {
                photos.entry(k).or_default().push(p);
            }
        }
        Ok((FaceIndex { faces, photos }, paths))
    }

    pub(crate) fn people_overview_with(
        &self,
        model: &str,
        people_store: &Path,
        opts: PeopleOptions,
    ) -> Result<PeopleOverview, DsError> {
        let known = PeopleStore::new(people_store).load()?;
        let (ix, paths) = self.face_index(model)?;
        let (people, confirmed, unnamed) = named_groups(&ix, &known, &paths);
        let links = lock(&self.session).face_links()?;
        let unnamed = unnamed_clusters(
            &ix, &paths, &unnamed, &links, &known, &confirmed, model, opts,
        );
        let c = lock(&self.session).face_counts(model)?;
        Ok(PeopleOverview {
            people,
            unnamed,
            status: FaceStatus {
                scanned: c.scanned as u64,
                total: c.total as u64,
                waiting: c.waiting as u64,
                faces: c.faces as u64,
            },
        })
    }

    /// `faces` with their photos (every photo with that content).
    fn resolve_faces(
        &self,
        model: &str,
        faces: &[FaceId],
    ) -> Result<Vec<(Face, Vec<lt_session::ImageId>)>, DsError> {
        let wanted: HashSet<FaceRef> = faces.iter().map(face_ref).collect::<Result<_, _>>()?;
        let (ix, _) = self.face_index(model)?;
        Ok(ix
            .faces
            .iter()
            .filter(|f| wanted.contains(&f.at))
            .map(|f| {
                let images = ix
                    .photos
                    .get(&f.at.key)
                    .into_iter()
                    .flatten()
                    .map(|p| p.image)
                    .collect();
                (f.clone(), images)
            })
            .collect())
    }

    pub(crate) fn name_faces_with(
        &self,
        model: &str,
        faces: &[FaceId],
        name: &str,
        people_store: &Path,
    ) -> Result<KnownPerson, DsError> {
        let resolved = self.resolve_faces(model, faces)?;
        let _guard = lock(&STORE_LOCK);
        let st = PeopleStore::new(people_store);
        let person = st.update(|p| {
            let Some(person) = p.ensure(name).cloned() else {
                return (None, false);
            };
            let ex = resolved
                .iter()
                .filter_map(|(f, _)| Exemplar::new(hex(&f.at.key), f.at.idx, model, &f.vector))
                .collect();
            p.add_exemplars(&person.id, ex);
            (Some(person), true)
        })?;
        let person = person.ok_or_else(|| DsError::invalid("a person needs a name"))?;
        let tag = PersonTag {
            name: person.name.clone(),
            id: Some(person.id.clone()),
            region: None,
        };
        let targets: Vec<(lt_session::ImageId, Option<NormRect>)> = resolved
            .iter()
            .flat_map(|(f, imgs)| imgs.iter().map(|i| (*i, Some(f.rect))))
            .collect();
        let mut s = lock(&self.session);
        s.tag_person(&targets, &tag)?;
        // Named now: links are no longer needed.
        s.unlink_faces(&resolved.iter().map(|(f, _)| f.at).collect::<Vec<_>>())?;
        Ok(KnownPerson {
            id: person.id,
            name: person.name,
            aliases: person.aliases,
        })
    }

    /// "Not `person`": reject at these faces and drop them from the person's
    /// confirmed faces. `person_id` names a store person; `name` a tag only.
    pub(crate) fn reject_faces_with(
        &self,
        model: &str,
        faces: &[FaceId],
        person_id: Option<&str>,
        name: &str,
        people_store: &Path,
    ) -> Result<(), DsError> {
        let resolved = self.resolve_faces(model, faces)?;
        {
            let _guard = lock(&STORE_LOCK);
            let refs: Vec<(String, u32)> = resolved
                .iter()
                .map(|(f, _)| (hex(&f.at.key), f.at.idx))
                .collect();
            PeopleStore::new(people_store).update(|p| {
                p.remove_exemplars(&refs);
                ((), true)
            })?;
        }
        let tag = PersonTag {
            name: name.to_owned(),
            id: person_id.map(str::to_owned),
            region: None,
        };
        let targets: Vec<(lt_session::ImageId, Option<NormRect>)> = resolved
            .iter()
            .flat_map(|(f, imgs)| imgs.iter().map(|i| (*i, Some(f.rect))))
            .collect();
        lock(&self.session).reject_person(&targets, &tag)?;
        Ok(())
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Photos to scan for faces with `model` (one per content).
    pub async fn face_jobs(
        self: Arc<Self>,
        model: String,
        token: Arc<CancelToken>,
    ) -> Result<Vec<FaceJobRecord>, DsError> {
        run_job(token, move |_| {
            Ok(lock(&self.session)
                .face_jobs(&model)?
                .into_iter()
                .map(|j| FaceJobRecord {
                    key: hex(&j.key),
                    path: j.path.to_string_lossy().into_owned(),
                })
                .collect())
        })
        .await
    }

    /// Store the faces found on the photo with content `key` (replacing any)
    /// and mark it scanned. Returns how many were kept.
    pub async fn store_faces(
        self: Arc<Self>,
        model: String,
        key: String,
        faces: Vec<DetectedFace>,
        token: Arc<CancelToken>,
    ) -> Result<u64, DsError> {
        run_job(token, move |_| {
            let k = unhex(&key)?;
            let faces: Vec<NewFace> = faces
                .into_iter()
                .map(|f| NewFace {
                    rect: f.rect.into(),
                    quality: f.quality,
                    vector: f.vector,
                })
                .collect();
            Ok(lock(&self.session).store_faces(&model, &k, &faces)? as u64)
        })
        .await
    }

    pub async fn face_status(
        self: Arc<Self>,
        model: String,
        token: Arc<CancelToken>,
    ) -> Result<FaceStatus, DsError> {
        run_job(token, move |_| {
            let c = lock(&self.session).face_counts(&model)?;
            Ok(FaceStatus {
                scanned: c.scanned as u64,
                total: c.total as u64,
                waiting: c.waiting as u64,
                faces: c.faces as u64,
            })
        })
        .await
    }

    /// Named people and unnamed face clusters (with "Is this …?" suggestions).
    pub async fn people_overview(
        self: Arc<Self>,
        model: String,
        people_store: String,
        options: PeopleOptions,
        token: Arc<CancelToken>,
    ) -> Result<PeopleOverview, DsError> {
        run_job(token, move |_| {
            self.people_overview_with(&model, Path::new(&people_store), options)
        })
        .await
    }

    /// "Who is this?" / confirm a suggestion / merge into a person: these
    /// faces are `name` (added to the people store if new). Returns the person.
    pub async fn name_faces(
        self: Arc<Self>,
        model: String,
        faces: Vec<FaceId>,
        name: String,
        people_store: String,
        token: Arc<CancelToken>,
    ) -> Result<KnownPerson, DsError> {
        run_job(token, move |_| {
            self.name_faces_with(&model, &faces, &name, Path::new(&people_store))
        })
        .await
    }

    /// "Not this person" (a rejected suggestion, or faces split off a person).
    pub async fn reject_faces(
        self: Arc<Self>,
        model: String,
        faces: Vec<FaceId>,
        person_id: Option<String>,
        name: String,
        people_store: String,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.reject_faces_with(
                &model,
                &faces,
                person_id.as_deref(),
                &name,
                Path::new(&people_store),
            )
        })
        .await
    }

    /// Merge unnamed faces: from now on they cluster as one person.
    pub async fn merge_faces(
        self: Arc<Self>,
        faces: Vec<FaceId>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            let refs: Vec<FaceRef> = faces.iter().map(face_ref).collect::<Result<_, _>>()?;
            let pairs: Vec<(FaceRef, FaceRef)> = refs.windows(2).map(|w| (w[0], w[1])).collect();
            Ok(lock(&self.session).link_faces(&pairs, true)?)
        })
        .await
    }

    /// Split unnamed faces off their cluster: `faces` stay together and never
    /// again with `rest`.
    pub async fn split_faces(
        self: Arc<Self>,
        faces: Vec<FaceId>,
        rest: Vec<FaceId>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            let a: Vec<FaceRef> = faces.iter().map(face_ref).collect::<Result<_, _>>()?;
            let b: Vec<FaceRef> = rest.iter().map(face_ref).collect::<Result<_, _>>()?;
            let mut s = lock(&self.session);
            s.unlink_faces(&a)?;
            let together: Vec<(FaceRef, FaceRef)> = a.windows(2).map(|w| (w[0], w[1])).collect();
            s.link_faces(&together, true)?;
            let apart: Vec<(FaceRef, FaceRef)> = a
                .iter()
                .flat_map(|x| b.iter().map(move |y| (*x, *y)))
                .collect();
            Ok(s.link_faces(&apart, false)?)
        })
        .await
    }
}

/// Everyone in the people store, by name.
#[uniffi::export]
pub fn known_people(people_store: String) -> Result<Vec<KnownPerson>, DsError> {
    crate::jobs::guard(move || {
        let mut out: Vec<KnownPerson> = store(&people_store)
            .load()?
            .people
            .into_iter()
            .map(|p| KnownPerson {
                id: p.id,
                name: p.name,
                aliases: p.aliases,
            })
            .collect();
        out.sort_by_key(|p| p.name.to_lowercase());
        Ok(out)
    })
}

/// Rename a person in the store and on the open session's photos (other
/// sessions' photos match the old name until they are next synced).
#[uniffi::export]
pub async fn rename_person(
    session: Option<Arc<SessionHandle>>,
    people_store: String,
    person_id: String,
    name: String,
    token: Arc<CancelToken>,
) -> Result<bool, DsError> {
    run_job(token, move |_| {
        let renamed = {
            let _guard = lock(&STORE_LOCK);
            store(&people_store).update(|p| {
                let ok = p.rename(&person_id, &name);
                (ok, ok)
            })?
        };
        if renamed && let Some(s) = session {
            lock(&s.session).rename_person(&person_id, name.trim())?;
        }
        Ok(renamed)
    })
    .await
}

/// The people store as suggestion centroids for `model` (plus their aliases).
pub(crate) fn centroids(people_store: &Path, model: &str) -> Result<Vec<PersonCentroid>, DsError> {
    Ok(PeopleStore::new(people_store)
        .load()?
        .people
        .iter()
        .filter_map(|p| PersonCentroid::of(&p.name, &p.vectors(model)))
        .collect())
}

#[cfg(test)]
#[path = "people_tests.rs"]
pub(crate) mod tests;
