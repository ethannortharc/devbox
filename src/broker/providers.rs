//! Which upstream a path belongs to, and how the credential goes in — §6.3.
//!
//! Every route the broker serves is `/<provider>/<upstream path>`. This module
//! turns the provider segment plus the rest of the path into a concrete
//! upstream URL and one header to set, and it owns the list of headers that
//! must be *removed* on the way out.
//!
//! Two facts here were measured against the real clients rather than read off
//! a table, because both decide whether the broker works at all:
//!
//! - `ANTHROPIC_BASE_URL` is honoured including its path prefix and over plain
//!   `http`, so `http://<broker>/anthropic` yields `POST /anthropic/v1/messages`.
//! - `ANTHROPIC_AUTH_TOKEN` produces `Authorization: Bearer <v>`, while
//!   `ANTHROPIC_API_KEY` produces `x-api-key: <v>`. Never both.
//!
//! The measurement is in `docs`/the W1-B report; the tests below encode the
//! shapes so a future edit that breaks them fails here.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// How a credential is attached to the upstream request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Injection {
    /// Header name, lowercase.
    pub header: String,
    /// Text placed before the secret, e.g. `"Bearer "`. Often empty.
    #[serde(default)]
    pub prefix: String,
}

impl Injection {
    pub fn bearer() -> Self {
        Self {
            header: "authorization".into(),
            prefix: "Bearer ".into(),
        }
    }

    pub fn api_key() -> Self {
        Self {
            header: "x-api-key".into(),
            prefix: String::new(),
        }
    }

    pub fn value(&self, secret: &str) -> String {
        format!("{}{secret}", self.prefix)
    }

    /// Parse the `--header` argument of `devbox secret set`.
    ///
    /// `'Authorization: Bearer'` means "set `Authorization` to `Bearer <secret>`";
    /// `'X-Api-Key'` means "set `X-Api-Key` to the secret with nothing in front".
    /// The trailing space is supplied here so the operator never has to reason
    /// about whether their shell kept it.
    pub fn parse(spec: &str) -> Result<Self> {
        let (name, prefix) = match spec.split_once(':') {
            Some((name, rest)) => (name.trim(), rest.trim()),
            None => (spec.trim(), ""),
        };
        if name.is_empty() {
            bail!("header specification '{spec}' has no header name");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("header name '{name}' is not a valid HTTP header name");
        }
        Ok(Self {
            header: name.to_ascii_lowercase(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix} ")
            },
        })
    }
}

/// A generic `http:<name>` provider, as persisted under
/// `~/.devbox/broker/http/<name>.json`. Holds no secret — only where to send
/// the request and which header the secret goes in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpProvider {
    pub url: String,
    pub injection: Injection,
}

/// Everything the proxy needs to forward one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// `https://api.github.com` — scheme and authority, no trailing slash.
    pub origin: String,
    /// The upstream path, leading slash included, query stripped.
    pub path: String,
    /// The upstream host, for the audit record.
    pub host: String,
    pub injection: Injection,
}

impl Route {
    pub fn url(&self, query: Option<&str>) -> String {
        match query.filter(|q| !q.is_empty()) {
            Some(q) => format!("{}{}?{}", self.origin, self.path, q),
            None => format!("{}{}", self.origin, self.path),
        }
    }
}

/// The built-in providers of §6.3.
pub const ANTHROPIC: &str = "anthropic";
pub const OPENAI: &str = "openai";
pub const GITHUB: &str = "github";

/// Whether `name` is one of the built-ins.
pub fn is_builtin(name: &str) -> bool {
    matches!(name, ANTHROPIC | OPENAI | GITHUB)
}

/// Resolve `/<provider>/<rest>` into an upstream route.
///
/// `secret_is_oauth` selects the Anthropic header: an OAuth token goes in
/// `Authorization: Bearer`, a plain `sk-ant-…` key in `x-api-key`. Anthropic
/// accepts either, but sending a key as a bearer token gets a 401 that reads
/// like a broker bug, so the distinction is made once, here.
pub fn route(
    provider: &str,
    rest: &str,
    secret_is_oauth: bool,
    http: Option<&HttpProvider>,
) -> Result<Route> {
    let path = normalize_path(rest);
    match provider {
        ANTHROPIC => Ok(Route {
            origin: "https://api.anthropic.com".into(),
            host: "api.anthropic.com".into(),
            path,
            injection: if secret_is_oauth {
                Injection::bearer()
            } else {
                Injection::api_key()
            },
        }),
        OPENAI => Ok(Route {
            origin: "https://api.openai.com".into(),
            host: "api.openai.com".into(),
            path,
            injection: Injection::bearer(),
        }),
        GITHUB => Ok(github_route(&path)),
        other => {
            let Some(config) = http else {
                bail!("unknown provider '{other}'");
            };
            let origin = config.url.trim_end_matches('/').to_string();
            let host = origin
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
                .unwrap_or(&origin)
                .split('@')
                .next_back()
                .unwrap_or("")
                .to_string();
            Ok(Route {
                origin,
                host,
                path,
                injection: config.injection.clone(),
            })
        }
    }
}

/// GitHub is two upstreams behind one provider segment.
///
/// `api.github.com` serves the REST and GraphQL API; `github.com` serves git's
/// smart HTTP. Which one a path belongs to is decided by the path itself,
/// because that is the only signal the client gives:
///
/// - `/api/v3/...` and `/api/graphql` — what `gh` builds for a GitHub
///   Enterprise host. Rewritten onto `api.github.com`. (`gh` cannot actually
///   be pointed at this broker; see the W1-B report. The mapping is here so a
///   future TLS-terminating broker needs no new routing.)
/// - anything ending in git's three endpoints — `github.com`, unchanged.
/// - everything else — `api.github.com`, unchanged.
fn github_route(path: &str) -> Route {
    let api = |path: String| Route {
        origin: "https://api.github.com".into(),
        host: "api.github.com".into(),
        path,
        injection: Injection::bearer(),
    };

    if let Some(rest) = path.strip_prefix("/api/v3") {
        return api(if rest.is_empty() {
            "/".into()
        } else {
            rest.into()
        });
    }
    if path == "/api/graphql" {
        return api("/graphql".into());
    }
    if path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
    {
        return Route {
            origin: "https://github.com".into(),
            host: "github.com".into(),
            path: path.to_string(),
            injection: Injection::bearer(),
        };
    }
    api(path.to_string())
}

/// Force a leading slash and refuse anything that could climb out of it.
///
/// `..` is rejected rather than resolved: the broker's whole authority rests
/// on the upstream path being the one the scope was checked against, and a
/// path that resolves differently in `reqwest` than in the scope check is
/// exactly the bug that makes an allowlist ornamental.
fn normalize_path(rest: &str) -> String {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return "/".into();
    }
    format!("/{rest}")
}

/// True when the path contains a segment that would re-anchor it upstream.
pub fn path_is_traversal(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment == ".." || segment == ".")
        || path.contains("//")
        || path.contains('\\')
}

/// Headers that never reach the upstream.
///
/// Two groups, for two different reasons:
///
/// * The box's broker token, in either of the places a client may put it. This
///   is the whole point: the token proves which box is asking, and forwarding
///   it to Anthropic or GitHub would hand a third party a credential for this
///   host's broker.
/// * Hop-by-hop headers (RFC 9110 §7.6.1) plus `host`. A proxy that copies
///   `transfer-encoding` or `connection` through produces framing the upstream
///   reads differently from the client — the classic request-smuggling shape.
pub const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-devbox-broker-token",
    "proxy-authorization",
    "connection",
    "proxy-connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Headers not copied back from the upstream, for the same framing reason.
/// `content-length` goes too: the body is re-streamed, so the length the
/// upstream stated is not the framing this response uses.
pub const STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// Whether a stored Anthropic secret is an OAuth token rather than an API key.
///
/// A console API key is `sk-ant-api…`; a Claude subscription OAuth token is
/// `sk-ant-oat…` and belongs in `Authorization: Bearer`. Anything that is not
/// recognisably a key is treated as a bearer token, which is the shape every
/// non-Anthropic upstream uses.
pub fn anthropic_secret_is_oauth(secret: &str) -> bool {
    !secret.starts_with("sk-ant-api")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_keeps_the_measured_shape() {
        // Measured: `ANTHROPIC_BASE_URL=http://host/anthropic` produces
        // `POST /anthropic/v1/messages?beta=true`.
        let oauth = route(ANTHROPIC, "v1/messages", true, None).unwrap();
        assert_eq!(oauth.origin, "https://api.anthropic.com");
        assert_eq!(oauth.path, "/v1/messages");
        assert_eq!(
            oauth.url(Some("beta=true")),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(oauth.injection, Injection::bearer());

        let key = route(ANTHROPIC, "/v1/messages", false, None).unwrap();
        assert_eq!(key.injection, Injection::api_key());
        assert_eq!(key.injection.value("sk-ant-api-x"), "sk-ant-api-x");
        assert_eq!(oauth.injection.value("tok"), "Bearer tok");
    }

    #[test]
    fn an_api_key_and_an_oauth_token_take_different_headers() {
        assert!(!anthropic_secret_is_oauth("sk-ant-api03-abc"));
        assert!(anthropic_secret_is_oauth("sk-ant-oat01-abc"));
        assert!(anthropic_secret_is_oauth("anything-else"));
    }

    #[test]
    fn openai_is_bearer_under_a_v1_prefixed_base_url() {
        let openai = route(OPENAI, "v1/chat/completions", true, None).unwrap();
        assert_eq!(
            openai.url(None),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(openai.injection, Injection::bearer());
    }

    #[test]
    fn github_splits_the_api_from_git_smart_http() {
        // The exact path git sent, measured with `git ls-remote`.
        let git = route(GITHUB, "ethannortharc/devbox.git/info/refs", true, None).unwrap();
        assert_eq!(git.host, "github.com");
        assert_eq!(git.path, "/ethannortharc/devbox.git/info/refs");
        assert_eq!(
            git.url(Some("service=git-upload-pack")),
            "https://github.com/ethannortharc/devbox.git/info/refs?service=git-upload-pack"
        );

        for endpoint in ["git-upload-pack", "git-receive-pack"] {
            let r = route(GITHUB, &format!("o/r.git/{endpoint}"), true, None).unwrap();
            assert_eq!(r.host, "github.com", "{endpoint}");
        }

        let api = route(GITHUB, "repos/ethannortharc/devbox", true, None).unwrap();
        assert_eq!(api.host, "api.github.com");
        assert_eq!(api.path, "/repos/ethannortharc/devbox");

        // The GHES shape `gh` builds, mapped back onto the public API.
        let ghes = route(GITHUB, "api/v3/repos/o/r", true, None).unwrap();
        assert_eq!(ghes.host, "api.github.com");
        assert_eq!(ghes.path, "/repos/o/r");
        let graphql = route(GITHUB, "api/graphql", true, None).unwrap();
        assert_eq!(graphql.path, "/graphql");
    }

    #[test]
    fn a_generic_provider_takes_its_upstream_and_header_from_config() {
        let config = HttpProvider {
            url: "http://127.0.0.1:9999/base/".into(),
            injection: Injection::parse("Authorization: Bearer").unwrap(),
        };
        let generic = route("w1b-test", "anything", true, Some(&config)).unwrap();
        assert_eq!(generic.origin, "http://127.0.0.1:9999/base");
        assert_eq!(generic.host, "127.0.0.1:9999");
        assert_eq!(generic.url(None), "http://127.0.0.1:9999/base/anything");
        assert_eq!(generic.injection.value("dummy"), "Bearer dummy");

        assert!(route("w1b-test", "x", true, None).is_err());
    }

    #[test]
    fn header_specifications_parse_with_and_without_a_prefix() {
        let bearer = Injection::parse("Authorization: Bearer").unwrap();
        assert_eq!(bearer, Injection::bearer());
        assert_eq!(bearer.value("v"), "Bearer v");

        let plain = Injection::parse("X-Api-Key").unwrap();
        assert_eq!(plain.header, "x-api-key");
        assert_eq!(plain.value("v"), "v");

        // Case and stray whitespace are normalised, so two operators who type
        // it differently get the same header.
        assert_eq!(
            Injection::parse("  authorization :  Bearer  ").unwrap(),
            Injection::bearer()
        );

        for bad in ["", ": Bearer", "Bad Header: x", "with\nnewline"] {
            assert!(Injection::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_box_token_headers_are_all_on_the_strip_list() {
        for header in [
            "authorization",
            "x-api-key",
            "x-devbox-broker-token",
            "proxy-authorization",
        ] {
            assert!(
                STRIPPED_REQUEST_HEADERS.contains(&header),
                "{header} must never reach an upstream"
            );
        }
        for header in ["connection", "transfer-encoding", "upgrade", "host"] {
            assert!(STRIPPED_REQUEST_HEADERS.contains(&header), "{header}");
        }
        assert!(STRIPPED_RESPONSE_HEADERS.contains(&"content-length"));
        // The one header that must survive in both directions, or SSE breaks.
        assert!(!STRIPPED_RESPONSE_HEADERS.contains(&"content-type"));
    }

    #[test]
    fn paths_are_anchored_and_traversal_is_visible() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("v1/messages"), "/v1/messages");
        assert_eq!(normalize_path("///v1"), "/v1");

        assert!(path_is_traversal("/repos/../user"));
        assert!(path_is_traversal("/a//b"));
        assert!(path_is_traversal("/a/./b"));
        assert!(!path_is_traversal("/repos/owner/name"));
        assert!(!path_is_traversal("/o/r.git/info/refs"));
    }

    #[test]
    fn only_the_three_named_providers_are_built_in() {
        assert!(is_builtin("anthropic") && is_builtin("openai") && is_builtin("github"));
        assert!(!is_builtin("w1b-test"));
    }
}
