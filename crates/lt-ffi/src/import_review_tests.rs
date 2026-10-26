//! Integration A review fixes (C1, H1, H2, H3, M1, M5, L1–L3).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use futures_executor::block_on;

use super::*;

fn card(root: &Path, n: usize, size: usize) -> Vec<PathBuf> {
    let dcim = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dcim).unwrap();
    (0..n)
        .map(|i| {
            let p = dcim.join(format!("DSC{:05}.ARW", 100 + i));
            fs::write(&p, vec![i as u8 + 1; size + i * 13]).unwrap();
            p
        })
        .collect()
}

/// A small first file and two big ones (6 chunks each): with a throttle, a
/// stop after the first file leaves the others unfinished.
fn uneven_card(root: &Path) -> Vec<PathBuf> {
    let dcim = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dcim).unwrap();
    [100_000usize, 24 << 20, 24 << 20]
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let p = dcim.join(format!("DSC{:05}.ARW", 100 + i));
            fs::write(&p, vec![i as u8 + 1; n]).unwrap();
            p
        })
        .collect()
}

fn options(lib: &Path, name: &str, index: &Path) -> ImportOptions {
    ImportOptions {
        session_dir: lib.join(name).to_string_lossy().into_owned(),
        session_name: name.into(),
        create_session: true,
        include: None,
        subfolder: None,
        rename: None,
        backup_dest: None,
        verify_source_reread: false,
        skip_duplicates: true,
        global_index: Some(index.to_string_lossy().into_owned()),
        auto_apply: None,
        metadata_preset: None,
        throttle_ms: 0,
    }
}

fn scan(root: &Path, index: Option<&Path>) -> Arc<CardScan> {
    Arc::new(scan_blocking(root, index, &|| false).unwrap())
}

fn import(s: &Arc<CardScan>, o: ImportOptions) -> Result<ImportResult, DsError> {
    block_on(s.clone().import(o, None, CancelToken::new()))
}

fn card_changed(e: &DsError) -> bool {
    matches!(e, DsError::InvalidArgument { message } if message.starts_with(CARD_CHANGED))
}

fn listing(root: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), blake3::hash(&fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

/// C1: another card with the same names at the same mount point is refused:
/// by volume identity, by the scanned size/mtime, and — for files an earlier
/// run copied — by the journal's partial hash. Nothing is created.
#[test]
fn a_different_card_at_the_same_path_is_never_imported() {
    let t = tempfile::tempdir().unwrap();
    let (root, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    card(&root, 3, 4000);
    // Another card: the scan recorded a different volume.
    let mut other = scan_blocking(&root, None, &|| false).unwrap();
    other.volume_id = "SOME-OTHER-CARD".into();
    let e = import(&Arc::new(other), options(&lib, "Swap", &index)).unwrap_err();
    assert!(card_changed(&e), "{e:?}");
    assert!(!lib.join("Swap").exists(), "nothing created");

    // Same volume, but a file is not the one scanned (card B: same names).
    let s = scan(&root, None);
    fs::write(root.join("DCIM/100MSDCF/DSC00101.ARW"), vec![9u8; 7777]).unwrap();
    let e = import(&s, options(&lib, "Swap", &index)).unwrap_err();
    assert!(card_changed(&e), "{e:?}");
    assert!(!lib.join("Swap").exists());
    // Rescanned: fine.
    let s = scan(&root, None);
    assert!(
        import(&s, options(&lib, "Swap", &index))
            .unwrap()
            .is_complete
    );
}

/// Cancels the import as soon as its first file is done.
struct StopAfterFirst(Arc<CancelToken>);

impl ProgressListener for StopAfterFirst {
    fn on_progress(&self, event: ProgressEvent) -> Result<(), DsError> {
        if event.kind == "import.files" && event.completed >= 1 {
            self.0.cancel();
        }
        Ok(())
    }
}

fn stop_after_first_file(s: &Arc<CardScan>, o: ImportOptions) -> ImportResult {
    let token = CancelToken::new();
    let l = Arc::new(StopAfterFirst(token.clone()));
    block_on(s.clone().import(o, Some(l), token)).unwrap()
}

/// C1: a look-alike file (same name, size and mtime) an earlier run already
/// copied is caught by the journal's partial hash on resume.
#[test]
fn resume_checks_the_journal_partial_hash() {
    let t = tempfile::tempdir().unwrap();
    let (root, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    let files = uneven_card(&root);
    let s = scan(&root, None);
    let mut o = options(&lib, "Look", &index);
    o.throttle_ms = 30;
    let r = stop_after_first_file(&s, o.clone());
    assert!(r.cancelled && !r.copied.is_empty(), "{r:?}");
    // The copied file is replaced by a look-alike: same size and mtime.
    let done = PathBuf::from(&r.copied[0].src);
    let m = fs::metadata(&done).unwrap();
    let mut bytes = fs::read(&done).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&done, &bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&done)
        .unwrap()
        .set_modified(m.modified().unwrap())
        .unwrap();
    let s = scan(&root, None);
    o.throttle_ms = 0;
    let e = import(&s, o).unwrap_err();
    assert!(card_changed(&e), "{e:?}");
    assert!(files.iter().all(|f| f.exists()));
}

/// H1: only a card whose every media file has a verified library copy is
/// "safe to remove"; unticked files are listed as staying on the card; a
/// camera database file doesn't count as media, `notes.txt` does stay.
#[test]
fn safe_to_remove_needs_every_media_file_in_the_library() {
    let t = tempfile::tempdir().unwrap();
    // A real exFAT card: lt-import trusts library copies only on another device.
    let Some(img) =
        lt_fsutil::testvol::require(t.path(), lt_fsutil::testvol::Fs::ExFat, 64, "SAFECARD")
    else {
        return;
    };
    let root = img.mount_point().to_path_buf();
    let (lib, index) = (t.path().join("lib"), t.path().join("g.db"));
    let files = card(&root, 3, 5000);
    fs::create_dir_all(root.join("PRIVATE/M4ROOT")).unwrap();
    fs::write(root.join("PRIVATE/M4ROOT/MEDIAPRO.XML"), b"<db/>").unwrap();
    let s = scan(&root, Some(&index));
    // Untick two of three.
    let mut o = options(&lib, "Part", &index);
    o.include = Some(vec![files[0].to_string_lossy().into_owned()]);
    let r = import(&s, o).unwrap();
    assert!(r.is_complete, "the plan completed: {r:?}");
    assert!(!r.safe_to_remove);
    let left: Vec<&str> = r.card_only.iter().map(|i| i.reason.as_str()).collect();
    assert_eq!(r.card_only.len(), 2, "{:?}", r.card_only);
    assert!(left.iter().all(|r| *r == "not selected for import"));
    // Import the rest into the same session: now everything is in the library
    // (the first file counts via the global index, re-verified).
    let s = scan(&root, Some(&index));
    let mut o = options(&lib, "Part", &index);
    o.create_session = false;
    o.include = Some(
        files[1..]
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect(),
    );
    let r = import(&s, o).unwrap();
    assert!(r.card_only.is_empty(), "{:?}", r.card_only);
    assert!(r.safe_to_remove, "only a camera database is left: {r:?}");
    // A note on the card stays only there: not "safe".
    fs::write(root.join("DCIM/notes.txt"), b"x").unwrap();
    let s = scan(&root, Some(&index));
    let mut o = options(&lib, "Part", &index);
    o.create_session = false;
    o.include = Some(vec![]);
    let r = import(&s, o).unwrap();
    assert!(r.card_only.is_empty() && !r.safe_to_remove, "{r:?}");
    drop(img);
}

/// H3: a file the session can't register is not imported: listed in
/// `register_errors`, not safe, the journal stays; opening the session
/// registers it (`recover_unregistered`), and so does importing again.
#[test]
fn registration_failure_is_not_imported_and_open_recovers() {
    let t = tempfile::tempdir().unwrap();
    let (root, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    let files = card(&root, 3, 6000);
    // A first import creates the session (one photo).
    let s = scan(&root, Some(&index));
    let mut o = options(&lib, "Reg", &index);
    o.include = Some(vec![files[0].to_string_lossy().into_owned()]);
    import(&s, o.clone()).unwrap();
    // The reviewer's repro: the sidecar folder becomes read-only.
    let settings = lib.join("Reg/Capture/.lt/settings");
    assert!(settings.is_dir());
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o555)).unwrap();
    let s = scan(&root, Some(&index));
    o.create_session = false;
    o.include = None;
    let r = import(&s, o.clone());
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o755)).unwrap();
    let r = r.unwrap();
    assert_eq!(r.register_errors.len(), 2, "{r:?}");
    assert!(
        r.register_errors
            .iter()
            .all(|e| !e.reason.starts_with(NOT_REGISTERED))
    );
    assert!(!r.is_complete && !r.safe_to_remove);
    assert_eq!(
        r.card_only.len(),
        2,
        "not registered = still only on the card: {:?}",
        r.card_only
    );
    assert!(
        pending_import(o.session_dir.clone()).unwrap().is_some(),
        "journal kept"
    );
    // The files are verified in Capture/ but not in the session: opening it adds them.
    let before = Session::open(Path::new(&o.session_dir))
        .unwrap()
        .images()
        .unwrap()
        .len();
    assert_eq!(before, 1);
    let h = block_on(crate::session::open_session(
        o.session_dir.clone(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(h.open_report().unwrap().added, 2);
    drop(h);
    let sess = Session::open(Path::new(&o.session_dir)).unwrap();
    assert_eq!(sess.images().unwrap().len(), 3);
    drop(sess);
    // Importing again finishes the journal (already registered = fine).
    let s = scan(&root, Some(&index));
    let r = import(&s, o.clone()).unwrap();
    assert!(r.is_complete && r.register_errors.is_empty(), "{r:?}");
    assert!(pending_import(o.session_dir.clone()).unwrap().is_none());
}

/// H3: recovery registers photos (a RAW+JPG pair once), never sidecars,
/// journals or `.partial`s, and nothing in an in-place session.
#[test]
fn recovery_registers_only_whole_photos() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("S");
    let mut s = Session::create(&dir, "S").unwrap();
    let cap = dir.join("Capture/day");
    fs::create_dir_all(&cap).unwrap();
    fs::write(cap.join("A.ARW"), vec![1u8; 5000]).unwrap();
    fs::write(cap.join("A.JPG"), vec![2u8; 500]).unwrap();
    fs::write(cap.join("C0001.MP4"), vec![3u8; 900]).unwrap();
    fs::write(cap.join(".B.ARW.partial"), vec![4u8; 50]).unwrap();
    fs::create_dir_all(dir.join("Capture/.lt")).unwrap();
    fs::write(dir.join("Capture/.lt/import-journal-x.json"), b"{}").unwrap();
    let r = recover_unregistered(&mut s);
    assert_eq!(r.added, 2, "{r:?}");
    let rels: Vec<String> = s
        .images()
        .unwrap()
        .iter()
        .map(|i| i.rel_path.as_str().to_owned())
        .collect();
    assert!(
        rels.contains(&"Capture/day/A.ARW".to_owned()) && rels.len() == 2,
        "{rels:?}"
    );
    assert_eq!(
        recover_unregistered(&mut s),
        Recovered::default(),
        "nothing new"
    );
    // In-place sessions are never touched here.
    let folder = t.path().join("F");
    fs::create_dir_all(&folder).unwrap();
    let mut ext = Session::create_external(&t.path().join("store"), "F", &folder).unwrap();
    assert_eq!(recover_unregistered(&mut ext), Recovered::default());
}

/// M1: an import that can't fit fails before anything is created, with a
/// clear message (real small APFS disk image as the library volume).
#[test]
fn preflight_refuses_an_import_that_cannot_fit() {
    let t = tempfile::tempdir().unwrap();
    let Some(img) =
        lt_fsutil::testvol::require(t.path(), lt_fsutil::testvol::Fs::Apfs, 40, "IMPFULL")
    else {
        return;
    };
    let lib = img.mount_point().join("lib");
    let (root, index) = (t.path().join("card"), t.path().join("g.db"));
    card(&root, 3, 20 << 20); // 60 MB onto a 40 MB volume
    let free = free_space(lib.to_string_lossy().into_owned()).unwrap();
    assert!(free < 60 << 20, "{free}");
    let s = scan(&root, None);
    let e = import(&s, options(&lib, "Full", &index)).unwrap_err();
    assert!(
        matches!(&e, DsError::Io { message } if message.starts_with(NOT_ENOUGH_SPACE)),
        "{e:?}"
    );
    assert!(!lib.join("Full").exists(), "no empty session left behind");
    // One file fits.
    let mut o = options(&lib, "One", &index);
    o.include = Some(vec![
        s.report.candidates[0]
            .primary
            .path
            .to_string_lossy()
            .into_owned(),
    ]);
    let r = import(&s, o).unwrap();
    assert!(r.is_complete, "{r:?}");
    drop(img);
}

/// L1 + L2: clips get no auto-apply edits; turning auto-apply off clears
/// the session's preset for later imports.
#[test]
fn auto_apply_skips_clips_and_off_clears_the_preset() {
    let t = tempfile::tempdir().unwrap();
    let (root, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    card(&root, 1, 3000);
    let clips = root.join("PRIVATE/M4ROOT/CLIP");
    fs::create_dir_all(&clips).unwrap();
    fs::write(clips.join("C0001.MP4"), vec![5u8; 2000]).unwrap();
    let mut adj: lt_core::Adjustments =
        serde_json::from_str(&crate::session::default_adjustments_json().unwrap()).unwrap();
    adj.exposure.exposure_ev = 1.5;
    let preset = AutoApplyPreset {
        name: "Bright".into(),
        adjustments_json: serde_json::to_string(&adj).unwrap(),
        groups: vec![crate::clipboard::AdjustGroup::Exposure],
        relative: vec![],
    };
    let s = scan(&root, None);
    let mut o = options(&lib, "Auto", &index);
    o.auto_apply = Some(preset);
    let r = import(&s, o.clone()).unwrap();
    assert!(r.is_complete, "{r:?}");
    let sess = Session::open(Path::new(&r.session_dir)).unwrap();
    for img in sess.images().unwrap() {
        let v = sess.variants(img.id).unwrap()[0];
        let edited = sess.variant(v).unwrap().edited;
        let clip = img.rel_path.as_str().to_ascii_lowercase().ends_with(".mp4");
        assert_eq!(edited, !clip, "{}", img.rel_path.as_str());
    }
    assert!(sess.auto_apply_preset().is_some());
    drop(sess);
    // Auto-apply switched off: the next import clears the session's preset.
    fs::write(root.join("DCIM/100MSDCF/DSC00200.ARW"), vec![8u8; 3100]).unwrap();
    let s = scan(&root, None);
    o.auto_apply = None;
    o.create_session = false;
    let r = import(&s, o).unwrap();
    let sess = Session::open(Path::new(&r.session_dir)).unwrap();
    assert!(sess.auto_apply_preset().is_none());
    let newest = sess
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str().ends_with("DSC00200.ARW"))
        .unwrap();
    let v = sess.variants(newest.id).unwrap()[0];
    assert!(!sess.variant(v).unwrap().edited);
}

/// L3: a damaged library copy is reported at scan, the card file is imported
/// again, and the result says so.
#[test]
fn a_damaged_library_copy_is_reported_and_imported_again() {
    let t = tempfile::tempdir().unwrap();
    let (root, index) = (t.path().join("card"), t.path().join("g.db"));
    let files = card(&root, 1, 400_000);
    // A library session that holds the file, recorded in the global index,
    // then damaged in the middle (same size, head and tail intact).
    let libdir = t.path().join("lib/Old");
    let mut sess = Session::create(&libdir, "Old").unwrap();
    let copy = libdir.join("Capture/DSC00100.ARW");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&files[0], &copy).unwrap();
    let h = full_hash(&copy).unwrap();
    let prepared = prepare(&copy, &copy, FileKind::Raw, None, &Listings::default());
    register(&mut sess, &libdir, &copy, FileKind::Raw, h, prepared).unwrap();
    GlobalIndex::open(&index)
        .unwrap()
        .sync_session(&sess)
        .unwrap();
    drop(sess);
    let mut bytes = fs::read(&copy).unwrap();
    bytes[200_000] ^= 0xff;
    fs::write(&copy, &bytes).unwrap();
    // A partial-hash match alone is not a duplicate: the damaged copy is named.
    let d = GlobalDedupe::open(&index).unwrap();
    assert_eq!(d.damaged_copy(&files[0], 400_000), Some(copy.clone()));
    // The card sits on the same device as the library in this test, where
    // lt-import never trusts a copy for dedupe; the report still names it.
    let s = scan(&root, Some(&index));
    let c = &s.summary().unwrap().candidates[0];
    assert!(!c.already_imported);
    assert_eq!(
        c.damaged_library_copy.as_deref(),
        Some(copy.to_string_lossy().as_ref())
    );
    let lib = t.path().join("lib");
    let r = import(&s, options(&lib, "New", &index)).unwrap();
    assert_eq!(r.damaged_library_copies.len(), 1, "{r:?}");
    assert!(r.damaged_library_copies[0].reason.contains("damaged"));
}

/// M5: pending imports name their card and backup root; card metadata and
/// free space helpers for the sheet.
#[test]
fn pending_imports_name_the_card_and_helpers_work() {
    let t = tempfile::tempdir().unwrap();
    let (root, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    uneven_card(&root);
    let s = scan(&root, None);
    let mut o = options(&lib, "Stopped", &index);
    o.backup_dest = Some(t.path().join("bk").to_string_lossy().into_owned());
    o.throttle_ms = 30;
    let token = CancelToken::new();
    token.cancel();
    // Cancelled before the job starts: an error, nothing created.
    assert!(block_on(s.clone().import(o.clone(), None, token)).is_err());
    let r = stop_after_first_file(&s, o.clone());
    assert!(r.cancelled && !r.card_only.is_empty());
    assert!(
        r.card_only.iter().any(|i| i.reason.contains("stopped")),
        "{:?}",
        r.card_only
    );
    let p = pending_imports(vec![], vec![o.session_dir.clone()]).unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].volumes, vec![s.volume_id.clone()]);
    assert_eq!(p[0].backup_dests, vec![o.backup_dest.clone().unwrap()]);
    discard_pending_import(o.session_dir.clone(), None).unwrap();
    assert!(pending_import(o.session_dir.clone()).unwrap().is_none());
    // Helpers.
    assert!(
        free_space(
            t.path()
                .join("not/yet/there")
                .to_string_lossy()
                .into_owned()
        )
        .is_some()
    );
    let m = card_file_meta(
        root.join("DCIM/100MSDCF/DSC00100.ARW")
            .to_string_lossy()
            .into_owned(),
    );
    assert_eq!(
        m,
        CardFileMeta {
            camera: None,
            capture_ms: None
        },
        "not a real RAW"
    );
    if let Some(sample) = lt_testdata::samples_with_ext("ARW").first() {
        let m = card_file_meta(sample.to_string_lossy().into_owned());
        assert_eq!(m.camera.as_deref(), Some("ILCE-9"));
        assert!(m.capture_ms.is_some());
    }
    assert_eq!(listing(&root).len(), 3, "the card is untouched");
}
