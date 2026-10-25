//! External-originals sessions ("open folder as session"), batch journal,
//! and the v1→v2 sidecar migration.
#![allow(clippy::too_many_lines)]

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ds_core::geom::Orientation;
use ds_core::{ColorTag, FileHash, PickState, Rating};

use crate::atomic::{WriteHook, WriteStage};
use crate::*;

fn meta() -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: Some(1_700_000_000_000),
        camera: Some("ILCE-9".into()),
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    }
}

fn hash(b: &[u8]) -> FileHash {
    FileHash(*blake3::hash(b).as_bytes())
}

/// Every file under `dir` with its bytes (to prove the folder is untouched).
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(dir).unwrap().to_owned(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

#[derive(Debug, Default)]
struct Background {
    kept: Vec<(ImageId, identity::Verdict)>,
    /// (old image, the new "file changed" image).
    replaced: Vec<(ImageId, ImageId)>,
    unreadable: Vec<ImageId>,
    relink: RelinkReport,
    registered: Vec<ImageId>,
}

struct Ext {
    _t: tempfile::TempDir,
    folder: PathBuf,
    root: PathBuf,
    s: Session,
}

fn ext(files: &[(&str, &[u8])]) -> Ext {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    for (rel, b) in files {
        let p = folder.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }
    fs::create_dir_all(&folder).unwrap();
    let root = t.path().join("store/session");
    let s = Session::create_external(&root, "Shoot", &folder).unwrap();
    Ext {
        _t: t,
        folder,
        root,
        s,
    }
}

impl Ext {
    /// Register the untracked files by stat (lazy hash), like the FFI does.
    fn register(&mut self, rels: &[String]) -> Vec<ImageId> {
        let items = rels.iter().map(|r| (r.clone(), UNHASHED, meta())).collect();
        self.s.add_originals(items).unwrap()
    }

    fn hash_all(&mut self) {
        for (id, path) in self.s.unhashed_images().unwrap() {
            let st = original_stat(&path).unwrap();
            let h = hash(&fs::read(&path).unwrap());
            assert!(self.s.set_verified_hash(id, h, st).unwrap());
        }
    }

    /// What the app's background pass does after a sync (see ds-ffi
    /// `hash_pending`): verify changed files, relink held ones by content,
    /// register the rest. `ranges` stands in for `ds_raw::image_data_ranges`.
    fn background(&mut self, sync: &ExternalSync, ranges: Option<&[(u64, u64)]>) -> Background {
        let mut out = Background::default();
        let never = || false;
        let mut full = Vec::new();
        let mut samples = Vec::new();
        for v in &sync.verify {
            if v.sample_only {
                if let Ok(s) = identity::sample_hash(&v.path) {
                    samples.push((v.image, s));
                }
            } else {
                full.push(v.clone());
            }
        }
        let differ = self.s.check_samples(&samples).unwrap();
        full.extend(
            sync.verify
                .iter()
                .filter(|v| differ.contains(&v.image))
                .cloned(),
        );
        let mut accept = Vec::new();
        for v in full {
            let Ok(h) = identity::hash_original(&v.path, ranges, &never) else {
                out.unreadable.push(v.image);
                continue;
            };
            let info = self.s.image(v.image).unwrap();
            match identity::verdict(info.hash, info.content_hash, &h) {
                identity::Verdict::Changed => {
                    let new = self
                        .s
                        .replace_image(v.image, v.stat, Some(h), meta())
                        .unwrap();
                    out.replaced.push((v.image, new.unwrap()));
                }
                verdict => {
                    out.kept.push((v.image, verdict));
                    accept.push(Verified {
                        image: v.image,
                        stat: v.stat,
                        hashes: h,
                    });
                }
            }
        }
        self.s.accept_verified(&accept).unwrap();
        let found: Vec<Found> = sync
            .held
            .iter()
            .filter_map(|rel| {
                let p = self.s.original_path(rel);
                Some(Found {
                    rel: rel.clone(),
                    stat: original_stat(&p)?,
                    hashes: identity::hash_original(&p, ranges, &never).ok()?,
                })
            })
            .collect();
        out.relink = self.s.relink_found(&found).unwrap();
        let mut fresh = sync.untracked.clone();
        fresh.extend(out.relink.unmatched.iter().cloned());
        out.registered = self.register(&fresh);
        out
    }

    fn reopen(&mut self) {
        self.s = Session::open(&self.root).unwrap();
    }

    fn variant(&self, id: ImageId) -> VariantId {
        self.s.variants(id).unwrap()[0]
    }
}

#[test]
fn external_session_reads_the_folder_and_never_writes_to_it() {
    let mut e = ext(&[("a.ARW", b"aaaa"), ("DCIM/100/b.ARW", b"bbbb")]);
    let before = snapshot(&e.folder);
    assert!(e.s.is_external());
    assert_eq!(e.s.external_root(), Some(e.folder.as_path()));
    let sync = e.s.sync_external().unwrap();
    assert_eq!(
        sync.untracked,
        vec!["Capture/DCIM/100/b.ARW", "Capture/a.ARW"]
    );
    let ids = e.register(&sync.untracked);
    assert_eq!(ids.len(), 2);
    assert_eq!(
        e.s.unhashed_images().unwrap().len(),
        2,
        "registered by stat, hashed later"
    );
    let v = e.variant(ids[1]);
    e.s.set_rating(&[v], Rating::new(4).unwrap()).unwrap();
    e.s.set_color_tag(&[v], ColorTag::Orange).unwrap();
    e.hash_all();
    assert_eq!(
        e.s.unhashed_images().unwrap(),
        [] as [(session::ImageId, std::path::PathBuf); 0]
    );
    assert_eq!(e.s.image(ids[1]).unwrap().hash, hash(b"aaaa"));
    assert_eq!(
        e.s.original_path(e.s.image(ids[1]).unwrap().rel_path.as_str()),
        e.folder.join("a.ARW")
    );
    assert_eq!(
        snapshot(&e.folder),
        before,
        "no byte of the user's folder changed"
    );
    assert!(!e.folder.join(".ds").exists());
    e.reopen();
    let v2 = e.s.variant(v).unwrap();
    assert_eq!(
        (v2.rating.stars(), v2.color_tag),
        (4, ColorTag::Orange),
        "persists"
    );
    let again = e.s.sync_external().unwrap();
    assert!(again.untracked.is_empty() && again.missing.is_empty() && again.verify.is_empty());
    assert_eq!(again.held, [] as [std::string::String; 0]);
}

struct NoTrash;
impl Trasher for NoTrash {
    fn trash(&self, _: &[PathBuf]) -> std::result::Result<(), String> {
        panic!("must not be called")
    }
}

#[test]
fn external_sessions_refuse_to_move_or_trash_originals() {
    let mut e = ext(&[("a.ARW", b"aaaa")]);
    let ids = e.register(&["Capture/a.ARW".to_owned()]);
    let v = e.variant(ids[0]);
    assert!(matches!(
        e.s.move_to(&[v], Folder::Trash),
        Err(SessionError::ExternalOriginals(_))
    ));
    assert!(matches!(
        e.s.empty_trash(&NoTrash),
        Err(SessionError::ExternalOriginals(_))
    ));
    assert!(matches!(
        e.s.add_originals(vec![("Selects/a.ARW".into(), UNHASHED, meta())]),
        Err(SessionError::ExternalOriginals(_))
    ));
    assert!(e.folder.join("a.ARW").exists());
    // Internal sessions can't use external-only operations.
    let t = tempfile::tempdir().unwrap();
    let mut internal = Session::create(&t.path().join("s"), "s").unwrap();
    assert!(matches!(
        internal.sync_external(),
        Err(SessionError::ExternalOnly(_))
    ));
    assert!(matches!(
        internal.set_external_root(t.path()),
        Err(SessionError::ExternalOnly(_))
    ));
    assert!(!internal.is_external() && internal.external_root().is_none());
    assert_eq!(internal.originals(), &Originals::Internal);
}

#[test]
fn finder_rename_delete_and_replace_are_handled() {
    let mut e = ext(&[
        ("a.ARW", b"aaaa"),
        ("b.ARW", b"bbbbb"),
        ("c.ARW", b"cccccc"),
    ]);
    let sync = e.s.sync_external().unwrap();
    let ids = e.register(&sync.untracked);
    e.hash_all();
    for id in &ids {
        let v = e.variant(*id);
        e.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    }
    // a: renamed in Finder; b: deleted; c: replaced by a different photo.
    fs::rename(e.folder.join("a.ARW"), e.folder.join("Keeper.ARW")).unwrap();
    fs::remove_file(e.folder.join("b.ARW")).unwrap();
    fs::remove_file(e.folder.join("c.ARW")).unwrap();
    fs::write(e.folder.join("c.ARW"), b"different!").unwrap();
    let r = e.s.sync_external().unwrap();
    assert_eq!(r.relinked.len(), 1, "{r:?}");
    assert_eq!(r.relinked[0].to.as_str(), "Capture/Keeper.ARW");
    assert_eq!(
        r.missing,
        vec![ids[1]],
        "b is missing, still in the session"
    );
    // c's stat changed: kept provisionally, checked in the background (nothing hashed on open).
    assert_eq!(r.verify.len(), 1);
    assert_eq!(
        (r.verify[0].image, r.verify[0].sample_only),
        (ids[2], false)
    );
    assert!(r.untracked.is_empty() && r.held.is_empty(), "{r:?}");
    assert_eq!(e.s.variant(e.variant(ids[2])).unwrap().rating.stars(), 5);
    let bg = e.background(&r, None);
    assert_eq!(bg.replaced.len(), 1, "c is a different photo: detached");
    let (old, new) = bg.replaced[0];
    assert_eq!(old, ids[2]);
    // The renamed photo kept its rating; the replaced one's metadata is set aside.
    let keeper = e.variant(ids[0]);
    assert_eq!(e.s.variant(keeper).unwrap().rating.stars(), 5);
    assert!(e.s.image(ids[2]).is_err());
    let fresh = e.s.image(new).unwrap();
    assert!(fresh.file_changed, "flagged");
    assert_eq!(fresh.hash, hash(b"different!"));
    assert_eq!(
        e.s.variant(e.variant(new)).unwrap().rating.stars(),
        0,
        "no inheritance"
    );
    // The flag survives a relaunch (it lives in the sidecar).
    e.reopen();
    assert!(e.s.image(new).unwrap().file_changed);
    e.s.rebuild_index().unwrap();
    assert!(e.s.image(new).unwrap().file_changed, "and an index rebuild");
    // Restore: the previous photo's ids, rating and edits describe this file now.
    let v_new = e.variant(new);
    e.s.set_rating(&[v_new], Rating::new(1).unwrap()).unwrap();
    let back = e.s.restore_replaced(new).unwrap();
    assert_eq!(back, ids[2]);
    let v_old = e.variant(back);
    let info = e.s.variant(v_old).unwrap();
    assert_eq!(info.rating.stars(), 5);
    let img = e.s.image(back).unwrap();
    assert!(!img.file_changed && !img.missing);
    assert_eq!(img.hash, hash(b"different!"), "identity is the file now");
    assert!(
        e.s.image(new).is_err(),
        "the fresh metadata went to the Trash"
    );
    assert!(
        e.root
            .join(format!("Trash/.ds/deleted/{}.json", v_new.0))
            .is_file()
    );
    // Nothing to restore any more; and after a reopen it stays restored (verified stat).
    assert!(matches!(
        e.s.restore_replaced(back),
        Err(SessionError::NotFound { .. })
    ));
    e.reopen();
    let again = e.s.sync_external().unwrap();
    // The index rebuild above forgot the verified stats: Keeper is re-checked (same
    // bytes, nothing changes); the restored photo was verified by the restore.
    assert_eq!(
        again.verify.iter().map(|v| v.image).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    let bg = e.background(&again, None);
    assert_eq!(bg.kept, vec![(ids[0], identity::Verdict::Same)]);
    assert_eq!(bg.replaced, [] as [(session::ImageId, session::ImageId); 0]);
    assert_eq!(
        e.s.sync_external().unwrap().verify,
        [] as [external::ToVerify; 0]
    );
    assert!(e.s.image(ids[1]).unwrap().missing);
    // Remove the missing photo from the session: sidecar kept recoverably, nothing else touched.
    assert_eq!(e.s.forget_images(&[ids[1], ImageId(12_345)]).unwrap(), 1);
    assert!(e.s.image(ids[1]).is_err());
    assert_eq!(e.s.forget_images(&[]).unwrap(), 0);
    assert!(e.root.join("Trash/.ds/deleted").read_dir().unwrap().count() >= 2);
}

#[test]
fn verified_hash_is_refused_when_the_file_changed_meanwhile() {
    let mut e = ext(&[("a.ARW", b"aaaa")]);
    let ids = e.register(&["Capture/a.ARW".to_owned()]);
    let path = e.folder.join("a.ARW");
    let st = original_stat(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    fs::write(&path, b"aaaa-edited").unwrap();
    assert!(!e.s.set_verified_hash(ids[0], hash(b"aaaa"), st).unwrap());
    assert_eq!(e.s.image(ids[0]).unwrap().hash, UNHASHED);
    assert!(original_stat(&e.folder.join("nope")).is_none());
}

#[test]
fn folder_moved_while_open_is_followed() {
    let mut e = ext(&[("a.ARW", b"aaaa")]);
    let ids = e.register(&["Capture/a.ARW".to_owned()]);
    let v = e.variant(ids[0]);
    let moved = e.folder.with_file_name("Shoot (renamed)");
    fs::rename(&e.folder, &moved).unwrap();
    e.s.set_external_root(&moved).unwrap();
    e.s.set_rating(&[v], Rating::new(3).unwrap()).unwrap();
    e.reopen();
    assert_eq!(e.s.external_root(), Some(moved.as_path()));
    assert_eq!(e.s.variant(v).unwrap().rating.stars(), 3);
    let r = e.s.sync_external().unwrap();
    assert!(r.missing.is_empty() && r.untracked.is_empty(), "{r:?}");
}

#[test]
fn external_walk_skips_hidden_bundles_and_respects_depth() {
    let deep = "d1/d2/d3/d4/d5/d6/d7/d8/d9/deep.ARW";
    let e = ext(&[
        ("a.ARW", b"a"),
        (".hidden/x.ARW", b"x"),
        ("Lib.photoslibrary/y.ARW", b"y"),
        ("iPhoto Library.photolibrary/Masters/w.ARW", b"w"),
        (".ds/session/z.ARW", b"z"),
        ("d1/d2/ok.ARW", b"o"),
        (deep, b"d"),
    ]);
    let (files, seen) = e.s.external_files().unwrap();
    let names: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(names, vec!["Capture/a.ARW", "Capture/d1/d2/ok.ARW"]);
    assert_eq!(seen, 2);
    assert_eq!(files[0].1, 1, "size");
    let t = tempfile::tempdir().unwrap();
    let internal = Session::create(&t.path().join("s"), "s").unwrap();
    assert_eq!(internal.external_files().unwrap(), (Vec::new(), 0));
}

#[test]
fn create_external_validates() {
    let t = tempfile::tempdir().unwrap();
    assert!(matches!(
        Session::create_external(&t.path().join("s"), "s", &t.path().join("nope")),
        Err(SessionError::OriginalMissing(_))
    ));
    fs::write(t.path().join("f"), b"x").unwrap();
    assert!(matches!(
        Session::create_external(t.path(), "s", t.path()),
        Err(SessionError::AlreadyExists(_))
    ));
    assert!(matches!(
        Session::create_external(&t.path().join("s2"), "", t.path()),
        Err(SessionError::InvalidName(_))
    ));
}

#[test]
fn hard_link_sessions_migrate_to_external() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    fs::create_dir_all(folder.join("DCIM")).unwrap();
    for (n, b) in [
        ("a.ARW", &b"aaaa"[..]),
        ("DCIM/b.ARW", b"bbbb"),
        ("gone.ARW", b"gg"),
    ] {
        fs::write(folder.join(n), b).unwrap();
    }
    // The old layout: <folder>/.ds/session with Capture/ hard links.
    let root = folder.join(".ds/session");
    let mut s = Session::create(&root, "Shoot").unwrap();
    let mut ids = Vec::new();
    for n in ["a.ARW", "DCIM/b.ARW", "gone.ARW"] {
        let link = root.join("Capture").join(n);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::hard_link(folder.join(n), &link).unwrap();
        let b = fs::read(&link).unwrap();
        ids.push(
            s.add_original(&format!("Capture/{n}"), hash(&b), meta())
                .unwrap(),
        );
    }
    let v = s.variants(ids[0]).unwrap()[0];
    s.set_rating(&[v], Rating::new(4).unwrap()).unwrap();
    s.set_pick(&[v], PickState::Picked).unwrap();
    drop(s);
    // The user deleted gone.ARW in Finder: the link holds the only copy; keep it.
    fs::remove_file(folder.join("gone.ARW")).unwrap();
    let m = Session::migrate_to_external(&root, &folder).unwrap();
    assert!(m.converted);
    assert_eq!(m.links_removed, 2);
    assert_eq!(m.kept, vec!["gone.ARW"]);
    assert!(!root.join("Capture/a.ARW").exists() && root.join("Capture/gone.ARW").exists());
    assert_eq!(
        fs::metadata(folder.join("a.ARW")).unwrap().nlink(),
        1,
        "space freed"
    );
    assert_eq!(
        fs::read(folder.join("a.ARW")).unwrap(),
        b"aaaa",
        "originals untouched"
    );
    let mut s = Session::open(&root).unwrap();
    assert_eq!(s.external_root(), Some(folder.as_path()));
    let vi = s.variant(v).unwrap();
    assert_eq!(
        (vi.rating.stars(), vi.pick),
        (4, PickState::Picked),
        "ids and metadata kept"
    );
    let r = s.sync_external().unwrap();
    assert_eq!(r.missing, vec![ids[2]]);
    assert!(r.relinked.is_empty() && r.verify.is_empty(), "{r:?}");
    drop(s);
    assert_eq!(
        Session::migrate_to_external(&root, &folder).unwrap(),
        Migrated::default()
    );
}

#[test]
fn a_crashed_batch_is_rolled_forward_on_open() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let mut s = Session::create(&root, "s").unwrap();
    let mut vs = Vec::new();
    for i in 0..6 {
        let rel = format!("Capture/{i}.ARW");
        let p = root.join(&rel);
        fs::write(&p, format!("{i}")).unwrap();
        let id = s
            .add_original(&rel, hash(format!("{i}").as_bytes()), meta())
            .unwrap();
        vs.push(s.variants(id).unwrap()[0]);
    }
    // "Crash" (the process dies: nothing runs after) once 2 of 6 sidecars
    // are renamed into place.
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let hook: WriteHook = Arc::new(move |_: &Path, _: WriteStage| {
        assert!(n2.fetch_add(1, Ordering::SeqCst) < 2, "power cut");
        Ok(())
    });
    s.set_write_hook(Some(hook));
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = s.set_rating(&vs, Rating::new(5).unwrap());
    }));
    assert!(crashed.is_err());
    assert_eq!(root.join(".ds/journal").read_dir().unwrap().count(), 1);
    assert_eq!(rating_on_disk(&root, 1), Some(5));
    assert_eq!(rating_on_disk(&root, 2), Some(0));
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 1);
    assert!(
        vs.iter()
            .all(|v| s.variant(*v).unwrap().rating.stars() == 5),
        "the whole batch applied"
    );
    assert_eq!(root.join(".ds/journal").read_dir().unwrap().count(), 0);
    // A torn journal (never completed) is discarded without touching sidecars.
    fs::write(root.join(".ds/journal/9.json"), b"[{\"rel\":").unwrap();
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 0);
    assert_eq!(root.join(".ds/journal").read_dir().unwrap().count(), 0);
    assert_eq!(s.variant(vs[0]).unwrap().rating.stars(), 5);
    // Successful batches leave no journal behind.
    let mut s = s;
    s.set_rating(&vs, Rating::new(1).unwrap()).unwrap();
    assert_eq!(root.join(".ds/journal").read_dir().unwrap().count(), 0);
}

/// The star rating stored in `Capture/<i>.ARW`'s first sidecar.
fn rating_on_disk(root: &Path, i: usize) -> Option<u64> {
    let p = root.join(format!("Capture/.ds/settings/{i}.ARW.json"));
    let v: serde_json::Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
    v.pointer("/rating").and_then(serde_json::Value::as_u64)
}

/// A session with `n` originals in `Capture/` and their first variants.
fn rated_session(root: &Path, n: usize) -> (Session, Vec<VariantId>) {
    let mut s = Session::create(root, "s").unwrap();
    let mut vs = Vec::new();
    for i in 0..n {
        let rel = format!("Capture/{i}.ARW");
        fs::write(root.join(&rel), format!("{i}")).unwrap();
        let id = s
            .add_original(&rel, hash(format!("{i}").as_bytes()), meta())
            .unwrap();
        vs.push(s.variants(id).unwrap()[0]);
    }
    (s, vs)
}

#[test]
fn a_failed_batch_never_overwrites_a_later_edit() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let (mut s, vs) = rated_session(&root, 4);
    // The batch fails on its last two sidecars (disk full, say)...
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    s.set_write_hook(Some(Arc::new(move |_: &Path, _: WriteStage| {
        if n2.fetch_add(1, Ordering::SeqCst) >= 2 {
            Err(io::Error::other("disk full"))
        } else {
            Ok(())
        }
    })));
    assert!(s.set_rating(&vs, Rating::new(5).unwrap()).is_err());
    // ...is resolved in the running session (its outcome is reported and
    // indexed), so no journal is left for a later open to replay...
    assert_eq!(root.join(".ds/journal").read_dir().unwrap().count(), 0);
    s.set_write_hook(None);
    // ...and later single edits, to a written and to an unwritten sidecar, stick.
    s.set_rating(&vs[..1], Rating::new(2).unwrap()).unwrap();
    s.set_rating(&vs[3..], Rating::new(3).unwrap()).unwrap();
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 0);
    let stars: Vec<u8> = vs
        .iter()
        .map(|v| s.variant(*v).unwrap().rating.stars())
        .collect();
    assert_eq!(stars, [2, 5, 0, 3]);
}

#[test]
fn replay_never_puts_back_content_older_than_the_disk() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let (mut s, vs) = rated_session(&root, 2);
    s.set_rating(&vs, Rating::new(4).unwrap()).unwrap();
    let four: Vec<_> = (0..2)
        .map(|i| {
            let p = root.join(format!("Capture/.ds/settings/{i}.ARW.json"));
            (p.clone(), fs::read(p).unwrap())
        })
        .collect();
    s.set_rating(&vs, Rating::new(1).unwrap()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    // A journal left behind (by an older build) for a batch that failed...
    let j = crate::journal::begin(&root, &four).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    // ...then a newer single edit of one of its sidecars.
    s.set_rating(&vs[..1], Rating::new(2).unwrap()).unwrap();
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 1);
    assert_eq!(
        s.variant(vs[0]).unwrap().rating.stars(),
        2,
        "newer edit kept"
    );
    assert_eq!(
        s.variant(vs[1]).unwrap().rating.stars(),
        4,
        "older sidecar rolled forward"
    );
    assert!(!j.exists());
}

#[test]
fn leftover_journals_replay_oldest_first() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let (mut s, vs) = rated_session(&root, 2);
    let contents = |s: &mut Session, stars: u8| -> Vec<(PathBuf, Vec<u8>)> {
        s.set_rating(&vs, Rating::new(stars).unwrap()).unwrap();
        (0..2)
            .map(|i| {
                let p = root.join(format!("Capture/.ds/settings/{i}.ARW.json"));
                (p.clone(), fs::read(p).unwrap())
            })
            .collect()
    };
    let (two, three, four) = (
        contents(&mut s, 2),
        contents(&mut s, 3),
        contents(&mut s, 4),
    );
    s.set_rating(&vs, Rating::new(1).unwrap()).unwrap();
    drop(s);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let dir = root.join(".ds/journal");
    fs::create_dir_all(&dir).unwrap();
    // An old-format journal (`<random id>.json`, ordered by its mtime) whose
    // name sorts last, then two new ones, all three left by crashes.
    let entries = |items: &[(PathBuf, Vec<u8>)]| {
        serde_json::json!(
            items
                .iter()
                .map(|(p, b)| serde_json::json!({
                    "rel": p.strip_prefix(&root).unwrap().to_str().unwrap(),
                    "json": String::from_utf8(b.clone()).unwrap(),
                }))
                .collect::<Vec<_>>()
        )
        .to_string()
    };
    fs::write(dir.join("9999999999.json"), entries(&two)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::journal::begin(&root, &three).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::journal::begin(&root, &four).unwrap().unwrap();
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 3);
    assert!(
        vs.iter()
            .all(|v| s.variant(*v).unwrap().rating.stars() == 4)
    );
    assert_eq!(dir.read_dir().unwrap().count(), 0);
}

#[test]
fn unhashed_originals_keep_identity_by_size_and_mtime() {
    let mut e = ext(&[("a.ARW", b"aaaa"), ("b.ARW", b"bbbbbb"), ("c.ARW", b"cc")]);
    let rels: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|n| format!("Capture/{n}.ARW"))
        .collect();
    let ids = e.register(&rels);
    let va = e.variant(ids[0]);
    e.s.set_rating(&[va], Rating::new(4).unwrap()).unwrap();
    // A copy of the folder (new inodes, same size and mtime): same photos.
    let copy = e.folder.with_file_name("Shoot copy");
    fs::create_dir_all(&copy).unwrap();
    for n in ["a", "b", "c"] {
        let (from, to) = (
            e.folder.join(format!("{n}.ARW")),
            copy.join(format!("{n}.ARW")),
        );
        fs::copy(&from, &to).unwrap();
        let mtime = fs::metadata(&from).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&to)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
    e.s.set_external_root(&copy).unwrap();
    let r = e.s.sync_external().unwrap();
    assert!(r.verify.is_empty() && r.missing.is_empty(), "{r:?}");
    assert_eq!(e.s.variant(va).unwrap().rating.stars(), 4);
    assert_eq!(
        e.s.image(ids[0]).unwrap().hash,
        UNHASHED,
        "still to be hashed"
    );
    // A Finder rename keeps the stat: followed without a hash.
    fs::rename(copy.join("a.ARW"), copy.join("a-renamed.ARW")).unwrap();
    // A different size can't be proven the same photo without a hash: checked in
    // the background (nothing detached on open).
    fs::write(copy.join("b.ARW"), b"b").unwrap();
    let r = e.s.sync_external().unwrap();
    assert_eq!(r.relinked.len(), 1, "{r:?}");
    assert_eq!(r.relinked[0].to.as_str(), "Capture/a-renamed.ARW");
    assert_eq!(
        r.verify.iter().map(|v| v.image).collect::<Vec<_>>(),
        vec![ids[1]]
    );
    assert_eq!(r.verify[0].hash, UNHASHED);
    assert_eq!(r.untracked, [] as [std::string::String; 0]);
    assert_eq!(e.s.variant(va).unwrap().rating.stars(), 4);
    // A `touch` of an unhashed original (same size and inode) is the same file.
    let a = copy.join("a-renamed.ARW");
    let later = fs::metadata(&a).unwrap().modified().unwrap() + std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let r = e.s.sync_external().unwrap();
    assert!(!r.verify.iter().any(|v| v.image == ids[0]), "{r:?}");
    // A deleted unhashed original is simply missing (no hash to search by).
    fs::remove_file(copy.join("c.ARW")).unwrap();
    let r = e.s.sync_external().unwrap();
    assert_eq!(r.missing, vec![ids[2]]);
}

// ------------------------------------------------------- round 5: identity ---

/// A fake RAW: 64 bytes of "metadata", then the sensor data.
fn fake_raw(meta_tag: u8, sensor: &[u8]) -> Vec<u8> {
    let mut v = vec![meta_tag; 64];
    v.extend_from_slice(sensor);
    v
}

fn sensor_ranges(len: usize) -> Vec<(u64, u64)> {
    vec![(64, len as u64 - 64)]
}

/// Replace a file the way exiftool does by default: write a temp copy, rename it over.
fn rewrite_via_rename(path: &Path, bytes: &[u8], keep_mtime: bool) {
    let mtime = fs::metadata(path).unwrap().modified().unwrap();
    let tmp = path.with_extension("tmp_exiftool");
    fs::write(&tmp, bytes).unwrap();
    if keep_mtime {
        fs::File::options()
            .write(true)
            .open(&tmp)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
    fs::rename(&tmp, path).unwrap();
}

fn hashed_ext(files: &[(&str, &[u8])]) -> (Ext, Vec<ImageId>) {
    let mut e = ext(files);
    let sync = e.s.sync_external().unwrap();
    let ids = e.register(&sync.untracked);
    // Hash with sensor ranges like the app does.
    let never = || false;
    let items: Vec<Verified> =
        e.s.unhashed_images()
            .unwrap()
            .into_iter()
            .map(|(id, p)| {
                let len = fs::metadata(&p).unwrap().len() as usize;
                Verified {
                    image: id,
                    stat: original_stat(&p).unwrap(),
                    hashes: identity::hash_original(&p, Some(&sensor_ranges(len)), &never).unwrap(),
                }
            })
            .collect();
    assert_eq!(e.s.accept_verified(&items).unwrap().len(), items.len());
    (e, ids)
}

#[test]
fn metadata_rewrite_keeps_culling_and_a_sensor_change_flags_the_file() {
    let sensor_a = vec![1u8; 4096];
    let sensor_b = vec![2u8; 4096];
    let (mut e, ids) = hashed_ext(&[
        ("A.ARW", &fake_raw(0, &sensor_a)),
        ("B.ARW", &fake_raw(0, &sensor_b)),
    ]);
    let (ia, ib) = (ids[0], ids[1]);
    let (va, vb) = (e.variant(ia), e.variant(ib));
    e.s.set_rating(&[va, vb], Rating::new(5).unwrap()).unwrap();
    assert!(e.s.image(ia).unwrap().content_hash.is_some());
    let ranges = sensor_ranges(64 + 4096);
    // exiftool: new inode, metadata bytes differ, mtime kept (-P).
    rewrite_via_rename(&e.folder.join("A.ARW"), &fake_raw(9, &sensor_a), true);
    // A different photo renamed over B.
    rewrite_via_rename(&e.folder.join("B.ARW"), &fake_raw(0, &[7u8; 4096]), false);
    let sync = e.s.sync_external().unwrap();
    assert_eq!(
        sync.verify.len(),
        2,
        "both re-checked, nothing decided on open"
    );
    let bg = e.background(&sync, Some(&ranges));
    assert_eq!(bg.kept, vec![(ia, identity::Verdict::MetadataEdited)]);
    assert_eq!(e.s.variant(va).unwrap().rating.stars(), 5, "culling kept");
    assert_eq!(
        e.s.image(ia).unwrap().hash,
        hash(&fake_raw(9, &sensor_a)),
        "new whole-file hash stored"
    );
    assert_eq!(bg.replaced.len(), 1);
    let (old, new) = bg.replaced[0];
    assert_eq!(old, ib);
    assert!(e.s.image(new).unwrap().file_changed);
    // Both survive a relaunch: A's new identity (no re-check) and B's badge.
    e.reopen();
    let again = e.s.sync_external().unwrap();
    assert!(again.verify.is_empty(), "{again:?}");
    assert!(e.s.image(new).unwrap().file_changed);
    // Keep it as a new photo: the flag goes, the old metadata stays in the Trash.
    e.s.dismiss_file_changed(new).unwrap();
    assert!(!e.s.image(new).unwrap().file_changed);
    e.s.rebuild_index().unwrap();
    assert!(!e.s.image(new).unwrap().file_changed, "sidecar agrees");
    assert!(matches!(
        e.s.restore_replaced(new),
        Err(SessionError::NotFound { .. })
    ));
}

#[test]
fn an_unreadable_file_is_never_treated_as_a_different_photo() {
    use std::os::unix::fs::PermissionsExt;
    let (mut e, ids) = hashed_ext(&[("A.ARW", &fake_raw(0, &[1u8; 512]))]);
    let v = e.variant(ids[0]);
    e.s.set_rating(&[v], Rating::new(5).unwrap()).unwrap();
    let p = e.folder.join("A.ARW");
    let later = fs::metadata(&p).unwrap().modified().unwrap() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(later)
        .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.verify.len(), 1);
    let bg = e.background(&sync, None);
    assert_eq!(bg.unreadable, vec![ids[0]]);
    assert!(bg.replaced.is_empty() && bg.kept.is_empty());
    assert_eq!(e.s.variant(v).unwrap().rating.stars(), 5, "kept as it was");
    assert!(!e.s.image(ids[0]).unwrap().missing);
    // Readable again: verified, same bytes, still the same photo.
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.verify.len(), 1, "looked at again");
    let bg = e.background(&sync, None);
    assert_eq!(bg.kept, vec![(ids[0], identity::Verdict::Same)]);
    assert_eq!(e.s.variant(v).unwrap().rating.stars(), 5);
    assert_eq!(
        e.s.sync_external().unwrap().verify,
        [] as [external::ToVerify; 0]
    );
}

#[test]
fn renames_follow_by_stat_and_copies_by_content_without_swapping_duplicates() {
    // P1 and P2 are byte-identical (a second edit of the same shot), Q differs.
    let same = fake_raw(0, &[3u8; 700]);
    let (mut e, ids) = hashed_ext(&[
        ("P1.ARW", &same),
        ("P2.ARW", &same),
        ("Q.ARW", &fake_raw(0, &[4u8; 700])),
    ]);
    let (v1, v2) = (e.variant(ids[0]), e.variant(ids[1]));
    e.s.set_rating(&[v1], Rating::new(2).unwrap()).unwrap();
    e.s.set_rating(&[v2], Rating::new(4).unwrap()).unwrap();
    // Finder bulk rename (keeps size, mtime, inode): followed on open, no hashing.
    for (from, to) in [("P1", "Trip_001"), ("P2", "Trip_002"), ("Q", "Trip_003")] {
        fs::rename(
            e.folder.join(format!("{from}.ARW")),
            e.folder.join(format!("{to}.ARW")),
        )
        .unwrap();
    }
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.relinked.len(), 3, "{sync:?}");
    assert!(sync.verify.is_empty() && sync.held.is_empty() && sync.missing.is_empty());
    let rel = |e: &Ext, id| e.s.image(id).unwrap().rel_path.as_str().to_owned();
    assert_eq!(rel(&e, ids[0]), "Capture/Trip_001.ARW");
    assert_eq!(rel(&e, ids[1]), "Capture/Trip_002.ARW");
    assert_eq!(e.s.variant(v1).unwrap().rating.stars(), 2);
    assert_eq!(e.s.variant(v2).unwrap().rating.stars(), 4);
    // Copies (new inodes and mtimes): matched by content, paired by name.
    for (from, to) in [("Trip_001", "Keep 001"), ("Trip_002", "Keep 002")] {
        let (a, b) = (
            e.folder.join(format!("{from}.ARW")),
            e.folder.join(format!("{to}.ARW")),
        );
        fs::copy(&a, &b).unwrap();
        fs::remove_file(&a).unwrap();
    }
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.missing.len(), 2);
    assert_eq!(
        sync.held,
        vec!["Capture/Keep 001.ARW", "Capture/Keep 002.ARW"]
    );
    assert!(sync.untracked.is_empty(), "held until hashed");
    let bg = e.background(&sync, None);
    assert_eq!(bg.relink.relinked.len(), 2, "{:?}", bg.relink);
    assert_eq!(rel(&e, ids[0]), "Capture/Keep 001.ARW");
    assert_eq!(rel(&e, ids[1]), "Capture/Keep 002.ARW");
    assert_eq!(e.s.variant(v1).unwrap().rating.stars(), 2, "never swapped");
    assert_eq!(e.s.variant(v2).unwrap().rating.stars(), 4);
    // Names that say nothing: ambiguous, reported, not guessed.
    for (from, to) in [("Keep 001", "x"), ("Keep 002", "y")] {
        let (a, b) = (
            e.folder.join(format!("{from}.ARW")),
            e.folder.join(format!("{to}.ARW")),
        );
        fs::copy(&a, &b).unwrap();
        fs::remove_file(&a).unwrap();
    }
    let sync = e.s.sync_external().unwrap();
    let bg = e.background(&sync, None);
    assert_eq!(bg.relink.relinked, [] as [files::Moved; 0]);
    assert_eq!(
        bg.relink.ambiguous,
        vec!["Capture/Keep 001.ARW", "Capture/Keep 002.ARW"]
    );
    assert_eq!(bg.relink.unmatched, vec!["Capture/x.ARW", "Capture/y.ARW"]);
    assert_eq!(bg.registered.len(), 2, "the files are shown as new photos");
    assert!(e.s.image(ids[0]).unwrap().missing && e.s.image(ids[1]).unwrap().missing);
    // A held file matching nothing is simply new.
    let found = e.s.relink_found(&[]).unwrap();
    assert_eq!(found, RelinkReport::default());
}

#[test]
fn a_renamed_and_retagged_file_is_found_by_its_sensor_data() {
    let sensor = vec![5u8; 2048];
    let (mut e, ids) = hashed_ext(&[("A.ARW", &fake_raw(0, &sensor))]);
    let v = e.variant(ids[0]);
    e.s.set_rating(&[v], Rating::new(3).unwrap()).unwrap();
    fs::remove_file(e.folder.join("A.ARW")).unwrap();
    fs::write(e.folder.join("A-geo.ARW"), fake_raw(8, &sensor)).unwrap();
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.held, vec!["Capture/A-geo.ARW"]);
    let bg = e.background(&sync, Some(&sensor_ranges(64 + 2048)));
    assert_eq!(bg.relink.relinked.len(), 1);
    let img = e.s.image(ids[0]).unwrap();
    assert_eq!(img.rel_path.as_str(), "Capture/A-geo.ARW");
    assert_eq!(
        img.hash,
        hash(&fake_raw(8, &sensor)),
        "new file hash recorded"
    );
    assert_eq!(e.s.variant(v).unwrap().rating.stars(), 3);
}

#[test]
fn fat_volumes_sample_check_unchanged_stats() {
    let (mut e, ids) = hashed_ext(&[("A.ARW", &fake_raw(0, &[1u8; 300]))]);
    let v = e.variant(ids[0]);
    e.s.set_rating(&[v], Rating::new(4).unwrap()).unwrap();
    let sync = e.s.sync_external_with(true).unwrap();
    assert_eq!(sync.verify.len(), 1);
    assert!(sync.verify[0].sample_only);
    // Stored sample matches: nothing else to do.
    let bg = e.background(&sync, None);
    assert!(bg.kept.is_empty() && bg.replaced.is_empty(), "{bg:?}");
    // A different file with the same name, size and mtime (inode reused on exFAT):
    // simulated by rewriting in place and restoring the mtime (same inode).
    let p = e.folder.join("A.ARW");
    let mtime = fs::metadata(&p).unwrap().modified().unwrap();
    fs::write(&p, fake_raw(0, &[9u8; 300])).unwrap();
    fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let sync = e.s.sync_external_with(true).unwrap();
    assert!(sync.verify[0].sample_only);
    let bg = e.background(&sync, None);
    assert_eq!(
        bg.replaced.len(),
        1,
        "caught by the sample, confirmed by the full hash"
    );
    // Without a stored sample, the first check records one.
    assert_eq!(
        e.s.check_samples(&[(ImageId(1), FileHash([1; 32]))])
            .unwrap(),
        [] as [session::ImageId; 0]
    );
}

#[test]
fn remove_missing_keeps_photos_whose_file_is_back() {
    let (mut e, ids) = hashed_ext(&[
        ("A.ARW", &fake_raw(1, b"aaaa")),
        ("B.ARW", &fake_raw(2, b"bbbb")),
    ]);
    fs::remove_file(e.folder.join("A.ARW")).unwrap();
    fs::remove_file(e.folder.join("B.ARW")).unwrap();
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.missing.len(), 2);
    // B comes back before the user clicks "Remove" on a stale list.
    fs::write(e.folder.join("B.ARW"), fake_raw(2, b"bbbb")).unwrap();
    assert_eq!(e.s.forget_missing(&[ids[0], ids[1]]).unwrap(), 1);
    assert!(e.s.image(ids[0]).is_err());
    assert!(e.s.image(ids[1]).is_ok(), "not dropped: its file exists");
}

/// `chmod` for the fail-closed tests (restored by the caller).
fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn remove_missing_fails_closed_when_the_folder_is_unreadable() {
    let (mut e, ids) = hashed_ext(&[("A.ARW", &fake_raw(1, b"aaaa"))]);
    fs::remove_file(e.folder.join("A.ARW")).unwrap();
    assert_eq!(e.s.sync_external().unwrap().missing, vec![ids[0]]);
    // The share is there but unreadable (EACCES): nothing can be told gone.
    chmod(&e.folder, 0o000);
    let r = e.s.forget_missing(&[ids[0]]);
    chmod(&e.folder, 0o755);
    assert!(matches!(r, Err(SessionError::Io { .. })), "{r:?}");
    assert!(e.s.image(ids[0]).is_ok(), "nothing forgotten");
    // A folder that's gone altogether (an unmounted drive) fails the same way.
    let moved = e.folder.with_extension("away");
    fs::rename(&e.folder, &moved).unwrap();
    assert!(e.s.forget_missing(&[ids[0]]).is_err());
    fs::rename(&moved, &e.folder).unwrap();
    assert_eq!(e.s.forget_missing(&[ids[0]]).unwrap(), 1);
}

#[test]
fn an_unreadable_subfolder_is_neither_missing_nor_removable() {
    let (mut e, ids) = hashed_ext(&[
        ("Top.ARW", &fake_raw(1, b"aaaa")),
        ("sub/A.ARW", &fake_raw(2, b"bbbb")),
        ("sub/B.ARW", &fake_raw(3, b"cccc")),
    ]);
    let (top, a, b) = (ids[0], ids[1], ids[2]);
    let sub = e.folder.join("sub");
    // One failed read of `sub` (a share hiccup, permissions): its photos are
    // left as they were, not marked missing; a photo really gone still is.
    fs::remove_file(e.folder.join("Top.ARW")).unwrap();
    chmod(&sub, 0o000);
    let sync = e.s.sync_external();
    chmod(&sub, 0o755);
    let sync = sync.unwrap();
    assert_eq!(sync.missing, vec![top]);
    assert!(sync.verify.is_empty() && sync.relinked.is_empty());
    assert!(!e.s.image(a).unwrap().missing && !e.s.image(b).unwrap().missing);

    // Remove can't tell a file in an unreadable folder from a gone one: kept.
    fs::remove_file(sub.join("B.ARW")).unwrap();
    chmod(&sub, 0o000);
    let n = e.s.forget_missing(&[top, a, b]);
    chmod(&sub, 0o755);
    assert_eq!(n.unwrap(), 1, "only the photo whose file is not found");
    assert!(e.s.image(top).is_err());
    assert!(e.s.image(a).is_ok() && e.s.image(b).is_ok());
    assert_eq!(e.s.forget_missing(&[b]).unwrap(), 1, "readable again: gone");
}

#[test]
fn capture_one_session_folders_list_only_capture_and_selects() {
    let e = ext(&[
        ("Capture/a.ARW", b"a"),
        ("Capture/CaptureOne/Cache/Proxies/a.ARW", b"p"),
        ("Selects/c.ARW", b"c"),
        ("Trash/rej.ARW", b"r"),
        ("Output/a.jpg", b"o"),
        ("CaptureOne/Settings153/x.ARW", b"s"),
    ]);
    fs::write(e.folder.join("Shoot.cosessiondb"), b"db").unwrap();
    let (files, _) = e.s.external_files().unwrap();
    let names: Vec<&str> = files
        .iter()
        .map(|(r, _)| r.as_str())
        .filter(|r| Path::new(r).extension().is_some_and(|e| e == "ARW"))
        .collect();
    assert_eq!(
        names,
        vec!["Capture/Capture/a.ARW", "Capture/Selects/c.ARW"]
    );
    // Without the C1 database, Trash/ and Output/ are ordinary folders.
    fs::remove_file(e.folder.join("Shoot.cosessiondb")).unwrap();
    let (files, _) = e.s.external_files().unwrap();
    assert!(files.iter().any(|(r, _)| r == "Capture/Trash/rej.ARW"));
}

/// Darkslide's `Developed/` folder (developed copies for a photo server) at the top
/// of a folder opened in place is never part of the session; a user's own
/// `Developed` folder (no marker) or one deeper down still is.
#[test]
fn developed_copies_are_not_photos_of_the_session() {
    let e = ext(&[
        ("a.ARW", b"a"),
        ("Developed/a.jpg", b"d"),
        ("Developed/Day 1/b.jpg", b"d"),
        ("Day 1/b.ARW", b"b"),
        ("Day 1/Developed/c.jpg", b"c"),
    ]);
    let listed = |e: &Ext| -> Vec<String> {
        let (files, _) = e.s.external_files().unwrap();
        files.into_iter().map(|(r, _)| r).collect()
    };
    // No marker: the user's own folder, listed like any other.
    assert!(listed(&e).contains(&"Capture/Developed/a.jpg".to_owned()));
    fs::write(e.folder.join("Developed").join(DEVELOPED_MARKER), b"").unwrap();
    let files = listed(&e);
    assert!(
        !files.iter().any(|r| r.starts_with("Capture/Developed/")),
        "{files:?}"
    );
    assert!(files.contains(&"Capture/a.ARW".to_owned()));
    assert!(files.contains(&"Capture/Day 1/b.ARW".to_owned()));
    assert!(
        files.contains(&"Capture/Day 1/Developed/c.jpg".to_owned()),
        "only the top-level folder is Darkslide's"
    );
}

/// An imported session only has photos under Capture/, Selects/ and Trash/: a
/// `Developed/` folder at its root is never registered, listed or untracked.
#[test]
fn developed_folder_of_an_imported_session_is_ignored() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let mut s = Session::create(&root, "s").unwrap();
    let dev = root.join(DEVELOPED_DIR).join("Capture");
    fs::create_dir_all(&dev).unwrap();
    fs::write(dev.join("a.jpg"), b"jpeg").unwrap();
    fs::write(root.join(DEVELOPED_DIR).join(DEVELOPED_MARKER), b"").unwrap();
    let report = s.rescan().unwrap();
    assert!(report.untracked.is_empty(), "{:?}", report.untracked);
    assert_eq!(s.image_count().unwrap(), 0);
    drop(s);
    let s = Session::open(&root).unwrap();
    assert_eq!(s.image_count().unwrap(), 0);
}

#[test]
fn journal_replay_never_writes_through_a_symlink() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let s = Session::create(&root, "s").unwrap();
    drop(s);
    let outside = t.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    // A crafted store: Capture/evil -> outside, plus a journal writing into it.
    std::os::unix::fs::symlink(&outside, root.join("Capture/evil")).unwrap();
    fs::create_dir_all(root.join(".ds/journal")).unwrap();
    fs::write(
        root.join(".ds/journal/1.json"),
        br#"[{"rel":"Capture/evil/.ds/settings/x.json","json":"{}","created":true},{"rel":"Capture/.ds/settings/ok.json","json":"{}","created":true}]"#,
    )
    .unwrap();
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 1);
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing written outside the store"
    );
    assert!(root.join("Capture/.ds/settings/ok.json").is_file());
}

#[test]
fn journal_replay_only_writes_sidecars() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    drop(Session::create(&root, "s").unwrap());
    let settings = root.join(".ds/settings");
    fs::write(settings.join("albums.json"), br#"{"albums":[]}"#).unwrap();
    let before = fs::read(settings.join("session.json")).unwrap();
    fs::create_dir_all(root.join(".ds/journal")).unwrap();
    fs::write(
        root.join(".ds/journal/1.json"),
        br#"[{"rel":".ds/settings/session.json","json":"{}"},
            {"rel":".ds/settings/albums.json","json":"{}"},
            {"rel":"Capture/.ds/settings/v2/ok.json","json":"{}","created":true}]"#,
    )
    .unwrap();
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 1);
    assert_eq!(fs::read(settings.join("session.json")).unwrap(), before);
    assert_eq!(
        fs::read(settings.join("albums.json")).unwrap(),
        br#"{"albums":[]}"#
    );
    assert!(root.join("Capture/.ds/settings/v2/ok.json").is_file());
}

#[test]
fn journal_replay_does_not_resurrect_a_sidecar_deleted_after_the_crash() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    let (mut s, vs) = rated_session(&root, 2);
    s.set_rating(&vs, Rating::new(3).unwrap()).unwrap();
    let items: Vec<(PathBuf, Vec<u8>)> = (0..2)
        .map(|i| {
            let p = root.join(format!("Capture/.ds/settings/{i}.ARW.json"));
            (p.clone(), fs::read(p).unwrap())
        })
        .collect();
    drop(s);
    crate::journal::begin(&root, &items).unwrap().unwrap();
    // The user deleted one photo's sidecar before reopening.
    fs::remove_file(&items[0].0).unwrap();
    let s = Session::open(&root).unwrap();
    assert_eq!(s.journal_replayed, 1);
    assert!(!items[0].0.exists(), "not brought back");
    assert!(items[1].0.exists());
}

#[test]
fn migration_removes_verified_duplicates_keeps_only_copies_and_can_restore_them() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("dup.ARW"), b"same").unwrap();
    fs::write(folder.join("diff.ARW"), b"folder-version").unwrap();
    let root = folder.join(".ds/session");
    let mut s = Session::create(&root, "Shoot").unwrap();
    // A round-3 folder copied with `cp -R`: plain copies instead of hard links.
    for (n, b) in [
        ("dup.ARW", &b"same"[..]),
        ("diff.ARW", b"store-version"),
        ("only.ARW", b"only"),
    ] {
        fs::write(root.join("Capture").join(n), b).unwrap();
        s.add_original(&format!("Capture/{n}"), hash(b), meta())
            .unwrap();
    }
    let only = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/only.ARW")
        .unwrap()
        .id;
    drop(s);
    let m = Session::migrate_to_external(&root, &folder).unwrap();
    assert_eq!((m.links_removed, m.duplicates_removed), (0, 1));
    assert_eq!(m.kept, vec!["diff.ARW", "only.ARW"]);
    assert!(
        !root.join("Capture/dup.ARW").exists(),
        "verified duplicate removed"
    );
    assert_eq!(fs::read(folder.join("dup.ARW")).unwrap(), b"same");
    assert_eq!(
        fs::read(root.join("Capture/diff.ARW")).unwrap(),
        b"store-version"
    );
    let mut s = Session::open(&root).unwrap();
    s.sync_external().unwrap();
    assert!(s.image(only).unwrap().missing);
    assert!(s.kept_copy(only).is_some(), "the only copy is in the store");
    s.restore_kept_copy(only).unwrap();
    assert_eq!(fs::read(folder.join("only.ARW")).unwrap(), b"only");
    assert!(!root.join("Capture/only.ARW").exists());
    assert!(!s.image(only).unwrap().missing);
    assert!(s.kept_copy(only).is_none());
    assert!(matches!(
        s.restore_kept_copy(only),
        Err(SessionError::NotFound { .. })
    ));
    // Never over a file that is there: diff.ARW exists in the folder.
    let diff = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/diff.ARW")
        .unwrap()
        .id;
    assert!(s.restore_kept_copy(diff).is_err());
    assert_eq!(
        fs::read(folder.join("diff.ARW")).unwrap(),
        b"folder-version"
    );
    assert_eq!(
        fs::read(root.join("Capture/diff.ARW")).unwrap(),
        b"store-version"
    );
}

#[test]
fn migration_never_follows_a_symlinked_original() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().join("Shoot");
    fs::create_dir_all(&folder).unwrap();
    let root = folder.join(".ds/session");
    let mut s = Session::create(&root, "Shoot").unwrap();
    fs::write(root.join("Capture/A.ARW"), b"aaaa").unwrap();
    s.add_original("Capture/A.ARW", hash(b"aaaa"), meta())
        .unwrap();
    drop(s);
    // The user keeps another hard link elsewhere; A.ARW in the folder is a symlink
    // pointing back into the store.
    fs::hard_link(root.join("Capture/A.ARW"), t.path().join("backup.ARW")).unwrap();
    std::os::unix::fs::symlink(root.join("Capture/A.ARW"), folder.join("A.ARW")).unwrap();
    let m = Session::migrate_to_external(&root, &folder).unwrap();
    assert_eq!(m.links_removed, 0);
    assert_eq!(m.kept, vec!["A.ARW"]);
    assert_eq!(
        fs::read(folder.join("A.ARW")).unwrap(),
        b"aaaa",
        "the symlink still resolves"
    );
}

/// A new photo's file a crash left half-written (`.ds-*.partial`, AI Upscale) is
/// removed when the folder is read once it is stale; a fresh one (a write in progress)
/// and everything else stay.
#[test]
fn stale_partial_files_are_removed_when_the_folder_is_read() {
    let e = ext(&[("a.ARW", b"a"), ("sub/b.ARW", b"b")]);
    let make = |rel: &str, age: u64| {
        let p = e.folder.join(rel);
        let f = fs::File::create(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age))
            .unwrap();
        p
    };
    let stale = make(".ds-1a2b-3.partial", 7200);
    let nested = make("sub/.ds-1a2b-4.partial", 7200);
    let fresh = make(".ds-1a2b-5.partial", 5);
    let other = make(".hidden-notes", 7200);
    let (files, _) = e.s.external_files().unwrap();
    assert_eq!(files.len(), 2);
    assert!(!stale.exists() && !nested.exists());
    assert!(fresh.exists() && other.exists());
}

/// The same in a plain session's folders (an index sync on open).
#[test]
fn stale_partial_files_are_removed_from_a_plain_session_on_open() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("s");
    drop(Session::create(&root, "s").unwrap());
    let p = root.join("Capture/.ds-77-1.partial");
    let f = fs::File::create(&p).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
        .unwrap();
    let keep = root.join("Capture/DSC1.ARW");
    fs::write(&keep, b"raw").unwrap();
    drop(Session::open(&root).unwrap());
    assert!(!p.exists());
    assert!(keep.exists());
}

#[test]
fn sidecars_and_other_files_do_not_count_toward_the_photo_cap() {
    let mut files: Vec<(String, Vec<u8>)> = (0..10)
        .map(|i| (format!("p{i:02}.ARW"), vec![i as u8]))
        .collect();
    // 70 files in all: past the 20-photo cap, within the 80-entry one.
    files.extend((0..30).map(|i| (format!("p{i:02}.xmp"), vec![1])));
    files.extend((0..30).map(|i| (format!("n{i:02}.txt"), vec![2])));
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let e = ext(&refs);
    let (listed, seen) = e.s.external_files().unwrap();
    assert_eq!(seen, 10, "only photos are counted");
    assert_eq!(listed.len(), 70, "everything under the cap is still listed");
}

#[test]
fn photos_past_the_cap_are_never_marked_missing() {
    let late: Vec<(String, Vec<u8>)> = (0..15)
        .map(|i| (format!("z{i:02}.ARW"), vec![i as u8; 4]))
        .collect();
    let refs: Vec<(&str, &[u8])> = late
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let mut e = ext(&refs);
    let sync = e.s.sync_external().unwrap();
    e.register(&sync.untracked);
    // More photos that sort first: the walk now stops before the z files.
    for i in 0..10 {
        fs::write(e.folder.join(format!("a{i:02}.ARW")), [9u8; 3]).unwrap();
    }
    let sync = e.s.sync_external().unwrap();
    assert_eq!(sync.seen, 25, "seen reports the truncation");
    assert!(
        sync.missing.is_empty(),
        "unlisted photos are unknown, not gone"
    );
    assert!(
        e.s.images().unwrap().iter().all(|i| !i.missing),
        "no row flagged"
    );
    let rescan = e.s.rescan().unwrap();
    assert_eq!(
        rescan.missing,
        [] as [(session::ImageId, relpath::RelPath); 0]
    );
}
