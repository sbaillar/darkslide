//! Search embeddings (natural-language and "find similar" search).
//!
//! The app embeds each original's preview with an image encoder (`MobileCLIP`,
//! Core ML in Swift) and stores the vector here, normalized and as f16, in the
//! index's `embeddings` table. Rows are keyed by **content identity**, not by
//! path or image id, so a renamed or relinked file, a rebuilt index or the same
//! photo in another session keeps its embedding:
//! - the sensor-data hash (`content_hash`) when known, so metadata edits
//!   (exiftool, geotaggers) don't count as a new photo;
//! - else the whole-file hash; an original not hashed yet has no key and waits.
//!
//! Embeddings are derived data (rebuildable, like the rest of the index). A
//! query scores the session's vectors (cached in memory as one `f32` matrix
//! until the next write) and keeps only variants that pass a [`Filter`].

use std::collections::HashMap;
use std::path::PathBuf;

use ds_core::FileHash;
use ds_core::embedding::{self, EmbeddingMatrix, RankOptions};
use rusqlite::{OptionalExtension, params, params_from_iter};

use crate::error::Result;
use crate::filter::{Filter, Sort};
use crate::relpath::Folder;
use crate::session::{ImageId, ImageInfo, Session, UNHASHED, VariantId};

/// The identity key of an image row (`images i`): content hash, else the file
/// hash unless it is the "not hashed yet" placeholder.
pub(crate) const KEY_SQL: &str = "CASE WHEN i.content_hash IS NOT NULL THEN i.content_hash
     WHEN i.hash <> x'0000000000000000000000000000000000000000000000000000000000000000'
     THEN i.hash END";

/// An original that needs an embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingJob {
    pub image: ImageId,
    pub path: PathBuf,
    pub key: FileHash,
}

/// A variant and its cosine similarity to the query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchHit {
    pub variant: VariantId,
    pub score: f32,
}

/// How many of the session's originals have an embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmbeddingCounts {
    pub embedded: usize,
    /// Originals that can be embedded (hashed, present, outside the Trash).
    pub total: usize,
    /// Present originals outside the Trash not hashed yet (no key: they can be
    /// embedded once the background identity pass has hashed them).
    pub waiting: usize,
}

/// One model's vectors in memory (dropped on every embeddings write), and
/// which row each image uses (rebuilt after any other write: keys change with
/// hashing, relinks and new originals).
#[derive(Debug, Default)]
pub(crate) struct Cache {
    model: String,
    loaded: bool,
    rows: HashMap<FileHash, usize>,
    matrix: EmbeddingMatrix,
    /// `(connection's total changes when built, image id → row)`.
    images: Option<(u64, HashMap<i64, usize>)>,
    /// `PRAGMA data_version` when loaded: another connection (an import's own
    /// `Session` on this index) committed since, so everything is reloaded.
    data_version: i64,
}

/// The embedding key of `info` (see the module docs).
pub fn embedding_key(info: &ImageInfo) -> Option<FileHash> {
    info.content_hash
        .or((info.hash != UNHASHED).then_some(info.hash))
}

fn key_of(bytes: Vec<u8>) -> Option<FileHash> {
    <[u8; 32]>::try_from(bytes).ok().map(FileHash)
}

impl Session {
    /// Originals (present, outside the Trash, hashed) whose content has no
    /// embedding for `model` yet, one per key, in path order. First carries
    /// vectors over to keys that changed (a sensor-data hash computed after the
    /// file hash) and drops vectors no original refers to any more.
    pub fn embedding_jobs(&mut self, model: &str) -> Result<Vec<EmbeddingJob>> {
        let carried = self.conn.execute(
            "INSERT OR IGNORE INTO embeddings(key, model, vec)
             SELECT i.content_hash, e.model, e.vec FROM images i
             JOIN embeddings e ON e.key = i.hash AND e.model = ?1
             WHERE i.content_hash IS NOT NULL",
            [model],
        )?;
        let pruned = self.conn.execute(
            "DELETE FROM embeddings WHERE model = ?1
             AND key NOT IN (SELECT hash FROM images)
             AND key NOT IN (SELECT content_hash FROM images WHERE content_hash IS NOT NULL)",
            [model],
        )?;
        if carried + pruned > 0 {
            self.embed_cache = Cache::default();
        }
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.id, i.rel_path, {KEY_SQL} AS k FROM images i
             WHERE i.missing = 0 AND i.folder <> ?1 AND k IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.key = k AND e.model = ?2)
             ORDER BY i.rel_path"
        ))?;
        let rows: Vec<(i64, String, Vec<u8>)> = st
            .query_map(params![Folder::Trash.code(), model], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut seen = std::collections::HashSet::new();
        Ok(rows
            .into_iter()
            .filter_map(|(id, rel, k)| {
                let key = key_of(k)?;
                seen.insert(key).then(|| EmbeddingJob {
                    image: ImageId(id),
                    path: self.original_path(&rel),
                    key,
                })
            })
            .collect())
    }

    /// Embedded / embeddable / not yet hashed originals (present, outside the Trash).
    pub fn embedding_counts(&self, model: &str) -> Result<EmbeddingCounts> {
        let (all, total, embedded): (i64, i64, i64) = self.conn.query_row(
            &format!(
                "SELECT count(*), count(x.k), count(e.key) FROM (SELECT {KEY_SQL} AS k FROM images i
                 WHERE i.missing = 0 AND i.folder <> ?1) x
                 LEFT JOIN embeddings e ON e.key = x.k AND e.model = ?2"
            ),
            params![Folder::Trash.code(), model],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(EmbeddingCounts {
            embedded: embedded as usize,
            total: total as usize,
            waiting: (all - total) as usize,
        })
    }

    /// Store vectors (normalized here, kept as f16). Vectors that can't be
    /// normalized (empty, zero, not finite) are skipped. Returns how many were
    /// stored.
    pub fn store_embeddings(
        &mut self,
        model: &str,
        items: &[(FileHash, Vec<f32>)],
    ) -> Result<usize> {
        let tx = self.write_lock()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO embeddings(key, model, vec) VALUES (?, ?, ?)",
            )?;
            for (key, v) in items {
                if let Some(unit) = embedding::normalized(v) {
                    st.execute(params![&key.0[..], model, embedding::encode_f16(&unit)])?;
                    n += 1;
                }
            }
        }
        tx.commit()?;
        if n > 0 {
            self.embed_cache = Cache::default();
        }
        Ok(n)
    }

    /// The stored vector for `key` (unit length, from f16).
    pub fn embedding(&self, model: &str, key: &FileHash) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vec FROM embeddings WHERE key = ? AND model = ?",
                params![&key.0[..], model],
                |r| r.get(0),
            )
            .optional()?;
        Ok(bytes.and_then(|b| embedding::decode_f16(&b)))
    }

    /// Embedded originals that other sessions can find (present, outside the
    /// Trash): `(path of the original, key)`, in path order. `only`: just those
    /// with these keys (a batch just stored), without reading the rest.
    pub fn embedded_originals(
        &self,
        model: &str,
        only: Option<&[FileHash]>,
    ) -> Result<Vec<(PathBuf, FileHash)>> {
        let rows: Vec<(String, Vec<u8>)> = if let Some(keys) = only {
            // A batch: indexed lookups by key (content hash, else file hash), not a scan.
            let mut st = self.conn.prepare_cached(&format!(
                "SELECT i.rel_path, {KEY_SQL} AS k FROM images i
                 WHERE (i.content_hash = ?3 OR (i.hash = ?3 AND i.content_hash IS NULL))
                 AND i.missing = 0 AND i.folder <> ?1
                 AND EXISTS (SELECT 1 FROM embeddings e WHERE e.key = ?3 AND e.model = ?2)"
            ))?;
            let mut rows = Vec::new();
            for k in keys {
                let found = st.query_map(params![Folder::Trash.code(), model, &k.0[..]], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
                rows.extend(found.collect::<rusqlite::Result<Vec<(String, Vec<u8>)>>>()?);
            }
            rows.sort();
            rows.dedup();
            rows
        } else {
            let mut st = self.conn.prepare_cached(&format!(
                "SELECT i.rel_path, {KEY_SQL} AS k FROM images i
                 WHERE i.missing = 0 AND i.folder <> ?1 AND k IS NOT NULL
                 AND EXISTS (SELECT 1 FROM embeddings e WHERE e.key = k AND e.model = ?2)
                 ORDER BY i.rel_path"
            ))?;
            st.query_map(params![Folder::Trash.code(), model], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        Ok(rows
            .into_iter()
            .filter_map(|(rel, k)| Some((self.original_path(&rel), key_of(k)?)))
            .collect())
    }

    /// Variants passing `filter`, ranked by similarity of their content's
    /// embedding to `query` (any length the model produces; normalized here).
    /// Variants without an embedding are left out.
    pub fn search_embeddings(
        &mut self,
        model: &str,
        query: &[f32],
        filter: &Filter,
        opts: RankOptions,
    ) -> Result<Vec<SearchHit>> {
        let Some(q) = embedding::normalized(query) else {
            return Ok(Vec::new());
        };
        self.load_cache(model)?;
        let candidates = self.candidates(filter)?;
        Ok(self.rank_candidates(&q, &candidates, None, opts))
    }

    /// Variants passing `filter` that look like `variant` (image-to-image),
    /// most similar first, excluding `variant`'s own original. Empty when that
    /// original has no embedding yet.
    pub fn similar_to(
        &mut self,
        model: &str,
        variant: VariantId,
        filter: &Filter,
        opts: RankOptions,
    ) -> Result<Vec<SearchHit>> {
        let image = self.variant(variant)?.image;
        self.load_cache(model)?;
        let Some(&row) = self.image_rows()?.get(&image.0) else {
            return Ok(Vec::new());
        };
        let q = self.embed_cache.matrix.row(row).to_vec();
        let candidates = self.candidates(filter)?;
        Ok(self.rank_candidates(&q, &candidates, Some(image), opts))
    }

    /// `(variant, image)` of every variant passing `filter`.
    fn candidates(&mut self, filter: &Filter) -> Result<Vec<(VariantId, i64)>> {
        self.image_rows()?;
        let (sql, params) = filter.to_sql_selecting("v.id, v.image_id", Sort::default(), None);
        let mut st = self.conn.prepare_cached(&sql)?;
        Ok(st
            .query_map(params_from_iter(params), |r| {
                Ok((VariantId(r.get(0)?), r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn rank_candidates(
        &self,
        q: &[f32],
        candidates: &[(VariantId, i64)],
        exclude: Option<ImageId>,
        opts: RankOptions,
    ) -> Vec<SearchHit> {
        let cache = &self.embed_cache;
        let Some((_, rows)) = &cache.images else {
            return Vec::new();
        };
        let exclude = exclude.map(|i| i.0);
        let scored = candidates.iter().enumerate().filter_map(|(i, (_, image))| {
            if Some(*image) == exclude {
                return None;
            }
            let row = *rows.get(image)?;
            Some((i, embedding::dot(cache.matrix.row(row), q)))
        });
        embedding::rank(scored, opts)
            .into_iter()
            .map(|(i, score)| SearchHit {
                variant: candidates[i].0,
                score,
            })
            .collect()
    }

    /// Image id → matrix row, rebuilt when anything was written since.
    fn image_rows(&mut self) -> Result<&HashMap<i64, usize>> {
        let stamp = self.conn.total_changes();
        let fresh = matches!(&self.embed_cache.images, Some((s, _)) if *s == stamp);
        if !fresh {
            let mut st = self
                .conn
                .prepare_cached("SELECT id, content_hash, hash FROM images")?;
            let keys: Vec<(i64, Option<Vec<u8>>, Vec<u8>)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let rows = &self.embed_cache.rows;
            // The content key first; the file hash too, for a vector stored before
            // the sensor hash was known (carried over by the next `embedding_jobs`).
            let map = keys
                .into_iter()
                .filter_map(|(id, content, file)| {
                    let row = content
                        .and_then(key_of)
                        .and_then(|k| rows.get(&k))
                        .or_else(|| {
                            key_of(file)
                                .filter(|k| *k != UNHASHED)
                                .and_then(|k| rows.get(&k))
                        })?;
                    Some((id, *row))
                })
                .collect();
            self.embed_cache.images = Some((stamp, map));
        }
        Ok(&self.embed_cache.images.as_ref().expect("set above").1)
    }

    /// Load `model`'s vectors into memory unless they already are.
    fn load_cache(&mut self, model: &str) -> Result<()> {
        let data_version: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        if self.embed_cache.loaded
            && self.embed_cache.model == model
            && self.embed_cache.data_version == data_version
        {
            return Ok(());
        }
        let mut st = self
            .conn
            .prepare_cached("SELECT key, vec FROM embeddings WHERE model = ?")?;
        let rows: Vec<(FileHash, Vec<u8>)> = st
            .query_map([model], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get(1)?)))?
            .filter_map(|r| r.map(|(k, v)| key_of(k).map(|k| (k, v))).transpose())
            .collect::<rusqlite::Result<_>>()?;
        let (matrix, keys) = EmbeddingMatrix::from_f16_rows(rows);
        self.embed_cache = Cache {
            model: model.to_owned(),
            loaded: true,
            rows: keys.into_iter().enumerate().map(|(i, k)| (k, i)).collect(),
            matrix,
            images: None,
            data_version,
        };
        Ok(())
    }
}
