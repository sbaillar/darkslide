//! The object-storage destination against a real S3-compatible server (`MinIO`, or any
//! other). Ignored by default: it needs a server. Run it with
//!
//! ```sh
//! DS_S3_ENDPOINT=http://127.0.0.1:9000 DS_S3_ACCESS_KEY=… DS_S3_SECRET_KEY=… \
//!   cargo test -p ds-archive --test minio -- --ignored
//! ```
//!
//! (`DS_S3_BUCKET` defaults to `darkslide-test`, created when missing;
//! `DS_S3_PATH_STYLE=0` switches to virtual-hosted URLs.) Requests go through
//! `/usr/bin/curl` here; the app sends them with `URLSession`.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ds_archive::object::{
    Credentials, ObjectDestination, ObjectStoreConfig, Request, Response, Transport, sigv4,
};
use ds_archive::{Destination, SessionSource};

/// A transport over curl, recording every request's method and query.
#[derive(Default)]
struct Curl {
    sent: Mutex<Vec<(String, String)>>,
    puts: AtomicUsize,
}

impl Transport for Curl {
    fn send(&self, r: Request) -> Result<Response, String> {
        let query = r
            .url
            .split_once('?')
            .map(|(_, q)| q.to_owned())
            .unwrap_or_default();
        self.sent.lock().unwrap().push((r.method.clone(), query));
        if r.method == "PUT" {
            self.puts.fetch_add(1, Ordering::Relaxed);
        }
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let (head, out, body) = (
            dir.path().join("h"),
            dir.path().join("o"),
            dir.path().join("b"),
        );
        std::fs::write(&body, &r.body).map_err(|e| e.to_string())?;
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.args(["-sS", "-H", "Expect:", "-D"])
            .arg(&head)
            .arg("-o")
            .arg(&out);
        if r.method == "HEAD" {
            cmd.arg("--head");
        } else {
            cmd.args(["-X", &r.method]);
            if !r.body.is_empty() || matches!(r.method.as_str(), "PUT" | "POST") {
                cmd.arg("--data-binary").arg(format!("@{}", body.display()));
            }
        }
        for (k, v) in &r.headers {
            cmd.args(["-H", &format!("{k}: {v}")]);
        }
        let done = cmd.arg(&r.url).output().map_err(|e| e.to_string())?;
        if !done.status.success() {
            return Err(String::from_utf8_lossy(&done.stderr).into_owned());
        }
        let text = std::fs::read_to_string(&head).map_err(|e| e.to_string())?;
        // The last response (after any `100 Continue`).
        let last = text
            .split("\r\n\r\n")
            .filter(|b| b.starts_with("HTTP/"))
            .last()
            .ok_or("no response")?;
        let mut lines = last.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or("no status")?;
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        Ok(Response {
            status,
            headers,
            body: std::fs::read(&out).unwrap_or_default(),
        })
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn write(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

#[test]
#[ignore = "needs an S3-compatible server (DS_S3_ENDPOINT, DS_S3_ACCESS_KEY, DS_S3_SECRET_KEY)"]
fn archives_to_a_real_s3_server() {
    let (Some(endpoint), Some(access), Some(secret)) = (
        env("DS_S3_ENDPOINT"),
        env("DS_S3_ACCESS_KEY"),
        env("DS_S3_SECRET_KEY"),
    ) else {
        panic!("set DS_S3_ENDPOINT, DS_S3_ACCESS_KEY and DS_S3_SECRET_KEY");
    };
    let bucket = env("DS_S3_BUCKET").unwrap_or_else(|| "darkslide-test".into());
    let cfg = ObjectStoreConfig {
        endpoint,
        region: env("DS_S3_REGION").unwrap_or_default(),
        bucket,
        prefix: format!("run {}/Été/", std::process::id()),
        path_style: env("DS_S3_PATH_STYLE").is_none_or(|v| v != "0"),
    };
    let creds = Credentials {
        access_key_id: access,
        secret_access_key: secret,
    };
    let curl = Arc::new(Curl::default());
    // The bucket (a test server starts empty).
    let e = cfg.endpoint().unwrap();
    let mut make = Request::new("PUT", e.url("", ""));
    sigv4::sign(
        &mut make,
        &creds,
        &e.region,
        "s3",
        SystemTime::now().into(),
        sigv4::EMPTY_SHA256,
    );
    let made = curl.send(make).unwrap();
    assert!(matches!(made.status, 200 | 409), "create bucket: {made:?}");

    let d = ObjectDestination::new(cfg, curl.clone(), creds).unwrap();
    let none = AtomicBool::new(false);
    let report = d.test_connection(SystemTime::now(), &none).unwrap();
    eprintln!("connection test object: {}", report.test_key);
    assert_eq!(d.check(&none).unwrap().as_deref(), Some("object storage"));

    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Ça va (1)");
    write(&dir.join("Capture/DSC0001.ARW"), &[7u8; 5000]);
    write(&dir.join("Capture/DSC0001.ARW.xmp"), b"<xmp/>");
    write(&dir.join("Developed/DSC0001.jpg"), b"jpeg");
    let big: Vec<u8> = (0..20_000_000u32).map(|i| (i % 253) as u8).collect();
    write(&dir.join("Capture/clip one.MP4"), &big);
    write(&dir.join("x.dsession-wal"), b"wal");
    let src = SessionSource::new(&dir);
    let claimed = d.claim(&dir, "0123456789abcdef", None, &none).unwrap();
    let out = d
        .archive(&src, &claimed, SystemTime::now(), &mut |_| {}, &none)
        .unwrap();
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    let key = |rel: &str| format!("{}{}/{rel}", d.prefix(), claimed.folder);
    let head = d
        .client()
        .head_object(&key("Capture/clip one.MP4"), &none)
        .unwrap()
        .unwrap();
    assert_eq!(head.size, big.len() as u64);
    assert!(head.etag.ends_with("-3"), "multipart ETag {}", head.etag);
    assert_eq!(
        head.blake3.as_deref(),
        Some(blake3::hash(&big).to_hex().as_str())
    );
    assert!(
        d.client()
            .head_object(&key("x.dsession-wal"), &none)
            .unwrap()
            .is_none()
    );

    // Again: nothing to send.
    let puts = curl.puts.load(Ordering::Relaxed);
    d.archive(&src, &claimed, SystemTime::now(), &mut |_| {}, &none)
        .unwrap();
    assert_eq!(curl.puts.load(Ordering::Relaxed), puts);

    // A changed sidecar: the old one is kept in the trash first.
    write(&dir.join("Capture/DSC0001.ARW.xmp"), b"<XMP/>");
    let out = d
        .archive(&src, &claimed, SystemTime::now(), &mut |_| {}, &none)
        .unwrap();
    let trash = format!(
        "{}{}/{}/Capture/DSC0001.ARW.xmp",
        d.prefix(),
        out.backup_dir,
        claimed.folder
    );
    let kept = d.client().head_object(&trash, &none).unwrap().unwrap();
    assert_eq!(kept.size, 6);
    let sent = curl.sent.lock().unwrap();
    assert!(
        sent.iter()
            .all(|(m, q)| m != "DELETE" || q.contains("uploadId=")),
        "nothing is ever deleted"
    );
    eprintln!("{} requests against {}", sent.len(), d.location());
}
