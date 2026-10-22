#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ds_core::FileHash;
use ds_session::{ImageMetaLite, UNHASHED, VariantId};

use super::*;
use crate::journal::{can_revert, revert};
use crate::model::{Develop, Setting, SourceKind};

struct Env {
    _dir: tempfile::TempDir,
    folder: PathBuf,
    store: PathBuf,
}

fn bytes(rel: &str) -> Vec<u8> {
    format!("raw bytes of {rel}").into_bytes()
}

fn hash_of(b: &[u8]) -> FileHash {
    FileHash(*blake3::hash(b).as_bytes())
}

const FILES: [&str; 3] = ["a.ARW", "b_longer.ARW", "sub/c.ARW"];

/// A folder of fake originals and the in-place Darkslide session over it.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Shoot");
    fs::create_dir_all(folder.join("sub")).unwrap();
    for rel in FILES {
        fs::write(folder.join(rel), bytes(rel)).unwrap();
    }
    let store = dir.path().join("store");
    let mut s = Session::create_external(&store, "Shoot", &folder).unwrap();
    for rel in FILES {
        let meta = ImageMetaLite {
            width: 6000,
            height: 4000,
            capture_time_ms: Some(1_000 + rel.len() as i64),
            ..ImageMetaLite::default()
        };
        s.add_original(&format!("Capture/{rel}"), hash_of(&bytes(rel)), meta)
            .unwrap();
    }
    Env {
        _dir: dir,
        folder,
        store,
    }
}

impl Env {
    fn session(&self) -> Session {
        Session::open(&self.store).unwrap()
    }

    fn photo(&self, key: &str, rel: &str) -> LrPhoto {
        LrPhoto {
            key: key.into(),
            path: self.folder.join(rel),
            ..LrPhoto::default()
        }
    }

    fn run(&self, lib: &LrLibrary, opts: &ImportOptions) -> ImportReport {
        import(&mut self.session(), lib, opts, &|| false).unwrap()
    }

    fn variant(&self, rel: &str, i: usize) -> VariantId {
        variant(&self.session(), rel, i)
    }
}

fn variant(s: &Session, rel: &str, i: usize) -> VariantId {
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|im| im.rel_path.as_str() == format!("Capture/{rel}"))
        .unwrap();
    s.variants(img.id).unwrap()[i]
}

fn lib(photos: Vec<LrPhoto>) -> LrLibrary {
    LrLibrary {
        kind: SourceKind::Catalog,
        origin: PathBuf::from("Catalog.lrcat"),
        id: Some("cat-1".into()),
        written_by: Some("Lightroom Classic 13".into()),
        photos,
        collections: Vec::new(),
        warnings: Vec::new(),
    }
}

fn rating(n: u8) -> Option<Rating> {
    Rating::new(n)
}

fn exposure(v: f64) -> Develop {
    BTreeMap::from([("Exposure2012".to_owned(), Setting::Num(v))])
}

const DRY: fn() -> ImportOptions = || ImportOptions {
    dry_run: true,
    ..ImportOptions::default()
};

fn overwrite() -> ImportOptions {
    ImportOptions {
        overwrite_edits: true,
        ..ImportOptions::default()
    }
}

/// Every file under `dir`: relative path → (size, BLAKE3).
fn checksums(dir: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, String)>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(root, &p, out);
            } else {
                let b = fs::read(&p).unwrap();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, (b.len() as u64, blake3::hash(&b).to_hex().to_string()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    // SQLite's WAL/shm and the index may be touched by opening alone.
    out.retain(|k, _| !k.ends_with("-wal") && !k.ends_with("-shm") && !k.ends_with(".dsession"));
    out
}

fn organized(e: &Env) -> LrLibrary {
    let mut a = e.photo("A", "a.ARW");
    a.rating = rating(4);
    a.color_tag = Some(ColorTag::Red);
    a.pick = Some(PickState::Picked);
    a.keywords = Some(vec![
        "Paris".into(),
        " France ".into(),
        "Paris".into(),
        String::new(),
    ]);
    a.title = Some(" Sunset ".into());
    a.caption = Some("Over the bridge".into());
    a.copyright = Some("(c) Someone".into());
    a.unmapped_label = None;
    let mut b = e.photo("B", "sub/c.ARW");
    b.rating = rating(2);
    b.unmapped_label = Some("Approved".into());
    lib(vec![a, b])
}

#[test]
fn first_import_writes_organization_and_is_idempotent() {
    let e = env();
    let l = organized(&e);
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!(
        (r.photos, r.matched, r.matched_by_path, r.variants),
        (2, 2, 2, 2)
    );
    assert_eq!(r.written_by.as_deref(), Some("Lightroom Classic 13"));
    for (cat, n) in [
        (Category::Rating, 2),
        (Category::Label, 1),
        (Category::Flag, 1),
        (Category::Keywords, 1),
        (Category::Metadata, 1),
    ] {
        assert_eq!(r.changed[&cat], n, "{cat:?}");
    }
    assert_eq!(r.unmapped_labels["Approved"], 1);
    let s = e.session();
    let v = s.variant(e.variant("a.ARW", 0)).unwrap();
    assert_eq!(v.rating, rating(4).unwrap());
    assert_eq!(v.color_tag, ColorTag::Red);
    assert_eq!(v.pick, PickState::Picked);
    assert_eq!(v.keywords, vec!["Paris", "France"]);
    assert_eq!(v.iptc.title, "Sunset");
    assert_eq!(v.iptc.caption, "Over the bridge");
    assert_eq!(v.iptc.copyright, "(c) Someone");
    let w = s.variant(e.variant("sub/c.ARW", 0)).unwrap();
    assert_eq!(w.rating, rating(2).unwrap());
    assert_eq!(w.color_tag, ColorTag::None);

    // The same source again: nothing to do.
    let again = e.run(&l, &ImportOptions::default());
    assert!(again.changed.is_empty(), "{:?}", again.changed);
    assert_eq!(again.in_sync, 2);
    assert_eq!(again.conflicts.len(), 0);
    assert!(can_revert(&e.session()));
}

#[test]
fn dry_run_reports_without_writing() {
    let e = env();
    let before = checksums(&e.store);
    let r = e.run(&organized(&e), &DRY());
    assert!(r.dry_run);
    assert_eq!(r.changed[&Category::Rating], 2);
    assert_eq!(checksums(&e.store), before, "a dry run writes nothing");
    assert!(!can_revert(&e.session()));
    let s = e.session();
    assert_eq!(
        s.variant(e.variant("a.ARW", 0)).unwrap().rating,
        Rating::default()
    );
}

#[test]
fn darkslide_edits_survive_unless_both_sides_changed() {
    let e = env();
    let rated = |n| {
        let mut p = e.photo("A", "a.ARW");
        p.rating = rating(n);
        lib(vec![p])
    };
    e.run(&rated(3), &ImportOptions::default());
    let v = e.variant("a.ARW", 0);
    // The user re-rates in Darkslide; Lightroom is unchanged: kept.
    e.session().set_rating(&[v], rating(5).unwrap()).unwrap();
    let r = e.run(&rated(3), &ImportOptions::default());
    assert_eq!((r.kept_darkslide, r.conflicts.len()), (1, 0));
    assert_eq!(e.session().variant(v).unwrap().rating, rating(5).unwrap());
    // Lightroom changes it too: a conflict, left alone ...
    let r = e.run(&rated(2), &ImportOptions::default());
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].field, "rating");
    assert_eq!(r.conflicts[0].file, "a.ARW");
    assert!(r.notes[0].notes[0].contains("Changed in both"));
    assert_eq!(e.session().variant(v).unwrap().rating, rating(5).unwrap());
    // ... until the caller says Lightroom wins.
    let r = e.run(&rated(2), &overwrite());
    assert_eq!((r.overwritten, r.conflicts.len()), (1, 0));
    assert_eq!(e.session().variant(v).unwrap().rating, rating(2).unwrap());
    // Darkslide untouched since the last import: Lightroom's new value is written.
    let r = e.run(&rated(1), &ImportOptions::default());
    assert_eq!(r.changed[&Category::Rating], 1);
    assert_eq!(e.session().variant(v).unwrap().rating, rating(1).unwrap());
}

#[test]
fn without_a_record_a_darkslide_value_counts_as_an_edit() {
    let e = env();
    let v = e.variant("a.ARW", 0);
    e.session().set_rating(&[v], rating(5).unwrap()).unwrap();
    let mut p = e.photo("A", "a.ARW");
    p.rating = rating(2);
    let r = e.run(&lib(vec![p.clone()]), &ImportOptions::default());
    assert_eq!(r.conflicts.len(), 1);
    // Lightroom saying "unrated" has nothing to say.
    p.rating = rating(0);
    let r = e.run(&lib(vec![p]), &ImportOptions::default());
    assert!(r.conflicts.is_empty() && r.changed.is_empty());
    assert_eq!(e.session().variant(v).unwrap().rating, rating(5).unwrap());
}

#[test]
fn unselected_categories_are_left_alone_and_unrecorded() {
    let e = env();
    let l = organized(&e);
    let only_ratings = ImportOptions {
        categories: [Category::Rating].into(),
        ..ImportOptions::default()
    };
    let r = e.run(&l, &only_ratings);
    assert_eq!(
        r.changed.keys().copied().collect::<Vec<_>>(),
        [Category::Rating]
    );
    let v = e.session().variant(e.variant("a.ARW", 0)).unwrap();
    assert!(v.keywords.is_empty() && v.iptc.title.is_empty());
    // Later, the rest: it is a first import for those fields.
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!(r.changed[&Category::Keywords], 1);
    assert!(!r.changed.contains_key(&Category::Rating));
}

#[test]
fn revert_puts_back_what_still_holds_the_imported_value() {
    let e = env();
    let l = organized(&e);
    e.run(&l, &ImportOptions::default());
    let a = e.variant("a.ARW", 0);
    // The user changes the rating afterwards; the keywords stay as imported.
    e.session().set_rating(&[a], rating(1).unwrap()).unwrap();
    let rep = revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(rep.restored, 2);
    assert_eq!(rep.kept, 1);
    let s = e.session();
    let v = s.variant(a).unwrap();
    assert_eq!(v.rating, rating(1).unwrap(), "changed since: kept");
    assert!(v.keywords.is_empty() && v.iptc.title.is_empty());
    assert_eq!(v.color_tag, ColorTag::None);
    assert_eq!(v.pick, PickState::Unflagged);
    assert_eq!(
        s.variant(e.variant("sub/c.ARW", 0)).unwrap().rating,
        Rating::default()
    );
    assert!(!can_revert(&s));
    assert!(revert(&mut e.session()).unwrap().is_none());
    // The import record went back too: the photos count as never imported.
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!(r.changed[&Category::Keywords], 1);
}

#[test]
fn imports_fold_into_one_journal() {
    let e = env();
    let rated = |n| {
        let mut p = e.photo("A", "a.ARW");
        p.rating = rating(n);
        p.title = Some(format!("T{n}"));
        lib(vec![p])
    };
    e.run(&rated(2), &ImportOptions::default());
    e.run(&rated(4), &ImportOptions::default());
    let rep = revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(rep.restored, 1);
    let v = e.session().variant(e.variant("a.ARW", 0)).unwrap();
    assert_eq!(
        v.rating,
        Rating::default(),
        "back to before the first import"
    );
    assert_eq!(v.iptc.title, "");
}

#[test]
fn virtual_copies_become_extra_variants_once() {
    let e = env();
    let mut master = e.photo("M", "a.ARW");
    master.rating = rating(3);
    let mut c1 = e.photo("C1", "a.ARW");
    c1.master = Some("M".into());
    c1.copy_name = Some("Copy 1".into());
    c1.order = 1;
    c1.rating = rating(5);
    c1.title = Some("Copy title".into());
    c1.keywords = Some(vec!["bw".into()]);
    let mut c2 = e.photo("C2", "a.ARW");
    c2.master = Some("M".into());
    c2.order = 2;
    c2.rating = rating(1);
    let l = lib(vec![c2, master, c1]);
    let dry = e.run(&l, &DRY());
    assert_eq!((dry.variants, dry.variants_created, dry.matched), (3, 2, 1));
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!((r.variants, r.variants_created), (3, 2));
    let s = e.session();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/a.ARW")
        .unwrap();
    let vs = s.variants(img.id).unwrap();
    assert_eq!(vs.len(), 3);
    let stars: Vec<u8> = vs
        .iter()
        .map(|v| s.variant(*v).unwrap().rating.stars())
        .collect();
    assert_eq!(stars, [3, 5, 1]);
    assert_eq!(s.variant(vs[1]).unwrap().iptc.title, "Copy title");
    assert_eq!(s.variant(vs[1]).unwrap().keywords, vec!["bw"]);
    // Again: the same variants, nothing created.
    let again = e.run(&l, &ImportOptions::default());
    assert_eq!((again.variants, again.variants_created), (3, 0));
    assert!(again.changed.is_empty(), "{:?}", again.changed);
    assert_eq!(e.session().variants(img.id).unwrap().len(), 3);
    // A variant the user deleted is not recreated.
    e.session().delete_variant(vs[2]).unwrap();
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!((r.deleted_in_darkslide, r.variants_created), (1, 0));
    // Revert removes the copies it made (the first one was changed since).
    e.session()
        .set_rating(&[vs[1]], rating(2).unwrap())
        .unwrap();
    let rep = revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(rep.variants_removed, 0);
    assert_eq!(rep.kept, 1);
    let s = e.session();
    assert_eq!(s.variants(img.id).unwrap().len(), 2);
    assert_eq!(s.variant(vs[0]).unwrap().rating, Rating::default());
}

#[test]
fn reverting_removes_created_variants_that_are_unchanged() {
    let e = env();
    let master = e.photo("M", "a.ARW");
    let mut c1 = e.photo("C1", "a.ARW");
    c1.master = Some("M".into());
    c1.order = 1;
    c1.rating = rating(5);
    e.run(&lib(vec![master, c1]), &ImportOptions::default());
    let rep = revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(rep.variants_removed, 1);
    let s = e.session();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/a.ARW")
        .unwrap();
    assert_eq!(s.variants(img.id).unwrap().len(), 1);
}

#[test]
fn orphan_copies_are_reported() {
    let e = env();
    let mut c = e.photo("C", "a.ARW");
    c.master = Some("gone".into());
    let r = e.run(&lib(vec![c]), &DRY());
    assert!(
        r.warnings.iter().any(|w| w.contains("virtual copy")),
        "{:?}",
        r.warnings
    );
    assert_eq!(r.matched, 0);
}

#[test]
fn photos_are_matched_by_path_content_then_name() {
    let e = env();
    let outside = tempfile::tempdir().unwrap();
    // Same bytes as a.ARW, but at a path the session doesn't know.
    let moved = outside.path().join("renamed.ARW");
    fs::write(&moved, bytes("a.ARW")).unwrap();
    let mut by_hash = e.photo("H", "a.ARW");
    by_hash.path = moved;
    // A path in the session with a different case.
    let mut by_case = e.photo("P", "SUB/C.arw");
    by_case.rating = rating(1);
    // A file that isn't on this machine, with the right name and capture time.
    let mut by_name = e.photo("N", "b_longer.ARW");
    by_name.path = PathBuf::from("/Volumes/Old Disk/2019/B_LONGER.arw");
    by_name.capture_time_ms = Some(1_000 + "b_longer.ARW".len() as i64);
    let r = e.run(&lib(vec![by_hash, by_case, by_name]), &DRY());
    assert_eq!(
        (
            r.matched,
            r.matched_by_path,
            r.matched_by_hash,
            r.matched_by_name
        ),
        (3, 1, 1, 1)
    );
    assert!(r.not_found.is_empty() && r.ambiguous.is_empty());
}

#[test]
fn nothing_found_is_reported_by_folder() {
    let e = env();
    let mk = |key: &str, p: &str| LrPhoto {
        key: key.into(),
        path: PathBuf::from(p),
        capture_time_ms: Some(5),
        ..LrPhoto::default()
    };
    let r = e.run(
        &lib(vec![
            mk("1", "/nowhere/x/a.ARW"),
            mk("2", "/nowhere/x/b.ARW"),
            mk("3", "/nowhere/y/c.ARW"),
        ]),
        &DRY(),
    );
    assert_eq!(r.matched, 0);
    assert_eq!(r.not_found.len(), 3);
    assert_eq!(
        r.not_found_folders,
        vec![
            FolderCount {
                folder: "/nowhere/x".into(),
                photos: 2
            },
            FolderCount {
                folder: "/nowhere/y".into(),
                photos: 1
            },
        ]
    );
    assert_eq!(r.not_found[0].path, "/nowhere/x/a.ARW");
}

#[test]
fn ambiguity_is_not_guessed() {
    let e = env();
    // A second session photo with the same bytes as a.ARW.
    fs::write(e.folder.join("dup.ARW"), bytes("a.ARW")).unwrap();
    let mut s = e.session();
    s.add_original(
        "Capture/dup.ARW",
        hash_of(&bytes("a.ARW")),
        ImageMetaLite::default(),
    )
    .unwrap();
    drop(s);
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().join("x.ARW");
    fs::write(&elsewhere, bytes("a.ARW")).unwrap();
    let mut two = e.photo("two", "a.ARW");
    two.path = elsewhere;
    // Another photo claims a.ARW by path first; this one then has nowhere to go.
    let first = e.photo("first", "b_longer.ARW");
    let mut second = e.photo("second", "b_longer.ARW");
    second.path = e.folder.join("B_LONGER.ARW");
    let r = e.run(&lib(vec![two, first, second]), &DRY());
    assert_eq!(r.matched, 1);
    assert_eq!(r.ambiguous.len(), 2, "{:?}", r.ambiguous);
    assert_eq!(r.not_found.len(), 0);
}

#[test]
fn unhashed_session_photos_are_hashed_on_demand() {
    let e = env();
    let content = b"lazily hashed original, unique size".to_vec();
    fs::write(e.folder.join("lazy.ARW"), &content).unwrap();
    e.session()
        .add_original("Capture/lazy.ARW", UNHASHED, ImageMetaLite::default())
        .unwrap();
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().join("x.ARW");
    fs::write(&elsewhere, &content).unwrap();
    let mut p = e.photo("K", "lazy.ARW");
    p.path = elsewhere;
    let r = e.run(&lib(vec![p]), &DRY());
    assert_eq!((r.matched, r.matched_by_hash), (1, 1));
}

#[test]
fn cancelling_while_planning_writes_nothing() {
    let e = env();
    let before = checksums(&e.store);
    let err = import(
        &mut e.session(),
        &organized(&e),
        &ImportOptions::default(),
        &|| true,
    )
    .unwrap_err();
    assert!(matches!(err, LrError::Cancelled));
    assert_eq!(checksums(&e.store), before);
}

#[test]
fn collections_become_albums_that_update_and_revert() {
    let e = env();
    let mut a = e.photo("A", "a.ARW");
    a.rating = rating(1);
    let b = e.photo("B", "sub/c.ARW");
    let mut l = lib(vec![a, b]);
    l.collections = vec![
        LrCollection {
            id: 7,
            name: "Best".into(),
            parents: vec!["Trip".into()],
            members: vec!["A".into(), "zzz".into(), "A".into()],
            ..LrCollection::default()
        },
        LrCollection {
            id: 8,
            name: "Five stars".into(),
            smart: true,
            ..LrCollection::default()
        },
        LrCollection {
            id: 9,
            name: " ".into(),
            members: vec!["B".into()],
            ..LrCollection::default()
        },
    ];
    let dry = e.run(&l, &DRY());
    assert_eq!(dry.albums_created, 2);
    assert!(e.session().albums().iter().all(|a| a.name != "Trip / Best"));
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!((r.albums_created, r.albums_updated), (2, 0));
    assert_eq!(r.collections_skipped.len(), 1);
    assert!(r.collections_skipped[0].reason.contains("smart"));
    let s = e.session();
    let album = s
        .albums()
        .iter()
        .find(|a| a.name == "Trip / Best")
        .unwrap()
        .id;
    assert_eq!(
        s.album_variants(album).unwrap(),
        vec![e.variant("a.ARW", 0)]
    );
    assert!(s.albums().iter().any(|a| a.name == "Lightroom collection"));
    // A new member is added, not a new album.
    l.collections[0].members.push("B".into());
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!((r.albums_created, r.albums_updated), (0, 1));
    assert_eq!(e.session().album_variants(album).unwrap().len(), 2);
    // Without the category, albums are not touched.
    let none = ImportOptions {
        categories: [Category::Rating].into(),
        ..ImportOptions::default()
    };
    l.collections[0].members.clear();
    let r = e.run(&l, &none);
    assert_eq!(r.albums_created + r.albums_updated, 0);
    // An album the user deleted is not recreated.
    e.session().delete_album(album).unwrap();
    let r = e.run(&l, &ImportOptions::default());
    assert!(
        r.collections_skipped
            .iter()
            .any(|s| s.reason.contains("deleted"))
    );
    // Revert: the created albums go away.
    let rep = revert(&mut e.session()).unwrap().unwrap();
    assert!(rep.albums_removed >= 1, "{rep:?}");
    assert!(
        e.session()
            .albums()
            .iter()
            .all(|a| a.name != "Lightroom collection")
    );
}

#[test]
fn another_catalog_finds_albums_by_name() {
    let e = env();
    let a = e.photo("A", "a.ARW");
    let mut l = lib(vec![a]);
    l.collections = vec![LrCollection {
        id: 1,
        name: "Keepers".into(),
        members: vec!["A".into()],
        ..LrCollection::default()
    }];
    e.run(&l, &ImportOptions::default());
    // Same names, other ids: the album is reused, not duplicated.
    l.id = Some("cat-2".into());
    l.collections[0].id = 99;
    let r = e.run(&l, &ImportOptions::default());
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("different Lightroom catalog"))
    );
    assert_eq!(r.albums_created, 0);
    let n = e
        .session()
        .albums()
        .iter()
        .filter(|a| a.name == "Keepers")
        .count();
    assert_eq!(n, 1);
}

#[test]
fn an_unreadable_record_is_reported_and_replaced() {
    let e = env();
    let l = organized(&e);
    e.run(&l, &ImportOptions::default());
    fs::write(state::path(&e.store), b"{broken").unwrap();
    let r = e.run(&l, &ImportOptions::default());
    assert!(r.warnings.iter().any(|w| w.contains("starting over")));
    assert!(state::load(&e.store).is_ok());
}

#[test]
fn notes_and_counts_come_from_the_mapping() {
    let e = env();
    let mut a = e.photo("A", "a.ARW");
    a.develop = Some(exposure(0.5));
    let r = e.run(&lib(vec![a]), &DRY());
    // The mapping decides what is exact, approximate or dropped; the
    // report only tallies it (see map.rs for the rules).
    assert_eq!(r.variants, 1);
    assert_eq!(
        r.mapped_keys.values().map(|(_, n)| *n).sum::<usize>(),
        r.mapped_keys.len()
    );
}

// The tests below need the real mapping (map.rs): Exposure2012 and Contrast2012
// map 1:1 onto `exposure.exposure_ev` and `exposure.contrast`, Vibrance onto
// `presence.vibrance` (same sign).

fn develop(pairs: &[(&str, f64)]) -> Develop {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Setting::Num(*v)))
        .collect()
}

fn adjustments(e: &Env, rel: &str, i: usize) -> Adjustments {
    let s = e.session();
    s.load_adjustments(variant(&s, rel, i)).unwrap()
}

#[test]
fn develop_categories_merge_independently() {
    let e = env();
    let src = |ev: f64, vibrance: f64| {
        let mut p = e.photo("A", "a.ARW");
        p.develop = Some(develop(&[
            ("Exposure2012", ev),
            ("Contrast2012", 20.0),
            ("Vibrance", vibrance),
        ]));
        lib(vec![p])
    };
    let dry = e.run(&src(0.5, 30.0), &DRY());
    assert_eq!(
        (dry.changed[&Category::Tone], dry.changed[&Category::Color]),
        (1, 1)
    );
    assert!(dry.mapped_keys.contains_key("Exposure2012"));
    assert_eq!(adjustments(&e, "a.ARW", 0), Adjustments::default());
    let r = e.run(&src(0.5, 30.0), &ImportOptions::default());
    assert_eq!(
        (r.changed[&Category::Tone], r.changed[&Category::Color]),
        (1, 1)
    );
    let a = adjustments(&e, "a.ARW", 0);
    assert_eq!((a.exposure.exposure_ev, a.exposure.contrast), (0.5, 20.0));
    assert!(a.presence.vibrance > 0.0);
    // Idempotent.
    let again = e.run(&src(0.5, 30.0), &ImportOptions::default());
    assert!(again.changed.is_empty() && again.conflicts.is_empty());
    // The user edits tone in Darkslide: kept while Lightroom is unchanged ...
    let v = e.variant("a.ARW", 0);
    let mut mine = adjustments(&e, "a.ARW", 0);
    mine.exposure.exposure_ev = 1.0;
    mine.presence.texture = 12.0;
    e.session().apply_batch(&[(v, mine)]).unwrap();
    let r = e.run(&src(0.5, 30.0), &ImportOptions::default());
    assert_eq!((r.kept_darkslide, r.conflicts.len()), (1, 0));
    // ... a conflict when Lightroom changed it too, while colour (only
    // Lightroom changed) is written ...
    let r = e.run(&src(0.8, 10.0), &ImportOptions::default());
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].field, "tone");
    assert_eq!(r.changed[&Category::Color], 1);
    assert_eq!(adjustments(&e, "a.ARW", 0).exposure.exposure_ev, 1.0);
    // ... and overwritten on request. Unrelated settings are never touched.
    let r = e.run(&src(0.8, 10.0), &overwrite());
    assert_eq!(r.overwritten, 1);
    let a = adjustments(&e, "a.ARW", 0);
    assert_eq!(a.exposure.exposure_ev, 0.8);
    assert_eq!(a.presence.texture, 12.0);
    // Reset in Lightroom: the import puts Darkslide back to default.
    let mut p = e.photo("A", "a.ARW");
    p.develop = Some(develop(&[("Exposure2012", 0.0)]));
    let r = e.run(&lib(vec![p]), &ImportOptions::default());
    assert_eq!(r.changed[&Category::Tone], 1);
    assert_eq!(adjustments(&e, "a.ARW", 0).exposure.exposure_ev, 0.0);
}

#[test]
fn develop_revert_touches_only_what_was_imported() {
    let e = env();
    let mut p = e.photo("A", "a.ARW");
    p.develop = Some(develop(&[("Exposure2012", 0.5)]));
    let l = lib(vec![p]);
    e.run(&l, &ImportOptions::default());
    let v = e.variant("a.ARW", 0);
    let mut mine = adjustments(&e, "a.ARW", 0);
    mine.presence.vibrance = 15.0;
    e.session().apply_batch(&[(v, mine)]).unwrap();
    revert(&mut e.session()).unwrap().unwrap();
    let a = adjustments(&e, "a.ARW", 0);
    assert_eq!(a.exposure.exposure_ev, 0.0);
    assert_eq!(a.presence.vibrance, 15.0);
}

#[test]
fn unselected_develop_categories_are_not_written() {
    let e = env();
    let mut p = e.photo("A", "a.ARW");
    p.develop = Some(develop(&[("Exposure2012", 0.5), ("Vibrance", 30.0)]));
    let opts = ImportOptions {
        categories: [Category::Color].into(),
        ..ImportOptions::default()
    };
    e.run(&lib(vec![p]), &opts);
    let a = adjustments(&e, "a.ARW", 0);
    assert_eq!(a.exposure.exposure_ev, 0.0);
    assert!(a.presence.vibrance > 0.0);
}

#[test]
fn virtual_copies_get_their_own_develop_settings() {
    let e = env();
    let mut master = e.photo("M", "a.ARW");
    master.develop = Some(develop(&[("Exposure2012", 0.5)]));
    let mut copy = e.photo("C", "a.ARW");
    copy.master = Some("M".into());
    copy.order = 1;
    copy.develop = Some(develop(&[("Exposure2012", -1.0)]));
    let l = lib(vec![master, copy]);
    e.run(&l, &ImportOptions::default());
    assert_eq!(adjustments(&e, "a.ARW", 0).exposure.exposure_ev, 0.5);
    assert_eq!(adjustments(&e, "a.ARW", 1).exposure.exposure_ev, -1.0);
    let again = e.run(&l, &ImportOptions::default());
    assert!(again.changed.is_empty(), "{:?}", again.changed);
    revert(&mut e.session()).unwrap().unwrap();
    let s = e.session();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/a.ARW")
        .unwrap();
    assert_eq!(s.variants(img.id).unwrap().len(), 1);
    assert_eq!(adjustments(&e, "a.ARW", 0).exposure.exposure_ev, 0.0);
}

#[test]
fn imported_masks_replace_only_imported_layers() {
    use ds_core::adjust::Layer;
    // Hand-written layers stand in for what the mapping makes: the groups
    // module owns the rule, this checks the import honours the user's layers.
    let e = env();
    let v = e.variant("a.ARW", 0);
    let mut mine = adjustments(&e, "a.ARW", 0);
    mine.layers.push(Layer {
        id: 3,
        name: "My layer".into(),
        ..Layer::default()
    });
    e.session().apply_batch(&[(v, mine)]).unwrap();
    let mut p = e.photo("A", "a.ARW");
    p.develop = Some(develop(&[("Exposure2012", 0.5)]));
    e.run(&lib(vec![p]), &ImportOptions::default());
    let a = adjustments(&e, "a.ARW", 0);
    assert_eq!(a.layers.len(), 1);
    assert_eq!(a.layers[0].name, "My layer");
}

fn gradient(ev: f64) -> Develop {
    let corr = Setting::Table(BTreeMap::from([
        ("LocalExposure2012".to_owned(), Setting::Num(ev)),
        ("ZeroX".to_owned(), Setting::Num(0.5)),
        ("ZeroY".to_owned(), Setting::Num(0.8)),
        ("FullX".to_owned(), Setting::Num(0.5)),
        ("FullY".to_owned(), Setting::Num(0.1)),
    ]));
    BTreeMap::from([(
        "GradientBasedCorrections".to_owned(),
        Setting::List(vec![corr]),
    )])
}

#[test]
fn imported_layers_are_tracked_by_id_not_by_name() {
    use ds_core::adjust::Layer;
    let e = env();
    let v = e.variant("a.ARW", 0);
    let mut mine = adjustments(&e, "a.ARW", 0);
    for (id, name) in [(3, "Lightroom look"), (4, "Lightroom: mine")] {
        mine.layers.push(Layer {
            id,
            name: name.into(),
            ..Layer::default()
        });
    }
    e.session().apply_batch(&[(v, mine)]).unwrap();
    let names = |e: &Env| -> Vec<(u64, String)> {
        adjustments(e, "a.ARW", 0)
            .layers
            .iter()
            .map(|l| (l.id, l.name.clone()))
            .collect()
    };
    let mut p = e.photo("A", "a.ARW");
    p.develop = Some(gradient(-0.5));
    e.run(&lib(vec![p.clone()]), &ImportOptions::default());
    let after = names(&e);
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(
        after[..2],
        [(3, "Lightroom look".into()), (4, "Lightroom: mine".into())]
    );
    assert!(after[2].1.starts_with("Lightroom: "));
    // Again: nothing changes. Then a new value replaces only the imported layer.
    assert!(
        e.run(&lib(vec![p.clone()]), &ImportOptions::default())
            .changed
            .is_empty()
    );
    p.develop = Some(gradient(-1.5));
    let r = e.run(&lib(vec![p]), &ImportOptions::default());
    assert_eq!(r.changed.get(&Category::Masks), Some(&1));
    let again = names(&e);
    assert_eq!(again.len(), 3);
    assert_eq!(again[..2], after[..2], "the user's layers are untouched");
    // Revert puts the layer list back.
    revert(&mut e.session()).unwrap().unwrap();
    let back = names(&e);
    assert_eq!(
        back,
        [(3, "Lightroom look".into()), (4, "Lightroom: mine".into())]
    );
}

#[test]
fn a_virtual_copy_never_takes_a_variant_the_user_made() {
    let e = env();
    let s0 = e.session();
    let img = s0
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/a.ARW")
        .unwrap();
    let mine = e.session().new_variant(img.id).unwrap();
    e.session().rename_variant(mine, "Mine").unwrap();
    let master = e.photo("M", "a.ARW");
    let mut copy = e.photo("C", "a.ARW");
    copy.master = Some("M".into());
    copy.order = 1;
    copy.rating = rating(4);
    let l = lib(vec![master, copy]);
    let r = e.run(&l, &ImportOptions::default());
    assert_eq!(r.variants_created, 1);
    let s = e.session();
    let vs = s.variants(img.id).unwrap();
    assert_eq!(vs.len(), 3);
    assert_eq!(s.variant(mine).unwrap().rating, Rating::default());
    assert_eq!(s.variant(mine).unwrap().name, "Mine");
    // The copy is found again through the record, not made twice.
    assert_eq!(e.run(&l, &ImportOptions::default()).variants_created, 0);
    assert_eq!(e.session().variants(img.id).unwrap().len(), 3);
}

#[test]
fn a_failure_part_way_through_creating_variants_can_be_reverted() {
    let e = env();
    let mut s = e.session();
    let img = s.images().unwrap()[0].id;
    let before = s.variants(img).unwrap().len();
    let creations = vec![
        (-1, img, Created::default()),
        (-2, ImageId(9_999_999), Created::default()),
    ];
    let mut writes = Writes::default();
    let mut resolved = HashMap::new();
    let mut state = State::default();
    let out = apply_planned(
        &mut s,
        Journal::new(State::default()),
        creations,
        (&mut writes, &mut resolved, &mut state),
    );
    assert!(out.is_err());
    assert_eq!(s.variants(img).unwrap().len(), before + 1, "one was made");
    assert!(can_revert(&s), "the journal was saved with it");
    let rep = revert(&mut s).unwrap().unwrap();
    assert_eq!(rep.variants_removed, 1);
    assert_eq!(s.variants(img).unwrap().len(), before);
}

#[test]
fn big_lists_do_not_go_quadratic() {
    let started = std::time::Instant::now();
    // Keywords, an album's members and the journal's record of them.
    let kw: Vec<String> = (0..100_000).map(|i| format!("kw{}", i % 60_000)).collect();
    assert_eq!(normalized(&kw).len(), 60_000);
    let ids: Vec<VariantId> = (0..100_000).map(VariantId).collect();
    let mut j = Journal::default();
    j.added_members(AlbumId(1), &ids);
    j.added_members(AlbumId(1), &ids);
    assert_eq!(j.members_added[&1].len(), 100_000);
    let e = env();
    let mut s = e.session();
    let resolved: HashMap<String, VariantId> = (0..100_000)
        .map(|i| (format!("k{i}"), VariantId(i)))
        .collect();
    let coll = LrCollection {
        id: 1,
        name: "Big".into(),
        members: (0..100_000).map(|i| format!("k{i}")).collect(),
        ..LrCollection::default()
    };
    let l = LrLibrary {
        collections: vec![coll],
        ..lib(Vec::new())
    };
    let mut report = ImportReport::default();
    import_collections(
        &mut s,
        &l,
        (&mut State::default(), &mut Writer::dry(), &HashMap::new()),
        &resolved,
        &DRY(),
        &mut report,
    )
    .unwrap();
    assert_eq!(report.albums_created, 1);
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}

#[test]
fn thousands_of_virtual_copies_import_in_bounded_time() {
    let e = env();
    let master = e.photo("M", "a.ARW");
    let mut photos = vec![master];
    for i in 1..=2_000 {
        let mut c = e.photo(&format!("C{i}"), "a.ARW");
        c.master = Some("M".into());
        c.order = i;
        photos.push(c);
    }
    let started = std::time::Instant::now();
    let r = e.run(&lib(photos), &ImportOptions::default());
    assert_eq!(r.variants_created, 2_000);
    assert!(started.elapsed().as_secs() < 240, "{:?}", started.elapsed());
}
