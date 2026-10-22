//! People through the FFI: face storage, clusters, naming, suggestions across
//! sessions, rejections, merge/split, renames, and Find's sync.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_executor::block_on;

use super::super::{ImageMetaLite, OpenOptions, Probe, RawMetadata, open_in_place_with};
use super::*;

pub(crate) const FP: &str = "vision-fp2";

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    let bytes = fs::read(p).ok()?;
    Some((
        ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + bytes.len() as i64 * 1000),
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

/// A folder of fake RAWs opened in place and hashed.
pub(crate) fn open(folder: &Path, files: &[(&str, &[u8])]) -> Arc<SessionHandle> {
    fs::create_dir_all(folder).unwrap();
    for (n, b) in files {
        fs::write(folder.join(n), b).unwrap();
    }
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

/// A face of person `p` (an axis), jittered.
pub(crate) fn vec_of(p: usize, jitter: f32) -> Vec<f32> {
    let mut v = vec![0.05f32; 8];
    v[p] = 1.0;
    v[(p + 1) % 8] += jitter;
    v
}

pub(crate) fn detected(x: f32, p: usize, jitter: f32) -> DetectedFace {
    DetectedFace {
        rect: FaceRect {
            x,
            y: 0.2,
            w: 0.1,
            h: 0.15,
        },
        quality: 0.6,
        vector: vec_of(p, jitter),
    }
}

pub(crate) const OPTS: PeopleOptions = PeopleOptions {
    cluster_threshold: 0.6,
    suggest_threshold: 0.6,
};

fn tok() -> Arc<CancelToken> {
    CancelToken::new()
}

/// Jobs by file name → key.
pub(crate) fn keys(h: &Arc<SessionHandle>) -> std::collections::HashMap<String, String> {
    block_on(h.clone().face_jobs(FP.into(), tok()))
        .unwrap()
        .into_iter()
        .map(|j| {
            let name = Path::new(&j.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (name, j.key)
        })
        .collect()
}

pub(crate) fn store(h: &Arc<SessionHandle>, key: &str, faces: Vec<DetectedFace>) {
    block_on(h.clone().store_faces(FP.into(), key.into(), faces, tok())).unwrap();
}

fn overview(h: &Arc<SessionHandle>, people: &Path) -> PeopleOverview {
    block_on(
        h.clone()
            .people_overview(FP.into(), people.to_string_lossy().into(), OPTS, tok()),
    )
    .unwrap()
}

fn ids(c: &FaceCluster) -> Vec<FaceId> {
    c.faces.iter().map(|f| f.id.clone()).collect()
}

fn file(f: &FaceRecord) -> String {
    Path::new(&f.path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Session "Trip": a (Alex), b (Alex + Sam), c (Sam), d (nobody).
fn trip(dir: &Path) -> (Arc<SessionHandle>, PathBuf) {
    let folder = dir.join("Trip");
    let h = open(
        &folder,
        &[
            ("a.ARW", b"aa"),
            ("b.ARW", b"bbb"),
            ("c.ARW", b"cccc"),
            ("d.ARW", b"ddddd"),
        ],
    );
    let k = keys(&h);
    assert_eq!(k.len(), 4);
    store(&h, &k["a.ARW"], vec![detected(0.1, 0, 0.0)]);
    store(
        &h,
        &k["b.ARW"],
        vec![detected(0.1, 0, 0.1), detected(0.6, 1, 0.0)],
    );
    store(&h, &k["c.ARW"], vec![detected(0.3, 1, 0.05)]);
    store(&h, &k["d.ARW"], vec![]);
    (h, folder)
}

#[test]
fn clusters_then_names_then_suggestions_elsewhere() {
    let d = tempfile::tempdir().unwrap();
    let people = d.path().join("people.json");
    let (h, _) = trip(d.path());
    assert!(keys(&h).is_empty(), "all scanned");
    let st = block_on(h.clone().face_status(FP.into(), tok())).unwrap();
    assert_eq!((st.scanned, st.total, st.faces), (4, 4, 4));

    let o = overview(&h, &people);
    assert_eq!(o.people.len(), 0);
    assert_eq!(o.unnamed.len(), 2);
    assert_eq!(o.status.faces, 4);
    let alex_cluster = o
        .unnamed
        .iter()
        .find(|c| c.faces.iter().any(|f| file(f) == "a.ARW"))
        .unwrap();
    assert_eq!(alex_cluster.faces.len(), 2);
    assert!(alex_cluster.suggestion.is_none(), "nobody known yet");
    assert_eq!(alex_cluster.faces[0].rect.x, 0.1);
    assert!(alex_cluster.faces[0].variant_id > 0);

    // "Who is this?" → Alex.
    let alex = block_on(h.clone().name_faces(
        FP.into(),
        ids(alex_cluster),
        " Alex ".into(),
        people.to_string_lossy().into(),
        tok(),
    ))
    .unwrap();
    assert_eq!(alex.name, "Alex");
    let o = overview(&h, &people);
    assert_eq!(o.people.len(), 1);
    assert_eq!(o.people[0].name, "Alex");
    assert_eq!(o.people[0].person_id.as_deref(), Some(alex.id.as_str()));
    assert_eq!((o.people[0].faces.len(), o.people[0].photos), (2, 2));
    assert_eq!(o.people[0].variant_ids.len(), 2);
    assert_eq!(o.unnamed.len(), 1, "Sam is left");
    // Tags are in the sidecars (photo records carry them).
    let names: Vec<(String, Vec<String>)> = h
        .list(&LOCAL, &|| false)
        .unwrap()
        .into_iter()
        .map(|r| (r.file_name, r.people))
        .collect();
    assert!(names.contains(&("a.ARW".into(), vec!["Alex".into()])));
    assert!(names.contains(&("c.ARW".into(), vec![])));
    let all = known_people(people.to_string_lossy().into()).unwrap();
    assert_eq!(all.len(), 1);

    // Another session: a face like Alex's is suggested.
    let other = open(&d.path().join("Home"), &[("x.ARW", b"x")]);
    let kx = keys(&other)["x.ARW"].clone();
    store(&other, &kx, vec![detected(0.4, 0, 0.05)]);
    let o2 = overview(&other, &people);
    let s = o2.unnamed[0].suggestion.as_ref().unwrap();
    assert_eq!(
        (s.name.as_str(), s.person_id.as_str()),
        ("Alex", alex.id.as_str())
    );
    assert!(s.similarity > 0.9);
    // "No": rejected at that face, never suggested again.
    block_on(other.clone().reject_faces(
        FP.into(),
        ids(&o2.unnamed[0]),
        Some(alex.id.clone()),
        "Alex".into(),
        people.to_string_lossy().into(),
        tok(),
    ))
    .unwrap();
    let o2 = overview(&other, &people);
    assert!(o2.unnamed[0].suggestion.is_none());
    // A rename reaches the store (with the old name as an alias) and this session.
    assert!(
        block_on(rename_person(
            Some(h.clone()),
            people.to_string_lossy().into(),
            alex.id.clone(),
            "Alexander".into(),
            tok()
        ))
        .unwrap()
    );
    let o = overview(&h, &people);
    assert_eq!(o.people[0].name, "Alexander");
    let all = known_people(people.to_string_lossy().into()).unwrap();
    assert_eq!(all[0].aliases, ["Alex"]);
    assert!(
        !block_on(rename_person(
            None,
            people.to_string_lossy().into(),
            "nobody".into(),
            "X".into(),
            tok()
        ))
        .unwrap()
    );
    // Splitting a face off a named person = "not them".
    let face = o.people[0].faces[0].id.clone();
    block_on(h.clone().reject_faces(
        FP.into(),
        vec![face],
        Some(alex.id.clone()),
        "Alexander".into(),
        people.to_string_lossy().into(),
        tok(),
    ))
    .unwrap();
    let o = overview(&h, &people);
    assert_eq!(o.people[0].faces.len(), 1);
    assert_eq!(o.people[0].photos, 1);
}

#[test]
fn merge_and_split_unnamed_faces() {
    let d = tempfile::tempdir().unwrap();
    let people = d.path().join("people.json");
    let (h, _) = trip(d.path());
    let o = overview(&h, &people);
    assert_eq!(o.unnamed.len(), 2);
    // Merge the two people: one cluster.
    let all: Vec<FaceId> = o.unnamed.iter().flat_map(ids).collect();
    block_on(h.clone().merge_faces(all.clone(), tok())).unwrap();
    let o = overview(&h, &people);
    assert_eq!(o.unnamed.len(), 1);
    assert_eq!(o.unnamed[0].faces.len(), 4);
    // Split one face off: it stays apart even though it looks like the rest.
    let one = vec![all[0].clone()];
    let rest = all[1..].to_vec();
    block_on(h.clone().split_faces(one.clone(), rest, tok())).unwrap();
    let o = overview(&h, &people);
    assert!(
        o.unnamed.iter().any(|c| ids(c) == one),
        "{:?}",
        o.unnamed.len()
    );
    // Naming clears its links.
    block_on(h.clone().name_faces(
        FP.into(),
        one,
        "Pat".into(),
        people.to_string_lossy().into(),
        tok(),
    ))
    .unwrap();
    assert!(
        lock(&h.session)
            .face_links()
            .unwrap()
            .iter()
            .all(|(a, b, _)| {
                let first = face_ref(&all[0]).unwrap();
                *a != first && *b != first
            })
    );
}

#[test]
fn errors_and_edges() {
    let d = tempfile::tempdir().unwrap();
    let people = d.path().join("people.json");
    let (h, _) = trip(d.path());
    let o = overview(&h, &people);
    // A blank name is refused.
    let r = block_on(h.clone().name_faces(
        FP.into(),
        ids(&o.unnamed[0]),
        "  ".into(),
        people.to_string_lossy().into(),
        tok(),
    ));
    assert!(matches!(r, Err(DsError::InvalidArgument { .. })));
    // Bad keys are invalid; unknown faces are ignored.
    let bad = FaceId {
        key: "zz".into(),
        idx: 0,
    };
    assert!(block_on(h.clone().merge_faces(vec![bad.clone()], tok())).is_err());
    assert!(block_on(h.clone().split_faces(vec![bad.clone()], vec![], tok())).is_err());
    assert!(block_on(h.clone().split_faces(vec![], vec![bad], tok())).is_err());
    assert!(
        block_on(
            h.clone()
                .store_faces(FP.into(), "nothex".into(), vec![], tok())
        )
        .is_err()
    );
    let ghost = FaceId {
        key: "00".repeat(32),
        idx: 0,
    };
    block_on(h.clone().reject_faces(
        FP.into(),
        vec![ghost],
        None,
        "X".into(),
        people.to_string_lossy().into(),
        tok(),
    ))
    .unwrap();
    // A damaged people file is an error, not replaced.
    fs::write(&people, b"{").unwrap();
    assert!(
        block_on(h.clone().people_overview(
            FP.into(),
            people.to_string_lossy().into(),
            OPTS,
            tok()
        ))
        .is_err()
    );
    assert!(known_people(people.to_string_lossy().into()).is_err());
    assert_eq!(fs::read(&people).unwrap(), b"{");
    // Tags not in the store still show as a person.
    let a = lock(&h.session).images().unwrap()[0].id;
    lock(&h.session)
        .tag_person(
            &[(a, None)],
            &ds_session::PersonTag {
                name: "Imported".into(),
                id: None,
                region: None,
            },
        )
        .unwrap();
    fs::remove_file(&people).unwrap();
    let o = overview(&h, &people);
    let imported = o.people.iter().find(|p| p.name == "Imported").unwrap();
    assert!(imported.faces.is_empty(), "no face box");
    assert_eq!((imported.photos, imported.variant_ids.len()), (1, 1));
}
