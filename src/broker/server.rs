//! The reverse proxy itself — §6.1.
//!
//! One axum service, one route: `/{provider}/{*rest}`. For each request it
//!
//! 1. identifies the box from its token (`Authorization: Bearer` or
//!    `x-devbox-broker-token`),
//! 2. resolves the provider to an upstream and an injection header,
//! 3. checks the scope,
//! 4. forwards the request body as a stream, with the box's token stripped and
//!    the real credential injected,
//! 5. streams the response body straight back,
//! 6. writes one `credential` event into that box's store.
//!
//! Both bodies are streamed rather than buffered. That is not an optimisation:
//! Claude Code sends `"stream": true` (measured) and reads the reply as
//! server-sent events, so a broker that collects the response before
//! forwarding it turns a live token stream into one long pause. The byte
//! counts in the audit record are therefore accumulated as the bytes pass,
//! and the response event is written when the stream ends.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::StreamExt as _;

use super::providers::{self, HttpProvider, Route};
use super::scope::{self, Denial, Scope};
use super::secrets::SecretStore;
use super::tokens::TokenStore;
use crate::obs::event::{Credential, Event, EventType};

/// Where a box's event store lives. Injectable so the tests can assert on the
/// audit trail without a real box on disk.
pub type StorePathFn = Box<dyn Fn(&str) -> PathBuf + Send + Sync>;

/// A box's default GitHub repository allowlist, from its project's remotes.
pub type ProjectReposFn = Box<dyn Fn(&str) -> BTreeSet<String> + Send + Sync>;

/// Everything a request handler needs, shared behind an `Arc`.
pub struct BrokerState {
    pub state_dir: PathBuf,
    pub tokens: TokenStore,
    pub secrets: SecretStore,
    pub client: reqwest::Client,
    /// Overridden by the tests so the audit assertions do not depend on a
    /// real box existing.
    pub store_path: StorePathFn,
    /// Resolves a box name to the repositories its project directory points
    /// at. Injectable for the same reason.
    pub project_repos: ProjectReposFn,
    pub requests: AtomicU64,
}

impl BrokerState {
    pub fn new(state_dir: PathBuf) -> Result<Self> {
        let client = reqwest::Client::builder()
            // No global timeout: a streaming completion legitimately stays
            // open for minutes. The connect phase is bounded instead, which
            // is the part that hangs when the network is wrong.
            .connect_timeout(std::time::Duration::from_secs(20))
            .user_agent(concat!("devbox-broker/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build the broker's upstream HTTP client")?;
        let store_dir = state_dir.clone();
        let repo_dir = state_dir.clone();
        Ok(Self {
            tokens: TokenStore::new(&state_dir),
            secrets: SecretStore::open(&state_dir),
            client,
            store_path: Box::new(move |box_id| {
                crate::obs::collector::store_path(&store_dir, box_id)
            }),
            project_repos: Box::new(move |box_id| project_repos_for_box(&repo_dir, box_id)),
            requests: AtomicU64::new(0),
            state_dir,
        })
    }

    fn scope_for(&self, provider: &str) -> Scope {
        super::scope_path(&self.state_dir, provider)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn http_provider(&self, name: &str) -> Option<HttpProvider> {
        let path = super::http_provider_path(&self.state_dir, name)?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// The repositories a box's project directory points at.
///
/// A box whose state cannot be read gets an empty set, which denies every
/// github request and says why — the same direction the policy context takes
/// when it cannot read a box (`enforce::discover_context`): an unreadable box
/// produces the strictest answer, never the most permissive.
fn project_repos_for_box(state_dir: &std::path::Path, box_id: &str) -> BTreeSet<String> {
    match crate::sandbox::state::SandboxState::load(state_dir, box_id) {
        Ok(state) => scope::project_repos(&state.project_dir),
        Err(_) => BTreeSet::new(),
    }
}

/// Build the router.
pub fn router(state: Arc<BrokerState>) -> axum::Router {
    axum::Router::new()
        .route("/healthz", any(healthz))
        .route("/{provider}", any(handle_root))
        .route("/{provider}/{*rest}", any(handle))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "devbox broker\n")
}

async fn handle_root(
    state: State<Arc<BrokerState>>,
    AxumPath(provider): AxumPath<String>,
    method: Method,
    uri: Uri,
    query: RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    proxy(
        state,
        provider,
        String::new(),
        method,
        uri,
        query,
        headers,
        body,
    )
    .await
}

async fn handle(
    state: State<Arc<BrokerState>>,
    AxumPath((provider, rest)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    query: RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    proxy(state, provider, rest, method, uri, query, headers, body).await
}

#[allow(clippy::too_many_arguments)]
async fn proxy(
    State(state): State<Arc<BrokerState>>,
    provider: String,
    rest: String,
    method: Method,
    _uri: Uri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);

    // Claude Code probes `<base>/api/hello` with HEAD and no credential at all
    // (measured: `Bun/1.4.1`, no auth header). Answering it here keeps the
    // probe from being logged as a denied credential use, and keeps a client
    // that treats the probe as fatal from failing before it ever authenticates.
    if rest == "api/hello" {
        return (StatusCode::OK, "ok\n").into_response();
    }

    let Some(token) = extract_token(&headers) else {
        return deny_unauthenticated("no devbox broker token on the request");
    };
    let Some(box_id) = state.tokens.box_for(&token) else {
        return deny_unauthenticated("the devbox broker token is not valid for any box");
    };

    let audit = |credential: Credential| write_credential_event(&state, &box_id, credential);

    let http = state.http_provider(&provider);
    if !providers::is_builtin(&provider) && http.is_none() {
        let credential = Credential {
            provider: provider.clone(),
            method: method.to_string(),
            path: format!("/{rest}"),
            verdict: "denied".into(),
            reason: "unknown provider".into(),
            ..Default::default()
        };
        audit(credential);
        return deny(
            StatusCode::NOT_FOUND,
            &provider,
            format!(
                "unknown provider '{provider}'. Configure it with \
                 `devbox secret set {provider} --url … --header …`"
            ),
        );
    }

    // A stored secret is required before anything is forwarded: without it the
    // broker would proxy the box's own token to the upstream, which is both
    // useless and a leak.
    let secret = match state.secrets.get(&provider) {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            audit(Credential {
                provider: provider.clone(),
                method: method.to_string(),
                path: format!("/{rest}"),
                verdict: "denied".into(),
                reason: "no secret stored for this provider".into(),
                ..Default::default()
            });
            return deny(
                StatusCode::FORBIDDEN,
                &provider,
                format!(
                    "no secret is stored for '{provider}'. \
                     Run `devbox secret set {provider}`"
                ),
            );
        }
        Err(error) => {
            audit(Credential {
                provider: provider.clone(),
                method: method.to_string(),
                path: format!("/{rest}"),
                verdict: "error".into(),
                reason: "secret store unavailable".into(),
                ..Default::default()
            });
            return deny(
                StatusCode::INTERNAL_SERVER_ERROR,
                &provider,
                format!("the secret store is unavailable: {error}"),
            );
        }
    };

    let route = match providers::route(
        &provider,
        &rest,
        providers::anthropic_secret_is_oauth(&secret),
        http.as_ref(),
    ) {
        Ok(route) => route,
        Err(error) => {
            return deny(StatusCode::NOT_FOUND, &provider, error.to_string());
        }
    };

    if providers::path_is_traversal(&route.path) {
        audit(Credential {
            provider: provider.clone(),
            method: method.to_string(),
            host: route.host.clone(),
            path: route.path.clone(),
            verdict: "denied".into(),
            reason: "path contains a traversal or empty segment".into(),
            ..Default::default()
        });
        return deny(
            StatusCode::BAD_REQUEST,
            &provider,
            "the request path contains a traversal or empty segment".to_string(),
        );
    }

    let is_github = provider == providers::GITHUB;
    let scope = state.scope_for(&provider);
    let repo = is_github
        .then(|| scope::repo_from_path(&route.path))
        .flatten();
    let defaults = if is_github {
        (state.project_repos)(&box_id)
    } else {
        BTreeSet::new()
    };
    if let Err(denial) = scope.check(
        method.as_str(),
        &route.path,
        repo.as_deref(),
        is_github,
        &defaults,
    ) {
        audit(Credential {
            provider: provider.clone(),
            method: method.to_string(),
            host: route.host.clone(),
            path: route.path.clone(),
            verdict: "denied".into(),
            reason: denial_reason(&denial),
            ..Default::default()
        });
        return deny(StatusCode::FORBIDDEN, &provider, denial.to_string());
    }

    forward(
        state, box_id, provider, route, method, query, headers, body, &secret,
    )
    .await
}

/// A short, stable machine-readable reason, so the audit trail groups.
fn denial_reason(denial: &Denial) -> String {
    match denial {
        Denial::Method { .. } => "method outside scope".into(),
        Denial::Path { .. } => "path outside scope".into(),
        Denial::Repo { .. } => "repository outside scope".into(),
        Denial::NoRepo => "request names no repository".into(),
        Denial::NoRemotes => "no repository in scope".into(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward(
    state: Arc<BrokerState>,
    box_id: String,
    provider: String,
    route: Route,
    method: Method,
    query: Option<String>,
    headers: HeaderMap,
    body: Body,
    secret: &str,
) -> Response {
    let url = route.url(query.as_deref());

    let mut request = state
        .client
        .request(method.clone(), &url)
        .header(&route.injection.header, route.injection.value(secret));

    // Copy through everything the client sent except the box's own credential
    // and the hop-by-hop framing headers.
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if providers::STRIPPED_REQUEST_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if lower == route.injection.header {
            continue;
        }
        request = request.header(name.clone(), value.clone());
    }

    // The request body streams too. `content-length` was stripped above, so
    // reqwest frames it as chunked, which is what an unknown-length stream is.
    let sent = Arc::new(AtomicU64::new(0));
    let counted = sent.clone();
    let stream = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            counted.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        chunk
    });
    request = request.body(reqwest::Body::wrap_stream(stream));

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            // Upstream failures are reported, not swallowed: a broker that
            // turns a connection reset into an empty 200 makes every client
            // above it debug the wrong layer.
            write_credential_event(
                &state,
                &box_id,
                Credential {
                    provider: provider.clone(),
                    method: method.to_string(),
                    host: route.host.clone(),
                    path: route.path.clone(),
                    req_bytes: sent.load(Ordering::Relaxed),
                    verdict: "error".into(),
                    reason: upstream_error_reason(&error),
                    ..Default::default()
                },
            );
            return deny(
                StatusCode::BAD_GATEWAY,
                &provider,
                format!("upstream {} could not be reached: {error}", route.host),
            );
        }
    };

    let status = upstream.status();
    let mut response = Response::builder().status(status);
    if let Some(map) = response.headers_mut() {
        for (name, value) in upstream.headers().iter() {
            let lower = name.as_str().to_ascii_lowercase();
            if providers::STRIPPED_RESPONSE_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            map.append(name.clone(), value.clone());
        }
    }

    // The response body is forwarded chunk by chunk. The audit event is
    // written when the last chunk goes out, which is why it carries the real
    // response size for a stream whose length nobody knew at header time.
    let received = Arc::new(AtomicU64::new(0));
    let counter = received.clone();
    let finish_state = state.clone();
    let finish_box = box_id.clone();
    let finish = Credential {
        provider,
        method: method.to_string(),
        host: route.host.clone(),
        path: route.path.clone(),
        status: status.as_u16(),
        req_bytes: sent.load(Ordering::Relaxed),
        resp_bytes: 0,
        verdict: "allowed".into(),
        reason: String::new(),
    };
    let stream = upstream.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        chunk
    });
    let stream = FinishOnDrop {
        inner: Box::pin(stream),
        state: finish_state,
        box_id: finish_box,
        credential: Some(finish),
        received,
    };

    match response.body(Body::from_stream(stream)) {
        Ok(response) => response,
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("broker could not build the response: {error}\n"),
        )
            .into_response(),
    }
}

/// A response-body stream that writes the audit record when it ends.
///
/// Writing the event at header time would report zero bytes for every
/// streamed response; writing it in a `finally` after the copy would miss a
/// client that disconnects mid-stream. `Drop` covers both, because axum drops
/// the body either way.
struct FinishOnDrop {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    state: Arc<BrokerState>,
    box_id: String,
    credential: Option<Credential>,
    received: Arc<AtomicU64>,
}

impl futures::Stream for FinishOnDrop {
    type Item = reqwest::Result<bytes::Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        if let Some(mut credential) = self.credential.take() {
            credential.resp_bytes = self.received.load(Ordering::Relaxed);
            write_credential_event(&self.state, &self.box_id, credential);
        }
    }
}

fn upstream_error_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "upstream timed out".into()
    } else if error.is_connect() {
        "upstream connection failed".into()
    } else {
        "upstream request failed".into()
    }
}

/// Pull the box token out of either header the design allows.
///
/// `x-devbox-broker-token` exists because the Anthropic clients spend
/// `Authorization` on their own token — which, in the brokered configuration,
/// *is* the box token, so `Authorization: Bearer <box token>` is the normal
/// case for `anthropic` and the dedicated header is what `curl`, `git`, and
/// anything with its own bearer scheme use.
fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-devbox-broker-token")
        && let Ok(text) = value.to_str()
        && !text.trim().is_empty()
    {
        return Some(text.trim().to_string());
    }
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let text = value.to_str().ok()?;
    let rest = text
        .strip_prefix("Bearer ")
        .or_else(|| text.strip_prefix("bearer "))?;
    let rest = rest.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

fn deny_unauthenticated(reason: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "type": "devbox_broker_unauthorized",
            "message": reason,
        }
    });
    let mut response = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    response.headers_mut().insert(
        HeaderName::from_static("www-authenticate"),
        HeaderValue::from_static("Bearer realm=\"devbox-broker\""),
    );
    response
}

fn deny(status: StatusCode, provider: &str, message: String) -> Response {
    let body = serde_json::json!({
        "error": {
            "type": "devbox_broker_denied",
            "provider": provider,
            "message": message,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// Append one `credential` event to the box's own store.
///
/// Best effort by design: an audit write that fails must not turn into a
/// failed API call for the agent, because the agent cannot fix it and the
/// operator finds out from `doctor`. The failure is logged, not swallowed
/// silently.
pub fn write_credential_event(state: &BrokerState, box_id: &str, credential: Credential) {
    let path = (state.store_path)(box_id);
    let event = credential_event(box_id, credential);
    let write = (|| -> Result<()> {
        let store = crate::obs::store::Store::open(&path)?;
        // Attributed, not plain. This is the one event writer that is not the
        // collector, so nothing else would give it a `run_id` — and a run
        // report's Credentials section reads by run, which left it empty while
        // the events it wanted were in the same table.
        store.insert_attributed(&event)?;
        Ok(())
    })();
    if let Err(error) = write {
        tracing::warn!(%error, box_id, "could not record a credential event");
    }
}

/// Build the event. Split out so the tests can assert on the envelope without
/// a store.
pub fn credential_event(box_id: &str, credential: Credential) -> Event {
    Event {
        ts_wall: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string(),
        // Not this process's clock. The field is a total order over the
        // *box's* boot, and the broker runs on the host — filling it from here
        // sorted a credential event into the middle of a run's process tree at
        // whatever offset the host happened to have been up for.
        ts_mono_ns: crate::obs::event::NO_MONOTONIC,
        box_id: box_id.to_string(),
        cgroup_id: 0,
        // The convention the store already uses for an event with no guest
        // process behind it (the packet-derived `policy` events do the same).
        // A brokered request is made *for* a box, not by a pid the host can
        // see, and A's run attribution treats `u32::MAX` as `window`.
        pid: u32::MAX,
        tid: u32::MAX,
        ppid: 0,
        comm: "broker".into(),
        uid: 0,
        kind: EventType::Credential,
        net: None,
        exec: None,
        file: None,
        api: None,
        policy: None,
        credential: Some(credential),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn a_token_is_read_from_either_header() {
        assert_eq!(
            extract_token(&headers(&[("authorization", "Bearer abc123")])).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_token(&headers(&[("x-devbox-broker-token", "abc123")])).as_deref(),
            Some("abc123")
        );
        // The dedicated header wins, so a client that legitimately needs
        // `Authorization` for something else can still identify itself.
        assert_eq!(
            extract_token(&headers(&[
                ("authorization", "Bearer wrong"),
                ("x-devbox-broker-token", "right"),
            ]))
            .as_deref(),
            Some("right")
        );
    }

    #[test]
    fn a_missing_or_unusable_token_is_not_silently_treated_as_empty() {
        assert_eq!(extract_token(&HeaderMap::new()), None);
        assert_eq!(
            extract_token(&headers(&[("authorization", "Basic x")])),
            None
        );
        assert_eq!(
            extract_token(&headers(&[("authorization", "Bearer ")])),
            None
        );
        assert_eq!(
            extract_token(&headers(&[("x-devbox-broker-token", " ")])),
            None
        );
    }

    #[test]
    fn a_credential_event_validates_and_stores_host_and_path_in_the_indexed_columns() {
        let event = credential_event(
            "myapp",
            Credential {
                provider: "anthropic".into(),
                method: "POST".into(),
                host: "api.anthropic.com".into(),
                path: "/v1/messages".into(),
                status: 200,
                req_bytes: 10,
                resp_bytes: 20,
                verdict: "allowed".into(),
                reason: String::new(),
            },
        );
        event.validate().unwrap();
        assert_eq!(event.kind.domain(), "credential");
        assert_eq!(event.peer().as_deref(), Some("api.anthropic.com"));
        assert_eq!(event.path(), Some("/v1/messages"));
        assert!(event.summary().contains("allowed"));

        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("Bearer"),
            "no header value may reach the store"
        );
    }

    #[test]
    fn every_denial_has_a_short_stable_reason() {
        for denial in [
            Denial::Method {
                method: "DELETE".into(),
                allowed: "GET".into(),
            },
            Denial::Path { path: "/x".into() },
            Denial::Repo { repo: "a/b".into() },
            Denial::NoRepo,
            Denial::NoRemotes,
        ] {
            let reason = denial_reason(&denial);
            assert!(!reason.is_empty());
            assert!(reason.len() < 48, "{reason}");
        }
    }
}
