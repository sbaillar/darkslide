#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ds_core::FileHash;
use ds_core::adjust::WhiteBalance;
use ds_core::geom::{NormRect, Orientation};
use ds_session::{AlbumKind, ImageMetaLite, VariantInfo};

use crate::journal::Journal;

use super::*;
use crate::criteria::parse_criteria;
use crate::fixtures::{self, FakeWb};
use crate::read::read_session;

struct Env {
    _dir: tempfile::TempDir,
    folder: PathBuf,
    store: PathBuf,
}

/// A synthetic C1 session folder and the Darkslide store of its in-place
/// session, holding what the in-place walk lists (not `Trash/`).
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Shoot");
    fixtures::build_session(&folder, &|rel| format!("raw bytes of {rel}").into_bytes());
    let store = dir.path().join("store");
    let mut s = Session::create_external(&store, "Shoot", &folder).unwrap();
    for (rel, o) in [
        ("a.ARW", Orientation::Normal),
        ("b.ARW", Orientation::Rotate270),
        ("Selects/c.ARW", Orientation::Normal),
    ] {
        let meta = ImageMetaLite {
            width: 6000,
            height: 4000,
            orientation: o,
            ..ImageMetaLite::default()
        };
        s.add_original(
            &format!("Capture/{rel}"),
            FileHash([rel.len() as u8; 32]),
            meta,
        )
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

    fn run(&self, opts: ImportOptions) -> ImportReport {
        let c1 = read_session(&self.folder).unwrap();
        import(&mut self.session(), &c1, &FakeWb(true), opts).unwrap()
    }

    fn write_cos(&self, sub: &str, name: &str, xml: &str) {
        fs::write(
            self.folder
                .join(format!("{sub}CaptureOne/Settings153/{name}.cos")),
            xml,
        )
        .unwrap();
    }
}

const REAL: ImportOptions = ImportOptions {
    dry_run: false,
    overwrite_edits: false,
};
const DRY: ImportOptions = ImportOptions {
    dry_run: true,
    overwrite_edits: false,
};
const OVERWRITE: ImportOptions = ImportOptions {
    dry_run: false,
    overwrite_edits: true,
};

fn variant(s: &Session, rel: &str, i: usize) -> VariantId {
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|im| im.rel_path.as_str() == format!("Capture/{rel}"))
        .unwrap();
    s.variants(img.id).unwrap()[i]
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
                let bytes = fs::read(&p).unwrap();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(
                    rel,
                    (
                        bytes.len() as u64,
                        blake3::hash(&bytes).to_hex().to_string(),
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn store_files(dir: &Path) -> BTreeMap<String, (u64, String)> {
    let mut all = checksums(dir);
    // SQLite's WAL/shm and the index may be touched by opening alone.
    all.retain(|k, _| !k.ends_with("-wal") && !k.ends_with("-shm") && !k.ends_with(".dsession"));
    all
}

#[test]
fn first_import_maps_organization_edits_and_albums() {
    let e = env();
    let r = e.run(REAL);
    assert!(!r.dry_run);
    assert_eq!(
        (
            r.images,
            r.matched,
            r.in_selects,
            r.in_trash,
            r.outside_folder
        ),
        (3, 3, 1, 1, 1)
    );
    assert_eq!(
        (r.variants, r.variants_created),
        (4, 1),
        "a's second C1 variant becomes a new variant"
    );
    assert_eq!(r.not_found.len(), 0);
    assert_eq!(
        (r.rated, r.tagged, r.picked, r.keyworded, r.edits_mapped),
        (3, 2, 1, 1, 2)
    );
    assert_eq!(r.conflicts.len(), 0);
    let s = e.session();

    let a1 = s.variant(variant(&s, "a.ARW", 0)).unwrap();
    assert_eq!(a1.rating.stars(), 4);
    assert_eq!(a1.color_tag, ColorTag::Red);
    assert_eq!(a1.keywords, vec!["Beach & Sun", "Family"]);
    let adj = s.load_adjustments(a1.id).unwrap();
    assert_eq!(adj.exposure.exposure_ev, 0.5);
    assert_eq!(adj.hdr.highlights, 20.0);
    assert!(matches!(adj.white_balance, WhiteBalance::Custom { .. }));
    assert_eq!(
        adj.crop.rect,
        NormRect {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5
        }
    );
    assert_eq!(adj.crop.angle_deg, -2.0);
    let a2 = s.variant(variant(&s, "a.ARW", 1)).unwrap();
    assert_eq!(a2.color_tag, ColorTag::Green);
    assert!(
        !a2.edited,
        "the new variant starts from defaults, not a copy of the first"
    );

    // b has no .cos: rating, brightness and a portrait crop from the database.
    let b = s.variant(variant(&s, "b.ARW", 0)).unwrap();
    assert_eq!(b.rating.stars(), 3);
    let badj = s.load_adjustments(b.id).unwrap();
    assert_eq!(badj.exposure.brightness, 10.0);
    // Sensor centre (2000, 3000) of 6000×4000, EXIF 8 → (0.75, 0.6667); 2000×3000 of
    // 4000×6000 = 0.5 × 0.5.
    let rect = badj.crop.rect;
    let want = [0.5, 0.416_666, 0.5, 0.5];
    let got = [rect.x, rect.y, rect.w, rect.h];
    assert!(
        got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-4),
        "{rect:?}"
    );

    let c = s.variant(variant(&s, "Selects/c.ARW", 0)).unwrap();
    assert_eq!(
        (c.rating.stars(), c.pick),
        (2, PickState::Picked),
        "Selects = picked"
    );
    assert_eq!(a1.pick, PickState::Unflagged);
}

#[test]
fn first_import_creates_albums() {
    let e = env();
    let r = e.run(REAL);
    let s = e.session();
    let a1 = s.variant(variant(&s, "a.ARW", 0)).unwrap();
    let a2 = s.variant(variant(&s, "a.ARW", 1)).unwrap();
    let b = s.variant(variant(&s, "b.ARW", 0)).unwrap();
    // Albums: "Trip / Favourites" (a's first variant and b; e is outside), "Picks" (a's second variant).
    let albums: Vec<_> = s.albums().iter().filter(|a| a.builtin.is_none()).collect();
    assert_eq!(albums.len(), 2);
    let fav = albums
        .iter()
        .find(|a| a.name == "Trip / Favourites")
        .unwrap();
    assert_eq!(
        fav.kind,
        AlbumKind::Manual {
            members: vec![a1.id, b.id]
        }
    );
    let picks = albums.iter().find(|a| a.name == "Picks").unwrap();
    assert_eq!(
        picks.kind,
        AlbumKind::Manual {
            members: vec![a2.id]
        }
    );
    assert_eq!(r.albums_created, 2);
    let skipped: Vec<_> = r
        .collections_skipped
        .iter()
        .map(|k| (k.name.as_str(), k.reason.as_str()))
        .collect();
    assert_eq!(skipped.len(), 2);
    assert!(
        skipped[0].0 == "Five Stars" && skipped[0].1.contains("5 stars"),
        "{skipped:?}"
    );
    assert!(
        skipped[1].0 == "All Images" && skipped[1].1.contains("every image"),
        "{skipped:?}"
    );
}

#[test]
fn unmapped_settings_are_reported_with_reasons() {
    let e = env();
    let r = e.run(DRY);
    assert_eq!(
        (r.edited_in_c1, r.with_unmapped),
        (2, 1),
        "a1 (.cos) and b (database, all mapped)"
    );
    let keys: Vec<_> = r.unmapped.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["FilmGrainAmount", "Shadow"]);
    assert!(r.unmapped["Shadow"].0.contains("per-channel"));
    assert_eq!(r.unmapped["FilmGrainAmount"].1, 1);
    assert_eq!(r.mapped_keys["Exposure"], 1);
    for k in [
        "Clarity",
        "GradationCurve",
        "UsmAmount",
        "Vignetting",
        "Highlight",
    ] {
        assert_eq!(r.mapped_keys[k], 1, "{k}");
    }
    assert_eq!(r.mapped_keys["Crop"], 2);
    assert_eq!(r.mapped_keys["Brightness"], 2);
}

#[test]
fn dry_run_writes_nothing_and_predicts_the_real_run() {
    let e = env();
    let before = store_files(&e.store);
    let dry = e.run(DRY);
    assert!(dry.dry_run);
    assert_eq!(store_files(&e.store), before, "a dry run writes nothing");
    assert!(!state::path(&e.store).exists());
    let real = e.run(REAL);
    let mut predicted = dry.clone();
    predicted.dry_run = false;
    assert_eq!(predicted, real, "same counts, same reasons");
}

#[test]
fn rerunning_updates_instead_of_duplicating() {
    let e = env();
    e.run(REAL);
    let s = e.session();
    let snapshot = |s: &Session| {
        let mut v: Vec<_> = s
            .all_variants()
            .unwrap()
            .into_iter()
            .map(|id| (s.variant(id).unwrap(), s.load_adjustments(id).unwrap()))
            .collect();
        v.sort_by_key(|(i, _)| i.id);
        (v, s.albums().to_vec())
    };
    let first = snapshot(&s);
    drop(s);
    let again = e.run(REAL);
    assert_eq!(
        (again.variants, again.variants_created, again.in_sync),
        (4, 0, 4)
    );
    assert_eq!(
        (
            again.rated,
            again.tagged,
            again.picked,
            again.keyworded,
            again.edits_mapped
        ),
        (0, 0, 0, 0, 0)
    );
    assert_eq!((again.albums_created, again.albums_updated), (0, 0));
    assert_eq!(snapshot(&e.session()), first);
    // The same through a dry run.
    let dry = e.run(DRY);
    assert_eq!((dry.in_sync, dry.rated, dry.albums_created), (4, 0, 0));
}

#[test]
fn darkslide_edits_survive_and_conflicts_need_overwrite() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    s.set_rating(&[a1], Rating::new(1).unwrap()).unwrap();
    let mut adj = s.load_adjustments(a1).unwrap();
    adj.exposure.exposure_ev = -1.0;
    s.save_adjustments(a1, &adj).unwrap();
    drop(s);

    // C1 unchanged: Darkslide's edits are kept, silently.
    let r = e.run(REAL);
    assert_eq!(r.kept_darkslide, 2);
    assert_eq!(r.conflicts.len(), 0);
    let s = e.session();
    assert_eq!(s.variant(a1).unwrap().rating.stars(), 1);
    assert_eq!(s.load_adjustments(a1).unwrap().exposure.exposure_ev, -1.0);
    drop(s);

    // C1 changed too: a conflict, still not overwritten...
    e.write_cos(
        "",
        "a.ARW",
        &fixtures::COS_A
            .replace(
                r#"<E K="Basic_Rating" V="4" />"#,
                r#"<E K="Basic_Rating" V="5" />"#,
            )
            .replace(
                r#"<E K="Exposure" V="0.5" />"#,
                r#"<E K="Exposure" V="0.7" />"#,
            ),
    );
    let dry = e.run(DRY);
    let r = e.run(REAL);
    assert_eq!(dry.conflicts, r.conflicts);
    let fields: Vec<_> = r
        .conflicts
        .iter()
        .map(|c| (c.file.as_str(), c.field))
        .collect();
    assert_eq!(
        fields,
        vec![("a.ARW", Field::Rating), ("a.ARW", Field::Adjustments)]
    );
    assert_eq!(r.conflicts[0].variant, a1);
    let s = e.session();
    assert_eq!(s.variant(a1).unwrap().rating.stars(), 1);
    drop(s);

    // ...until the user says so.
    let r = e.run(OVERWRITE);
    assert_eq!((r.overwritten, r.rated, r.edits_mapped), (2, 1, 1));
    assert_eq!(r.conflicts.len(), 0);
    let s = e.session();
    assert_eq!(s.variant(a1).unwrap().rating.stars(), 5);
    let adj = s.load_adjustments(a1).unwrap();
    assert_eq!(adj.exposure.exposure_ev, 0.7);
    drop(s);
    assert_eq!(e.run(REAL).in_sync, 4, "settled");
}

#[test]
fn c1_changes_flow_in_when_darkslide_is_untouched() {
    let e = env();
    e.run(REAL);
    // Back to unrated, untagged and unedited in C1; colour on the second variant changes.
    let reset = fixtures::COS_A
        .replace(r#"<E K="Basic_Rating" V="4" />"#, "")
        .replace(r#"<E K="Color_Tag_Index" V="1" />"#, "")
        .replace(
            r#"<E K="Exposure" V="0.5" />"#,
            r#"<E K="Exposure" V="0" />"#,
        )
        .replace(
            r#"<E K="Color_Tag_Index" V="4" />"#,
            r#"<E K="Color_Tag_Index" V="5" />"#,
        );
    e.write_cos("", "a.ARW", &reset);
    let r = e.run(REAL);
    assert_eq!((r.rated, r.tagged, r.edits_mapped), (1, 2, 1));
    let s = e.session();
    let a1 = s.variant(variant(&s, "a.ARW", 0)).unwrap();
    assert_eq!((a1.rating.stars(), a1.color_tag), (0, ColorTag::None));
    assert_eq!(s.load_adjustments(a1.id).unwrap().exposure.exposure_ev, 0.0);
    assert_eq!(
        s.variant(variant(&s, "a.ARW", 1)).unwrap().color_tag,
        ColorTag::Blue
    );
}

#[test]
fn edits_made_before_the_first_import_are_conflicts() {
    let e = env();
    let mut s = e.session();
    let b = variant(&s, "b.ARW", 0);
    let c = variant(&s, "Selects/c.ARW", 0);
    s.set_rating(&[b], Rating::new(2).unwrap()).unwrap();
    s.set_color_tag(&[b], ColorTag::Blue).unwrap();
    s.set_pick(&[c], PickState::Rejected).unwrap();
    drop(s);
    let r = e.run(REAL);
    let fields: Vec<_> = r
        .conflicts
        .iter()
        .map(|k| (k.file.as_str(), k.field))
        .collect();
    assert_eq!(
        fields,
        vec![("b.ARW", Field::Rating), ("Selects/c.ARW", Field::Pick)]
    );
    let s = e.session();
    let bi = s.variant(b).unwrap();
    assert_eq!(
        (bi.rating.stars(), bi.color_tag),
        (2, ColorTag::Blue),
        "C1 has no tag for b: Blue stays, no conflict"
    );
    assert_eq!(s.variant(c).unwrap().pick, PickState::Rejected);
}

#[test]
fn the_c1_folder_is_never_modified() {
    let e = env();
    let before = checksums(&e.folder);
    e.run(DRY);
    e.run(REAL);
    e.run(REAL);
    e.run(OVERWRITE);
    assert_eq!(checksums(&e.folder), before, "byte-identical, no new files");
}

#[test]
fn only_the_folders_own_in_place_session() {
    let e = env();
    let c1 = read_session(&e.folder).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut internal = Session::create(&dir.path().join("plain"), "Plain").unwrap();
    assert!(matches!(
        import(&mut internal, &c1, &FakeWb(true), REAL),
        Err(C1Error::WrongSession(_))
    ));
    let other = tempfile::tempdir().unwrap();
    let mut elsewhere =
        Session::create_external(&dir.path().join("ext"), "X", other.path()).unwrap();
    let err = import(&mut elsewhere, &c1, &FakeWb(true), REAL).unwrap_err();
    assert!(err.to_string().contains("Shoot"), "{err}");
}

#[test]
fn photos_missing_from_darkslide_are_listed() {
    let e = env();
    fs::write(e.folder.join("new.ARW"), b"raw").unwrap();
    e.write_cos("", "new.ARW", &fixtures::COS_D.replace("VAR-D1", "VAR-N1"));
    let r = e.run(DRY);
    assert_eq!(r.not_found, vec!["new.ARW"]);
    assert_eq!((r.images, r.matched), (4, 3));
}

#[test]
fn deleted_albums_and_variants_are_not_recreated() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let fav = s
        .albums()
        .iter()
        .find(|a| a.name == "Trip / Favourites")
        .unwrap()
        .id;
    s.delete_album(fav).unwrap();
    let picks = s.albums().iter().find(|a| a.name == "Picks").unwrap().id;
    s.remove_from_album(picks, &s.album_variants(picks).unwrap())
        .unwrap();
    s.delete_variant(variant(&s, "a.ARW", 1)).unwrap();
    drop(s);
    let r = e.run(REAL);
    assert_eq!(r.deleted_in_darkslide, 1);
    assert_eq!((r.variants, r.variants_created), (3, 0));
    assert!(
        r.collections_skipped
            .iter()
            .any(|k| k.name == "Trip / Favourites" && k.reason.contains("deleted"))
    );
    let s = e.session();
    assert!(s.albums().iter().all(|a| a.name != "Trip / Favourites"));
    assert_eq!(
        s.variants(s.variant(variant(&s, "a.ARW", 0)).unwrap().image)
            .unwrap()
            .len(),
        1
    );
    // "Picks" lost its only member (a variant now deleted): nothing to add back.
    assert_eq!(r.albums_updated, 0);
}

#[test]
fn album_members_removed_by_the_user_are_added_back_only_when_c1_lists_them() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let fav = s
        .albums()
        .iter()
        .find(|a| a.name == "Trip / Favourites")
        .unwrap()
        .id;
    let b = variant(&s, "b.ARW", 0);
    s.remove_from_album(fav, &[b]).unwrap();
    drop(s);
    let r = e.run(REAL);
    assert_eq!(r.albums_updated, 1);
    assert!(e.session().album_variants(fav).unwrap().contains(&b));
}

#[test]
fn an_unreadable_state_starts_over_safely() {
    let e = env();
    e.run(REAL);
    fs::write(state::path(&e.store), b"{garbage").unwrap();
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    s.set_rating(&[a1], Rating::new(1).unwrap()).unwrap();
    drop(s);
    let r = e.run(REAL);
    assert!(
        r.warnings.iter().any(|w| w.contains(state::STATE_FILE)),
        "{:?}",
        r.warnings
    );
    assert_eq!(
        r.conflicts.len(),
        1,
        "without a record the Darkslide edit is a conflict"
    );
    assert_eq!(e.session().variant(a1).unwrap().rating.stars(), 1);
    // Albums aren't known any more either: created again (the old ones stay).
    assert_eq!(r.albums_created, 2);
}

#[test]
fn smart_album_translation() {
    let parse = |xml: &str| parse_criteria(xml).unwrap();
    let cond = |key: &str, op: i64, crit: &str| {
        format!(
            r#"<MatchOperator Kind="AND"><Condition><Key>{key}</Key><Operator>{op}</Operator><Criterion>{crit}</Criterion></Condition></MatchOperator>"#
        )
    };
    let f = translate(&parse(&cond("IB_I_BASIC_RATING", 0, "3"))).unwrap();
    assert_eq!(
        (f.min_rating, f.max_rating),
        (Rating::new(3), Rating::new(3))
    );
    let unrated = translate(&parse(&cond("IB_I_BASIC_RATING", 0, "0"))).unwrap();
    assert_eq!(
        (unrated.min_rating, unrated.max_rating),
        (None, Rating::new(0))
    );
    let builtins = builtin_smart_folders(&[]);
    assert!(
        builtins
            .iter()
            .any(|(k, _, f)| k == "unrated" && *f == unrated)
    );
    assert!(
        translate(&parse(&cond("IB_I_BASIC_RATING", 0, "9")))
            .unwrap_err()
            .contains("0–5")
    );
    assert!(
        translate(&parse(&cond("IB_I_BASIC_RATING", 4, "3")))
            .unwrap_err()
            .contains("operator 4")
    );
    assert!(
        translate(&parse(&cond("IB_S_CONTENT_KEYWORDS", 0, "x")))
            .unwrap_err()
            .contains("IB_S_CONTENT_KEYWORDS")
    );
    let or = r#"<MatchOperator Kind="OR"><Condition><Key>A</Key><Operator>0</Operator><Criterion>1</Criterion></Condition><Condition><Key>B</Key><Operator>0</Operator><Criterion>1</Criterion></Condition></MatchOperator>"#;
    assert!(translate(&parse(or)).unwrap_err().contains("OR"));
}

#[test]
fn smart_albums_without_a_builtin_match_are_reported() {
    let e = env();
    let mut c1 = read_session(&e.folder).unwrap();
    c1.collections[0].criteria = parse_criteria(
        r#"<MatchOperator Kind="AND"><Condition><Key>IB_S_CAMERA_MAKE</Key><Operator>0</Operator><Criterion>SONY</Criterion></Condition></MatchOperator>"#,
    );
    c1.collections[1].name = "  ".into();
    c1.collections[2].name = String::new();
    c1.collections[2].parents.clear();
    let r = import(&mut e.session(), &c1, &FakeWb(true), REAL).unwrap();
    assert!(r.collections_skipped[0].reason.contains("IB_S_CAMERA_MAKE"));
    let s = e.session();
    assert!(
        s.albums().iter().any(|a| a.name == "Capture One album"),
        "blank C1 names get a valid one"
    );
}

#[test]
fn decisions() {
    use Decision::*;
    assert_eq!(decide(&1, &1, None, &0), InSync);
    assert_eq!(decide(&2, &3, Some(&3), &0), Keep);
    assert_eq!(decide(&3, &4, Some(&3), &0), Write);
    assert_eq!(decide(&2, &4, Some(&3), &0), Conflict);
    assert_eq!(decide(&2, &0, None, &0), Skip);
    assert_eq!(decide(&0, &4, None, &0), Write);
    assert_eq!(decide(&2, &4, None, &0), Conflict);
    assert_eq!(
        normalized(&[" a ".into(), "a".into(), String::new(), "b".into()]),
        vec!["a", "b"]
    );
}

/// Every variant's organization and adjustments, and the albums.
fn state_of(s: &Session) -> (Vec<(VariantInfo, Adjustments)>, Vec<ds_session::Album>) {
    let mut v: Vec<_> = s
        .all_variants()
        .unwrap()
        .into_iter()
        .map(|id| (s.variant(id).unwrap(), s.load_adjustments(id).unwrap()))
        .collect();
    v.sort_by_key(|(i, _)| i.id);
    (v, s.albums().to_vec())
}

#[test]
fn revert_takes_the_import_back_and_a_reimport_redoes_it() {
    let e = env();
    let before = state_of(&e.session());
    assert!(!crate::can_revert(&e.session()));
    assert_eq!(crate::revert(&mut e.session()).unwrap(), None);
    let dry = e.run(DRY);
    assert!(
        !crate::can_revert(&e.session()),
        "a dry run journals nothing"
    );
    let first = e.run(REAL);
    assert_eq!(first.variants_created, 1);
    assert_eq!(first.albums_created, 2);
    assert!(crate::can_revert(&e.session()));
    let imported = state_of(&e.session());
    assert_ne!(imported, before);

    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(
        r,
        crate::RevertReport {
            restored: 3, // a1, b and the picked c
            kept: 0,
            variants_removed: 1,
            albums_removed: 2,
            albums_trimmed: 0,
        }
    );
    assert_eq!(state_of(&e.session()), before, "back to before the import");
    assert!(!crate::can_revert(&e.session()));
    // The import record is back to before, so importing again does it all again.
    let mut again = e.run(REAL);
    again.dry_run = true;
    assert_eq!(again, dry);
}

#[test]
fn revert_keeps_what_changed_in_darkslide_after_the_import() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    let a2 = variant(&s, "a.ARW", 1);
    s.set_rating(&[a1], Rating::new(1).unwrap()).unwrap();
    s.set_color_tag(&[a2], ColorTag::Blue).unwrap(); // the created variant
    let album = s.albums().iter().find(|a| a.builtin.is_none()).unwrap().id;
    let c = variant(&s, "Selects/c.ARW", 0);
    s.add_to_album(album, &[c]).unwrap();
    drop(s);

    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(
        (r.kept, r.variants_removed, r.albums_removed),
        (3, 0, 1),
        "{r:?}"
    );
    let s = e.session();
    let info = s.variant(a1).unwrap();
    assert_eq!(info.rating.stars(), 1, "changed since the import: kept");
    assert_eq!(info.color_tag, ColorTag::None, "untouched since: reverted");
    assert!(s.load_adjustments(a1).unwrap().is_identity());
    assert_eq!(s.variant(a2).unwrap().color_tag, ColorTag::Blue);
    assert_eq!(
        s.album_variants(album).unwrap(),
        vec![c],
        "the album the user added to stays, without the imported members"
    );
}

#[test]
fn reimports_fold_into_one_revert() {
    let e = env();
    let before = state_of(&e.session());
    e.run(REAL);
    e.write_cos(
        "",
        "a.ARW",
        &fixtures::COS_A.replace(
            r#"<E K="Exposure" V="0.5" />"#,
            r#"<E K="Exposure" V="0.9" />"#,
        ),
    );
    assert_eq!(e.run(REAL).edits_mapped, 1);
    let s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    assert_eq!(s.load_adjustments(a1).unwrap().exposure.exposure_ev, 0.9);
    drop(s);
    crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(state_of(&e.session()), before);
}

#[test]
fn revert_trims_albums_that_existed_and_survives_a_bad_journal() {
    let e = env();
    let mut s = e.session();
    let mine = s.create_album("Mine").unwrap();
    let a1 = variant(&s, "a.ARW", 0);
    let b = variant(&s, "b.ARW", 0);
    s.add_to_album(mine, &[b]).unwrap();
    s.add_to_album(mine, &[a1]).unwrap();
    // As if an import had added a1 to an album the user made.
    let mut j = Journal::new(State::default());
    j.added_members(mine, &[a1]);
    j.added_members(mine, &[a1]);
    crate::journal::record(&e.store, j).unwrap();
    let r = crate::revert(&mut s).unwrap().unwrap();
    assert_eq!((r.albums_trimmed, r.albums_removed), (1, 0));
    assert_eq!(s.album_variants(mine).unwrap(), vec![b]);

    // An unreadable journal: a revert reports it; the next import replaces it.
    let p = crate::journal::path(&e.store);
    fs::write(&p, b"{oops").unwrap();
    assert!(crate::revert(&mut s).is_err());
    drop(s);
    e.run(REAL);
    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(r.variants_removed, 1);
    // A journal that names variants and albums deleted since is still revertible.
    e.run(REAL);
    let mut s = e.session();
    let created = variant(&s, "a.ARW", 1);
    s.delete_variant(created).unwrap();
    let albums: Vec<_> = s
        .albums()
        .iter()
        .filter(|a| a.builtin.is_none() && a.id != mine)
        .map(|a| a.id)
        .collect();
    for a in albums {
        s.delete_album(a).unwrap();
    }
    let r = crate::revert(&mut s).unwrap().unwrap();
    assert_eq!((r.variants_removed, r.albums_removed), (0, 0));
    assert!(s.load_adjustments(a1).unwrap().is_identity());
}

// ------------------------------------------------- review round 1 (H/M/L) ---

/// The sidecar of `rel`'s first variant.
fn sidecar_path(e: &Env, rel: &str) -> PathBuf {
    let r = ds_session::RelPath::new(&format!("Capture/{rel}")).unwrap();
    e.store.join(r.sidecar_rel(1))
}

fn edit(s: &mut Session, v: VariantId, f: impl FnOnce(&mut Adjustments)) {
    let mut a = s.load_adjustments(v).unwrap();
    f(&mut a);
    s.save_adjustments(v, &a).unwrap();
}

#[test]
fn h1_revert_keeps_a_created_variant_edited_after_the_import() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let a2 = variant(&s, "a.ARW", 1); // created with only a colour tag
    edit(&mut s, a2, |a| a.exposure.exposure_ev = 1.7);
    drop(s);
    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!((r.variants_removed, r.kept), (0, 1), "{r:?}");
    let s = e.session();
    assert_eq!(s.load_adjustments(a2).unwrap().exposure.exposure_ev, 1.7);
}

#[test]
fn h2_revert_keeps_a_created_variant_rated_after_the_import() {
    let e = env();
    // VAR-A2 with no settings at all: created, but no field written.
    e.write_cos(
        "",
        "a.ARW",
        &fixtures::COS_A.replace(r#"<E K="Color_Tag_Index" V="4" />"#, ""),
    );
    assert_eq!(e.run(REAL).variants_created, 1);
    let mut s = e.session();
    let a2 = variant(&s, "a.ARW", 1);
    s.set_rating(&[a2], Rating::new(5).unwrap()).unwrap();
    drop(s);
    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!((r.variants_removed, r.kept), (0, 1), "{r:?}");
    assert_eq!(e.session().variant(a2).unwrap().rating.stars(), 5);
    // Untouched since: removed.
    let e = env();
    e.write_cos(
        "",
        "a.ARW",
        &fixtures::COS_A.replace(r#"<E K="Color_Tag_Index" V="4" />"#, ""),
    );
    e.run(REAL);
    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(r.variants_removed, 1);
}

#[test]
fn h3_revert_keeps_settings_changed_between_imports() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    edit(&mut s, a1, |a| a.presence.vibrance = 30.0);
    drop(s);
    e.write_cos(
        "",
        "a.ARW",
        &fixtures::COS_A.replace(
            r#"<E K="Exposure" V="0.5" />"#,
            r#"<E K="Exposure" V="0.9" />"#,
        ),
    );
    assert_eq!(e.run(REAL).edits_mapped, 1);
    crate::revert(&mut e.session()).unwrap().unwrap();
    let mut want = Adjustments::default();
    want.presence.vibrance = 30.0;
    assert_eq!(e.session().load_adjustments(a1).unwrap(), want);
}

#[test]
fn h3_revert_is_per_setting() {
    let e = env();
    e.run(REAL);
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    edit(&mut s, a1, |a| a.exposure.contrast = 55.0);
    drop(s);
    let r = crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(r.kept, 1, "{r:?}");
    let a = e.session().load_adjustments(a1).unwrap();
    assert_eq!(a.exposure.contrast, 55.0, "changed since: kept");
    assert_eq!(a.exposure.exposure_ev, 0.0, "the rest of the import goes");
    assert_eq!(a.hdr.highlights, 0.0);
    let mut only = Adjustments::default();
    only.exposure.contrast = 55.0;
    assert_eq!(a, only);
}

#[test]
fn m1_a_created_variant_starts_from_defaults() {
    let e = env();
    let mut s = e.session();
    let a1 = variant(&s, "a.ARW", 0);
    edit(&mut s, a1, |a| a.presence.vibrance = 40.0);
    drop(s);
    e.run(REAL);
    let s = e.session();
    let a2 = variant(&s, "a.ARW", 1);
    assert_eq!(s.load_adjustments(a2).unwrap(), Adjustments::default());
    assert_eq!(s.load_adjustments(a1).unwrap().presence.vibrance, 40.0);
}

#[test]
fn m2_a_corrupt_sidecar_is_a_warning_for_that_photo() {
    let e = env();
    fs::write(sidecar_path(&e, "a.ARW"), b"{oops").unwrap();
    let c1 = read_session(&e.folder).unwrap();
    let r = import(&mut e.session(), &c1, &FakeWb(true), REAL).unwrap();
    assert!(
        r.warnings.iter().any(|w| w.contains("a.ARW")),
        "{:?}",
        r.warnings
    );
    let s = e.session();
    assert_eq!(
        s.variant(variant(&s, "b.ARW", 0)).unwrap().rating.stars(),
        3
    );
}

#[test]
fn m2_the_journal_covers_a_failed_import() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    let before = state_of(&e.session());
    // c's sidecar folder can't be written: the import fails part-way.
    let dir = sidecar_path(&e, "Selects/c.ARW")
        .parent()
        .unwrap()
        .to_owned();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let c1 = read_session(&e.folder).unwrap();
    let res = import(&mut e.session(), &c1, &FakeWb(true), REAL);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(res.is_err());
    assert!(crate::can_revert(&e.session()), "journaled before writing");
    crate::revert(&mut e.session()).unwrap().unwrap();
    assert_eq!(state_of(&e.session()), before);
}

#[test]
fn m3_dry_run_and_real_run_agree_on_a_read_only_photo() {
    let e = env();
    let p = sidecar_path(&e, "a.ARW");
    let mut j: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    j["schema_version"] = serde_json::json!(ds_core::SIDECAR_SCHEMA_VERSION + 1);
    fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
    e.session().sync_index().unwrap();
    let dry = e.run(DRY);
    let mut real = e.run(REAL);
    assert_eq!(
        (real.read_only, real.variants_created),
        (2, 0),
        "a newer-build photo gets no new variant"
    );
    real.dry_run = true;
    assert_eq!(real, dry);
}

#[test]
fn l2_a_replaced_c1_database_does_not_reuse_album_keys() {
    let e = env();
    e.run(REAL);
    // As if the C1 database were replaced: the record names another session.
    let p = state::path(&e.store);
    let mut j: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    j["c1_session"] = serde_json::json!("ANOTHER-SESSION");
    // A key that, in the new database, would point at an album the user owns.
    let mut s = e.session();
    let mine = s.create_album("Mine").unwrap();
    drop(s);
    j["albums"]["pk:999"] = serde_json::json!(mine.0);
    fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
    let r = e.run(REAL);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("different Capture One session")),
        "{:?}",
        r.warnings
    );
    assert!(
        e.session().album_variants(mine).unwrap().is_empty(),
        "the user's album is not filled"
    );
    let names: Vec<String> = e
        .session()
        .albums()
        .iter()
        .filter(|a| a.builtin.is_none())
        .map(|a| a.name.clone())
        .collect();
    assert_eq!(
        names.len(),
        3,
        "albums matched by name, not duplicated: {names:?}"
    );
}

#[test]
fn l3_a_cancelled_import_writes_nothing() {
    let e = env();
    let before = store_files(&e.store);
    let c1 = read_session(&e.folder).unwrap();
    let r = crate::import_cancellable(&mut e.session(), &c1, &FakeWb(true), REAL, &|| true);
    assert!(matches!(r, Err(C1Error::Cancelled)));
    assert_eq!(store_files(&e.store), before);
    assert!(!crate::can_revert(&e.session()));
}

#[test]
fn l4_an_oversized_settings_file_is_skipped_with_a_warning() {
    let e = env();
    let big = format!(
        "{}<!--{}-->",
        fixtures::COS_A,
        " ".repeat(crate::read::MAX_COS_BYTES as usize)
    );
    e.write_cos("", "a.ARW", &big);
    let c1 = read_session(&e.folder).unwrap();
    assert!(
        c1.warnings.iter().any(|w| w.contains("larger than")),
        "{:?}",
        c1.warnings
    );
}
