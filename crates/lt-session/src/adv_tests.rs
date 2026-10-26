//! Adversarial review tests. Each asserts the behavior a user needs; a failure
//! is a finding.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lt_core::geom::Orientation;
use lt_core::{Adjustments, FileHash, Rating};

use crate::*;

struct Fx {
    dir: tempfile::TempDir,
    root: PathBuf,
    s: Session,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sess");
    let s = Session::create(&root, "sess").unwrap();
    Fx { dir, root, s }
}

fn put(root: &Path, rel: &str, bytes: &[u8]) -> FileHash {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
    FileHash(*blake3::hash(bytes).as_bytes())
}

fn meta() -> ImageMetaLite {
    ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    }
}

impl Fx {
    fn add_bytes(&mut self, rel: &str, bytes: &[u8]) -> (ImageId, VariantId) {
        let h = put(&self.root, rel, bytes);
        let id = self.s.add_original(rel, h, meta()).unwrap();
        (id, self.s.variants(id).unwrap()[0])
    }
    fn add(&mut self, rel: &str) -> (ImageId, VariantId) {
        self.add_bytes(rel, format!("raw-{rel}").as_bytes())
    }
}

fn ev(x: f32) -> Adjustments {
    let mut a = Adjustments::default();
    a.exposure.exposure_ev = x;
    a
}

/// Two handles on one session (two windows / two app instances): album edits
/// from one must not be erased by the other.
#[test]
fn adv_albums_lost_update_across_handles() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW");
    let mut b = Session::open(&f.root).unwrap();
    let x = f.s.create_album("Portfolio").unwrap();
    f.s.add_to_album(x, &[v]).unwrap();
    let y = b.create_album("Client").unwrap();
    let reopened = Session::open(&f.root).unwrap();
    let names: Vec<&str> = reopened.albums().iter().map(|a| a.name.as_str()).collect();
    eprintln!("ids {x:?} {y:?}");
    assert!(names.contains(&"Portfolio"), "album lost: {names:?}");
}

/// A non-finite float anywhere in the stack is serialized as `null`, which
/// the reader rejects: the sidecar becomes unreadable.
#[test]
fn adv_nan_adjustment_bricks_sidecar() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW");
    f.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    let res = f.s.save_adjustments(v, &ev(f32::NAN));
    if res.is_ok() {
        assert!(
            f.s.load_adjustments(v).is_ok(),
            "save accepted NaN and the sidecar is now unreadable"
        );
    }
}

/// B is renamed over A's old name (A deleted/moved away). rescan must not
/// silently keep A's edits on B's pixels.
#[test]
fn adv_rename_onto_other_files_name_keeps_wrong_edits() {
    let mut f = fx();
    let (a, va) = f.add_bytes("Capture/a.ARW", b"AAAAAAAA");
    let (b, _vb) = f.add_bytes("Capture/b.ARW", b"BBBBBBBB");
    f.s.set_rating(&[va], Rating::new(5).unwrap()).unwrap();
    fs::remove_file(f.root.join("Capture/a.ARW")).unwrap();
    fs::rename(f.root.join("Capture/b.ARW"), f.root.join("Capture/a.ARW")).unwrap();
    let rep = f.s.rescan().unwrap();
    let a_rel = RelPath::new("Capture/a.ARW").unwrap();
    assert_eq!(rep.replaced, vec![(a, a_rel.clone())]);
    // B's content is at A's path, which A's record still holds: reported, not
    // silently relinked over A.
    assert_eq!(rep.conflicts, vec![(b, a_rel)]);
    assert!(f.s.image(b).unwrap().missing);
    // Stable on a second pass (A stays flagged even though the stat now
    // matches nothing we verified).
    let rep2 = f.s.rescan().unwrap();
    assert!(rep2.replaced.len() == 1 && f.s.image(a).unwrap().missing);
    let on_disk = crate::files::hash_file(&f.root.join("Capture/a.ARW")).unwrap();
    let img = f.s.image(a).unwrap();
    assert!(
        img.hash == on_disk || img.missing,
        "image A (5 stars) now points at B's content and nobody noticed"
    );
}

/// A stray sidecar at the relink destination makes every rescan fail.
#[test]
fn adv_rescan_blocked_by_stray_sidecar_at_target() {
    let mut f = fx();
    let (_, _va) = f.add("Capture/a.ARW");
    fs::rename(f.root.join("Capture/a.ARW"), f.root.join("Capture/b.ARW")).unwrap();
    fs::write(f.root.join("Capture/.lt/settings/b.ARW.json"), b"{garbage").unwrap();
    let r1 = f.s.rescan();
    assert!(r1.is_ok(), "rescan aborted: {:?}", r1.err());
}

/// Sidecar at the current schema with a field this build does not know
/// (newer build that added an optional field without a bump) is stripped on
/// the next unrelated write.
#[test]
fn adv_unknown_fields_dropped_on_rewrite() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW");
    let rel = f.s.locate(v).unwrap().sidecar;
    let p = f.root.join(&rel);
    let mut j: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    j["adjustments"]["exposure"]["future_knob"] = serde_json::json!(0.7);
    j["future_top"] = serde_json::json!({"x": 1});
    fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
    f.s.sync_index().unwrap();
    f.s.set_rating(&[v], Rating::new(3).unwrap()).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    assert!(
        after.get("future_top").is_some(),
        "unknown top-level field silently dropped by a rating change"
    );
}

/// A Finder duplicate of a folder ("Day1 copy", including .lt) wins over the
/// real folder when the index is rebuilt, because it sorts first.
#[test]
fn adv_rebuild_prefers_finder_copy_over_original() {
    let mut f = fx();
    let (img, v) = f.add("Capture/Day1/a.ARW");
    f.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    // cp -R Capture/Day1 "Capture/Day1 copy"
    let st = std::process::Command::new("/bin/cp")
        .args(["-Rp"])
        .arg(f.root.join("Capture/Day1"))
        .arg(f.root.join("Capture/Day1 copy"))
        .status()
        .unwrap();
    assert!(st.success());
    f.s.sync_index().unwrap(); // live index: original keeps its row
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Capture/Day1/a.ARW"
    );
    drop(std::mem::replace(&mut f.s, Session::open(&f.root).unwrap()));
    // Rebuild from sidecars
    f.s.rebuild_index().unwrap();
    assert_eq!(
        f.s.image(img).unwrap().rel_path.as_str(),
        "Capture/Day1/a.ARW",
        "after rebuild the image id is bound to the copy"
    );
}

/// ⌘Z after a rating change reverts the previous *exposure* edit instead.
#[test]
fn adv_undo_after_rating_reverts_unrelated_exposure() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW");
    f.s.save_adjustments(v, &ev(1.0)).unwrap();
    f.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    f.s.undo(v).unwrap();
    assert_eq!(
        f.s.load_adjustments(v).unwrap().exposure.exposure_ev,
        1.0,
        "undo meant for the rating reverted the exposure"
    );
}

/// Exposure drag, then (within 500 ms) an unrelated WB click: one undo step
/// reverts both.
#[test]
fn adv_coalescing_merges_unrelated_edits() {
    let mut f = fx();
    let (_, v) = f.add("Capture/a.ARW");
    let t0 = Instant::now();
    let t = std::sync::Arc::new(std::sync::Mutex::new(t0));
    let t2 = t.clone();
    f.s.set_clock(move || *t2.lock().unwrap());
    f.s.save_adjustments(v, &ev(1.0)).unwrap();
    *t.lock().unwrap() += Duration::from_millis(300);
    let mut wb = ev(1.0);
    wb.white_balance = lt_core::adjust::WhiteBalance::Custom {
        temp_k: 5600.0,
        tint: 0.0,
    };
    f.s.save_adjustments(v, &wb).unwrap();
    f.s.undo(v).unwrap();
    assert_eq!(
        f.s.load_adjustments(v).unwrap().exposure.exposure_ev,
        1.0,
        "undoing the WB click also reverted the exposure drag"
    );
}

/// 500-image paste, 3 re-edited, undo group, then per-variant undo.
#[test]
fn adv_batch_500_undo_partial() {
    let mut f = fx();
    let vs: Vec<VariantId> = (0..500)
        .map(|i| f.add(&format!("Capture/{i:03}.ARW")).1)
        .collect();
    let edits: Vec<_> = vs.iter().map(|v| (*v, ev(0.5))).collect();
    let t = Instant::now();
    let g = f.s.apply_batch(&edits).unwrap().unwrap();
    eprintln!("apply_batch 500: {:?}", t.elapsed());
    for v in &vs[..3] {
        f.s.save_adjustments(*v, &ev(2.0)).unwrap();
    }
    let t = Instant::now();
    assert!(f.s.undo_group(g).unwrap());
    eprintln!("undo_group 500: {:?}", t.elapsed());
    for v in &vs[3..] {
        assert_eq!(f.s.load_adjustments(*v).unwrap().exposure.exposure_ev, 0.0);
    }
    for v in &vs[..3] {
        assert_eq!(f.s.load_adjustments(*v).unwrap().exposure.exposure_ev, 2.0);
        f.s.undo(*v).unwrap();
        assert_eq!(f.s.load_adjustments(*v).unwrap().exposure.exposure_ev, 0.5);
        f.s.undo(*v).unwrap();
        assert_eq!(f.s.load_adjustments(*v).unwrap().exposure.exposure_ev, 0.0);
    }
    assert!(f.s.redo_group(g).unwrap());
    assert_eq!(
        f.s.load_adjustments(vs[100]).unwrap().exposure.exposure_ev,
        0.5
    );
}

/// rescan hashes candidates while holding the write lock; a second handle's
/// save during that time waits and then fails with `SQLITE_BUSY` after 5 s.
#[test]
#[ignore = "writes ~1 GB"]
fn adv_rescan_hashing_blocks_other_handle() {
    let mut f = fx();
    let size = 49 << 20;
    let body = vec![7u8; size];
    let (_, _va) = f.add_bytes("Capture/a.ARW", &body);
    let (_, vb) = f.add("Capture/b.ARW");
    fs::remove_file(f.root.join("Capture/a.ARW")).unwrap();
    for i in 0..20 {
        let mut other = body.clone();
        other[0] = i + 1;
        fs::write(f.root.join(format!("Capture/untracked{i:02}.ARW")), &other).unwrap();
    }
    let root = f.root.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let mut b = Session::open(&root);
        let t = Instant::now();
        let r = b.as_mut().map(|b| b.save_adjustments(vb, &ev(1.0)));
        (t.elapsed(), r.map(|r| r.is_ok()).map_err(|e| e.to_string()))
    });
    let t = Instant::now();
    f.s.rescan().unwrap();
    eprintln!("rescan with 20 x 49 MB candidates: {:?}", t.elapsed());
    let (waited, ok) = h.join().unwrap();
    eprintln!("other handle save waited {waited:?}: {ok:?}");
    assert!(
        waited < Duration::from_millis(100),
        "UI save blocked by rescan"
    );
}

/// Deleting a variant removes its sidecar permanently (not via Trash); the
/// work cannot be recovered or undone.
#[test]
fn adv_delete_variant_is_unrecoverable() {
    let mut f = fx();
    let (img, _v1) = f.add("Capture/a.ARW");
    let v2 = f.s.new_variant(img).unwrap();
    f.s.save_adjustments(v2, &ev(3.0)).unwrap();
    let rel = f.s.locate(v2).unwrap().sidecar;
    let content = fs::read(f.root.join(&rel)).unwrap();
    f.s.delete_variant(v2).unwrap();
    // Per the coordinator's decision the sidecar moves to the session Trash
    // (recoverable, sent to the macOS Trash only by empty_trash).
    assert!(!f.root.join(&rel).exists());
    let kept = f.root.join(format!("Trash/.lt/deleted/{}.json", v2.0));
    assert_eq!(
        fs::read(&kept).unwrap(),
        content,
        "variant sidecar hard-deleted"
    );
    // It is not re-indexed as a live variant.
    f.s.rebuild_index().unwrap();
    assert!(f.s.variant(v2).is_err());
}

/// A very long (but legal) original name cannot get a sidecar.
#[test]
fn adv_long_file_name_cannot_be_registered() {
    let mut f = fx();
    let name = format!("Capture/{}.ARW", "x".repeat(245));
    let h = put(&f.root, &name, b"x");
    let r = f.s.add_original(&name, h, meta());
    assert!(r.is_ok(), "{:?}", r.err());
    let img = r.unwrap();
    let v = f.s.variants(img).unwrap()[0];
    f.s.set_rating(&[v], Rating::new(4).unwrap()).unwrap();
    let v2 = f.s.new_variant(img).unwrap();
    // Survives a rebuild from sidecars (the shortened name is resolved).
    fs::remove_file(f.s.db_path()).unwrap();
    f.s = Session::open(&f.root).unwrap();
    assert!(
        f.s.open_report().errors.is_empty(),
        "{:?}",
        f.s.open_report().errors
    );
    assert_eq!(f.s.image(img).unwrap().rel_path.as_str(), name);
    assert_eq!(f.s.variants(img).unwrap(), vec![v, v2]);
    assert_eq!(f.s.variant(v).unwrap().rating, Rating::new(4).unwrap());
    // And moves with its original.
    f.s.move_to(&[v], Folder::Selects).unwrap();
    f.s.rebuild_index().unwrap();
    assert_eq!(f.s.image(img).unwrap().folder(), Folder::Selects);
}

/// `get()` on the cache is measured in lt-cache; here: two handles, B holds a
/// stale view of settings and overwrites A's auto-apply preset.
#[test]
fn adv_settings_lost_update_across_handles() {
    let mut f = fx();
    let mut b = Session::open(&f.root).unwrap();
    let p = Preset::new(
        "usual",
        ev(0.3),
        [lt_core::AdjustGroup::Exposure].into_iter().collect(),
    );
    f.s.set_auto_apply_preset(Some(p)).unwrap();
    // B changes nothing about auto-apply but any settings write clobbers it.
    b.set_auto_apply_preset(b.auto_apply_preset().cloned())
        .unwrap();
    let re = Session::open(&f.root).unwrap();
    assert!(re.auto_apply_preset().is_some(), "auto-apply preset lost");
}

#[test]
fn adv_nan_then_rebuild() {
    let mut f = fx();
    let (img, v) = f.add("Capture/a.ARW");
    f.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    f.s.save_adjustments(v, &ev(f32::NAN)).unwrap();
    let rel = f.s.locate(v).unwrap().sidecar;
    let text = String::from_utf8(fs::read(f.root.join(&rel)).unwrap()).unwrap();
    assert!(
        text.contains("\"exposure_ev\": 0.0"),
        "sanitized before writing"
    );
    assert_eq!(f.s.load_adjustments(v).unwrap().exposure.exposure_ev, 0.0);
    // A hand-written null (older build) also loads.
    fs::write(
        f.root.join(&rel),
        text.replace("\"exposure_ev\": 0.0", "\"exposure_ev\": null"),
    )
    .unwrap();
    assert!(f.s.load_adjustments(v).is_ok());
    f.s.set_rating(&[v], Rating::new(1).unwrap()).unwrap();
    let rep = f.s.rebuild_index().unwrap();
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    assert!(f.s.image(img).is_ok());
    assert_eq!(f.s.variant(v).unwrap().rating, Rating::new(1).unwrap());
}

/// A session folder received from elsewhere with `Capture/.lt/settings` as a
/// symlink: sidecar writes (atomic replace) land outside the session.
#[test]
fn adv_symlinked_settings_dir_escapes_session() {
    let mut f = fx();
    let outside = f.dir.path().join("victim");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("a.ARW.json"), b"precious").unwrap();
    fs::create_dir_all(f.root.join("Capture/.lt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("Capture/.lt/settings")).unwrap();
    // The existing file blocks add_original; delete it to see where a new one goes.
    fs::remove_file(outside.join("a.ARW.json")).unwrap();
    let h = put(&f.root, "Capture/a.ARW", b"x");
    let r = f.s.add_original("Capture/a.ARW", h, meta());
    assert!(
        !outside.join("a.ARW.json").exists(),
        "sidecar written outside the session through a symlink ({r:?})"
    );
    assert!(matches!(r, Err(SessionError::InvalidPath { .. })));
    // A sidecar planted outside is not picked up through the link either.
    fs::write(outside.join("b.ARW.json"), b"{}").unwrap();
    assert_eq!(
        f.s.sync_index().unwrap().errors,
        [] as [(std::string::String, std::string::String); 0]
    );
}

/// `new_variant` from a sidecar written by a newer build (unknown enum in
/// adjustments): the copy silently gets default adjustments, stamped with
/// the current schema.
#[test]
fn adv_new_variant_from_too_new_sidecar() {
    let mut f = fx();
    let (img, v) = f.add("Capture/a.ARW");
    f.s.save_adjustments(v, &ev(2.0)).unwrap();
    let rel = f.s.locate(v).unwrap().sidecar;
    let p = f.root.join(&rel);
    let mut j: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    j["schema_version"] = serde_json::json!(lt_core::SIDECAR_SCHEMA_VERSION + 1);
    j["adjustments"]["white_balance"] = serde_json::json!({"mode": "spectral"});
    fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
    f.s.sync_index().unwrap();
    let r = f.s.new_variant(img);
    assert!(
        matches!(r, Err(SessionError::SidecarTooNew { .. })),
        "new variant of a newer-build image would silently lose its adjustments: {r:?}"
    );
    assert_eq!(f.s.variants(img).unwrap().len(), 1);
}
