//! The HTTP seam: one blocking call per request, implemented by the app
//! (`URLSession`) or by the [`crate::mock`] server in tests.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// One HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_owned(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// A JSON body with its content type.
    #[must_use]
    pub fn json(self, value: &serde_json::Value) -> Self {
        self.header("Content-Type", "application/json")
            .body(value.to_string().into_bytes())
    }

    /// An `application/x-www-form-urlencoded` body.
    #[must_use]
    pub fn form(self, fields: &[(&str, &str)]) -> Self {
        self.header("Content-Type", "application/x-www-form-urlencoded")
            .body(encode_pairs(fields).into_bytes())
    }

    /// The first header called `name` (any case).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        find(&self.headers, name)
    }
}

/// One HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// The first header called `name` (any case).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        find(&self.headers, name)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Google's error message (`{"error": {"message": …}}` or OAuth's
    /// `{"error": "invalid_grant", "error_description": …}`), else the body.
    pub fn error_message(&self) -> String {
        let parsed: Option<serde_json::Value> = serde_json::from_slice(&self.body).ok();
        let from_json = parsed.as_ref().and_then(|v| {
            let e = v.get("error")?;
            if let Some(m) = e.get("message").and_then(|m| m.as_str()) {
                return Some(m.to_owned());
            }
            let code = e.as_str()?;
            Some(match v.get("error_description").and_then(|d| d.as_str()) {
                Some(d) => format!("{code}: {d}"),
                None => code.to_owned(),
            })
        });
        from_json.unwrap_or_else(|| {
            let t = self.text();
            let t = t.trim();
            if t.is_empty() {
                format!("HTTP {}", self.status)
            } else {
                t.chars().take(300).collect()
            }
        })
    }
}

/// Sends requests. Called on a worker thread, one request at a time per job;
/// must be thread-safe. `Err` is a transport failure (no answer: offline, DNS,
/// timeout), which the client retries.
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> Result<Response, String>;
}

fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// RFC 3986 unreserved characters stay as they are.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one query or form component.
pub fn encode(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// `a=1&b=x%20y`.
pub fn encode_pairs(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Decode a query/form component (`+` is a space).
pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// The `key=value` pairs of a query string (or form body), decoded.
pub fn parse_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_components() {
        assert_eq!(encode("a b/c?d=é~"), "a%20b%2Fc%3Fd%3D%C3%A9~");
        assert_eq!(
            encode_pairs(&[("x", "1 2"), ("redirect_uri", "http://127.0.0.1:5/")]),
            "x=1%202&redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2F"
        );
        assert_eq!(decode("a+b%2Fc"), "a b/c");
        assert_eq!(
            parse_pairs("code=4%2F0A&state=s+1&flag&"),
            vec![
                ("code".into(), "4/0A".into()),
                ("state".into(), "s 1".into()),
                ("flag".into(), String::new()),
            ]
        );
    }

    #[test]
    fn requests_and_headers() {
        let r = Request::new("POST", "https://x")
            .header("X-Goog-Upload-Protocol", "raw")
            .form(&[("a", "b c")]);
        assert_eq!(r.header_value("x-goog-upload-protocol"), Some("raw"));
        assert_eq!(
            r.header_value("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(r.body, b"a=b%20c");
        assert_eq!(r.header_value("missing"), None);
        let j = Request::new("POST", "u").json(&serde_json::json!({"k": 1}));
        assert_eq!(j.body, br#"{"k":1}"#);
    }

    #[test]
    fn error_messages() {
        let google = Response::new(400, r#"{"error":{"code":400,"message":"Invalid album"}}"#);
        assert_eq!(google.error_message(), "Invalid album");
        let oauth = Response::new(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        );
        assert_eq!(
            oauth.error_message(),
            "invalid_grant: Token has been expired or revoked."
        );
        assert_eq!(
            Response::new(400, r#"{"error":"invalid_client"}"#).error_message(),
            "invalid_client"
        );
        assert_eq!(Response::new(502, "  ").error_message(), "HTTP 502");
        assert_eq!(Response::new(500, "boom").error_message(), "boom");
        assert!(Response::new(204, "").is_success());
        assert!(!Response::new(301, "").is_success());
        let h = Response::new(200, "").header("X-Goog-Upload-URL", "u");
        assert_eq!(h.header_value("x-goog-upload-url"), Some("u"));
    }
}
