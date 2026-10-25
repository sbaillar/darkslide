//! The HTTP seam: one blocking request per call, performed by the app
//! (`URLSession`, through ds-ffi) or by the in-process [`super::mock`] server in
//! tests. A request body is at most one part (see [`super::PART_SIZE`]), so memory
//! stays bounded however large the file.

/// One HTTP request. `url` is fully encoded (see [`super::sigv4::encode_path`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Request {
    /// The `Authorization` header is left out (it names the access key).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
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

    /// The first header called `name` (any case).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
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
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The first header called `name` (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Sends requests. Called on a worker thread, one request at a time; must be
/// thread-safe. `Err` is a transport failure (no answer: offline, DNS, TLS,
/// timeout); any HTTP answer is `Ok`, whatever its status. Redirects must not be
/// followed (a 3xx is an answer).
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> Result<Response, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_case_insensitive_and_debug_hides_auth() {
        let r = Request::new("PUT", "https://h/k")
            .header("Authorization", "AWS4-HMAC-SHA256 Credential=AKID/…")
            .header("x-amz-date", "d");
        assert_eq!(r.header_value("X-AMZ-DATE"), Some("d"));
        assert_eq!(r.header_value("missing"), None);
        assert!(!format!("{r:?}").contains("AKID"));
        let a = Response::new(200, "x").with_header("ETag", "\"e\"");
        assert_eq!(a.header("etag"), Some("\"e\""));
        assert!(a.is_success());
        assert!(!Response::new(301, "").is_success());
    }
}
