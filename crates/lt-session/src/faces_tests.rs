//! Faces, people tags in sidecars, face links, and suggestions across sessions.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};

use lt_core::FileHash;
use lt_core::geom::{NormRect, Orientation};

use crate::*;

const FP: &str = "vision-fp2";

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    s: Session,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Iceland");
    let s = Session::create(&root, "Iceland").unwrap();
    Fx { _dir: dir, root, s }
}

impl Fx {
    fn add(&mut self, name: &str) -> (ImageId, VariantId, FileHash) {
        let rel = format!("Capture/{name}.ARW");
        fs::write(self.root.join(&rel), name.as_bytes()).unwrap();
        let h = FileHash(*blake3::hash(name.as_bytes()).as_bytes());
        let meta = ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000),
            width: 6000,
            height: 4000,
            orientation: Orientation::Normal,
            ..ImageMetaLite::default()
        };
        let id = self.s.add_original(&rel, h, meta).unwrap();
        (id, self.s.variants(id).unwrap()[0], h)
    }
}

fn rect(x: f32) -> NormRect {
    NormRect {
        x,
        y: 0.1,
        w: 0.2,
        h: 0.3,
    }
}

fn face(x: f32, v: &[f32]) -> NewFace {
    NewFace {
        rect: rect(x),
        quality: 0.5,
        vector: v.to_vec(),
    }
}

fn tag(name: &str, id: Option<&str>) -> PersonTag {
    PersonTag {
        name: name.into(),
        id: id.map(str::to_owned),
        region: None,
    }
}

#[test]
fn faces_are_stored_per_content_and_scanned_once() {
    let mut f = fx();
    let (_, _, ka) = f.add("a");
    let (_, _, kb) = f.add("b");
    let jobs = f.s.face_jobs(FP).unwrap();
    assert_eq!(jobs.iter().map(|j| j.key).collect::<Vec<_>>(), [ka, kb]);
    assert_eq!(jobs[0].path, f.root.join("Capture/a.ARW"));
    assert_eq!(
        f.s.face_counts(FP).unwrap(),
        FaceCounts {
            scanned: 0,
            total: 2,
            waiting: 0,
            faces: 0
        }
    );
    // Two faces on a (one unusable vector dropped), none on b.
    let n =
        f.s.store_faces(
            FP,
            &ka,
            &[
                face(0.1, &[1.0, 0.0]),
                face(0.5, &[0.0, 0.0]),
                face(0.7, &[0.0, 2.0]),
            ],
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(f.s.store_faces(FP, &kb, &[]).unwrap(), 0);
    assert!(
        f.s.face_jobs(FP).unwrap().is_empty(),
        "a photo without faces isn't rescanned"
    );
    assert_eq!(f.s.face_jobs("other-model").unwrap().len(), 2);
    let faces = f.s.faces(FP).unwrap();
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[1].at, FaceRef { key: ka, idx: 1 });
    assert_eq!(faces[1].rect, rect(0.7));
    assert!((faces[1].vector[1] - 1.0).abs() < 1e-3, "normalized");
    assert_eq!(faces[0].quality, 0.5);
    let c = f.s.face_counts(FP).unwrap();
    assert_eq!((c.scanned, c.faces), (2, 2));
    // A rescan replaces.
    f.s.store_faces(FP, &ka, &[face(0.3, &[1.0, 1.0])]).unwrap();
    assert_eq!(f.s.faces(FP).unwrap().len(), 1);
}

#[test]
fn trashed_and_unhashed_photos_have_no_faces_or_jobs() {
    let mut f = fx();
    let (_, va, ka) = f.add("a");
    f.s.store_faces(FP, &ka, &[face(0.1, &[1.0, 0.0])]).unwrap();
    f.s.move_to(&[va], Folder::Trash).unwrap();
    assert_eq!(f.s.faces(FP).unwrap(), [] as [faces::Face; 0]);
    assert_eq!(f.s.photo_people().unwrap(), [] as [faces::PhotoPeople; 0]);
    let rel = "Capture/u.ARW";
    fs::write(f.root.join(rel), b"u").unwrap();
    f.s.add_original(rel, UNHASHED, ImageMetaLite::default())
        .unwrap();
    assert_eq!(f.s.face_jobs(FP).unwrap(), [] as [faces::FaceJob; 0]);
    assert_eq!(f.s.face_counts(FP).unwrap().waiting, 1);
}

#[test]
fn tags_live_in_sidecars_on_every_variant() {
    let mut f = fx();
    let (a, va, ka) = f.add("a");
    let (b, _, _) = f.add("b");
    let second = f.s.new_variant(a).unwrap();
    let alex = tag("Alex", Some("p1"));
    f.s.tag_person(&[(a, Some(rect(0.1))), (b, None)], &alex)
        .unwrap();
    // Twice: no duplicate.
    f.s.tag_person(&[(a, Some(rect(0.11)))], &alex).unwrap();
    let pp = f.s.photo_people().unwrap();
    assert_eq!(pp.len(), 2);
    assert_eq!(pp[0].image, a);
    assert_eq!(pp[0].key, Some(ka));
    assert_eq!(pp[0].variants, [va, second]);
    assert_eq!(pp[0].tags.len(), 1);
    assert_eq!(pp[0].tags[0].region, Some(rect(0.1)));
    assert_eq!(pp[1].tags[0].region, None);
    // The sidecar is the source of truth: a rebuilt index has the tags.
    f.s.rebuild_index().unwrap();
    let s = &mut f.s;
    let pp = s.photo_people().unwrap();
    assert_eq!(
        pp[0].tags,
        [PersonTag {
            region: Some(rect(0.1)),
            ..alex.clone()
        }]
    );
    // Find sees the names, and the text filter does too.
    let recs = s.find_records().unwrap();
    assert_eq!(recs[0].people, ["Alex"]);
    let hits = s
        .query(
            &Filter {
                text: Some("alex".into()),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap();
    assert_eq!(hits.len(), 3, "both variants of a, and b");
    // Another person on the same face replaces Alex there.
    let sam = tag("Sam", Some("p2"));
    s.tag_person(&[(a, Some(rect(0.12)))], &sam).unwrap();
    let pp = s.photo_people().unwrap();
    assert_eq!(
        pp[0]
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["Sam"]
    );
    // Untag: only that face, or anywhere.
    s.untag_person(&[(a, Some(rect(0.6)))], &sam).unwrap();
    assert_eq!(s.photo_people().unwrap()[0].tags.len(), 1);
    s.untag_person(&[(a, None)], &sam).unwrap();
    assert_eq!(
        s.photo_people().unwrap()[0].tags,
        [] as [sidecar::PersonTag; 0]
    );
    // Unchanged: nothing written.
    s.untag_person(&[(a, None)], &sam).unwrap();
}

#[test]
fn rejections_and_renames() {
    let mut f = fx();
    let (a, _, _) = f.add("a");
    let alex = tag("Alex", Some("p1"));
    f.s.tag_person(&[(a, Some(rect(0.1)))], &alex).unwrap();
    f.s.reject_person(&[(a, Some(rect(0.1)))], &alex).unwrap();
    let pp = f.s.photo_people().unwrap();
    assert_eq!(pp[0].tags, [] as [sidecar::PersonTag; 0]);
    assert_eq!(pp[0].rejected.len(), 1);
    assert_eq!(f.s.find_records().unwrap()[0].rejected, ["Alex"]);
    // Rejecting again changes nothing; tagging again lifts the rejection.
    f.s.reject_person(&[(a, Some(rect(0.1)))], &alex).unwrap();
    assert_eq!(f.s.photo_people().unwrap()[0].rejected.len(), 1);
    f.s.tag_person(&[(a, Some(rect(0.1)))], &alex).unwrap();
    let pp = f.s.photo_people().unwrap();
    assert_eq!(pp[0].rejected, [] as [sidecar::PersonTag; 0]);
    assert_eq!(pp[0].tags.len(), 1);
    // Renames follow the id, also into rejections.
    f.s.reject_person(&[(a, Some(rect(0.5)))], &alex).unwrap();
    assert_eq!(f.s.rename_person("p1", "Alexander").unwrap(), 1);
    let pp = f.s.photo_people().unwrap();
    assert_eq!(pp[0].tags[0].name, "Alexander");
    assert_eq!(pp[0].rejected[0].name, "Alexander");
    assert_eq!(f.s.rename_person("p1", "Alexander").unwrap(), 0);
    // Tags without an id match by name, any case.
    let plain = tag("dana", None);
    f.s.tag_person(&[(a, None)], &plain).unwrap();
    f.s.untag_person(&[(a, None)], &tag("DANA", None)).unwrap();
    assert_eq!(f.s.photo_people().unwrap()[0].tags.len(), 1);
}

#[test]
fn regions_and_person_identity() {
    let a = tag("A", Some("1"));
    assert!(a.same_person(&tag("other name", Some("1"))));
    assert!(!a.same_person(&tag("A", Some("2"))));
    assert!(a.same_person(&tag("a", None)));
    let r = PersonTag {
        region: Some(rect(0.1)),
        ..a.clone()
    };
    assert!(r.same_region(Some(rect(0.12))));
    assert!(!r.same_region(Some(rect(0.5))));
    assert!(!r.same_region(None));
    assert!(a.same_region(None));
    assert!((iou(rect(0.1), rect(0.1)) - 1.0).abs() < 1e-6);
    assert_eq!(iou(rect(0.1), rect(0.6)), 0.0);
    let zero = NormRect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };
    assert_eq!(iou(zero, zero), 0.0);
}

#[test]
fn links_between_faces() {
    let mut f = fx();
    let (_, _, ka) = f.add("a");
    let (_, _, kb) = f.add("b");
    let fa = FaceRef { key: ka, idx: 0 };
    let fb = FaceRef { key: kb, idx: 0 };
    let fb1 = FaceRef { key: kb, idx: 1 };
    f.s.link_faces(&[(fb, fa), (fa, fa)], true).unwrap();
    f.s.link_faces(&[(fa, fb1)], false).unwrap();
    let mut links = f.s.face_links().unwrap();
    links.sort_by_key(|l| (l.0, l.1));
    assert_eq!(links.len(), 2);
    assert!(links.contains(&(fa.min(fb), fa.max(fb), true)));
    // Re-linking replaces.
    f.s.link_faces(&[(fa, fb)], false).unwrap();
    assert!(f.s.face_links().unwrap().iter().all(|l| !l.2));
    f.s.unlink_faces(&[fa]).unwrap();
    assert_eq!(
        f.s.face_links().unwrap(),
        [] as [(faces::FaceRef, faces::FaceRef, bool); 0]
    );
}

#[test]
fn face_links_survive_a_lost_or_recreated_index() {
    let mut f = fx();
    let (_, _, ka) = f.add("a");
    let (_, _, kb) = f.add("b");
    let fa = FaceRef { key: ka, idx: 0 };
    let fb = FaceRef { key: kb, idx: 1 };
    f.s.link_faces(&[(fa, fb)], true).unwrap();
    let before = f.s.face_links().unwrap();
    assert_eq!(before.len(), 1);
    drop(f.s);
    // The index is disposable: delete it (and its WAL files) outright.
    for e in fs::read_dir(&f.root).unwrap().flatten() {
        if e.file_name().to_string_lossy().contains(".ltsession") {
            fs::remove_file(e.path()).unwrap();
        }
    }
    let mut s = Session::open(&f.root).unwrap();
    assert_eq!(
        s.face_links().unwrap(),
        before,
        "restored from the settings file"
    );
    // Unlinking is remembered too, not resurrected by the file.
    s.unlink_faces(&[fa]).unwrap();
    drop(s);
    let s = Session::open(&f.root).unwrap();
    assert_eq!(
        s.face_links().unwrap(),
        [] as [(faces::FaceRef, faces::FaceRef, bool); 0]
    );
}

#[test]
fn existing_links_are_exported_when_there_is_no_file_yet() {
    let mut f = fx();
    let (_, _, ka) = f.add("a");
    let (_, _, kb) = f.add("b");
    let (fa, fb) = (FaceRef { key: ka, idx: 0 }, FaceRef { key: kb, idx: 0 });
    f.s.link_faces(&[(fa, fb)], false).unwrap();
    let file = f.root.join(".lt/settings/face_links.json");
    fs::remove_file(&file).unwrap(); // a session from before the file existed
    f.s.sync_index().unwrap();
    assert!(file.is_file());
    // A damaged file is reported, not fatal, and left for the user.
    fs::write(&file, b"{").unwrap();
    let report = f.s.sync_index().unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|(p, _)| p.ends_with("face_links.json"))
    );
    assert_eq!(fs::read(&file).unwrap(), b"{");
    assert_eq!(f.s.face_links().unwrap().len(), 1);
}

#[test]
fn suggestions_cross_sessions_through_the_global_index() {
    let d = tempfile::tempdir().unwrap();
    let mut g = GlobalIndex::open(&d.path().join("global.db")).unwrap();
    let k = |n: u8| FileHash([n; 32]);
    let rec = |p: &str, n: u8| FindRecord {
        path: PathBuf::from(p),
        key: Some(k(n)),
        ..FindRecord::default()
    };
    // An offline session: Alex's face on 1, a stranger on 2, a rejected Alex on 3.
    let items = [
        rec("/s/1.ARW", 1),
        rec("/s/2.ARW", 2),
        FindRecord {
            rejected: vec!["alex".into()],
            ..rec("/s/3.ARW", 3)
        },
        FindRecord {
            people: vec!["Alex".into()],
            ..rec("/s/4.ARW", 4)
        },
        FindRecord {
            key: None,
            ..rec("/s/5.ARW", 5)
        },
    ];
    g.sync_find_items(Path::new("/s"), "S", &items).unwrap();
    let faces = vec![
        (k(1), 0, vec![1.0, 0.1]),
        (k(2), 0, vec![0.0, 1.0]),
        (k(3), 0, vec![1.0, 0.0]),
        (k(4), 0, vec![1.0, 0.0]),
        (k(1), 0, vec![1.0, 0.1]),
    ];
    assert!(g.sync_faces(FP, &faces).unwrap());
    assert!(!g.sync_faces(FP, &faces).unwrap(), "unchanged");
    let alex = PersonCentroid::of("Alex", &[vec![1.0, 0.0], vec![1.0, 0.05]]).unwrap();
    assert_eq!(alex.count, 2);
    assert_eq!(
        g.refresh_suggestions(FP, std::slice::from_ref(&alex), 0.8)
            .unwrap(),
        1
    );
    assert_eq!(
        g.refresh_suggestions(FP, std::slice::from_ref(&alex), 0.8)
            .unwrap(),
        0,
        "idempotent"
    );
    let o = g
        .find(&FindSpec {
            people: vec![PersonQuery {
                term: "alex".into(),
                names: vec!["Alex".into()],
            }],
            model: "m".into(),
            ..FindSpec::default()
        })
        .unwrap();
    let names: Vec<String> = o
        .photos
        .iter()
        .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["4.ARW", "1.ARW"], "tagged first, then suggested");
    // Nobody known any more: suggestions go.
    assert_eq!(g.refresh_suggestions(FP, &[], 0.8).unwrap(), 1);
    // Faces of photos no session has are pruned when the session goes.
    g.unregister_session(Path::new("/s")).unwrap();
    assert!(
        g.sync_faces(FP, &faces[..1]).unwrap(),
        "pruned, so rewritten"
    );
    // Unusable people are never similar.
    let empty = PersonCentroid {
        name: "x".into(),
        sum: vec![0.0; 2],
        count: 0,
    };
    assert_eq!(empty.similarity(&[1.0, 0.0]), f32::NEG_INFINITY);
    assert_eq!(alex.similarity(&[1.0, 0.0, 0.0]), f32::NEG_INFINITY);
    assert!(PersonCentroid::of("x", &[vec![0.0, 0.0]]).is_none());
    let mixed = PersonCentroid::of("x", &[vec![1.0, 0.0], vec![1.0]]).unwrap();
    assert_eq!(mixed.count, 1);
}

#[test]
fn places_are_read_once_per_content() {
    let mut f = fx();
    let (_, _, ka) = f.add("a");
    let (_, _, kb) = f.add("b");
    let jobs = f.s.location_jobs().unwrap();
    assert_eq!(jobs.iter().map(|j| j.key).collect::<Vec<_>>(), [ka, kb]);
    let vik = PhotoPlace {
        names: vec!["Vik".into(), "Two\nLines".into()],
        gps: Some((63.4, -19.0)),
        stamp: "a.xmp:12:34".into(),
    };
    f.s.store_locations(&[(ka, vik.clone()), (kb, PhotoPlace::default())])
        .unwrap();
    assert_eq!(f.s.location_jobs().unwrap(), [] as [faces::FaceJob; 0]);
    let stored = f.s.location(&ka).unwrap().unwrap();
    assert_eq!(
        stored.names,
        ["Vik", "Two Lines"],
        "a newline can't split a name"
    );
    assert_eq!(stored.gps, vik.gps);
    let stamps = f.s.location_stamps().unwrap();
    assert_eq!(stamps.len(), 2);
    assert_eq!((stamps[0].0.key, stamps[0].1.as_str()), (ka, "a.xmp:12:34"));
    assert_eq!(stamps[1].1, "");
    assert_eq!(f.s.location(&kb).unwrap(), Some(PhotoPlace::default()));
    assert_eq!(f.s.location(&FileHash([9; 32])).unwrap(), None);
    let recs = f.s.find_records().unwrap();
    assert_eq!(recs[0].location, ["Vik", "Two Lines"]);
    assert_eq!(recs[1].location, [] as [std::string::String; 0]);
}
