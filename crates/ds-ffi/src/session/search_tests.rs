//! Search through the FFI: jobs and reuse across sessions, the global index,
//! renames, filters, find similar, keywords, the tokenizer.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_executor::block_on;

use super::super::{
    ColorLabel, ImageMetaLite, OpenOptions, Probe, RawMetadata, open_in_place_with,
};
use super::*;

const M: &str = "mobileclip-s2";

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    let bytes = fs::read(p).ok()?;
    Some((
        ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + bytes.len() as i64 * 1000),
            camera: Some("SONY ILCE-9".into()),
            width: 6000,
            height: 4000,
            ..ImageMetaLite::default()
        },
        RawMetadata::default(),
    ))
}

const LOCAL: Probe = Probe {
    is_supported: arw,
    read,
    volume_ok: |_| Ok(()),
    ranges: |_| None,
    untrusted_stat: |_| false,
    is_raw_name: arw,
};

/// A folder of fake RAWs opened in place and hashed (what the app does).
fn open(folder: &Path, files: &[(&str, &[u8])]) -> Arc<SessionHandle> {
    for (n, b) in files {
        fs::create_dir_all(folder).unwrap();
        fs::write(folder.join(n), b).unwrap();
    }
    reopen(folder)
}

fn reopen(folder: &Path) -> Arc<SessionHandle> {
    let h = Arc::new(
        open_in_place_with(
            folder,
            None,
            &OpenOptions::default(),
            None,
            &LOCAL,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap(),
    );
    h.hash_pending_with(&LOCAL, &|| false, &mut |_, _| {})
        .unwrap();
    h
}

/// A fake "image encoder": a unit vector along an axis picked by file name.
fn embed(path: &str) -> Vec<f32> {
    let name = Path::new(path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let mut v = vec![0.0f32; 16];
    let axis = ["waterfall", "sea", "car", "beach"]
        .iter()
        .position(|w| name.contains(w))
        .unwrap_or(15);
    v[axis] = 1.0;
    v[14] = 0.2; // something in common
    v
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 16];
    v[i] = 1.0;
    v
}

fn index(h: &SessionHandle, global: Option<&Path>) -> usize {
    let jobs = h.embedding_jobs_with(M, global).unwrap().jobs;
    let records: Vec<EmbeddingRecord> = jobs
        .iter()
        .map(|j| EmbeddingRecord {
            key: j.key.clone(),
            vector: embed(&j.path),
        })
        .collect();
    let r = h.store_embeddings_with(M, &records, global).unwrap();
    assert_eq!(r.global_error, None);
    assert_eq!(r.stored as usize, jobs.len());
    jobs.len()
}

fn opts() -> SearchOptions {
    SearchOptions {
        limit: 100,
        min_score: 0.5,
        window: 1.0,
    }
}

fn names(h: &SessionHandle, hits: &[SearchHitRecord]) -> Vec<String> {
    let list = h.list(&LOCAL, &|| false).unwrap();
    hits.iter()
        .map(|hit| {
            list.iter()
                .find(|r| r.variant_id == hit.variant_id)
                .unwrap()
                .file_name
                .clone()
        })
        .collect()
}

#[test]
fn content_keys_round_trip_as_hex() {
    let h = FileHash([0xab; 32]);
    let s = hex(&h);
    assert_eq!(s.len(), 64);
    assert_eq!(unhex(&s).unwrap(), h);
    for bad in ["", "ab", &"zz".repeat(32), &"é".repeat(32)] {
        assert!(
            matches!(unhex(bad), Err(DsError::InvalidArgument { .. })),
            "{bad}"
        );
    }
}

#[test]
fn index_search_and_reuse_vectors_across_sessions() {
    let t = tempfile::tempdir().unwrap();
    let global = t.path().join("support/global.db");
    let crete = open(
        &t.path().join("Crete"),
        &[
            ("Waterfall-1.ARW", b"w1"),
            ("Waterfall-2.ARW", b"w22"),
            ("Sea.ARW", b"sea-1"),
            ("Car.ARW", b"car-12"),
        ],
    );
    let status = block_on(crete.clone().embedding_status(M.into(), CancelToken::new())).unwrap();
    assert_eq!(
        status,
        EmbeddingStatus {
            embedded: 0,
            total: 4,
            waiting: 0
        }
    );
    assert_eq!(index(&crete, Some(&global)), 4);
    assert_eq!(index(&crete, Some(&global)), 0, "nothing left to embed");
    let status = block_on(crete.clone().embedding_status(M.into(), CancelToken::new())).unwrap();
    assert_eq!(status.embedded, 4);

    // "waterfall" ranks the waterfall shots first, and only those pass the floor.
    let hits = block_on(crete.clone().search_embedding(
        M.into(),
        axis(0),
        None,
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    let mut found = names(&crete, &hits);
    found.sort();
    assert_eq!(found, ["Waterfall-1.ARW", "Waterfall-2.ARW"]);

    // A second session holding a copy of a waterfall shot reuses its vector.
    let sicily = open(
        &t.path().join("Sicily"),
        &[
            ("Copy of Waterfall.ARW", b"w1"),
            ("Beach.ARW", b"beach-123"),
        ],
    );
    let jobs = sicily.embedding_jobs_with(M, Some(&global)).unwrap().jobs;
    assert_eq!(jobs.len(), 1, "only the beach: {jobs:?}");
    assert!(jobs[0].path.ends_with("Beach.ARW"));
    assert_eq!(index(&sicily, Some(&global)), 1);

    // Search all sessions: grouped by session, best session first.
    let groups = block_on(search_all_sessions(
        global.to_string_lossy().into_owned(),
        M.into(),
        axis(0),
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(groups.len(), 2, "{groups:?}");
    let crete_group = groups.iter().find(|g| g.name == "Crete").unwrap();
    assert_eq!(crete_group.hits.len(), 2);
    assert!(crete_group.available);
    assert!(
        Path::new(&crete_group.root).ends_with("Crete"),
        "the folder to open"
    );
    let sicily_group = groups.iter().find(|g| g.name == "Sicily").unwrap();
    assert_eq!(sicily_group.hits.len(), 1);
    assert!(sicily_group.hits[0].path.ends_with("Copy of Waterfall.ARW"));
    assert!(groups[0].best_score >= groups[1].best_score);
    let beach = block_on(search_all_sessions(
        global.to_string_lossy().into_owned(),
        M.into(),
        axis(3),
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(beach.len(), 1);
    assert_eq!(beach[0].name, "Sicily");

    // Without the global index, the session alone still works.
    let solo = open(&t.path().join("Solo"), &[("Car Show.ARW", b"car-show")]);
    assert_eq!(index(&solo, None), 1);
    let cars = block_on(solo.clone().search_embedding(
        M.into(),
        axis(2),
        None,
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(names(&solo, &cars), ["Car Show.ARW"]);
}

#[test]
fn a_renamed_file_is_not_embedded_again_and_its_new_path_is_searchable() {
    let t = tempfile::tempdir().unwrap();
    let global = t.path().join("global.db");
    let folder = t.path().join("Shoot");
    let h = open(&folder, &[("DSC0001.ARW", b"waterfall-bytes")]);
    let jobs = h.embedding_jobs_with(M, Some(&global)).unwrap().jobs;
    assert_eq!(jobs.len(), 1);
    h.store_embeddings_with(
        M,
        &[EmbeddingRecord {
            key: jobs[0].key.clone(),
            vector: axis(0),
        }],
        Some(&global),
    )
    .unwrap();
    drop(h);
    fs::rename(folder.join("DSC0001.ARW"), folder.join("Waterfall.ARW")).unwrap();
    let h = reopen(&folder);
    assert!(
        h.embedding_jobs_with(M, Some(&global))
            .unwrap()
            .jobs
            .is_empty(),
        "same content"
    );
    let hits =
        block_on(
            h.clone()
                .search_embedding(M.into(), axis(0), None, opts(), CancelToken::new()),
        )
        .unwrap();
    assert_eq!(names(&h, &hits), ["Waterfall.ARW"]);
    let groups = block_on(search_all_sessions(
        global.to_string_lossy().into_owned(),
        M.into(),
        axis(0),
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(groups.len(), 1);
    assert!(
        groups[0].hits[0].path.ends_with("Waterfall.ARW"),
        "{groups:?}"
    );
}

#[test]
fn search_combines_with_the_browser_filters() {
    let t = tempfile::tempdir().unwrap();
    let h = open(
        &t.path().join("Filters"),
        &[
            ("Waterfall-a.ARW", b"a"),
            ("Waterfall-bb.ARW", b"bb"),
            ("Waterfall-ccc.ARW", b"ccc"),
        ],
    );
    index(&h, None);
    let list = h.list(&LOCAL, &|| false).unwrap();
    let id = |n: &str| list.iter().find(|r| r.file_name == n).unwrap().variant_id;
    block_on(
        h.clone()
            .set_rating(vec![id("Waterfall-bb.ARW")], 4, CancelToken::new()),
    )
    .unwrap();
    block_on(h.clone().set_color_tag(
        vec![id("Waterfall-ccc.ARW")],
        ColorLabel::Red,
        CancelToken::new(),
    ))
    .unwrap();
    let search = |f: Option<SmartFilterRecord>| {
        block_on(
            h.clone()
                .search_embedding(M.into(), axis(0), f, opts(), CancelToken::new()),
        )
    };
    assert_eq!(search(None).unwrap().len(), 3);
    let rated = SmartFilterRecord {
        min_rating: Some(3),
        ..SmartFilterRecord::default()
    };
    assert_eq!(
        names(&h, &search(Some(rated)).unwrap()),
        ["Waterfall-bb.ARW"]
    );
    let red = SmartFilterRecord {
        tags: vec![ColorLabel::Red],
        ..SmartFilterRecord::default()
    };
    assert_eq!(
        names(&h, &search(Some(red)).unwrap()),
        ["Waterfall-ccc.ARW"]
    );
    // Capture dates: the fake metadata dates each file by its size.
    let dated = SmartFilterRecord {
        date_from_ms: Some(1_700_000_002_000),
        date_to_ms: Some(1_700_000_002_000),
        ..SmartFilterRecord::default()
    };
    assert_eq!(
        names(&h, &search(Some(dated)).unwrap()),
        ["Waterfall-bb.ARW"]
    );
    let bad = SmartFilterRecord {
        min_rating: Some(9),
        ..SmartFilterRecord::default()
    };
    assert!(matches!(
        search(Some(bad)),
        Err(DsError::InvalidArgument { .. })
    ));
}

#[test]
fn find_similar_and_keyword_suggestions() {
    let t = tempfile::tempdir().unwrap();
    let h = open(
        &t.path().join("Similar"),
        &[
            ("Sea-1.ARW", b"1"),
            ("Sea-2.ARW", b"22"),
            ("Car.ARW", b"333"),
            ("Unindexed.ARW", b"4444"),
        ],
    );
    index(&h, None);
    let list = h.list(&LOCAL, &|| false).unwrap();
    let id = |n: &str| list.iter().find(|r| r.file_name == n).unwrap().variant_id;
    let similar = block_on(h.clone().find_similar(
        M.into(),
        id("Sea-1.ARW"),
        None,
        opts(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(
        names(&h, &similar),
        ["Sea-2.ARW"],
        "itself excluded; the car is too far"
    );

    let labels: Vec<String> = ["waterfall", "sea", "car"].map(String::from).to_vec();
    let vectors: Vec<f32> = [axis(0), axis(1), axis(2)].concat();
    let vocabulary = KeywordVocabulary {
        labels,
        label_vectors: vectors,
        top: 2,
        min_probability: 0.05,
    };
    let kw = block_on(h.clone().suggest_keywords(
        M.into(),
        id("Car.ARW"),
        vocabulary.clone(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(kw.len(), 1, "{kw:?}");
    assert_eq!(kw[0].keyword, "car");
    assert!(kw[0].probability > 0.99);
    // Nothing else looks like the odd one out (the fake encoder's "other" axis).
    let none = block_on(h.clone().find_similar(
        M.into(),
        id("Unindexed.ARW"),
        None,
        SearchOptions {
            limit: 10,
            min_score: 0.9,
            window: 1.0,
        },
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(none.len(), 0);
    assert!(
        block_on(
            h.clone()
                .find_similar(M.into(), 424_242, None, opts(), CancelToken::new())
        )
        .is_err()
    );
    let no_model = block_on(h.clone().suggest_keywords(
        "other".into(),
        id("Car.ARW"),
        vocabulary,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(no_model.len(), 0);
}

#[test]
fn zero_shot_keywords_validate_their_inputs() {
    let image = embedding::normalized(&[1.0, 0.1]).unwrap();
    let labels = vec!["a".to_owned(), "b".to_owned()];
    let vocab = |labels: &[String], label_vectors: &[f32], top: u32| KeywordVocabulary {
        labels: labels.to_vec(),
        label_vectors: label_vectors.to_vec(),
        top,
        min_probability: 0.0,
    };
    let r = zero_shot_keywords(&image, &vocab(&labels, &[1.0, 0.0, 0.0, 1.0], 5)).unwrap();
    assert_eq!(
        r.iter().map(|k| k.keyword.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!((r.iter().map(|k| k.probability).sum::<f32>() - 1.0).abs() < 1e-5);
    // A zero label vector can't be normalized: it scores nothing but doesn't fail.
    let z = zero_shot_keywords(&image, &vocab(&labels, &[1.0, 0.0, 0.0, 0.0], 1)).unwrap();
    assert_eq!(z[0].keyword, "a");
    assert_eq!(
        zero_shot_keywords(&image, &vocab(&[], &[], 5))
            .unwrap()
            .len(),
        0
    );
    assert!(matches!(
        zero_shot_keywords(&image, &vocab(&labels, &[1.0], 5)),
        Err(DsError::InvalidArgument { .. })
    ));
    assert!(zero_shot_keywords(&[], &vocab(&labels, &[], 5)).is_err());
}

#[test]
fn grouping_marks_offline_sessions_and_keeps_hit_order() {
    let hit = |root: &str, path: &str, score: f32| GlobalSearchHit {
        session_root: root.into(),
        session_name: root.trim_start_matches('/').into(),
        path: path.into(),
        score,
    };
    let groups = group_by_session(
        vec![
            hit("/b", "/b/1", 0.9),
            hit("/a", "/a/1", 0.8),
            hit("/b", "/b/2", 0.7),
        ],
        |p| p == Path::new("/a"),
    );
    assert_eq!(groups.len(), 2);
    assert_eq!((groups[0].name.as_str(), groups[0].available), ("b", false));
    assert_eq!(groups[0].best_score, 0.9);
    assert_eq!(
        groups[0]
            .hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<Vec<_>>(),
        ["/b/1", "/b/2"]
    );
    assert!(groups[1].available);
    assert_eq!(group_by_session(Vec::new(), |_| true).len(), 0);
}

#[test]
fn global_index_failures_dont_fail_the_session() {
    let t = tempfile::tempdir().unwrap();
    let h = open(&t.path().join("Shoot"), &[("Sea.ARW", b"s")]);
    // A global index path that can't be a database (a directory is in the way).
    let blocked = t.path().join("blocked.db");
    fs::create_dir_all(&blocked).unwrap();
    // Jobs still come (this session indexes), with the global failure reported.
    let soft = h.embedding_jobs_with(M, Some(&blocked)).unwrap();
    assert_eq!(soft.jobs.len(), 1);
    assert!(soft.global_error.is_some(), "jobs report it");
    let jobs = h.embedding_jobs_with(M, None).unwrap();
    assert!(jobs.global_error.is_none());
    let jobs = jobs.jobs;
    let r = h
        .store_embeddings_with(
            M,
            &[EmbeddingRecord {
                key: jobs[0].key.clone(),
                vector: axis(1),
            }],
            Some(&blocked),
        )
        .unwrap();
    assert_eq!(r.stored, 1);
    assert!(r.global_error.is_some());
    // Bad keys are refused before anything is written.
    let bad = h.store_embeddings_with(
        M,
        &[EmbeddingRecord {
            key: "nope".into(),
            vector: axis(1),
        }],
        None,
    );
    assert!(matches!(bad, Err(DsError::InvalidArgument { .. })));
    assert!(
        block_on(search_all_sessions(
            blocked.to_string_lossy().into_owned(),
            M.into(),
            axis(1),
            opts(),
            CancelToken::new()
        ))
        .is_err()
    );
}

#[test]
fn async_store_and_jobs_and_activity_gate() {
    let t = tempfile::tempdir().unwrap();
    let global = t.path().join("g.db");
    let g = Some(global.to_string_lossy().into_owned());
    let h = open(&t.path().join("Async"), &[("Car.ARW", b"c")]);
    let jobs = block_on(
        h.clone()
            .embedding_jobs(M.into(), g.clone(), CancelToken::new()),
    )
    .unwrap()
    .jobs;
    assert_eq!(jobs.len(), 1);
    let r = block_on(h.clone().store_embeddings(
        M.into(),
        vec![EmbeddingRecord {
            key: jobs[0].key.clone(),
            vector: axis(2),
        }],
        g,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(r.stored, 1);
    crate::background::note_user_activity().unwrap();
    assert!(user_is_active().unwrap());
}

#[test]
fn tokenizer_loads_merges_and_reports_bad_files() {
    let t = tempfile::tempdir().unwrap();
    let missing =
        ClipTokenizer::from_merges_file(t.path().join("nope.txt").to_string_lossy().into());
    assert!(matches!(missing, Err(DsError::Io { .. })));
    let junk = t.path().join("junk.txt");
    fs::write(&junk, "#version: 0.2\na b\n").unwrap();
    assert!(matches!(
        ClipTokenizer::from_merges_file(junk.to_string_lossy().into()),
        Err(DsError::InvalidArgument { .. })
    ));
    // With the real file (downloaded with the model), a query becomes 77 ids.
    let Ok(real) = std::env::var("DS_CLIP_MERGES") else {
        eprintln!("DS_CLIP_MERGES not set: skipping the real-merges check");
        return;
    };
    let tok = ClipTokenizer::from_merges_file(real).unwrap();
    let ids = tok.encode("kids on the beach at sunset".into()).unwrap();
    assert_eq!(ids.len(), 77);
    assert_eq!(ids[0], 49_406);
    assert!(ids.contains(&49_407));
}

#[test]
fn forgetting_a_session_removes_it_from_all_sessions_search() {
    let t = tempfile::tempdir().unwrap();
    let global = t.path().join("global.db");
    let g = global.to_string_lossy().into_owned();
    let folder = t.path().join("Gone");
    let h = open(&folder, &[("Waterfall.ARW", b"w")]);
    index(&h, Some(&global));
    let found = |g: &str| {
        block_on(search_all_sessions(
            g.into(),
            M.into(),
            axis(0),
            opts(),
            CancelToken::new(),
        ))
        .unwrap()
    };
    assert_eq!(found(&g).len(), 1);
    // The app passes the folder as it knows it (possibly not canonical).
    let as_known = folder
        .join("..")
        .join("Gone")
        .to_string_lossy()
        .into_owned();
    assert!(
        block_on(forget_session_in_search(
            g.clone(),
            as_known,
            CancelToken::new()
        ))
        .unwrap()
    );
    assert!(found(&g).is_empty(), "no offline group left behind");
    assert!(
        !block_on(forget_session_in_search(
            g.clone(),
            "/nowhere".into(),
            CancelToken::new()
        ))
        .unwrap()
    );
}

#[test]
fn forgetting_a_vanished_folder_matches_its_recorded_spelling() {
    let t = tempfile::tempdir().unwrap();
    let global = t.path().join("global.db");
    let g = global.to_string_lossy().into_owned();
    let folder = t.path().join("Gone");
    let h = open(&folder, &[("Waterfall.ARW", b"w")]);
    index(&h, Some(&global));
    drop(h);
    let found = |g: &str| {
        block_on(search_all_sessions(
            g.into(),
            M.into(),
            axis(0),
            opts(),
            CancelToken::new(),
        ))
        .unwrap()
    };
    let recorded = PathBuf::from(&found(&g)[0].root);
    // The folder was deleted (or its volume is gone): no canonical path, and the
    // app knows it by another spelling than the one recorded.
    fs::remove_dir_all(&folder).unwrap();
    let other = recorded.join("x").join("..");
    assert!(
        block_on(forget_session_in_search(
            g.clone(),
            other.to_string_lossy().into_owned(),
            CancelToken::new()
        ))
        .unwrap()
    );
    assert!(found(&g).is_empty(), "no offline group left behind");
}

#[test]
fn same_folder_key_folds_dots_and_private() {
    assert_eq!(
        same_folder_key(Path::new("/private/var/folders/x/../y/./S")),
        PathBuf::from("/var/folders/y/S")
    );
    assert_eq!(
        same_folder_key(Path::new("/Volumes/pics/S")),
        PathBuf::from("/Volumes/pics/S")
    );
}
