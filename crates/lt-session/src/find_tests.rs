//! Find across sessions: sync, entity resolution, hard filters, ranking,
//! reasons and session matches.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use lt_core::date::{DateFilter, days_from_civil};
use lt_core::geom::Orientation;
use lt_core::{FileHash, PickState, Rating};

use crate::*;

const M: &str = "mobileclip-s2";

fn axis(i: usize, j: usize, mix: f32) -> Vec<f32> {
    let mut v = vec![0.0; 8];
    v[i] = 1.0;
    v[j] += mix;
    v
}

fn day(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d) * 86_400_000
}

fn key(n: u8) -> FileHash {
    FileHash([n; 32])
}

fn rec(path: &str, k: u8) -> FindRecord {
    FindRecord {
        path: PathBuf::from(path),
        key: Some(key(k)),
        capture_ms: Some(day(2024, 7, 1)),
        ..FindRecord::default()
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn spec() -> FindSpec {
    FindSpec {
        model: M.into(),
        ..FindSpec::default()
    }
}

fn paths(o: &FindOutcome) -> Vec<String> {
    o.photos
        .iter()
        .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// Three sessions: an Iceland trip (Alex and Sam), home (Alex), and an
/// unnamed card dump with a Sicily folder and keywords.
fn library(dir: &Path) -> GlobalIndex {
    let mut g = GlobalIndex::open(&dir.join("global.db")).unwrap();
    let iceland = [
        FindRecord {
            people: names(&["Alex"]),
            rating: 4,
            ..rec("/pics/2024-07 Iceland/Capture/h1.ARW", 1)
        },
        FindRecord {
            people: names(&["Sam"]),
            suggested: names(&["Alex"]),
            pick: PickState::Picked,
            ..rec("/pics/2024-07 Iceland/Capture/h2.ARW", 2)
        },
        FindRecord {
            capture_ms: Some(day(2023, 3, 5)),
            ..rec("/pics/2024-07 Iceland/Capture/h3.ARW", 3)
        },
    ];
    g.sync_find_items(
        Path::new("/pics/2024-07 Iceland"),
        "2024-07 Iceland",
        &iceland,
    )
    .unwrap();
    let home = [
        FindRecord {
            people: names(&["Alex", "Alexander"]),
            ..rec("/pics/Home/Capture/c1.ARW", 4)
        },
        FindRecord {
            capture_ms: None,
            ..rec("/pics/Home/Capture/c2.ARW", 5)
        },
    ];
    g.sync_find_items(Path::new("/pics/Home"), "Home", &home)
        .unwrap();
    let card = [
        FindRecord {
            keywords: names(&["Vestra Horn", "boat"]),
            ..rec("/pics/Card/Sicily trip/k1.ARW", 6)
        },
        FindRecord {
            location: names(&["Vik", "Sudurland", "Iceland"]),
            key: None,
            ..rec("/pics/Card/Misc/k2.ARW", 7)
        },
        FindRecord {
            keywords: names(&["zephyr"]),
            ..rec("/pics/Card/Misc/k3.ARW", 8)
        },
    ];
    g.sync_find_items(Path::new("/pics/Card"), "Card dump", &card)
        .unwrap();
    // h1, c1: axis 0 ("beach"); h2: axis 1; h3, k1: axis 2 ("cliffs"); others 3.
    g.store_embeddings(
        M,
        &[
            (key(1), axis(0, 1, 0.1)),
            (key(2), axis(1, 0, 0.1)),
            (key(3), axis(2, 0, 0.0)),
            (key(4), axis(0, 3, 0.3)),
            (key(5), axis(3, 0, 0.0)),
            (key(6), axis(2, 0, 0.2)),
            (key(8), axis(3, 0, 0.0)),
        ],
    )
    .unwrap();
    g
}

#[test]
fn alex_in_iceland() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let o = g
        .find(&FindSpec {
            people: vec![PersonQuery {
                term: "alex".into(),
                names: names(&["Alex", "Alexander"]),
            }],
            places: vec!["iceland".into()],
            ..spec()
        })
        .unwrap();
    // h1 (tagged), h2 (suggested) in the Iceland session; k2's location says
    // Iceland but nobody is in it; c1 is Alex at home.
    assert_eq!(paths(&o), ["h1.ARW", "h2.ARW"]);
    assert_eq!(o.total, 2);
    assert_eq!(
        o.photos[0].reasons,
        [
            MatchReason::Person {
                name: "Alex".into(),
                suggested: false
            },
            MatchReason::Place {
                term: "iceland".into(),
                source: PlaceSource::SessionName("2024-07 Iceland".into())
            }
        ]
    );
    assert_eq!(
        o.photos[1].reasons[0],
        MatchReason::Person {
            name: "Alex".into(),
            suggested: true
        }
    );
    assert_eq!(o.photos[0].session_name, "2024-07 Iceland");
    assert_eq!(
        o.photos[0].session_root,
        PathBuf::from("/pics/2024-07 Iceland")
    );
    assert!(o.photos[0].score > o.photos[1].score, "confirmed first");
    assert_eq!(
        o.places,
        [PlaceResolution {
            term: "iceland".into(),
            sources: vec![
                PlaceSource::Location("Iceland".into()),
                PlaceSource::SessionName("2024-07 Iceland".into()),
                PlaceSource::Folder("2024-07 Iceland".into()),
            ]
        }]
    );
    // Sessions: the one named after the place (with its hits), and the card
    // whose photo's location matched isn't a name match.
    assert_eq!(o.sessions[0].name, "2024-07 Iceland");
    assert_eq!(
        o.sessions[0].reason,
        SessionReason::Name {
            term: "iceland".into()
        }
    );
    assert_eq!(o.sessions[0].hits, 2);
    assert_eq!(o.unknown_people, [] as [std::string::String; 0]);
}

#[test]
fn place_sources_folder_keyword_location() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let find = |g: &mut GlobalIndex, place: &str| {
        g.find(&FindSpec {
            places: vec![place.into()],
            ..spec()
        })
        .unwrap()
    };
    let o = find(&mut g, "Sicily");
    assert_eq!(paths(&o), ["k1.ARW"]);
    assert_eq!(
        o.photos[0].reasons,
        [MatchReason::Place {
            term: "sicily".into(),
            source: PlaceSource::Folder("Sicily trip".into())
        }]
    );
    assert_eq!(
        o.sessions[0].reason,
        SessionReason::Folder {
            term: "sicily".into(),
            folder: "Sicily trip".into()
        }
    );
    let o = find(&mut g, "vestra horn");
    assert_eq!(paths(&o), ["k1.ARW"]);
    assert_eq!(
        o.photos[0].reasons[0],
        MatchReason::Place {
            term: "vestra horn".into(),
            source: PlaceSource::Keyword("Vestra Horn".into())
        }
    );
    assert!(matches!(
        o.sessions[0].reason,
        SessionReason::Keyword { .. }
    ));
    // Location fields win over the session name; k2 has no vector but needs none.
    let o = find(&mut g, "iceland");
    assert_eq!(o.total, 4);
    let k2 = o
        .photos
        .iter()
        .find(|p| p.path.ends_with("k2.ARW"))
        .unwrap();
    assert_eq!(
        k2.reasons[0],
        MatchReason::Place {
            term: "iceland".into(),
            source: PlaceSource::Location("Iceland".into())
        }
    );
    // Whole words only: "icelan" and "horn vestra" resolve to nothing (and without
    // a vector to look for, filter nothing; the summary says so).
    for t in ["icelan", "horn vestra"] {
        let o = find(&mut g, t);
        assert!(o.places[0].sources.is_empty(), "{t}");
        assert_eq!(o.total, 8);
    }
}

#[test]
fn a_place_nothing_is_called_is_a_soft_visual_signal() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    // "tropical cliffs" (axis 2): nothing is called that; looks like h3 and k1.
    let mut place_vectors = HashMap::new();
    place_vectors.insert("tropical".to_owned(), axis(2, 0, 0.0));
    let o = g
        .find(&FindSpec {
            places: vec!["tropical".into()],
            place_vectors: place_vectors.clone(),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["h3.ARW", "k1.ARW"]);
    assert_eq!(
        o.photos[0].reasons,
        [MatchReason::Place {
            term: "tropical".into(),
            source: PlaceSource::LooksLike
        }]
    );
    assert_eq!(o.places[0].sources, [PlaceSource::LooksLike]);
    // Without a vector it means nothing: no filter, no ranking.
    let o = g
        .find(&FindSpec {
            places: vec!["tropical".into()],
            ..spec()
        })
        .unwrap();
    assert_eq!(o.places[0].sources, [] as [find::PlaceSource; 0]);
    assert_eq!(o.total, 8);
    // With a text query it only boosts: "beach" (axis 0) ranks h1 and c1;
    // a place looking like axis 3 lifts c1 (0.3 of axis 3) over h1.
    place_vectors.insert("tropical".to_owned(), axis(3, 0, 0.0));
    let o = g
        .find(&FindSpec {
            places: vec!["tropical".into()],
            place_vectors,
            text: "beach".into(),
            text_vector: Some(axis(0, 0, 0.0)),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["c1.ARW", "h1.ARW"]);
    assert!(matches!(o.photos[0].reasons[0], MatchReason::Visual { .. }));
    // A vector of the wrong size is ignored rather than failing.
    let mut bad = HashMap::new();
    bad.insert("tropical".to_owned(), vec![1.0; 3]);
    let o = g
        .find(&FindSpec {
            places: vec!["tropical".into()],
            place_vectors: bad,
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 8);
}

#[test]
fn text_ranks_with_floor_and_window() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let o = g
        .find(&FindSpec {
            text: "cliffs".into(),
            text_vector: Some(axis(2, 0, 0.0)),
            ..spec()
        })
        .unwrap();
    // h3 1.0, k1 0.98; everything else ≤ 0.1 (below the floor).
    assert_eq!(paths(&o), ["h3.ARW", "k1.ARW"]);
    if let MatchReason::Visual { score } = o.photos[0].reasons[0] {
        assert!((score - 1.0).abs() < 1e-3);
    } else {
        panic!("{:?}", o.photos[0].reasons);
    }
    // A wide window and low floor bring in the rest that have vectors (7 of 8).
    let o = g
        .find(&FindSpec {
            text_vector: Some(axis(2, 0, 0.0)),
            options: FindOptions {
                min_score: -1.0,
                window: 3.0,
                limit: 3,
                ..FindOptions::default()
            },
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 7);
    assert_eq!(o.photos.len(), 3);
    // A text vector of another size matches nothing visually: filters only.
    let o = g
        .find(&FindSpec {
            text_vector: Some(vec![1.0; 3]),
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 8);
}

#[test]
fn hard_filters_date_rating_flag() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let o = g
        .find(&FindSpec {
            date: Some(DateFilter::months(1 << 2)),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["h3.ARW"]);
    assert_eq!(o.photos[0].reasons, [MatchReason::Date]);
    let o = g
        .find(&FindSpec {
            date: Some(DateFilter::range(day(2024, 1, 1), day(2025, 1, 1) - 1)),
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 6, "c2 has no capture time, h3 is 2023");
    let o = g
        .find(&FindSpec {
            min_rating: Some(4),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["h1.ARW"]);
    assert_eq!(o.photos[0].reasons, [MatchReason::Rating(4)]);
    let o = g
        .find(&FindSpec {
            max_rating: Some(0),
            pick: Some(PickState::Picked),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["h2.ARW"]);
    assert_eq!(
        o.photos[0].reasons,
        [MatchReason::Rating(0), MatchReason::Flag(PickState::Picked)]
    );
    // An empty query lists everything, newest first (undated last).
    let o = g.find(&spec()).unwrap();
    assert_eq!(o.total, 8);
    assert_eq!(paths(&o)[0], "h1.ARW");
    assert_eq!(paths(&o).last().unwrap(), "c2.ARW");
}

#[test]
fn unknown_people_are_reported_and_only_boost() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let o = g
        .find(&FindSpec {
            people: vec![PersonQuery {
                term: "Zephyr".into(),
                names: vec![],
            }],
            text: "x".into(),
            text_vector: Some(axis(3, 0, 0.0)),
            ..spec()
        })
        .unwrap();
    assert_eq!(o.unknown_people, ["Zephyr"]);
    // c2 and k3 look alike; k3's keyword names Zephyr.
    assert_eq!(paths(&o), ["k3.ARW", "c2.ARW"]);
    assert!(o.photos[0].reasons.contains(&MatchReason::Text {
        term: "zephyr".into()
    }));
    // A known person tagged nowhere: nothing.
    let o = g
        .find(&FindSpec {
            people: vec![PersonQuery {
                term: "dana".into(),
                names: names(&["Dana"]),
            }],
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 0);
    // Two people: both must be on the photo (c1 has Alex and Alexander).
    let o = g
        .find(&FindSpec {
            people: vec![
                PersonQuery {
                    term: "alex".into(),
                    names: names(&["Alex"]),
                },
                PersonQuery {
                    term: "alexander".into(),
                    names: names(&["Alexander"]),
                },
            ],
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["c1.ARW"]);
    // A folder named like the unknown person boosts too.
    let o = g
        .find(&FindSpec {
            people: vec![PersonQuery {
                term: "sicily".into(),
                names: vec![],
            }],
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o)[0], "k1.ARW");
}

#[test]
fn sessions_with_many_hits_and_name_matches_from_the_text() {
    let d = tempfile::tempdir().unwrap();
    let mut g = GlobalIndex::open(&d.path().join("global.db")).unwrap();
    let many: Vec<FindRecord> = (0..4)
        .map(|i| rec(&format!("/pics/Beach day/{i}.ARW"), 10 + i))
        .collect();
    g.sync_find_items(Path::new("/pics/Beach day"), "Beach day", &many)
        .unwrap();
    let few: Vec<FindRecord> = (0..4)
        .map(|i| rec(&format!("/pics/Party/{i}.ARW"), 20 + i))
        .collect();
    g.sync_find_items(Path::new("/pics/Party"), "Party", &few)
        .unwrap();
    let vecs: Vec<(FileHash, Vec<f32>)> = (0..4)
        .map(|i| (key(10 + i), axis(0, 1, 0.05 * f32::from(i))))
        .chain((0..4).map(|i| (key(20 + i), axis(0, 1, 0.1 + 0.05 * f32::from(i)))))
        .collect();
    g.store_embeddings(M, &vecs).unwrap();
    let o = g
        .find(&FindSpec {
            text: "beach".into(),
            text_vector: Some(axis(0, 0, 0.0)),
            ..spec()
        })
        .unwrap();
    assert_eq!(o.total, 8);
    assert_eq!(o.sessions.len(), 2);
    assert_eq!(o.sessions[0].name, "Beach day");
    assert_eq!(
        o.sessions[0].reason,
        SessionReason::Name {
            term: "beach".into()
        }
    );
    assert_eq!(o.sessions[1].name, "Party");
    assert_eq!(o.sessions[1].reason, SessionReason::ManyHits);
    assert_eq!(o.sessions[1].hits, 4);
    assert!(o.sessions[1].best_score > 0.9);
}

#[test]
fn syncs_write_only_changes_and_forgetting_drops_rows() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    let root = Path::new("/pics/Home");
    let gen0 = g.generation().unwrap();
    let home = [
        FindRecord {
            people: names(&["Alex", "Alexander"]),
            ..rec("/pics/Home/Capture/c1.ARW", 4)
        },
        FindRecord {
            capture_ms: None,
            ..rec("/pics/Home/Capture/c2.ARW", 5)
        },
    ];
    // Same rows in another order: no write.
    let reversed = [home[1].clone(), home[0].clone()];
    assert!(!g.sync_find_items(root, "Home", &reversed).unwrap());
    assert_eq!(g.generation().unwrap(), gen0);
    // A rating change is written and reaches results.
    let mut changed = home.clone();
    changed[1].rating = 5;
    assert!(g.sync_find_items(root, "Home", &changed).unwrap());
    let o = g
        .find(&FindSpec {
            min_rating: Some(5),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["c2.ARW"]);
    // Removed from the app's list: gone from Find.
    assert!(!g.forget_search(root).unwrap(), "it had no search items");
    let o = g.find(&spec()).unwrap();
    assert_eq!(o.total, 6);
    assert!(g.unregister_session(Path::new("/pics/Card")).unwrap());
    assert_eq!(g.find(&spec()).unwrap().total, 3);
    // Newlines in list values can't split them.
    let odd = [FindRecord {
        keywords: names(&["two\nlines"]),
        ..rec("/pics/X/a.ARW", 40)
    }];
    g.sync_find_items(Path::new("/pics/X"), "X", &odd).unwrap();
    let o = g
        .find(&FindSpec {
            places: vec!["two lines".into()],
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o), ["a.ARW"]);
}

#[test]
fn known_places_for_the_parser() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    assert_eq!(
        g.find_places(M).unwrap(),
        [
            "card dump",
            "home",
            "iceland",
            "misc",
            "sicily trip",
            "sudurland",
            "vik"
        ]
    );
}

#[test]
fn a_v2_global_index_gains_find_rows() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("global.db");
    drop(GlobalIndex::open(&path).unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("DROP TABLE find_items; PRAGMA user_version = 2;")
        .unwrap();
    drop(c);
    let mut g = GlobalIndex::open(&path).unwrap();
    assert!(
        g.sync_find_items(Path::new("/s"), "S", &[rec("/s/a.ARW", 1)])
            .unwrap()
    );
    assert_eq!(g.find(&spec()).unwrap().total, 1);
}

#[test]
fn session_records_cover_variants_and_skip_the_trash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Trip");
    let mut s = Session::create(&root, "Trip").unwrap();
    let mut add = |name: &str, i: i64| {
        let rel = format!("Capture/{name}.ARW");
        fs::write(root.join(&rel), name.as_bytes()).unwrap();
        let h = FileHash(*blake3::hash(name.as_bytes()).as_bytes());
        let meta = ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + i),
            width: 6000,
            height: 4000,
            orientation: Orientation::Normal,
            ..ImageMetaLite::default()
        };
        let id = s.add_original(&rel, h, meta).unwrap();
        (id, s.variants(id).unwrap()[0], h)
    };
    let (a, va, ha) = add("a", 0);
    let (_, vb, _) = add("b", 1);
    let (_, vc, _) = add("c", 2);
    s.set_rating(&[va], Rating::new(2).unwrap()).unwrap();
    s.set_keywords(&[va], &names(&["beach", "Crete"])).unwrap();
    s.set_pick(&[va], PickState::Picked).unwrap();
    let second = s.new_variant(a).unwrap();
    s.set_rating(&[second], Rating::new(5).unwrap()).unwrap();
    s.set_keywords(&[second], &names(&["Crete", "sunset"]))
        .unwrap();
    s.set_pick(&[second], PickState::Rejected).unwrap();
    s.move_to(&[vc], Folder::Trash).unwrap();
    s.set_iptc(
        &[va],
        &lt_core::IptcMeta {
            title: "Surf".into(),
            caption: "Big day".into(),
            ..lt_core::IptcMeta::default()
        },
    )
    .unwrap();
    s.set_iptc(
        &[second],
        &lt_core::IptcMeta {
            title: "Surf".into(),
            ..lt_core::IptcMeta::default()
        },
    )
    .unwrap();
    let _ = vb;
    let r = s.find_records().unwrap();
    assert_eq!(r.len(), 2, "c is in the Trash");
    assert_eq!(r[0].path, root.join("Capture/a.ARW"));
    assert_eq!(r[0].key, Some(ha));
    assert_eq!(r[0].capture_ms, Some(1_700_000_000_000));
    assert_eq!(r[0].rating, 5, "best variant");
    assert_eq!(r[0].pick, PickState::Picked, "first variant");
    assert_eq!(r[0].keywords, names(&["beach", "Crete", "sunset"]));
    assert_eq!(
        r[0].titles,
        names(&["Surf", "Big day"]),
        "every variant's, once"
    );
    assert_eq!(r[1].rating, 0);
    assert_eq!(r[1].titles, [] as [std::string::String; 0]);
}

#[test]
fn titles_and_captions_match_the_description() {
    let d = tempfile::tempdir().unwrap();
    let mut g = library(d.path());
    // c2 (no visual match for "cliffs") is titled with it; k2 has no vector at all.
    let titled = [
        FindRecord {
            capture_ms: None,
            titles: names(&["Sea Cliffs at Dusk"]),
            ..rec("/pics/Home/Capture/c2.ARW", 5)
        },
        FindRecord {
            people: names(&["Alex", "Alexander"]),
            ..rec("/pics/Home/Capture/c1.ARW", 4)
        },
    ];
    g.sync_find_items(Path::new("/pics/Home"), "Home", &titled)
        .unwrap();
    let card = [FindRecord {
        key: None,
        titles: names(&["Boat", "Caption about the cliffs"]),
        ..rec("/pics/Card/Misc/k2.ARW", 7)
    }];
    g.sync_find_items(Path::new("/pics/Card"), "Card dump", &card)
        .unwrap();
    let o = g
        .find(&FindSpec {
            text: "Cliffs".into(),
            text_vector: Some(axis(2, 0, 0.0)),
            ..spec()
        })
        .unwrap();
    // The titled photos are kept below the floor and without a vector, after
    // the strong visual match (the bonus orders, it doesn't outrank).
    assert_eq!(paths(&o), ["h3.ARW", "k2.ARW", "c2.ARW"]);
    assert_eq!(
        o.photos[1].reasons,
        [MatchReason::Title {
            text: "Caption about the cliffs".into()
        }]
    );
    assert!(o.photos[2].reasons.contains(&MatchReason::Title {
        text: "Sea Cliffs at Dusk".into()
    }));
    // Whole words only; without a model the title still finds and orders.
    let o = g
        .find(&FindSpec {
            text: "cliff".into(),
            ..spec()
        })
        .unwrap();
    assert!(o.photos.iter().all(|p| p.reasons.is_empty()));
    let o = g
        .find(&FindSpec {
            text: "sea cliffs".into(),
            ..spec()
        })
        .unwrap();
    assert_eq!(paths(&o)[0], "c2.ARW");
}

#[test]
fn a_v3_global_index_gains_the_titles_column() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("global.db");
    let mut g = GlobalIndex::open(&path).unwrap();
    g.sync_find_items(Path::new("/s"), "S", &[rec("/s/a.ARW", 1)])
        .unwrap();
    drop(g);
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("ALTER TABLE find_items DROP COLUMN titles; PRAGMA user_version = 3;")
        .unwrap();
    drop(c);
    let mut g = GlobalIndex::open(&path).unwrap();
    assert_eq!(g.find(&spec()).unwrap().total, 1, "rows kept");
    // The next sync of the session fills the titles.
    let titled = [FindRecord {
        titles: names(&["Lighthouse"]),
        ..rec("/s/a.ARW", 1)
    }];
    assert!(g.sync_find_items(Path::new("/s"), "S", &titled).unwrap());
    let o = g
        .find(&FindSpec {
            text: "lighthouse".into(),
            ..spec()
        })
        .unwrap();
    assert!(matches!(o.photos[0].reasons[0], MatchReason::Title { .. }));
    drop(g);
    // A second opener that read v3 before the first upgraded it: no duplicate column.
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA user_version = 3;").unwrap();
    drop(c);
    assert!(GlobalIndex::open(&path).is_ok());
    let v: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        v, 6,
        "3 -> 4 (titles) -> 5 (the map's positions) -> 6 (the library)"
    );
}
