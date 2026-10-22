//! Find through the FFI: sync, prompts, people (tagged, suggested, unknown,
//! renamed), places (session name, keyword, looks like), dates, ranking.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::path::Path;
use std::sync::Arc;

use futures_executor::block_on;

use super::super::people::tests::{FP, detected, keys, open, store};
use super::super::people::{FaceId, rename_person};
use super::super::search::EmbeddingRecord;
use super::*;

const M: &str = "mobileclip-s2";
const TODAY: FindDay = FindDay {
    year: 2026,
    month: 10,
    day: 4,
};
const OPTIONS: FindOptionsRecord = FindOptionsRecord {
    limit: 100,
    min_score: 0.18,
    window: 0.09,
    place_weight: 0.5,
};

fn tok() -> Arc<CancelToken> {
    CancelToken::new()
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 8];
    v[i] = 1.0;
    v
}

struct Lib {
    _dir: tempfile::TempDir,
    global: String,
    people: String,
    iceland: Arc<SessionHandle>,
    home: Arc<SessionHandle>,
}

impl Lib {
    fn sync(&self, h: &Arc<SessionHandle>) -> bool {
        block_on(h.clone().sync_find(
            self.global.clone(),
            FP.into(),
            self.people.clone(),
            0.6,
            tok(),
        ))
        .unwrap()
    }

    fn prompts(&self, text: &str) -> Vec<String> {
        let p = block_on(find_prompts(
            text.into(),
            self.global.clone(),
            self.people.clone(),
            TODAY,
            M.into(),
            tok(),
        ))
        .unwrap();
        assert_ne!(p.text.as_deref(), Some(""), "no empty description");
        p.text.into_iter().chain(p.places).collect()
    }

    fn find(
        &self,
        text: &str,
        vector: Option<Vec<f32>>,
        places: Vec<FindPlaceVector>,
    ) -> FindResults {
        block_on(find_photos(
            text.into(),
            self.global.clone(),
            self.people.clone(),
            TODAY,
            M.into(),
            vector,
            places,
            OPTIONS,
            tok(),
        ))
        .unwrap()
    }
}

fn names(r: &FindResults) -> Vec<String> {
    r.photos
        .iter()
        .map(|p| {
            Path::new(&p.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// "2024-07 Iceland": a (Alex, named), b (a face like Alex's), c (a
/// waterfall, keyword "Vestra Horn"); "Home": h (Alex, named there too).
fn library() -> Lib {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.db").to_string_lossy().into_owned();
    let people = dir
        .path()
        .join("people.json")
        .to_string_lossy()
        .into_owned();
    let iceland = open(
        &dir.path().join("2024-07 Iceland"),
        &[("a.ARW", &b"a"[..]), ("b.ARW", b"bb"), ("c.ARW", b"ccc")],
    );
    let home = open(&dir.path().join("Home"), &[("h.ARW", b"hhhh")]);
    let kh = keys(&iceland);
    let kk = keys(&home);
    store(&iceland, &kh["a.ARW"], vec![detected(0.1, 0, 0.0)]);
    store(&iceland, &kh["b.ARW"], vec![detected(0.5, 0, 0.1)]);
    store(&iceland, &kh["c.ARW"], vec![]);
    store(&home, &kk["h.ARW"], vec![detected(0.2, 0, 0.05)]);
    let face = |k: &str| FaceId {
        key: k.into(),
        idx: 0,
    };
    block_on(iceland.clone().name_faces(
        FP.into(),
        vec![face(&kh["a.ARW"])],
        "Alex".into(),
        people.clone(),
        tok(),
    ))
    .unwrap();
    block_on(home.clone().name_faces(
        FP.into(),
        vec![face(&kk["h.ARW"])],
        "Alex".into(),
        people.clone(),
        tok(),
    ))
    .unwrap();
    let c = lock(&iceland.session).images().unwrap();
    let cv = lock(&iceland.session).variants(c[2].id).unwrap();
    lock(&iceland.session)
        .set_keywords(&cv, &["Vestra Horn".into()])
        .unwrap();
    let g = Path::new(&global);
    for (h, ks) in [(&iceland, &kh), (&home, &kk)] {
        let records: Vec<EmbeddingRecord> = ks
            .iter()
            .map(|(name, key)| EmbeddingRecord {
                key: key.clone(),
                vector: match name.as_str() {
                    "a.ARW" | "h.ARW" => axis(0),
                    "b.ARW" => axis(1),
                    _ => axis(2),
                },
            })
            .collect();
        h.store_embeddings_with(M, &records, Some(g)).unwrap();
    }
    let lib = Lib {
        _dir: dir,
        global,
        people,
        iceland,
        home,
    };
    assert!(lib.sync(&lib.iceland));
    assert!(lib.sync(&lib.home));
    assert!(!lib.sync(&lib.home), "unchanged: nothing written");
    lib
}

#[test]
fn alex_in_iceland_across_sessions() {
    let lib = library();
    assert_eq!(
        lib.prompts("let's find pics of alex in iceland"),
        ["iceland"]
    );
    assert_eq!(lib.prompts("waterfall in sicily"), ["waterfall", "sicily"]);
    let r = lib.find("let's find pics of alex in iceland", None, vec![]);
    assert_eq!(
        r.query.people,
        [FindPersonRecord {
            term: "alex".into(),
            names: vec!["Alex".into()]
        }]
    );
    // a is tagged, b only looks like Alex (suggested); h is Alex at home.
    assert_eq!(names(&r), ["a.ARW", "b.ARW"]);
    assert_eq!(
        r.photos[1].reasons[0],
        MatchReasonRecord::Person {
            name: "Alex".into(),
            suggested: true
        }
    );
    assert_eq!(
        r.photos[0].reasons[1],
        MatchReasonRecord::Place {
            term: "iceland".into(),
            source: PlaceSourceRecord::SessionName {
                name: "2024-07 Iceland".into()
            }
        }
    );
    assert_eq!(r.photos[0].session_name, "2024-07 Iceland");
    assert!(r.photos[0].capture_time_ms.is_some());
    assert_eq!(r.sessions[0].name, "2024-07 Iceland");
    assert!(r.sessions[0].available);
    assert_eq!(
        r.sessions[0].reason,
        SessionReasonRecord::Name {
            term: "iceland".into()
        }
    );
    assert_eq!(r.sessions[0].hits, 2);
    assert!(
        r.query.places[0]
            .sources
            .contains(&PlaceSourceRecord::SessionName {
                name: "2024-07 Iceland".into()
            })
    );
    // Alex anywhere: three photos, tagged ones first.
    let r = lib.find("alex", None, vec![]);
    assert_eq!(r.total, 3);
    assert_eq!(names(&r)[2], "b.ARW");
}

#[test]
fn unknown_people_places_by_keyword_and_looks_like() {
    let lib = library();
    let r = lib.find("pics of Dana", None, vec![]);
    assert_eq!(r.query.people[0].names, Vec::<String>::new());
    assert_eq!(r.total, 4, "an unknown name filters nothing");
    let r = lib.find("photos at vestra horn", None, vec![]);
    assert_eq!(names(&r), ["c.ARW"]);
    assert_eq!(
        r.sessions[0].reason,
        SessionReasonRecord::Keyword {
            term: "vestra horn".into(),
            keyword: "Vestra Horn".into()
        }
    );
    // The description ranks; a place nobody named looks like itself.
    let r = lib.find("waterfall", Some(axis(2)), vec![]);
    assert_eq!(names(&r), ["c.ARW"]);
    assert!(matches!(r.photos[0].reasons[0], MatchReasonRecord::Visual { score } if score > 0.99));
    let r = lib.find(
        "in sicily",
        None,
        vec![FindPlaceVector {
            term: "sicily".into(),
            vector: axis(1),
        }],
    );
    assert_eq!(names(&r), ["b.ARW"]);
    assert_eq!(r.query.places[0].sources, [PlaceSourceRecord::LooksLike]);
    assert_eq!(
        r.photos[0].reasons[0],
        MatchReasonRecord::Place {
            term: "sicily".into(),
            source: PlaceSourceRecord::LooksLike
        }
    );
}

#[test]
fn dates_ratings_flags_and_renames() {
    let lib = library();
    let r = lib.find("alex in 2023", None, vec![]);
    assert_eq!(r.total, 3);
    let d = r.query.date.as_ref().unwrap();
    assert_eq!(d.phrase, "in 2023");
    assert!(d.from_ms.is_some() && d.to_ms.is_some());
    assert_eq!(r.photos[0].reasons.last(), Some(&MatchReasonRecord::Date));
    assert_eq!(lib.find("alex last summer", None, vec![]).total, 0);
    // Ratings and flags.
    let a = lock(&lib.iceland.session).images().unwrap()[0].id;
    let va = lock(&lib.iceland.session).variants(a).unwrap();
    lock(&lib.iceland.session)
        .set_rating(&va, ds_core::Rating::new(5).unwrap())
        .unwrap();
    lock(&lib.iceland.session)
        .set_pick(&va, PickState::Picked)
        .unwrap();
    assert!(lib.sync(&lib.iceland));
    let r = lib.find("5 star picks", None, vec![]);
    assert_eq!(names(&r), ["a.ARW"]);
    assert_eq!((r.query.min_rating, r.query.max_rating), (Some(5), Some(5)));
    assert_eq!(r.query.flag, Some(PickFlag::Picked));
    assert!(
        r.photos[0]
            .reasons
            .contains(&MatchReasonRecord::Rating { stars: 5 })
    );
    assert!(r.photos[0].reasons.contains(&MatchReasonRecord::Flag {
        pick: PickFlag::Picked
    }));
    // Renamed in Iceland; Home's photo still says "Alex" until it's synced again.
    let id = known_people_id(&lib.people);
    assert!(
        block_on(rename_person(
            Some(lib.iceland.clone()),
            lib.people.clone(),
            id,
            "Alexander".into(),
            tok()
        ))
        .unwrap()
    );
    assert!(lib.sync(&lib.iceland));
    let r = lib.find("alexander", None, vec![]);
    assert_eq!(
        r.total,
        3,
        "the alias still finds home's photo: {:?}",
        names(&r)
    );
    assert_eq!(
        r.query.people[0].names,
        ["Alexander"],
        "one person, by its name now"
    );
    // Suggestions follow the store: b's face is Alexander's now.
    let r = lib.find("alex", None, vec![]);
    assert!(
        r.photos
            .iter()
            .any(|p| p.reasons.contains(&MatchReasonRecord::Person {
                name: "Alexander".into(),
                suggested: true
            }))
    );
}

#[test]
fn xmp_locations_are_scanned_and_found() {
    let lib = library();
    let folder = lib.iceland.search_root(lock(&lib.iceland.session).root());
    std::fs::write(
        folder.join("c.xmp"),
        r#"<x:xmpmeta><rdf:Description photoshop:City="Vik" photoshop:State="Sudurland"/></x:xmpmeta>"#,
    )
    .unwrap();
    // Two at a time: one left after the first pass.
    let left = block_on(lib.iceland.clone().scan_locations(2, tok())).unwrap();
    assert_eq!(left, 1);
    assert_eq!(
        block_on(lib.iceland.clone().scan_locations(10, tok())).unwrap(),
        0
    );
    assert_eq!(
        block_on(lib.iceland.clone().scan_locations(10, tok())).unwrap(),
        0,
        "read once"
    );
    assert!(lib.sync(&lib.iceland));
    let r = lib.find("photos in vik", None, vec![]);
    assert_eq!(names(&r), ["c.ARW"]);
    assert_eq!(
        r.photos[0].reasons[0],
        MatchReasonRecord::Place {
            term: "vik".into(),
            source: PlaceSourceRecord::Location {
                value: "Vik".into()
            }
        }
    );
    // Known places are recognized without "in".
    assert_eq!(lib.prompts("vik sunset"), ["sunset", "vik"]);
    // A sidecar written later (Lightroom) is read on the next scan.
    std::fs::write(
        folder.join("a.ARW.xmp"),
        r#"<x:xmpmeta><rdf:Description photoshop:City="Selfoss"/></x:xmpmeta>"#,
    )
    .unwrap();
    assert_eq!(
        block_on(lib.iceland.clone().scan_locations(1, tok())).unwrap(),
        0
    );
    assert!(lib.sync(&lib.iceland));
    assert_eq!(names(&lib.find("selfoss", None, vec![])), ["a.ARW"]);
}

fn known_people_id(store: &str) -> String {
    super::super::people::known_people(store.into()).unwrap()[0]
        .id
        .clone()
}

/// A dated trip folder (C1 calls the session "Iceland"): "iceland",
/// "crete" (nothing called that) and the whole title.
#[test]
fn a_dated_trip_folder_is_a_place() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.db").to_string_lossy().into_owned();
    let people = dir
        .path()
        .join("people.json")
        .to_string_lossy()
        .into_owned();
    let trip = open(
        &dir.path().join("2026-08-06 Iceland Family Vacation"),
        &[("DSC1.ARW", &b"1"[..]), ("DSC2.ARW", b"22")],
    );
    let home = open(&dir.path().join("Home"), &[("h.ARW", &b"333"[..])]);
    let lib = Lib {
        _dir: dir,
        global,
        people,
        iceland: trip,
        home,
    };
    lib.sync(&lib.iceland);
    lib.sync(&lib.home);
    for q in [
        "iceland",
        "pics from iceland",
        "iceland family vacation",
        "photos of the iceland family vacation",
    ] {
        let r = lib.find(q, None, vec![]);
        let mut found = names(&r);
        found.sort();
        assert_eq!(found, ["DSC1.ARW", "DSC2.ARW"], "{q}");
        assert!(
            matches!(
                &r.sessions[0].reason,
                SessionReasonRecord::Name { .. } | SessionReasonRecord::Folder { .. }
            ),
            "{q}: {:?}",
            r.sessions[0].reason
        );
        assert!(
            r.query.places[0].sources.iter().any(|s| matches!(
                s,
                PlaceSourceRecord::SessionName { name } | PlaceSourceRecord::Folder { name }
                    if name == "2026-08-06 Iceland Family Vacation"
            )),
            "{q}: {:?}",
            r.query.places
        );
    }
    // A place nothing is called, without a vector: said so, nothing filtered.
    let r = lib.find("crete", None, vec![]);
    assert_eq!(r.query.places[0].sources.len(), 0);
    assert_eq!(r.total, 3);
}

#[test]
fn titles_written_in_the_app_are_found() {
    let lib = library();
    let imgs = lock(&lib.iceland.session).images().unwrap();
    let b = lock(&lib.iceland.session).variants(imgs[1].id).unwrap();
    block_on(lib.iceland.clone().set_metadata(
        b.iter().map(|v| v.0).collect(),
        crate::session::metadata::PhotoMetadata {
            title: "The old lighthouse".into(),
            ..Default::default()
        },
        tok(),
    ))
    .unwrap();
    assert!(lib.sync(&lib.iceland), "the title changed the find rows");
    // b looks nothing like the description (axis 1 vs 2) but its title says it.
    let r = lib.find("lighthouse", Some(axis(2)), vec![]);
    let at = names(&r).iter().position(|n| n == "b.ARW").unwrap();
    assert!(r.photos[at].reasons.contains(&MatchReasonRecord::Title {
        text: "The old lighthouse".into()
    }));
}
