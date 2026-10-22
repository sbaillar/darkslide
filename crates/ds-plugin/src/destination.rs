//! The `export_destination` contract (version 1). Specified in v1, run by no plug-in
//! yet: the types say what a destination gets and returns, and the host-side checks
//! (which requests may leave the Mac, where tokens live) are implemented and tested so
//! a native destination (Google Photos, Amazon Photos) can adopt the shape today.
//!
//! - **Online** (Google Photos): `account:<name>` + `network:<host>`. The host signs in
//!   (OAuth 2.0 + PKCE), keeps the tokens in the Keychain under
//!   [`keychain_item`], and attaches them to the destination's requests itself. The
//!   destination builds [`HttpRequest`]s; [`check_request`] decides which may go.
//! - **Local** (Amazon Photos' watched folder): `files.write:<label>`. The destination
//!   asks the host to copy an exported file into the granted folder; the host writes it
//!   atomically and never replaces a file.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::manifest::{AccountSpec, Permission, https_host};

/// The Keychain service every plug-in token is stored under.
pub const KEYCHAIN_SERVICE: &str = "casa.baillargeon.darkslide.plugin";

/// The Keychain account of `plugin`'s `account` (`com.example.gphotos/google`).
#[must_use]
pub fn keychain_item(plugin: &str, account: &str) -> String {
    format!("{plugin}/{account}")
}

/// One exported file handed to a destination (by handle: the destination never gets a
/// path it could write to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredFile {
    pub handle: u32,
    /// The file's name ("DSC01234.jpg"), its format, and its size.
    pub name: String,
    pub format: String,
    pub bytes: u64,
    /// The photo's metadata (title, caption, keywords, capture time), when the plug-in
    /// has `photo_metadata`.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// What a destination answers per file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub handle: u32,
    pub ok: bool,
    /// The service's id and a link to the item, when it has them.
    #[serde(default)]
    pub remote_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Shown in the export report when `ok` is false.
    #[serde(default)]
    pub message: Option<String>,
}

/// The body of a request: none, bytes, or a slice of a delivered file (streamed by
/// the host, so an upload never passes through the plug-in's memory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    None,
    Bytes(Vec<u8>),
    File { handle: u32, offset: u64, len: u64 },
}

/// A request a destination asks the host to make (`http_request`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// The account whose token the host attaches (`Authorization: Bearer …`).
    #[serde(default)]
    pub auth: Option<String>,
}

/// Limits the host applies to every request.
pub const CONNECT_TIMEOUT_S: u64 = 30;
pub const REQUEST_TIMEOUT_S: u64 = 600;
pub const MAX_RESPONSE_BYTES: usize = 4 << 20;

/// Headers only the host sets: a plug-in can't forge auth or redirect where a body goes.
const HOST_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "proxy-authorization",
    "content-length",
];

/// Whether `req` may be sent for a plug-in holding `granted` (and the files handed to
/// this job): HTTPS to a granted host, an account it was granted, a known method, no
/// host-only headers, a body from one of this job's files.
pub fn check_request(
    req: &HttpRequest,
    granted: &BTreeSet<Permission>,
    accounts: &BTreeMap<String, AccountSpec>,
    handles: &[u32],
) -> Result<(), String> {
    const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];
    if !METHODS.contains(&req.method.as_str()) {
        return Err(format!("method `{}` is not allowed", req.method));
    }
    let host = https_host(&req.url)
        .ok_or_else(|| format!("`{}` is not an https:// URL to a host name", req.url))?;
    if !granted.iter().any(|p| p.allows_host(host)) {
        return Err(format!(
            "`{host}` is not one of the plug-in's network permissions"
        ));
    }
    if let Some(a) = &req.auth {
        if !granted.contains(&Permission::Account(a.clone())) {
            return Err(format!("account `{a}` is not granted"));
        }
        // The token goes only to the account's own API hosts, never to another host on
        // the plug-in's network list.
        let spec = accounts.get(a).ok_or_else(|| format!("no account `{a}`"))?;
        if !spec.api_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Err(format!(
                "the {a} sign-in is only sent to {}, not `{host}`",
                spec.api_hosts.join(", ")
            ));
        }
    }
    if let Some((h, _)) = req
        .headers
        .iter()
        .find(|(h, _)| HOST_HEADERS.contains(&h.to_ascii_lowercase().as_str()))
    {
        return Err(format!(
            "the `{h}` header is set by Darkslide, not the plug-in"
        ));
    }
    if let Some((h, _)) = req.headers.iter().find(|(h, v)| {
        h.is_empty() || h.contains([':', ' ', '\r', '\n']) || v.contains(['\r', '\n', '\0'])
    }) {
        return Err(format!(
            "the header `{}` has a bad name or value",
            h.escape_debug()
        ));
    }
    if let Body::File { handle, .. } = req.body
        && !handles.contains(&handle)
    {
        return Err(format!("file {handle} was not handed to this export"));
    }
    Ok(())
}

/// Whether a redirect from a checked request to `location` may be followed: only to a
/// host the plug-in may reach, over HTTPS (the host drops the token on a host change).
#[must_use]
pub fn may_follow_redirect(location: &str, granted: &BTreeSet<Permission>) -> bool {
    https_host(location).is_some_and(|h| granted.iter().any(|p| p.allows_host(h)))
}

/// Whether a followed redirect from `from` to `location` keeps the `Authorization`
/// header: only when the host is the same (any host change drops the token).
#[must_use]
pub fn redirect_keeps_auth(from: &str, location: &str) -> bool {
    matches!((https_host(from), https_host(location)), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted() -> BTreeSet<Permission> {
        [
            "network:photoslibrary.googleapis.com",
            "network:*.example.com",
            "account:google",
        ]
        .iter()
        .map(|p| Permission::parse(p).unwrap())
        .collect()
    }

    fn accts() -> BTreeMap<String, AccountSpec> {
        let spec: AccountSpec = toml::from_str(
            r#"api_hosts = ["photoslibrary.googleapis.com"]
auth = "oauth2-pkce"
authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
client_id = "x""#,
        )
        .unwrap();
        [("google".to_owned(), spec)].into_iter().collect()
    }

    #[test]
    fn tokens_go_only_to_the_accounts_own_hosts() {
        let g = granted();
        let leak = HttpRequest {
            auth: Some("google".into()),
            ..req("https://collector.example.com/steal")
        };
        let e = check_request(&leak, &g, &accts(), &[]).unwrap_err();
        assert!(
            e.contains("only sent to photoslibrary.googleapis.com"),
            "{e}"
        );
        assert!(
            check_request(&leak, &g, &BTreeMap::new(), &[])
                .unwrap_err()
                .contains("no account")
        );
        assert!(redirect_keeps_auth(
            "https://a.example.com/x",
            "https://A.example.com/y"
        ));
        assert!(!redirect_keeps_auth(
            "https://a.example.com/x",
            "https://b.example.com/y"
        ));
        assert!(!redirect_keeps_auth(
            "https://a.example.com/x",
            "http://a.example.com/y"
        ));
    }

    fn req(url: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            url: url.into(),
            headers: vec![],
            body: Body::None,
            auth: None,
        }
    }

    #[test]
    fn requests_go_only_to_granted_https_hosts() {
        let g = granted();
        assert!(
            check_request(
                &req("https://photoslibrary.googleapis.com/v1/uploads"),
                &g,
                &accts(),
                &[]
            )
            .is_ok()
        );
        assert!(check_request(&req("https://up.example.com/x"), &g, &accts(), &[]).is_ok());
        for bad in [
            "http://photoslibrary.googleapis.com/v1",
            "https://evil.com/",
            "https://example.com/", // `*.` needs a subdomain
            "https://user@photoslibrary.googleapis.com/",
            "https://photoslibrary.googleapis.com:8443/",
            "https://10.0.0.1/",
            "ftp://x.example.com/",
        ] {
            assert!(
                check_request(&req(bad), &g, &accts(), &[]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn auth_headers_methods_and_files_are_checked() {
        let g = granted();
        let base = req("https://photoslibrary.googleapis.com/v1/uploads");
        let ok = HttpRequest {
            auth: Some("google".into()),
            body: Body::File {
                handle: 7,
                offset: 0,
                len: 10,
            },
            headers: vec![("X-Goog-Upload-Protocol".into(), "raw".into())],
            ..base.clone()
        };
        assert!(check_request(&ok, &g, &accts(), &[7]).is_ok());
        assert!(
            check_request(&ok, &g, &accts(), &[8])
                .unwrap_err()
                .contains("not handed")
        );
        let other = HttpRequest {
            auth: Some("dropbox".into()),
            ..base.clone()
        };
        assert!(
            check_request(&other, &g, &accts(), &[])
                .unwrap_err()
                .contains("not granted")
        );
        let forged = HttpRequest {
            headers: vec![("Authorization".into(), "Bearer x".into())],
            ..base.clone()
        };
        assert!(
            check_request(&forged, &g, &accts(), &[])
                .unwrap_err()
                .contains("set by Darkslide")
        );
        let method = HttpRequest {
            method: "CONNECT".into(),
            ..base
        };
        assert!(check_request(&method, &g, &accts(), &[]).is_err());
    }

    #[test]
    fn header_names_and_values_cannot_smuggle_lines() {
        let g = granted();
        let base = req("https://photoslibrary.googleapis.com/v1/uploads");
        for (h, v) in [
            ("X-A", "a\r\nHost: evil"),
            ("X\nB", "b"),
            ("", "c"),
            ("X C", "d"),
            ("X:D", "e"),
        ] {
            let r = HttpRequest {
                headers: vec![(h.into(), v.into())],
                ..base.clone()
            };
            assert!(
                check_request(&r, &g, &accts(), &[])
                    .unwrap_err()
                    .contains("bad name or value"),
                "{h:?}"
            );
        }
    }

    #[test]
    fn redirects_stay_inside_the_permissions() {
        let g = granted();
        assert!(may_follow_redirect("https://a.example.com/next", &g));
        assert!(!may_follow_redirect("https://elsewhere.net/", &g));
        assert!(!may_follow_redirect("http://a.example.com/", &g));
    }

    #[test]
    fn keychain_items_are_per_plugin_and_account() {
        assert_eq!(
            keychain_item("com.example.gphotos", "google"),
            "com.example.gphotos/google"
        );
    }

    #[test]
    fn contract_types_round_trip_as_json() {
        let r = DeliveryResult {
            handle: 1,
            ok: true,
            remote_id: Some("abc".into()),
            url: None,
            message: None,
        };
        let back: DeliveryResult =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let f: DeliveredFile =
            serde_json::from_str(r#"{"handle":1,"name":"a.jpg","format":"jpeg","bytes":10}"#)
                .unwrap();
        assert_eq!(f.metadata, None);
        let b: Body = serde_json::from_str(r#"{"file":{"handle":1,"offset":0,"len":5}}"#).unwrap();
        assert_eq!(
            b,
            Body::File {
                handle: 1,
                offset: 0,
                len: 5
            }
        );
    }
}
