//! Round-5 identity tests through the FFI layer: store keys per folder
//! (R4-H1), content identity across metadata rewrites (R4-H3), background
//! work that can be cancelled and resumed (R4-M1), unreadable files (L11),
//! migration leftovers (R4-M5), and the contention measurement (R4-M3).
#![allow(clippy::too_many_lines)]

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use futures_executor::block_on;

use super::*;

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    let bytes = fs::read(p).ok()?;
    Some((
        ImageMetaLite {
            capture_time_ms: Some(bytes.len() as i64),
            camera: Some("SONY ILCE-9".into()),
            width: 6000,
            height: 4000,
            ..ImageMetaLite::default()
        },
        RawMetadata::default(),
    ))
}

const LOCAL: Probe = Probe {
    is_supported: arw,
    read,
    volume_ok: |_| Ok(()),
    ranges: |_| None,
    untrusted_stat: |_| false,
    is_raw_name: arw,
};

const CARD: Probe = Probe {
    volume_ok: |_| Err("exfat volumes (memory cards) are never written to".into()),
    ..LOCAL
};

fn folder_with(dir: &Path, files: &[(&str, &[u8])]) {
    for (n, b) in files {
        let p = dir.join(n);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }
}

fn open_as(
    folder: &Path,
    fb: &Path,
    identity: &str,
    moved: bool,
    probe: &Probe,
) -> Arc<SessionHandle> {
    let opts = OpenOptions {
        identity: Some(identity.into()),
        allow_move: moved,
    };
    Arc::new(
        open_in_place_with(
            folder,
            Some(fb),
            &opts,
            None,
            probe,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap(),
    )
}

fn rate(h: &Arc<SessionHandle>, name: &str, stars: u8) {
    let id = h
        .list(&LOCAL, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == name)
        .unwrap()
        .variant_id;
    block_on(h.clone().set_rating(vec![id], stars, CancelToken::new())).unwrap();
}

fn rating(h: &SessionHandle, name: &str, probe: &Probe) -> Option<(u8, bool, bool)> {
    h.list(probe, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == name)
        .map(|r| (r.rating, r.file_changed, r.missing))
}

/// Two network shares (no volume UUID) with the same sub-path used to map to
/// one store; the second took it over and detached the first's ratings.
/// Now each folder's identity is recorded, a store made for another folder is
/// never re-pointed, and both keep their ratings.
#[test]
fn two_mounts_with_the_same_sub_path_never_share_a_store() {
    let t = tempfile::tempdir().unwrap();
    let share_a = t.path().join("pics/2026-08-06 Beach Trip");
    let share_b = t.path().join("backup/2026-08-06 Beach Trip");
    // Same names, one file different (a backup that diverged).
    folder_with(
        &share_a,
        &[("A.ARW", b"alpha"), ("B.ARW", b"bravo-original")],
    );
    folder_with(&share_b, &[("A.ARW", b"alpha"), ("B.ARW", b"bravo-other")]);
    // Worst case: both keys collide on the same fallback path (the old Swift key).
    let fb = t.path().join("InPlace/no-volume-id/samehash");
    let id_a = "net:smbfs://nas/pics|/2026-08-06 Beach Trip";
    let id_b = "net:smbfs://nas/backup|/2026-08-06 Beach Trip";
    let a = open_as(&share_a, &fb, id_a, false, &CARD);
    a.hash_pending_with(&CARD, &|| false, &mut |_, _| {})
        .unwrap();
    rate(&a, "A.ARW", 5);
    rate(&a, "B.ARW", 4);
    drop(a);
    let b = open_as(&share_b, &fb, id_b, false, &CARD);
    assert_ne!(b.report.store, fb.to_string_lossy(), "a separate store");
    assert_eq!(
        b.report.to_verify + b.report.missing,
        0,
        "nothing re-pointed"
    );
    assert_eq!(rating(&b, "A.ARW", &CARD), Some((0, false, false)));
    rate(&b, "A.ARW", 1);
    drop(b);
    // Back on share A: its store, ratings and edits are intact.
    let a = open_as(&share_a, &fb, id_a, false, &CARD);
    assert_eq!(a.report.store, fb.to_string_lossy());
    assert_eq!((a.report.to_verify, a.report.missing), (0, 0));
    assert_eq!(rating(&a, "A.ARW", &CARD), Some((5, false, false)));
    assert_eq!(rating(&a, "B.ARW", &CARD), Some((4, false, false)));
    drop(a);
    // Share B finds its own store again.
    let b = open_as(&share_b, &fb, id_b, false, &CARD);
    assert_eq!(rating(&b, "A.ARW", &CARD), Some((1, false, false)));
    drop(b);
    // A folder proven moved (bookmark) may take its store with it.
    let moved = t.path().join("pics/Beach Trip (renamed)");
    fs::rename(&share_a, &moved).unwrap();
    let m = open_as(
        &moved,
        &fb,
        "net:smbfs://nas/pics|/Beach Trip (renamed)",
        true,
        &CARD,
    );
    assert_eq!(m.report.store, fb.to_string_lossy());
    assert_eq!(rating(&m, "A.ARW", &CARD), Some((5, false, false)));
    // A store from before identities were recorded is kept for its own folder only.
    let old = t.path().join("old");
    folder_with(&old, &[("A.ARW", b"old")]);
    let old_fb = t.path().join("InPlace/legacy");
    drop(
        open_in_place(
            &old,
            Some(&old_fb),
            None,
            &CARD,
            &|| false,
            &mut |_, _, _| {},
        )
        .unwrap(),
    );
    let same = open_as(&old, &old_fb, "dev:exfat:/dev/disk9s1|/old", false, &CARD);
    assert_eq!(same.report.store, old_fb.to_string_lossy());
    drop(same);
    let s = Session::open(&old_fb).unwrap();
    assert_eq!(s.folder_identity(), Some("dev:exfat:/dev/disk9s1|/old"));
}

#[test]
fn volume_identity_names_the_mount() {
    let t = tempfile::tempdir().unwrap();
    let id = volume_identity(t.path().to_string_lossy().into_owned()).unwrap();
    assert!(
        id.starts_with("dev:apfs:") || id.starts_with("dev:hfs:"),
        "{id}"
    );
    assert!(volume_identity("/nonexistent/folder".into()).is_err());
}

/// L11: a RAW that can't be opened is reported, not silently left out.
#[test]
fn unreadable_raws_are_reported_on_open() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    folder_with(
        t.path(),
        &[("A.ARW", b"alpha"), ("locked.ARW", b"x"), ("n.txt", b"t")],
    );
    let p = t.path().join("locked.ARW");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    let probe = Probe {
        is_supported: |p| arw(p) && fs::File::open(p).is_ok(),
        ..LOCAL
    };
    let h = open_in_place(t.path(), None, None, &probe, &|| false, &mut |_, _, _| {}).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(h.report.added, 1);
    assert_eq!(h.report.failed.len(), 1, "{:?}", h.report.failed);
    assert!(h.report.failed[0].starts_with("Capture/locked.ARW: unreadable"));
}

/// Cancelling the background pass keeps what is left for the next call.
#[test]
fn cancelled_background_work_resumes() {
    let t = tempfile::tempdir().unwrap();
    folder_with(t.path(), &[("A.ARW", b"alpha"), ("B.ARW", b"bravo")]);
    let h = open_in_place(t.path(), None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap();
    h.hash_pending_with(&LOCAL, &|| false, &mut |_, _| {})
        .unwrap();
    // Touch both: re-checked in the background.
    for n in ["A.ARW", "B.ARW"] {
        let p = t.path().join(n);
        let later =
            fs::metadata(&p).unwrap().modified().unwrap() + std::time::Duration::from_secs(9);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
    let h = open_in_place(t.path(), None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap();
    assert_eq!(h.report.to_verify, 2);
    assert_eq!(
        h.hash_pending_with(&LOCAL, &|| true, &mut |_, _| {}).err(),
        Some(DsError::Cancelled)
    );
    let mut seen = Vec::new();
    let r = h
        .hash_pending_with(&LOCAL, &|| false, &mut |d, t| seen.push((d, t)))
        .unwrap();
    assert_eq!((r.verified, r.changed), (2, 0), "{r:?}");
    assert_eq!(seen.first(), Some(&(0, 2)), "determinate progress");
    assert_eq!(seen.last(), Some(&(2, 2)));
    assert!(!r.listing_changed);
    let again = open_in_place(t.path(), None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap();
    assert_eq!(again.report.to_verify, 0, "touch recorded");
}

/// Held files: copies (new inodes) of missing photos are relinked by content,
/// others registered, both reported so the app lists again.
#[test]
fn held_files_are_relinked_or_registered_in_the_background() {
    let t = tempfile::tempdir().unwrap();
    folder_with(t.path(), &[("A.ARW", b"alpha"), ("B.ARW", b"bravo")]);
    let h = Arc::new(
        open_in_place(t.path(), None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap(),
    );
    h.hash_pending_with(&LOCAL, &|| false, &mut |_, _| {})
        .unwrap();
    rate(&h, "A.ARW", 5);
    drop(h);
    fs::copy(t.path().join("A.ARW"), t.path().join("A copy.ARW")).unwrap();
    fs::remove_file(t.path().join("A.ARW")).unwrap();
    fs::write(t.path().join("Z.ARW"), b"zulu!").unwrap(); // same size as a missing photo
    let h = open_in_place(t.path(), None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap();
    assert_eq!((h.report.held, h.report.added, h.report.missing), (2, 0, 1));
    assert!(rating(&h, "Z.ARW", &LOCAL).is_none(), "held until hashed");
    let r = h
        .hash_pending_with(&LOCAL, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!((r.relinked, r.registered), (1, 1), "{r:?}");
    assert!(r.listing_changed);
    assert_eq!(rating(&h, "A copy.ARW", &LOCAL), Some((5, false, false)));
    assert_eq!(rating(&h, "Z.ARW", &LOCAL), Some((0, false, false)));
}

/// "Keep as new": the file-changed flag goes; M5: a migration's only copy
/// goes back into the folder.
#[test]
fn dismiss_file_changed_and_restore_to_folder() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().canonicalize().unwrap();
    folder_with(&dir, &[("A.ARW", b"alpha"), ("B.ARW", b"bravo")]);
    // A round-3 session copied with `cp -R`: plain copies; B's original was deleted.
    let root = dir.join(IN_PLACE_DIR);
    let mut s = Session::create(&root, "old").unwrap();
    for (n, b) in [("A.ARW", &b"alpha"[..]), ("B.ARW", b"bravo")] {
        fs::write(root.join("Capture").join(n), b).unwrap();
        let h = identity::hash_original(&root.join("Capture").join(n), None, &|| false).unwrap();
        s.add_original(&format!("Capture/{n}"), h.file, ImageMetaLite::default())
            .unwrap();
    }
    drop(s);
    fs::remove_file(dir.join("B.ARW")).unwrap();
    let h =
        Arc::new(open_in_place(&dir, None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap());
    assert_eq!(
        h.report.duplicates_removed, 1,
        "A's copy was a verified duplicate"
    );
    assert_eq!(h.report.kept_in_store, vec!["B.ARW"]);
    let b = h
        .list(&LOCAL, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == "B.ARW")
        .unwrap();
    assert!(b.missing && b.kept_in_store);
    block_on(
        h.clone()
            .restore_to_folder(b.variant_id, CancelToken::new()),
    )
    .unwrap();
    assert_eq!(fs::read(dir.join("B.ARW")).unwrap(), b"bravo");
    let b2 = h
        .list(&LOCAL, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == "B.ARW")
        .unwrap();
    assert!(!b2.missing && !b2.kept_in_store);
    // Replace A with a different photo, then keep it as new.
    fs::write(dir.join("A.ARW"), b"another photo").unwrap();
    let h =
        Arc::new(open_in_place(&dir, None, None, &LOCAL, &|| false, &mut |_, _, _| {}).unwrap());
    let r = h
        .hash_pending_with(&LOCAL, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!(r.changed, 1, "{r:?}");
    let a = h
        .list(&LOCAL, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == "A.ARW")
        .unwrap();
    assert!(a.file_changed);
    block_on(
        h.clone()
            .dismiss_file_changed(a.variant_id, CancelToken::new()),
    )
    .unwrap();
    assert_eq!(rating(&h, "A.ARW", &LOCAL), Some((0, false, false)));
    assert!(block_on(h.clone().restore_to_folder(424_242, CancelToken::new())).is_err());
}

/// R4-H3 with real A9 files: exiftool-style metadata rewrites keep culling;
/// a change in the sensor data flags the file (persistently) and can be
/// restored.
#[test]
fn real_raws_keep_identity_across_metadata_rewrites() {
    let samples = ds_testdata::samples_with_ext("ARW");
    let [s1, s2, ..] = samples.as_slice() else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().canonicalize().unwrap();
    fs::copy(s1, dir.join("A.ARW")).unwrap();
    fs::copy(s2, dir.join("B.ARW")).unwrap();
    let raw = Probe {
        volume_ok: |_| Ok(()),
        ..Probe::RAW
    };
    let open =
        || Arc::new(open_in_place(&dir, None, None, &raw, &|| false, &mut |_, _, _| {}).unwrap());
    let h = open();
    let r = h
        .hash_pending_with(&raw, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!(r.hashed, 2);
    rate(&h, "A.ARW", 5);
    rate(&h, "B.ARW", 4);
    drop(h);
    // A: exiftool -P style rewrite (temp file renamed over, mtime kept) that
    // changes a tag value outside the sensor data.
    let a = dir.join("A.ARW");
    let mut bytes = fs::read(&a).unwrap();
    let ranges = ds_raw::image_data_ranges(&a).unwrap().unwrap();
    let model = bytes.windows(6).position(|w| w == b"ILCE-9").unwrap();
    assert!(
        ranges
            .iter()
            .all(|&(o, l)| (model as u64) < o || model as u64 >= o + l)
    );
    bytes[model + 5] = b'8';
    let mtime = fs::metadata(&a).unwrap().modified().unwrap();
    let tmp = dir.join("A.ARW_exiftool_tmp");
    fs::write(&tmp, &bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::rename(&tmp, &a).unwrap();
    // B: one byte of sensor data changed in place.
    let b = dir.join("B.ARW");
    let (off, _) = ds_raw::image_data_ranges(&b).unwrap().unwrap()[0];
    let f = fs::File::options().write(true).open(&b).unwrap();
    f.write_all_at(&[0x5a], off + 4096).unwrap();
    drop(f);
    let h = open();
    assert_eq!(h.report.to_verify, 2);
    let r = h
        .hash_pending_with(&raw, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!((r.metadata_edited, r.changed), (1, 1), "{r:?}");
    assert_eq!(
        rating(&h, "A.ARW", &raw),
        Some((5, false, false)),
        "culling kept"
    );
    assert_eq!(
        rating(&h, "B.ARW", &raw),
        Some((0, true, false)),
        "file changed"
    );
    drop(h);
    // Relaunch: the badge is still there, nothing is re-checked.
    let h = open();
    assert_eq!(h.report.to_verify, 0);
    assert_eq!(rating(&h, "B.ARW", &raw), Some((0, true, false)));
    // Restore the previous metadata onto the changed file.
    let id = h
        .list(&raw, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == "B.ARW")
        .unwrap()
        .variant_id;
    block_on(h.clone().restore_previous(id, CancelToken::new())).unwrap();
    assert_eq!(rating(&h, "B.ARW", &raw), Some((4, false, false)));
    // Locate accepts a metadata-edited copy of a missing photo.
    drop(h);
    let away = tempfile::tempdir().unwrap();
    fs::rename(&a, away.path().join("A.ARW")).unwrap();
    let h = open();
    assert_eq!(h.report.missing, 1);
    // It comes back re-tagged (another metadata byte changed) under a new name.
    let mut bytes = fs::read(away.path().join("A.ARW")).unwrap();
    bytes[model + 5] = b'7';
    fs::write(dir.join("A-moved.bin"), &bytes).unwrap();
    let aid = h
        .list(&raw, &|| false)
        .unwrap()
        .into_iter()
        .find(|r| r.file_name == "A.ARW")
        .unwrap()
        .variant_id;
    fs::rename(dir.join("A-moved.bin"), dir.join("A-found.ARW")).unwrap();
    block_on(h.clone().locate(
        aid,
        dir.join("A-found.ARW").to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(rating(&h, "A-found.ARW", &raw), Some((5, false, false)));
}

/// R4-M3 measurement: culling latency while the background pass hashes a big
/// folder. `DS_M3_FOLDER=<dir of unique-content ARWs> cargo test --release -p
/// ds-ffi contention -- --ignored --nocapture`. The folder is only read.
#[test]
#[ignore = "needs DS_M3_FOLDER"]
fn contention_while_hashing() {
    let Some(src) = std::env::var_os("DS_M3_FOLDER") else {
        return;
    };
    let src = PathBuf::from(src);
    let store = tempfile::tempdir().unwrap();
    let fb = store.path().join("store");
    let probe = Probe {
        volume_ok: |_| Err("fixture is read-only".into()),
        ..Probe::RAW
    };
    let t0 = std::time::Instant::now();
    let h = Arc::new(
        open_in_place(&src, Some(&fb), None, &probe, &|| false, &mut |_, _, _| {}).unwrap(),
    );
    let v = h.list(&probe, &|| false).unwrap();
    eprintln!("M3: {} files open+list {:?}", v.len(), t0.elapsed());
    let paths: Vec<String> = v.iter().map(|r| r.original_path.clone()).collect();
    let ids: Vec<i64> = v.iter().map(|r| r.variant_id).collect();
    let percentile = |mut xs: Vec<f64>, p: f64| {
        xs.sort_by(f64::total_cmp);
        xs[((xs.len() as f64 - 1.0) * p).round() as usize]
    };
    for with_activity in [false, true] {
        // Fresh store per pass so there is always hashing to do.
        let store = tempfile::tempdir().unwrap();
        let fb = store.path().join("store");
        let h = Arc::new(
            open_in_place(&src, Some(&fb), None, &probe, &|| false, &mut |_, _, _| {}).unwrap(),
        );
        let ids: Vec<i64> = h
            .list(&probe, &|| false)
            .unwrap()
            .iter()
            .map(|r| r.variant_id)
            .collect();
        let hasher = {
            let h = h.clone();
            std::thread::spawn(move || {
                let t = std::time::Instant::now();
                let r = h.hash_pending_with(&Probe::RAW, &|| false, &mut |_, _| {});
                (t.elapsed(), r.map(|r| r.hashed))
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (mut prev, mut rate_ms) = (Vec::new(), Vec::new());
        for i in 0..150 {
            if hasher.is_finished() {
                break;
            }
            if with_activity {
                crate::background::mark_activity();
            }
            let p = paths[(i * 37) % paths.len()].clone();
            let t = std::time::Instant::now();
            block_on(crate::raw::embedded_preview(p, CancelToken::new())).unwrap();
            prev.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = std::time::Instant::now();
            block_on(h.clone().set_rating(
                vec![ids[i % ids.len()]],
                (i % 5) as u8,
                CancelToken::new(),
            ))
            .unwrap();
            rate_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if with_activity {
            // Let the pass finish once the user is idle.
        }
        let (took, hashed) = hasher.join().unwrap();
        eprintln!(
            "M3 activity={with_activity}: preview p50 {:.1} p95 {:.1} max {:.1} ms; \
             rating p50 {:.1} p95 {:.1} max {:.1} ms ({} samples); hash pass {:?} ({:?} files)",
            percentile(prev.clone(), 0.5),
            percentile(prev.clone(), 0.95),
            percentile(prev.clone(), 1.0),
            percentile(rate_ms.clone(), 0.5),
            percentile(rate_ms.clone(), 0.95),
            percentile(rate_ms.clone(), 1.0),
            prev.len(),
            took,
            hashed
        );
    }
    let _ = (h, ids);
}

/// R4-M1 measurement: reopen after a Finder bulk rename and after `touch`
/// of every file. `DS_M1_FOLDER=<scratch copy of unique-content ARWs> cargo
/// test --release -p ds-ffi reopen_after_bulk -- --ignored --nocapture`.
/// **Renames and touches the files in that folder** (restores the names).
#[test]
#[ignore = "needs DS_M1_FOLDER (a scratch folder it may rename and touch)"]
fn reopen_after_bulk_rename_and_touch() {
    let Some(src) = std::env::var_os("DS_M1_FOLDER") else {
        return;
    };
    let src = PathBuf::from(src);
    let store = tempfile::tempdir().unwrap();
    let fb = store.path().join("store");
    let probe = Probe {
        volume_ok: |_| Err("measure with the store outside".into()),
        ..Probe::RAW
    };
    let open = || {
        let t = std::time::Instant::now();
        let h = open_in_place(&src, Some(&fb), None, &probe, &|| false, &mut |_, _, _| {}).unwrap();
        let n = h.list(&probe, &|| false).unwrap().len();
        (h, n, t.elapsed())
    };
    let bg = |h: &SessionHandle| {
        let t = std::time::Instant::now();
        let r = h
            .hash_pending_with(&Probe::RAW, &|| false, &mut |_, _| {})
            .unwrap();
        (r, t.elapsed())
    };
    let (h, n, first) = open();
    let (r, hash) = bg(&h);
    eprintln!(
        "M1 first open+list {n} files: {first:?}; background {hash:?} (hashed {})",
        r.hashed
    );
    drop(h);
    let names: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = fs::read_dir(&src)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| arw(p))
            .collect();
        v.sort();
        v
    };
    for (i, p) in names.iter().enumerate() {
        fs::rename(p, src.join(format!("Trip_{i:05}.ARW"))).unwrap();
    }
    let (h, n2, reopen) = open();
    eprintln!(
        "M1 bulk rename: reopen+list {n2} files {reopen:?} (relinked {}, held {}, verify {})",
        h.report.relinked, h.report.held, h.report.to_verify
    );
    let (r, t) = bg(&h);
    eprintln!("M1 bulk rename: background {t:?} {r:?}");
    drop(h);
    let later = std::time::SystemTime::now();
    for i in 0..names.len() {
        let p = src.join(format!("Trip_{i:05}.ARW"));
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
    let (h, n3, reopen) = open();
    eprintln!(
        "M1 touch: reopen+list {n3} files {reopen:?} (verify {})",
        h.report.to_verify
    );
    let (r, t) = bg(&h);
    eprintln!(
        "M1 touch: background {t:?} (verified {}, changed {}, hashed {})",
        r.verified, r.changed, r.hashed
    );
    drop(h);
    for (i, p) in names.iter().enumerate() {
        fs::rename(src.join(format!("Trip_{i:05}.ARW")), p).unwrap();
    }
}

fn tail_ranges(p: &Path) -> Option<Vec<(u64, u64)>> {
    let len = fs::metadata(p).ok()?.len();
    (len > 8).then(|| vec![(8, len - 8)])
}

fn read_or_bad(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    let bytes = fs::read(p).ok()?;
    if bytes.starts_with(b"BAD") {
        return None;
    }
    read(p)
}

/// Synthetic end-to-end of the background decisions: sample checks on
/// untrusted volumes, metadata-only rewrites (the first 8 bytes stand in for
/// the tags), an unhashed file rewritten with the same capture metadata, and
/// held files whose metadata can't be read.
#[test]
fn background_decisions_on_synthetic_files() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().canonicalize().unwrap();
    let probe = Probe {
        ranges: tail_ranges,
        untrusted_stat: |_| true,
        read: read_or_bad,
        ..LOCAL
    };
    folder_with(
        &dir,
        &[
            ("A.ARW", b"tagsAAAAsensor-a"),
            ("B.ARW", b"tagsBBBBsensor-b"),
            ("C.ARW", b"tagsCCCCsensor-c"),
        ],
    );
    let open =
        || Arc::new(open_in_place(&dir, None, None, &probe, &|| false, &mut |_, _, _| {}).unwrap());
    let h = open();
    // First pass: hash A and B only (C stays registered-by-stat).
    let c = dir.join("C.ARW");
    let c_bytes = fs::read(&c).unwrap();
    let c_stat_mtime = fs::metadata(&c).unwrap().modified().unwrap();
    fs::remove_file(&c).unwrap();
    let r = h
        .hash_pending_with(&probe, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!(r.hashed, 2);
    fs::write(&c, &c_bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&c)
        .unwrap()
        .set_modified(c_stat_mtime)
        .unwrap();
    for n in ["A.ARW", "B.ARW", "C.ARW"] {
        rate(&h, n, 3);
    }
    drop(h);
    // Untrusted volume, nothing changed: only sample checks (A, B hashed).
    let h = open();
    assert_eq!(h.report.to_verify, 2, "sample checks for hashed files");
    let r = h
        .hash_pending_with(&probe, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!(
        (r.verified, r.changed, r.metadata_edited),
        (0, 0, 0),
        "{r:?}"
    );
    drop(h);
    // A: tags rewritten in place, same size and mtime (the stat can't tell).
    let a = dir.join("A.ARW");
    let mtime = fs::metadata(&a).unwrap().modified().unwrap();
    fs::write(&a, b"TAGSaaaasensor-a").unwrap();
    fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    // C (never hashed): rewritten with the same length (same fake capture time).
    fs::write(&c, b"tagsXXXXsensor-c").unwrap();
    // A new file whose metadata can't be read, same size as nothing missing: registered path.
    fs::write(dir.join("D.ARW"), b"BAD file").unwrap();
    let h = open();
    let r = h
        .hash_pending_with(&probe, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!(
        r.metadata_edited, 2,
        "A by its sensor data, C by its capture metadata: {r:?}"
    );
    assert_eq!(r.changed, 0);
    assert_eq!(rating(&h, "A.ARW", &probe), Some((3, false, false)));
    assert_eq!(rating(&h, "C.ARW", &probe), Some((3, false, false)));
    assert!(
        h.report
            .failed
            .iter()
            .any(|f| f.starts_with("Capture/D.ARW")),
        "{:?}",
        h.report.failed
    );
    // A held file with unreadable metadata is reported, not registered.
    drop(h);
    let away = tempfile::tempdir().unwrap();
    fs::rename(dir.join("B.ARW"), away.path().join("B.away")).unwrap();
    fs::write(dir.join("E.ARW"), b"BADBBBBBsensor-x").unwrap(); // same size as the missing B
    let h = open();
    assert_eq!(h.report.held, 1, "{:?}", h.report);
    let r = h
        .hash_pending_with(&probe, &|| false, &mut |_, _| {})
        .unwrap();
    assert_eq!((r.relinked, r.registered), (0, 0), "{r:?}");
    assert!(r.failed.iter().any(|f| f.contains("E.ARW")), "{r:?}");
}
