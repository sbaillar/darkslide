//! Capture One import through the FFI: a synthetic C1 session folder opened
//! in place (store in a temp fallback, as the app does), dry run, import,
//! idempotency, and the folder left byte-identical. With `samples/`, the same
//! on real A9 ARWs with the real white-balance conversion.
#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ds_core::adjust::WhiteBalance;
use ds_core::geom::Orientation;
use ds_session::ImageMetaLite;
use futures_executor::block_on;

use super::*;
use crate::raw::RawMetadata;
use crate::session::{Probe, open_in_place};

const SQL: &str = include_str!("../../ds-c1/fixtures/session.sql");
const COS_A: &str = include_str!("../../ds-c1/fixtures/a.ARW.cos");
const COS_C: &str = include_str!("../../ds-c1/fixtures/c.ARW.cos");
const COS_D: &str = include_str!("../../ds-c1/fixtures/d.ARW.cos");

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn fake_read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    fs::metadata(p).ok()?;
    let lite = ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: if p.ends_with("b.ARW") {
            Orientation::Rotate270
        } else {
            Orientation::Normal
        },
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

/// The synthetic C1 session of `ds-c1/fixtures` at `dir`.
fn c1_folder(dir: &Path, image: &dyn Fn(&str) -> Vec<u8>) {
    for d in ["Capture", "Output", "Selects", "Trash"] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    build_db(&dir.join("Shoot.cosessiondb"));
    for rel in ["a.ARW", "b.ARW", "Selects/c.ARW", "Trash/d.ARW"] {
        fs::write(dir.join(rel), image(rel)).unwrap();
    }
    for (sub, name, xml) in [
        ("", "a.ARW", COS_A),
        ("Selects/", "c.ARW", COS_C),
        ("Trash/", "d.ARW", COS_D),
    ] {
        let s = dir.join(format!("{sub}CaptureOne/Settings153"));
        fs::create_dir_all(&s).unwrap();
        fs::write(s.join(format!("{name}.cos")), xml).unwrap();
    }
}

fn build_db(path: &Path) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(SQL)
        .unwrap();
}

fn checksums(dir: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), blake3::hash(&fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

fn no() -> Cancel {
    Arc::new(|| false)
}

struct NoWb;
impl WbConverter for NoWb {
    fn convert(&self, _: &Path, _: Option<[f64; 3]>, _: [f64; 3]) -> Option<WhiteBalance> {
        None
    }
}

const DRY: ImportOptions = ImportOptions {
    dry_run: true,
    overwrite_edits: false,
};
const REAL: ImportOptions = ImportOptions {
    dry_run: false,
    overwrite_edits: false,
};

#[test]
fn imports_a_synthetic_session_opened_in_place() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    c1_folder(&folder, &|rel| format!("raw {rel}").into_bytes());
    let before = checksums(&folder);
    let store = t.path().join("support/InPlace/vol/abc");
    let h = open_in_place(
        &folder,
        Some(&store),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap();
    assert_eq!(
        h.open_report().unwrap().added,
        3,
        "a, b and Selects/c; C1's Trash is not listed"
    );
    assert!(
        lock(&h.session).root().starts_with(&store),
        "the store is never in the C1 folder"
    );

    let dry = h.import_capture_one_with(&NoWb, DRY, &no()).unwrap();
    assert!(dry.dry_run);
    assert_eq!(
        (dry.images, dry.matched, dry.in_trash, dry.outside_folder),
        (3, 3, 1, 1)
    );
    assert_eq!(
        (
            dry.rated,
            dry.tagged,
            dry.picked,
            dry.keyworded,
            dry.edits_mapped
        ),
        (3, 2, 1, 1, 2)
    );
    assert_eq!(dry.written_by.as_deref(), Some("16.1.3.10 Pro Mac"));
    // NoWb can't read originals: white balance is listed as not mapped, with the reason.
    let wb = dry
        .unmapped
        .iter()
        .find(|u| u.key == "WhiteBalance")
        .unwrap();
    assert!(
        wb.reason.contains("couldn't be read") && wb.count == 1,
        "{wb:?}"
    );
    assert!(
        dry.unmapped.windows(2).all(|w| w[0].count >= w[1].count),
        "most frequent first"
    );
    assert_eq!(
        dry.mapped[0].key, "Brightness",
        "a and b both change it: first"
    );
    assert_eq!(dry.mapped[0].count, 2);

    let real = h.import_capture_one_with(&NoWb, REAL, &no()).unwrap();
    assert_eq!(real.rated, dry.rated);
    assert_eq!(real.albums_created, 2);
    let s = lock(&h.session);
    let ratings: BTreeMap<String, u8> = s
        .images()
        .unwrap()
        .iter()
        .map(|i| {
            let v = s.variants(i.id).unwrap()[0];
            (
                i.rel_path.as_str().to_owned(),
                s.variant(v).unwrap().rating.stars(),
            )
        })
        .collect();
    drop(s);
    assert_eq!(
        ratings,
        BTreeMap::from([
            ("Capture/Selects/c.ARW".to_owned(), 2),
            ("Capture/a.ARW".to_owned(), 4),
            ("Capture/b.ARW".to_owned(), 3)
        ])
    );
    let again = h.import_capture_one_with(&NoWb, REAL, &no()).unwrap();
    assert_eq!(
        (
            again.rated,
            again.edits_mapped,
            again.variants_created,
            again.in_sync
        ),
        (0, 0, 0, 4)
    );
    assert_eq!(
        checksums(&folder),
        before,
        "the C1 folder is byte-identical"
    );
}

#[test]
fn conflicts_cross_the_ffi_and_errors_map() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    c1_folder(&folder, &|rel| format!("raw {rel}").into_bytes());
    let h = open_in_place(
        &folder,
        Some(&t.path().join("store")),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap();
    {
        let mut s = lock(&h.session);
        let b = s
            .images()
            .unwrap()
            .into_iter()
            .find(|i| i.rel_path.as_str() == "Capture/b.ARW")
            .unwrap();
        let v = s.variants(b.id).unwrap()[0];
        s.set_rating(&[v], ds_core::Rating::new(1).unwrap())
            .unwrap();
    }
    let r = h.import_capture_one_with(&NoWb, REAL, &no()).unwrap();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(
        (r.conflicts[0].file.as_str(), r.conflicts[0].field.as_str()),
        ("b.ARW", "rating")
    );
    let r = h
        .import_capture_one_with(
            &NoWb,
            ImportOptions {
                dry_run: false,
                overwrite_edits: true,
            },
            &no(),
        )
        .unwrap();
    assert_eq!(r.overwritten, 1);

    // Not a C1 folder any more (its database is gone): reported, nothing written.
    fs::remove_file(folder.join("Shoot.cosessiondb")).unwrap();
    let e = h.import_capture_one_with(&NoWb, DRY, &no()).unwrap_err();
    assert!(matches!(e, DsError::InvalidArgument { .. }), "{e:?}");
    // The folder moved away.
    fs::rename(&folder, t.path().join("Moved")).unwrap();
    assert!(matches!(
        h.import_capture_one_with(&NoWb, DRY, &no()),
        Err(DsError::FolderMoved { .. })
    ));
}

#[test]
fn an_import_reverts_through_the_ffi() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    c1_folder(&folder, &|rel| format!("raw {rel}").into_bytes());
    let before = checksums(&folder);
    let h = std::sync::Arc::new(
        open_in_place(
            &folder,
            Some(&t.path().join("store")),
            None,
            &FAKE,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap(),
    );
    assert!(!h.can_revert_capture_one_import());
    let revert = |token| block_on(h.clone().revert_capture_one_import(token));
    assert_eq!(revert(CancelToken::new()), Ok(None));
    h.import_capture_one_with(&NoWb, DRY, &no()).unwrap();
    assert!(
        !h.can_revert_capture_one_import(),
        "dry runs change nothing"
    );
    let first = h.import_capture_one_with(&NoWb, REAL, &no()).unwrap();
    assert!(h.can_revert_capture_one_import());
    let token = CancelToken::new();
    token.cancel();
    assert_eq!(revert(token), Err(DsError::Cancelled));
    assert!(
        h.can_revert_capture_one_import(),
        "a cancelled revert does nothing"
    );
    let r = revert(CancelToken::new()).unwrap().unwrap();
    assert_eq!(
        (r.restored, r.kept, r.variants_removed, r.albums_removed),
        (3, 0, 1, 2)
    );
    assert!(!h.can_revert_capture_one_import());
    let again = h.import_capture_one_with(&NoWb, REAL, &no()).unwrap();
    assert_eq!(again, first, "the import record went back too");
    assert_eq!(checksums(&folder), before);
}

#[test]
fn only_in_place_sessions_import() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("plain");
    ds_session::Session::create(&dir, "Plain").unwrap();
    let open = || {
        block_on(crate::session::open_session(
            dir.to_string_lossy().into_owned(),
            CancelToken::new(),
        ))
        .unwrap()
    };
    let h = open();
    let e = block_on(h.import_capture_one(true, false, CancelToken::new())).unwrap_err();
    assert!(
        matches!(e, DsError::InvalidArgument { ref message } if message.contains("in place")),
        "{e:?}"
    );
    let token = CancelToken::new();
    token.cancel();
    assert_eq!(
        block_on(open().import_capture_one(true, false, token)),
        Err(DsError::Cancelled)
    );
}

#[test]
fn error_mapping_and_folder_detection() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(
        is_capture_one_folder(t.path().to_string_lossy().into_owned()),
        Ok(false)
    );
    fs::write(t.path().join("X.cosessiondb"), b"").unwrap();
    assert_eq!(
        is_capture_one_folder(t.path().to_string_lossy().into_owned()),
        Ok(true)
    );
    let io = DsError::from(C1Error::Db {
        path: "x".into(),
        message: "bad".into(),
    });
    assert!(matches!(io, DsError::Io { .. }));
    let s = DsError::from(C1Error::Session(ds_session::SessionError::InvalidName(
        String::new(),
    )));
    assert!(!matches!(s, DsError::Io { .. }));
    assert!(matches!(
        DsError::from(C1Error::WrongSession("/x".into())),
        DsError::InvalidArgument { .. }
    ));
}

#[test]
fn white_balance_is_relative_to_the_as_shot() {
    let info = ds_raw::ColorInfo {
        wb_as_shot: [2.0, 1.0, 1.6],
        wb_daylight: [2.2, 1.0, 1.5],
        cam_to_xyz: [[0.6, 0.3, 0.1], [0.25, 0.7, 0.05], [0.0, 0.1, 0.9]],
    };
    let temp = |w: WhiteBalance| match w {
        WhiteBalance::Custom { temp_k, .. } => temp_k,
        WhiteBalance::AsShot => panic!("custom expected"),
    };
    let as_shot = temp(multipliers_to_wb(
        info.wb_as_shot,
        &CameraColor {
            cam_to_xyz: info.cam_to_xyz,
            daylight: normalize_mults(info.wb_daylight),
        },
    ));
    let same = temp(wb_from(&info, Some([2.5, 1.0, 1.5]), [2.5, 1.0, 1.5]));
    assert!((same - as_shot).abs() < 1.0, "{same} vs {as_shot}");
    let warmer = temp(wb_from(&info, Some([2.5, 1.0, 1.5]), [2.7, 1.0, 1.4]));
    assert!(
        warmer > as_shot + 100.0,
        "more red gain = a warmer setting: {warmer} vs {as_shot}"
    );
    // Without C1's as-shot the multipliers are taken as the camera's.
    let direct = temp(wb_from(&info, None, [2.0, 1.0, 1.6]));
    assert!((direct - as_shot).abs() < 1.0);
    assert!(
        RawWb
            .convert(Path::new("/nonexistent.ARW"), None, [2.0, 1.0, 1.6])
            .is_none()
    );
}

/// The synthetic session again, with real A9 ARWs and the real conversion.
#[test]
fn real_arws_get_a_converted_white_balance() {
    let samples = ds_testdata::samples_with_ext("arw");
    if samples.len() < 4 {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    let pick = |rel: &str| match rel {
        "a.ARW" => &samples[0],
        "b.ARW" => &samples[1],
        "Selects/c.ARW" => &samples[2],
        _ => &samples[3],
    };
    for d in ["Selects", "Trash"] {
        fs::create_dir_all(folder.join(d)).unwrap();
    }
    c1_folder(&folder, &|rel| fs::read(pick(rel)).unwrap());
    let before = checksums(&folder);
    let h = open_in_place(
        &folder,
        Some(&t.path().join("store")),
        None,
        &Probe {
            volume_ok: |_| Ok(()),
            ..Probe::RAW
        },
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap();
    let r = h.import_capture_one_with(&RawWb, REAL, &no()).unwrap();
    assert!(
        r.unmapped.iter().all(|u| u.key != "WhiteBalance"),
        "{:?}",
        r.unmapped
    );
    let s = lock(&h.session);
    let a = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/a.ARW")
        .unwrap();
    let adj = s.load_adjustments(s.variants(a.id).unwrap()[0]).unwrap();
    let WhiteBalance::Custom { temp_k, .. } = adj.white_balance else {
        panic!("{:?}", adj.white_balance)
    };
    assert!((2000.0..15000.0).contains(&temp_k), "{temp_k}");
    drop(s);
    assert_eq!(checksums(&folder), before);
}

/// Reads originals from the real folder for a scratch copy of it.
struct Remap(Option<PathBuf>, PathBuf);
impl WbConverter for Remap {
    fn convert(&self, original: &Path, d: Option<[f64; 3]>, u: [f64; 3]) -> Option<WhiteBalance> {
        let real = self.0.as_ref()?.join(original.strip_prefix(&self.1).ok()?);
        RawWb.convert(&real, d, u)
    }
}

/// Dry run against read-only copies of a real C1 session (counts only):
/// `DS_C1_COPY` = a scratch folder holding the copied `.cosessiondb` and
/// `CaptureOne/Settings*/*.cos` files plus empty placeholders named like the
/// images; `DS_C1_REAL` (optional) = the real folder, whose originals are
/// only read for the white-balance conversion.
#[test]
#[ignore = "needs DS_C1_COPY (a scratch copy of a real session's metadata)"]
fn real_session_dry_run_counts() {
    let Some(copy) = std::env::var_os("DS_C1_COPY").map(PathBuf::from) else {
        return;
    };
    let wb = Remap(
        std::env::var_os("DS_C1_REAL").map(PathBuf::from),
        copy.canonicalize().unwrap(),
    );
    let t = tempfile::tempdir().unwrap();
    let before = checksums(&copy);
    let h = open_in_place(
        &copy,
        Some(&t.path().join("store")),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap();
    let r = h.import_capture_one_with(&wb, DRY, &no()).unwrap();
    println!("{r:#?}");
    assert_eq!(
        r.matched, r.images,
        "every C1 image in the folder is listed"
    );
    assert!(
        r.conflicts.is_empty(),
        "a fresh store has no Darkslide edits"
    );
    assert!(!h.can_revert_capture_one_import());
    assert_eq!(checksums(&copy), before);
}

#[test]
fn a_cancelled_import_is_cancelled() {
    assert_eq!(DsError::from(C1Error::Cancelled), DsError::Cancelled);
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    c1_folder(&folder, &|rel| format!("raw {rel}").into_bytes());
    let h = open_in_place(
        &folder,
        Some(&t.path().join("store")),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap();
    assert_eq!(
        h.import_capture_one_with(&NoWb, REAL, &(Arc::new(|| true) as Cancel)),
        Err(DsError::Cancelled)
    );
    assert!(!h.can_revert_capture_one_import());
}
