//! The Library: sync from sessions, queries, overview, edits written through,
//! collections, schema upgrade.
#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};

use ds_core::date::days_from_civil;
use ds_core::geom::Orientation;
use ds_core::{Adjustments, ColorTag, FileHash, GeoPoint, IptcMeta, PickState, Rating};

use crate::*;

const DAY: i64 = 86_400_000;

fn day(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d) * DAY
}

fn meta(capture: Option<i64>, camera: &str) -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: capture,
        camera: Some(camera.into()),
        lens: Some("FE 24-70mm F2.8 GM".into()),
        iso: Some(100),
        focal_length_mm: Some(35.0),
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        as_shot_wb: None,
    }
}

fn add(s: &mut Session, rel: &str, capture: Option<i64>, camera: &str) -> VariantId {
    let p = s.root().join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, rel.as_bytes()).unwrap();
    let h = FileHash(*blake3::hash(rel.as_bytes()).as_bytes());
    let id = s.add_original(rel, h, meta(capture, camera)).unwrap();
    s.variants(id).unwrap()[0]
}

fn stars(n: u8) -> Rating {
    Rating::new(n).unwrap()
}

fn source(s: &Session) -> LibrarySource {
    LibrarySource {
        store: s.root().to_owned(),
        external: s.external_root().map(Path::to_path_buf),
        stamp: index_stamp(s.root()).unwrap(),
    }
}

fn sync(g: &mut GlobalIndex, s: &Session) -> bool {
    let known = g.library_known(s.root()).unwrap();
    let recs = s.library_records(&known).unwrap();
    g.sync_library(s.root(), s.name(), &source(s), &recs)
        .unwrap()
}

fn names(g: &mut GlobalIndex, q: &LibraryQuery) -> Vec<String> {
    let ids = g.library_query(q).unwrap();
    g.library_items(&ids)
        .unwrap()
        .into_iter()
        .map(|i| {
            let f = i.path.file_name().unwrap().to_string_lossy().into_owned();
            if i.ordinal == 1 {
                f
            } else {
                format!("{f}#{}", i.ordinal)
            }
        })
        .collect()
}

fn items_of(g: &mut GlobalIndex, q: &LibraryQuery) -> Vec<LibraryItem> {
    let ids = g.library_query(q).unwrap();
    g.library_items(&ids).unwrap()
}

struct Lib {
    dir: tempfile::TempDir,
    g: GlobalIndex,
    hike: Session,
    city: Session,
}

/// Two sessions: a hike (A9, three dated photos and one undated) and a city
/// walk (A7, two photos), with ratings, labels, flags, keywords and a title.
fn lib() -> Lib {
    let dir = tempfile::tempdir().unwrap();
    let mut hike = Session::create(&dir.path().join("Fall Hike"), "Fall Hike").unwrap();
    let a = add(
        &mut hike,
        "Capture/DSC2.ARW",
        Some(day(2024, 10, 5) + 1000),
        "ILCE-9",
    );
    let b = add(
        &mut hike,
        "Capture/DSC10.ARW",
        Some(day(2024, 10, 5) + 2000),
        "ILCE-9",
    );
    let c = add(
        &mut hike,
        "Capture/DSC3.ARW",
        Some(day(2024, 10, 6)),
        "ILCE-9",
    );
    let _undated = add(&mut hike, "Capture/scan.ARW", None, "ILCE-9");
    hike.set_rating(&[a], stars(5)).unwrap();
    hike.set_rating(&[b], stars(3)).unwrap();
    hike.set_color_tag(&[b], ColorTag::Red).unwrap();
    hike.set_pick(&[a], PickState::Picked).unwrap();
    hike.set_pick(&[c], PickState::Rejected).unwrap();
    hike.set_keywords(&[a, c], &["Trail".into(), "Fall".into()])
        .unwrap();
    hike.set_iptc(
        &[b],
        &IptcMeta {
            caption: "Summit at dawn".into(),
            ..IptcMeta::default()
        },
    )
    .unwrap();
    hike.set_geotag(&[c], GeoPoint::new(46.5, 8.0, None))
        .unwrap();
    let mut city = Session::create(&dir.path().join("City Walk"), "City Walk").unwrap();
    let d = add(
        &mut city,
        "Capture/IMG1.ARW",
        Some(day(2023, 3, 1)),
        "ILCE-7M4",
    );
    add(
        &mut city,
        "Capture/IMG2.ARW",
        Some(day(2023, 3, 1) + 5),
        "ILCE-7M4",
    );
    city.set_rating(&[d], stars(4)).unwrap();
    city.set_keywords(&[d], &["trail".into()]).unwrap();
    let mut g = GlobalIndex::open(&dir.path().join("global.db")).unwrap();
    assert!(sync(&mut g, &hike));
    assert!(sync(&mut g, &city));
    Lib { dir, g, hike, city }
}

fn session_id(g: &mut GlobalIndex, name: &str) -> i64 {
    g.library_overview()
        .unwrap()
        .sessions
        .into_iter()
        .find(|s| s.name == name)
        .unwrap()
        .id
}

#[test]
fn records_mirror_the_sessions_and_trash_and_missing_are_left_out() {
    let mut l = lib();
    let recs = l.hike.library_records(&KnownEdits::new()).unwrap();
    let a = recs.iter().find(|r| r.rel == "Capture/DSC2.ARW").unwrap();
    assert_eq!(
        (a.rating, a.pick, a.camera.as_str(), a.keywords.clone()),
        (
            5,
            PickState::Picked,
            "ILCE-9",
            vec!["Trail".into(), "Fall".into()]
        )
    );
    assert_eq!(a.lens, "FE 24-70mm F2.8 GM");
    let b = recs.iter().find(|r| r.rel == "Capture/DSC10.ARW").unwrap();
    assert_eq!(
        (b.label, b.title.as_str()),
        (ColorTag::Red, "Summit at dawn")
    );
    let c = recs.iter().find(|r| r.rel == "Capture/DSC3.ARW").unwrap();
    assert!(c.gps && !a.gps);
    assert_eq!(
        c.variant,
        l.hike.variants_at(&[(c.rel.clone(), 1)]).unwrap()[0]
            .unwrap()
            .0
    );

    // A title wins over the caption; a second variant is its own row.
    let id = l.hike.images().unwrap()[0].id;
    let v1 = l.hike.new_variant(id).unwrap();
    l.hike
        .set_iptc(
            &[v1],
            &IptcMeta {
                title: "Copy".into(),
                caption: "ignored".into(),
                ..IptcMeta::default()
            },
        )
        .unwrap();
    // Trash and missing originals aren't in the Library.
    let scan = l
        .hike
        .variants_at(&[("Capture/scan.ARW".into(), 1)])
        .unwrap()[0]
        .unwrap();
    l.hike.move_to(&[scan], Folder::Trash).unwrap();
    l.city
        .conn
        .execute(
            "UPDATE images SET missing = 1 WHERE rel_path = 'Capture/IMG2.ARW'",
            [],
        )
        .unwrap();
    assert!(sync(&mut l.g, &l.hike));
    assert!(sync(&mut l.g, &l.city));
    let all = names(&mut l.g, &LibraryQuery::default());
    assert_eq!(
        all,
        [
            "IMG1.ARW",
            "DSC2.ARW",
            "DSC10.ARW",
            "DSC10.ARW#2",
            "DSC3.ARW"
        ]
    );
    assert!(!all.contains(&"scan.ARW".to_owned()) && !all.contains(&"IMG2.ARW".to_owned()));
    let copy = items_of(&mut l.g, &LibraryQuery::default())
        .into_iter()
        .find(|i| i.ordinal == 2)
        .unwrap();
    assert_eq!(copy.title, "Copy");
}

#[test]
fn an_unchanged_sync_writes_nothing_and_a_change_rewrites_one_row() {
    let mut l = lib();
    assert!(!sync(&mut l.g, &l.hike), "nothing changed");
    let before = l.g.library_query(&LibraryQuery::default()).unwrap();
    let a = l
        .hike
        .variants_at(&[("Capture/DSC3.ARW".into(), 1)])
        .unwrap()[0]
        .unwrap();
    l.hike.set_rating(&[a], stars(2)).unwrap();
    assert!(sync(&mut l.g, &l.hike));
    let after = l.g.library_query(&LibraryQuery::default()).unwrap();
    assert_eq!(before, after, "ids are stable across syncs");
    let q = LibraryQuery {
        min_rating: 2,
        ..LibraryQuery::default()
    };
    assert!(names(&mut l.g, &q).contains(&"DSC3.ARW".to_owned()));
    // Moving the store (or its stamp) alone updates the source, not the rows.
    let mut src = source(&l.hike);
    src.stamp += 1;
    let known = l.g.library_known(l.hike.root()).unwrap();
    let recs = l.hike.library_records(&known).unwrap();
    assert!(
        !l.g.sync_library(l.hike.root(), "Fall Hike", &src, &recs)
            .unwrap()
    );
    let s = l.g.library_sessions().unwrap();
    let hike = s.iter().find(|s| s.name == "Fall Hike").unwrap();
    assert_eq!(hike.source.as_ref().unwrap().stamp, src.stamp);
    // Duplicate records count once.
    let mut twice = recs.clone();
    twice.push(recs[0].clone());
    assert!(
        !l.g.sync_library(l.hike.root(), "Fall Hike", &src, &twice)
            .unwrap()
    );
}

#[test]
fn an_edit_is_hashed_once_per_sidecar_change() {
    let mut l = lib();
    let a = l
        .hike
        .variants_at(&[("Capture/DSC2.ARW".into(), 1)])
        .unwrap()[0]
        .unwrap();
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 0.7;
    l.hike.save_adjustments(a, &adj).unwrap();
    let recs = l.hike.library_records(&KnownEdits::new()).unwrap();
    let rec = recs.iter().find(|r| r.variant == a.0).unwrap().clone();
    assert_eq!(
        rec.edit,
        Some(edit_hash(&l.hike.load_adjustments(a).unwrap()))
    );
    assert!(
        recs.iter()
            .filter(|r| r.variant != a.0)
            .all(|r| r.edit.is_none())
    );
    // Known at the same stamp: the sidecar isn't read again.
    let mut known = KnownEdits::new();
    known.insert((rec.rel.clone(), 1), (rec.stamp, Some([9; 32])));
    let again = l.hike.library_records(&known).unwrap();
    assert_eq!(
        again.iter().find(|r| r.variant == a.0).unwrap().edit,
        Some([9; 32])
    );
    // At another stamp it is.
    known.insert((rec.rel.clone(), 1), (rec.stamp + 1, Some([9; 32])));
    let fresh = l.hike.library_records(&known).unwrap();
    assert_eq!(
        fresh.iter().find(|r| r.variant == a.0).unwrap().edit,
        rec.edit
    );
    assert!(sync(&mut l.g, &l.hike));
    assert_eq!(
        l.g.library_known(l.hike.root()).unwrap()[&(rec.rel.clone(), 1)].1,
        rec.edit
    );
    let ids = l.g.library_query(&LibraryQuery::default()).unwrap();
    let item =
        l.g.library_items(&ids)
            .unwrap()
            .into_iter()
            .find(|i| i.variant == a.0)
            .unwrap();
    assert_eq!(item.edit, rec.edit);
}

#[test]
fn queries_filter_by_every_criterion() {
    let mut l = lib();
    let hike = session_id(&mut l.g, "Fall Hike");
    let q = |f: fn(&mut LibraryQuery)| {
        let mut q = LibraryQuery::default();
        f(&mut q);
        q
    };
    // Capture order, undated last.
    assert_eq!(
        names(&mut l.g, &LibraryQuery::default()),
        [
            "IMG1.ARW",
            "IMG2.ARW",
            "DSC2.ARW",
            "DSC10.ARW",
            "DSC3.ARW",
            "scan.ARW"
        ]
    );
    let mut by_session = LibraryQuery {
        sessions: vec![hike],
        ..LibraryQuery::default()
    };
    assert_eq!(names(&mut l.g, &by_session).len(), 4);
    by_session.from_ms = Some(day(2024, 10, 5));
    by_session.to_ms = Some(day(2024, 10, 6));
    assert_eq!(names(&mut l.g, &by_session), ["DSC2.ARW", "DSC10.ARW"]);
    by_session.to_ms = None;
    by_session.from_ms = Some(day(2024, 10, 6));
    assert_eq!(
        names(&mut l.g, &by_session),
        ["DSC3.ARW"],
        "undated never matches a date"
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.min_rating = 4)),
        ["IMG1.ARW", "DSC2.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.unrated = true)),
        ["IMG2.ARW", "DSC3.ARW", "scan.ARW"]
    );
    assert_eq!(
        names(
            &mut l.g,
            &q(|q| q.labels = vec![ColorTag::Red, ColorTag::Blue])
        ),
        ["DSC10.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.pick = PickFilter::Picked)),
        ["DSC2.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.pick = PickFilter::Rejected)),
        ["DSC3.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.pick = PickFilter::Unflagged)).len(),
        4
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.pick = PickFilter::Unrejected)).len(),
        5
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.cameras = vec!["ILCE-7M4".into()])),
        ["IMG1.ARW", "IMG2.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.cameras = vec!["Nope".into()])),
        [] as [std::string::String; 0]
    );
    // Keywords: any case, every one wanted.
    assert_eq!(
        names(&mut l.g, &q(|q| q.keywords = vec!["TRAIL".into()])),
        ["IMG1.ARW", "DSC2.ARW", "DSC3.ARW"]
    );
    assert_eq!(
        names(
            &mut l.g,
            &q(|q| q.keywords = vec!["trail".into(), "fall".into()])
        ),
        ["DSC2.ARW", "DSC3.ARW"]
    );
    // Text: word starts in the file name, title, keywords, camera or session name.
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "dsc1".into())),
        ["DSC10.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "Sümmit".into())),
        ["DSC10.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "walk".into())),
        ["IMG1.ARW", "IMG2.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "hike trail".into())),
        ["DSC2.ARW", "DSC3.ARW"]
    );
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "m4".into())).len(),
        0,
        "word starts only"
    );
    assert_eq!(names(&mut l.g, &q(|q| q.text = "ilce 7m4".into())).len(), 2);
    assert_eq!(
        names(&mut l.g, &q(|q| q.text = "  ".into())).len(),
        6,
        "blank text: all"
    );
}

#[test]
fn queries_sort() {
    let mut l = lib();
    let q = |sort, descending| LibraryQuery {
        sort,
        descending,
        ..LibraryQuery::default()
    };
    assert_eq!(
        names(&mut l.g, &q(LibrarySort::Captured, true)),
        [
            "scan.ARW",
            "DSC3.ARW",
            "DSC10.ARW",
            "DSC2.ARW",
            "IMG2.ARW",
            "IMG1.ARW"
        ]
    );
    assert_eq!(
        names(&mut l.g, &q(LibrarySort::Rating, false)),
        [
            "DSC2.ARW",
            "IMG1.ARW",
            "DSC10.ARW",
            "IMG2.ARW",
            "DSC3.ARW",
            "scan.ARW"
        ],
        "most stars first, then capture time"
    );
    assert_eq!(
        names(&mut l.g, &q(LibrarySort::Name, false)),
        [
            "DSC2.ARW",
            "DSC3.ARW",
            "DSC10.ARW",
            "IMG1.ARW",
            "IMG2.ARW",
            "scan.ARW"
        ],
        "numbers in names compare as numbers"
    );
    assert_eq!(
        names(&mut l.g, &q(LibrarySort::Session, false)),
        [
            "IMG1.ARW",
            "IMG2.ARW",
            "DSC2.ARW",
            "DSC10.ARW",
            "DSC3.ARW",
            "scan.ARW"
        ]
    );
}

#[test]
fn natural_order() {
    use crate::library::natural;
    use std::cmp::Ordering::{Equal, Greater, Less};
    assert_eq!(natural("a2", "a10"), Less);
    assert_eq!(natural("A10", "a9"), Greater);
    assert_eq!(natural("a007", "a7"), Less, "same number: the text decides");
    assert_eq!(natural("abc", "ABD"), Less);
    assert_eq!(natural("ab", "abc"), Less);
    assert_eq!(natural("abc", "ab"), Greater);
    assert_eq!(natural("x", "x"), Equal);
}

#[test]
fn items_resolve_paths_for_plain_and_external_sessions() {
    let mut l = lib();
    let ids = l.g.library_query(&LibraryQuery::default()).unwrap();
    let items = l.g.library_items(&ids).unwrap();
    let a = items.iter().find(|i| i.rel == "Capture/DSC2.ARW").unwrap();
    assert_eq!(a.path, l.hike.root().join("Capture/DSC2.ARW"));
    assert_eq!(a.session_root, l.hike.root());
    assert_eq!(a.session_name, "Fall Hike");
    assert_eq!(a.capture_ms, Some(day(2024, 10, 5) + 1000));
    assert_eq!(
        l.g.library_items(&[-5, ids[0]]).unwrap().len(),
        1,
        "unknown ids dropped"
    );

    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("x.ARW"), b"x").unwrap();
    let mut ext = Session::create_external(&t.path().join("store"), "Shoot", &folder).unwrap();
    ext.add_originals(vec![(
        "Capture/x.ARW".into(),
        UNHASHED,
        meta(None, "ILCE-9"),
    )])
    .unwrap();
    // The app's root for it is the folder; the store is elsewhere.
    let known = l.g.library_known(&folder).unwrap();
    let recs = ext.library_records(&known).unwrap();
    assert!(
        l.g.sync_library(&folder, "Shoot", &source(&ext), &recs)
            .unwrap()
    );
    let q = LibraryQuery {
        text: "x".into(),
        ..LibraryQuery::default()
    };
    let items = items_of(&mut l.g, &q);
    assert_eq!(items[0].path, folder.join("x.ARW"));
    assert_eq!(items[0].session_root, folder);
}

#[test]
fn overview_counts_sessions_days_cameras_keywords() {
    let mut l = lib();
    // A session registered for dedupe only: listed with nothing.
    l.g.register_session(Path::new("/nowhere/Old"), "Old")
        .unwrap();
    let o = l.g.library_overview().unwrap();
    assert_eq!(o.total, 6);
    let counts: Vec<(&str, u64)> = o
        .sessions
        .iter()
        .map(|s| (s.name.as_str(), s.count))
        .collect();
    assert_eq!(counts, [("City Walk", 2), ("Fall Hike", 4), ("Old", 0)]);
    let days: Vec<(i32, u32, u32, u64)> = o
        .days
        .iter()
        .map(|d| (d.year, d.month, d.day, d.count))
        .collect();
    assert_eq!(days, [(2023, 3, 1, 2), (2024, 10, 5, 2), (2024, 10, 6, 1)]);
    assert_eq!(o.undated, 1);
    assert_eq!(o.cameras, [("ILCE-7M4".into(), 2), ("ILCE-9".into(), 4)]);
    assert_eq!(
        o.keywords,
        [("Fall".into(), 2), ("Trail".into(), 2), ("trail".into(), 1)]
    );
    assert_eq!(o.collections, [] as [library::LibraryCollection; 0]);
}

#[test]
fn edits_write_through_to_the_sidecars_and_patch_the_rows() {
    let mut l = lib();
    let q = LibraryQuery {
        text: "dsc3 img1".into(),
        ..LibraryQuery::default()
    };
    assert_eq!(l.g.library_query(&q).unwrap(), [] as [i64; 0]);
    let all = l.g.library_query(&LibraryQuery::default()).unwrap();
    let items = l.g.library_items(&all).unwrap();
    let pick = |rel: &str| items.iter().find(|i| i.rel == rel).unwrap().id;
    let ids = [pick("Capture/DSC3.ARW"), pick("Capture/IMG1.ARW"), -1];
    let targets = l.g.library_targets(&ids).unwrap();
    assert_eq!(targets.len(), 2);
    let generation = l.g.library.generation;
    for t in &targets {
        let mut s = Session::open(&t.store).unwrap();
        let rels: Vec<(String, u32)> = t.photos.iter().map(|p| p.1.clone()).collect();
        let done = apply_library_edit(&mut s, &rels, LibraryEdit::Rating(stars(1))).unwrap();
        assert_eq!(done, [0]);
        let done = apply_library_edit(&mut s, &rels, LibraryEdit::Label(ColorTag::Green)).unwrap();
        assert_eq!(done, [0]);
        let mut gone = rels.clone();
        gone.push(("Capture/nope.ARW".into(), 1));
        let done = apply_library_edit(&mut s, &gone, LibraryEdit::Pick(PickState::Picked)).unwrap();
        assert_eq!(done, [0], "a photo no longer there is skipped");
    }
    assert_eq!(
        l.g.library_patch(&ids, LibraryEdit::Rating(stars(1)))
            .unwrap(),
        2
    );
    assert_eq!(
        l.g.library_patch(&ids, LibraryEdit::Label(ColorTag::Green))
            .unwrap(),
        2
    );
    assert_eq!(
        l.g.library_patch(&ids, LibraryEdit::Pick(PickState::Picked))
            .unwrap(),
        2
    );
    assert_eq!(
        l.g.library_patch(&ids, LibraryEdit::Rating(stars(1)))
            .unwrap(),
        0,
        "no change"
    );
    // Patched in place: same rows, newer generation, no reload.
    assert!(l.g.library.generation > generation);
    let got = l.g.library_items(&ids[..2]).unwrap();
    assert!(
        got.iter()
            .all(|i| i.rating == 1 && i.label == ColorTag::Green && i.pick == PickState::Picked)
    );
    // The sidecars say so, so a fresh sync agrees.
    let hike = Session::open(l.hike.root()).unwrap();
    let v = hike.variants_at(&[("Capture/DSC3.ARW".into(), 1)]).unwrap()[0].unwrap();
    let info = hike.variant(v).unwrap();
    assert_eq!(
        (info.rating.stars(), info.color_tag, info.pick),
        (1, ColorTag::Green, PickState::Picked)
    );
    sync(&mut l.g, &hike);
    assert_eq!(l.g.library_items(&ids[..1]).unwrap()[0].rating, 1);

    // Another writer between: the next query reloads instead of patching.
    let other = GlobalIndex::open(&l.dir.path().join("global.db")).unwrap();
    other
        .conn
        .execute("UPDATE library_generation SET n = n + 1", [])
        .unwrap();
    assert_eq!(
        l.g.library_patch(&ids, LibraryEdit::Rating(stars(2)))
            .unwrap(),
        2
    );
    assert!(
        l.g.library_items(&ids[..2])
            .unwrap()
            .iter()
            .all(|i| i.rating == 2)
    );
    // A session never synced has no store: nothing to target.
    let lone = l.g.register_session(Path::new("/nowhere"), "X").unwrap();
    l.g.conn
        .execute(
            "INSERT INTO library_items(session_id, rel, ordinal, variant, rating, label, pick, camera, lens, keywords, title, gps, stamp)
             VALUES (?, 'Capture/a.ARW', 0, 1, 0, 0, 0, '', '', '', '', 0, 0)",
            [lone],
        )
        .unwrap();
    l.g.conn
        .execute("UPDATE library_generation SET n = n + 1", [])
        .unwrap();
    let id = l.g.conn.last_insert_rowid();
    let all = l.g.library_query(&LibraryQuery::default()).unwrap();
    assert_eq!(all.len(), 7);
    assert!(
        l.g.library_targets(&all)
            .unwrap()
            .iter()
            .all(|t| t.session_name != "X")
    );
    let orphan =
        l.g.library_items(&all)
            .unwrap()
            .into_iter()
            .find(|i| i.session_name == "X")
            .unwrap();
    assert_eq!(
        orphan.path,
        PathBuf::from("/nowhere/Capture/a.ARW"),
        "no store: under its root"
    );
    assert!(id > 0);
}

#[test]
fn collections_span_sessions_and_survive_a_resync() {
    let mut l = lib();
    let all = l.g.library_query(&LibraryQuery::default()).unwrap();
    let items = l.g.library_items(&all).unwrap();
    let id_of = |rel: &str| items.iter().find(|i| i.rel == rel).unwrap().id;
    let (a, d) = (id_of("Capture/DSC2.ARW"), id_of("Capture/IMG1.ARW"));
    let best = l.g.create_collection(" Best ", None, &[a, d, a]).unwrap();
    let in_best = LibraryQuery {
        collection: Some(best),
        ..LibraryQuery::default()
    };
    assert_eq!(names(&mut l.g, &in_best), ["IMG1.ARW", "DSC2.ARW"]);
    assert_eq!(
        l.g.add_to_collection(best, &[a, id_of("Capture/DSC3.ARW")])
            .unwrap(),
        1
    );
    assert_eq!(l.g.remove_from_collection(best, &[d, d]).unwrap(), 1);
    assert_eq!(names(&mut l.g, &in_best), ["DSC2.ARW", "DSC3.ARW"]);
    // The collection's own criteria and the query's combine.
    let rated = LibraryQuery {
        min_rating: 5,
        ..in_best.clone()
    };
    assert_eq!(names(&mut l.g, &rated), ["DSC2.ARW"]);

    // Smart: rules, ANDed with the query; its sort and collection are dropped.
    let rules = LibraryQuery {
        keywords: vec!["trail".into()],
        text: "dsc".into(),
        sort: LibrarySort::Name,
        collection: Some(best),
        ..LibraryQuery::default()
    };
    let smart = l.g.create_collection("Trail", Some(&rules), &[d]).unwrap();
    let in_smart = LibraryQuery {
        collection: Some(smart),
        ..LibraryQuery::default()
    };
    assert_eq!(names(&mut l.g, &in_smart), ["DSC2.ARW", "DSC3.ARW"]);
    assert!(matches!(
        l.g.add_to_collection(smart, &[d]),
        Err(SessionError::SmartCollection(_))
    ));
    assert!(matches!(
        l.g.add_to_collection(999, &[d]),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        l.g.create_collection("  ", None, &[]),
        Err(SessionError::InvalidName(_))
    ));
    let cs = l.g.collections().unwrap();
    let listed: Vec<(&str, u64, bool)> = cs
        .iter()
        .map(|c| (c.name.as_str(), c.count, c.rules.is_some()))
        .collect();
    assert_eq!(listed, [("Best", 2, false), ("Trail", 2, true)]);
    assert_eq!(cs[1].rules.as_ref().unwrap().sort, LibrarySort::Captured);
    assert_eq!(cs[1].rules.as_ref().unwrap().collection, None);

    // Rename; new rules for a smart one; a plain one stays plain.
    let narrower = LibraryQuery {
        pick: PickFilter::Picked,
        ..LibraryQuery::default()
    };
    assert!(
        l.g.update_collection(smart, "Picked", Some(&narrower))
            .unwrap()
    );
    assert_eq!(names(&mut l.g, &in_smart), ["DSC2.ARW"]);
    assert!(
        l.g.update_collection(best, "Keepers", Some(&narrower))
            .unwrap()
    );
    assert_eq!(names(&mut l.g, &in_best), ["DSC2.ARW", "DSC3.ARW"]);
    assert!(!l.g.update_collection(999, "x", None).unwrap());

    // Membership is by session, file and variant: it survives the rows being rebuilt.
    let hike = l.hike.root().to_owned();
    l.g.add_original(&hike, "Capture/DSC2.ARW", &FileHash([1; 32]))
        .unwrap();
    l.g.forget_search(&hike).unwrap();
    assert_eq!(names(&mut l.g, &in_best), [] as [std::string::String; 0]);
    assert!(sync(&mut l.g, &l.hike));
    assert_eq!(names(&mut l.g, &in_best), ["DSC2.ARW", "DSC3.ARW"]);

    assert!(l.g.delete_collection(best).unwrap());
    assert!(!l.g.delete_collection(best).unwrap());
    assert!(
        names(&mut l.g, &in_best).is_empty(),
        "a deleted collection holds nothing"
    );
    assert_eq!(l.g.library_overview().unwrap().collections.len(), 1);
}

#[test]
fn forgetting_or_unregistering_a_session_drops_its_rows() {
    let mut l = lib();
    l.g.forget_search(l.city.root()).unwrap();
    assert_eq!(
        l.g.library_query(&LibraryQuery::default()).unwrap().len(),
        4
    );
    assert!(l.g.unregister_session(l.hike.root()).unwrap());
    assert_eq!(
        l.g.library_query(&LibraryQuery::default()).unwrap(),
        [] as [i64; 0]
    );
    // A rename reaches the Library.
    assert!(sync(&mut l.g, &l.city));
    l.g.register_session(l.city.root(), "Renamed").unwrap();
    let items = items_of(&mut l.g, &LibraryQuery::default());
    assert!(items.iter().all(|i| i.session_name == "Renamed"));
}

#[test]
fn index_stamp_moves_with_each_write() {
    let l = lib();
    let mut s = Session::open(l.hike.root()).unwrap();
    let before = index_stamp(s.root()).unwrap();
    let v = s.variants_at(&[("Capture/DSC2.ARW".into(), 1)]).unwrap()[0].unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    s.set_rating(&[v], stars(1)).unwrap();
    assert_ne!(index_stamp(s.root()), Some(before));
    let empty = tempfile::tempdir().unwrap();
    fs::write(empty.path().join("notes.txt"), b"x").unwrap();
    fs::write(empty.path().join("noext"), b"x").unwrap();
    assert_eq!(index_stamp(empty.path()), None);
    assert_eq!(index_stamp(Path::new("/nowhere/at/all")), None);
}

#[test]
fn a_v5_index_gains_the_library_tables() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("global.db");
    {
        let g = GlobalIndex::open(&p).unwrap();
        g.conn
            .execute_batch(
                "DROP TABLE library_items; DROP TABLE library_sources; DROP TABLE collection_items;
                 DROP TABLE collections; DROP TABLE library_generation; PRAGMA user_version=5;",
            )
            .unwrap();
    }
    let mut g = GlobalIndex::open(&p).unwrap();
    let v: i64 = g
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 6);
    assert_eq!(
        g.library_query(&LibraryQuery::default()).unwrap(),
        [] as [i64; 0]
    );
    assert_eq!(g.library_overview().unwrap(), LibraryOverview::default());
    // A v4 file goes all the way.
    drop(g);
    {
        let g = GlobalIndex::open(&p).unwrap();
        g.conn
            .execute_batch(
                "DROP TABLE library_items; DROP TABLE library_sources; DROP TABLE collection_items;
                 DROP TABLE collections; DROP TABLE library_generation;
                 ALTER TABLE find_items DROP COLUMN lat; ALTER TABLE find_items DROP COLUMN lon;
                 PRAGMA user_version=4;",
            )
            .unwrap();
    }
    let mut g = GlobalIndex::open(&p).unwrap();
    assert_eq!(
        g.library_sessions().unwrap(),
        [] as [library::LibrarySession; 0]
    );
    assert_eq!(
        g.collections().unwrap(),
        [] as [library::LibraryCollection; 0]
    );
}

#[test]
fn a_smart_collection_with_unreadable_rules_holds_nothing() {
    let mut l = lib();
    let id =
        l.g.create_collection("S", Some(&LibraryQuery::default()), &[])
            .unwrap();
    l.g.conn
        .execute(
            "UPDATE collections SET rules = 'garbage' WHERE id = ?",
            [id],
        )
        .unwrap();
    let q = LibraryQuery {
        collection: Some(id),
        ..LibraryQuery::default()
    };
    assert_eq!(l.g.library_query(&q).unwrap(), [] as [i64; 0]);
    let cs = l.g.collections().unwrap();
    assert_eq!(
        (cs[0].rules.clone(), cs[0].count),
        (None, 0),
        "listed as plain, empty"
    );
}

#[test]
fn edit_hash_is_the_json_hash() {
    let mut a = Adjustments::default();
    let h0 = edit_hash(&a);
    a.exposure.exposure_ev = 1.0;
    assert_ne!(edit_hash(&a), h0);
    assert_eq!(
        edit_hash(&a),
        *blake3::hash(&serde_json::to_vec(&a).unwrap()).as_bytes()
    );
}
