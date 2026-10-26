//! An in-process mock of the S3 calls the archive makes, behind the [`Transport`]
//! seam. It checks what a real service checks: the `SigV4` signature (with its own
//! secret), the clock (15 minutes), `x-amz-content-sha256` against the body and
//! `Content-MD5` (required on uploads, like a strict service). Both URL styles work.
//! Knobs: offline, a missing bucket, a skewed server clock, scripted faults
//! (statuses or no answer), a wrong `ETag`, small list pages, and a hook run on every
//! request (to cancel or to change a file mid-upload). Every request is logged.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, NaiveDateTime, Utc};

use super::ObjectStoreConfig;
use super::client::md5;
use super::http::{Request, Response, Transport};
use super::sigv4::{self, Credentials, Scope};

pub const HOST: &str = "s3.mock.test";
pub const BUCKET: &str = "photos";
pub const REGION: &str = "us-east-1";
pub const ACCESS_KEY: &str = "MOCKACCESSKEYID";
pub const SECRET: &str = "mock/secret+access/key";

/// Settings that reach the mock (`path_style` either way).
pub fn config(prefix: &str, path_style: bool) -> ObjectStoreConfig {
    ObjectStoreConfig {
        endpoint: format!("https://{HOST}"),
        region: REGION.into(),
        bucket: BUCKET.into(),
        prefix: prefix.into(),
        path_style,
    }
}

/// The keys the mock accepts.
pub fn creds() -> Credentials {
    Credentials {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockObject {
    pub data: Vec<u8>,
    /// `x-amz-meta-*` headers, lowercase names.
    pub meta: BTreeMap<String, String>,
    pub etag: String,
}

#[derive(Debug, Clone, Default)]
pub struct MockUpload {
    pub key: String,
    pub meta: BTreeMap<String, String>,
    pub parts: BTreeMap<u32, (Vec<u8>, String)>,
}

/// A request as the mock saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub method: String,
    /// Decoded object key (`""`: the bucket).
    pub key: String,
    pub query: String,
    pub host: String,
    pub status: u16,
}

/// A scripted failure: the next `times` requests matching it fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// `""`: any method.
    pub method: String,
    /// Substring of the query (`partNumber=2`), `""`: any.
    pub query: String,
    /// Substring of the key, `""`: any.
    pub key: String,
    /// `None`: no answer at all (a transport error).
    pub status: Option<u16>,
    pub times: u32,
}

type Hook = Box<dyn FnMut(&Request) + Send>;

/// How the mock treats `If-None-Match: *` on a `PutObject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Conditional {
    /// As S3 does: 412 when the key exists.
    #[default]
    Honoured,
    /// Written as if the header weren't there (some S3-compatible services).
    Ignored,
    /// 501 `NotImplemented`, nothing written.
    NotImplemented,
}

/// Another writer racing for one key: its object appears just before the mock
/// handles a `PUT` of `key` (`after`: just after, replacing what was put), once
/// `skip` earlier `PUT`s of it went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Race {
    pub key: String,
    pub data: Vec<u8>,
    pub after: bool,
    pub skip: u32,
}

#[allow(clippy::struct_excessive_bools)] // independent knobs
pub struct MockState {
    pub offline: bool,
    pub bucket_exists: bool,
    /// Uploads without `Content-MD5` are refused.
    pub require_content_md5: bool,
    /// Single-part uploads answer a wrong `ETag`.
    pub wrong_etag: bool,
    /// The server's clock runs this far ahead of the real one.
    pub clock_offset: i64,
    pub list_page: usize,
    pub faults: Vec<Fault>,
    pub objects: BTreeMap<String, MockObject>,
    pub uploads: BTreeMap<String, MockUpload>,
    pub requests: Vec<Logged>,
    /// Called (with the state unlocked) before each request is handled.
    pub hook: Option<Hook>,
    pub conditional: Conditional,
    pub race: Option<Race>,
    next_upload: u64,
}

impl std::fmt::Debug for MockState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockState")
            .field("objects", &self.objects.keys().collect::<Vec<_>>())
            .field("uploads", &self.uploads.len())
            .field("requests", &self.requests.len())
            .finish_non_exhaustive()
    }
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            offline: false,
            bucket_exists: true,
            require_content_md5: true,
            wrong_etag: false,
            clock_offset: 0,
            list_page: 1000,
            faults: Vec::new(),
            objects: BTreeMap::new(),
            uploads: BTreeMap::new(),
            requests: Vec::new(),
            hook: None,
            conditional: Conditional::Honoured,
            race: None,
            next_upload: 0,
        }
    }
}

impl MockState {
    /// Requests with this method (and, if not empty, a query containing `query`).
    pub fn count(&self, method: &str, query: &str) -> usize {
        self.requests
            .iter()
            .filter(|r| r.method == method && (query.is_empty() || r.query.contains(query)))
            .count()
    }

    /// `DELETE`s of an object (not an upload abort). Must always be zero.
    pub fn object_deletes(&self) -> usize {
        self.requests
            .iter()
            .filter(|r| r.method == "DELETE" && !r.query.contains("uploadId="))
            .count()
    }

    pub fn object(&self, key: &str) -> Option<&MockObject> {
        self.objects.get(key)
    }
}

/// The mock server.
#[derive(Debug, Default)]
pub struct MockS3 {
    state: Mutex<MockState>,
}

fn http_date(t: SystemTime) -> String {
    DateTime::<Utc>::from(t)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn error(status: u16, code: &str, message: &str) -> Response {
    Response::new(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{}</Message></Error>",
            xml_escape(message)
        ),
    )
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn query_map(q: &str) -> BTreeMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn meta_of(req: &Request) -> BTreeMap<String, String> {
    req.headers
        .iter()
        .filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-amz-meta-"))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect()
}

impl MockS3 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fail the next `times` requests matching `method`/`query`/`key` (`None`: no answer).
    pub fn fail(&self, method: &str, query: &str, key: &str, status: Option<u16>, times: u32) {
        self.state().faults.push(Fault {
            method: method.into(),
            query: query.into(),
            key: key.into(),
            status,
            times,
        });
    }

    fn now(&self) -> SystemTime {
        let off = self.state().clock_offset;
        let d = Duration::from_secs(off.unsigned_abs());
        if off >= 0 {
            SystemTime::now() + d
        } else {
            SystemTime::now() - d
        }
    }

    /// Signature, clock and payload checks; `Some(answer)` when the request is refused.
    fn authenticate(&self, req: &Request) -> Option<Response> {
        let now = self.now();
        let deny =
            |code: &str, msg: &str| Some(error(403, code, msg).with_header("Date", http_date(now)));
        let Some(auth) = req
            .header_value("Authorization")
            .and_then(sigv4::parse_authorization)
        else {
            return deny("AccessDenied", "no signature");
        };
        if auth.access_key_id != ACCESS_KEY {
            return deny(
                "InvalidAccessKeyId",
                "The AWS Access Key Id you provided does not exist in our records.",
            );
        }
        let amz = req
            .header_value("x-amz-date")
            .unwrap_or_default()
            .to_owned();
        let Ok(when) = NaiveDateTime::parse_from_str(&amz, "%Y%m%dT%H%M%SZ") else {
            return deny("AccessDenied", "bad x-amz-date");
        };
        let when = SystemTime::from(when.and_utc());
        let skew = now
            .duration_since(when)
            .or_else(|_| when.duration_since(now))
            .unwrap_or_default();
        if skew > Duration::from_secs(15 * 60) {
            return deny(
                "RequestTimeTooSkewed",
                "The difference between the request time and the current time is too large.",
            );
        }
        let payload = req.header_value("x-amz-content-sha256").unwrap_or_default();
        let scope = Scope {
            region: &auth.region,
            service: &auth.service,
            amz_date: &amz,
        };
        if !auth.signed.iter().any(|h| h == "x-amz-content-sha256")
            || sigv4::signature(req, &auth.signed, payload, SECRET, scope) != auth.signature
            || auth.region != REGION
        {
            return deny(
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match.",
            );
        }
        if payload != sigv4::sha256_hex(&req.body) {
            return Some(error(
                400,
                "XAmzContentSHA256Mismatch",
                "payload hash mismatch",
            ));
        }
        if let Some(m) = req.header_value("Content-MD5")
            && m != md5(&req.body).0
        {
            return Some(error(
                400,
                "BadDigest",
                "The Content-MD5 you specified did not match.",
            ));
        }
        None
    }

    #[allow(clippy::too_many_lines)] // one dispatcher over every call the archive makes
    fn handle(&self, req: &Request, bucket: &str, key: &str, query: &str) -> Response {
        if let Some(denied) = self.authenticate(req) {
            return denied;
        }
        let mut s = self.state();
        if bucket != BUCKET || !s.bucket_exists {
            return if req.method == "HEAD" {
                Response::new(404, "")
            } else {
                error(404, "NoSuchBucket", "The specified bucket does not exist")
            };
        }
        let q = query_map(query);
        let needs_md5 = s.require_content_md5 && req.header_value("Content-MD5").is_none();
        match (req.method.as_str(), key.is_empty()) {
            ("HEAD", true) => Response::new(200, ""),
            ("GET", true) if q.get("list-type").map(String::as_str) == Some("2") => {
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let after = q.get("continuation-token").cloned().unwrap_or_default();
                let keys: Vec<(&String, &MockObject)> = s
                    .objects
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix) && (after.is_empty() || **k > after))
                    .collect();
                let page = &keys[..keys.len().min(s.list_page)];
                let truncated = keys.len() > page.len();
                let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
                for (k, o) in page {
                    let _ = write!(
                        xml,
                        "<Contents><Key>{}</Key><Size>{}</Size><ETag>\"{}\"</ETag></Contents>",
                        xml_escape(k),
                        o.data.len(),
                        o.etag
                    );
                }
                let _ = write!(xml, "<IsTruncated>{truncated}</IsTruncated>");
                if truncated && let Some((k, _)) = page.last() {
                    let _ = write!(
                        xml,
                        "<NextContinuationToken>{}</NextContinuationToken>",
                        xml_escape(k)
                    );
                }
                xml.push_str("</ListBucketResult>");
                Response::new(200, xml)
            }
            ("HEAD", false) => match s.objects.get(key) {
                Some(o) => {
                    let mut r = Response::new(200, "")
                        .with_header("Content-Length", o.data.len().to_string())
                        .with_header("ETag", format!("\"{}\"", o.etag));
                    for (k, v) in &o.meta {
                        r = r.with_header(k, v.clone());
                    }
                    r
                }
                None => Response::new(404, ""),
            },
            ("GET", false) => match s.objects.get(key) {
                Some(o) => Response::new(200, o.data.clone()),
                None => error(404, "NoSuchKey", "The specified key does not exist."),
            },
            ("PUT", false) if req.header_value("x-amz-copy-source").is_some() => {
                let src = decode(req.header_value("x-amz-copy-source").unwrap_or_default());
                let src = src
                    .strip_prefix(&format!("/{BUCKET}/"))
                    .unwrap_or(&src)
                    .to_owned();
                match s.objects.get(&src).cloned() {
                    Some(o) => {
                        let etag = o.etag.clone();
                        s.objects.insert(key.to_owned(), o);
                        Response::new(
                            200,
                            format!("<CopyObjectResult><ETag>\"{etag}\"</ETag></CopyObjectResult>"),
                        )
                    }
                    None => error(404, "NoSuchKey", "The specified key does not exist."),
                }
            }
            ("PUT", false) if q.contains_key("uploadId") => {
                if needs_md5 {
                    return error(400, "InvalidRequest", "Content-MD5 required");
                }
                let n: u32 = q
                    .get("partNumber")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let Some(u) = s.uploads.get_mut(&q["uploadId"]) else {
                    return error(404, "NoSuchUpload", "no such upload");
                };
                let etag = md5(&req.body).1;
                u.parts.insert(n, (req.body.clone(), etag.clone()));
                Response::new(200, "").with_header("ETag", format!("\"{etag}\""))
            }
            ("PUT", false) => {
                if needs_md5 {
                    return error(400, "InvalidRequest", "Content-MD5 required");
                }
                if req.header_value("If-None-Match") == Some("*") {
                    match s.conditional {
                        Conditional::Honoured if s.objects.contains_key(key) => {
                            return error(
                                412,
                                "PreconditionFailed",
                                "At least one of the pre-conditions you specified did not hold",
                            );
                        }
                        Conditional::NotImplemented => {
                            return error(
                                501,
                                "NotImplemented",
                                "A header you provided implies functionality that is not implemented",
                            );
                        }
                        _ => {}
                    }
                }
                let etag = md5(&req.body).1;
                s.objects.insert(
                    key.to_owned(),
                    MockObject {
                        data: req.body.clone(),
                        meta: meta_of(req),
                        etag: etag.clone(),
                    },
                );
                let etag = if s.wrong_etag { "0".repeat(32) } else { etag };
                Response::new(200, "").with_header("ETag", format!("\"{etag}\""))
            }
            ("POST", false) if q.contains_key("uploads") => {
                s.next_upload += 1;
                let id = format!("upload-{}", s.next_upload);
                s.uploads.insert(
                    id.clone(),
                    MockUpload {
                        key: key.to_owned(),
                        meta: meta_of(req),
                        parts: BTreeMap::new(),
                    },
                );
                Response::new(
                    200,
                    format!(
                        "<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                    ),
                )
            }
            ("POST", false) if q.contains_key("uploadId") => {
                let Some(u) = s.uploads.remove(&q["uploadId"]) else {
                    return error(404, "NoSuchUpload", "no such upload");
                };
                let text = String::from_utf8_lossy(&req.body).into_owned();
                let Ok(doc) = roxmltree::Document::parse(&text) else {
                    return error(400, "MalformedXML", "bad XML");
                };
                let mut data = Vec::new();
                let mut md5s = Vec::new();
                let mut n = 0;
                for part in doc.descendants().filter(|n| n.has_tag_name("Part")) {
                    let field = |name: &str| {
                        part.children()
                            .find(|c| c.has_tag_name(name))
                            .and_then(|c| c.text())
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_owned()
                    };
                    let number: u32 = field("PartNumber").parse().unwrap_or(0);
                    match u.parts.get(&number) {
                        Some((bytes, etag)) if *etag == field("ETag") => {
                            data.extend_from_slice(bytes);
                            md5s.extend_from_slice(&super::hex_bytes(etag));
                            n += 1;
                        }
                        _ => return error(400, "InvalidPart", "part missing or ETag wrong"),
                    }
                }
                let etag = format!("{}-{n}", md5(&md5s).1);
                s.objects.insert(
                    u.key.clone(),
                    MockObject {
                        data,
                        meta: u.meta,
                        etag: etag.clone(),
                    },
                );
                Response::new(
                    200,
                    format!(
                        "<CompleteMultipartUploadResult><ETag>\"{etag}\"</ETag></CompleteMultipartUploadResult>"
                    ),
                )
            }
            ("DELETE", false) if q.contains_key("uploadId") => {
                s.uploads.remove(&q["uploadId"]);
                Response::new(204, "")
            }
            ("DELETE", false) => {
                // Logged (and counted by `object_deletes`), never expected.
                s.objects.remove(key);
                Response::new(204, "")
            }
            _ => error(405, "MethodNotAllowed", "not supported by the mock"),
        }
    }
}

impl Transport for MockS3 {
    fn send(&self, req: Request) -> Result<Response, String> {
        let hook = self.state().hook.take();
        if let Some(mut hook) = hook {
            hook(&req);
            let mut s = self.state();
            if s.hook.is_none() {
                s.hook = Some(hook);
            }
        }
        let (host, path, query) =
            sigv4::split_url(&req.url).ok_or_else(|| format!("bad URL {}", req.url))?;
        let (host, path, query) = (host.to_owned(), path.to_owned(), query.to_owned());
        let (bucket, key) = if host == HOST {
            let p = path.trim_start_matches('/');
            let (b, k) = p.split_once('/').unwrap_or((p, ""));
            (b.to_owned(), decode(k))
        } else if let Some(b) = host.strip_suffix(&format!(".{HOST}")) {
            (b.to_owned(), decode(path.trim_start_matches('/')))
        } else {
            return Err(format!("unknown host {host}"));
        };
        let log = |status: u16| Logged {
            method: req.method.clone(),
            key: key.clone(),
            query: query.clone(),
            host: host.clone(),
            status,
        };
        {
            let mut s = self.state();
            if s.offline {
                s.requests.push(log(0));
                return Err("The Internet connection appears to be offline.".into());
            }
            let fault = s.faults.iter_mut().find(|f| {
                f.times > 0
                    && (f.method.is_empty() || f.method == req.method)
                    && query.contains(&f.query)
                    && key.contains(&f.key)
            });
            if let Some(f) = fault {
                f.times -= 1;
                let status = f.status;
                s.requests.push(log(status.unwrap_or(0)));
                return match status {
                    None => Err("The network connection was lost.".into()),
                    Some(503) => Ok(error(503, "SlowDown", "Please reduce your request rate.")),
                    Some(st) if st >= 500 => Ok(error(
                        st,
                        "InternalError",
                        "We encountered an internal error.",
                    )),
                    Some(st) => Ok(error(st, "MockFault", "A scripted failure.")),
                };
            }
        }
        let race = {
            let mut s = self.state();
            match s.race.as_mut() {
                Some(r) if req.method == "PUT" && r.key == key && r.skip > 0 => {
                    r.skip -= 1;
                    None
                }
                Some(r) if req.method == "PUT" && r.key == key => s.race.take(),
                _ => None,
            }
        };
        let land = |r: &Race| {
            self.state().objects.insert(
                r.key.clone(),
                MockObject {
                    data: r.data.clone(),
                    meta: BTreeMap::new(),
                    etag: md5(&r.data).1,
                },
            );
        };
        if let Some(r) = race.as_ref().filter(|r| !r.after) {
            land(r);
        }
        let r = self.handle(&req, &bucket, &key, &query);
        if let Some(r) = race.as_ref().filter(|r| r.after) {
            land(r);
        }
        self.state().requests.push(log(r.status));
        Ok(r)
    }
}
