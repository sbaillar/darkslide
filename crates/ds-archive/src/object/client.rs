//! A small blocking S3 client over the [`Transport`] seam: exactly the calls the
//! archive needs. Every request is signed (`SigV4`, real payload hash), retried with
//! exponential backoff on transport failures, 5xx, 429 and `SlowDown`, and checked
//! for cancellation between attempts.
//!
//! **There is no `DeleteObject` here.** The one `DELETE` is
//! [`Client::abort_multipart`], which discards the parts of an upload that never
//! completed (no object, nothing the user had).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use chrono::{DateTime, Utc};

use super::http::{Request, Response, Transport};
use super::sigv4::{self, Credentials};

/// What went wrong talking to the storage service.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObjectError {
    /// No answer (offline, DNS, TLS, timeout), after the retries.
    #[error("the storage service didn't answer: {0}")]
    Network(String),
    /// The keys were refused (401/403): wrong, revoked, or not allowed on this bucket.
    #[error("the storage service refused the access key: {0}")]
    Auth(String),
    /// The Mac's clock is too far from the service's (`SigV4` allows 15 minutes).
    #[error("this Mac's clock is off by more than 15 minutes: {0}")]
    ClockSkew(String),
    #[error("the bucket doesn't exist: {0}")]
    NoSuchBucket(String),
    /// Wrong region, endpoint or addressing style for this bucket.
    #[error("check the storage settings: {0}")]
    Settings(String),
    #[error("the storage service answered {status} {code}: {message}")]
    Http {
        status: u16,
        code: String,
        message: String,
    },
    /// What was stored doesn't match what was sent.
    #[error("the stored copy doesn't match: {0}")]
    Integrity(String),
    /// An answer this client doesn't understand.
    #[error("unexpected answer from the storage service: {0}")]
    Protocol(String),
    #[error("cancelled")]
    Cancelled,
}

/// Where a bucket is and how to address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `https` (or `http` for a local test server).
    pub scheme: String,
    /// `host[:port]`, e.g. `s3.eu-central-1.amazonaws.com`.
    pub host: String,
    pub bucket: String,
    pub region: String,
    /// `https://host/bucket/key` instead of `https://bucket.host/key`.
    pub path_style: bool,
}

/// Whether plain `http://` may be used for `host` (`name[:port]`): only for a
/// server on the local network, such as a `MinIO` on the LAN: a loopback,
/// private (RFC 1918) or link-local address, `localhost`, a `.local` name, or a
/// single-label name. Anything else must use `https://`, since every file and
/// the requests' signatures would otherwise cross the internet in the clear.
pub fn plain_http_allowed(host: &str) -> bool {
    let name = host
        .rsplit_once(':')
        .map_or(host, |(n, _)| n)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if let Ok(ip) = name.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback() || ip.is_private() || ip.is_link_local();
    }
    if name
        .split('.')
        .all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        // Looks like an address but isn't one (`10.1`, `999.0.0.1`).
        return false;
    }
    name == "localhost"
        || !name.contains('.')
        || matches!(name.rsplit_once('.'), Some((_, "local" | "localhost")))
}

/// Why `scheme://host` may not be used, in words; `None` when it may.
pub fn scheme_problem(scheme: &str, host: &str) -> Option<String> {
    match scheme {
        "https" => None,
        "http" if plain_http_allowed(host) => None,
        "http" => Some(format!(
            "http:// is only for a server on this network (localhost, a private address, \
             a .local or single-word name); use https:// for {host}"
        )),
        _ => Some(
            "the endpoint must be an https:// address (http:// only for a server on this \
             network)"
                .into(),
        ),
    }
}

impl Endpoint {
    /// The URL of `key` (`""`: the bucket) with an already-encoded `query`.
    pub fn url(&self, key: &str, query: &str) -> String {
        let key = sigv4::encode_path(key);
        let mut url = if self.path_style {
            let sep = if key.is_empty() { "" } else { "/" };
            format!("{}://{}/{}{sep}{key}", self.scheme, self.host, self.bucket)
        } else {
            format!("{}://{}.{}/{key}", self.scheme, self.bucket, self.host)
        };
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        url
    }
}

/// How often and how patiently a request is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Tries in total (at least 1).
    pub attempts: u32,
    /// Wait before the second try; doubled each time.
    pub base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 4,
            base: Duration::from_millis(500),
        }
    }
}

/// `HeadObject`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectHead {
    pub size: u64,
    pub etag: String,
    /// `x-amz-meta-ds-blake3`: what Darkslide stored (None: not stored by Darkslide).
    pub blake3: Option<String>,
}

/// What [`Client::put_if_absent`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutIfAbsent {
    /// Stored (or the service ignored the condition and stored it anyway).
    Written,
    /// There was an object there already: nothing written.
    Exists,
    /// The service doesn't do conditional writes: nothing written.
    Unsupported,
}

/// User metadata header with the file's BLAKE3.
pub const META_BLAKE3: &str = "x-amz-meta-ds-blake3";

type Sleep = Arc<dyn Fn(Duration) + Send + Sync>;
type Now = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// A signed S3 client for one bucket.
#[derive(Clone)]
pub struct Client {
    transport: Arc<dyn Transport>,
    creds: Credentials,
    endpoint: Endpoint,
    retry: RetryPolicy,
    sleep: Sleep,
    now: Now,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .field("creds", &self.creds)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// Base64 of the body's MD5 (`Content-MD5`), and its hex (to compare with an `ETag`).
pub fn md5(body: &[u8]) -> (String, String) {
    use md5::Digest;
    let d = md5::Md5::digest(body);
    (
        base64::engine::general_purpose::STANDARD.encode(d),
        sigv4::hex(&d),
    )
}

/// An `ETag` without its quotes.
fn unquote(etag: &str) -> &str {
    etag.trim().trim_matches('"')
}

/// The text of the first `<name>` element.
fn xml_text(doc: &roxmltree::Document<'_>, name: &str) -> Option<String> {
    doc.descendants()
        .find(|n| n.has_tag_name(name))
        .map(|n| n.text().unwrap_or_default().to_owned())
}

/// The `<Error>` in a body, if it holds one: `(code, message)`.
fn xml_error(body: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = roxmltree::Document::parse(text).ok()?;
    if !doc.root_element().has_tag_name("Error") {
        return None;
    }
    Some((
        xml_text(&doc, "Code").unwrap_or_default(),
        xml_text(&doc, "Message").unwrap_or_default(),
    ))
}

/// A failed answer as an error, with what the status, error code and headers say.
fn classify(r: &Response, now: SystemTime) -> ObjectError {
    let (code, message) = xml_error(&r.body).unwrap_or_default();
    let message = if message.is_empty() {
        format!("HTTP {}", r.status)
    } else {
        message
    };
    let skewed = r
        .header("Date")
        .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
        .is_some_and(|server| {
            let server = SystemTime::from(server);
            let diff = server
                .duration_since(now)
                .or_else(|_| now.duration_since(server))
                .unwrap_or_default();
            diff > Duration::from_secs(15 * 60)
        });
    if code == "RequestTimeTooSkewed" || (matches!(r.status, 401 | 403) && skewed) {
        return ObjectError::ClockSkew(message);
    }
    if let Some(region) = r.header("x-amz-bucket-region")
        && (matches!(r.status, 301 | 307 | 400) || code == "AuthorizationHeaderMalformed")
    {
        return ObjectError::Settings(format!("the bucket is in region {region}"));
    }
    match (r.status, code.as_str()) {
        (401 | 403, _) => ObjectError::Auth(if code.is_empty() {
            message
        } else {
            format!("{code}: {message}")
        }),
        (_, "NoSuchBucket") => ObjectError::NoSuchBucket(message),
        (301 | 307, _) | (_, "PermanentRedirect" | "AuthorizationHeaderMalformed") => {
            ObjectError::Settings(format!(
                "the bucket answers at another endpoint or region ({message})"
            ))
        }
        _ => ObjectError::Http {
            status: r.status,
            code,
            message,
        },
    }
}

/// Worth another try: the service is busy or had a hiccup.
fn retryable(r: &Response) -> bool {
    if matches!(r.status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    !r.is_success()
        && xml_error(&r.body).is_some_and(|(code, _)| {
            matches!(
                code.as_str(),
                "SlowDown" | "RequestTimeout" | "InternalError" | "ServiceUnavailable"
            )
        })
}

impl Client {
    pub fn new(transport: Arc<dyn Transport>, creds: Credentials, endpoint: Endpoint) -> Self {
        Self {
            transport,
            creds,
            endpoint,
            retry: RetryPolicy::default(),
            sleep: Arc::new(std::thread::sleep),
            now: Arc::new(SystemTime::now),
        }
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Tests: a sleep that doesn't wait.
    #[must_use]
    pub fn with_sleep(mut self, sleep: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.sleep = sleep;
        self
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Wait `d` in slices, returning early (false) when cancelled.
    fn pause(&self, d: Duration, cancel: &AtomicBool) -> bool {
        let slice = Duration::from_millis(100);
        let mut left = d;
        while !left.is_zero() {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let step = left.min(slice);
            (self.sleep)(step);
            left = left.saturating_sub(step);
        }
        !cancel.load(Ordering::Relaxed)
    }

    /// Send one signed request, retrying what is worth retrying. A failed answer that
    /// isn't retried comes back as `Ok` (the caller classifies it).
    fn send(
        &self,
        method: &str,
        key: &str,
        query: &str,
        headers: &[(&str, String)],
        body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Response, ObjectError> {
        if let Some(why) = scheme_problem(&self.endpoint.scheme, &self.endpoint.host) {
            return Err(ObjectError::Settings(why));
        }
        let payload = sigv4::sha256_hex(body);
        let mut last = ObjectError::Network("not sent".into());
        let attempts = self.retry.attempts.max(1);
        for attempt in 0..attempts {
            if cancel.load(Ordering::Relaxed) {
                return Err(ObjectError::Cancelled);
            }
            let mut req = Request::new(method, self.endpoint.url(key, query));
            for (k, v) in headers {
                req.headers.push(((*k).to_owned(), v.clone()));
            }
            req.body = body.to_vec();
            let now: DateTime<Utc> = (self.now)().into();
            sigv4::sign(
                &mut req,
                &self.creds,
                &self.endpoint.region,
                "s3",
                now,
                &payload,
            );
            match self.transport.send(req) {
                Err(e) => last = ObjectError::Network(e),
                Ok(r) if retryable(&r) => last = classify(&r, (self.now)()),
                Ok(r) => return Ok(r),
            }
            if attempt + 1 < attempts {
                let wait = self.retry.base.saturating_mul(1 << attempt.min(16));
                tracing::debug!(attempt, ?wait, error = %last, "object storage: retrying");
                if !self.pause(wait, cancel) {
                    return Err(ObjectError::Cancelled);
                }
            }
        }
        Err(last)
    }

    /// [`Self::send`], with a failed answer as an error.
    fn ok(
        &self,
        method: &str,
        key: &str,
        query: &str,
        headers: &[(&str, String)],
        body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Response, ObjectError> {
        let r = self.send(method, key, query, headers, body, cancel)?;
        if r.is_success() {
            Ok(r)
        } else {
            Err(classify(&r, (self.now)()))
        }
    }

    /// `HeadBucket`: the bucket exists and the keys may use it.
    pub fn head_bucket(&self, cancel: &AtomicBool) -> Result<(), ObjectError> {
        let r = self.send("HEAD", "", "", &[], &[], cancel)?;
        match r.status {
            s if (200..300).contains(&s) => Ok(()),
            404 => Err(ObjectError::NoSuchBucket(self.endpoint.bucket.clone())),
            _ => Err(classify(&r, (self.now)())),
        }
    }

    /// `HeadObject`: `None` when there is no such object.
    pub fn head_object(
        &self,
        key: &str,
        cancel: &AtomicBool,
    ) -> Result<Option<ObjectHead>, ObjectError> {
        let r = self.send("HEAD", key, "", &[], &[], cancel)?;
        if r.status == 404 {
            return Ok(None);
        }
        if !r.is_success() {
            return Err(classify(&r, (self.now)()));
        }
        let size = r
            .header("Content-Length")
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| ObjectError::Protocol(format!("no size for {key}")))?;
        Ok(Some(ObjectHead {
            size,
            etag: unquote(r.header("ETag").unwrap_or_default()).to_owned(),
            blake3: r.header(META_BLAKE3).map(|v| v.trim().to_owned()),
        }))
    }

    /// `GetObject` of a small object (an owner marker): `None` when there is none.
    pub fn get_small(
        &self,
        key: &str,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<u8>>, ObjectError> {
        let r = self.send("GET", key, "", &[], &[], cancel)?;
        match r.status {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(r.body)),
            _ => Err(classify(&r, (self.now)())),
        }
    }

    /// `PutObject` with `Content-MD5` and user metadata; returns the `ETag` (unquoted).
    pub fn put_object(
        &self,
        key: &str,
        body: &[u8],
        meta: &[(&str, String)],
        cancel: &AtomicBool,
    ) -> Result<String, ObjectError> {
        let (md5_b64, _) = md5(body);
        let mut headers = vec![
            ("Content-MD5", md5_b64),
            ("Content-Type", "application/octet-stream".to_owned()),
        ];
        headers.extend(meta.iter().cloned());
        let r = self.ok("PUT", key, "", &headers, body, cancel)?;
        Ok(unquote(r.header("ETag").unwrap_or_default()).to_owned())
    }

    /// `PutObject` with `If-None-Match: *`: written only if there is no object at
    /// `key` yet. A service that doesn't do conditional writes says so with 501
    /// ([`PutIfAbsent::Unsupported`]: nothing was written); some just ignore the
    /// header and write, so the caller reads back what is stored.
    pub fn put_if_absent(
        &self,
        key: &str,
        body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<PutIfAbsent, ObjectError> {
        let headers = [
            ("Content-MD5", md5(body).0),
            ("Content-Type", "application/octet-stream".to_owned()),
            ("If-None-Match", "*".to_owned()),
        ];
        let r = self.send("PUT", key, "", &headers, body, cancel)?;
        match r.status {
            s if (200..300).contains(&s) => Ok(PutIfAbsent::Written),
            // 409: a conditional write to the same key was in flight (AWS).
            409 | 412 => Ok(PutIfAbsent::Exists),
            501 => Ok(PutIfAbsent::Unsupported),
            _ => Err(classify(&r, (self.now)())),
        }
    }

    /// Server-side `CopyObject` (metadata copied). A 200 can still carry an
    /// `<Error>` (S3 sends the status before the copy finishes): that is an error too.
    pub fn copy_object(
        &self,
        from: &str,
        to: &str,
        cancel: &AtomicBool,
    ) -> Result<(), ObjectError> {
        let source = format!("/{}/{}", self.endpoint.bucket, sigv4::encode_path(from));
        let r = self.ok("PUT", to, "", &[("x-amz-copy-source", source)], &[], cancel)?;
        if let Some((code, message)) = xml_error(&r.body) {
            return Err(ObjectError::Http {
                status: r.status,
                code,
                message,
            });
        }
        Ok(())
    }

    /// `CreateMultipartUpload` with user metadata; returns the upload id.
    pub fn create_multipart(
        &self,
        key: &str,
        meta: &[(&str, String)],
        cancel: &AtomicBool,
    ) -> Result<String, ObjectError> {
        let mut headers = vec![("Content-Type", "application/octet-stream".to_owned())];
        headers.extend(meta.iter().cloned());
        let r = self.ok("POST", key, "uploads", &headers, &[], cancel)?;
        let text = String::from_utf8_lossy(&r.body);
        roxmltree::Document::parse(&text)
            .ok()
            .and_then(|d| xml_text(&d, "UploadId"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ObjectError::Protocol("no UploadId".into()))
    }

    /// `UploadPart` with `Content-MD5`; returns the part's `ETag` (unquoted).
    pub fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part: u32,
        body: &[u8],
        cancel: &AtomicBool,
    ) -> Result<String, ObjectError> {
        let (md5_b64, _) = md5(body);
        let query = format!(
            "partNumber={part}&uploadId={}",
            sigv4::encode_component(upload_id)
        );
        let r = self.ok(
            "PUT",
            key,
            &query,
            &[("Content-MD5", md5_b64)],
            body,
            cancel,
        )?;
        let etag = unquote(r.header("ETag").unwrap_or_default()).to_owned();
        if etag.is_empty() {
            return Err(ObjectError::Protocol(format!("no ETag for part {part}")));
        }
        Ok(etag)
    }

    /// `CompleteMultipartUpload`; returns the object's `ETag` (unquoted).
    pub fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[String],
        cancel: &AtomicBool,
    ) -> Result<String, ObjectError> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, etag) in parts.iter().enumerate() {
            let _ = write!(
                xml,
                "<Part><PartNumber>{}</PartNumber><ETag>\"{etag}\"</ETag></Part>",
                i + 1
            );
        }
        xml.push_str("</CompleteMultipartUpload>");
        let query = format!("uploadId={}", sigv4::encode_component(upload_id));
        let r = self.ok(
            "POST",
            key,
            &query,
            &[("Content-Type", "application/xml".to_owned())],
            xml.as_bytes(),
            cancel,
        )?;
        if let Some((code, message)) = xml_error(&r.body) {
            return Err(ObjectError::Http {
                status: r.status,
                code,
                message,
            });
        }
        let text = String::from_utf8_lossy(&r.body);
        Ok(roxmltree::Document::parse(&text)
            .ok()
            .and_then(|d| xml_text(&d, "ETag"))
            .map(|e| unquote(&e).to_owned())
            .unwrap_or_default())
    }

    /// `AbortMultipartUpload`: drops the parts of an upload that never completed. Best
    /// effort, never retried for long, and never cancelled (so a cancelled run still
    /// cleans up). This is the client's only `DELETE`; it never removes an object.
    pub fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), ObjectError> {
        let query = format!("uploadId={}", sigv4::encode_component(upload_id));
        let never = AtomicBool::new(false);
        let r = self.send("DELETE", key, &query, &[], &[], &never)?;
        if r.is_success() || r.status == 404 {
            Ok(())
        } else {
            Err(classify(&r, (self.now)()))
        }
    }

    /// `ListObjectsV2` under `prefix`: every key and its size (all pages).
    pub fn list(
        &self,
        prefix: &str,
        cancel: &AtomicBool,
    ) -> Result<Vec<(String, u64)>, ObjectError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = format!("list-type=2&prefix={}", sigv4::encode_component(prefix));
            if let Some(t) = &token {
                let _ = write!(query, "&continuation-token={}", sigv4::encode_component(t));
            }
            let r = self.ok("GET", "", &query, &[], &[], cancel)?;
            let text = String::from_utf8_lossy(&r.body).into_owned();
            let doc = roxmltree::Document::parse(&text)
                .map_err(|e| ObjectError::Protocol(format!("object list: {e}")))?;
            for c in doc.descendants().filter(|n| n.has_tag_name("Contents")) {
                let field = |name: &str| {
                    c.children()
                        .find(|n| n.has_tag_name(name))
                        .and_then(|n| n.text())
                        .unwrap_or_default()
                };
                out.push((field("Key").to_owned(), field("Size").parse().unwrap_or(0)));
            }
            let truncated = xml_text(&doc, "IsTruncated").is_some_and(|t| t == "true");
            token = xml_text(&doc, "NextContinuationToken").filter(|t| !t.is_empty());
            if !truncated || token.is_none() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &str) -> Response {
        Response::new(status, body)
    }

    #[test]
    fn urls_for_both_addressing_styles() {
        let mut e = Endpoint {
            scheme: "https".into(),
            host: "s3.example.test".into(),
            bucket: "b".into(),
            region: "auto".into(),
            path_style: false,
        };
        assert_eq!(e.url("a b/c", ""), "https://b.s3.example.test/a%20b/c");
        assert_eq!(
            e.url("", "list-type=2"),
            "https://b.s3.example.test/?list-type=2"
        );
        e.path_style = true;
        assert_eq!(e.url("a b/c", "x"), "https://s3.example.test/b/a%20b/c?x");
        assert_eq!(e.url("", ""), "https://s3.example.test/b");
    }

    /// An `Endpoint` built by hand still can't send in the clear to the internet.
    #[test]
    fn plain_http_is_refused_before_sending_off_the_network() {
        struct Never;
        impl Transport for Never {
            fn send(&self, _: Request) -> Result<Response, String> {
                panic!("nothing may be sent")
            }
        }
        let endpoint = Endpoint {
            scheme: "http".into(),
            host: "s3.example.test".into(),
            bucket: "b".into(),
            region: "auto".into(),
            path_style: true,
        };
        let creds = Credentials {
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
        };
        let c = Client::new(Arc::new(Never), creds, endpoint);
        let e = c.head_bucket(&AtomicBool::new(false)).unwrap_err();
        assert!(
            matches!(&e, ObjectError::Settings(m) if m.contains("use https://")),
            "{e}"
        );
        assert!(scheme_problem("ftp", "x").is_some());
        assert!(!plain_http_allowed(""));
        assert!(plain_http_allowed("127.0.0.1:9000"));
    }

    #[test]
    fn answers_are_classified() {
        let now = SystemTime::now();
        let err = |code: &str, msg: &str| {
            format!(
                "<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{msg}</Message></Error>"
            )
        };
        assert!(matches!(
            classify(&resp(403, &err("RequestTimeTooSkewed", "skew")), now),
            ObjectError::ClockSkew(_)
        ));
        // A HEAD has no body: the server's Date says the clock is off.
        let old = resp(403, "").with_header("Date", "Fri, 24 May 2013 00:00:00 GMT");
        assert!(matches!(classify(&old, now), ObjectError::ClockSkew(_)));
        assert_eq!(
            classify(&resp(403, &err("InvalidAccessKeyId", "no such key")), now),
            ObjectError::Auth("InvalidAccessKeyId: no such key".into())
        );
        assert_eq!(
            classify(&resp(401, ""), now),
            ObjectError::Auth("HTTP 401".into())
        );
        assert!(matches!(
            classify(&resp(404, &err("NoSuchBucket", "gone")), now),
            ObjectError::NoSuchBucket(_)
        ));
        let moved = resp(301, "").with_header("x-amz-bucket-region", "eu-west-3");
        assert_eq!(
            classify(&moved, now),
            ObjectError::Settings("the bucket is in region eu-west-3".into())
        );
        assert!(matches!(
            classify(&resp(301, &err("PermanentRedirect", "elsewhere")), now),
            ObjectError::Settings(_)
        ));
        assert_eq!(
            classify(&resp(500, "oops"), now),
            ObjectError::Http {
                status: 500,
                code: String::new(),
                message: "HTTP 500".into()
            }
        );
        assert!(retryable(&resp(503, "")));
        assert!(retryable(&resp(400, &err("RequestTimeout", "slow"))));
        assert!(!retryable(&resp(400, &err("InvalidArgument", "x"))));
        assert!(!retryable(&resp(200, &err("SlowDown", "x"))));
        assert_eq!(xml_error(b"<NotAnError/>"), None);
        assert_eq!(xml_error(b"\xff"), None);
    }

    #[test]
    fn md5_both_ways() {
        let (b64, hex) = md5(b"");
        assert_eq!(b64, "1B2M2Y8AsgTpgAmY7PhCfg==");
        assert_eq!(hex, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(unquote(" \"abc\" "), "abc");
    }
}
