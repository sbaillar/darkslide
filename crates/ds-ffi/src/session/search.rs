//! Natural-language and "find similar" photo search (see `docs/FFI.md`).
//!
//! The app runs the `MobileCLIP` encoders (Core ML); Rust tokenizes the query
//! text ([`ClipTokenizer`]), stores the vectors (`ds-session`, keyed by content
//! identity) and ranks them:
//! 1. `embedding_jobs` → originals still to embed (vectors the global index
//!    already has for the same content are reused, not recomputed);
//! 2. `store_embeddings` → the session index and the global index;
//! 3. `search_embedding` / `find_similar` (this session, with the browser's
//!    filters) or [`search_all_sessions`] (grouped by session).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use ds_core::FileHash;
use ds_core::embedding::{self, EmbeddingMatrix, RankOptions};
use ds_session::{GlobalIndex, GlobalSearchHit};

use super::{CancelToken, DsError, Filter, SessionHandle, SmartFilterRecord, lock, run_job};
use crate::jobs::guard;
use std::sync::Arc;

/// CLIP's logit scale (softmax temperature for zero-shot keywords).
const LOGIT_SCALE: f32 = 100.0;

/// An original that needs an embedding: embed the preview of `path`, store it under `key`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EmbeddingJobRecord {
    /// Content identity (hex); pass it back with the vector.
    pub key: String,
    pub path: String,
}

/// What `embedding_jobs` found.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EmbeddingJobList {
    pub jobs: Vec<EmbeddingJobRecord>,
    /// The global index couldn't be read or updated (busy, damaged, too new):
    /// this session indexes anyway, without reuse or cross-session search.
    pub global_error: Option<String>,
}

/// One image embedding, as the encoder produced it (normalized by Rust).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct EmbeddingRecord {
    pub key: String,
    pub vector: Vec<f32>,
}

/// How far indexing got in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct EmbeddingStatus {
    pub embedded: u64,
    /// Originals that can be embedded (present, hashed, outside the Trash).
    pub total: u64,
    /// Originals not hashed yet (embeddable after the background identity pass).
    pub waiting: u64,
}

/// What `store_embeddings` wrote.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StoreReport {
    pub stored: u64,
    /// The global index couldn't be updated (the session's search still works;
    /// "search all sessions" lacks these until the next indexing run).
    pub global_error: Option<String>,
}

/// Which results count: at most `limit`, scoring ≥ `min_score` and within
/// `window` of the best one (cosine similarity).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct SearchOptions {
    pub limit: u32,
    pub min_score: f32,
    pub window: f32,
}

impl SearchOptions {
    fn rank(self) -> RankOptions {
        RankOptions {
            limit: self.limit as usize,
            min_score: self.min_score,
            window: self.window,
        }
    }
}

/// A variant and its similarity to the query.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct SearchHitRecord {
    pub variant_id: i64,
    pub score: f32,
}

/// A photo found in another (or this) session.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct GlobalPhotoHit {
    pub path: String,
    pub score: f32,
}

/// One session's results in "search all sessions".
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionSearchGroup {
    /// What to open (the folder of an in-place session, the session folder of an imported one).
    pub root: String,
    pub name: String,
    /// The folder is reachable now (an offline volume's sessions still show).
    pub available: bool,
    pub best_score: f32,
    pub hits: Vec<GlobalPhotoHit>,
}

/// Zero-shot keyword candidates: `labels` with their text embeddings
/// (`label_vectors`, concatenated, one per label); the `top` most probable with
/// probability ≥ `min_probability` are suggested.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct KeywordVocabulary {
    pub labels: Vec<String>,
    pub label_vectors: Vec<f32>,
    pub top: u32,
    pub min_probability: f32,
}

/// A zero-shot keyword for a photo.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct KeywordSuggestion {
    pub keyword: String,
    pub probability: f32,
}

pub(crate) fn hex(h: &FileHash) -> String {
    use std::fmt::Write;
    h.0.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub(crate) fn unhex(s: &str) -> Result<FileHash, DsError> {
    let bad = || DsError::invalid(format!("not a content key: {s:?}"));
    if s.len() != 64 || !s.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(FileHash(out))
}

/// Open global indexes, one per path, kept so searches reuse their in-memory vectors.
static GLOBALS: LazyLock<Mutex<HashMap<PathBuf, GlobalIndex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn with_global<T>(
    path: &Path,
    f: impl FnOnce(&mut GlobalIndex) -> Result<T, DsError>,
) -> Result<T, DsError> {
    let mut all = lock(&GLOBALS);
    if !all.contains_key(path) {
        all.insert(path.to_owned(), GlobalIndex::open(path)?);
    }
    f(all.get_mut(path).expect("inserted above"))
}

impl SessionHandle {
    /// The folder the app opens for this session (and lists in other sessions' results).
    pub(crate) fn search_root(&self, session_root: &Path) -> PathBuf {
        self.in_place
            .clone()
            .or_else(|| self.plain_root.clone())
            .unwrap_or_else(|| session_root.to_owned())
    }

    pub(crate) fn embedding_jobs_with(
        &self,
        model: &str,
        global: Option<&Path>,
    ) -> Result<EmbeddingJobList, DsError> {
        let mut jobs = lock(&self.session).embedding_jobs(model)?;
        // A global-index failure never stops this session's indexing (as in `store`).
        let global_error = global
            .and_then(|g| {
                (|| {
                    // Reuse what the global index has for the same content (another
                    // session, or this one before its index was rebuilt).
                    let keys: Vec<FileHash> = jobs.iter().map(|j| j.key).collect();
                    let known = with_global(g, |g| Ok(g.embeddings(model, &keys)?))?;
                    if !known.is_empty() {
                        lock(&self.session).store_embeddings(model, &known)?;
                        let reused: HashSet<FileHash> = known.iter().map(|k| k.0).collect();
                        jobs.retain(|j| !reused.contains(&j.key));
                    }
                    self.sync_global(model, g, None)
                })()
                .err()
            })
            .map(|e| e.to_string());
        Ok(EmbeddingJobList {
            jobs: jobs
                .into_iter()
                .map(|j| EmbeddingJobRecord {
                    key: hex(&j.key),
                    path: j.path.to_string_lossy().into_owned(),
                })
                .collect(),
            global_error,
        })
    }

    /// Make this session's embedded originals searchable from the global
    /// index (current paths; vectors it lacks are copied). `only`: just the
    /// originals with these keys, added to what it has (after a batch);
    /// `None`: replace the session's items (whenever jobs are listed; no
    /// write when nothing changed).
    fn sync_global(
        &self,
        model: &str,
        global: &Path,
        only: Option<&[FileHash]>,
    ) -> Result<(), DsError> {
        let (root, name, items) = {
            let s = lock(&self.session);
            (
                self.search_root(s.root()),
                s.name().to_owned(),
                s.embedded_originals(model, only)?,
            )
        };
        let mut keys: Vec<FileHash> = items.iter().map(|i| i.1).collect();
        keys.sort_unstable_by_key(|k| k.0);
        keys.dedup();
        let missing = with_global(global, |g| {
            match only {
                Some(_) => g.add_search_items(&root, &name, &items)?,
                None => g.sync_search_items(&root, &name, &items)?,
            };
            Ok(g.missing_embeddings(model, &keys)?)
        })?;
        if missing.is_empty() {
            return Ok(());
        }
        let vectors = {
            let s = lock(&self.session);
            missing
                .iter()
                .filter_map(|k| {
                    s.embedding(model, k)
                        .transpose()
                        .map(|v| v.map(|v| (*k, v)))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        with_global(global, |g| Ok(g.store_embeddings(model, &vectors)?))?;
        Ok(())
    }

    pub(crate) fn store_embeddings_with(
        &self,
        model: &str,
        records: &[EmbeddingRecord],
        global: Option<&Path>,
    ) -> Result<StoreReport, DsError> {
        let items = records
            .iter()
            .map(|r| Ok((unhex(&r.key)?, r.vector.clone())))
            .collect::<Result<Vec<_>, DsError>>()?;
        let stored = lock(&self.session).store_embeddings(model, &items)? as u64;
        let batch: Vec<FileHash> = items.iter().map(|i| i.0).collect();
        let global_error = global
            .and_then(|g| self.sync_global(model, g, Some(&batch)).err())
            .map(|e| e.to_string());
        Ok(StoreReport {
            stored,
            global_error,
        })
    }

    fn filter_of(filter: Option<&SmartFilterRecord>) -> Result<Filter, DsError> {
        filter.map_or_else(|| Ok(Filter::default()), SmartFilterRecord::to_filter)
    }

    fn hits(hits: Vec<ds_session::SearchHit>) -> Vec<SearchHitRecord> {
        hits.into_iter()
            .map(|h| SearchHitRecord {
                variant_id: h.variant.0,
                score: h.score,
            })
            .collect()
    }
}

#[uniffi::export]
impl SessionHandle {
    /// Originals still to embed with `model` (one per content). With
    /// `global_index`, vectors it has for the same content are reused, and
    /// this session's embedded originals become searchable from other sessions.
    pub async fn embedding_jobs(
        self: Arc<Self>,
        model: String,
        global_index: Option<String>,
        token: Arc<CancelToken>,
    ) -> Result<EmbeddingJobList, DsError> {
        run_job(token, move |_| {
            self.embedding_jobs_with(&model, global_index.as_deref().map(Path::new))
        })
        .await
    }

    /// Store image embeddings (normalized here, kept as f16) in the session
    /// and, with `global_index`, in the global index.
    pub async fn store_embeddings(
        self: Arc<Self>,
        model: String,
        records: Vec<EmbeddingRecord>,
        global_index: Option<String>,
        token: Arc<CancelToken>,
    ) -> Result<StoreReport, DsError> {
        run_job(token, move |_| {
            self.store_embeddings_with(&model, &records, global_index.as_deref().map(Path::new))
        })
        .await
    }

    /// How many originals have an embedding.
    pub async fn embedding_status(
        self: Arc<Self>,
        model: String,
        token: Arc<CancelToken>,
    ) -> Result<EmbeddingStatus, DsError> {
        run_job(token, move |_| {
            let c = lock(&self.session).embedding_counts(&model)?;
            Ok(EmbeddingStatus {
                embedded: c.embedded as u64,
                total: c.total as u64,
                waiting: c.waiting as u64,
            })
        })
        .await
    }

    /// Variants passing `filter` ranked by similarity to `query` (a text
    /// embedding), best first. Variants not embedded yet are left out.
    pub async fn search_embedding(
        self: Arc<Self>,
        model: String,
        query: Vec<f32>,
        filter: Option<SmartFilterRecord>,
        options: SearchOptions,
        token: Arc<CancelToken>,
    ) -> Result<Vec<SearchHitRecord>, DsError> {
        run_job(token, move |_| {
            let f = Self::filter_of(filter.as_ref())?;
            let hits = lock(&self.session).search_embeddings(&model, &query, &f, options.rank())?;
            Ok(Self::hits(hits))
        })
        .await
    }

    /// "Find similar": variants passing `filter` that look like `variant_id`,
    /// most similar first (its own original excluded). Empty until it is embedded.
    pub async fn find_similar(
        self: Arc<Self>,
        model: String,
        variant_id: i64,
        filter: Option<SmartFilterRecord>,
        options: SearchOptions,
        token: Arc<CancelToken>,
    ) -> Result<Vec<SearchHitRecord>, DsError> {
        run_job(token, move |_| {
            let f = Self::filter_of(filter.as_ref())?;
            let hits = lock(&self.session).similar_to(
                &model,
                ds_session::VariantId(variant_id),
                &f,
                options.rank(),
            )?;
            Ok(Self::hits(hits))
        })
        .await
    }

    /// Zero-shot keywords for a photo from `vocabulary`, most probable first.
    /// Empty until the photo is embedded.
    pub async fn suggest_keywords(
        self: Arc<Self>,
        model: String,
        variant_id: i64,
        vocabulary: KeywordVocabulary,
        token: Arc<CancelToken>,
    ) -> Result<Vec<KeywordSuggestion>, DsError> {
        run_job(token, move |_| {
            let image = {
                let s = lock(&self.session);
                let info = s.image(s.variant(ds_session::VariantId(variant_id))?.image)?;
                match ds_session::embedding_key(&info) {
                    Some(k) => s.embedding(&model, &k)?,
                    None => None,
                }
            };
            let Some(image) = image else {
                return Ok(Vec::new());
            };
            zero_shot_keywords(&image, &vocabulary)
        })
        .await
    }
}

/// Softmax over `labels` (rows of `label_vectors`), best first.
pub(crate) fn zero_shot_keywords(
    image: &[f32],
    vocabulary: &KeywordVocabulary,
) -> Result<Vec<KeywordSuggestion>, DsError> {
    let KeywordVocabulary {
        labels,
        label_vectors,
        top,
        min_probability,
    } = vocabulary;
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let dim = image.len();
    if dim == 0 || label_vectors.len() != dim * labels.len() {
        return Err(DsError::invalid(format!(
            "{} label values for {} labels of {dim} dimensions",
            label_vectors.len(),
            labels.len()
        )));
    }
    let mut m = EmbeddingMatrix::new(dim);
    for row in label_vectors.chunks_exact(dim) {
        m.push(&embedding::normalized(row).unwrap_or_else(|| vec![0.0; dim]));
    }
    let p = embedding::zero_shot(image, &m, LOGIT_SCALE);
    let opts = RankOptions {
        limit: *top as usize,
        min_score: *min_probability,
        window: f32::INFINITY,
    };
    Ok(embedding::rank(p.into_iter().enumerate(), opts)
        .into_iter()
        .map(|(i, probability)| KeywordSuggestion {
            keyword: labels[i].clone(),
            probability,
        })
        .collect())
}

/// Group global hits by session (sessions by their best hit), keeping the
/// hit order within each.
pub(crate) fn group_by_session(
    hits: Vec<GlobalSearchHit>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<SessionSearchGroup> {
    let mut groups: Vec<SessionSearchGroup> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    for h in hits {
        let i = *index.entry(h.session_root.clone()).or_insert_with(|| {
            groups.push(SessionSearchGroup {
                root: h.session_root.to_string_lossy().into_owned(),
                name: h.session_name.clone(),
                available: exists(&h.session_root),
                best_score: h.score,
                hits: Vec::new(),
            });
            groups.len() - 1
        });
        groups[i].hits.push(GlobalPhotoHit {
            path: h.path.to_string_lossy().into_owned(),
            score: h.score,
        });
    }
    groups
}

/// Search every session in the global index, grouped by session (best
/// session first). `options.limit` bounds the photos over all sessions.
#[uniffi::export]
pub async fn search_all_sessions(
    global_index: String,
    model: String,
    query: Vec<f32>,
    options: SearchOptions,
    token: Arc<CancelToken>,
) -> Result<Vec<SessionSearchGroup>, DsError> {
    run_job(token, move |_| {
        let hits = with_global(Path::new(&global_index), |g| {
            Ok(g.search(&model, &query, options.rank())?)
        })?;
        Ok(group_by_session(hits, Path::is_dir))
    })
    .await
}

/// The app forgot the session at `root` (Sessions ▸ Remove from List): it
/// leaves "search all sessions" (its items and the vectors only it used).
/// `root` may be given unresolved; its canonical path is tried too, and when
/// the folder is gone (no canonical path), every recorded root that spells the
/// same folder (`..`, `/private/var` vs `/var`). Returns whether it was
/// searchable.
#[uniffi::export]
pub async fn forget_session_in_search(
    global_index: String,
    root: String,
    token: Arc<CancelToken>,
) -> Result<bool, DsError> {
    run_job(token, move |_| {
        let given = PathBuf::from(&root);
        let mut roots = vec![given.clone()];
        if let Ok(c) = given.canonicalize()
            && c != given
        {
            roots.push(c);
        }
        with_global(Path::new(&global_index), |g| {
            let wanted: Vec<PathBuf> = roots.iter().map(|r| same_folder_key(r)).collect();
            for s in g.sessions()? {
                if !roots.contains(&s.root) && wanted.contains(&same_folder_key(&s.root)) {
                    roots.push(s.root);
                }
            }
            let mut any = false;
            for r in &roots {
                any |= g.forget_search(r)?;
            }
            Ok(any)
        })
    })
    .await
}

/// `p` without `.`/`..` components and macOS's `/private` firmlink prefix: two
/// spellings of one folder that may no longer exist (so can't be canonicalized).
fn same_folder_key(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    for top in ["var", "tmp", "etc"] {
        let private = Path::new("/private").join(top);
        if let Ok(rest) = out.strip_prefix(&private) {
            return Path::new("/").join(top).join(rest);
        }
    }
    out
}

/// Whether the user navigated or rated within the last moment (the same gate
/// that pauses background hashing): background indexing waits while it's true.
#[uniffi::export]
pub fn user_is_active() -> Result<bool, DsError> {
    guard(|| Ok(crate::background::user_active()))
}

/// CLIP's BPE tokenizer (`ds_ai::ClipTokenizer`) for the text encoder.
#[derive(Debug, uniffi::Object)]
pub struct ClipTokenizer {
    inner: ds_ai::ClipTokenizer,
}

#[uniffi::export]
impl ClipTokenizer {
    /// From a CLIP `merges.txt` (downloaded with the model).
    #[uniffi::constructor]
    pub fn from_merges_file(path: String) -> Result<Arc<Self>, DsError> {
        guard(move || {
            let text = std::fs::read_to_string(path)?;
            let inner = ds_ai::ClipTokenizer::from_merges(&text)
                .map_err(|e| DsError::invalid(e.to_string()))?;
            Ok(Arc::new(Self { inner }))
        })
    }

    /// The 77 token ids of `text` (start, BPE tokens, end, zero padding).
    pub fn encode(&self, text: String) -> Result<Vec<i32>, DsError> {
        guard(move || Ok(self.inner.encode(&text)))
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
