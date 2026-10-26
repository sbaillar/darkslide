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

    /// A JSON body with its content type.
    #[must_use]
    pub fn json(mut self, value: &serde_json::Value) -> Self {
        self.body = value.to_string().into_bytes();
        self.header("Content-Type", "application/json")
    }

    /// The first header called `name` (any case).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Immich's error message (`{"message": "…"}`, or a list of validation
    /// messages), else the body, else the status.
    pub fn error_message(&self) -> String {
        let parsed: Option<serde_json::Value> = serde_json::from_slice(&self.body).ok();
        let from_json = parsed
            .as_ref()
            .and_then(|v| match v.get("message")? {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(a) => Some(
                    a.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                _ => None,
            })
            .filter(|m| !m.is_empty());
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

/// Sends requests. Called on a worker thread; must be thread-safe. `Err` is a
/// transport failure (no answer: offline, DNS, timeout).
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> Result<Response, String>;
}

/// RFC 3986 unreserved characters stay as they are.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one URL path segment or query component.
pub fn encode(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// Decode a percent-encoded component.
pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_segments() {
        assert_eq!(encode("a b/c?d=é~"), "a%20b%2Fc%3Fd%3D%C3%A9~");
        assert_eq!(decode("a%20b%2Fc"), "a b/c");
        assert_eq!(decode(&encode("ünï/cødé")), "ünï/cødé");
    }

    #[test]
    fn requests_and_headers() {
        let r = Request::new("PUT", "https://x")
            .header("x-api-key", "k")
            .json(&serde_json::json!({"ids": ["a"]}));
        assert_eq!(r.header_value("X-API-KEY"), Some("k"));
        assert_eq!(r.header_value("content-type"), Some("application/json"));
        assert_eq!(r.body, br#"{"ids":["a"]}"#);
        assert_eq!(r.header_value("missing"), None);
    }

    #[test]
    fn immich_error_messages() {
        let one = Response::new(
            401,
            r#"{"message":"Invalid API key","error":"Unauthorized","statusCode":401}"#,
        );
        assert_eq!(one.error_message(), "Invalid API key");
        let many = Response::new(
            400,
            r#"{"message":["albumName should not be empty","ids must be an array"],"error":"Bad Request","statusCode":400}"#,
        );
        assert_eq!(
            many.error_message(),
            "albumName should not be empty; ids must be an array"
        );
        // Neither form: the body, else the status.
        assert_eq!(
            Response::new(400, r#"{"message":7}"#).error_message(),
            r#"{"message":7}"#
        );
        assert_eq!(
            Response::new(400, r#"{"message":[]}"#).error_message(),
            r#"{"message":[]}"#
        );
        assert_eq!(Response::new(502, "  ").error_message(), "HTTP 502");
        assert_eq!(Response::new(500, "boom").error_message(), "boom");
        assert_eq!(
            Response::new(500, "x".repeat(400)).error_message().len(),
            300
        );
        assert!(Response::new(204, "").is_success());
        assert!(!Response::new(301, "").is_success());
    }
}
