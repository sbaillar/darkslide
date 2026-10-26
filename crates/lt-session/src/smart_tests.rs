//! Smart folders: built-in exact-rating/pick/tag albums, keyword filters,
//! user smart albums, mirror flag.

use std::fs;
use std::path::PathBuf;

use lt_core::geom::Orientation;
use lt_core::{ColorTag, FileHash, PickState, Rating};

use crate::*;

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    s: Session,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Smart");
    let s = Session::create(&root, "Smart").unwrap();
    Fx { _dir: dir, root, s }
}

impl Fx {
    fn add(&mut self, i: i64) -> VariantId {
        let rel = format!("Capture/{i}.ARW");
        let p = self.root.join(&rel);
        let bytes = format!("raw-{i}");
        fs::write(&p, bytes.as_bytes()).unwrap();
        let meta = ImageMetaLite {
            capture_time_ms: Some(1_700_000_000_000 + i * 1000),
            camera: Some("ILCE-9".into()),
            lens: None,
            iso: Some(100),
            focal_length_mm: None,
            width: 6000,
            height: 4000,
            orientation: Orientation::Normal,
            as_shot_wb: None,
        };
        let id = self
            .s
            .add_original(
                &rel,
                FileHash(*blake3::hash(bytes.as_bytes()).as_bytes()),
                meta,
            )
            .unwrap();
        self.s.variants(id).unwrap()[0]
    }

    fn key(&self, k: &str) -> AlbumId {
        self.s
            .albums()
            .iter()
            .find(|a| a.builtin.as_deref() == Some(k))
            .unwrap_or_else(|| panic!("no built-in {k}"))
            .id
    }
}

fn r(n: u8) -> Rating {
    Rating::new(n).unwrap()
}

#[test]
fn builtin_folders_are_exact_and_idempotent() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..7).map(|i| f.add(i)).collect();
    assert_eq!(f.s.tags_in_use().unwrap(), [] as [lt_core::ColorTag; 0]);
    assert_eq!(
        f.s.ensure_builtin_albums().unwrap(),
        8,
        "5 exact ratings + unrated + picked + rejected"
    );
    assert_eq!(f.s.ensure_builtin_albums().unwrap(), 0, "idempotent");
    for (i, v) in vs.iter().take(5).enumerate() {
        f.s.set_rating(&[*v], r(u8::try_from(i + 1).unwrap()))
            .unwrap();
    }
    f.s.set_pick(&[vs[5]], PickState::Picked).unwrap();
    f.s.set_pick(&[vs[0]], PickState::Rejected).unwrap();
    for n in 1..=5 {
        let id = f.key(&format!("rating:{n}"));
        assert_eq!(
            f.s.album_variants(id).unwrap(),
            vec![vs[n - 1]],
            "exactly {n} stars, not {n} and up"
        );
    }
    assert_eq!(f.s.album(f.key("rating:1")).unwrap().name, "1 star");
    assert_eq!(f.s.album(f.key("rating:4")).unwrap().name, "4 stars");
    assert_eq!(
        f.s.album_variants(f.key("unrated")).unwrap(),
        vec![vs[5], vs[6]]
    );
    assert_eq!(f.s.album_variants(f.key("picked")).unwrap(), vec![vs[5]]);
    assert_eq!(f.s.album_variants(f.key("rejected")).unwrap(), vec![vs[0]]);
    let names: Vec<String> = builtin_smart_folders(&[])
        .into_iter()
        .map(|b| b.1)
        .collect();
    assert_eq!(
        names,
        [
            "5 stars", "4 stars", "3 stars", "2 stars", "1 star", "Unrated", "Picked", "Rejected"
        ]
    );
}

#[test]
fn tag_folders_follow_tags_in_use() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..3).map(|i| f.add(i)).collect();
    f.s.ensure_builtin_albums().unwrap();
    f.s.set_color_tag(&[vs[1]], ColorTag::Orange).unwrap();
    f.s.set_color_tag(&[vs[2]], ColorTag::Pink).unwrap();
    assert_eq!(
        f.s.tags_in_use().unwrap(),
        vec![ColorTag::Orange, ColorTag::Pink]
    );
    assert_eq!(f.s.ensure_builtin_albums().unwrap(), 2);
    let orange = f.key("tag:orange");
    assert_eq!(f.s.album(orange).unwrap().name, "Orange");
    assert_eq!(f.s.album_variants(orange).unwrap(), vec![vs[1]]);
    assert_eq!(f.s.album_variants(f.key("tag:pink")).unwrap(), vec![vs[2]]);
    // Tags only in the Trash don't count.
    f.s.move_to(&[vs[2]], Folder::Trash).unwrap();
    assert_eq!(f.s.tags_in_use().unwrap(), vec![ColorTag::Orange]);
    assert!(
        builtin_smart_folders(&[ColorTag::None])
            .iter()
            .all(|b| !b.0.starts_with("tag:")),
        "no folder for 'no tag'"
    );
}

#[test]
fn keywords_match_case_insensitively_beyond_ascii() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..3).map(|i| f.add(i)).collect();
    f.s.add_keyword(&[vs[0]], "Été").unwrap();
    f.s.add_keyword(&[vs[1]], "Österreich").unwrap();
    f.s.add_keyword(&[vs[2]], "Ørsted").unwrap();
    let q = |kw: &str| {
        f.s.query(
            &Filter {
                keywords: vec![kw.to_owned()],
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap()
    };
    assert_eq!(q("été"), vec![vs[0]]);
    assert_eq!(q("ÉTÉ"), vec![vs[0]]);
    assert_eq!(q("österreich"), vec![vs[1]]);
    assert_eq!(q("ørsted"), vec![vs[2]]);
}

#[test]
fn keywords_match_whole_words() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..3).map(|i| f.add(i)).collect();
    f.s.add_keyword(&[vs[0]], "sea").unwrap();
    f.s.add_keyword(&[vs[1]], "Seaside").unwrap();
    f.s.add_keyword(&[vs[2]], "Sea").unwrap();
    f.s.add_keyword(&[vs[2]], "sunset").unwrap();
    let q = |kws: &[&str]| {
        f.s.query(
            &Filter {
                keywords: kws.iter().map(|s| (*s).to_owned()).collect(),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap()
    };
    assert_eq!(
        q(&["SEA"]),
        vec![vs[0], vs[2]],
        "case-insensitive, whole keyword"
    );
    assert_eq!(q(&["sea", "sunset"]), vec![vs[2]], "all must match");
    assert_eq!(q(&["  "]).len(), 3, "blank keywords ignored");
    // Rating range: max only and min..max.
    f.s.set_rating(&[vs[0]], r(2)).unwrap();
    f.s.set_rating(&[vs[1]], r(4)).unwrap();
    let range =
        f.s.query(
            &Filter {
                min_rating: Rating::new(2),
                max_rating: Rating::new(3),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap();
    assert_eq!(range, vec![vs[0]]);
}

#[test]
fn user_folders_are_editable_builtins_are_protected_and_come_back() {
    let mut f = fx();
    let v = f.add(0);
    f.s.set_color_tag(&[v], ColorTag::Red).unwrap();
    f.s.ensure_builtin_albums().unwrap();
    let red = f.key("tag:red");
    assert!(matches!(
        f.s.delete_album(red),
        Err(SessionError::BuiltinAlbum(_))
    ));
    assert!(matches!(
        f.s.update_smart_album(red, "X", Filter::default()),
        Err(SessionError::BuiltinAlbum(_))
    ));
    f.s.set_album_mirror(red, true).unwrap();
    let user =
        f.s.create_smart_album("Mine", Filter::default(), Sort::default())
            .unwrap();
    assert!(f.s.album(user).unwrap().builtin.is_none());
    f.s.update_smart_album(
        user,
        "Red ones",
        Filter {
            tags: vec![ColorTag::Red],
            ..Filter::default()
        },
    )
    .unwrap();
    assert_eq!(f.s.album_variants(user).unwrap(), vec![v]);
    assert!(matches!(
        f.s.update_smart_album(user, " ", Filter::default()),
        Err(SessionError::InvalidName(_))
    ));
    let manual = f.s.create_album("Manual").unwrap();
    assert!(
        f.s.update_smart_album(manual, "M", Filter::default())
            .is_err()
    );

    // Survives reopen; a built-in dropped from albums.json comes back on ensure.
    f.s = Session::open(&f.root).unwrap();
    assert!(f.s.album(red).unwrap().mirror);
    assert_eq!(f.s.album(user).unwrap().name, "Red ones");
    let path = f.root.join(".lt/settings/albums.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["albums"]
        .as_array_mut()
        .unwrap()
        .retain(|a| a["builtin"] != "picked");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    f.s = Session::open(&f.root).unwrap();
    assert!(
        f.s.albums()
            .iter()
            .all(|a| a.builtin.as_deref() != Some("picked"))
    );
    assert_eq!(f.s.ensure_builtin_albums().unwrap(), 1);
    f.s.delete_album(user).unwrap();
    // Old albums.json without the new fields still loads.
    let legacy: Album = serde_json::from_str(
        r#"{"id":1,"name":"Old","kind":"smart","filter":{"min_rating":3},"sort":{}}"#,
    )
    .unwrap();
    assert!(legacy.builtin.is_none() && !legacy.mirror);
}
