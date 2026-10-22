use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::client::tests::{RecordingSleeper, creds, jpeg};
use crate::client::{API, Client};
use crate::mock::MockGoogle;

fn files(dir: &Path, n: usize) -> Vec<UploadFile> {
    (0..n)
        .map(|i| UploadFile {
            path: jpeg(dir, &format!("IMG_{i:04}.jpg"), 40 + i),
            description: (i % 2 == 0).then(|| format!("  Photo {i}  ")),
        })
        .collect()
}

#[test]
fn uploads_into_a_new_album_with_descriptions() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 3);
    let mut c = Client::new(&g, creds());
    let mut seen = Vec::new();
    let r = upload(
        &mut c,
        &f,
        &AlbumChoice::Named("Iceland".into()),
        &mut |p| seen.push(p),
    )
    .unwrap();
    assert_eq!(r.uploaded.len(), 3);
    assert!(r.failed.is_empty() && !r.cancelled);
    assert_eq!(r.album.as_ref().unwrap().title, "Iceland");
    let media = g.media();
    assert_eq!(media.len(), 3);
    assert!(
        media
            .iter()
            .all(|m| m.album_id == r.album.as_ref().map(|a| a.id.clone()))
    );
    assert_eq!(media[0].description.as_deref(), Some("Photo 0"));
    assert_eq!(media[1].description, None);
    assert_eq!(media[0].file_name, "IMG_0000.jpg");
    assert_eq!(media[2].bytes, std::fs::read(&f[2].path).unwrap());
    // Per-photo progress, then adding, then the end.
    assert_eq!(seen[0].stage, Stage::Album);
    let uploading: Vec<usize> = seen
        .iter()
        .filter(|p| p.stage == Stage::Uploading)
        .map(|p| p.done)
        .collect();
    assert_eq!(uploading, [0, 1, 2]);
    assert_eq!(
        *seen.last().unwrap(),
        Progress {
            stage: Stage::Adding,
            done: 3,
            total: 3
        }
    );
    // A second run reuses the album by title.
    upload(
        &mut c,
        &f[..1],
        &AlbumChoice::Named("Iceland".into()),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(g.state().albums.len(), 1);
}

#[test]
fn more_than_50_files_go_in_batches_of_50() {
    let g = MockGoogle::new();
    let id = g.add_album("Big");
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 120);
    let mut c = Client::new(&g, creds());
    let r = upload(
        &mut c,
        &f,
        &AlbumChoice::Existing {
            id: id.clone(),
            title: "Big".into(),
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(r.uploaded.len(), 120);
    let batches: Vec<usize> = g
        .requests_to(&format!("{API}/mediaItems:batchCreate"))
        .iter()
        .map(|r| {
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(v["albumId"], id.as_str());
            v["newMediaItems"].as_array().unwrap().len()
        })
        .collect();
    assert_eq!(batches, [50, 50, 20]);
    // Order is kept.
    assert_eq!(r.uploaded[119].0, f[119].path);
    assert!(
        g.requests_to(&format!("{API}/albums")).is_empty(),
        "existing album isn't looked up"
    );
}

#[test]
fn partial_batch_failure_is_reported_per_file() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let mut f = files(dir.path(), 4);
    f.push(UploadFile {
        path: jpeg(dir.path(), "scan.tif", 10),
        description: None,
    });
    g.state().fail_file_names.insert("IMG_0002.jpg".into());
    let mut c = Client::new(&g, creds());
    let r = upload(&mut c, &f, &AlbumChoice::Named("Mixed".into()), &mut |_| {}).unwrap();
    assert_eq!(r.uploaded.len(), 3);
    let failed: Vec<_> = r
        .failed
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(failed, ["scan.tif", "IMG_0002.jpg"]);
    assert!(r.failed[0].1.contains("JPEG and HEIC"));
}

#[test]
fn rate_limited_then_expired_token_still_uploads_everything() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 3);
    g.state().rate_limit = 1;
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    let mut n = 0;
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |p| {
        if p.stage == Stage::Uploading && p.done == 1 && n == 0 {
            n += 1;
            g.expire_access_tokens();
        }
    })
    .unwrap();
    assert_eq!(r.uploaded.len(), 3, "{:?}", r.failed);
    assert_eq!(s.waits()[0].as_secs(), 30);
    assert_eq!(g.requests_to(crate::oauth::TOKEN_ENDPOINT).len(), 2);
}

#[test]
fn sign_in_lost_mid_job_fails_the_rest() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 4);
    let mut c = Client::new(&g, creds());
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |p| {
        if p.stage == Stage::Uploading && p.done == 2 {
            // Revoked elsewhere, and the access token expires.
            let mut s = g.state();
            s.refresh_tokens.clear();
            s.access_tokens.clear();
        }
    })
    .unwrap();
    assert!(r.uploaded.is_empty(), "the 2 sent couldn't be added either");
    assert_eq!(r.failed.len(), 4);
    assert!(
        r.failed.iter().all(|(_, why)| why.contains("sign-in")),
        "{:?}",
        r.failed
    );
    assert_eq!(g.state().sessions.len(), 2, "no more files sent");
}

#[test]
fn album_failure_fails_the_job() {
    let g = MockGoogle::new();
    g.state().refresh_tokens.clear();
    let mut c = Client::new(&g, creds());
    let e = upload(&mut c, &[], &AlbumChoice::Named("A".into()), &mut |_| {}).unwrap_err();
    assert!(matches!(e, GphotosError::Auth(_)));
}

#[test]
fn cancel_keeps_what_was_added() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 55);
    let flag = AtomicBool::new(false);
    let cancelled = || flag.load(Ordering::Relaxed);
    let mut c = Client::new(&g, creds()).with_cancel(&cancelled);
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |p| {
        if p.stage == Stage::Uploading && p.done == 52 {
            flag.store(true, Ordering::Relaxed);
        }
    })
    .unwrap();
    assert!(r.cancelled);
    assert_eq!(r.uploaded.len(), 50, "the first batch was added");
    assert_eq!(r.failed.len(), 2, "sent but not added");
    assert!(r.failed.iter().all(|(_, w)| w.contains("cancelled")));
    assert_eq!(g.media().len(), 50);
}

#[test]
fn cancel_during_the_batch_call() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 3);
    let flag = AtomicBool::new(false);
    let cancelled = || flag.load(Ordering::Relaxed);
    let mut c = Client::new(&g, creds()).with_cancel(&cancelled);
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |p| {
        if p.stage == Stage::Adding && p.done == 0 {
            flag.store(true, Ordering::Relaxed);
        }
    })
    .unwrap();
    assert!(r.cancelled);
    assert_eq!(r.uploaded.len(), 0);
    assert_eq!(r.failed.len(), 3);
}

#[test]
fn whole_batch_refused_fails_its_files_and_goes_on() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 2);
    g.state().batch_status = Some(400);
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |_| {}).unwrap();
    assert_eq!(r.failed.len(), 2);
    assert!(r.failed[0].1.contains("batchCreate refused"));
    // A batch that is rate limited to the end stops the job.
    g.state().batch_status = Some(429);
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |_| {}).unwrap();
    assert_eq!(r.failed.len(), 2);
    assert!(r.failed[0].1.contains("rate limiting"));
}

#[test]
fn rate_limited_upload_stops_and_adds_what_was_sent() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    let dir = tempfile::tempdir().unwrap();
    let f = files(dir.path(), 3);
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    let r = upload(&mut c, &f, &AlbumChoice::Named("A".into()), &mut |p| {
        if p.stage == Stage::Uploading && p.done == 1 {
            g.state().rate_limit = 6;
        }
    })
    .unwrap();
    // File 2 couldn't start (6 × 429); file 1 was still added, file 3 never tried.
    assert_eq!(r.uploaded.len(), 1);
    assert_eq!(r.failed.len(), 2);
    assert!(r.failed.iter().all(|(_, w)| w.contains("rate limiting")));
}

#[test]
fn descriptions_are_trimmed_and_capped() {
    assert_eq!(description(None), None);
    assert_eq!(description(Some("   ")), None);
    assert_eq!(description(Some(" a ")).as_deref(), Some("a"));
    let long = "é".repeat(1500);
    assert_eq!(description(Some(&long)).unwrap().chars().count(), 1000);
}
