//! Search embeddings: storage, identity keys, filtered ranking, the global
//! (cross-session) index.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};

use lt_core::embedding::RankOptions;
use lt_core::geom::Orientation;
use lt_core::{ColorTag, FileHash, Rating};

use crate::*;

const M: &str = "mobileclip-s2";

fn meta(i: i64) -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: Some(1_700_000_000_000 + i * 1000),
        camera: Some("ILCE-9".into()),
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    s: Session,
}

fn fx(name: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    let s = Session::create(&root, name).unwrap();
    Fx { _dir: dir, root, s }
}

impl Fx {
    fn add(&mut self, name: &str, i: i64) -> (ImageId, VariantId, FileHash) {
        let rel = format!("Capture/{name}.ARW");
        let bytes = format!("raw-{name}");
        fs::write(self.root.join(&rel), bytes.as_bytes()).unwrap();
        let h = FileHash(*blake3::hash(bytes.as_bytes()).as_bytes());
        let id = self.s.add_original(&rel, h, meta(i)).unwrap();
        (id, self.s.variants(id).unwrap()[0], h)
    }
}

/// A unit vector along axis `i` (plus a little of `j`) in 8 dimensions.
fn axis(i: usize, j: usize, mix: f32) -> Vec<f32> {
    let mut v = vec![0.0; 8];
    v[i] = 1.0;
    v[j] += mix;
    v
}

fn ids(hits: &[SearchHit]) -> Vec<VariantId> {
    hits.iter().map(|h| h.variant).collect()
}

#[test]
fn embeddings_round_trip_and_jobs_shrink_as_they_are_stored() {
    let mut f = fx("Trip");
    let (a, _, ka) = f.add("a", 0);
    let (_, _, kb) = f.add("b", 1);
    let jobs = f.s.embedding_jobs(M).unwrap();
    assert_eq!(
        jobs.iter().map(|j| j.key).collect::<Vec<_>>(),
        vec![ka, kb],
        "path order"
    );
    assert_eq!(jobs[0].image, a);
    assert_eq!(jobs[0].path, f.root.join("Capture/a.ARW"));
    assert_eq!(
        f.s.embedding_counts(M).unwrap(),
        EmbeddingCounts {
            embedded: 0,
            total: 2,
            waiting: 0
        }
    );

    let raw: Vec<f32> = (0..512).map(|i| (i as f32 * 0.37).sin()).collect();
    let stored =
        f.s.store_embeddings(M, &[(ka, raw.clone()), (kb, vec![0.0; 512]), (kb, vec![])])
            .unwrap();
    assert_eq!(stored, 1, "zero and empty vectors are skipped");
    let back = f.s.embedding(M, &ka).unwrap().unwrap();
    let unit = lt_core::embedding::normalized(&raw).unwrap();
    let err = unit
        .iter()
        .zip(&back)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max);
    assert!(err < 1e-3, "f16 round trip error {err}");
    assert_eq!(f.s.embedding(M, &kb).unwrap(), None);
    assert_eq!(f.s.embedding("other-model", &ka).unwrap(), None);
    assert_eq!(
        f.s.embedding_jobs(M)
            .unwrap()
            .iter()
            .map(|j| j.key)
            .collect::<Vec<_>>(),
        vec![kb]
    );
    assert_eq!(
        f.s.embedding_jobs("other-model").unwrap().len(),
        2,
        "per model"
    );
    assert_eq!(f.s.embedding_counts(M).unwrap().embedded, 1);

    // The index is derived data, but it survives a reopen.
    f.s = Session::open(&f.root).unwrap();
    assert!(f.s.embedding(M, &ka).unwrap().is_some());
}

#[test]
fn jobs_skip_unhashed_missing_trashed_and_duplicate_content() {
    let mut f = fx("Skip");
    let (_, va, _) = f.add("a", 0);
    let (b, _, _) = f.add("b", 1);
    let (_, _, kc) = f.add("c", 2);
    // d: the same bytes as c under another name (one job per content).
    fs::write(f.root.join("Capture/d.ARW"), b"raw-c").unwrap();
    f.s.add_original("Capture/d.ARW", kc, meta(3)).unwrap();
    f.s.move_to(&[va], Folder::Trash).unwrap();
    f.s.conn
        .execute("UPDATE images SET missing = 1 WHERE id = ?", [b.0])
        .unwrap();
    fs::write(f.root.join("Capture/e.ARW"), b"e").unwrap();
    f.s.add_original("Capture/e.ARW", UNHASHED, meta(4))
        .unwrap();
    let jobs = f.s.embedding_jobs(M).unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].key, kc);
    assert_eq!(
        f.s.embedding_counts(M).unwrap(),
        EmbeddingCounts {
            embedded: 0,
            total: 2,
            waiting: 1
        },
        "c and d; e isn't hashed yet"
    );
}

#[test]
fn key_is_content_hash_then_file_hash_never_the_placeholder() {
    let mut f = fx("Keys");
    let (a, _, ka) = f.add("a", 0);
    let mut info = f.s.image(a).unwrap();
    assert_eq!(embedding_key(&info), Some(ka));
    info.content_hash = Some(FileHash([7; 32]));
    assert_eq!(embedding_key(&info), Some(FileHash([7; 32])));
    info.content_hash = None;
    info.hash = UNHASHED;
    assert_eq!(embedding_key(&info), None);
}

#[test]
fn a_vector_follows_its_content_when_the_sensor_hash_arrives_and_orphans_go() {
    let mut f = fx("Carry");
    let (a, va, ka) = f.add("a", 0);
    f.s.store_embeddings(M, &[(ka, axis(0, 1, 0.0))]).unwrap();
    f.s.store_embeddings(M, &[(FileHash([9; 32]), axis(1, 0, 0.0))])
        .unwrap();
    // The sensor-data hash is learned later (e.g. a verify pass): same photo.
    let content = FileHash([5; 32]);
    f.s.conn
        .execute(
            "UPDATE images SET content_hash = ? WHERE id = ?",
            rusqlite::params![&content.0[..], a.0],
        )
        .unwrap();
    assert!(
        f.s.embedding_jobs(M).unwrap().is_empty(),
        "carried over, no re-embed"
    );
    assert!(f.s.embedding(M, &content).unwrap().is_some());
    assert_eq!(
        f.s.embedding(M, &FileHash([9; 32])).unwrap(),
        None,
        "orphan pruned"
    );
    let hits =
        f.s.search_embeddings(
            M,
            &axis(0, 1, 0.0),
            &Filter::default(),
            RankOptions::default(),
        )
        .unwrap();
    assert_eq!(ids(&hits), vec![va]);
}

#[test]
fn search_ranks_by_cosine_and_honours_filters_and_thresholds() {
    let mut f = fx("Rank");
    let (_, beach, kb) = f.add("beach", 0);
    let (_, sunset, ks) = f.add("sunset", 1);
    let (_, car, kc) = f.add("car", 2);
    let (_, plain, _) = f.add("not-embedded", 3);
    f.s.store_embeddings(
        M,
        &[
            (kb, axis(0, 1, 0.3)),
            (ks, axis(1, 0, 0.3)),
            (kc, axis(2, 3, 0.1)),
        ],
    )
    .unwrap();
    let q = axis(0, 1, 0.5);
    let all =
        f.s.search_embeddings(M, &q, &Filter::default(), RankOptions::default())
            .unwrap();
    assert_eq!(
        ids(&all),
        vec![beach, sunset, car],
        "not-embedded is left out"
    );
    assert!(all[0].score > all[1].score && all[1].score > all[2].score);
    assert!(
        (all[0].score - 0.99).abs() < 0.02,
        "cosine, normalized: {}",
        all[0].score
    );
    assert!(!ids(&all).contains(&plain));

    // Relevance floor and window.
    let floor = RankOptions {
        min_score: 0.2,
        ..RankOptions::default()
    };
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), floor)
            .unwrap()),
        vec![beach, sunset]
    );
    let top1 = RankOptions {
        limit: 1,
        ..RankOptions::default()
    };
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), top1)
            .unwrap()),
        vec![beach]
    );

    // Combined with the filters: rating, colour tag, capture date.
    f.s.set_rating(&[sunset, car], Rating::new(3).unwrap())
        .unwrap();
    f.s.set_color_tag(&[car], ColorTag::Red).unwrap();
    let rated = Filter {
        min_rating: Rating::new(2),
        ..Filter::default()
    };
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &rated, RankOptions::default())
            .unwrap()),
        vec![sunset, car]
    );
    let red = Filter {
        tags: vec![ColorTag::Red],
        ..rated.clone()
    };
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &red, RankOptions::default())
            .unwrap()),
        vec![car]
    );
    let dated = Filter {
        date_from_ms: Some(1_700_000_001_000),
        date_to_ms: Some(1_700_000_001_000),
        ..Filter::default()
    };
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &dated, RankOptions::default())
            .unwrap()),
        vec![sunset]
    );

    // A query that can't be normalized finds nothing; another model has nothing.
    assert_eq!(
        f.s.search_embeddings(M, &[0.0; 8], &Filter::default(), RankOptions::default())
            .unwrap(),
        [] as [embed::SearchHit; 0]
    );
    assert_eq!(
        f.s.search_embeddings("other", &q, &Filter::default(), RankOptions::default())
            .unwrap(),
        [] as [embed::SearchHit; 0]
    );
    // Back to the first model: its cache is rebuilt.
    assert_eq!(
        f.s.search_embeddings(M, &q, &Filter::default(), RankOptions::default())
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn every_variant_of_a_match_is_a_hit_and_trash_is_excluded() {
    let mut f = fx("Variants");
    let (a, va, ka) = f.add("a", 0);
    let (_, vb, kb) = f.add("b", 1);
    let va2 = f.s.new_variant(a).unwrap();
    f.s.store_embeddings(M, &[(ka, axis(0, 1, 0.0)), (kb, axis(0, 1, 0.5))])
        .unwrap();
    let q = axis(0, 1, 0.0);
    let hits =
        f.s.search_embeddings(M, &q, &Filter::default(), RankOptions::default())
            .unwrap();
    let mut first_two = ids(&hits[..2]);
    first_two.sort();
    let mut expected = vec![va, va2];
    expected.sort();
    assert_eq!(first_two, expected);
    assert_eq!(hits[2].variant, vb);
    assert_eq!(hits[0].score, hits[1].score);
    f.s.move_to(&[vb], Folder::Trash).unwrap();
    let hits =
        f.s.search_embeddings(M, &q, &Filter::default(), RankOptions::default())
            .unwrap();
    assert!(!ids(&hits).contains(&vb), "the Trash is never searched");
}

#[test]
fn find_similar_excludes_the_photo_itself() {
    let mut f = fx("Similar");
    let (a, va, ka) = f.add("a", 0);
    let (_, vb, kb) = f.add("b", 1);
    let (_, vc, kc) = f.add("c", 2);
    let (_, vd, _) = f.add("d", 3);
    let va2 = f.s.new_variant(a).unwrap();
    f.s.store_embeddings(
        M,
        &[
            (ka, axis(0, 1, 0.0)),
            (kb, axis(2, 0, 0.2)),
            (kc, axis(0, 1, 0.3)),
        ],
    )
    .unwrap();
    let hits =
        f.s.similar_to(M, va, &Filter::default(), RankOptions::default())
            .unwrap();
    assert_eq!(
        ids(&hits),
        vec![vc, vb],
        "its other variant isn't 'similar', it's the same photo"
    );
    assert!(!ids(&hits).contains(&va2));
    assert_eq!(
        f.s.similar_to(M, vd, &Filter::default(), RankOptions::default())
            .unwrap(),
        [] as [embed::SearchHit; 0]
    );
    assert!(
        f.s.similar_to(
            M,
            VariantId(999_999),
            &Filter::default(),
            RankOptions::default()
        )
        .is_err()
    );
    // An unhashed original has no key.
    fs::write(f.root.join("Capture/u.ARW"), b"u").unwrap();
    let u =
        f.s.add_original("Capture/u.ARW", UNHASHED, meta(9))
            .unwrap();
    let vu = f.s.variants(u).unwrap()[0];
    assert_eq!(
        f.s.similar_to(M, vu, &Filter::default(), RankOptions::default())
            .unwrap(),
        [] as [embed::SearchHit; 0]
    );
}

#[test]
fn new_vectors_are_seen_by_the_next_search_and_damaged_rows_are_skipped() {
    let mut f = fx("Cache");
    let (_, va, ka) = f.add("a", 0);
    let (_, vb, kb) = f.add("b", 1);
    f.s.store_embeddings(M, &[(ka, axis(0, 1, 0.0))]).unwrap();
    let q = axis(0, 1, 0.0);
    let opts = RankOptions::default();
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap()),
        vec![va]
    );
    f.s.store_embeddings(M, &[(kb, axis(0, 1, 0.1))]).unwrap();
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap()),
        vec![va, vb]
    );
    // Rows another build (or a bug) wrote: wrong length, odd bytes, bad key.
    f.s.conn
        .execute(
            "UPDATE embeddings SET vec = x'0000003c' WHERE key = ?",
            [&kb.0[..]],
        )
        .unwrap();
    f.s.conn
        .execute_batch(&format!(
            "INSERT INTO embeddings VALUES (x'01', '{M}', x'0000'); \
             INSERT INTO embeddings VALUES (x'{}', '{M}', x'00');",
            "02".repeat(32)
        ))
        .unwrap();
    f.s = Session::open(&f.root).unwrap();
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap()),
        vec![va]
    );
}

// --------------------------------------------------------- renames (identity) ---

#[test]
fn a_finder_rename_keeps_the_embedding_no_re_embed() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("a.ARW"), b"aaaa").unwrap();
    let root = t.path().join("store/session");
    let mut s = Session::create_external(&root, "Shoot", &folder).unwrap();
    let sync = s.sync_external().unwrap();
    let items = sync
        .untracked
        .iter()
        .map(|r| (r.clone(), UNHASHED, meta(0)))
        .collect();
    let ids_ = s.add_originals(items).unwrap();
    assert!(
        s.embedding_jobs(M).unwrap().is_empty(),
        "not hashed yet: waits"
    );
    for (id, path) in s.unhashed_images().unwrap() {
        let st = original_stat(&path).unwrap();
        let h = FileHash(*blake3::hash(&fs::read(&path).unwrap()).as_bytes());
        assert!(s.set_verified_hash(id, h, st).unwrap());
    }
    let jobs = s.embedding_jobs(M).unwrap();
    assert_eq!(jobs.len(), 1);
    s.store_embeddings(M, &[(jobs[0].key, axis(0, 1, 0.0))])
        .unwrap();

    fs::rename(folder.join("a.ARW"), folder.join("Keeper.ARW")).unwrap();
    let r = s.sync_external().unwrap();
    assert_eq!(r.relinked.len(), 1);
    assert!(
        s.embedding_jobs(M).unwrap().is_empty(),
        "same content: nothing to embed"
    );
    let v = s.variants(ids_[0]).unwrap()[0];
    let hits = s
        .search_embeddings(
            M,
            &axis(0, 1, 0.0),
            &Filter::default(),
            RankOptions::default(),
        )
        .unwrap();
    assert_eq!(ids(&hits), vec![v]);
    let found = s.embedded_originals(M, None).unwrap();
    assert_eq!(found, vec![(folder.join("Keeper.ARW"), jobs[0].key)]);
    assert_eq!(
        s.embedded_originals(M, Some(&[jobs[0].key])).unwrap(),
        found
    );
    assert_eq!(
        s.embedded_originals(M, Some(&[FileHash([9; 32])])).unwrap(),
        [] as [(std::path::PathBuf, lt_core::FileHash); 0]
    );
}

// ------------------------------------------------------------- global index ---

fn global(dir: &Path) -> GlobalIndex {
    GlobalIndex::open(&dir.join("global.db")).unwrap()
}

#[test]
fn search_across_sessions_groups_by_session_and_follows_writes() {
    let mut one = fx("Crete");
    let mut two = fx("Sicily");
    let (_, _, kw) = one.add("waterfall", 0);
    let (_, _, ks) = one.add("sea", 1);
    let (_, _, kc) = two.add("cliffs", 0);
    // The same photo imported into both sessions.
    fs::write(
        two.root.join("Capture/waterfall-copy.ARW"),
        b"raw-waterfall",
    )
    .unwrap();
    two.s
        .add_original("Capture/waterfall-copy.ARW", kw, meta(1))
        .unwrap();
    one.s
        .store_embeddings(M, &[(kw, axis(0, 1, 0.0)), (ks, axis(2, 0, 0.0))])
        .unwrap();
    two.s.store_embeddings(M, &[(kc, axis(3, 0, 0.0))]).unwrap();

    let d = tempfile::tempdir().unwrap();
    let mut g = global(d.path());
    for f in [&mut one, &mut two] {
        // What the app does per session: reuse vectors the global index has
        // (the copy in Sicily is the same photo as in Crete), embed the rest.
        let jobs = f.s.embedding_jobs(M).unwrap();
        let keys: Vec<FileHash> = jobs.iter().map(|j| j.key).collect();
        let reused = g.embeddings(M, &keys).unwrap();
        f.s.store_embeddings(M, &reused).unwrap();
        assert_eq!(
            f.s.embedding_jobs(M).unwrap(),
            [] as [embed::EmbeddingJob; 0]
        );
        let items = f.s.embedded_originals(M, None).unwrap();
        let keys: Vec<FileHash> = items.iter().map(|i| i.1).collect();
        g.sync_search_items(&f.root, f.s.name(), &items).unwrap();
        let missing = g.missing_embeddings(M, &keys).unwrap();
        let vecs: Vec<(FileHash, Vec<f32>)> = missing
            .iter()
            .filter_map(|k| Some((*k, f.s.embedding(M, k).unwrap()?)))
            .collect();
        g.store_embeddings(M, &vecs).unwrap();
    }
    assert_eq!(
        g.missing_embeddings(M, &[kw, ks, kc]).unwrap(),
        [] as [lt_core::FileHash; 0]
    );
    assert_eq!(g.embeddings(M, &[kw, FileHash([1; 32])]).unwrap().len(), 1);

    let hits = g
        .search(M, &axis(0, 1, 0.0), RankOptions::default())
        .unwrap();
    let top: Vec<(String, PathBuf)> = hits
        .iter()
        .take(2)
        .map(|h| (h.session_name.clone(), h.path.clone()))
        .collect();
    assert!(top.contains(&("Crete".into(), one.root.join("Capture/waterfall.ARW"))));
    assert!(top.contains(&("Sicily".into(), two.root.join("Capture/waterfall-copy.ARW"))));
    assert_eq!(hits.len(), 4);
    assert!(hits[0].score > 0.99 && hits[2].score < 0.01);
    assert_eq!(
        hits[0].session_root.file_name(),
        Some(hits[0].session_name.as_ref())
    );
    let strict = RankOptions {
        min_score: 0.5,
        ..RankOptions::default()
    };
    assert_eq!(g.search(M, &axis(0, 1, 0.0), strict).unwrap().len(), 2);

    // A write (a session dropped) is seen by the next search (cache reload).
    assert!(g.unregister_session(&two.root).unwrap());
    let hits = g
        .search(M, &axis(0, 1, 0.0), RankOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.session_name == "Crete"));
    // Sicily's own vector went with it; the one Crete shares stays.
    assert_eq!(g.embedding_count(M).unwrap(), 2);
    assert_eq!(g.missing_embeddings(M, &[kw, kc]).unwrap(), vec![kc]);
    // A re-sync replaces the session's items.
    g.sync_search_items(
        &one.root,
        "Crete",
        &[(one.root.join("Capture/sea.ARW"), ks)],
    )
    .unwrap();
    assert_eq!(
        g.search(M, &axis(2, 0, 0.0), RankOptions::default())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        g.search(M, &[0.0; 8], RankOptions::default()).unwrap(),
        [] as [global::GlobalSearchHit; 0]
    );
    assert_eq!(
        g.search("other", &axis(2, 0, 0.0), RankOptions::default())
            .unwrap(),
        [] as [global::GlobalSearchHit; 0]
    );
    assert_eq!(
        g.store_embeddings(M, &[(kc, vec![f32::NAN; 8])]).unwrap(),
        0
    );
}

#[test]
fn a_v1_global_index_is_upgraded_keeping_its_originals() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("global.db");
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(
        "CREATE TABLE sessions(id INTEGER PRIMARY KEY, root TEXT NOT NULL UNIQUE, name TEXT NOT NULL);
         CREATE TABLE originals(session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             rel_path TEXT NOT NULL, hash BLOB NOT NULL, PRIMARY KEY(session_id, rel_path)) WITHOUT ROWID;
         CREATE INDEX originals_hash ON originals(hash);
         INSERT INTO sessions VALUES (1, '/s', 'S');
         INSERT INTO originals VALUES (1, 'Capture/a.ARW', x'0101010101010101010101010101010101010101010101010101010101010101');
         PRAGMA user_version = 1;",
    )
    .unwrap();
    drop(c);
    let mut g = GlobalIndex::open(&path).unwrap();
    assert!(g.contains(&FileHash([1; 32])).unwrap(), "kept");
    g.sync_search_items(
        Path::new("/s"),
        "S",
        &[(PathBuf::from("/s/a.ARW"), FileHash([1; 32]))],
    )
    .unwrap();
    g.store_embeddings(M, &[(FileHash([1; 32]), axis(0, 1, 0.0))])
        .unwrap();
    assert_eq!(
        g.search(M, &axis(0, 1, 0.0), RankOptions::default())
            .unwrap()
            .len(),
        1
    );
    drop(g);
    let v: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        v, 6,
        "v1 -> v6 in one step (find rows with titles and positions, the library)"
    );
}

#[test]
fn unchanged_syncs_write_nothing_and_renames_reach_results() {
    let d = tempfile::tempdir().unwrap();
    let mut g = global(d.path());
    let root = PathBuf::from("/pics/Crete");
    let a = (root.join("a.ARW"), FileHash([1; 32]));
    let b = (root.join("b.ARW"), FileHash([2; 32]));
    g.store_embeddings(M, &[(a.1, axis(0, 1, 0.0)), (b.1, axis(1, 0, 0.0))])
        .unwrap();
    assert!(
        g.sync_search_items(&root, "Crete", std::slice::from_ref(&a))
            .unwrap()
    );
    let gen0 = g.generation().unwrap();
    assert!(
        !g.sync_search_items(&root, "Crete", std::slice::from_ref(&a))
            .unwrap(),
        "same items: no write"
    );
    assert_eq!(
        g.generation().unwrap(),
        gen0,
        "the search cache stays valid"
    );
    // Batches add to what is there.
    assert!(
        g.add_search_items(&root, "Crete", std::slice::from_ref(&b))
            .unwrap()
    );
    assert!(
        !g.add_search_items(&root, "Crete", std::slice::from_ref(&b))
            .unwrap()
    );
    assert_eq!(
        g.search(M, &axis(1, 0, 0.0), RankOptions::default())
            .unwrap()
            .len(),
        2
    );
    // A renamed session shows its new name.
    g.register_session(&root, "Crete 2026").unwrap();
    let hits = g
        .search(M, &axis(1, 0, 0.0), RankOptions::default())
        .unwrap();
    assert!(hits.iter().all(|h| h.session_name == "Crete 2026"));
    let gen1 = g.generation().unwrap();
    g.register_session(&root, "Crete 2026").unwrap();
    assert_eq!(g.generation().unwrap(), gen1, "same name: no bump");
}

#[test]
fn the_search_migration_can_run_twice() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("global.db");
    drop(GlobalIndex::open(&path).unwrap());
    // A second opener that read user_version = 1 before the first upgraded it.
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA user_version = 1").unwrap();
    drop(c);
    assert!(GlobalIndex::open(&path).is_ok());
    // A second opener that read user_version = 0 before the first created it.
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA user_version = 0").unwrap();
    drop(c);
    let mut g = GlobalIndex::open(&path).unwrap();
    assert!(
        g.register_session(Path::new("/pics/Crete"), "Crete")
            .is_ok()
    );
}

#[test]
fn another_connections_vectors_and_a_late_sensor_hash_are_found() {
    let mut f = fx("Conn");
    let (a, va, ka) = f.add("a", 0);
    let (_, vb, kb) = f.add("b", 1);
    f.s.store_embeddings(M, &[(ka, axis(0, 1, 0.0))]).unwrap();
    let q = axis(0, 1, 0.0);
    let opts = RankOptions::default();
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap()),
        vec![va]
    );
    // Another `Session` on the same index (an import's) stores a vector.
    let mut other = Session::open(&f.root).unwrap();
    other.store_embeddings(M, &[(kb, axis(0, 1, 0.1))]).unwrap();
    drop(other);
    assert_eq!(
        ids(&f
            .s
            .search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap()),
        vec![va, vb]
    );
    // The sensor hash arrives after the vector was stored under the file hash:
    // still found before `embedding_jobs` carries it over.
    f.s.conn
        .execute(
            "UPDATE images SET content_hash = ? WHERE id = ?",
            rusqlite::params![&[7u8; 32][..], a.0],
        )
        .unwrap();
    let hits =
        f.s.search_embeddings(M, &q, &Filter::default(), opts)
            .unwrap();
    assert!(ids(&hits).contains(&va));
}

#[test]
fn a_forgotten_session_leaves_search_but_keeps_import_dedupe() {
    let d = tempfile::tempdir().unwrap();
    let mut g = global(d.path());
    let (crete, sicily) = (PathBuf::from("/pics/Crete"), PathBuf::from("/pics/Sicily"));
    let shared = FileHash([1; 32]);
    let only_crete = FileHash([2; 32]);
    g.store_embeddings(
        M,
        &[(shared, axis(0, 1, 0.0)), (only_crete, axis(1, 0, 0.0))],
    )
    .unwrap();
    g.sync_search_items(
        &crete,
        "Crete",
        &[
            (crete.join("a.ARW"), shared),
            (crete.join("b.ARW"), only_crete),
        ],
    )
    .unwrap();
    g.sync_search_items(&sicily, "Sicily", &[(sicily.join("a.ARW"), shared)])
        .unwrap();
    // Sicily was imported: its originals are the dedupe record.
    g.add_original(&sicily, "Capture/a.ARW", &shared).unwrap();
    assert_eq!(
        g.search(M, &axis(0, 1, 0.0), RankOptions::default())
            .unwrap()
            .len(),
        3
    );

    assert!(g.forget_search(&crete).unwrap());
    let hits = g
        .search(M, &axis(0, 1, 0.0), RankOptions::default())
        .unwrap();
    assert!(
        hits.iter().all(|h| h.session_name == "Sicily"),
        "no offline Crete group"
    );
    assert_eq!(
        g.embedding_count(M).unwrap(),
        1,
        "Crete-only vector pruned, shared kept"
    );
    assert!(g.sessions().unwrap().iter().all(|s| s.root != crete));

    assert!(g.forget_search(&sicily).unwrap());
    assert_eq!(
        g.search(M, &axis(0, 1, 0.0), RankOptions::default())
            .unwrap(),
        [] as [global::GlobalSearchHit; 0]
    );
    assert_eq!(g.embedding_count(M).unwrap(), 0);
    assert!(
        g.contains(&shared).unwrap(),
        "import dedupe still knows the card's files"
    );
    assert!(!g.forget_search(&sicily).unwrap(), "already gone");
}

#[test]
fn photos_removed_from_a_session_leave_with_their_unshared_vectors() {
    let d = tempfile::tempdir().unwrap();
    let mut g = global(d.path());
    let (crete, sicily) = (PathBuf::from("/pics/Crete"), PathBuf::from("/pics/Sicily"));
    let (gone, shared, kept) = (FileHash([1; 32]), FileHash([2; 32]), FileHash([3; 32]));
    g.sync_search_items(
        &crete,
        "Crete",
        &[
            (crete.join("a.ARW"), gone),
            (crete.join("b.ARW"), shared),
            (crete.join("c.ARW"), kept),
        ],
    )
    .unwrap();
    g.sync_search_items(&sicily, "Sicily", &[(sicily.join("b.ARW"), shared)])
        .unwrap();
    g.store_embeddings(
        M,
        &[
            (gone, axis(0, 1, 0.0)),
            (shared, axis(1, 0, 0.0)),
            (kept, axis(2, 0, 0.0)),
        ],
    )
    .unwrap();
    // a.ARW and b.ARW were removed from Crete (trashed, forgotten as missing).
    assert!(
        g.sync_search_items(&crete, "Crete", &[(crete.join("c.ARW"), kept)])
            .unwrap()
    );
    assert_eq!(g.embedding_count(M).unwrap(), 2, "only a.ARW's vector went");
    assert_eq!(
        g.missing_embeddings(M, &[gone, shared, kept]).unwrap(),
        vec![gone]
    );
    let hits = g
        .search(M, &axis(0, 1, 0.0), RankOptions::default())
        .unwrap();
    assert!(hits.iter().all(|h| !h.path.ends_with("a.ARW")));
}

#[test]
fn a_batch_reads_only_its_originals_by_index() {
    let mut f = fx("Batch");
    let (_, _, ka) = f.add("a", 0);
    let (b, _, kb) = f.add("b", 1);
    let (_, _, kc) = f.add("c", 2);
    // b's sensor hash arrived: its key is that one, the file hash no longer counts.
    let sensor = FileHash([5; 32]);
    f.s.conn
        .execute(
            "UPDATE images SET content_hash = ? WHERE id = ?",
            rusqlite::params![&sensor.0[..], b.0],
        )
        .unwrap();
    f.s.store_embeddings(
        M,
        &[
            (ka, axis(0, 1, 0.0)),
            (sensor, axis(1, 0, 0.0)),
            (kc, axis(2, 0, 0.0)),
        ],
    )
    .unwrap();
    let got = f.s.embedded_originals(M, Some(&[ka, sensor, kb])).unwrap();
    assert_eq!(
        got,
        vec![
            (f.root.join("Capture/a.ARW"), ka),
            (f.root.join("Capture/b.ARW"), sensor)
        ]
    );
    assert_eq!(f.s.embedded_originals(M, None).unwrap().len(), 3);
    // Indexed lookups, never a scan of every original.
    let plan: Vec<String> =
        f.s.conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT i.rel_path FROM images i
             WHERE (i.content_hash = ?1 OR (i.hash = ?1 AND i.content_hash IS NULL))",
            )
            .unwrap()
            .query_map([&ka.0[..]], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    assert!(plan.iter().all(|p| !p.starts_with("SCAN")), "{plan:?}");
}
