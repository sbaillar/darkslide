use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use super::mock::{self, MockS3};
use super::*;
use crate::archiver::Snapshotter;

fn retry_fast(c: Client) -> Client {
    c.with_retry(RetryPolicy {
        attempts: 3,
        base: Duration::from_millis(1),
    })
    .with_sleep(Arc::new(|_| {}))
}

fn dest_with(s3: &Arc<MockS3>, prefix: &str, path_style: bool) -> ObjectDestination {
    ObjectDestination::new(mock::config(prefix, path_style), s3.clone(), mock::creds())
        .unwrap()
        .with_client(retry_fast)
}

fn write(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// A session folder with originals, a sidecar, a developed copy, the live DB with its
/// WAL, a cache entry and a partial transfer.
fn session(root: &Path, name: &str) -> PathBuf {
    let s = root.join(name);
    write(&s.join("Capture/DSC0001.ARW"), &[1u8; 3000]);
    write(&s.join("Capture/DSC0001.ARW.xmp"), b"<xmp/>");
    write(&s.join("Developed/DSC0001.jpg"), b"jpeg");
    write(&s.join("session.dsession"), b"live db");
    write(&s.join("session.dsession-wal"), b"wal");
    write(&s.join(".ds/cache/thumb.bin"), b"cache");
    write(&s.join("Capture/x.ARW.partial"), b"half");
    write(&s.join(".ds/edits.json"), b"{}");
    s
}

struct Snap;
impl Snapshotter for Snap {
    fn snapshot(&self, _: &Path, dest: &Path) -> Result<(), String> {
        std::fs::write(dest, b"snapshot").map_err(|e| e.to_string())
    }
}

struct FailingSnap;
impl Snapshotter for FailingSnap {
    fn snapshot(&self, _: &Path, _: &Path) -> Result<(), String> {
        Err("db busy".into())
    }
}

fn source(dir: &Path) -> SessionSource {
    SessionSource {
        dir: dir.to_path_buf(),
        excludes: vec![],
        mirror_folders: vec!["Capture".into(), "Selects".into()],
        snapshotter: Some(Arc::new(Snap)),
    }
}

fn run(
    d: &ObjectDestination,
    src: &SessionSource,
    owner: &str,
) -> Result<ArchiveOutcome, ArchiveError> {
    let none = AtomicBool::new(false);
    let claimed = d.claim(&src.dir, owner, None, &none)?;
    d.archive(src, &claimed, SystemTime::now(), &mut |_| {}, &none)
}

fn keys(s3: &MockS3) -> Vec<String> {
    s3.state().objects.keys().cloned().collect()
}

#[test]
fn put_skip_and_replace_with_trash() {
    let t = tempfile::tempdir().unwrap();
    let dir = session(t.path(), "Iceland");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "darkslide", false);
    let out = run(&d, &source(&dir), "aaaa1111bbbb2222").unwrap();
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    assert_eq!(out.dest, PathBuf::from("s3://photos/darkslide/Iceland"));
    assert!(out.backup_dir.starts_with(".ds-archive-trash/"));
    assert_eq!(out.snapshot, Some(dir.join(SNAPSHOT_REL)));
    assert_eq!(
        keys(&s3),
        [
            "darkslide/Iceland/.ds-archive-owner",
            "darkslide/Iceland/.ds/edits.json",
            "darkslide/Iceland/.ds/index-snapshot.db",
            "darkslide/Iceland/Capture/DSC0001.ARW",
            "darkslide/Iceland/Capture/DSC0001.ARW.xmp",
            "darkslide/Iceland/Developed/DSC0001.jpg",
        ],
        "the live DB, its WAL, .ds/cache and partial files stay out"
    );
    {
        let s = s3.state();
        let o = s.object("darkslide/Iceland/Capture/DSC0001.ARW").unwrap();
        assert_eq!(
            o.meta.get(META_BLAKE3).unwrap(),
            &blake3::hash(&[1u8; 3000]).to_hex().to_string()
        );
        assert!(o.meta.contains_key("x-amz-meta-ds-mtime"));
        assert_eq!(s.object_deletes(), 0);
    }

    // Nothing changed: no upload, no HeadObject (size, mtime and the listing agree).
    let puts = s3.state().count("PUT", "");
    let heads = |s: &mock::MockState, suffix: &str| {
        s.requests
            .iter()
            .filter(|r| r.method == "HEAD" && r.key.ends_with(suffix))
            .count()
    };
    let (arw, snap) = {
        let s = s3.state();
        (heads(&s, ".ARW"), heads(&s, "snapshot.db"))
    };
    run(&d, &source(&dir), "aaaa1111bbbb2222").unwrap();
    {
        let s = s3.state();
        // Nothing is sent: the snapshot was rewritten (new mtime) with the same
        // content, which its HeadObject's hash shows.
        assert_eq!(s.count("PUT", ""), puts);
        assert_eq!(heads(&s, ".ARW"), arw);
        assert_eq!(heads(&s, "snapshot.db"), snap + 1);
    }

    // The pre-check cache is gone: the hash decides, nothing is re-sent.
    for e in std::fs::read_dir(dir.join(".ds/cache")).unwrap().flatten() {
        if e.file_name()
            .to_string_lossy()
            .starts_with("archive-object-")
        {
            std::fs::remove_file(e.path()).unwrap();
        }
    }
    let puts = s3.state().count("PUT", "");
    run(&d, &source(&dir), "aaaa1111bbbb2222").unwrap();
    assert_eq!(
        s3.state().count("PUT", ""),
        puts,
        "the hashes match: nothing sent"
    );

    // Same size, new content: the old one goes to the trash first.
    write(&dir.join("Capture/DSC0001.ARW.xmp"), b"<XMP/>");
    let out = run(&d, &source(&dir), "aaaa1111bbbb2222").unwrap();
    let s = s3.state();
    let trash = format!(
        "darkslide/{}/Iceland/Capture/DSC0001.ARW.xmp",
        out.backup_dir
    );
    assert_eq!(s.object(&trash).unwrap().data, b"<xmp/>");
    assert_eq!(
        s.object("darkslide/Iceland/Capture/DSC0001.ARW.xmp")
            .unwrap()
            .data,
        b"<XMP/>"
    );
    // The snapshot is replaced in place, never trashed.
    assert!(
        !s.objects
            .keys()
            .any(|k| k.contains(".ds-archive-trash") && k.ends_with("index-snapshot.db"))
    );
    assert_eq!(s.object_deletes(), 0, "nothing is ever deleted");
    assert!(
        s.requests.iter().all(|r| r.method != "DELETE"),
        "not even an abort: nothing failed"
    );
}

#[test]
fn rejects_are_excluded_and_a_failed_snapshot_is_a_problem() {
    let t = tempfile::tempdir().unwrap();
    let dir = session(t.path(), "S");
    write(&dir.join("Trash/old.ARW"), b"x");
    write(&dir.join("Capture/a*b.ARW"), b"x");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", true);
    let mut src = source(&dir);
    src.excludes = vec!["/Trash/".into(), "/Capture/a\\*b.ARW".into()];
    src.snapshotter = Some(Arc::new(FailingSnap));
    std::fs::write(dir.join(SNAPSHOT_REL), b"stale").unwrap();
    let out = run(&d, &src, "abc").unwrap();
    assert_eq!(out.snapshot, None);
    assert_eq!(out.problems.len(), 1);
    assert!(
        out.problems[0].contains("snapshot failed"),
        "{:?}",
        out.problems
    );
    let k = keys(&s3);
    assert!(
        k.iter()
            .all(|k| !k.contains("Trash") && !k.contains("a*b") && !k.contains("snapshot")),
        "{k:?}"
    );
    assert!(k.contains(&"S/Capture/DSC0001.ARW".to_owned()));
}

#[test]
fn spaces_and_unicode_in_prefix_session_and_file_names() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Été 2026 (Lisboa)");
    write(&dir.join("Capture/Ça va (1)+ü.ARW"), b"raw");
    let s3 = Arc::new(MockS3::new());
    for path_style in [false, true] {
        let d = dest_with(&s3, " /my archive//ünï/ ", path_style);
        assert_eq!(d.prefix(), "my archive/ünï/");
        run(&d, &source(&dir), "abc").unwrap();
        let k = "my archive/ünï/Été 2026 (Lisboa)/Capture/Ça va (1)+ü.ARW";
        assert_eq!(s3.state().object(k).unwrap().data, b"raw");
        let s = s3.state();
        let hosts: Vec<&str> = s.requests.iter().map(|r| r.host.as_str()).collect();
        let want = if path_style {
            mock::HOST.to_owned()
        } else {
            format!("{}.{}", mock::BUCKET, mock::HOST)
        };
        assert_eq!(hosts.last().copied(), Some(want.as_str()));
    }
    assert_eq!(
        dest_with(&s3, "x", false).location(),
        format!("s3://photos/x/ at {}", mock::HOST)
    );
    assert_eq!(
        dest_with(&s3, "x", true).lock_key(),
        PathBuf::from(format!("s3://{}/photos/x/", mock::HOST))
    );
}

/// 20 MB: three parts of 8 MB, 8 MB and 4 MB.
fn big(dir: &Path) -> Vec<u8> {
    let data: Vec<u8> = (0..20_000_000u32).map(|i| (i % 251) as u8).collect();
    write(&dir.join("Capture/clip.MP4"), &data);
    data
}

#[test]
fn multipart_upload_verifies_and_survives_transient_failures() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Video");
    let data = big(&dir);
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    // Two transient failures on part 2 (a 503 and a dropped connection): retried.
    s3.fail("PUT", "partNumber=2", "", Some(503), 1);
    s3.fail("PUT", "partNumber=2", "", None, 1);
    let out = run(&d, &source(&dir), "abc").unwrap();
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    let s = s3.state();
    let o = s.object("Video/Capture/clip.MP4").unwrap();
    assert_eq!(o.data, data);
    assert!(o.etag.ends_with("-3"), "{}", o.etag);
    assert_eq!(s.count("PUT", "partNumber="), 5, "3 parts + 2 retries");
    assert!(s.uploads.is_empty());
}

#[test]
fn a_multipart_failure_aborts_cleanly_and_leaves_no_object() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Video");
    big(&dir);
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    s3.fail("PUT", "partNumber=2", "", Some(500), 10);
    let out = run(&d, &source(&dir), "abc").unwrap();
    assert_eq!(out.problems.len(), 1, "{:?}", out.problems);
    assert!(
        out.problems[0].starts_with("Capture/clip.MP4: "),
        "{:?}",
        out.problems
    );
    let s = s3.state();
    assert!(s.object("Video/Capture/clip.MP4").is_none());
    assert!(s.uploads.is_empty(), "the upload was aborted");
    assert_eq!(s.count("DELETE", "uploadId="), 1);
    assert_eq!(s.object_deletes(), 0);
    drop(s);
    s3.state().faults.clear();
    // Offline in the middle: the run stops (offline, retried later), still aborted.
    s3.fail("PUT", "partNumber=2", "", None, 10);
    let err = run(&d, &source(&dir), "abc").unwrap_err();
    assert!(
        matches!(err, ArchiveError::TargetUnavailable { .. }),
        "{err}"
    );
    assert!(s3.state().uploads.is_empty());
}

#[test]
fn a_file_changing_during_its_upload_is_not_completed() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Video");
    big(&dir);
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    let path = dir.join("Capture/clip.MP4");
    s3.state().hook = Some(Box::new(move |r| {
        if r.url.contains("partNumber=1&") {
            // Past the part being sent: the parts still to read change.
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(19_000_000)).unwrap();
            std::io::Write::write_all(&mut f, &[9u8; 100]).unwrap();
        }
    }));
    let out = run(&d, &source(&dir), "abc").unwrap();
    assert!(
        out.problems[0].contains("changed while"),
        "{:?}",
        out.problems
    );
    let s = s3.state();
    assert!(s.object("Video/Capture/clip.MP4").is_none());
    assert!(s.uploads.is_empty());
}

#[test]
fn cancelling_mid_upload_aborts_it() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Video");
    big(&dir);
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    s3.state().hook = Some(Box::new(move |r| {
        if r.url.contains("partNumber=1&") {
            c.store(true, Ordering::Relaxed);
        }
    }));
    let none = AtomicBool::new(false);
    let claimed = d.claim(&dir, "abc", None, &none).unwrap();
    let err = d
        .archive(
            &source(&dir),
            &claimed,
            SystemTime::now(),
            &mut |_| {},
            &cancel,
        )
        .unwrap_err();
    assert!(matches!(err, ArchiveError::Cancelled), "{err}");
    let s = s3.state();
    assert!(s.uploads.is_empty());
    assert_eq!(s.count("PUT", "partNumber=2"), 0);
    assert!(s.object("Video/Capture/clip.MP4").is_none());
    drop(s);
    // Cancelled before anything: no request at all after the bucket check.
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(d.check(&cancel), Err(ArchiveError::Cancelled)));
}

#[test]
fn retries_then_offline() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("S");
    write(&dir.join("a.jpg"), b"a");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    s3.fail("PUT", "", "a.jpg", None, 2);
    let out = run(&d, &source(&dir), "abc").unwrap();
    assert_eq!(out.problems, [] as [String; 0]);
    assert_eq!(
        s3.state().count("PUT", ""),
        5,
        "marker, snapshot, a.jpg after 2 failures"
    );
    s3.state().offline = true;
    let err = d.check(&AtomicBool::new(false)).unwrap_err();
    assert!(
        matches!(err, ArchiveError::TargetUnavailable { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("didn't answer"), "{err}");
    s3.state().offline = false;
    // A 5xx that never ends is a storage error for that file.
    s3.fail("PUT", "", "a.jpg", Some(500), 100);
    write(&dir.join("a.jpg"), b"b");
    let out = run(&d, &source(&dir), "abc").unwrap();
    assert!(out.problems[0].contains("500"), "{:?}", out.problems);
}

#[test]
fn wrong_keys_missing_bucket_and_clock_skew_need_the_user() {
    let s3 = Arc::new(MockS3::new());
    let none = AtomicBool::new(false);
    let bad = |access: &str, secret: &str| {
        ObjectDestination::new(
            mock::config("", false),
            s3.clone(),
            Credentials {
                access_key_id: access.into(),
                secret_access_key: secret.into(),
            },
        )
        .unwrap()
        .with_client(retry_fast)
    };
    for d in [bad("WRONG", mock::SECRET), bad(mock::ACCESS_KEY, "wrong")] {
        let err = d.test_connection(SystemTime::now(), &none).unwrap_err();
        assert!(matches!(err, ArchiveError::TargetNotUsable { .. }), "{err}");
        assert!(err.to_string().contains("refused the access key"), "{err}");
    }
    let d = dest_with(&s3, "", false);
    s3.state().bucket_exists = false;
    let err = d.check(&none).unwrap_err();
    assert!(err.to_string().contains("bucket doesn't exist"), "{err}");
    let err = d.test_connection(SystemTime::now(), &none).unwrap_err();
    assert!(err.to_string().contains("bucket doesn't exist"), "{err}");
    s3.state().bucket_exists = true;
    s3.state().clock_offset = 3600;
    // HeadBucket has no body: the server's Date tells.
    let err = d.check(&none).unwrap_err();
    assert!(err.to_string().contains("clock"), "{err}");
    let err = d.test_connection(SystemTime::now(), &none).unwrap_err();
    assert!(err.to_string().contains("clock"), "{err}");
    s3.state().clock_offset = -3600;
    assert!(d.check(&none).unwrap_err().to_string().contains("clock"));
    s3.state().clock_offset = 0;
    assert_eq!(s3.state().object_deletes(), 0);
}

#[test]
fn connection_test_leaves_one_small_object() {
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "darkslide/", true);
    let none = AtomicBool::new(false);
    let r = d.test_connection(SystemTime::now(), &none).unwrap();
    assert_eq!(r.test_key, "darkslide/.ds-archive-test/connection-test.txt");
    assert_eq!(
        r.location,
        format!("s3://photos/darkslide/ at {}", mock::HOST)
    );
    d.test_connection(SystemTime::now(), &none).unwrap();
    assert_eq!(keys(&s3), std::slice::from_ref(&r.test_key));
    assert_eq!(d.check(&none).unwrap().as_deref(), Some("object storage"));
    assert_eq!(d.kind(), DestinationKind::ObjectStorage);
    s3.state().wrong_etag = true;
    let err = d.test_connection(SystemTime::now(), &none).unwrap_err();
    assert!(matches!(err, ArchiveError::Storage(_)), "{err}");
}

#[test]
fn a_wrong_etag_is_a_problem_not_a_success() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("S");
    write(&dir.join("a.jpg"), b"a");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    let none = AtomicBool::new(false);
    let claimed = d.claim(&dir, "abc", None, &none).unwrap();
    s3.state().wrong_etag = true;
    let out = d
        .archive(
            &source(&dir),
            &claimed,
            SystemTime::now(),
            &mut |_| {},
            &none,
        )
        .unwrap();
    assert!(
        out.problems
            .iter()
            .any(|p| p.starts_with("a.jpg: ") && p.contains("ETag")),
        "{:?}",
        out.problems
    );
}

#[test]
fn claims_mirror_the_folder_rules() {
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("one/Trip");
    let b = t.path().join("two/Trip");
    write(&a.join("x.jpg"), b"a");
    write(&b.join("y.jpg"), b"b");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "p", false);
    let none = AtomicBool::new(false);
    let ca = d.claim(&a, "aaaaaaaa11", None, &none).unwrap();
    assert_eq!(ca.folder, "Trip");
    d.archive(&source(&a), &ca, SystemTime::now(), &mut |_| {}, &none)
        .unwrap();
    // Another session with the same name gets its own folder.
    let cb = d.claim(&b, "bbbbbbbb22", None, &none).unwrap();
    assert_eq!(cb.folder, "Trip (bbbbbbbb)");
    // Its recorded folder is kept; someone else's recorded folder is not taken.
    assert_eq!(
        d.claim(&b, "bbbbbbbb22", Some("Trip (bbbbbbbb)"), &none)
            .unwrap()
            .folder,
        "Trip (bbbbbbbb)"
    );
    assert_eq!(
        d.claim(&b, "bbbbbbbb22", Some("Trip"), &none)
            .unwrap()
            .folder,
        "Trip (bbbbbbbb)"
    );
    assert_eq!(
        d.claim(&a, "aaaaaaaa11", None, &none).unwrap().folder,
        "Trip"
    );
    // A non-empty folder without a marker is not adopted.
    s3.state().objects.insert(
        "p/Other/z".into(),
        mock::MockObject {
            data: vec![],
            meta: BTreeMap::default(),
            etag: String::new(),
        },
    );
    let c = t.path().join("Other");
    std::fs::create_dir(&c).unwrap();
    assert_eq!(
        d.claim(&c, "cccccccc33", None, &none).unwrap().folder,
        "Other (cccccccc)"
    );
    // Both names taken by others: refused.
    for (folder, owner) in [
        ("Third", "x"),
        ("Third (dddddddd)", "y"),
        ("Third (dddddddd44)", "z"),
    ] {
        s3.state().objects.insert(
            format!("p/{folder}/.ds-archive-owner"),
            mock::MockObject {
                data: owner.as_bytes().to_vec(),
                meta: BTreeMap::default(),
                etag: String::new(),
            },
        );
    }
    let third = t.path().join("Third");
    std::fs::create_dir(&third).unwrap();
    assert!(matches!(
        d.claim(&third, "dddddddd44", None, &none),
        Err(ArchiveError::TargetNotUsable { .. })
    ));
    // Invalid owners and sessions.
    assert!(d.claim(&third, "a/b", None, &none).is_err());
    assert!(d.claim(Path::new("/"), "abc", None, &none).is_err());
    assert!(matches!(
        d.archive(
            &SessionSource::new("relative"),
            &ca,
            SystemTime::now(),
            &mut |_| {},
            &none
        ),
        Err(ArchiveError::InvalidSession(_))
    ));
}

/// Two sessions named alike claiming the same folder at once: the owner marker is
/// created with `If-None-Match: *`, and whoever loses takes the next name. A
/// service without conditional writes falls back to a plain write, and the marker
/// read back decides.
#[test]
fn a_claim_race_is_lost_cleanly() {
    use mock::{Conditional, Race};
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("Trip");
    write(&a.join("x.jpg"), b"a");
    let none = AtomicBool::new(false);
    let marker = "p/Trip/.ds-archive-owner";
    for (conditional, after, skip) in [
        (Conditional::Honoured, false, 0),
        (Conditional::Ignored, true, 0),
        // The 501 writes nothing; the plain write after it is the one raced.
        (Conditional::NotImplemented, true, 1),
    ] {
        let s3 = Arc::new(MockS3::new());
        let d = dest_with(&s3, "p", false);
        s3.state().conditional = conditional;
        // The other session's marker lands between our check and our write (or,
        // where the condition does nothing, right after our write).
        s3.state().race = Some(Race {
            key: marker.into(),
            data: b"bbbbbbbb22".to_vec(),
            after,
            skip,
        });
        let c = d.claim(&a, "aaaaaaaa11", None, &none).unwrap();
        assert_eq!(c.folder, "Trip (aaaaaaaa)", "{conditional:?}");
        let s = s3.state();
        assert_eq!(
            s.object(marker).unwrap().data,
            b"bbbbbbbb22",
            "theirs stays"
        );
        assert_eq!(
            s.object("p/Trip (aaaaaaaa)/.ds-archive-owner")
                .unwrap()
                .data,
            b"aaaaaaaa11"
        );
        if conditional == Conditional::Honoured {
            assert!(
                s.requests
                    .iter()
                    .any(|r| r.method == "PUT" && r.key == marker && r.status == 412)
            );
        }
        assert_eq!(s.object_deletes(), 0);
    }
    // Our own marker already there (a retried write whose first answer was
    // lost): the 412 is ours, and the folder is claimed.
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "p", false);
    s3.state().race = Some(Race {
        key: marker.into(),
        data: b"aaaaaaaa11".to_vec(),
        after: false,
        skip: 0,
    });
    assert_eq!(
        d.claim(&a, "aaaaaaaa11", None, &none).unwrap().folder,
        "Trip"
    );
    // No race: one conditional write, and the folder is ours.
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "p", false);
    assert_eq!(
        d.claim(&a, "aaaaaaaa11", None, &none).unwrap().folder,
        "Trip"
    );
    assert_eq!(s3.state().object(marker).unwrap().data, b"aaaaaaaa11");
    // An error answer to the conditional write is an error, not a claim.
    s3.fail("PUT", "", "Other/.ds-archive-owner", Some(403), 1);
    let other = t.path().join("Other");
    std::fs::create_dir(&other).unwrap();
    assert!(d.claim(&other, "aaaaaaaa11", None, &none).is_err());
}

/// An empty file is archived as a zero-byte object (a body-less `PutObject`).
#[test]
fn zero_byte_files_are_archived() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Empty");
    write(&dir.join("Capture/empty.txt"), b"");
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "p", false);
    let out = run(&d, &source(&dir), "aaaaaaaa11").unwrap();
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    let s = s3.state();
    let o = s.object("p/Empty/Capture/empty.txt").unwrap();
    assert_eq!(o.data, Vec::<u8>::new());
    assert_eq!(
        o.meta.get(META_BLAKE3).unwrap(),
        &blake3::hash(b"").to_hex().to_string()
    );
}

#[test]
fn progress_and_listing_pages() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("S");
    for i in 0..5 {
        write(&dir.join(format!("f{i}.jpg")), &[i as u8; 1000]);
    }
    let s3 = Arc::new(MockS3::new());
    s3.state().list_page = 2;
    let d = dest_with(&s3, "", false);
    let none = AtomicBool::new(false);
    let claimed = d.claim(&dir, "abc", None, &none).unwrap();
    let mut seen = Vec::new();
    d.archive(
        &source(&dir),
        &claimed,
        SystemTime::now(),
        &mut |p| seen.push(p.clone()),
        &none,
    )
    .unwrap();
    let last = seen.last().unwrap();
    assert_eq!(last.percent, 100);
    assert_eq!(last.to_check, Some((0, 6)));
    assert_eq!(last.files_transferred, Some(6));
    assert!(seen.len() > 6);
    assert_eq!(
        d.client().list("S/", &none).unwrap().len(),
        7,
        "paged list: 5 files, snapshot, marker"
    );
    // Second run: everything listed and cached, nothing to do.
    let mut seen = Vec::new();
    d.archive(
        &source(&dir),
        &claimed,
        SystemTime::now(),
        &mut |p| seen.push(p.clone()),
        &none,
    )
    .unwrap();
    assert_eq!(
        seen.last().unwrap().to_check,
        Some((0, 1)),
        "only the snapshot"
    );
}

#[test]
fn unreadable_files_and_links() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("S");
    write(&dir.join("ok.jpg"), b"ok");
    write(&dir.join("locked.jpg"), b"no");
    std::fs::set_permissions(
        dir.join("locked.jpg"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    std::fs::create_dir(dir.join("Private")).unwrap();
    std::fs::set_permissions(dir.join("Private"), std::fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink("ok.jpg", dir.join("link.jpg")).unwrap();
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    let out = run(&d, &source(&dir), "abc");
    std::fs::set_permissions(dir.join("Private"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = out.unwrap();
    assert!(
        out.problems.iter().any(|p| p.starts_with("locked.jpg: ")),
        "{:?}",
        out.problems
    );
    assert!(
        out.problems.iter().any(|p| p.starts_with("Private: ")),
        "{:?}",
        out.problems
    );
    assert!(
        out.warnings.iter().any(|w| w.starts_with("link.jpg: ")),
        "{:?}",
        out.warnings
    );
    assert!(s3.state().object("S/ok.jpg").is_some());
}

#[test]
fn plain_http_only_on_this_network() {
    let with = |endpoint: &str| ObjectStoreConfig {
        endpoint: endpoint.into(),
        ..mock::config("", false)
    };
    // Plain http only for a server on this network.
    for local in [
        "http://localhost:9000",
        "http://10.0.0.5:9000",
        "http://172.16.1.1",
        "http://172.31.255.255",
        "http://192.168.1.20:9000",
        "http://169.254.3.4",
        "http://nas.local:9000",
        "http://nas",
        "http://minio.localhost",
    ] {
        let e = with(local).endpoint();
        assert_eq!(e.map(|e| e.scheme), Ok("http".into()), "{local}");
    }
    for public in [
        "http://s3.example.com",
        "http://8.8.8.8:9000",
        "http://172.32.0.1",
        "http://192.169.1.1",
        "http://10.1",
        "http://nas.local.example.com",
    ] {
        let e = with(public).endpoint().unwrap_err();
        assert!(e.contains("use https://"), "{public}: {e}");
    }
    assert!(
        with("ftp://x")
            .endpoint()
            .unwrap_err()
            .contains("must be an https://")
    );
}

#[test]
fn settings_are_validated() {
    let base = mock::config("", false);
    let with = |f: &dyn Fn(&mut ObjectStoreConfig)| {
        let mut c = base.clone();
        f(&mut c);
        c
    };
    // AWS: the endpoint comes from the region.
    let aws = with(&|c| {
        c.endpoint = String::new();
        c.region = String::new();
    })
    .endpoint()
    .unwrap();
    assert_eq!(aws.host, "s3.us-east-1.amazonaws.com");
    assert_eq!(aws.region, "us-east-1");
    let e = with(&|c| c.endpoint = "HTTPS://Minio.Local:443/".into())
        .endpoint()
        .unwrap();
    assert_eq!(
        (e.scheme.as_str(), e.host.as_str()),
        ("https", "minio.local")
    );
    let e = with(&|c| c.endpoint = "http://127.0.0.1:9000".into())
        .endpoint()
        .unwrap();
    assert_eq!(
        (e.scheme.as_str(), e.host.as_str()),
        ("http", "127.0.0.1:9000")
    );
    let e = with(&|c| c.endpoint = "s3.example.com".into())
        .endpoint()
        .unwrap();
    assert_eq!(e.scheme, "https");
    for bad in [
        with(&|c| c.bucket = "ab".into()),
        with(&|c| c.bucket = "-abc".into()),
        with(&|c| c.bucket = "a b c".into()),
        with(&|c| c.bucket = "my.bucket".into()),
        with(&|c| c.region = "us east".into()),
        with(&|c| c.endpoint = "ftp://x".into()),
        with(&|c| c.endpoint = "https://x/path".into()),
        with(&|c| c.endpoint = "https://".into()),
    ] {
        assert!(bad.endpoint().is_err(), "{bad:?}");
    }
    assert!(
        with(&|c| {
            c.bucket = "my.bucket".into();
            c.path_style = true;
        })
        .endpoint()
        .is_ok()
    );
    assert_eq!(
        with(&|c| c.prefix = "/a//b/".into())
            .normalized_prefix()
            .unwrap(),
        "a/b/"
    );
    assert_eq!(
        with(&|c| c.prefix = "  ".into())
            .normalized_prefix()
            .unwrap(),
        ""
    );
    assert!(
        with(&|c| c.prefix = "a/../b".into())
            .normalized_prefix()
            .is_err()
    );
    // Bad settings or missing keys can't make a destination.
    let s3: Arc<dyn Transport> = Arc::new(MockS3::new());
    for (cfg, creds) in [
        (with(&|c| c.bucket = String::new()), mock::creds()),
        (with(&|c| c.prefix = "..".into()), mock::creds()),
        (
            base.clone(),
            Credentials {
                access_key_id: " ".into(),
                secret_access_key: "s".into(),
            },
        ),
        (
            base.clone(),
            Credentials {
                access_key_id: "a".into(),
                secret_access_key: String::new(),
            },
        ),
    ] {
        assert!(matches!(
            ObjectDestination::new(cfg, s3.clone(), creds),
            Err(ArchiveError::TargetNotUsable { .. })
        ));
    }
    let d =
        ObjectDestination::new(with(&|c| c.endpoint = String::new()), s3, mock::creds()).unwrap();
    assert_eq!(d.location(), "s3://photos/");
    assert_eq!(d.config().bucket, "photos");
}

#[test]
fn large_copies_and_part_sizes() {
    assert_eq!(part_size_for(1), PART_SIZE);
    assert_eq!(
        part_size_for(80 * 1024 * 1024 * 1024),
        PART_SIZE + 1024 * 1024
    );
    assert!(
        part_size_for(5 * 1024 * 1024 * 1024 * 1024) as u64 * MAX_PARTS
            >= 5 * 1024 * 1024 * 1024 * 1024
    );
    let s3 = Arc::new(MockS3::new());
    let d = dest_with(&s3, "", false);
    let head = ObjectHead {
        size: MAX_COPY + 1,
        etag: String::new(),
        blake3: None,
    };
    match d.keep_previous("k", &head, "t", &AtomicBool::new(false)) {
        Err(FileError::Problem(p)) => assert!(p.contains("over 5 GB"), "{p}"),
        _ => panic!("a huge object is never replaced without its copy"),
    }
    // A copy that can't be checked: not replaced either.
    let head = ObjectHead {
        size: 3,
        etag: String::new(),
        blake3: None,
    };
    s3.state().objects.insert(
        "k".into(),
        mock::MockObject {
            data: b"abc".to_vec(),
            meta: BTreeMap::default(),
            etag: String::new(),
        },
    );
    s3.fail("HEAD", "", ".ds-archive-trash", Some(404), 1);
    assert!(matches!(
        d.keep_previous("k", &head, "t", &AtomicBool::new(false)),
        Err(FileError::Problem(_))
    ));
    assert_eq!(hex_bytes("00ff"), [0, 255]);
    assert!(md5_etag("d41d8cd98f00b204e9800998ecf8427e"));
    assert!(!md5_etag("abc-2"));
}

#[test]
fn requests_never_delete_objects_across_every_scenario() {
    // The mock counts DELETEs of objects; the client has no call that sends one.
    let src = include_str!("client.rs");
    let deletes = src.matches("\"DELETE\"").count();
    assert_eq!(deletes, 1, "only abort_multipart sends a DELETE");
    let abort = src.find("pub fn abort_multipart").unwrap();
    let delete = src.find("\"DELETE\"").unwrap();
    assert!(delete > abort && delete - abort < 800);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let s3 = Arc::new(MockS3::new());
    s3.state().hook = Some(Box::new(move |_| {
        c.fetch_add(1, Ordering::Relaxed);
    }));
    let t = tempfile::tempdir().unwrap();
    let dir = session(t.path(), "S");
    let d = dest_with(&s3, "", false);
    run(&d, &source(&dir), "abc").unwrap();
    write(&dir.join(".ds/edits.json"), b"{\"x\":1}");
    run(&d, &source(&dir), "abc").unwrap();
    assert!(calls.load(Ordering::Relaxed) > 5);
    assert_eq!(s3.state().object_deletes(), 0);
}
