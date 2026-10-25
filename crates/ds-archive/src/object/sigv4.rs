//! AWS Signature Version 4 (header auth), hand-written over `RustCrypto`'s `sha2` and
//! `hmac` and checked against AWS's published examples (see the tests). S3 rules:
//! the URI path is encoded once and never normalised, and every request carries
//! `x-amz-content-sha256` (here always the real payload hash, so the service also
//! checks the body).

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};

use super::http::Request;

/// SHA-256 of an empty body.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// An access key pair. The secret never appears in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length.
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// RFC 3986 unreserved characters stay; everything else is percent-encoded.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
/// The same, keeping `/` (an object key's path separators).
const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

/// An object key (or a bucket and key) as a URI path, encoded once as S3 expects.
pub fn encode_path(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

/// A query name or value (`/` encoded too).
pub fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

/// `(host[:port], path, query)` of an `http(s)://` URL; `None` for anything else.
pub fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (host, rest) = rest.split_at(host_end);
    if host.is_empty() {
        return None;
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some((host, if path.is_empty() { "/" } else { path }, query))
}

/// The query's pairs sorted by name, then value (the URL's components are encoded
/// already); a bare name gets `=`.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// A header value for signing: trimmed, inner runs of spaces collapsed.
fn canonical_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a signature covers, besides the secret.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'`.
    pub amz_date: &'a str,
}

/// The canonical request over the `signed` headers (lowercase names; `host` comes
/// from the URL unless the request carries one).
pub fn canonical_request(req: &Request, signed: &[String], payload_hash: &str) -> String {
    let (host, path, query) = split_url(&req.url).unwrap_or(("", "/", ""));
    let mut lines = String::new();
    for name in signed {
        let values: Vec<String> = req
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| canonical_value(v))
            .collect();
        let value = if values.is_empty() && name == "host" {
            host.to_owned()
        } else {
            values.join(",")
        };
        let _ = writeln!(lines, "{name}:{value}");
    }
    format!(
        "{}\n{path}\n{}\n{lines}\n{}\n{payload_hash}",
        req.method,
        canonical_query(query),
        signed.join(";")
    )
}

/// The hex signature of `req` over `signed` headers.
pub fn signature(
    req: &Request,
    signed: &[String],
    payload_hash: &str,
    secret: &str,
    scope: Scope<'_>,
) -> String {
    let date = scope.amz_date.get(..8).unwrap_or_default();
    let canonical = canonical_request(req, signed, payload_hash);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{date}/{}/{}/aws4_request\n{}",
        scope.amz_date,
        scope.region,
        scope.service,
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, scope.region.as_bytes());
    let k = hmac(&k, scope.service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    hex(&hmac(&k, string_to_sign.as_bytes()))
}

/// `YYYYMMDD'T'HHMMSS'Z'` for `now`.
pub fn amz_date(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Sign `req` in place: adds `x-amz-date`, (for S3) `x-amz-content-sha256`, and
/// `Authorization`, signing `host` and every header the request carries.
pub fn sign(
    req: &mut Request,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: DateTime<Utc>,
    payload_hash: &str,
) {
    let amz = amz_date(now);
    req.headers.push(("x-amz-date".into(), amz.clone()));
    if service == "s3" && req.header_value("x-amz-content-sha256").is_none() {
        req.headers
            .push(("x-amz-content-sha256".into(), payload_hash.to_owned()));
    }
    let mut signed: Vec<String> = req
        .headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .chain(std::iter::once("host".to_owned()))
        .collect();
    signed.sort_unstable();
    signed.dedup();
    let scope = Scope {
        region,
        service,
        amz_date: &amz,
    };
    let sig = signature(req, &signed, payload_hash, &creds.secret_access_key, scope);
    req.headers.push((
        "Authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{region}/{service}/aws4_request, SignedHeaders={}, Signature={sig}",
            creds.access_key_id,
            &amz[..8],
            signed.join(";")
        ),
    ));
}

/// The parts of an `Authorization` header: access key, date, region, service,
/// signed headers, signature (for the mock server's check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub access_key_id: String,
    pub date: String,
    pub region: String,
    pub service: String,
    pub signed: Vec<String>,
    pub signature: String,
}

pub fn parse_authorization(h: &str) -> Option<Parsed> {
    let rest = h.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let mut cred = None;
    let mut signed = None;
    let mut sig = None;
    for part in rest.split(',').map(str::trim) {
        let (k, v) = part.split_once('=')?;
        match k {
            "Credential" => cred = Some(v),
            "SignedHeaders" => signed = Some(v),
            "Signature" => sig = Some(v),
            _ => {}
        }
    }
    let mut c = cred?.split('/');
    let (access_key_id, date, region, service) = (c.next()?, c.next()?, c.next()?, c.next()?);
    Some(Parsed {
        access_key_id: access_key_id.to_owned(),
        date: date.to_owned(),
        region: region.to_owned(),
        service: service.to_owned(),
        signed: signed?.split(';').map(str::to_owned).collect(),
        signature: sig?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn auth(r: &Request) -> &str {
        r.header_value("Authorization").unwrap()
    }

    /// AWS's `SigV4` test suite, `get-vanilla` (service `service`, no payload).
    #[test]
    fn aws_test_suite_get_vanilla() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        };
        let mut r = Request::new("GET", "https://example.amazonaws.com/");
        sign(
            &mut r,
            &creds,
            "us-east-1",
            "service",
            at(2015, 8, 30, 12, 36, 0),
            EMPTY_SHA256,
        );
        assert_eq!(
            auth(&r),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert!(r.header_value("x-amz-content-sha256").is_none());
    }

    fn s3_example() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    /// The S3 `SigV4` documentation's "GET Object" example (with a Range header).
    #[test]
    fn s3_docs_get_object() {
        let mut r = Request::new("GET", "https://examplebucket.s3.amazonaws.com/test.txt")
            .header("Range", "bytes=0-9");
        sign(
            &mut r,
            &s3_example(),
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 0),
            EMPTY_SHA256,
        );
        assert!(auth(&r).ends_with(
            "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        ));
        assert!(
            auth(&r).contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request")
        );
    }

    /// The S3 documentation's "PUT Object" example: a `$` in the key is encoded.
    #[test]
    fn s3_docs_put_object() {
        let body = b"Welcome to Amazon S3.";
        let mut r = Request::new(
            "PUT",
            format!(
                "https://examplebucket.s3.amazonaws.com/{}",
                encode_path("test$file.text")
            ),
        )
        .header("Date", "Fri, 24 May 2013 00:00:00 GMT")
        .header("x-amz-storage-class", "REDUCED_REDUNDANCY");
        let hash = sha256_hex(body);
        assert_eq!(
            hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        sign(
            &mut r,
            &s3_example(),
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 0),
            &hash,
        );
        assert!(auth(&r).ends_with(
            "SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class, \
             Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        ));
    }

    /// "GET Bucket Lifecycle" (a bare query name) and "GET Bucket (List Objects)"
    /// (query pairs sorted).
    #[test]
    fn s3_docs_bucket_queries() {
        let mut r = Request::new("GET", "https://examplebucket.s3.amazonaws.com/?lifecycle");
        sign(
            &mut r,
            &s3_example(),
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 0),
            EMPTY_SHA256,
        );
        assert!(auth(&r).ends_with(
            "Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        ));
        let mut r = Request::new(
            "GET",
            "https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J",
        );
        sign(
            &mut r,
            &s3_example(),
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 0),
            EMPTY_SHA256,
        );
        assert!(auth(&r).ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn urls_queries_and_encoding() {
        assert_eq!(
            split_url("https://h:9000/b/k?x=1"),
            Some(("h:9000", "/b/k", "x=1"))
        );
        assert_eq!(split_url("http://h?uploads"), Some(("h", "/", "uploads")));
        assert_eq!(split_url("ftp://h/x"), None);
        assert_eq!(split_url("https:///x"), None);
        assert_eq!(canonical_query("b=2&a=&uploads&a=1"), "a=&a=1&b=2&uploads=");
        assert_eq!(
            encode_path("darkslide/Ünï cødé/a+b (1).ARW"),
            "darkslide/%C3%9Cn%C3%AF%20c%C3%B8d%C3%A9/a%2Bb%20%281%29.ARW"
        );
        assert_eq!(encode_component("a/b c"), "a%2Fb%20c");
        assert_eq!(canonical_value("  a   b  "), "a b");
        assert_eq!(hex(&[0, 255]), "00ff");
    }

    #[test]
    fn authorization_round_trips_and_secret_stays_hidden() {
        let creds = s3_example();
        assert!(!format!("{creds:?}").contains("EXAMPLEKEY"));
        let mut r = Request::new("PUT", "https://h/b/k").header("Content-MD5", "x");
        sign(
            &mut r,
            &creds,
            "eu-west-1",
            "s3",
            at(2026, 1, 2, 3, 4, 5),
            EMPTY_SHA256,
        );
        let p = parse_authorization(auth(&r)).unwrap();
        assert_eq!(p.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(
            (p.date.as_str(), p.region.as_str()),
            ("20260102", "eu-west-1")
        );
        assert_eq!(p.service, "s3");
        assert_eq!(
            p.signed,
            ["content-md5", "host", "x-amz-content-sha256", "x-amz-date"]
        );
        let scope = Scope {
            region: "eu-west-1",
            service: "s3",
            amz_date: "20260102T030405Z",
        };
        assert_eq!(
            signature(&r, &p.signed, EMPTY_SHA256, &creds.secret_access_key, scope),
            p.signature
        );
        assert_ne!(
            signature(&r, &p.signed, EMPTY_SHA256, "wrong", scope),
            p.signature
        );
        for bad in [
            "Basic x",
            "AWS4-HMAC-SHA256 Credential=a/b/c",
            "AWS4-HMAC-SHA256 nonsense",
            "AWS4-HMAC-SHA256 Credential=a/b/c/d, SignedHeaders=host",
        ] {
            assert_eq!(parse_authorization(bad), None, "{bad}");
        }
    }
}
