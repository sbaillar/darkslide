//! OAuth 2.0 for installed apps (RFC 8252) with PKCE (RFC 7636).
//!
//! Google allows only a loopback redirect (`http://127.0.0.1:<port>/`) for a
//! "Desktop app" client, so the app listens on a loopback port, shows the consent
//! page, and hands the request the browser makes to [`code_from_redirect`]. The
//! PKCE verifier and challenge are made by the app (`CryptoKit`). A Desktop
//! client also has a client secret, which Google says is not treated as secret
//! for installed apps but still expects at the token endpoint.

use serde::Deserialize;

use crate::GphotosError;
use crate::http::{Request, Response, Transport, encode_pairs, parse_pairs};

pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";

/// Upload and create albums.
pub const SCOPE_APPEND: &str = "https://www.googleapis.com/auth/photoslibrary.appendonly";
/// List the albums this app created (for the album picker and reuse by title).
pub const SCOPE_READ_APP: &str =
    "https://www.googleapis.com/auth/photoslibrary.readonly.appcreateddata";
pub const SCOPES: &[&str] = &[SCOPE_APPEND, SCOPE_READ_APP];

/// What the token endpoint returned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Seconds the access token stays valid.
    #[serde(default)]
    pub expires_in: u64,
    /// Present on the code exchange (installed apps always get one).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Space-separated scopes granted (the user may untick some).
    #[serde(default)]
    pub scope: String,
}

/// The consent page to open.
pub fn authorization_url(
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let scope = SCOPES.join(" ");
    format!(
        "{AUTH_ENDPOINT}?{}",
        encode_pairs(&[
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", &scope),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("access_type", "offline"),
        ])
    )
}

/// The authorization code in the redirect the browser sent to the loopback
/// listener (`/?code=…&state=…`, or a full URL). Refuses another `state` (a
/// forged redirect) and reports the user's "Cancel" (`error=access_denied`).
pub fn code_from_redirect(target: &str, expected_state: &str) -> Result<String, GphotosError> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    let pairs = parse_pairs(query);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if get("state") != Some(expected_state) {
        return Err(GphotosError::Auth(
            "the sign-in answer didn't match this request; try again".into(),
        ));
    }
    if let Some(e) = get("error") {
        return Err(GphotosError::Auth(if e == "access_denied" {
            "access was not allowed".into()
        } else {
            e.to_owned()
        }));
    }
    match get("code") {
        Some(c) if !c.is_empty() => Ok(c.to_owned()),
        _ => Err(GphotosError::Protocol(
            "no code in the sign-in answer".into(),
        )),
    }
}

/// Exchange the authorization code (with the PKCE verifier) for tokens.
/// Fails when the user didn't allow uploading.
pub fn exchange_code(
    transport: &dyn Transport,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, GphotosError> {
    let mut fields = vec![
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ];
    if let Some(s) = client_secret.filter(|s| !s.is_empty()) {
        fields.push(("client_secret", s));
    }
    let tokens = token_call(transport, &fields)?;
    if tokens.refresh_token.as_deref().is_none_or(str::is_empty) {
        return Err(GphotosError::Protocol(
            "Google returned no refresh token".into(),
        ));
    }
    if !tokens.scope.is_empty() && !tokens.scope.split(' ').any(|s| s == SCOPE_APPEND) {
        return Err(GphotosError::Auth(
            "Luxtable wasn't allowed to add photos; sign in again and tick that box".into(),
        ));
    }
    Ok(tokens)
}

/// A fresh access token for `refresh_token`.
pub fn refresh(
    transport: &dyn Transport,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<Tokens, GphotosError> {
    let mut fields = vec![
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    if let Some(s) = client_secret.filter(|s| !s.is_empty()) {
        fields.push(("client_secret", s));
    }
    token_call(transport, &fields)
}

/// Revoke `token` (sign out). An already invalid token counts as revoked.
pub fn revoke(transport: &dyn Transport, token: &str) -> Result<(), GphotosError> {
    let r = transport
        .send(Request::new("POST", REVOKE_ENDPOINT).form(&[("token", token)]))
        .map_err(GphotosError::Network)?;
    if r.is_success() || r.status == 400 {
        Ok(())
    } else {
        Err(http_error(&r))
    }
}

fn token_call(transport: &dyn Transport, fields: &[(&str, &str)]) -> Result<Tokens, GphotosError> {
    let r = transport
        .send(Request::new("POST", TOKEN_ENDPOINT).form(fields))
        .map_err(GphotosError::Network)?;
    if r.status == 400 || r.status == 401 {
        return Err(GphotosError::Auth(r.error_message()));
    }
    if !r.is_success() {
        return Err(http_error(&r));
    }
    serde_json::from_slice(&r.body).map_err(|e| GphotosError::Protocol(format!("token: {e}")))
}

pub(crate) fn http_error(r: &Response) -> GphotosError {
    GphotosError::Http {
        status: r.status,
        message: r.error_message(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockGoogle;

    #[test]
    fn authorization_url_has_pkce_and_scopes() {
        let u = authorization_url("id.apps", "http://127.0.0.1:5000/", "CHAL", "st8");
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=id.apps&"));
        let pairs = parse_pairs(u.split_once('?').unwrap().1);
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("redirect_uri"), "http://127.0.0.1:5000/");
        assert_eq!(get("code_challenge"), "CHAL");
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("state"), "st8");
        assert_eq!(get("scope"), format!("{SCOPE_APPEND} {SCOPE_READ_APP}"));
    }

    #[test]
    fn redirect_code_state_and_errors() {
        assert_eq!(
            code_from_redirect("/?state=s&code=4%2F0Ab&scope=x", "s").unwrap(),
            "4/0Ab"
        );
        assert_eq!(
            code_from_redirect("http://127.0.0.1:9/?code=c&state=s#frag", "s").unwrap(),
            "c"
        );
        let forged = code_from_redirect("/?code=c&state=other", "s").unwrap_err();
        assert!(matches!(forged, GphotosError::Auth(_)), "{forged}");
        assert!(matches!(
            code_from_redirect("/favicon.ico", "s"),
            Err(GphotosError::Auth(_))
        ));
        let denied = code_from_redirect("/?error=access_denied&state=s", "s").unwrap_err();
        assert_eq!(
            denied.to_string(),
            "Google sign-in needed: access was not allowed"
        );
        let other = code_from_redirect("/?error=server_error&state=s", "s").unwrap_err();
        assert!(other.to_string().contains("server_error"));
        assert!(matches!(
            code_from_redirect("/?state=s&code=", "s"),
            Err(GphotosError::Protocol(_))
        ));
    }

    #[test]
    fn exchange_sends_verifier_and_secret() {
        let g = MockGoogle::new();
        let t = exchange_code(
            &g,
            "cid",
            Some("sec"),
            "the-code",
            "verifier",
            "http://127.0.0.1:1/",
        )
        .unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("refresh-1"));
        assert!(t.expires_in > 0);
        let req = g.requests_to(TOKEN_ENDPOINT).pop().unwrap();
        let form = parse_pairs(&String::from_utf8(req.body).unwrap());
        assert!(form.contains(&("code_verifier".into(), "verifier".into())));
        assert!(form.contains(&("client_secret".into(), "sec".into())));
        assert!(form.contains(&("grant_type".into(), "authorization_code".into())));
        // No secret (an iOS-type client): the field is left out.
        exchange_code(&g, "cid", Some(""), "the-code", "verifier", "r").unwrap();
        let req = g.requests_to(TOKEN_ENDPOINT).pop().unwrap();
        assert!(
            !String::from_utf8(req.body)
                .unwrap()
                .contains("client_secret")
        );
    }

    #[test]
    fn exchange_failures() {
        let g = MockGoogle::new();
        let bad = exchange_code(&g, "cid", None, "wrong-code", "v", "r").unwrap_err();
        assert!(
            matches!(bad, GphotosError::Auth(ref m) if m.contains("invalid_grant")),
            "{bad}"
        );
        g.state().granted_scope = SCOPE_READ_APP.into();
        let narrow = exchange_code(&g, "cid", None, "the-code", "v", "r").unwrap_err();
        assert!(narrow.to_string().contains("add photos"), "{narrow}");
        g.state().granted_scope = SCOPES.join(" ");
        g.state().omit_refresh_token = true;
        assert!(matches!(
            exchange_code(&g, "cid", None, "the-code", "v", "r"),
            Err(GphotosError::Protocol(_))
        ));
        g.state().offline = true;
        assert!(matches!(
            exchange_code(&g, "cid", None, "the-code", "v", "r"),
            Err(GphotosError::Network(_))
        ));
    }

    #[test]
    fn refresh_and_revoke() {
        let g = MockGoogle::new();
        let t = refresh(&g, "cid", Some("sec"), "refresh-1").unwrap();
        assert!(t.access_token.starts_with("access-"));
        assert!(matches!(
            refresh(&g, "cid", None, "revoked"),
            Err(GphotosError::Auth(_))
        ));
        revoke(&g, "refresh-1").unwrap();
        // Revoked: refreshing it now fails, and revoking again is fine.
        assert!(matches!(
            refresh(&g, "cid", None, "refresh-1"),
            Err(GphotosError::Auth(_))
        ));
        revoke(&g, "refresh-1").unwrap();
        g.state().token_status = Some(503);
        assert!(matches!(
            refresh(&g, "cid", None, "refresh-1"),
            Err(GphotosError::Http { status: 503, .. })
        ));
        g.state().token_status = Some(200);
        assert!(matches!(
            refresh(&g, "cid", None, "refresh-1"),
            Err(GphotosError::Protocol(_))
        ));
        g.state().revoke_status = Some(500);
        assert!(matches!(
            revoke(&g, "x"),
            Err(GphotosError::Http { status: 500, .. })
        ));
        g.state().offline = true;
        assert!(matches!(revoke(&g, "x"), Err(GphotosError::Network(_))));
    }
}
