//! IPTC metadata (title, caption, credits…): sidecar round trip, old sidecars,
//! undo, the index column and search, the v6 → v7 index upgrade.

use std::fs;

use lt_core::{FileHash, IptcMeta, SIDECAR_SCHEMA_VERSION};

use crate::*;

fn session() -> (tempfile::TempDir, std::path::PathBuf, Session) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Shoot");
    let s = Session::create(&root, "Shoot").unwrap();
    (dir, root, s)
}

fn add(root: &std::path::Path, s: &mut Session, name: &str) -> VariantId {
    let rel = format!("Capture/{name}");
    fs::write(root.join(&rel), name.as_bytes()).unwrap();
    let h = FileHash(*blake3::hash(name.as_bytes()).as_bytes());
    let id = s.add_original(&rel, h, ImageMetaLite::default()).unwrap();
    s.variants(id).unwrap()[0]
}

fn full() -> IptcMeta {
    IptcMeta {
        title: "Morning Surf".into(),
        caption: "Surfers at dawn, Skjalfandi Bay".into(),
        creator: "Alexa".into(),
        copyright: "© 2026 Alexa".into(),
        credit: "Luxtable".into(),
        headline: "Big swell".into(),
        city: "Husavik".into(),
        state: "Nordurland".into(),
        country: "Iceland".into(),
        sublocation: "Skjalfandi Bay".into(),
        instructions: "No crops".into(),
        usage_terms: "Editorial use only".into(),
    }
}

#[test]
fn sidecar_round_trip_and_old_sidecars_load() {
    let mut sc = crate::sidecar::tests::sample();
    sc.iptc = full();
    let back = Sidecar::from_json(&sc.to_json()).unwrap().sidecar;
    assert_eq!(back.iptc, full());

    // A sidecar from before IPTC fields (same schema version) loads with none set.
    let mut v: serde_json::Value = serde_json::from_slice(&sc.to_json()).unwrap();
    v.as_object_mut().unwrap().remove("iptc");
    assert_eq!(v["schema_version"], SIDECAR_SCHEMA_VERSION);
    let old = Sidecar::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(old.sidecar.iptc.is_empty());
    assert!(!old.too_new);

    // Cleared fields are written (empty), so a rewrite over the file drops them.
    let mut cleared = sc.clone();
    cleared.iptc = IptcMeta::default();
    let base: serde_json::Value = serde_json::from_slice(&sc.to_json()).unwrap();
    let rewritten = cleared.to_json_preserving(Some(&base));
    assert!(
        Sidecar::from_json(&rewritten)
            .unwrap()
            .sidecar
            .iptc
            .is_empty()
    );
}

#[test]
fn set_iptc_persists_indexes_and_undoes_without_touching_the_edit() {
    let (_d, root, mut s) = session();
    let a = add(&root, &mut s, "a.ARW");
    let b = add(&root, &mut s, "b.ARW");
    let mut adj = lt_core::Adjustments::default();
    adj.exposure.exposure_ev = 0.7;
    s.save_adjustments(a, &adj).unwrap();
    let before_adj = s.load_adjustments(a).unwrap();

    let mut m = full();
    m.title = "  Morning Surf  ".into();
    s.set_iptc(&[a, b], &m).unwrap();
    assert_eq!(s.variant(a).unwrap().iptc, full(), "trimmed");
    assert_eq!(s.variant(b).unwrap().iptc, full());
    assert_eq!(
        s.load_adjustments(a).unwrap(),
        before_adj,
        "the edit is untouched"
    );

    // Persisted in the sidecar and back after a reopen.
    drop(s);
    let mut s = Session::open(&root).unwrap();
    assert_eq!(s.variant(a).unwrap().iptc, full());

    // Undo is one group step for both photos.
    let mut s2 = full();
    s2.title = "Other".into();
    s.set_iptc(&[a], &s2).unwrap();
    assert!(s.undo(a).unwrap());
    assert_eq!(s.variant(a).unwrap().iptc.title, "Morning Surf");
    assert!(s.redo(a).unwrap());
    assert_eq!(s.variant(a).unwrap().iptc.title, "Other");
    // Writing the same value again is not a change.
    let depth = s.undo_depth(a);
    s.set_iptc(&[a], &s2).unwrap();
    assert_eq!(s.undo_depth(a), depth);
    // A new variant starts with the photo's descriptive metadata.
    let v2 = s.new_variant(s.variant(a).unwrap().image).unwrap();
    assert_eq!(s.variant(v2).unwrap().iptc.title, "Other");
}

#[test]
fn search_matches_title_and_caption() {
    let (_d, root, mut s) = session();
    let a = add(&root, &mut s, "a.ARW");
    let b = add(&root, &mut s, "b.ARW");
    let c = add(&root, &mut s, "c.ARW");
    s.set_iptc(
        &[a],
        &IptcMeta {
            title: "Sunset over Sicily".into(),
            ..IptcMeta::default()
        },
    )
    .unwrap();
    s.set_iptc(
        &[b],
        &IptcMeta {
            caption: "Two surfers paddle out at DAWN".into(),
            city: "Vik".into(),
            ..IptcMeta::default()
        },
    )
    .unwrap();
    let find = |text: &str| {
        s.query(
            &Filter {
                text: Some(text.into()),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap()
    };
    assert_eq!(find("sicily"), [a]);
    assert_eq!(find("dawn"), [b]);
    assert!(
        find("vik").is_empty(),
        "only title and caption are searched"
    );
    assert_eq!(find("c.arw"), [c]);
}

#[test]
fn a_v6_index_is_upgraded_and_finds_titles_from_the_sidecars() {
    let (_d, root, mut s) = session();
    let a = add(&root, &mut s, "a.ARW");
    s.set_iptc(
        &[a],
        &IptcMeta {
            title: "Lighthouse".into(),
            ..IptcMeta::default()
        },
    )
    .unwrap();
    // Turn the index back into v6: no column, a search text without the title.
    let db = s.db_path();
    s.conn
        .execute_batch(
            "ALTER TABLE variants DROP COLUMN iptc; UPDATE variants SET search = '';
             PRAGMA user_version = 6;",
        )
        .unwrap();
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.open_report().recovery, None, "upgraded in place");
    assert_eq!(s.variant(a).unwrap().iptc.title, "Lighthouse");
    let hits = s
        .query(
            &Filter {
                text: Some("lighthouse".into()),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap();
    assert_eq!(hits, [a]);
    let v: i64 = s
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, INDEX_SCHEMA_VERSION);
    assert!(db.exists());
}

#[test]
fn new_originals_take_their_iptc_fields() {
    let (_d, root, mut s) = session();
    fs::write(root.join("Capture/x.ARW"), b"x").unwrap();
    let ids = s
        .add_new_originals(vec![NewOriginal {
            rel: "Capture/x.ARW".into(),
            hash: FileHash([1; 32]),
            content_hash: None,
            meta: ImageMetaLite::default(),
            replaced: Vec::new(),
            iptc: IptcMeta {
                creator: " Alexa ".into(),
                ..IptcMeta::default()
            },
        }])
        .unwrap();
    let v = s.variants(ids[0]).unwrap()[0];
    assert_eq!(s.variant(v).unwrap().iptc.creator, "Alexa");
}
