use std::collections::BTreeMap;
use std::sync::Mutex;

use futures_executor::block_on;

use super::*;

#[derive(Default)]
struct Recorder(Mutex<Vec<ProgressEvent>>);

impl ProgressListener for Recorder {
    fn on_progress(&self, event: ProgressEvent) -> Result<(), DsError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

/// Every file under `root` with its bytes' BLAKE3 and mtime: the card must be
/// byte-identical (and untouched) after an import.
/// Folders macOS itself writes at a mounted volume's root (`.Trashes/._501` appeared
/// once on the test image with nothing of ours involved: the Finder/`fseventsd` touch
/// every writable volume). They aren't the card's content and our code never writes
/// anywhere on a card, so the listing leaves them out; every other file (the DCIM and
/// PRIVATE trees, and any stray file) must stay byte-identical, with its mtime.
const OS_METADATA: [&str; 4] = [
    ".Trashes",
    ".fseventsd",
    ".Spotlight-V100",
    ".TemporaryItems",
];

fn listing(root: &Path) -> BTreeMap<PathBuf, (blake3::Hash, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if d == root && OS_METADATA.iter().any(|n| e.file_name() == *n) {
                continue;
            }
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                let h = blake3::hash(&fs::read(&p).unwrap());
                out.insert(
                    p.strip_prefix(root).unwrap().to_owned(),
                    (h, m.modified().unwrap()),
                );
            }
        }
    }
    out
}

/// A card-like folder: real sample ARWs when present (else synthetic ones),
/// a RAW+JPG pair, an MP4 and an unknown file.
fn make_card(root: &Path, raws: usize) -> Vec<PathBuf> {
    let dcim = root.join("DCIM/100MSDCF");
    fs::create_dir_all(&dcim).unwrap();
    let samples = ds_testdata::samples_with_ext("ARW");
    let mut out = Vec::new();
    for i in 0..raws {
        let p = dcim.join(format!("DSC{:05}.ARW", 100 + i));
        match samples.get(i) {
            Some(s) => {
                fs::copy(s, &p).unwrap();
            }
            None => fs::write(&p, vec![i as u8 + 1; 200_000 + i * 17]).unwrap(),
        }
        out.push(p);
    }
    fs::write(dcim.join("DSC00100.JPG"), b"jpeg pair of the first raw").unwrap();
    let clip = root.join("PRIVATE/M4ROOT/CLIP");
    fs::create_dir_all(&clip).unwrap();
    fs::write(clip.join("C0001.MP4"), vec![7u8; 50_000]).unwrap();
    fs::write(root.join("DCIM/notes.txt"), b"camera junk").unwrap();
    out
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
    block_on(scan_card(
        root.to_string_lossy().into_owned(),
        index.map(|p| p.to_string_lossy().into_owned()),
        CancelToken::new(),
    ))
    .unwrap()
}

fn import(
    s: &Arc<CardScan>,
    o: ImportOptions,
    listener: Option<Arc<dyn ProgressListener>>,
    token: Arc<CancelToken>,
) -> Result<ImportResult, DsError> {
    block_on(s.clone().import(o, listener, token))
}

#[test]
#[allow(clippy::too_many_lines)]
fn imports_a_card_into_a_new_session_registers_and_dedupes() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("global.db"),
    );
    make_card(&card, 3);
    let before = listing(&card);

    let s = scan(&card, Some(&index));
    let sum = s.summary().unwrap();
    assert_eq!(
        sum.candidates.len(),
        4,
        "3 RAWs and a clip: {:?}",
        sum.candidates
    );
    let first = sum
        .candidates
        .iter()
        .find(|c| c.path.ends_with("DSC00100.ARW"))
        .unwrap();
    assert_eq!(first.kind, ImportFileKind::Raw);
    assert!(
        first.sidecars.iter().any(|p| p.ends_with("DSC00100.JPG")),
        "RAW+JPG pair"
    );
    assert!(
        sum.candidates
            .iter()
            .any(|c| c.kind == ImportFileKind::Video)
    );
    assert!(sum.candidates.iter().all(|c| !c.already_imported));
    assert!(
        sum.skipped_unknown
            .iter()
            .any(|i| i.path.ends_with("notes.txt"))
    );
    assert_eq!(sum.scan_errors.len(), 0);
    assert_eq!(
        sum.total_bytes,
        sum.candidates.iter().map(|c| c.total_bytes).sum::<u64>()
    );

    let rec = Arc::new(Recorder::default());
    let r = import(
        &s,
        options(&lib, "Trip", &index),
        Some(rec.clone()),
        CancelToken::new(),
    )
    .unwrap();
    assert!(r.is_complete, "{r:?}");
    assert!(
        r.eject_needs_confirmation,
        "notes.txt was skipped: the user must acknowledge it"
    );
    assert_eq!(r.copied.len(), 5, "4 primaries + the JPG sidecar");
    assert_eq!(r.registered, 4, "sidecars aren't photos");
    assert!(r.register_errors.is_empty(), "{:?}", r.register_errors);
    // Every media file has a verified copy; notes.txt stays only on the card.
    assert!(r.card_only.is_empty(), "{:?}", r.card_only);
    assert!(!r.safe_to_remove, "notes.txt is only on the card");
    assert!(!r.volume_id.is_empty() && r.volume_id == sum.volume_id);
    assert!(
        r.skipped_unknown
            .iter()
            .any(|i| i.path.ends_with("notes.txt"))
    );
    assert_eq!(r.session_name, "Trip");
    assert!(
        r.copied
            .iter()
            .all(|c| c.dest.starts_with(&r.session_dir) && !c.resumed)
    );
    let ev = rec.0.lock().unwrap().clone();
    assert!(
        ev.iter()
            .any(|e| e.kind == "import" && e.phase == TaskPhase::Started)
    );
    assert!(
        ev.iter()
            .any(|e| e.kind == "import" && e.phase == TaskPhase::Finished)
    );
    assert!(
        ev.iter()
            .any(|e| e.kind == "import.files" && e.completed == e.total && e.total == 5)
    );

    // The session lists the imported photos (Internal originals under Capture/).
    let sess = Session::open(Path::new(&r.session_dir)).unwrap();
    let images = sess.images().unwrap();
    assert_eq!(images.len(), 4);
    assert!(
        images
            .iter()
            .all(|i| i.rel_path.as_str().starts_with("Capture/"))
    );
    assert!(!sess.is_external());
    drop(sess);

    // The card is byte-identical.
    assert_eq!(listing(&card), before);

    // Same device as the "card": ds-import never trusts such a copy for dedupe
    // (the exFAT image test checks the real dedupe path).
    let s2 = scan(&card, Some(&index));
    assert!(
        s2.summary()
            .unwrap()
            .candidates
            .iter()
            .all(|c| !c.already_imported)
    );
    assert_eq!(listing(&card), before);
}

#[test]
fn rename_subfolder_backup_and_auto_apply() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    make_card(&card, 2);
    let s = scan(&card, None);
    let mut o = options(&lib, "Named", &index);
    o.rename = Some("Trip_{seq:3}".into());
    o.subfolder = Some("day".into());
    o.backup_dest = Some(t.path().join("backup").to_string_lossy().into_owned());
    o.verify_source_reread = true;
    o.auto_apply = Some(AutoApplyPreset {
        name: "Warm".into(),
        adjustments_json: crate::session::default_adjustments_json().unwrap(),
        groups: vec![crate::clipboard::AdjustGroup::WhiteBalance],
        relative: vec![],
    });
    let r = import(&s, o, None, CancelToken::new()).unwrap();
    assert!(r.is_complete, "{r:?}");
    assert!(
        r.copied
            .iter()
            .any(|c| c.dest.ends_with("Capture/day/Trip_001.ARW")),
        "{:?}",
        r.copied
    );
    // The RAW+JPG pair shares one new stem, whatever its sequence number.
    let jpg = r
        .copied
        .iter()
        .find(|c| c.src.ends_with("DSC00100.JPG"))
        .unwrap();
    let raw = r
        .copied
        .iter()
        .find(|c| c.src.ends_with("DSC00100.ARW"))
        .unwrap();
    assert_eq!(
        Path::new(&jpg.dest).with_extension(""),
        Path::new(&raw.dest).with_extension("")
    );
    assert!(
        Path::new(&jpg.dest)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Trip_")
    );
    assert!(r.copied.iter().all(|c| c.backup.is_some()));
    assert!(t.path().join("backup/day/Trip_001.ARW").is_file());
    let sess = Session::open(Path::new(&r.session_dir)).unwrap();
    assert_eq!(
        sess.auto_apply_preset().map(|p| p.name.as_str()),
        Some("Warm")
    );
}

#[test]
fn a_metadata_preset_is_set_on_imported_photos_not_clips() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    make_card(&card, 2);
    // Review M4: the XMP sidecar is read beside the card's file, so it counts
    // whether or not its copy has landed when the photo is registered.
    fs::write(
        card.join("DCIM/100MSDCF/DSC00101.xmp"),
        "<x:xmpmeta><dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">Reef</rdf:li></rdf:Alt>\
         </dc:title><dc:creator><rdf:Seq><rdf:li>Card Owner</rdf:li></rdf:Seq></dc:creator></x:xmpmeta>",
    )
    .unwrap();
    let s = scan(&card, None);
    let mut o = options(&lib, "Credited", &index);
    o.metadata_preset = Some(crate::session::metadata::PhotoMetadata {
        creator: "Alexa".into(),
        copyright: "© 2026 Alexa".into(),
        ..Default::default()
    });
    let r = import(&s, o, None, CancelToken::new()).unwrap();
    assert!(r.is_complete, "{r:?}");
    let sess = Session::open(Path::new(&r.session_dir)).unwrap();
    let mut photos = 0;
    for img in sess.images().unwrap() {
        let v = sess.variant(sess.variants(img.id).unwrap()[0]).unwrap();
        if img.rel_path.as_str().contains("C0001") {
            assert!(v.iptc.is_empty(), "clips get no preset");
        } else {
            photos += 1;
            assert_eq!(
                (v.iptc.creator.as_str(), v.iptc.copyright.as_str()),
                ("Alexa", "© 2026 Alexa"),
                "the preset wins over the file"
            );
            let reef = img.rel_path.as_str().contains("101");
            assert_eq!(v.iptc.title, if reef { "Reef" } else { "" });
        }
    }
    assert!(photos >= 2);
}

#[test]
fn import_reads_the_files_own_iptc_as_defaults() {
    let t = tempfile::tempdir().unwrap();
    let jpg = t.path().join("A.JPG");
    fs::write(
        &jpg,
        b"\xFF\xD8 <x:xmpmeta><rdf:Description dc:x=\"1\"><dc:title><rdf:Alt>\
          <rdf:li xml:lang=\"x-default\">Pier</rdf:li></rdf:Alt></dc:title>\
          </rdf:Description></x:xmpmeta>",
    )
    .unwrap();
    let (_, iptc) = meta_for(&jpg, &jpg, FileKind::Jpeg, &Listings::default());
    assert_eq!(iptc.title, "Pier");
    let (_, clip) = meta_for(&jpg, &jpg, FileKind::Video, &Listings::default());
    assert!(clip.is_empty());
}

#[test]
fn cancel_leaves_a_resumable_journal_and_resume_finishes() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    make_card(&card, 4);
    let before = listing(&card);
    let s = scan(&card, None);
    let mut o = options(&lib, "Slow", &index);
    o.throttle_ms = 40;
    let token = CancelToken::new();
    let t2 = token.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        t2.cancel();
    });
    let r = import(&s, o.clone(), None, token).unwrap();
    canceller.join().unwrap();
    assert!(
        r.cancelled && !r.is_complete && r.eject_needs_confirmation,
        "{r:?}"
    );
    let pending = pending_imports(vec![lib.to_string_lossy().into_owned()], vec![]).unwrap();
    assert_eq!(
        pending[0].volumes.len(),
        1,
        "the card that resumes it: {pending:?}"
    );
    // Listed once whether found under a root or named as a session.
    assert_eq!(
        pending_imports(
            vec![lib.to_string_lossy().into_owned()],
            vec![o.session_dir.clone()]
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].session_dir, o.session_dir);
    assert!(pending[0].pending > 0);
    assert_eq!(
        pending_import(o.session_dir.clone()).unwrap(),
        Some(pending[0].clone())
    );

    // Resume: same card, same session, not created again (the app may still
    // say "new session": a folder that already is one is reopened).
    o.throttle_ms = 0;
    let r2 = import(&s, o.clone(), None, CancelToken::new()).unwrap();
    assert!(r2.is_complete, "{r2:?}");
    let sess = Session::open(Path::new(&o.session_dir)).unwrap();
    assert_eq!(sess.images().unwrap().len(), 5, "4 RAWs + clip, each once");
    assert!(pending_import(o.session_dir.clone()).unwrap().is_none());
    assert_eq!(listing(&card), before);
}

#[test]
fn discard_removes_only_the_journal() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    make_card(&card, 3);
    let s = scan(&card, None);
    let mut o = options(&lib, "Half", &index);
    o.throttle_ms = 40;
    let token = CancelToken::new();
    let t2 = token.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        t2.cancel();
    });
    let r = import(&s, o.clone(), None, token).unwrap();
    canceller.join().unwrap();
    assert!(r.cancelled);
    let copied: Vec<PathBuf> = r.copied.iter().map(|c| PathBuf::from(&c.dest)).collect();
    discard_pending_import(o.session_dir.clone(), None).unwrap();
    assert!(pending_import(o.session_dir.clone()).unwrap().is_none());
    assert!(
        copied.iter().all(|p| p.is_file()),
        "finished copies are never removed"
    );
    assert_eq!(
        pending_imports(
            vec![t.path().join("nope").to_string_lossy().into_owned()],
            vec![]
        )
        .unwrap()
        .len(),
        0
    );
}

#[test]
fn a_folder_opened_in_place_is_not_an_import_destination() {
    let t = tempfile::tempdir().unwrap();
    let (card, index) = (t.path().join("card"), t.path().join("g.db"));
    make_card(&card, 1);
    let folder = t.path().join("Photos");
    fs::create_dir_all(&folder).unwrap();
    let store = t.path().join("store");
    Session::create_external(&store, "Photos", &folder).unwrap();
    let s = scan(&card, None);
    let mut o = options(t.path(), "store", &index);
    o.create_session = false;
    let e = import(&s, o, None, CancelToken::new()).unwrap_err();
    assert!(matches!(e, DsError::Unsupported { .. }), "{e:?}");
    // A new session over an existing, non-empty folder is refused too.
    let mut o = options(t.path(), "card", &index);
    o.create_session = true;
    assert!(import(&s, o, None, CancelToken::new()).is_err());
}

#[test]
fn bad_templates_and_busy_errors_map_to_ds_errors() {
    let t = tempfile::tempdir().unwrap();
    let (card, lib, index) = (
        t.path().join("card"),
        t.path().join("lib"),
        t.path().join("g.db"),
    );
    make_card(&card, 1);
    let s = scan(&card, None);
    let mut o = options(&lib, "Bad", &index);
    o.rename = Some("{nope}".into());
    assert!(matches!(
        import(&s, o, None, CancelToken::new()).unwrap_err(),
        DsError::InvalidArgument { .. }
    ));
    assert!(matches!(
        DsError::from(ImportError::Busy(PathBuf::from("/x"))),
        DsError::Unsupported { .. }
    ));
    assert!(matches!(
        DsError::from(ImportError::JournalVersion {
            path: PathBuf::from("/x"),
            version: 9
        }),
        DsError::Unsupported { .. }
    ));
    assert!(matches!(
        DsError::from(ImportError::Journal("bad".into())),
        DsError::Io { .. }
    ));
    assert!(
        block_on(scan_card(
            "/definitely/not/here".into(),
            None,
            CancelToken::new()
        ))
        .is_err()
    );
}

#[test]
fn templates_render_like_import() {
    // 2026-08-06 09:30:05 camera clock (as UTC).
    let ms = 1_786_008_605_000;
    assert_eq!(
        render_import_template(
            "{date}_{seq:4}".into(),
            "DSC01234.ARW".into(),
            Some(ms),
            None,
            7,
            false
        )
        .unwrap(),
        "20260806_0007.ARW"
    );
    assert_eq!(
        render_import_template(
            "{camera}-{name}".into(),
            "DSC01234.ARW".into(),
            None,
            Some("ILCE-9".into()),
            1,
            false
        )
        .unwrap(),
        "ILCE-9-DSC01234.ARW"
    );
    assert_eq!(
        render_import_template(
            "{date:%Y}/{date:%m-%d}".into(),
            "a.ARW".into(),
            Some(ms),
            None,
            1,
            true
        )
        .unwrap(),
        "2026/08-06"
    );
    assert!(matches!(
        render_import_template("{oops".into(), "a.ARW".into(), None, None, 1, false),
        Err(DsError::InvalidArgument { .. })
    ));
    assert!(render_import_template("..".into(), "a.ARW".into(), None, None, 1, true).is_err());
}

#[test]
fn global_dedupe_matches_by_size_then_partial_hash() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("S");
    fs::create_dir_all(root.join("Capture")).unwrap();
    fs::write(root.join("Capture/a.ARW"), vec![1u8; 300_000]).unwrap();
    fs::write(root.join("Capture/b.ARW"), vec![2u8; 300_000]).unwrap();
    let h = FileHash([3; 32]);
    let d = GlobalDedupe::from_entries(vec![
        (root.clone(), "Capture/a.ARW".into(), h),
        (root.clone(), "Capture/b.ARW".into(), h),
        (root.clone(), "Capture/gone.ARW".into(), h),
    ]);
    assert!(d.has_size(300_000) && !d.has_size(1));
    let card = t.path().join("x.ARW");
    fs::write(&card, vec![1u8; 300_000]).unwrap();
    let p = partial_hash(&card).unwrap();
    let m = d.matches(300_000, &p);
    assert_eq!(m.len(), 1);
    assert_eq!(
        m[0].path.as_deref(),
        Some(root.join("Capture/a.ARW").as_path())
    );
    assert_eq!(d.matches(5, &p).len(), 0);
    // Cached: a second query doesn't need the file.
    assert_eq!(d.matches(300_000, &p).len(), 1);
    // An index that can't be opened is an error; the scan treats it as "no marks".
    assert!(GlobalDedupe::open(t.path()).is_err());
}

/// Live path: a real exFAT "card" (disk image), real sample ARWs and an MP4,
/// imported into a new session. The card is byte-identical afterwards.
#[test]
fn exfat_card_image_imports_and_is_never_written() {
    let t = tempfile::tempdir().unwrap();
    let Some(mut img) =
        ds_fsutil::testvol::require(t.path(), ds_fsutil::testvol::Fs::ExFat, 256, "IMPCARD")
    else {
        return;
    };
    let card = img.mount_point().to_path_buf();
    make_card(&card, 3);
    // Flush the copies onto the image, then remount so we read what a card holds.
    img.detach(false).unwrap();
    img.attach(&t.path().join("IMPCARD-mnt")).unwrap();
    let card = img.mount_point().to_path_buf();
    let before = listing(&card);
    let (lib, index) = (t.path().join("lib"), t.path().join("g.db"));
    let s = scan(&card, Some(&index));
    let started = std::time::Instant::now();
    let r = import(&s, options(&lib, "Card", &index), None, CancelToken::new()).unwrap();
    let secs = started.elapsed().as_secs_f64();
    assert!(r.is_complete, "{r:?}");
    assert_eq!(r.registered, 4);
    eprintln!(
        "IMPORT exFAT image: {} files, {:.1} MB in {:.2} s = {:.1} MB/s",
        r.copied.len(),
        r.bytes_copied as f64 / 1e6,
        secs,
        r.bytes_copied as f64 / 1e6 / secs
    );
    assert_eq!(listing(&card), before, "the card is byte-identical");

    // Rescan: everything is in the library now (global index dedupe), and an
    // import that includes one anyway skips it as a verified duplicate.
    let s2 = scan(&card, Some(&index));
    let sum2 = s2.summary().unwrap();
    assert!(
        sum2.candidates.iter().all(|c| c.already_imported),
        "{:?}",
        sum2.candidates
    );
    let mut o = options(&lib, "Card again", &index);
    o.include = Some(vec![sum2.candidates[0].path.clone()]);
    let r2 = import(&s2, o, None, CancelToken::new()).unwrap();
    assert_eq!(r2.skipped_duplicates.len(), 1, "{r2:?}");
    assert_eq!(r2.registered, 0);
    assert_eq!(listing(&card), before, "still byte-identical");
    img.detach(false).unwrap();
}

#[test]
fn card_file_meta_of_an_unreadable_or_damaged_file_is_empty() {
    let t = tempfile::tempdir().unwrap();
    let junk = t.path().join("junk.ARW");
    fs::write(&junk, b"not a raw file at all").unwrap();
    for p in [junk, t.path().join("missing.ARW")] {
        let m = card_file_meta(p.to_string_lossy().into_owned());
        assert_eq!((m.camera, m.capture_ms), (None, None));
    }
}
