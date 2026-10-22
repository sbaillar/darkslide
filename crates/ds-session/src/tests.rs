//! Session-level behavior tests (in-crate so they can use the write-fault seam).
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ds_core::adjust::{Aspect, Crop};
use ds_core::clipboard::Clipboard;
use ds_core::geom::{NormRect, Orientation, Size};
use ds_core::{AdjustGroup, Adjustments, ColorTag, FileHash, PasteOptions, PickState, Rating};

use crate::atomic::{WriteHook, WriteStage};
use crate::*;

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    s: Session,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("2026-09-25 Fall Hike");
    let s = Session::create(&root, "2026-09-25 Fall Hike").unwrap();
    Fx { _dir: dir, root, s }
}

fn put(root: &Path, rel: &str, bytes: &[u8]) -> FileHash {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
    FileHash(*blake3::hash(bytes).as_bytes())
}

fn meta(i: i64) -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: Some(1_700_000_000_000 + i * 1000),
        camera: Some("ILCE-9".into()),
        lens: Some("FE 24-70mm F2.8 GM".into()),
        iso: Some(100 * u32::try_from(i + 1).unwrap()),
        focal_length_mm: Some(35.0),
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        as_shot_wb: None,
    }
}

impl Fx {
    fn add(&mut self, rel: &str, i: i64) -> (ImageId, VariantId) {
        let h = put(&self.root, rel, format!("raw-bytes-{rel}").as_bytes());
        let id = self.s.add_original(rel, h, meta(i)).unwrap();
        (id, self.s.variants(id).unwrap()[0])
    }

    fn reopen(&mut self) {
        self.s = Session::open(&self.root).unwrap();
    }

    fn sidecar(&self, v: VariantId) -> Sidecar {
        let rel = self.s.locate(v).unwrap().sidecar;
        Sidecar::from_json(&fs::read(self.root.join(rel)).unwrap())
            .unwrap()
            .sidecar
    }
}

fn ev(x: f32) -> Adjustments {
    let mut a = Adjustments::default();
    a.exposure.exposure_ev = x;
    a
}

fn r(n: u8) -> Rating {
    Rating::new(n).unwrap()
}

// ------------------------------------------------------------------ layout ---

#[test]
fn create_makes_layout_and_rejects_bad_input() {
    let f = fx();
    for sub in [
        "Capture",
        "Selects",
        "Output",
        "Trash",
        ".ds/settings",
        ".ds/cache",
    ] {
        assert!(f.root.join(sub).is_dir(), "{sub}");
    }
    assert!(f.root.join("2026-09-25 Fall Hike.dsession").is_file());
    assert!(f.root.join(".ds/settings/session.json").is_file());
    assert_eq!(f.s.name(), "2026-09-25 Fall Hike");
    assert_eq!(f.s.root(), f.root);
    assert_eq!(f.s.cache_dir(), f.root.join(".ds/cache"));
    assert_eq!(f.s.open_report().recovery, Some(Recovery::Missing));
    assert!(format!("{:?}", f.s).contains("Fall Hike"));

    assert!(matches!(
        Session::create(&f.root, "x"),
        Err(SessionError::AlreadyExists(_))
    ));
    let d = tempfile::tempdir().unwrap();
    for bad in ["", " ", ".hidden", "a/b", "a:b"] {
        assert!(
            matches!(
                Session::create(&d.path().join("s"), bad),
                Err(SessionError::InvalidName(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        Session::open(d.path()),
        Err(SessionError::NotASession(_))
    ));
    assert!(matches!(
        Session::open(&d.path().join("nope")),
        Err(SessionError::NotASession(_))
    ));
}

#[test]
fn open_recovers_missing_session_json_from_index_name() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    fs::remove_file(f.root.join(".ds/settings/session.json")).unwrap();
    f.reopen();
    assert_eq!(f.s.name(), "2026-09-25 Fall Hike");
    assert!(f.s.variant(v).is_ok());
    assert!(f.root.join(".ds/settings/session.json").is_file());

    fs::write(f.root.join(".ds/settings/session.json"), b"{not json").unwrap();
    assert!(matches!(
        Session::open(&f.root),
        Err(SessionError::Json { .. })
    ));
}

#[test]
fn images_are_found_by_path_and_by_hash() {
    let mut f = fx();
    let (a, _) = f.add("Capture/a.ARW", 0);
    let (b, _) = f.add("Capture/b.ARW", 1);
    let rel = |p: &str| RelPath::new(p).unwrap();
    assert_eq!(
        f.s.image_by_rel(&rel("Capture/b.ARW")).unwrap().unwrap().id,
        b
    );
    assert!(f.s.image_by_rel(&rel("Capture/zz.ARW")).unwrap().is_none());
    let ha = f.s.image(a).unwrap().hash;
    assert_eq!(f.s.image_by_hash(&ha).unwrap().unwrap().id, a);
    assert!(f.s.image_by_hash(&FileHash([7; 32])).unwrap().is_none());

    f.add("Capture/c.ARW", 2);
    let rels = |v: Vec<ImageInfo>| {
        v.iter()
            .map(|i| i.rel_path.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rels(f.s.images_after(None, 2).unwrap()),
        ["Capture/a.ARW", "Capture/b.ARW"]
    );
    let after = rel("Capture/b.ARW");
    assert_eq!(
        rels(f.s.images_after(Some(&after), 2).unwrap()),
        ["Capture/c.ARW"]
    );
}

// --------------------------------------------------------------- originals ---

#[test]
fn add_original_writes_sidecar_and_index() {
    let mut f = fx();
    let (img, v) = f.add("Capture/Day 1/DSC00412.ARW", 3);
    let info = f.s.image(img).unwrap();
    assert_eq!(info.rel_path.as_str(), "Capture/Day 1/DSC00412.ARW");
    assert_eq!(info.folder(), Folder::Capture);
    assert_eq!(info.meta, meta(3));
    assert_eq!(
        info.file_size,
        "raw-bytes-Capture/Day 1/DSC00412.ARW".len() as u64
    );
    assert!(!info.missing);
    assert!(
        f.root
            .join("Capture/Day 1/.ds/settings/DSC00412.ARW.json")
            .is_file()
    );
    let vi = f.s.variant(v).unwrap();
    assert_eq!(
        (vi.image, vi.ordinal, vi.rating, vi.edited, vi.read_only),
        (img, 1, Rating::default(), false, false)
    );
    let sc = f.sidecar(v);
    assert_eq!(sc.schema_version, ds_core::SIDECAR_SCHEMA_VERSION);
    assert_eq!(sc.hash, info.hash);
    assert_eq!(f.s.images().unwrap(), vec![info]);
    assert_eq!(f.s.image_count().unwrap(), 1);
    assert_eq!(f.s.all_variants().unwrap(), vec![v]);
    assert_eq!(f.s.load_adjustments(v).unwrap(), Adjustments::default());
}

#[test]
fn add_original_rejects_traversal_duplicates_and_missing_files() {
    let mut f = fx();
    let h = put(&f.root, "Capture/a.ARW", b"a");
    fs::write(f.root.parent().unwrap().join("outside.ARW"), b"o").unwrap();
    for bad in [
        "../outside.ARW",
        "Capture/../../outside.ARW",
        "/etc/passwd",
        "Capture/.ds/x",
        "Output/a.ARW",
    ] {
        assert!(
            matches!(
                f.s.add_original(bad, h, meta(0)),
                Err(SessionError::InvalidPath { .. })
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        f.s.add_original("Capture/none.ARW", h, meta(0)),
        Err(SessionError::OriginalMissing(_))
    ));
    f.s.add_original("Capture/a.ARW", h, meta(0)).unwrap();
    assert!(matches!(
        f.s.add_original("Capture/a.ARW", h, meta(0)),
        Err(SessionError::DuplicatePath(_))
    ));
    put(&f.root, "Capture/b.ARW", b"b");
    let dup = vec![
        ("Capture/b.ARW".to_owned(), h, meta(0)),
        ("Capture/b.ARW".to_owned(), h, meta(0)),
    ];
    assert!(matches!(
        f.s.add_originals(dup),
        Err(SessionError::DuplicatePath(_))
    ));
    assert_eq!(f.s.image_count().unwrap(), 1, "failed batch wrote nothing");
    assert!(matches!(
        f.s.image(ImageId(42)),
        Err(SessionError::NotFound { kind: "image", .. })
    ));
    assert!(matches!(
        f.s.variant(VariantId(42)),
        Err(SessionError::NotFound {
            kind: "variant",
            ..
        })
    ));
    assert!(matches!(
        f.s.variants(ImageId(42)),
        Err(SessionError::NotFound { .. })
    ));
}

#[test]
fn unicode_file_names_survive_everything() {
    let mut f = fx();
    let (img, v) = f.add("Capture/été 日本/📷 Ünïcødé.ARW", 0);
    f.s.set_keywords(&[v], &["Fête".into(), "東京".into()])
        .unwrap();
    assert_eq!(
        f.s.query(
            &Filter {
                text: Some("東京 fête".into()),
                ..Filter::default()
            },
            Sort::default()
        )
        .unwrap(),
        vec![v]
    );
    assert_eq!(
        f.s.query(
            &Filter {
                text: Some("ünïcødé".into()),
                ..Filter::default()
            },
            Sort::default()
        )
        .unwrap(),
        vec![v]
    );
    f.s.move_to(&[v], Folder::Selects).unwrap();
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Selects/été 日本/📷 Ünïcødé.ARW"
    );
    fs::remove_file(f.s.db_path()).unwrap();
    f.reopen();
    assert_eq!(f.s.variant(v).unwrap().keywords, ["Fête", "東京"]);
    assert_eq!(
        f.s.rescan().unwrap().missing,
        [] as [(session::ImageId, relpath::RelPath); 0]
    );
}

// ----------------------------------------------------------------- variants ---

#[test]
fn variants_clone_adjustments_and_delete_rules() {
    let mut f = fx();
    let (img, v1) = f.add("Capture/IMG", 0);
    f.s.save_adjustments(v1, &ev(1.0)).unwrap();
    f.s.set_rating(&[v1], r(5)).unwrap();
    let v2 = f.s.new_variant(img).unwrap();
    let v3 = f.s.new_variant(img).unwrap();
    assert_eq!(f.s.variants(img).unwrap(), vec![v1, v2, v3]);
    assert_eq!(f.s.load_adjustments(v2).unwrap(), ev(1.0));
    let i2 = f.s.variant(v2).unwrap();
    assert_eq!(
        (i2.ordinal, i2.rating, i2.name.as_str(), i2.edited),
        (2, Rating::default(), "Variant 2", true)
    );
    assert!(f.root.join("Capture/.ds/settings/v2/IMG.json").is_file());
    // Name that would collide under a suffix scheme gets its own sidecar.
    let (_, other) = f.add("Capture/IMG.v2", 1);
    assert_ne!(
        f.s.locate(other).unwrap().sidecar,
        f.s.locate(v2).unwrap().sidecar
    );

    f.s.rename_variant(v2, "  B&W  ").unwrap();
    assert_eq!(f.s.variant(v2).unwrap().name, "B&W");
    f.s.delete_variant(v2).unwrap();
    assert!(!f.root.join("Capture/.ds/settings/v2/IMG.json").exists());
    assert_eq!(f.s.variants(img).unwrap(), vec![v1, v3]);
    let v4 = f.s.new_variant(img).unwrap();
    assert_eq!(
        f.s.variant(v4).unwrap().ordinal,
        4,
        "ordinals are not reused"
    );
    assert!(![v1, v2, v3].contains(&v4));
    f.s.delete_variant(v3).unwrap();
    f.s.delete_variant(v4).unwrap();
    assert!(matches!(
        f.s.delete_variant(v1),
        Err(SessionError::LastVariant)
    ));
    assert!(matches!(
        f.s.delete_variant(v2),
        Err(SessionError::NotFound { .. })
    ));
}

// ----------------------------------------------------------- organization ---

#[test]
fn ratings_tags_picks_keywords_persist_and_batch() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..5)
        .map(|i| f.add(&format!("Capture/{i}.ARW"), i).1)
        .collect();
    f.s.set_rating(&vs[..3], r(4)).unwrap();
    f.s.set_color_tag(&vs[1..], ColorTag::Red).unwrap();
    f.s.set_pick(&[vs[0], vs[0], vs[4]], PickState::Rejected)
        .unwrap();
    f.s.set_keywords(
        &vs[..2],
        &[
            " surf ".into(),
            "surf".into(),
            String::new(),
            "beach".into(),
        ],
    )
    .unwrap();
    f.s.add_keyword(&vs[..3], "sunset").unwrap();
    f.s.add_keyword(&vs[..3], "sunset").unwrap();
    f.s.add_keyword(&vs, "  ").unwrap();
    f.s.remove_keyword(&vs, "beach").unwrap();

    let check = |s: &Session| {
        let i0 = s.variant(vs[0]).unwrap();
        assert_eq!(
            (i0.rating, i0.color_tag, i0.pick),
            (r(4), ColorTag::None, PickState::Rejected)
        );
        assert_eq!(i0.keywords, ["surf", "sunset"]);
        let i2 = s.variant(vs[2]).unwrap();
        assert_eq!(
            (i2.rating, i2.color_tag, i2.pick),
            (r(4), ColorTag::Red, PickState::Unflagged)
        );
        assert_eq!(i2.keywords, ["sunset"]);
        let i4 = s.variant(vs[4]).unwrap();
        assert_eq!(
            (i4.rating, i4.color_tag, i4.pick),
            (Rating::default(), ColorTag::Red, PickState::Rejected)
        );
    };
    check(&f.s);
    let sc = f.sidecar(vs[0]);
    assert_eq!((sc.rating, sc.pick), (r(4), PickState::Rejected));
    f.reopen();
    check(&f.s);
    // Index gone: everything comes back from sidecars with the same ids.
    fs::remove_file(f.s.db_path()).unwrap();
    f.reopen();
    check(&f.s);
    assert_eq!(f.s.rebuild_index().unwrap().indexed, 5);
    check(&f.s);
    assert!(matches!(
        f.s.set_rating(&[VariantId(1)], r(1)),
        Err(SessionError::NotFound { .. })
    ));
}

#[test]
fn unchanged_values_do_not_rewrite_sidecars() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    f.s.set_rating(&[v], r(3)).unwrap();
    let path = f.root.join(f.s.locate(v).unwrap().sidecar);
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    let hook: WriteHook = Arc::new(|_, _| Err(io::Error::other("must not write")));
    f.s.set_write_hook(Some(hook));
    f.s.set_rating(&[v], r(3)).unwrap();
    f.s.save_adjustments(v, &Adjustments::default()).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
}

// ------------------------------------------------------------ index health ---

#[test]
fn open_rebuilds_corrupt_or_outdated_index() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    f.s.set_rating(&[v], r(2)).unwrap();
    let db = f.s.db_path();

    drop(f.s);
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    fs::write(
        &db,
        b"this is not a database at all, not even close........",
    )
    .unwrap();
    f.s = Session::open(&f.root).unwrap();
    assert!(matches!(
        f.s.open_report().recovery,
        Some(Recovery::Corrupt(_))
    ));
    assert_eq!(f.s.variant(v).unwrap().rating, r(2));

    f.s.conn.execute_batch("PRAGMA user_version = 0").unwrap();
    f.reopen();
    assert_eq!(
        f.s.open_report().recovery,
        Some(Recovery::SchemaChanged { found: 0 })
    );
    assert_eq!(f.s.variant(v).unwrap().rating, r(2));
}

#[test]
fn open_picks_up_sidecar_changes_made_behind_its_back() {
    // Simulates a crash after the sidecar rename but before the index update,
    // or another tool editing the sidecar.
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let (_, gone) = f.add("Capture/b.ARW", 1);
    let mut sc = f.sidecar(v);
    sc.rating = r(5);
    sc.keywords = vec!["edited elsewhere".into()];
    let rel = f.s.locate(v).unwrap().sidecar;
    fs::write(f.root.join(&rel), sc.to_json()).unwrap();
    fs::remove_file(f.root.join(f.s.locate(gone).unwrap().sidecar)).unwrap();
    f.reopen();
    assert_eq!(f.s.variant(v).unwrap().rating, r(5));
    assert_eq!(f.s.open_report().indexed, 1);
    assert_eq!(f.s.open_report().removed, 1);
    assert!(f.s.variant(gone).is_err());
}

#[test]
fn broken_and_duplicated_sidecars_are_reported_not_fatal() {
    let mut f = fx();
    let (_, v) = f.add("Capture/sub/a.ARW", 0);
    fs::write(
        f.root.join("Capture/sub/.ds/settings/junk.ARW.json"),
        b"{garbage",
    )
    .unwrap();
    // A folder duplicated in Finder carries the same ids.
    fs::create_dir_all(f.root.join("Capture/copy/.ds/settings")).unwrap();
    fs::copy(
        f.root.join("Capture/sub/a.ARW"),
        f.root.join("Capture/copy/a.ARW"),
    )
    .unwrap();
    fs::copy(
        f.root.join("Capture/sub/.ds/settings/a.ARW.json"),
        f.root.join("Capture/copy/.ds/settings/a.ARW.json"),
    )
    .unwrap();
    let report = f.s.sync_index().unwrap();
    assert_eq!(report.errors.len(), 2, "{report:?}");
    assert_eq!(
        f.s.locate(v).unwrap().sidecar,
        "Capture/sub/.ds/settings/a.ARW.json",
        "original row kept"
    );
    assert_eq!(f.s.image_count().unwrap(), 1);
}

#[test]
fn stale_temp_files_are_cleaned_but_fresh_ones_kept() {
    let mut f = fx();
    f.add("Capture/a.ARW", 0);
    let old = f.root.join("Capture/.ds/settings/.a.ARW.json.dstmp-1-1");
    let fresh = f.root.join("Capture/.ds/settings/.a.ARW.json.dstmp-1-2");
    fs::write(&old, b"{partial").unwrap();
    fs::write(&fresh, b"{partial").unwrap();
    let file = fs::File::options().write(true).open(&old).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    f.reopen();
    assert!(!old.exists());
    assert!(fresh.exists(), "may belong to a live writer");
    assert_eq!(
        f.s.open_report().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
}

#[test]
fn snapshot_is_a_consistent_openable_copy() {
    let mut f = fx();
    for i in 0..10 {
        f.add(&format!("Capture/{i}.ARW"), i);
    }
    let snap = f.root.join(".ds/index-snapshot.db");
    f.s.snapshot_to(&snap).unwrap();
    f.s.snapshot_to(&snap).unwrap(); // overwrite is fine
    let c = rusqlite::Connection::open(&snap).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM variants", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 10);
    assert!(!PathBuf::from(format!("{}.partial", snap.display())).exists());
}

// ---------------------------------------------------------- newer / faults ---

#[test]
fn sidecar_from_newer_build_is_indexed_read_only_and_never_rewritten() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let rel = f.s.locate(v).unwrap().sidecar;
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join(&rel)).unwrap()).unwrap();
    json["schema_version"] = 99.into();
    json["rating"] = 3.into();
    json["hologram"] = serde_json::json!({"depth": 1});
    let bytes = serde_json::to_vec(&json).unwrap();
    fs::write(f.root.join(&rel), &bytes).unwrap();
    f.reopen();
    let info = f.s.variant(v).unwrap();
    assert!(info.read_only);
    assert_eq!(info.rating, r(3));
    for res in [
        f.s.set_rating(&[v], r(1)),
        f.s.save_adjustments(v, &ev(1.0)),
        f.s.apply_batch(&[(v, ev(1.0))]).map(|_| ()),
    ] {
        assert!(matches!(
            res,
            Err(SessionError::SidecarTooNew { found: 99, .. })
        ));
    }
    assert_eq!(fs::read(f.root.join(&rel)).unwrap(), bytes, "untouched");
    // It can still be moved (files move, content is not rewritten).
    f.s.move_to(&[v], Folder::Selects).unwrap();
    assert_eq!(
        fs::read(f.root.join(f.s.locate(v).unwrap().sidecar)).unwrap(),
        bytes
    );
}

#[test]
fn old_sidecar_without_migration_path_is_reported() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let rel = f.s.locate(v).unwrap().sidecar;
    let text = String::from_utf8(fs::read(f.root.join(&rel)).unwrap()).unwrap();
    fs::write(
        f.root.join(&rel),
        text.replace("\"schema_version\": 1", "\"schema_version\": 0"),
    )
    .unwrap();
    assert!(matches!(
        f.s.load_adjustments(v),
        Err(SessionError::NoMigration { found: 0, .. })
    ));
    let report = f.s.sync_index().unwrap();
    assert_eq!(report.errors.len(), 1);
}

#[test]
fn crash_after_temp_write_keeps_old_sidecar_and_index() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    f.s.set_rating(&[v], r(2)).unwrap();
    let hook: WriteHook = Arc::new(|_, stage| {
        assert_eq!(stage, WriteStage::TempWritten);
        Err(io::Error::other("power cut"))
    });
    f.s.set_write_hook(Some(hook));
    assert!(matches!(
        f.s.set_rating(&[v], r(5)),
        Err(SessionError::Io { .. })
    ));
    assert_eq!(f.sidecar(v).rating, r(2));
    assert_eq!(f.s.variant(v).unwrap().rating, r(2));
    assert!(f.s.save_adjustments(v, &ev(2.0)).is_err());
    assert_eq!(
        f.s.undo_depth(v),
        1,
        "only the successful rating is in the history; failed edits are not"
    );
    f.s.set_write_hook(None);
    let settings = f.root.join("Capture/.ds/settings");
    assert_eq!(fs::read_dir(settings).unwrap().count(), 1, "no temp debris");
}

#[test]
fn partial_batch_failure_keeps_index_consistent_with_sidecars() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..40)
        .map(|i| f.add(&format!("Capture/{i:02}.ARW"), i).1)
        .collect();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let hook: WriteHook = Arc::new(move |p, _| {
        // Fail every 7th write, whichever thread it lands on.
        if n2.fetch_add(1, Ordering::SeqCst) % 7 == 3 {
            Err(io::Error::other(format!("disk full at {}", p.display())))
        } else {
            Ok(())
        }
    });
    f.s.set_write_hook(Some(hook));
    assert!(f.s.set_rating(&vs, r(4)).is_err());
    f.s.set_write_hook(None);
    let indexed: Vec<Rating> = vs.iter().map(|v| f.s.variant(*v).unwrap().rating).collect();
    let on_disk: Vec<Rating> = vs.iter().map(|v| f.sidecar(*v).rating).collect();
    assert_eq!(indexed, on_disk);
    let written = on_disk.iter().filter(|x| **x == r(4)).count();
    assert!(written > 0 && written < 40, "{written}");
    assert_eq!(n.load(Ordering::SeqCst), 40);
}

#[test]
fn concurrent_sessions_on_one_folder_stay_consistent() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..20)
        .map(|i| f.add(&format!("Capture/{i:02}.ARW"), i).1)
        .collect();
    let root = f.root.clone();
    std::thread::scope(|s| {
        for (t, chunk) in vs.chunks(10).enumerate() {
            let root = &root;
            s.spawn(move || {
                let mut sess = Session::open(root).unwrap();
                for (i, v) in chunk.iter().enumerate() {
                    sess.set_rating(&[*v], r(u8::try_from(t + 1).unwrap()))
                        .unwrap();
                    sess.add_keyword(&[*v], &format!("k{i}")).unwrap();
                }
            });
        }
        // Both also hammer the same variant: last writer wins, never torn.
        for _ in 0..2 {
            let root = &root;
            let v = vs[0];
            s.spawn(move || {
                let mut sess = Session::open(root).unwrap();
                for i in 0..10 {
                    sess.rename_variant(v, &format!("n{i}")).unwrap();
                }
            });
        }
    });
    f.s.sync_index().unwrap();
    for (i, v) in vs.iter().enumerate() {
        let info = f.s.variant(*v).unwrap();
        assert_eq!(info.rating, r(if i < 10 { 1 } else { 2 }));
        assert_eq!(f.sidecar(*v).rating, info.rating);
    }
    assert_eq!(f.s.variant(vs[0]).unwrap().name, "n9");
}

// ------------------------------------------------------------ edits / undo ---

struct ManualClock(Mutex<Instant>);

impl ManualClock {
    fn advance(&self, ms: u64) {
        *self.0.lock().unwrap() += Duration::from_millis(ms);
    }
}

#[test]
fn slider_drags_coalesce_and_undo_redo_round_trip() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
    let c2 = clock.clone();
    f.s.set_clock(move || *c2.0.lock().unwrap());
    for i in 1..=10 {
        f.s.save_adjustments(v, &ev(i as f32 * 0.1)).unwrap();
        clock.advance(100);
    }
    clock.advance(1000);
    f.s.save_adjustments(v, &ev(2.0)).unwrap();
    assert_eq!(f.s.undo_depth(v), 2);
    assert!(f.s.variant(v).unwrap().edited);

    assert!(f.s.undo(v).unwrap());
    assert_eq!(f.s.load_adjustments(v).unwrap(), ev(1.0));
    assert!(f.s.undo(v).unwrap());
    assert_eq!(f.s.load_adjustments(v).unwrap(), Adjustments::default());
    assert!(!f.s.variant(v).unwrap().edited);
    assert!(!f.s.undo(v).unwrap());
    assert_eq!(f.s.redo_depth(v), 2);
    assert!(f.s.redo(v).unwrap());
    assert!(f.s.redo(v).unwrap());
    assert_eq!(f.s.load_adjustments(v).unwrap(), ev(2.0));
    assert!(!f.s.redo(v).unwrap());
}

#[test]
fn undo_history_is_bounded() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    f.s.set_undo_config(UndoConfig {
        max_steps: 5,
        coalesce: Duration::ZERO,
    });
    for i in 0..20 {
        f.s.save_adjustments(v, &ev(i as f32 / 10.0 + 0.1)).unwrap();
    }
    assert_eq!(f.s.undo_depth(v), 5);
    while f.s.undo(v).unwrap() {}
    assert_eq!(f.s.load_adjustments(v).unwrap(), ev(1.5));
}

#[test]
fn batch_apply_is_one_undo_step_across_variants() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..30)
        .map(|i| f.add(&format!("Capture/{i:02}.ARW"), i).1)
        .collect();
    f.s.save_adjustments(vs[0], &ev(0.5)).unwrap();
    let edits: Vec<(VariantId, Adjustments)> = vs.iter().map(|v| (*v, ev(1.0))).collect();
    let g = f.s.apply_batch(&edits).unwrap().unwrap();
    assert!(
        vs.iter()
            .all(|v| f.s.load_adjustments(*v).unwrap() == ev(1.0))
    );
    // Variant 5 is edited again after the batch; undo keeps that later edit.
    f.s.save_adjustments(vs[5], &ev(3.0)).unwrap();
    assert!(f.s.undo(vs[10]).unwrap());
    assert_eq!(f.s.load_adjustments(vs[0]).unwrap(), ev(0.5));
    assert_eq!(f.s.load_adjustments(vs[1]).unwrap(), Adjustments::default());
    assert_eq!(f.s.load_adjustments(vs[5]).unwrap(), ev(3.0));
    assert!(f.s.redo_group(g).unwrap());
    assert_eq!(f.s.load_adjustments(vs[29]).unwrap(), ev(1.0));
    assert!(f.s.undo_group(g).unwrap());
    assert!(!f.s.undo_group(g).unwrap() || f.s.load_adjustments(vs[5]).unwrap() == ev(1.0));
    assert_eq!(
        f.s.apply_batch(&[(vs[1], Adjustments::default())]).unwrap(),
        None,
        "no-op batch"
    );
}

#[test]
fn copy_apply_uses_each_targets_orientation() {
    let mut f = fx();
    let (_, src) = f.add("Capture/land.ARW", 0);
    let h = put(&f.root, "Capture/port.ARW", b"p");
    let mut pm = meta(1);
    pm.orientation = Orientation::Rotate90; // stored landscape, displayed portrait
    let port = f.s.add_original("Capture/port.ARW", h, pm).unwrap();
    let port = f.s.variants(port).unwrap()[0];
    let (_, other) = f.add("Capture/other.ARW", 2);
    f.s.save_adjustments(other, &ev(-1.0)).unwrap();

    let mut source = ev(0.7);
    source.crop = Crop {
        rect: NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.6,
            h: 0.6,
        },
        aspect: Some(Aspect { long: 3, short: 2 }),
        ..Crop::default()
    };
    let clip = Clipboard::new(source, Adjustments::default(), Size::new(6000, 4000));
    let opts = PasteOptions {
        groups: [AdjustGroup::Exposure, AdjustGroup::Crop]
            .into_iter()
            .collect(),
        relative: [AdjustGroup::Exposure].into_iter().collect(),
        ..PasteOptions::default()
    };
    let applied =
        f.s.apply_clipboard(&clip, &[src, port, other], &opts)
            .unwrap();
    assert_eq!(applied.wb_absolute_fallback, [] as [session::VariantId; 0]);
    let g = applied.group.unwrap();
    let pc = f.s.load_adjustments(port).unwrap().crop.rect;
    let (pw, ph) = (f64::from(pc.w) * 4000.0, f64::from(pc.h) * 6000.0);
    assert!(
        (ph / pw - 1.5).abs() < 1e-3,
        "portrait target gets a 2:3 crop, got {pw}x{ph}"
    );
    assert_eq!(
        f.s.load_adjustments(other).unwrap().exposure.exposure_ev,
        -0.3,
        "relative exposure"
    );
    assert!(f.s.undo_group(g).unwrap());
    assert_eq!(f.s.load_adjustments(other).unwrap(), ev(-1.0));
}

#[test]
fn relative_wb_paste_uses_as_shot_and_reports_fallbacks() {
    use ds_core::adjust::WhiteBalance;
    let mut f = fx();
    let mut m = meta(0);
    m.as_shot_wb = Some((5000.0, 0.0));
    let h = put(&f.root, "Capture/src.ARW", b"s");
    let src = f.s.add_original("Capture/src.ARW", h, m.clone()).unwrap();
    let src = f.s.variants(src).unwrap()[0];
    m.as_shot_wb = Some((4000.0, 2.0));
    let h = put(&f.root, "Capture/t.ARW", b"t");
    let known = f.s.add_original("Capture/t.ARW", h, m).unwrap();
    let known = f.s.variants(known).unwrap()[0];
    let (_, unknown) = f.add("Capture/u.ARW", 2); // as-shot WB unknown
    // Source warmed from as-shot 5000 K to 5500 K (+10%), tint +4.
    let warm = Adjustments {
        white_balance: WhiteBalance::Custom {
            temp_k: 5500.0,
            tint: 4.0,
        },
        ..Adjustments::default()
    };
    f.s.save_adjustments(src, &warm).unwrap();
    let clip = f.s.copy_adjustments(src).unwrap();
    assert_eq!(clip.source_as_shot, Some((5000.0, 0.0)));
    let opts = PasteOptions {
        groups: [AdjustGroup::WhiteBalance].into_iter().collect(),
        relative: [AdjustGroup::WhiteBalance].into_iter().collect(),
        ..PasteOptions::default()
    };
    let applied =
        f.s.apply_clipboard(&clip, &[known, unknown], &opts)
            .unwrap();
    assert_eq!(applied.wb_absolute_fallback, vec![unknown]);
    match f.s.load_adjustments(known).unwrap().white_balance {
        WhiteBalance::Custom { temp_k, tint } => {
            assert!((temp_k - 4400.0).abs() < 1.0, "{temp_k}");
            assert!((tint - 6.0).abs() < 1e-3, "{tint}");
        }
        WhiteBalance::AsShot => panic!("expected a custom WB"),
    }
    assert_eq!(
        f.s.load_adjustments(unknown).unwrap().white_balance,
        warm.white_balance
    );
    assert_eq!(
        f.s.image(f.s.variant(known).unwrap().image)
            .unwrap()
            .meta
            .as_shot_wb,
        Some((4000.0, 2.0))
    );
}

#[test]
fn presets_apply_as_group_and_auto_apply_on_import() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let mut a = Adjustments::default();
    a.detail.sharpen_amount = 250.0;
    let preset = Preset::new("A9 base", a, [AdjustGroup::Detail].into_iter().collect());
    let g = f.s.apply_preset(&preset, &[v]).unwrap();
    assert!(g.is_some());
    assert_eq!(
        f.s.load_adjustments(v).unwrap().detail.sharpen_amount,
        250.0
    );

    assert!(f.s.auto_apply_preset().is_none());
    f.s.set_auto_apply_preset(Some(preset.clone())).unwrap();
    f.reopen();
    assert_eq!(f.s.auto_apply_preset(), Some(&preset));
    let (_, v2) = f.add("Capture/b.ARW", 1);
    assert_eq!(
        f.s.load_adjustments(v2).unwrap().detail.sharpen_amount,
        250.0
    );
    assert!(f.s.variant(v2).unwrap().edited);
    // Clips are never developed: they are added without the preset.
    let h = put(&f.root, "Capture/clip.MP4", b"mp4");
    let ids =
        f.s.add_new_originals_with(
            vec![NewOriginal {
                rel: "Capture/clip.MP4".into(),
                hash: h,
                content_hash: None,
                meta: meta(9),
                replaced: Vec::new(),
                iptc: ds_core::IptcMeta::default(),
            }],
            false,
        )
        .unwrap();
    let clip = f.s.variants(ids[0]).unwrap()[0];
    assert!(!f.s.variant(clip).unwrap().edited);
    f.s.set_auto_apply_preset(None).unwrap();
    let (_, v3) = f.add("Capture/c.ARW", 2);
    assert!(!f.s.variant(v3).unwrap().edited);
}

// ------------------------------------------------------------------ filters ---

#[test]
fn filters_and_sorting() {
    let mut f = fx();
    let mut vs = Vec::new();
    for i in 0..12 {
        let h = put(
            &f.root,
            &format!("Capture/f{i:02}.ARW"),
            format!("{i}").as_bytes(),
        );
        let mut m = meta(i);
        if i % 3 == 0 {
            m.camera = Some("ILCE-7RM5".into());
            m.lens = Some("FE 90mm F2.8 Macro G".into());
        }
        if i == 11 {
            m.capture_time_ms = None;
        }
        let id =
            f.s.add_original(&format!("Capture/f{i:02}.ARW"), h, m)
                .unwrap();
        vs.push(f.s.variants(id).unwrap()[0]);
    }
    for (i, v) in vs.iter().enumerate() {
        f.s.set_rating(&[*v], r(u8::try_from(i % 6).unwrap()))
            .unwrap();
    }
    f.s.set_color_tag(&vs[0..2], ColorTag::Green).unwrap();
    f.s.set_color_tag(&vs[2..3], ColorTag::Blue).unwrap();
    f.s.set_pick(&vs[3..5], PickState::Picked).unwrap();
    f.s.add_keyword(&vs[6..7], "50%_off").unwrap();
    f.s.add_keyword(&vs[7..8], "500 off").unwrap();
    f.s.save_adjustments(vs[8], &ev(0.3)).unwrap();
    f.s.move_to(&vs[9..10], Folder::Trash).unwrap();
    f.s.move_to(&vs[10..11], Folder::Selects).unwrap();

    let q = |filter: Filter| f.s.query(&filter, Sort::default()).unwrap();
    let idx = |got: Vec<VariantId>| {
        got.iter()
            .map(|v| vs.iter().position(|x| x == v).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        idx(q(Filter::default())),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11],
        "trash excluded, null time last"
    );
    assert_eq!(
        idx(q(Filter {
            min_rating: Rating::new(4),
            ..Filter::default()
        })),
        [4, 5, 10, 11]
    );
    assert_eq!(
        idx(q(Filter {
            min_rating: Rating::new(0),
            ..Filter::default()
        }))
        .len(),
        11
    );
    assert_eq!(
        idx(q(Filter {
            tags: vec![ColorTag::Green, ColorTag::Blue],
            ..Filter::default()
        })),
        [0, 1, 2]
    );
    assert_eq!(
        idx(q(Filter {
            pick: Some(PickState::Picked),
            ..Filter::default()
        })),
        [3, 4]
    );
    assert_eq!(
        idx(q(Filter {
            text: Some("50%_".into()),
            ..Filter::default()
        })),
        [6],
        "LIKE wildcards escaped"
    );
    assert_eq!(
        idx(q(Filter {
            text: Some("F07".into()),
            ..Filter::default()
        })),
        [7],
        "file name, case-insensitive"
    );
    assert_eq!(
        idx(q(Filter {
            camera: Some("7rm5".into()),
            ..Filter::default()
        })),
        [0, 3, 6]
    );
    assert_eq!(
        idx(q(Filter {
            lens: Some("macro".into()),
            camera: Some(String::new()),
            ..Filter::default()
        })),
        [0, 3, 6]
    );
    let t0 = 1_700_000_000_000;
    assert_eq!(
        idx(q(Filter {
            date_from_ms: Some(t0 + 2000),
            date_to_ms: Some(t0 + 4000),
            ..Filter::default()
        })),
        [2, 3, 4]
    );
    assert_eq!(
        idx(q(Filter {
            iso_min: Some(1000),
            iso_max: Some(1100),
            ..Filter::default()
        })),
        [10]
    );
    assert_eq!(
        idx(q(Filter {
            edited: Some(true),
            ..Filter::default()
        })),
        [8]
    );
    assert_eq!(
        idx(q(Filter {
            edited: Some(false),
            ..Filter::default()
        }))
        .len(),
        10
    );
    assert_eq!(
        idx(q(Filter {
            folder: Some(Folder::Trash),
            ..Filter::default()
        })),
        [9]
    );
    assert_eq!(
        idx(q(Filter {
            folder: Some(Folder::Selects),
            ..Filter::default()
        })),
        [10]
    );

    let sorted = |key, descending| {
        idx(f
            .s
            .query(&Filter::default(), Sort { key, descending })
            .unwrap())
    };
    assert_eq!(
        sorted(SortKey::Name, false),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11]
    );
    assert_eq!(
        sorted(SortKey::Name, true),
        [11, 10, 8, 7, 6, 5, 4, 3, 2, 1, 0]
    );
    assert_eq!(sorted(SortKey::Rating, true)[..4], [5, 11, 4, 10]);
    assert_eq!(sorted(SortKey::CaptureTime, true)[0], 10);
}

// ------------------------------------------------------------------- albums ---

#[test]
fn manual_and_smart_albums() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..6)
        .map(|i| f.add(&format!("Capture/{i}.ARW"), i).1)
        .collect();
    f.s.set_rating(&vs[..3], r(5)).unwrap();
    let a = f.s.create_album("Portfolio").unwrap();
    f.s.add_to_album(a, &[vs[4], vs[1], vs[0], vs[4]]).unwrap();
    assert_eq!(f.s.album_variants(a).unwrap(), vec![vs[4], vs[1], vs[0]]);
    assert_eq!(
        f.s.query_album(
            a,
            &Filter {
                min_rating: Rating::new(5),
                ..Filter::default()
            },
            Sort::by(SortKey::Name)
        )
        .unwrap(),
        vec![vs[0], vs[1]]
    );
    let smart =
        f.s.create_smart_album(
            "Five stars",
            Filter {
                min_rating: Rating::new(5),
                ..Filter::default()
            },
            Sort::default(),
        )
        .unwrap();
    assert_eq!(f.s.album_variants(smart).unwrap(), vs[..3].to_vec());
    assert_eq!(
        f.s.query_album(smart, &Filter::default(), Sort::default())
            .unwrap()
            .len(),
        6
    );
    assert!(matches!(
        f.s.add_to_album(smart, &[vs[5]]),
        Err(SessionError::SmartAlbum(_))
    ));
    assert!(matches!(
        f.s.add_to_album(a, &[VariantId(7)]),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        f.s.create_album("  "),
        Err(SessionError::InvalidName(_))
    ));
    assert!(matches!(
        f.s.rename_album(a, ""),
        Err(SessionError::InvalidName(_))
    ));
    f.s.rename_album(a, "Best").unwrap();
    f.s.remove_from_album(a, &[vs[1]]).unwrap();

    // Survives reopen and a full index rebuild.
    fs::remove_file(f.s.db_path()).unwrap();
    f.reopen();
    assert_eq!(f.s.albums().len(), 2);
    assert_eq!(f.s.album(a).unwrap().name, "Best");
    assert_eq!(f.s.album_variants(a).unwrap(), vec![vs[4], vs[0]]);
    f.s.rebuild_index().unwrap();
    assert_eq!(f.s.album_variants(a).unwrap(), vec![vs[4], vs[0]]);

    // Emptying the trash drops members.
    f.s.move_to(&[vs[4]], Folder::Trash).unwrap();
    f.s.empty_trash(&FakeTrash::new(f.root.parent().unwrap().join("bin")))
        .unwrap();
    assert_eq!(f.s.album_variants(a).unwrap(), vec![vs[0]]);
    f.s.delete_album(a).unwrap();
    assert!(matches!(
        f.s.delete_album(a),
        Err(SessionError::NotFound { kind: "album", .. })
    ));
    assert!(f.s.album(a).is_err());
    assert!(matches!(
        f.s.album_variants(AlbumId(99)),
        Err(SessionError::NotFound { .. })
    ));
}

#[test]
fn large_batch_edits_are_chunked_but_all_or_nothing_up_front() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..120)
        .map(|i| f.add(&format!("Capture/{i}.ARW"), 0).1)
        .collect();
    // An unknown id in the last chunk: nothing is written anywhere.
    let mut bad = vs.clone();
    bad.push(VariantId(987_654_321));
    assert!(f.s.set_rating(&bad, r(5)).is_err());
    assert!(vs.iter().all(|&v| f.s.variant(v).unwrap().rating == r(0)));
    // Duplicates across chunks are applied once.
    let mut twice = vs.clone();
    twice.extend(vs.iter().copied());
    f.s.set_rating(&twice, r(5)).unwrap();
    assert!(vs.iter().all(|&v| f.s.variant(v).unwrap().rating == r(5)));
    // The sidecars on disk agree with the index after a rebuild.
    f.s.rebuild_index().unwrap();
    assert!(vs.iter().all(|&v| f.s.variant(v).unwrap().rating == r(5)));
}

#[test]
fn large_batch_with_a_too_new_sidecar_writes_nothing() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..120)
        .map(|i| f.add(&format!("Capture/{i}.ARW"), 0).1)
        .collect();
    // The last chunk's sidecar comes from a newer build (read-only).
    let rel = f.s.locate(vs[119]).unwrap().sidecar;
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join(&rel)).unwrap()).unwrap();
    json["schema_version"] = 99.into();
    fs::write(f.root.join(&rel), serde_json::to_vec(&json).unwrap()).unwrap();
    f.reopen();
    assert!(matches!(
        f.s.set_rating(&vs, r(5)),
        Err(SessionError::SidecarTooNew { found: 99, .. })
    ));
    assert!(
        vs.iter().all(|&v| f.s.variant(v).unwrap().rating == r(0)),
        "no chunk written"
    );
}

#[test]
fn a_bad_albums_file_is_moved_aside_and_the_session_still_opens() {
    for bad in [
        b"[".to_vec(),
        br#"{"albums":[{"id":1,"name":"x","kind":"from the future"}]}"#.to_vec(),
        vec![b' '; ds_core::limits::MAX_JSON_BYTES as usize + 1],
    ] {
        let mut f = fx();
        let (_, v) = f.add("Capture/a.ARW", 0);
        f.s.set_rating(&[v], r(4)).unwrap();
        let path = f.root.join(".ds/settings/albums.json");
        fs::write(&path, &bad).unwrap();
        let s = Session::open(&f.root).unwrap();
        let errs = &s.open_report().errors;
        assert!(
            errs.iter()
                .any(|(p, why)| p.ends_with("albums.json") && why.contains("corrupt")),
            "{errs:?}"
        );
        assert_eq!(
            s.variant(v).unwrap().rating,
            r(4),
            "photos and edits are intact"
        );
        let aside: Vec<_> = fs::read_dir(f.root.join(".ds/settings"))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("albums.json.corrupt-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "the bad file is kept");
        assert_eq!(fs::read(aside[0].path()).unwrap(), bad);
        assert!(!path.exists() || s.albums().iter().all(|a| a.builtin.is_some()));
    }
}

// ------------------------------------------------------------ move / trash ---

#[test]
fn move_to_moves_originals_and_all_sidecars_and_uniquifies() {
    let mut f = fx();
    let (img, v1) = f.add("Capture/Day 1/a.ARW", 0);
    let v2 = f.s.new_variant(img).unwrap();
    f.s.set_rating(&[v2], r(3)).unwrap();
    // Something already sits at the destination name.
    put(&f.root, "Selects/Day 1/a.ARW", b"unrelated file");
    let moved = f.s.move_to(&[v2, v1], Folder::Selects).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].to.as_str(), "Selects/Day 1/a-2.ARW");
    assert_eq!(
        fs::read(f.root.join("Selects/Day 1/a.ARW")).unwrap(),
        b"unrelated file"
    );
    assert!(f.root.join("Selects/Day 1/a-2.ARW").is_file());
    assert!(!f.root.join("Capture/Day 1/a.ARW").exists());
    assert!(
        f.root
            .join("Selects/Day 1/.ds/settings/a-2.ARW.json")
            .is_file()
    );
    assert!(
        f.root
            .join("Selects/Day 1/.ds/settings/v2/a-2.ARW.json")
            .is_file()
    );
    assert_eq!(f.s.variant(v2).unwrap().rating, r(3));
    assert!(
        f.s.move_to(&[v1], Folder::Selects).unwrap().is_empty(),
        "already there"
    );
    // Rebuild agrees with the new layout.
    f.s.rebuild_index().unwrap();
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Selects/Day 1/a-2.ARW"
    );
    assert_eq!(f.s.variants(img).unwrap(), vec![v1, v2]);
    f.s.move_to(&[v1], Folder::Capture).unwrap();
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Capture/Day 1/a-2.ARW"
    );
}

#[test]
fn move_to_rolls_back_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fx();
    let (a, va) = f.add("Capture/a.ARW", 0);
    let (b, vb) = f.add("Capture/sub/b.ARW", 1);
    // b's destination directory is not writable: the batch fails midway,
    // after a has already moved.
    let locked = f.root.join("Trash/sub");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let res = f.s.move_to(&[va, vb], Folder::Trash);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(res.is_err());
    for (img, rel) in [(a, "Capture/a.ARW"), (b, "Capture/sub/b.ARW")] {
        assert_eq!(f.s.image(img).unwrap().rel_path.as_str(), rel);
        assert!(f.root.join(rel).is_file(), "{rel} restored");
    }
    assert!(f.root.join("Capture/.ds/settings/a.ARW.json").is_file());
    assert!(!f.root.join("Trash/a.ARW").exists());
    assert_eq!(
        f.s.sync_index().unwrap().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
}

#[test]
fn move_to_avoids_occupied_sidecar_locations() {
    let mut f = fx();
    let (b, vb) = f.add("Capture/b.ARW", 1);
    // A stray directory sits where b's sidecar would go.
    fs::create_dir_all(f.root.join("Trash/.ds/settings/b.ARW.json")).unwrap();
    let moved = f.s.move_to(&[vb], Folder::Trash).unwrap();
    assert_eq!(moved[0].to.as_str(), "Trash/b-2.ARW");
    assert_eq!(f.s.image(b).unwrap().rel_path.as_str(), "Trash/b-2.ARW");
    assert!(f.root.join("Trash/.ds/settings/b-2.ARW.json").is_file());
}

struct FakeTrash {
    bin: PathBuf,
    fail: bool,
    got: Mutex<Vec<PathBuf>>,
}

impl FakeTrash {
    fn new(bin: PathBuf) -> Self {
        fs::create_dir_all(&bin).unwrap();
        Self {
            bin,
            fail: false,
            got: Mutex::new(Vec::new()),
        }
    }
}

impl Trasher for FakeTrash {
    fn trash(&self, paths: &[PathBuf]) -> Result<(), String> {
        if self.fail {
            return Err("Finder said no".into());
        }
        for (i, p) in paths.iter().enumerate() {
            fs::rename(
                p,
                self.bin
                    .join(format!("{i}-{}", p.file_name().unwrap().to_str().unwrap())),
            )
            .unwrap();
        }
        self.got.lock().unwrap().extend_from_slice(paths);
        Ok(())
    }
}

#[test]
fn empty_trash_hands_files_to_trasher_and_drops_rows() {
    let mut f = fx();
    let (img, v) = f.add("Capture/a.ARW", 0);
    let v2 = f.s.new_variant(img).unwrap();
    let (_, keep) = f.add("Capture/b.ARW", 1);
    f.s.save_adjustments(v, &ev(1.0)).unwrap();
    f.s.move_to(&[v], Folder::Trash).unwrap();

    let mut failing = FakeTrash::new(f.root.parent().unwrap().join("bin"));
    failing.fail = true;
    assert!(matches!(
        f.s.empty_trash(&failing),
        Err(SessionError::Trash(_))
    ));
    assert!(f.s.variant(v).is_ok(), "nothing dropped on failure");
    assert!(f.root.join("Trash/a.ARW").is_file());

    let bin = FakeTrash::new(f.root.parent().unwrap().join("bin2"));
    assert_eq!(f.s.empty_trash(&bin).unwrap(), 1);
    assert_eq!(bin.got.lock().unwrap().len(), 3, "original + two sidecars");
    assert!(!f.root.join("Trash/a.ARW").exists());
    assert!(f.s.variant(v).is_err() && f.s.variant(v2).is_err());
    assert_eq!(f.s.undo_depth(v), 0);
    assert!(f.s.variant(keep).is_ok());
    assert_eq!(f.s.empty_trash(&bin).unwrap(), 0);
    assert_eq!(f.s.rebuild_index().unwrap().indexed, 1);
}

// ------------------------------------------------------------------- rescan ---

#[test]
fn rescan_relinks_moved_files_by_hash_and_reports_missing_and_untracked() {
    let mut f = fx();
    let (a, va) = f.add("Capture/a.ARW", 0);
    let v2 = f.s.new_variant(a).unwrap();
    f.s.save_adjustments(v2, &ev(0.4)).unwrap();
    let (b, _) = f.add("Capture/b.ARW", 1);
    let (c, _) = f.add("Capture/c.ARW", 2);
    // Finder: a renamed into a subfolder, b deleted, c replaced by other bytes, a stray file.
    fs::create_dir_all(f.root.join("Selects/picked")).unwrap();
    fs::rename(
        f.root.join("Capture/a.ARW"),
        f.root.join("Selects/picked/renamed.ARW"),
    )
    .unwrap();
    fs::remove_file(f.root.join("Capture/b.ARW")).unwrap();
    fs::rename(
        f.root.join("Capture/c.ARW"),
        f.root.join("Capture/c-moved.ARW"),
    )
    .unwrap();
    fs::write(
        f.root.join("Capture/c-moved.ARW"),
        b"different content, same name",
    )
    .unwrap();
    put(&f.root, "Capture/stray.JPG", b"x");

    let rep = f.s.rescan().unwrap();
    assert_eq!(rep.relinked.len(), 1);
    assert_eq!(
        (rep.relinked[0].image, rep.relinked[0].to.as_str()),
        (a, "Selects/picked/renamed.ARW")
    );
    let missing: Vec<ImageId> = rep.missing.iter().map(|(id, _)| *id).collect();
    assert_eq!(missing, vec![b, c]);
    assert_eq!(rep.untracked, ["Capture/c-moved.ARW", "Capture/stray.JPG"]);
    assert!(f.s.image(b).unwrap().missing);
    assert!(
        f.root
            .join("Selects/picked/.ds/settings/renamed.ARW.json")
            .is_file()
    );
    assert!(
        f.root
            .join("Selects/picked/.ds/settings/v2/renamed.ARW.json")
            .is_file()
    );
    assert_eq!(f.s.load_adjustments(v2).unwrap(), ev(0.4));
    assert_eq!(f.s.variants(a).unwrap(), vec![va, v2]);

    // The file comes back: the flag clears.
    put(&f.root, "Capture/b.ARW", b"raw-bytes-Capture/b.ARW");
    let rep = f.s.rescan().unwrap();
    assert!(!f.s.image(b).unwrap().missing);
    assert_eq!(rep.missing.len(), 1);
    // A rebuild agrees.
    fs::remove_file(f.s.db_path()).unwrap();
    f.reopen();
    assert_eq!(
        f.s.image(a).unwrap().rel_path.as_str(),
        "Selects/picked/renamed.ARW"
    );
}

#[test]
fn folder_moved_with_its_sidecars_needs_no_hashing() {
    let mut f = fx();
    let (a, v) = f.add("Capture/day/a.ARW", 0);
    f.s.set_rating(&[v], r(4)).unwrap();
    fs::rename(
        f.root.join("Capture/day"),
        f.root.join("Capture/day-renamed"),
    )
    .unwrap();
    let rep = f.s.rescan().unwrap();
    assert!(
        rep.relinked.is_empty() && rep.missing.is_empty() && rep.untracked.is_empty(),
        "{rep:?}"
    );
    assert_eq!(
        f.s.image(a).unwrap().rel_path.as_str(),
        "Capture/day-renamed/a.ARW"
    );
    assert_eq!(f.s.variant(v).unwrap().rating, r(4));
}

// ------------------------------------------------------------- global index ---

#[test]
fn global_index_dedupe_and_search() {
    let d = tempfile::tempdir().unwrap();
    let gpath = d.path().join("support/global.db");
    let mut g = GlobalIndex::open(&gpath).unwrap();
    let mut f = fx();
    let (a, _) = f.add("Capture/a.ARW", 0);
    f.add("Capture/b.ARW", 1);
    assert_eq!(g.sync_session(&f.s).unwrap(), 2);
    let ha = f.s.image(a).unwrap().hash;
    assert!(g.contains(&ha).unwrap());
    assert!(!g.contains(&FileHash([9; 32])).unwrap());
    let hits = g.lookup(&ha).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        (hits[0].session_root.as_path(), hits[0].rel_path.as_str()),
        (f.root.as_path(), "Capture/a.ARW")
    );
    assert_eq!(hits[0].session_name, "2026-09-25 Fall Hike");

    let other = d.path().join("Other Session");
    g.add_original(&other, "Capture/copy.ARW", &ha).unwrap();
    g.add_original(&other, "Capture/copy2.ARW", &ha).unwrap();
    assert_eq!(g.lookup(&ha).unwrap().len(), 3);
    // Every original, for import dedupe (sorted by root then path).
    let all = g.originals().unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.contains(&(other.clone(), "Capture/copy2.ARW".into(), ha)));
    assert!(all.contains(&(f.root.clone(), "Capture/a.ARW".into(), ha)));
    let sessions = g.sessions().unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["2026-09-25 Fall Hike", "Other Session"]
    );

    // Re-sync after the trash is emptied drops the hash for that session.
    let va = f.s.variants(a).unwrap()[0];
    f.s.move_to(&[va], Folder::Trash).unwrap();
    f.s.empty_trash(&FakeTrash::new(d.path().join("bin")))
        .unwrap();
    g.sync_session(&f.s).unwrap();
    assert_eq!(g.lookup(&ha).unwrap().len(), 2);
    assert!(g.unregister_session(&other).unwrap());
    assert!(!g.unregister_session(&other).unwrap());
    assert!(!g.contains(&ha).unwrap());

    let snap = d.path().join("global-snap.db");
    g.snapshot_to(&snap).unwrap();
    drop(g);
    let g = GlobalIndex::open(&snap).unwrap();
    assert_eq!(g.sessions().unwrap().len(), 1);
    drop(g);

    // Corrupt or foreign-version files are replaced, not fatal.
    fs::write(&gpath, vec![1u8; 4096]).unwrap();
    for s in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{s}", gpath.display()));
    }
    assert_eq!(
        GlobalIndex::open(&gpath).unwrap().sessions().unwrap(),
        [] as [global::RegisteredSession; 0]
    );
    let c = rusqlite::Connection::open(&gpath).unwrap();
    c.execute_batch("PRAGMA user_version = 7").unwrap();
    drop(c);
    // A newer build's index is left alone, not wiped.
    assert!(matches!(
        GlobalIndex::open(&gpath),
        Err(SessionError::IndexTooNew { found: 7, .. })
    ));
    assert!(gpath.exists());
    assert!(
        GlobalIndex::default_path()
            .unwrap()
            .ends_with("Darkslide/global.db")
    );
}

// -------------------------------------------------------------------- scale ---

#[test]
fn ten_thousand_images() {
    let mut f = fx();
    let n = 10_000;
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let rel = format!("Capture/{:02}/DSC{i:05}.ARW", i / 1000);
        let h = put(&f.root, &rel, &(i as u64).to_le_bytes());
        items.push((rel, h, meta(i as i64)));
    }
    let t = Instant::now();
    let ids = f.s.add_originals(items).unwrap();
    let add = t.elapsed();
    assert_eq!(ids.len(), n);
    let vs: Vec<VariantId> = f.s.all_variants().unwrap();
    f.s.set_rating(&vs[..2000], r(3)).unwrap();

    let filter = Filter {
        min_rating: Rating::new(3),
        text: Some("dsc0".into()),
        ..Filter::default()
    };
    let t = Instant::now();
    let hits = f.s.query(&filter, Sort::default()).unwrap();
    let query = t.elapsed();
    assert_eq!(hits.len(), 2000 - count_non_dsc0(&f.s, &vs[..2000]));

    let t = Instant::now();
    fs::remove_file(f.s.db_path()).unwrap();
    f.reopen();
    let reopen = t.elapsed();
    assert_eq!(f.s.image_count().unwrap(), n);
    let t = Instant::now();
    f.reopen();
    let warm_open = t.elapsed();
    eprintln!(
        "10k: add {add:?}, filter {query:?}, rebuild-open {reopen:?}, warm open {warm_open:?}"
    );
}

/// Variants among `vs` whose file name lacks `dsc0` (the `DSC1xxxx` files).
fn count_non_dsc0(s: &Session, vs: &[VariantId]) -> usize {
    vs.iter()
        .filter(|v| {
            let img = s.variant(**v).unwrap().image;
            !s.image(img)
                .unwrap()
                .rel_path
                .file_name()
                .to_lowercase()
                .contains("dsc0")
        })
        .count()
}

#[test]
fn creation_never_overwrites_sidecars_the_index_lost() {
    let mut f = fx();
    let (img, v1) = f.add("Capture/a.ARW", 0);
    // An unparseable v2 sidecar the index cannot know about.
    fs::create_dir_all(f.root.join("Capture/.ds/settings/v2")).unwrap();
    fs::write(
        f.root.join("Capture/.ds/settings/v2/a.ARW.json"),
        b"{precious",
    )
    .unwrap();
    let v = f.s.new_variant(img).unwrap();
    assert_eq!(f.s.variant(v).unwrap().ordinal, 3);
    assert_eq!(
        fs::read(f.root.join("Capture/.ds/settings/v2/a.ARW.json")).unwrap(),
        b"{precious"
    );
    // Same for a new original whose sidecar location is occupied.
    let h = put(&f.root, "Capture/b.ARW", b"b");
    fs::write(f.root.join("Capture/.ds/settings/b.ARW.json"), b"{precious").unwrap();
    assert!(matches!(
        f.s.add_original("Capture/b.ARW", h, meta(1)),
        Err(SessionError::DuplicatePath(_))
    ));
    assert_eq!(
        fs::read(f.root.join("Capture/.ds/settings/b.ARW.json")).unwrap(),
        b"{precious"
    );
    assert_eq!(f.s.variants(img).unwrap(), vec![v1, v]);
}

#[test]
fn concurrent_new_variants_get_distinct_sidecars() {
    let mut f = fx();
    let (img, _) = f.add("Capture/a.ARW", 0);
    let root = f.root.clone();
    let made: Vec<VariantId> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let root = &root;
                s.spawn(move || {
                    let mut sess = Session::open(root).unwrap();
                    (0..5)
                        .map(|_| sess.new_variant(img).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    f.s.sync_index().unwrap();
    assert_eq!(f.s.variants(img).unwrap().len(), 21);
    let mut ords: Vec<u32> = made
        .iter()
        .map(|v| f.s.variant(*v).unwrap().ordinal)
        .collect();
    ords.sort_unstable();
    assert_eq!(ords, (2..=21).collect::<Vec<u32>>());
}

#[test]
fn organizational_edits_are_undoable_single_and_batch() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..3)
        .map(|i| f.add(&format!("Capture/{i}.ARW"), i).1)
        .collect();
    f.s.set_keywords(&vs[..1], &["beach".into()]).unwrap();
    f.s.set_color_tag(&vs, ColorTag::Red).unwrap(); // one group step
    f.s.set_pick(&vs[..1], PickState::Picked).unwrap();
    assert!(f.s.undo(vs[0]).unwrap());
    assert_eq!(f.s.variant(vs[0]).unwrap().pick, PickState::Unflagged);
    assert!(
        f.s.undo(vs[0]).unwrap(),
        "undoes the batch tag for all three"
    );
    for v in &vs {
        assert_eq!(f.s.variant(*v).unwrap().color_tag, ColorTag::None);
    }
    assert!(f.s.redo(vs[2]).unwrap());
    assert!(
        vs.iter()
            .all(|v| f.s.variant(*v).unwrap().color_tag == ColorTag::Red)
    );
    f.s.undo(vs[1]).unwrap();
    assert!(f.s.undo(vs[0]).unwrap());
    assert_eq!(
        f.s.variant(vs[0]).unwrap().keywords,
        [] as [std::string::String; 0]
    );
}

#[test]
fn settings_changed_by_another_handle_are_seen_after_sync() {
    let mut f = fx();
    let mut other = Session::open(&f.root).unwrap();
    let p = Preset::new(
        "base",
        ev(0.2),
        [AdjustGroup::Exposure].into_iter().collect(),
    );
    other.set_auto_apply_preset(Some(p.clone())).unwrap();
    assert!(f.s.auto_apply_preset().is_none());
    f.s.sync_index().unwrap();
    assert_eq!(f.s.auto_apply_preset(), Some(&p));
    // An explicit change from the stale-free view is written.
    f.s.set_auto_apply_preset(None).unwrap();
    other.sync_index().unwrap();
    assert!(other.auto_apply_preset().is_none());
}

#[test]
fn open_refuses_an_index_from_a_newer_build() {
    let f = fx();
    f.s.conn.execute_batch("PRAGMA user_version = 99").unwrap();
    assert!(matches!(
        Session::open(&f.root),
        Err(SessionError::IndexTooNew { found: 99, .. })
    ));
}

#[test]
fn rescan_keeps_unreadable_files_and_never_swaps_duplicates() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fx();
    let (a, va) = f.add("Capture/a.ARW", 0);
    f.s.set_rating(&[va], r(5)).unwrap();
    // Two byte-identical originals with different ratings.
    let h = put(&f.root, "Capture/P1.ARW", b"same bytes");
    put(&f.root, "Capture/P2.ARW", b"same bytes");
    let p1 = f.s.add_original("Capture/P1.ARW", h, meta(1)).unwrap();
    let p2 = f.s.add_original("Capture/P2.ARW", h, meta(2)).unwrap();
    let (v1, v2) = (f.s.variants(p1).unwrap()[0], f.s.variants(p2).unwrap()[0]);
    f.s.set_rating(&[v1], r(2)).unwrap();
    f.s.set_rating(&[v2], r(4)).unwrap();
    // a: stat changed and unreadable.
    let pa = f.root.join("Capture/a.ARW");
    fs::write(&pa, b"raw-bytes-Capture/a.ARW").unwrap();
    fs::set_permissions(&pa, fs::Permissions::from_mode(0o000)).unwrap();
    // P1/P2 copied to uninformative names (new inodes).
    for (from, to) in [("P1", "x"), ("P2", "y")] {
        fs::copy(
            f.root.join(format!("Capture/{from}.ARW")),
            f.root.join(format!("Capture/{to}.ARW")),
        )
        .unwrap();
        fs::remove_file(f.root.join(format!("Capture/{from}.ARW"))).unwrap();
    }
    let rep = f.s.rescan().unwrap();
    fs::set_permissions(&pa, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(rep.unreadable.len(), 1);
    assert_eq!(rep.unreadable[0].0, a);
    assert!(rep.replaced.is_empty(), "{rep:?}");
    assert_eq!(f.s.variant(va).unwrap().rating, r(5));
    assert!(
        rep.relinked.is_empty(),
        "ambiguous duplicates are not guessed"
    );
    assert!(f.s.image(p1).unwrap().missing && f.s.image(p2).unwrap().missing);
    // Informative names: paired, never swapped.
    fs::rename(
        f.root.join("Capture/x.ARW"),
        f.root.join("Capture/P1 copy.ARW"),
    )
    .unwrap();
    fs::rename(
        f.root.join("Capture/y.ARW"),
        f.root.join("Capture/P2 copy.ARW"),
    )
    .unwrap();
    let rep = f.s.rescan().unwrap();
    assert_eq!(rep.relinked.len(), 2, "{rep:?}");
    assert_eq!(
        f.s.image(p1).unwrap().rel_path.as_str(),
        "Capture/P1 copy.ARW"
    );
    assert_eq!(f.s.variant(v1).unwrap().rating, r(2));
    assert_eq!(f.s.variant(v2).unwrap().rating, r(4));
}

// ------------------------------------------- finishing a cull ---

/// A photo with a picked variant and a rejected one stays where it
/// is (only the rejected variant goes); a photo whose every variant is rejected
/// moves to Trash. Undo puts both back exactly.
#[test]
fn trash_rejected_moves_only_all_rejected_photos_and_hides_rejected_variants() {
    let mut f = fx();
    let (mixed, keep_v) = f.add("Capture/a.ARW", 0);
    let reject_v = f.s.new_variant(mixed).unwrap();
    f.s.set_pick(&[keep_v], PickState::Picked).unwrap();
    f.s.set_pick(&[reject_v], PickState::Rejected).unwrap();
    f.s.save_adjustments(reject_v, &ev(0.7)).unwrap();
    let (gone, gone_v) = f.add("Capture/b.ARW", 1);
    let gone_v2 = f.s.new_variant(gone).unwrap();
    f.s.set_pick(&[gone_v, gone_v2], PickState::Rejected)
        .unwrap();
    let (_, plain) = f.add("Capture/c.ARW", 2);
    assert!(!f.s.is_rejected(&f.s.image(mixed).unwrap()).unwrap());
    assert!(f.s.is_rejected(&f.s.image(gone).unwrap()).unwrap());

    let done = f.s.trash_rejected().unwrap();
    assert!(done.error.is_none());
    assert_eq!(done.moved.len(), 1);
    assert_eq!(done.moved[0].image, gone);
    assert_eq!(done.moved[0].to.as_str(), "Trash/b.ARW");
    assert_eq!(done.hidden, vec![reject_v]);
    // The file with a kept variant did not move; its picked variant is intact.
    assert!(f.root.join("Capture/a.ARW").is_file());
    assert_eq!(f.s.image(mixed).unwrap().rel_path.as_str(), "Capture/a.ARW");
    assert_eq!(f.s.variants(mixed).unwrap(), vec![keep_v]);
    assert_eq!(f.s.variant(keep_v).unwrap().pick, PickState::Picked);
    assert!(!f.s.is_rejected(&f.s.image(mixed).unwrap()).unwrap());
    assert!(
        f.s.variant(reject_v).is_err(),
        "the rejected variant is hidden"
    );
    assert!(
        f.root
            .join(format!("Trash/.ds/deleted/{}.json", reject_v.0))
            .is_file()
    );
    assert!(f.root.join("Trash/b.ARW").is_file() && f.s.variant(plain).is_ok());
    // Nothing left to do a second time.
    let again = f.s.trash_rejected().unwrap();
    assert!(again.moved.is_empty() && again.hidden.is_empty());

    // Undo: the original returns to its path, the variant with its edits.
    let back: Vec<_> = done
        .moved
        .iter()
        .map(|m| (f.s.variants(m.image).unwrap()[0], m.from.clone()))
        .collect();
    assert_eq!(f.s.move_back(&back).unwrap().len(), 1);
    assert_eq!(f.s.restore_variants(&done.hidden).unwrap(), 1);
    assert_eq!(f.s.image(gone).unwrap().rel_path.as_str(), "Capture/b.ARW");
    assert_eq!(f.s.variants(mixed).unwrap(), vec![keep_v, reject_v]);
    assert_eq!(f.s.variant(reject_v).unwrap().pick, PickState::Rejected);
    assert_eq!(f.s.load_adjustments(reject_v).unwrap(), ev(0.7));
    // Already back: nothing more to do.
    assert_eq!(f.s.move_back(&back).unwrap(), [] as [files::Moved; 0]);
    assert_eq!(f.s.restore_variants(&done.hidden).unwrap(), 0);
    assert_eq!(
        f.s.sync_index().unwrap().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
}

/// A rejected variant hidden by "Move Rejected to Trash" is found
/// on disk after a relaunch (no undo record left) and comes back.
#[test]
fn set_aside_variants_survive_a_relaunch_and_restore() {
    let mut f = fx();
    assert_eq!(
        f.s.set_aside_variants().unwrap(),
        [] as [session::VariantId; 0]
    );
    let (mixed, keep_v) = f.add("Capture/a.ARW", 0);
    let reject_v = f.s.new_variant(mixed).unwrap();
    f.s.set_pick(&[reject_v], PickState::Rejected).unwrap();
    f.s.save_adjustments(reject_v, &ev(0.4)).unwrap();
    let done = f.s.trash_rejected().unwrap();
    assert_eq!(done.hidden, vec![reject_v]);
    // Junk next to the set-aside sidecars is not a variant.
    put(&f.root, "Trash/.ds/deleted/.ds-1-2.tmp", b"x");
    put(&f.root, "Trash/.ds/deleted/notes.json", b"{}");
    f.reopen();
    assert!(f.s.variant(reject_v).is_err());
    let aside = f.s.set_aside_variants().unwrap();
    assert_eq!(aside, vec![reject_v]);
    assert_eq!(f.s.restore_variants(&aside).unwrap(), 1);
    assert_eq!(f.s.variants(mixed).unwrap(), vec![keep_v, reject_v]);
    assert_eq!(f.s.load_adjustments(reject_v).unwrap(), ev(0.4));
    assert_eq!(
        f.s.set_aside_variants().unwrap(),
        [] as [session::VariantId; 0]
    );
}

/// Every file-organising command works on an exFAT session disk
/// (no `RENAME_EXCL` there), never overwrites, and loses nothing.
#[cfg(target_os = "macos")]
#[test]
fn file_commands_work_on_exfat() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(mut vol) =
        ds_fsutil::testvol::require(tmp.path(), ds_fsutil::testvol::Fs::ExFat, 64, "DSEXFAT")
    else {
        return;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = vol.mount_point().join("Card Day");
        let mut s = Session::create(&root, "Card Day").unwrap();
        let add = |s: &mut Session, rel: &str, i: i64| {
            let h = put(&root, rel, format!("raw-{rel}").as_bytes());
            let id = s.add_original(rel, h, meta(i)).unwrap();
            (id, s.variants(id).unwrap()[0])
        };
        let (a, av) = add(&mut s, "Capture/a.ARW", 0);
        let (b, bv) = add(&mut s, "Capture/b.ARW", 1);
        let (m, mv) = add(&mut s, "Capture/m.ARW", 2);
        let mv2 = s.new_variant(m).unwrap();
        s.set_pick(&[av, mv], PickState::Picked).unwrap();
        s.set_pick(&[bv, mv2], PickState::Rejected).unwrap();

        // Move Rejected to Trash: b moves, m's rejected variant is set aside.
        let done = s.trash_rejected().unwrap();
        assert!(done.error.is_none());
        assert_eq!(done.moved.len(), 1);
        assert_eq!(done.hidden, vec![mv2]);
        assert!(root.join("Trash/b.ARW").is_file() && !root.join("Capture/b.ARW").exists());
        // Selects, and back.
        let sel = s.move_to(&[av], Folder::Selects).unwrap();
        assert_eq!(s.image(a).unwrap().rel_path.as_str(), "Selects/a.ARW");
        assert!(root.join("Selects/.ds/settings/a.ARW.json").is_file());
        s.move_back(&[(av, sel[0].from.clone())]).unwrap();
        assert!(root.join("Capture/a.ARW").is_file());
        // A clash gets a free name; nothing is overwritten.
        put(&root, "Selects/a.ARW", b"somebody else's");
        let sel = s.move_to(&[av], Folder::Selects).unwrap();
        assert_eq!(sel[0].to.as_str(), "Selects/a-2.ARW");
        assert_eq!(
            fs::read(root.join("Selects/a.ARW")).unwrap(),
            b"somebody else's"
        );
        // Restore from Session Trash (after a relaunch).
        drop(s);
        let mut s = Session::open(&root).unwrap();
        s.move_to(&[bv], Folder::Capture).unwrap();
        assert_eq!(
            fs::read(root.join("Capture/b.ARW")).unwrap(),
            b"raw-Capture/b.ARW"
        );
        let aside = s.set_aside_variants().unwrap();
        assert_eq!(s.restore_variants(&aside).unwrap(), 1);
        assert_eq!(s.variants(m).unwrap(), vec![mv, mv2]);
        // Delete Rejected: rejects to the session Trash, then to the (fake) bin.
        s.set_pick(&[mv2], PickState::Picked).unwrap();
        let done = s.trash_rejected().unwrap();
        assert_eq!(done.moved.len(), 1);
        let bin = FakeTrash::new(vol.mount_point().join("bin"));
        assert_eq!(s.empty_trash(&bin).unwrap(), 1);
        assert!(s.image(b).is_err());
        let binned: Vec<_> = fs::read_dir(vol.mount_point().join("bin"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(binned.iter().any(|n| n.ends_with("b.ARW")), "{binned:?}");
        // Everything else is where the index says, with its bytes.
        for img in s.images().unwrap() {
            assert!(
                img.rel_path.to_path(&root).is_file(),
                "{}",
                img.rel_path.as_str()
            );
        }
        assert_eq!(
            s.sync_index().unwrap().errors,
            [] as [(std::string::String, std::string::String); 0]
        );
    }));
    vol.detach(false).unwrap();
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

/// Undo puts a trashed Select back into Selects, under its old name
/// when the move had to rename it and that name is free again.
#[test]
fn move_back_returns_to_the_original_folder_and_name() {
    let mut f = fx();
    let (img, v) = f.add("Capture/Day 1/a.ARW", 0);
    f.s.move_to(&[v], Folder::Selects).unwrap();
    put(&f.root, "Trash/Day 1/a.ARW", b"something else");
    let moved = f.s.move_to(&[v], Folder::Trash).unwrap();
    assert_eq!(moved[0].to.as_str(), "Trash/Day 1/a-2.ARW");
    assert_eq!(moved[0].from.as_str(), "Selects/Day 1/a.ARW");
    assert_eq!(
        f.s.move_back(&[(v, moved[0].from.clone())]).unwrap().len(),
        1
    );
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Selects/Day 1/a.ARW"
    );
    assert!(
        f.root
            .join("Selects/Day 1/.ds/settings/a.ARW.json")
            .is_file()
    );
    // Its old name was taken meanwhile: a free name next to it.
    f.s.move_to(&[v], Folder::Trash).unwrap();
    put(&f.root, "Selects/Day 1/a.ARW", b"a newcomer");
    let to = RelPath::new("Selects/Day 1/a.ARW").unwrap();
    let back = f.s.move_back(&[(v, to)]).unwrap();
    assert_eq!(back[0].to.as_str(), "Selects/Day 1/a-2.ARW");
    assert_eq!(
        fs::read(f.root.join("Selects/Day 1/a.ARW")).unwrap(),
        b"a newcomer"
    );
    assert!(
        f.s.move_back(&[(VariantId(424_242), RelPath::new("Capture/x.ARW").unwrap())])
            .is_err()
    );
}

/// A deleted variant whose sidecar location a newer variant took, or whose
/// original left the session, stays set aside (never overwrites anything).
#[test]
fn restore_variants_never_overwrites() {
    let mut f = fx();
    let (img, v1) = f.add("Capture/a.ARW", 0);
    let v2 = f.s.new_variant(img).unwrap();
    f.s.delete_variant(v2).unwrap();
    let v3 = f.s.new_variant(img).unwrap();
    assert_eq!(f.s.variant(v3).unwrap().ordinal, 2, "v2's location");
    assert_eq!(f.s.restore_variants(&[v2, VariantId(7)]).unwrap(), 0);
    assert!(
        f.root
            .join(format!("Trash/.ds/deleted/{}.json", v2.0))
            .is_file()
    );
    assert!(f.s.variant(v1).is_ok() && f.s.variant(v3).is_ok());
    // The original is gone from the session: nothing comes back.
    let (b, bv) = f.add("Capture/b.ARW", 1);
    let bv2 = f.s.new_variant(b).unwrap();
    f.s.delete_variant(bv2).unwrap();
    f.s.move_to(&[bv], Folder::Trash).unwrap();
    f.s.empty_trash(&FakeTrash::new(f.root.parent().unwrap().join("bin")))
        .unwrap();
    assert_eq!(f.s.restore_variants(&[bv2]).unwrap(), 0);
}

/// A trasher that refuses part-way (a volume without a Trash)
/// deletes nothing; what it already took leaves the index, the rest stays.
#[test]
fn empty_trash_refused_part_way_keeps_index_and_disk_in_step() {
    struct RefuseSecond {
        bin: FakeTrash,
        calls: AtomicUsize,
    }
    impl Trasher for RefuseSecond {
        fn trash(&self, paths: &[PathBuf]) -> Result<(), String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err("volume has no Trash".into());
            }
            self.bin.trash(paths)
        }
    }
    let mut f = fx();
    let (a, va) = f.add("Capture/a.ARW", 0);
    let (b, vb) = f.add("Capture/b.ARW", 1);
    f.s.move_to(&[va, vb], Folder::Trash).unwrap();
    let t = RefuseSecond {
        bin: FakeTrash::new(f.root.parent().unwrap().join("bin")),
        calls: AtomicUsize::new(0),
    };
    let e = f.s.empty_trash(&t).unwrap_err();
    assert!(matches!(e, SessionError::Trash(_)));
    assert!(e.to_string().contains("nothing was deleted"), "{e}");
    let (first, second) = if f.s.image(a).is_err() {
        (a, b)
    } else {
        (b, a)
    };
    assert!(f.s.image(first).is_err(), "handed over: out of the index");
    let left = f.s.image(second).unwrap();
    assert!(
        f.root.join(left.rel_path.as_str()).is_file(),
        "refused: still there"
    );
    assert_eq!(
        f.s.sync_index().unwrap().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
    assert_eq!(f.s.images().unwrap().len(), 1);
}

/// A small disk image, attached without Finder (no browse, no auto-open) at a
/// temp mount point, and always detached. `None` (with a printed SKIP) when
/// hdiutil is unavailable or hangs.
struct DiskImage {
    _dir: tempfile::TempDir,
    mount: PathBuf,
}

impl DiskImage {
    fn hdiutil(args: &[&std::ffi::OsStr]) -> bool {
        let Ok(mut child) = std::process::Command::new("/usr/bin/hdiutil")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Ok(Some(st)) = child.try_wait() {
                return st.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn new(megabytes: u32) -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("vol.dmg");
        let mount = dir.path().join("mnt");
        fs::create_dir_all(&mount).unwrap();
        let size = format!("{megabytes}m");
        let ok = Self::hdiutil(&[
            "create".as_ref(),
            "-quiet".as_ref(),
            "-size".as_ref(),
            size.as_ref(),
            "-fs".as_ref(),
            "APFS".as_ref(),
            "-volname".as_ref(),
            "DSCullTest".as_ref(),
            img.as_os_str(),
        ]) && Self::hdiutil(&[
            "attach".as_ref(),
            "-quiet".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
            img.as_os_str(),
        ]);
        let d = Self { _dir: dir, mount };
        if ok {
            Some(d)
        } else {
            eprintln!("SKIP: hdiutil could not make a disk image");
            None
        }
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        if !Self::hdiutil(&["detach".as_ref(), self.mount.as_os_str()]) {
            Self::hdiutil(&["detach".as_ref(), "-force".as_ref(), self.mount.as_os_str()]);
        }
    }
}

/// `move_to` on a volume that fills up part-way rolls
/// every rename back: the index and the disk agree afterwards.
#[test]
fn move_to_on_a_full_disk_rolls_back() {
    let Some(vol) = DiskImage::new(8) else { return };
    let root = vol.mount.join("Full");
    let mut s = Session::create(&root, "Full").unwrap();
    let mut vs = Vec::new();
    for i in 0..30 {
        let rel = format!("Capture/sub{}/p{i}.ARW", i % 7);
        let h = put(&root, &rel, format!("raw-bytes-{rel}").as_bytes());
        let id = s.add_original(&rel, h, meta(i)).unwrap();
        let v = s.variants(id).unwrap()[0];
        s.save_adjustments(v, &ev(0.1 * i as f32)).unwrap();
        vs.push(v);
    }
    // Fill the volume.
    let mut n = 0;
    while fs::write(vol.mount.join(format!("fill{n}")), vec![7u8; 256 * 1024]).is_ok() {
        n += 1;
    }
    for k in 0..200 {
        if fs::write(vol.mount.join(format!("tiny{k}")), b"x").is_err() {
            break;
        }
    }
    let res = s.move_to(&vs, Folder::Trash);
    let mut disagreements = Vec::new();
    for &v in &vs {
        let loc = s.locate(v).unwrap();
        let img = s.image(loc.image).unwrap();
        if !img.rel_path.to_path(&root).is_file() {
            disagreements.push(format!("original missing at {}", img.rel_path.as_str()));
        }
        if !root.join(&loc.sidecar).is_file() {
            disagreements.push(format!("sidecar missing at {}", loc.sidecar));
        }
    }
    for k in 0..n {
        let _ = fs::remove_file(vol.mount.join(format!("fill{k}")));
    }
    assert!(res.is_err(), "the volume was full: {res:?}");
    assert!(disagreements.is_empty(), "{disagreements:?}");
    assert_eq!(
        s.sync_index().unwrap().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
}

/// The real `SystemTrash` (`NSFileManager`) moves a file into the
/// volume's own Trash (inside the disk image, never the user's), and refuses a
/// missing one without touching anything.
#[test]
fn system_trash_moves_files_into_the_volume_trash() {
    use std::os::unix::fs::MetadataExt;
    let Some(vol) = DiskImage::new(8) else { return };
    let file = vol.mount.join("x.ARW");
    fs::write(&file, b"raw").unwrap();
    // `.Trashes` itself can't be listed; each user's folder in it can.
    let mine = vol
        .mount
        .join(".Trashes")
        .join(fs::metadata(&file).unwrap().uid().to_string());
    SystemTrash
        .trash(std::slice::from_ref(&file))
        .expect("an APFS volume has a Trash");
    assert!(!file.exists());
    let names: Vec<String> = fs::read_dir(&mine)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with('x')),
        "in the volume's Trash: {names:?}"
    );
    assert_eq!(
        fs::read(mine.join(names.iter().find(|n| n.starts_with('x')).unwrap())).unwrap(),
        b"raw"
    );
    // A file that isn't there: refused.
    assert!(SystemTrash.trash(std::slice::from_ref(&file)).is_err());
}

/// A variant hidden by Move Rejected to Trash (its sibling kept) comes back into the
/// manual albums it was in, at its place; albums deleted meanwhile are skipped.
#[test]
fn restored_variants_rejoin_their_albums() {
    let mut f = fx();
    let (img, keep) = f.add("Capture/a.ARW", 0);
    let reject = f.s.new_variant(img).unwrap();
    let (_, other) = f.add("Capture/b.ARW", 1);
    let a = f.s.create_album("Trip").unwrap();
    let gone = f.s.create_album("Gone").unwrap();
    f.s.add_to_album(a, &[other, reject, keep]).unwrap();
    f.s.add_to_album(gone, &[reject]).unwrap();
    f.s.set_pick(&[reject], PickState::Rejected).unwrap();
    let done = f.s.trash_rejected().unwrap();
    assert_eq!(done.hidden, [reject]);
    let members = |s: &Session, id| match &s.albums().iter().find(|x| x.id == id).unwrap().kind {
        AlbumKind::Manual { members } => members.clone(),
        AlbumKind::Smart { .. } => unreachable!(),
    };
    assert_eq!(members(&f.s, a), [other, keep]);
    f.s.delete_album(gone).unwrap();
    assert_eq!(f.s.restore_variants(&done.hidden).unwrap(), 1);
    assert_eq!(members(&f.s, a), [other, reject, keep], "back at its place");
    assert!(
        !f.root
            .join(format!("Trash/.ds/deleted/{}.albums.json", reject.0))
            .exists()
    );
    // A variant in no album records nothing.
    let lone = f.s.new_variant(img).unwrap();
    f.s.delete_variant(lone).unwrap();
    assert!(
        !f.root
            .join(format!("Trash/.ds/deleted/{}.albums.json", lone.0))
            .exists()
    );
}

#[test]
fn neutral_edits_are_listed_for_loading_and_survive_an_index_upgrade() {
    let mut f = fx();
    let (_, a) = f.add("Capture/a.ARW", 0);
    let (_, b) = f.add("Capture/b.ARW", 0);
    let (_, c) = f.add("Capture/c.ARW", 0);
    f.add("Capture/d.ARW", 0);
    let neutral = crate::neutral_default();
    assert_eq!(neutral.profile, ds_core::adjust::Profile::Neutral);
    let neutral_ev = Adjustments {
        profile: ds_core::adjust::Profile::Neutral,
        ..ev(0.5)
    };
    f.s.save_adjustments(a, &neutral).unwrap();
    f.s.save_adjustments(b, &ev(1.0)).unwrap();
    f.s.save_adjustments(c, &neutral_ev).unwrap();
    // Not edited (no badge, the Edited filter skips it), but loaded: not the default.
    // Exactly the default + Neutral is listed apart (no sidecar read needed).
    assert!(!f.s.variant(a).unwrap().edited && f.s.variant(b).unwrap().edited);
    // Ids are random: compare as sets.
    let set = |v: Vec<VariantId>| v.into_iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(
        set(f.s.variants_with_adjustments().unwrap()),
        set(vec![b, c])
    );
    assert_eq!(f.s.neutral_default_variants().unwrap(), vec![a]);
    // Back to the default: no longer listed.
    f.s.save_adjustments(b, &Adjustments::default()).unwrap();
    f.s.save_adjustments(c, &neutral).unwrap();
    assert_eq!(
        f.s.variants_with_adjustments().unwrap(),
        [] as [session::VariantId; 0]
    );
    assert_eq!(f.s.neutral_default_variants().unwrap().len(), 2);
    f.s.save_adjustments(c, &Adjustments::default()).unwrap();
    f.s.save_adjustments(b, &neutral_ev).unwrap();
    // A v3 index (before the column): upgraded in place and backfilled from the
    // sidecars on open.
    let db = f.s.db_path();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("ALTER TABLE variants DROP COLUMN custom; PRAGMA user_version=3;")
        .unwrap();
    drop(conn);
    f.reopen();
    assert_eq!(f.s.open_report().recovery, None);
    assert_eq!(f.s.variants_with_adjustments().unwrap(), vec![b]);
    assert_eq!(f.s.neutral_default_variants().unwrap(), vec![a]);
    assert!(!f.s.variant(a).unwrap().edited);
}

#[test]
fn optimize_reports_are_kept_in_the_sidecar() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let (_, w) = f.add("Capture/b.ARW", 1);
    assert_eq!(f.s.optimize_report(v).unwrap(), None);
    let rel = f.s.locate(v).unwrap().sidecar;
    let report = serde_json::json!({"version": 1, "summary": "brightens", "changes": []});
    f.s.set_optimize_report(&[v, w], Some(&report)).unwrap();
    f.reopen();
    assert_eq!(f.s.optimize_report(v).unwrap(), Some(report.clone()));
    assert_eq!(f.sidecar(w).optimize_report, Some(report.clone()));
    // Other edits keep it; it is not an undo step.
    f.s.save_adjustments(v, &ev(0.5)).unwrap();
    assert_eq!(f.s.optimize_report(v).unwrap(), Some(report.clone()));
    // A new variant copies it with the adjustments it describes.
    let image = f.s.variant(v).unwrap().image;
    let nv = f.s.new_variant(image).unwrap();
    assert_eq!(f.s.optimize_report(nv).unwrap(), Some(report));
    f.s.set_optimize_report(&[v], None).unwrap();
    f.reopen();
    assert_eq!(f.s.optimize_report(v).unwrap(), None);
    assert!(
        fs::read_to_string(f.root.join(&rel))
            .unwrap()
            .contains("\"optimize_report\": null")
    );
    assert!(f.s.optimize_report(VariantId(9999)).is_err());
}

#[test]
fn ai_changes_are_kept_in_the_sidecar() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW", 0);
    let (_, w) = f.add("Capture/b.ARW", 1);
    assert_eq!(f.s.ai_changes(v).unwrap(), None);
    let rel = f.s.locate(v).unwrap().sidecar;
    let changes =
        serde_json::json!({"version": 1, "steps": [{"source": "auto", "rows": ["exposure/ev"]}]});
    f.s.set_ai_changes(&[v, w], Some(&changes)).unwrap();
    f.reopen();
    assert_eq!(f.s.ai_changes(v).unwrap(), Some(changes.clone()));
    assert_eq!(f.sidecar(w).ai_changes, Some(changes.clone()));
    // Other edits keep them; setting the same value again writes nothing new.
    f.s.save_adjustments(v, &ev(0.5)).unwrap();
    f.s.set_ai_changes(&[v], Some(&changes)).unwrap();
    assert_eq!(f.s.ai_changes(v).unwrap(), Some(changes.clone()));
    // The optimize report is a separate field.
    assert_eq!(f.s.optimize_report(v).unwrap(), None);
    f.s.set_ai_changes(&[v], None).unwrap();
    f.reopen();
    assert_eq!(f.s.ai_changes(v).unwrap(), None);
    assert!(
        fs::read_to_string(f.root.join(&rel))
            .unwrap()
            .contains("\"ai_changes\": null")
    );
    assert!(f.s.ai_changes(VariantId(9999)).is_err());
    assert!(f.s.set_ai_changes(&[VariantId(9999)], None).is_err());
}

#[test]
fn sidecars_without_ai_changes_still_load() {
    let mut j: serde_json::Value =
        serde_json::from_slice(&crate::sidecar::tests::sample().to_json()).unwrap();
    j.as_object_mut().unwrap().remove("ai_changes");
    let loaded = Sidecar::from_json(&serde_json::to_vec(&j).unwrap()).unwrap();
    assert_eq!(loaded.sidecar.ai_changes, None);
    assert!(!loaded.too_new);
    assert_eq!(loaded.sidecar.schema_version, 1);
    // Written back by this build and read again: the same sidecar, with `ai_changes: null`.
    let written: serde_json::Value = serde_json::from_slice(&loaded.sidecar.to_json()).unwrap();
    assert_eq!(written["ai_changes"], serde_json::Value::Null);
    let again = Sidecar::from_json(&loaded.sidecar.to_json()).unwrap();
    assert_eq!(again.sidecar, loaded.sidecar);
    // What an older build (no `ai_changes` key) reads back is the rest unchanged.
    let mut older = written.clone();
    older.as_object_mut().unwrap().remove("ai_changes");
    assert_eq!(older, j);
}

#[test]
fn sidecars_without_an_optimize_report_still_load() {
    let mut j: serde_json::Value =
        serde_json::from_slice(&crate::sidecar::tests::sample().to_json()).unwrap();
    j.as_object_mut().unwrap().remove("optimize_report");
    let loaded = Sidecar::from_json(&serde_json::to_vec(&j).unwrap()).unwrap();
    assert_eq!(loaded.sidecar.optimize_report, None);
    assert!(!loaded.too_new);
}

#[test]
fn a_derived_photo_starts_with_the_source_adjustments_and_no_undo_step() {
    let mut f = fx();
    let (_, src) = f.add("Capture/a.ARW", 0);
    let mut a = ev(1.5);
    a.detail.sharpen_amount = 120.0;
    f.s.save_adjustments(src, &a).unwrap();
    // The auto-apply preset is for imports, not for a photo made from another.
    let mut p = Adjustments::default();
    p.detail.sharpen_amount = 250.0;
    let preset = Preset::new("A9 base", p, [AdjustGroup::Detail].into_iter().collect());
    f.s.set_auto_apply_preset(Some(preset)).unwrap();
    let h = put(&f.root, "Capture/a-Upscaled.dng", b"dng");
    let item = |rel: &str| NewOriginal {
        rel: rel.into(),
        hash: h,
        content_hash: None,
        meta: meta(0),
        replaced: Vec::new(),
        iptc: ds_core::IptcMeta::default(),
    };
    let v = f.s.add_derived(item("Capture/a-Upscaled.dng"), &a).unwrap();
    assert_eq!(f.s.load_adjustments(v).unwrap(), a);
    assert_eq!(f.s.undo_depth(v), 0);
    assert!(f.s.variant(v).unwrap().edited);
    f.reopen();
    assert_eq!(f.s.load_adjustments(v).unwrap(), a, "in the sidecar");
    assert_eq!(f.s.image_count().unwrap(), 2);
    // A file already in the session, or none at all, is refused.
    assert!(f.s.add_derived(item("Capture/a-Upscaled.dng"), &a).is_err());
    assert!(f.s.add_derived(item("Capture/missing.dng"), &a).is_err());
}
