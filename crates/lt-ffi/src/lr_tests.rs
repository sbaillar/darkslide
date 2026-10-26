//! Lightroom import through the FFI: a folder of XMP sidecars beside fake
//! originals, opened in place; kind detection, dry run, import, revert,
//! presets, and the source folder left byte-identical.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use futures_executor::block_on;
use lt_core::geom::Orientation;
use lt_session::ImageMetaLite;

use super::*;
use crate::raw::RawMetadata;
use crate::session::{Probe, open_in_place};

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn fake_read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    fs::metadata(p).ok()?;
    let lite = ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    };
    Some((lite, RawMetadata::default()))
}

const FAKE: Probe = Probe {
    is_supported: arw,
    read: fake_read,
    volume_ok: |_| Ok(()),
    ranges: |_| None,
    untrusted_stat: |_| false,
    is_raw_name: arw,
};

const NS: &str = r#"xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/""#;

fn xmp(attrs: &str) -> String {
    format!(r"<x:xmpmeta {NS}><rdf:RDF><rdf:Description {attrs}/></rdf:RDF></x:xmpmeta>")
}

/// `a.ARW` (rated 4, +1 EV in the sidecar) and `b.ARW` (rated 2), plus a
/// sidecar for a photo that isn't there.
fn folder(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for f in ["a.ARW", "b.ARW"] {
        fs::write(dir.join(f), format!("raw {f}")).unwrap();
    }
    fs::write(
        dir.join("a.xmp"),
        xmp(r#"xmp:Rating="4" crs:Exposure2012="+1.00""#),
    )
    .unwrap();
    fs::write(dir.join("b.xmp"), xmp(r#"xmp:Rating="2""#)).unwrap();
    fs::write(dir.join("gone.xmp"), xmp(r#"xmp:Rating="5""#)).unwrap();
}

fn checksums(dir: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_name() == ".lt" {
                continue; // Luxtable's own store
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), blake3::hash(&fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

fn opts(dry_run: bool) -> ImportOptions {
    ImportOptions {
        dry_run,
        ..ImportOptions::default()
    }
}

fn open(t: &Path, folder: &Path) -> Arc<SessionHandle> {
    open_in_place(
        folder,
        Some(&t.join("support/InPlace/vol/abc")),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap()
    .into()
}

#[test]
fn imports_sidecars_into_an_in_place_session_and_reverts() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Shoot");
    folder(&dir);
    let before = checksums(&dir);
    let h = open(t.path(), &dir);
    let source = LightroomSource::load(dir.to_str().unwrap(), &|| false).unwrap();
    let s = source.summary();
    assert_eq!(
        (s.kind.as_str(), s.origin.as_str(), s.photos),
        ("sidecars", "Shoot", 3)
    );

    assert!(!h.can_revert_lightroom_import());
    let dry = h
        .import_lightroom_with(&source, &opts(true), &|| false)
        .unwrap();
    assert!(dry.dry_run);
    assert_eq!((dry.photos, dry.matched, dry.matched_by_path), (3, 2, 2));
    assert_eq!(dry.not_found_count, 1);
    assert_eq!(dry.not_found.len(), 1);
    assert!(
        dry.not_found[0].path.ends_with("gone"),
        "{:?}",
        dry.not_found
    );
    assert!(dry.changed.windows(2).all(|w| w[0].count >= w[1].count));
    assert!(
        dry.changed
            .iter()
            .any(|c| c.category == "rating" && c.count == 2)
    );
    assert!(
        dry.changed
            .iter()
            .any(|c| c.category == "tone" && c.count == 1),
        "the +1 EV of a.xmp: {:?}",
        dry.changed
    );
    assert!(!h.can_revert_lightroom_import());

    let real = h
        .import_lightroom_with(&source, &opts(false), &|| false)
        .unwrap();
    assert_eq!(real.changed, dry.changed);
    assert!(h.can_revert_lightroom_import());
    let again = h
        .import_lightroom_with(&source, &opts(false), &|| false)
        .unwrap();
    assert!(again.changed.is_empty(), "idempotent: {:?}", again.changed);
    assert_eq!(
        checksums(&dir),
        before,
        "Lightroom's files and the originals are untouched"
    );

    let r = block_on(h.clone().revert_lightroom_import(CancelToken::new()))
        .unwrap()
        .unwrap();
    assert!(r.restored >= 2);
    assert!(!h.can_revert_lightroom_import());
    assert!(
        block_on(h.clone().revert_lightroom_import(CancelToken::new()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn async_import_checks_categories_and_cancel() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Shoot");
    folder(&dir);
    let h = open(t.path(), &dir);
    let source = LightroomSource::load(dir.to_str().unwrap(), &|| false).unwrap();
    let r = block_on(h.clone().import_lightroom(
        source.clone(),
        vec!["rating".into()],
        true,
        false,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(r.changed.len(), 1);
    assert_eq!(r.changed[0].category, "rating");
    let bad = block_on(h.clone().import_lightroom(
        source.clone(),
        vec!["nope".into()],
        true,
        false,
        CancelToken::new(),
    ));
    assert!(matches!(bad, Err(DsError::InvalidArgument { .. })));
    let token = CancelToken::new();
    token.cancel();
    let cancelled = block_on(
        h.clone()
            .import_lightroom(source, vec![], true, false, token),
    );
    assert!(matches!(cancelled, Err(DsError::Cancelled)));
}

#[test]
fn kinds_are_told_apart_without_a_full_read() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path();
    fs::write(d.join("c.lrcat"), b"x").unwrap();
    assert_eq!(source_kind(&d.join("c.lrcat")), "catalog");
    fs::write(d.join("p.lrtemplate"), b"x").unwrap();
    assert_eq!(source_kind(&d.join("p.lrtemplate")), "presets");
    fs::write(d.join("p.xmp"), xmp(r#"crs:PresetType="Normal""#)).unwrap();
    assert_eq!(source_kind(&d.join("p.xmp")), "presets");
    fs::write(d.join("s.xmp"), xmp(r#"xmp:Rating="1""#)).unwrap();
    assert_eq!(source_kind(&d.join("s.xmp")), "unknown");
    assert_eq!(source_kind(&d.join("missing")), "unknown");
    fs::write(d.join("notes.txt"), b"x").unwrap();
    assert_eq!(source_kind(&d.join("notes.txt")), "unknown");

    let side = d.join("side");
    fs::create_dir_all(side.join("deeper")).unwrap();
    assert_eq!(source_kind(&side), "unknown");
    fs::write(side.join("deeper/a.xmp"), xmp(r#"xmp:Rating="1""#)).unwrap();
    assert_eq!(source_kind(&side), "sidecars");
    let pre = d.join("pre");
    fs::create_dir_all(pre.join("Group")).unwrap();
    fs::write(pre.join("Group/x.xmp"), xmp(r#"crs:PresetType="Normal""#)).unwrap();
    assert_eq!(source_kind(&pre), "presets");
    // A DNG beside them makes it a folder of sidecars.
    fs::write(pre.join("a.dng"), b"x").unwrap();
    assert_eq!(source_kind(&pre), "sidecars");
    // The word in a caption is not a preset; a preset-looking file beside its photo is a sidecar.
    let cap = d.join("cap");
    fs::create_dir_all(&cap).unwrap();
    fs::write(
        cap.join("a.xmp"),
        xmp(r#"dc:description="PresetType notes""#),
    )
    .unwrap();
    assert_eq!(source_kind(&cap), "sidecars");
    assert_eq!(source_kind(&cap.join("a.xmp")), "unknown");
    fs::write(cap.join("b.xmp"), xmp(r#"crs:PresetType="Normal""#)).unwrap();
    fs::write(cap.join("b.ARW"), b"x").unwrap();
    assert_eq!(source_kind(&cap.join("b.xmp")), "unknown");
    assert_eq!(
        lightroom_source_kind(side.to_string_lossy().into_owned()).unwrap(),
        "sidecars"
    );
    // Loading refuses what isn't a catalog or sidecars.
    assert!(LightroomSource::load(d.join("p.xmp").to_str().unwrap(), &|| false).is_err());
    assert!(LightroomSource::load(d.join("notes.txt").to_str().unwrap(), &|| false).is_err());
    assert!(
        block_on(load_lightroom_source(
            d.join("side").to_string_lossy().into_owned(),
            CancelToken::new()
        ))
        .is_ok()
    );
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        block_on(load_lightroom_source(
            d.to_string_lossy().into_owned(),
            token
        )),
        Err(DsError::Cancelled)
    ));
}

#[test]
fn presets_map_to_adjustments_groups_and_notes() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("Warm.xmp");
    fs::write(
        &p,
        xmp(r#"crs:PresetType="Normal" crs:Name="Warm" crs:Group="Mine" crs:Exposure2012="+0.50""#),
    )
    .unwrap();
    let rec = block_on(read_lightroom_presets(
        p.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].group.as_deref(), Some("Mine"));
    let a: lt_core::Adjustments = serde_json::from_str(&rec[0].adjustments_json).unwrap();
    assert_eq!(rec[0].name, "Warm");
    // The preset carries the exposure and says which group it sets.
    assert!((a.exposure.exposure_ev - 0.5).abs() < 1e-6);
    assert!(rec[0].groups.contains(&AdjustGroup::Exposure));
    assert!(fs::metadata(&p).is_ok());
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        block_on(read_lightroom_presets(
            p.to_string_lossy().into_owned(),
            token
        )),
        Err(DsError::Cancelled)
    ));
    assert!(presets_at(&t.path().join("nope.xmp"), &|| false).is_err());
}

#[test]
fn groups_follow_categories() {
    use std::collections::BTreeSet;
    let g = groups_of(&BTreeSet::from([
        Category::Tone,
        Category::Color,
        Category::Rating,
    ]));
    assert_eq!(
        g,
        [
            AdjustGroup::Exposure,
            AdjustGroup::Hdr,
            AdjustGroup::Curve,
            AdjustGroup::Presence,
            AdjustGroup::ColorEditor,
            AdjustGroup::ColorBalance
        ]
    );
    assert_eq!(groups_of(&BTreeSet::from([Category::Rating])).len(), 0);
    let all = groups_of(&Category::ALL.into_iter().collect());
    for want in [
        AdjustGroup::Crop,
        AdjustGroup::WhiteBalance,
        AdjustGroup::Detail,
        AdjustGroup::Noise,
        AdjustGroup::Vignette,
        AdjustGroup::Effects,
        AdjustGroup::Lens,
        AdjustGroup::Layers,
    ] {
        assert!(all.contains(&want), "{want:?}");
    }
}

#[test]
fn reports_convert_sorted_and_capped() {
    let mut r = ImportReport {
        dry_run: true,
        ..ImportReport::default()
    };
    r.changed.insert(Category::Rating, 1);
    r.changed.insert(Category::Tone, 5);
    r.mapped_keys.insert("A".into(), (Fidelity::Exact, 1));
    r.mapped_keys.insert("B".into(), (Fidelity::Approximate, 3));
    r.dropped.insert("X".into(), ("why".into(), 2));
    r.dropped.insert("Y".into(), ("why".into(), 9));
    r.unmapped_labels.insert("Review".into(), 4);
    for i in 0..(MAX_NOT_FOUND + 5) {
        r.not_found.push(lt_lr::NotFound {
            path: format!("/p/{i}.arw"),
            folder: "/p".into(),
        });
    }
    r.not_found_folders.push(lt_lr::FolderCount {
        folder: "/p".into(),
        photos: MAX_NOT_FOUND + 5,
    });
    r.conflicts.push(lt_lr::Conflict {
        variant: lt_session::VariantId(7),
        file: "a".into(),
        field: "rating".into(),
    });
    r.collections_skipped.push(lt_lr::Skipped {
        name: "S".into(),
        reason: "smart".into(),
    });
    r.notes.push(lt_lr::PhotoNote {
        file: "a".into(),
        notes: vec!["n".into()],
    });
    let rec = LrImportReportRecord::from(r);
    assert_eq!(rec.changed[0].category, "tone");
    assert_eq!(rec.mapped[0].key, "B");
    assert!(rec.mapped[0].approximate && !rec.mapped[1].approximate);
    assert_eq!(rec.dropped[0].key, "Y");
    assert_eq!(rec.not_found.len(), MAX_NOT_FOUND);
    assert_eq!(rec.not_found_count as usize, MAX_NOT_FOUND + 5);
    assert_eq!(rec.not_found_folders[0].photos as usize, MAX_NOT_FOUND + 5);
    assert_eq!(
        (rec.conflicts[0].variant_id, rec.unmapped_labels[0].count),
        (7, 4)
    );
    assert_eq!((rec.collections_skipped.len(), rec.notes.len()), (1, 1));
    let rev = LrRevertRecord::from(RevertReport {
        restored: 1,
        kept: 2,
        variants_removed: 3,
        albums_removed: 4,
        albums_trimmed: 5,
    });
    assert_eq!(
        (
            rev.restored,
            rev.kept,
            rev.variants_removed,
            rev.albums_removed,
            rev.albums_trimmed
        ),
        (1, 2, 3, 4, 5)
    );
}

#[test]
fn errors_map_to_ffi_errors() {
    let io = || std::io::Error::other("x");
    let p = || PathBuf::from("p");
    let cases = [
        (
            LrError::Io {
                path: p(),
                source: io(),
            },
            "io",
        ),
        (
            LrError::Db {
                path: p(),
                message: "m".into(),
            },
            "io",
        ),
        (
            LrError::Xmp {
                path: p(),
                message: "m".into(),
            },
            "io",
        ),
        (
            LrError::Preset {
                path: p(),
                message: "m".into(),
            },
            "io",
        ),
        (
            LrError::NotACatalog {
                path: p(),
                message: "m".into(),
            },
            "arg",
        ),
        (LrError::Cancelled, "cancel"),
    ];
    for (e, want) in cases {
        let got = match DsError::from(e) {
            DsError::Io { .. } => "io",
            DsError::InvalidArgument { .. } => "arg",
            DsError::Cancelled => "cancel",
            other => panic!("{other:?}"),
        };
        assert_eq!(got, want);
    }
}
