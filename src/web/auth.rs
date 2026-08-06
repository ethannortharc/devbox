//! Console authentication.
//!
//! The console governs local boxes, so the trust boundary is the local user.
//! Two mechanisms enforce it (see `DECISIONS.md` ADR-0004):
//!
//! 1. The listener binds `127.0.0.1` only — nothing off-host can reach it.
//! 2. A per-launch random token, handed out once in the opened URL
//!    (`?t=…`) and immediately exchanged for an `HttpOnly` session cookie.
//!
//! The exchange matters: a token that lived only in the query string would be
//! lost on the first internal link and would leak through `Referer`. After the
//! exchange the token never appears in a URL again.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::state::AppState;

/// Name of the session cookie holding the console token.
pub const COOKIE_NAME: &str = "devbox_console";

/// Query parameter carrying the token on the initial navigation.
pub const TOKEN_PARAM: &str = "t";

/// Generate a fresh 256-bit console token, URL-safe base64 encoded.
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare two tokens without leaking their contents through timing.
///
/// Length is compared first and non-secretly — token length is fixed and
/// public, so this reveals nothing an attacker does not already know.
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extract a cookie value from a raw `Cookie` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// Pull the `t=` token out of a query string.
pub fn query_token(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == TOKEN_PARAM).then_some(v)
    })
}

/// Rebuild a URI with the token parameter stripped, so the redirect target is
/// a clean path the user can bookmark and share within their own session.
pub fn strip_token(uri: &Uri) -> String {
    let path = uri.path();
    let rest: Vec<&str> = uri
        .query()
        .map(|q| {
            q.split('&')
                .filter(|pair| !pair.starts_with(&format!("{TOKEN_PARAM}=")))
                .filter(|pair| !pair.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

/// Paths served without a token: static assets (public library code), the
/// browser's unprompted favicon request, and the liveness probe (used by tests
/// and by `devbox doctor`). None of these expose box state.
pub fn is_public(path: &str) -> bool {
    path.starts_with("/assets/")
        || path == "/healthz"
        || path == "/favicon.ico"
        // Prometheus scrapes without a cookie. `/metrics` exposes counts and
        // statuses — never box contents — and the loopback bind plus the Host
        // check are still in force.
        || path == "/metrics"
}

/// Whether a `Host` header names this machine's loopback interface.
///
/// This is the DNS-rebinding guard. An attacker's page cannot read our
/// responses cross-origin and cannot send our `SameSite=Strict` cookie, but it
/// *can* point its own hostname at `127.0.0.1` and issue requests that the
/// browser considers same-origin with the attacker. Requiring a loopback
/// `Host` closes that door: `evil.example` never appears here, whatever it
/// resolves to.
/// Does this request originate from the console's own origin?
///
/// `Origin` is set by the browser and cannot be forged by page script, so it
/// is the honest answer to "who is asking". It is absent on ordinary top-level
/// navigations, which is why absence is allowed — but *present and different*
/// is a cross-origin request, and the console has none it needs to serve.
///
/// The comparison includes the port, which is exactly what `SameSite` does not
/// do and why this check exists at all.
fn origin_is_self<B>(req: &axum::http::Request<B>) -> bool {
    let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true; // absent: a top-level navigation, not a cross-site call
    };

    // `null` is what a sandboxed iframe or a `file://` page sends. Neither is
    // the console.
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };

    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| host.eq_ignore_ascii_case(authority))
}

pub fn is_loopback_host(host: &str) -> bool {
    // Strip the port, tolerating a bracketed IPv6 literal.
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };

    name.eq_ignore_ascii_case("localhost")
        || name == "127.0.0.1"
        || name == "::1"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Axum middleware enforcing the loopback host and the token on every
/// non-public route.
pub async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Applies to public paths too: even a stylesheet should not be reachable
    // through a rebound hostname, and rejecting early keeps the rule simple.
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_loopback_host);
    if !host_ok {
        return wrong_host();
    }

    let path = req.uri().path();
    if is_public(path) {
        return next.run(req).await;
    }

    // Already authenticated for this browser session?
    let cookie_ok = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| cookie_value(h, COOKIE_NAME))
        .is_some_and(|t| tokens_match(t, &state.token));

    if cookie_ok {
        // The cookie alone is not enough. `SameSite=Strict` compares *sites*,
        // and a site ignores the port — so a hostile page served from another
        // 127.0.0.1 port is same-site with the console, and the browser
        // attaches this cookie to its requests. The Host check above does not
        // help: the attacker aims at loopback on purpose.
        //
        // So a cookie-authenticated request must also prove where it came
        // from. A same-origin request either sends a matching Origin or, for
        // plain top-level navigations, none at all.
        if !origin_is_self(&req) {
            return unauthorized();
        }
        return next.run(req).await;
    }

    // First navigation: `?t=…` is exchanged for the session cookie.
    if query_token(req.uri().query()).is_some_and(|t| tokens_match(t, &state.token)) {
        let target = strip_token(req.uri());
        let cookie = format!(
            "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict",
            state.token
        );
        return (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, target.as_str()),
                (header::SET_COOKIE, cookie.as_str()),
            ],
        )
            .into_response();
    }

    unauthorized()
}

fn wrong_host() -> Response {
    (
        StatusCode::MISDIRECTED_REQUEST,
        "the devbox console only answers to a loopback host name",
    )
        .into_response()
}

fn unauthorized() -> Response {
    let body = "<!doctype html><meta charset=utf-8><title>devbox</title>\
        <style>body{font:14px/1.6 system-ui;margin:64px auto;max-width:36rem;color:#444}\
        code{background:#eee;padding:2px 6px;border-radius:4px}</style>\
        <h1>Not authorized</h1><p>This console requires the launch token. \
        Re-open the URL printed by <code>devbox web</code>.</p>";
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    use axum::http::Request;

    fn req(host: &str, origin: Option<&str>) -> Request<()> {
        let mut b = Request::builder().header(header::HOST, host);
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn a_page_on_another_loopback_port_is_not_us() {
        // The attack this exists for: `SameSite` compares sites, and a site
        // ignores the port, so the browser sends the console's cookie to a
        // request forged by a page on another 127.0.0.1 port.
        assert!(!origin_is_self(&req(
            "127.0.0.1:7777",
            Some("http://127.0.0.1:9999")
        )));
        assert!(!origin_is_self(&req(
            "localhost:7777",
            Some("http://evil.example")
        )));
        // A sandboxed iframe or a file:// page.
        assert!(!origin_is_self(&req("127.0.0.1:7777", Some("null"))));
    }

    #[test]
    fn our_own_page_and_plain_navigations_are_accepted() {
        assert!(origin_is_self(&req(
            "127.0.0.1:7777",
            Some("http://127.0.0.1:7777")
        )));
        // Ordinary top-level navigation: browsers send no Origin, and
        // rejecting it would make the console unopenable.
        assert!(origin_is_self(&req("127.0.0.1:7777", None)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_long_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        // 32 bytes base64url without padding = 43 chars.
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "ab"));
        assert!(!tokens_match("", "x"));
        assert!(tokens_match("", ""));
    }

    #[test]
    fn parses_cookie_values() {
        let h = "foo=1; devbox_console=tok123; bar=2";
        assert_eq!(cookie_value(h, COOKIE_NAME), Some("tok123"));
        assert_eq!(cookie_value(h, "foo"), Some("1"));
        assert_eq!(cookie_value(h, "missing"), None);
        assert_eq!(
            cookie_value("devbox_console=solo", COOKIE_NAME),
            Some("solo")
        );
    }

    #[test]
    fn parses_query_token() {
        assert_eq!(query_token(Some("t=abc")), Some("abc"));
        assert_eq!(query_token(Some("x=1&t=abc&y=2")), Some("abc"));
        assert_eq!(query_token(Some("x=1")), None);
        assert_eq!(query_token(None), None);
    }

    #[test]
    fn strips_token_from_uri() {
        let uri: Uri = "/boxes/foo?t=secret".parse().unwrap();
        assert_eq!(strip_token(&uri), "/boxes/foo");

        let uri: Uri = "/boxes/foo?t=secret&tab=activity".parse().unwrap();
        assert_eq!(strip_token(&uri), "/boxes/foo?tab=activity");

        let uri: Uri = "/".parse().unwrap();
        assert_eq!(strip_token(&uri), "/");
    }

    #[test]
    fn public_paths_bypass_auth() {
        assert!(is_public("/assets/js/htmx.min.js"));
        assert!(is_public("/healthz"));
        assert!(is_public("/favicon.ico"));
        assert!(is_public("/metrics"));
        assert!(!is_public("/"));
        assert!(!is_public("/api/boxes"));
        // A path that merely mentions assets must not slip through.
        assert!(!is_public("/api/assets/js"));
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:7878",
            "localhost",
            "localhost:7878",
            "LocalHost:7878",
            "[::1]:7878",
            "127.0.0.5:7878",
        ] {
            assert!(is_loopback_host(host), "{host} should be loopback");
        }
    }

    #[test]
    fn rebound_hosts_are_rejected() {
        // A DNS-rebinding attacker resolves their own name to 127.0.0.1; the
        // Host header still carries their name, which is what we reject.
        for host in [
            "evil.example",
            "evil.example:7878",
            "192.168.1.10:7878",
            "0.0.0.0:7878",
            "localhost.evil.example",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host} must not pass");
        }
    }
}
