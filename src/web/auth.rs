//! Console authentication.
//!
//! The console governs local boxes, so the trust boundary is the local user.
//! Two mechanisms enforce it (see `DECISIONS.md` ADR-0048, superseding 0004):
//!
//! 1. The listener binds `127.0.0.1` only — nothing off-host can reach it.
//! 2. A per-launch random key, held in the browser's origin-scoped storage and
//!    presented explicitly on every request that carries data.
//!
//! ## Why the session cookie is gone
//!
//! It was the whole vulnerability. Cookies are scoped by *host*, and a host has
//! no port — so the browser attached the console's cookie to every request it
//! made to any other service on `127.0.0.1`. A project's own dev server on
//! another port therefore read the console token straight out of its inbound
//! `Cookie` header, and could then call the console directly.
//!
//! No header check can close that. The replayer is not a browser: it omits
//! `Origin` and `Sec-Fetch-*` — which the guards below must tolerate, because a
//! genuine top-level navigation omits them too — and it can equally forge them.
//! Headers are not secrets. The only repair is a credential that never reaches
//! the other port at all.
//!
//! `sessionStorage` is that credential. It is scoped to an origin, *port
//! included*, so `127.0.0.1:3000` cannot read what `127.0.0.1:7878` stored, and
//! nothing attaches it automatically — page script must choose to send it. That
//! second property is what retires CSRF here as a class: an ambient credential
//! is the thing forgery rides, and there no longer is one.
//!
//! Per *tab*, not per browser, and that is the second half of the scoping. The
//! console binds a predictable port, so a page served earlier from that same
//! port by something since stopped shares this origin exactly. `localStorage`
//! would have handed such a page the key — every tab on an origin shares it,
//! and the `storage` event announces each write — leaving it free to replay
//! same-origin against the terminal and lifecycle routes. See ADR-0048 for the
//! one residual this leaves.
//!
//! ## The two secrets, and why they are two
//!
//! [`AppState::token`] rides the URL `devbox web` prints (`?t=…`). It buys one
//! thing: the bootstrap page that installs the key. [`AppState::key`] is what
//! every other request is judged by.
//!
//! They must not be the same value. If the key were recoverable from the token,
//! then recovering either would recover both, and the separation would be
//! decoration — which is exactly how the cookie failed, its value having been
//! the token itself.
//!
//! ## What a request without the key gets
//!
//! A top-level navigation cannot send a header, so the first hop to any page
//! arrives bare. It is answered with [`SHELL`] — a fixed document holding no
//! box data, which reads the key from storage and fetches the real page itself.
//! The shell is served to anyone, and discloses strictly less than `/metrics`
//! already does. Everything that carries data needs the key.

use askama::Template;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::state::AppState;

/// Header carrying the console key on requests that can set one.
pub const KEY_HEADER: &str = "x-devbox-key";

/// Query parameter carrying the console key on requests that cannot.
///
/// `EventSource` and `WebSocket` accept no custom headers, so the two live
/// channels present the key in the URL instead. That is a weaker place to put a
/// secret in general — URLs reach logs and history in a way headers do not —
/// but not here: both are subresource requests issued by script that already
/// holds the key, so the URL is never navigated to, never recorded in history,
/// and never sent as a `Referer` to anyone. The only reader is the console's
/// own access log.
pub const KEY_PARAM: &str = "k";

/// Query parameter carrying the bootstrap token on the initial navigation.
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

/// Pull a named parameter out of a query string.
///
/// No percent-decoding, deliberately. Both secrets this reads are URL-safe
/// base64 — `[A-Za-z0-9_-]`, which every encoder leaves alone — so decoding
/// would change nothing it is asked about while quietly widening what compares
/// equal to a secret.
pub fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Pull the `t=` bootstrap token out of a query string.
pub fn query_token(query: Option<&str>) -> Option<&str> {
    query_param(query, TOKEN_PARAM)
}

/// The console key this request presents, from wherever it is allowed to.
///
/// The header is the ordinary path. The query parameter is accepted only below
/// `/api/`, where the live channels that cannot set headers live; a navigable
/// page is excluded on purpose, so no URL a user can see, bookmark, or paste
/// ever becomes a credential.
fn presented_key<B>(req: &axum::http::Request<B>) -> Option<&str> {
    if let Some(header) = req.headers().get(KEY_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(header);
    }
    if is_page_path(req.uri().path()) {
        return None;
    }
    query_param(req.uri().query(), KEY_PARAM)
}

/// Can this path answer a browser navigation, and so be met with the shell?
///
/// Defined by exclusion, which makes the default safe in both directions: a
/// page route added later gets a shell without anyone remembering to say so,
/// and anything under `/api/` demands the key without anyone remembering
/// either. Listing the four page routes here instead would mean a fifth added
/// to the router and forgotten here answers navigations with a 401 no one can
/// explain.
///
/// Serving a shell for a path that does not exist is harmless — the shell then
/// fetches it with the key and gets the same 404 the navigation would have.
fn is_page_path(path: &str) -> bool {
    !path.starts_with("/api/")
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

/// Did something other than the user or the console itself cause this request?
///
/// `origin_is_self` has to allow a missing `Origin`, because that is what an
/// ordinary top-level navigation looks like — and that allowance is a hole a
/// GET cannot be trusted through. A hostile page on another loopback port can
/// navigate or frame the browser at any console URL; the navigation carries no
/// `Origin`, so it is served, and any request the rendered page then makes on
/// its own carries a perfectly correct one. Moving the side effect from the
/// GET to a POST does not help when the page fires that POST on load.
///
/// `Sec-Fetch-Site` is the header that can tell these apart, because the
/// browser sets it from what *initiated* the request rather than from who is
/// sending it: `none` when the user typed or bookmarked the URL,
/// `same-origin` when the console navigated itself, `same-site` or
/// `cross-site` when another page did.
///
/// `same-site` is rejected deliberately. A site ignores the port, so a page
/// served from another `127.0.0.1` port is same-site with this console — which
/// is the entire threat model here, not a hypothetical.
///
/// Absent means a client that does not send it: an older browser, `curl`, the
/// test suite. Those fall back to the checks above rather than being locked
/// out, and the fallback is not a weakness — a browser modern enough to be
/// steered into this attack is modern enough to send the header.
fn foreign_initiated<B>(req: &axum::http::Request<B>) -> bool {
    req.headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|site| !matches!(site, "none" | "same-origin"))
}

/// Is this request for an embedded context rather than the page itself?
///
/// Framing the console is never legitimate, and it is how a hostile page would
/// read a navigation it caused instead of merely triggering its side effects.
fn embedded<B>(req: &axum::http::Request<B>) -> bool {
    req.headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|dest| matches!(dest, "iframe" | "frame" | "embed" | "object"))
}

/// Whether a `Host` header names this machine's loopback interface.
///
/// This is the DNS-rebinding guard. An attacker's page cannot read our
/// responses cross-origin and cannot send our `SameSite=Strict` cookie, but it
/// *can* point its own hostname at `127.0.0.1` and issue requests that the
/// browser considers same-origin with the attacker. Requiring a loopback
/// `Host` closes that door: `evil.example` never appears here, whatever it
/// resolves to.
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
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !is_loopback_host(&host) {
        return wrong_host();
    }

    let path = req.uri().path().to_string();
    if is_public(&path) {
        return no_framing(next.run(req).await);
    }

    // The printed URL, spent on the one page that installs the key.
    //
    // Before the key check, so that re-opening it repairs a browser whose
    // stored key has gone stale — the situation every user is in each time the
    // console is relaunched.
    //
    // And before the origin checks below, which is the ordering worth
    // justifying rather than leaving to be re-derived. Those checks refuse a
    // `cross-site` initiator, and clicking the printed URL out of a chat window
    // or a webmail tab *is* a cross-site initiator; ordering them first would
    // refuse the one navigation the whole flow depends on. What makes that safe
    // is that this branch is already gated on the launch token, and anyone
    // holding that can mint a key directly — so the guards protect nothing here
    // that the token has not already given away.
    //
    // This is the check that has to be re-examined if the token ever becomes
    // guessable, cacheable, or reusable across launches.
    if query_token(req.uri().query()).is_some_and(|t| tokens_match(t, &state.token)) {
        return bootstrap(&state.key, safe_target(&strip_token(req.uri())));
    }

    // Who *caused* this request — asked before anything is served, the shell
    // included.
    //
    // Ordering this after the shell branch reintroduced the CSRF the POST on
    // the terminal tab was moved to a POST to avoid. A hostile page navigates
    // the browser to `/boxes/<name>?tab=terminal`; the navigation carries no
    // key, so it took the shell branch before ever reaching these checks — and
    // the shell then fetched the page itself, same-origin and keyed. That fetch
    // is indistinguishable from a real one. The shell laundered the foreign
    // navigation into a local request, the terminal page auto-posted `/start`,
    // and the box came up for a page the user never chose to visit.
    //
    // A genuine navigation is `none` (typed, bookmarked, opened by `devbox
    // web`) or `same-origin` (a link inside the console); both still pass. The
    // shell is only reachable by someone who could have reached the page.
    if !origin_is_self(&req) || foreign_initiated(&req) || embedded(&req) {
        return unauthorized();
    }

    let offered = presented_key(&req);
    if !offered.is_some_and(|k| tokens_match(k, &state.key)) {
        // Offering *nothing* is a navigation. It cannot do otherwise, so it is
        // the ordinary first hop rather than an intrusion, and the shell it
        // gets back holds nothing worth having.
        //
        // Offering a key that is wrong is a different event, and conflating
        // the two produced a real failure: a browser holding a key from a
        // previous launch got a second shell instead of a refusal, wrote it
        // over itself, and showed a blank page — no notice, no error, and the
        // dead key still stored, so every reload did it again. The shell only
        // ever fetches when it holds a key, so answering a presented-but-wrong
        // key with 401 also makes shell-into-shell structurally impossible.
        if offered.is_none() && req.method() == Method::GET && is_page_path(&path) {
            return shell();
        }
        return unauthorized();
    }

    no_framing(next.run(req).await)
}

/// A redirect target that cannot leave this origin.
///
/// [`strip_token`] rebuilds from `Uri::path`, and a request line is free to
/// carry `//evil.example` there. `location.replace` reads that as
/// protocol-relative and follows it off-host, which would turn the bootstrap
/// page into an open redirect that runs with a fresh key in hand. A backslash
/// is included because some URL parsers normalise it to a slash.
fn safe_target(target: &str) -> &str {
    let protocol_relative = target.starts_with("//") || target.starts_with("/\\");
    if target.starts_with('/') && !protocol_relative {
        target
    } else {
        "/"
    }
}

/// The document a bare navigation receives.
///
/// Fixed and data-free by construction: it is served to anyone who asks, so
/// nothing in it may depend on which box was requested or on any box existing.
/// Its only input is `location`, which it already has.
///
/// It replaces itself with a whole document rather than swapping a fragment
/// into a shared skeleton. The alternative would have meant splitting every
/// template in two and re-deriving each page's `<head>` here — and the detail
/// page's head loads `xterm.js` before `term.js`, an order htmx does not
/// preserve for injected scripts. `term.js` gives up silently when `Terminal`
/// is undefined, so getting that wrong produces a terminal tab that simply
/// never connects. Handing the browser a document to parse keeps every page's
/// head, and every script order, exactly as it already was.
pub const SHELL: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <meta name="referrer" content="no-referrer" />
    <title>devbox</title>
    <link rel="icon" href="/assets/favicon.svg" type="image/svg+xml" />
    <link rel="stylesheet" href="/assets/css/app.css" />
  </head>
  <body>
    <noscript>
      <main class="notice">
        <h1>devbox</h1>
        <p>The console needs JavaScript: its key is held by the browser and
        presented per request, which a plain document load cannot do.</p>
      </main>
    </noscript>
    <script src="/assets/js/key.js"></script>
    <script src="/assets/js/shell.js"></script>
  </body>
</html>
"#;

/// Hand back the shell, uncached.
///
/// `no-store` and the `Vary` matter because one URL now has two answers that
/// differ only by a request header. A cached shell would be replayed to the
/// keyed fetch that is trying to replace it, and a cached page would be handed
/// to a navigation that has no key — the second being the one that would
/// actually leak.
fn shell() -> Response {
    no_framing(
        (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
                (header::VARY, KEY_HEADER),
            ],
            SHELL,
        )
            .into_response(),
    )
}

/// The page `?t=…` lands on: it installs the key, then leaves.
///
/// Both values reach the document as attributes rather than as script text.
/// `target` is derived from a URI the caller controls, and the difference
/// between an escaped attribute and an interpolated JavaScript string literal
/// is the difference between a quoted path and a script of the caller's
/// choosing. Askama escapes the attribute; nothing has to escape the script,
/// because there is nothing in it to escape.
#[derive(Template)]
#[template(path = "bootstrap.html")]
struct BootstrapTemplate<'a> {
    key: &'a str,
    target: &'a str,
}

fn bootstrap(key: &str, target: &str) -> Response {
    match (BootstrapTemplate { key, target }).render() {
        Ok(html) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            html,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "bootstrap render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template render failed").into_response()
        }
    }
}

fn wrong_host() -> Response {
    (
        StatusCode::MISDIRECTED_REQUEST,
        "the devbox console only answers to a loopback host name",
    )
        .into_response()
}

/// Refuse to be embedded, for browsers that will not tell us who asked.
///
/// `embedded` reads `Sec-Fetch-Dest`, which a client is free not to send; this
/// says the same thing in the other direction, where the browser enforces it
/// without being asked. Both are cheap and they fail independently.
///
/// `frame-ancestors` is the modern spelling and `X-Frame-Options` the one
/// older browsers obey — the pair is deliberate, not redundancy left in by
/// accident.
fn no_framing(mut res: Response) -> Response {
    let headers = res.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-frame-options"),
        axum::http::HeaderValue::from_static("DENY"),
    );
    res
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
    fn parses_query_params() {
        assert_eq!(query_param(Some("k=abc"), KEY_PARAM), Some("abc"));
        assert_eq!(query_param(Some("x=1&k=abc&y=2"), KEY_PARAM), Some("abc"));
        assert_eq!(query_param(Some("x=1"), KEY_PARAM), None);
        assert_eq!(query_param(None, KEY_PARAM), None);
        // A prefix is not a name: `kk=` must not answer for `k=`.
        assert_eq!(query_param(Some("kk=abc"), KEY_PARAM), None);
    }

    fn keyed(uri: &str, header_key: Option<&str>) -> Request<()> {
        let mut b = Request::builder()
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7878");
        if let Some(k) = header_key {
            b = b.header(KEY_HEADER, k);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn the_header_carries_the_key_anywhere() {
        assert_eq!(presented_key(&keyed("/", Some("secret"))), Some("secret"));
        assert_eq!(
            presented_key(&keyed("/api/boxes", Some("secret"))),
            Some("secret")
        );
    }

    #[test]
    fn a_url_can_only_carry_the_key_where_a_header_is_impossible() {
        // `EventSource` and `WebSocket` set no headers, so the two live
        // channels present the key in the query.
        assert_eq!(
            presented_key(&keyed("/api/stream?k=secret", None)),
            Some("secret")
        );
        assert_eq!(
            presented_key(&keyed("/api/boxes/web/term?k=secret", None)),
            Some("secret")
        );

        // A page must not, or the key becomes something a user can see in the
        // address bar, bookmark, and paste into a chat window — which is the
        // property that made the launch token worth replacing.
        assert_eq!(presented_key(&keyed("/?k=secret", None)), None);
        assert_eq!(presented_key(&keyed("/boxes/web?k=secret", None)), None);
    }

    #[test]
    fn pages_may_be_shelled_and_data_routes_may_not() {
        for path in ["/", "/boxes/web", "/help", "/help/zellij", "/nonsense"] {
            assert!(is_page_path(path), "{path} answers navigations");
        }
        for path in ["/api/boxes", "/api/stream", "/api/boxes/web/term"] {
            assert!(!is_page_path(path), "{path} must demand the key");
        }
    }

    #[test]
    fn a_bootstrap_target_cannot_leave_this_origin() {
        assert_eq!(safe_target("/boxes/web"), "/boxes/web");
        assert_eq!(
            safe_target("/boxes/web?tab=terminal"),
            "/boxes/web?tab=terminal"
        );
        assert_eq!(safe_target("/"), "/");

        // `location.replace("//evil.example")` is protocol-relative and leaves
        // the host. The bootstrap page runs with a fresh key in hand, so an
        // open redirect there is a redirect that has just been authenticated.
        assert_eq!(safe_target("//evil.example"), "/");
        assert_eq!(safe_target("//evil.example/path"), "/");
        // Some URL parsers normalise a backslash to a slash.
        assert_eq!(safe_target("/\\evil.example"), "/");
        // Anything not rooted at all.
        assert_eq!(safe_target("https://evil.example"), "/");
        assert_eq!(safe_target(""), "/");
    }

    #[test]
    fn the_shell_carries_no_box_data_and_no_secret() {
        // It is served to anyone who asks, so this is the property that makes
        // that safe. Asserted on the constant rather than on a rendered page
        // because there is nothing to render it *from* — which is the point.
        assert!(SHELL.contains("/assets/js/shell.js"));
        assert!(SHELL.contains("/assets/js/key.js"));
        for forbidden in ["devbox.key", "sandbox", "runtime", "{{", "{%"] {
            assert!(
                !SHELL.contains(forbidden),
                "the shell must not mention `{forbidden}`"
            );
        }
        // `key.js` must be parsed before anything that reads it.
        let key_at = SHELL.find("/assets/js/key.js").unwrap();
        let shell_at = SHELL.find("/assets/js/shell.js").unwrap();
        assert!(key_at < shell_at, "key.js has to come first");
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

    fn nav(site: Option<&str>, dest: Option<&str>) -> Request<()> {
        let mut b = Request::builder().header(header::HOST, "127.0.0.1:7878");
        if let Some(s) = site {
            b = b.header("sec-fetch-site", s);
        }
        if let Some(d) = dest {
            b = b.header("sec-fetch-dest", d);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn a_navigation_another_page_caused_is_refused() {
        // The hole `origin_is_self` cannot close on its own: it has to permit
        // a missing `Origin`, because that is what an ordinary top-level
        // navigation looks like. So a hostile page could point the browser at
        // `/boxes/<name>?tab=terminal`, the navigation would be served, and
        // the rendered page's own load-triggered POST — carrying an entirely
        // correct Origin — would start the box. Moving the side effect off the
        // GET did not help, because the page itself performs the POST.
        assert!(foreign_initiated(&nav(Some("cross-site"), None)));
        // `same-site` is refused too, and it is the one that matters: a site
        // ignores the port, so another page on 127.0.0.1 is same-site with
        // this console. That is the actual attacker, not a hypothetical one.
        assert!(foreign_initiated(&nav(Some("same-site"), None)));
    }

    #[test]
    fn the_user_and_the_console_are_still_allowed_in() {
        // Typed, bookmarked, or opened by `devbox web`.
        assert!(!foreign_initiated(&nav(Some("none"), None)));
        // The console navigating or fetching within itself.
        assert!(!foreign_initiated(&nav(Some("same-origin"), None)));
        // Absent: an older browser, curl, the test suite.
        //
        // Tolerating absence used to be argued for — "a browser new enough to
        // be steered into this attack is new enough to send the header" — and
        // that argument was answering the wrong threat. The replayer was not a
        // browser. It omitted this header precisely because omission was
        // allowed, and could have forged it just as easily; a header is not a
        // secret. Tolerance is not what makes this safe now, and never was.
        // The key is. This check no longer stands between anyone and the box.
        assert!(!foreign_initiated(&nav(None, None)));
    }

    #[test]
    fn the_console_refuses_to_be_framed() {
        for dest in ["iframe", "frame", "embed", "object"] {
            assert!(embedded(&nav(Some("same-origin"), Some(dest))), "{dest}");
        }
        // The ordinary destinations a console produces.
        for dest in ["document", "empty", "script", "style", "image"] {
            assert!(!embedded(&nav(Some("same-origin"), Some(dest))), "{dest}");
        }
    }
}
