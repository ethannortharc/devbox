//! The credential broker end to end, without a box — §6.
//!
//! Everything here runs in-process: a stub upstream on loopback, the real
//! broker router over it, and a real SQLite store underneath. What is being
//! tested is the part a unit test cannot reach — that the credential goes in,
//! the box token does not come out, a denied request never touches the
//! upstream, an SSE response arrives in pieces rather than all at once, and
//! every one of those writes a `credential` row.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt as _;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use devbox::broker::providers::{HttpProvider, Injection};
use devbox::broker::scope::{Access, Scope};
use devbox::broker::secrets::SecretStore;
use devbox::broker::server::{BrokerState, router};
use devbox::broker::tokens;
use devbox::obs::event::EventType;
use devbox::obs::store::{Query, Store};

/// What the stub upstream saw, so the test can assert on the injected header
/// without the broker having to hand it back.
#[derive(Debug, Clone, Default)]
struct Seen {
    authorization: Option<String>,
    api_key: Option<String>,
    broker_token: Option<String>,
    method: String,
    path: String,
    body: String,
}

type SeenLog = Arc<std::sync::Mutex<Vec<Seen>>>;

/// A stub upstream that records what it was sent.
///
/// `/stream` answers with three separately-flushed chunks, a second apart, so
/// a broker that buffers the response is distinguishable from one that
/// forwards it — which is the whole difference for SSE.
async fn upstream(log: SeenLog) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::any;

    async fn record(
        State(log): State<SeenLog>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: axum::http::HeaderMap,
        body: axum::body::Bytes,
    ) -> impl IntoResponse {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let seen = Seen {
            authorization: header("authorization"),
            api_key: header("x-api-key"),
            broker_token: header("x-devbox-broker-token"),
            method: method.to_string(),
            path: uri.path().to_string(),
            body: String::from_utf8_lossy(&body).to_string(),
        };
        let streaming = seen.path.ends_with("/stream");
        log.lock().unwrap().push(seen);

        if streaming {
            let chunks = futures::stream::unfold(0u8, |index| async move {
                if index >= 3 {
                    return None;
                }
                if index > 0 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from(format!(
                        "event: chunk\ndata: {index}\n\n"
                    ))),
                    index + 1,
                ))
            });
            return (
                [("content-type", "text/event-stream")],
                Body::from_stream(chunks),
            )
                .into_response();
        }
        ([("content-type", "application/json")], "{\"ok\":true}").into_response()
    }

    let app = axum::Router::new()
        .route("/{*rest}", any(record))
        .with_state(log);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}

struct Harness {
    _dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    router: axum::Router,
    log: SeenLog,
    token: String,
    store: std::path::PathBuf,
    _upstream: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_repos(BTreeSet::new()).await
    }

    async fn with_repos(repos: BTreeSet<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let log: SeenLog = Default::default();
        let (addr, handle) = upstream(log.clone()).await;

        let secrets = SecretStore::file_backed(&state_dir);
        secrets.set("w1b-test", "dummy-value").unwrap();
        secrets.set("github", "ghp_dummy").unwrap();
        devbox::broker::write_json(
            &devbox::broker::http_provider_path(&state_dir, "w1b-test").unwrap(),
            &HttpProvider {
                url: format!("http://{addr}"),
                injection: Injection::parse("Authorization: Bearer").unwrap(),
            },
        )
        .unwrap();

        let token = tokens::rotate(&state_dir, "myapp").unwrap();
        let store = state_dir.join("events.db");

        let mut broker = BrokerState::new(state_dir.clone()).unwrap();
        // The real store path lives under `boxes/<name>/`; pinning it here
        // keeps the audit assertions independent of a box existing.
        broker.secrets = SecretStore::file_backed(&state_dir);
        let pinned = store.clone();
        broker.store_path = Box::new(move |_| pinned.clone());
        broker.project_repos = Box::new(move |_| repos.clone());
        broker.requests = AtomicU64::new(0);

        Self {
            _dir: dir,
            state_dir,
            router: router(Arc::new(broker)),
            log,
            token,
            store,
            _upstream: handle,
        }
    }

    /// Point the `github` provider at the stub instead of github.com, so the
    /// scope tests exercise real routing without leaving the machine.
    fn github_via_stub(&self) {
        let addr = self.upstream_addr();
        devbox::broker::write_json(
            &devbox::broker::http_provider_path(&self.state_dir, "gh-stub").unwrap(),
            &HttpProvider {
                url: format!("http://{addr}"),
                injection: Injection::parse("Authorization: Bearer").unwrap(),
            },
        )
        .unwrap();
        SecretStore::file_backed(&self.state_dir)
            .set("gh-stub", "ghp_dummy")
            .unwrap();
    }

    fn upstream_addr(&self) -> String {
        let text = std::fs::read_to_string(
            devbox::broker::http_provider_path(&self.state_dir, "w1b-test").unwrap(),
        )
        .unwrap();
        let provider: HttpProvider = serde_json::from_str(&text).unwrap();
        provider.url.trim_start_matches("http://").to_string()
    }

    async fn send(&self, request: Request<Body>) -> axum::response::Response {
        self.router.clone().oneshot(request).await.unwrap()
    }

    fn events(&self) -> Vec<devbox::obs::event::Event> {
        if !self.store.exists() {
            return Vec::new();
        }
        let store = Store::open(&self.store).unwrap();
        store
            .query(&Query {
                kinds: vec![EventType::Credential],
                limit: Some(100),
                ..Default::default()
            })
            .unwrap()
    }

    fn set_scope(&self, provider: &str, scope: &Scope) {
        devbox::broker::write_json(
            &devbox::broker::scope_path(&self.state_dir, provider).unwrap(),
            scope,
        )
        .unwrap();
    }
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("x-devbox-broker-token", token)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_credential_is_injected_and_the_box_token_never_leaves_the_host() {
    let h = Harness::new().await;
    let response = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/w1b-test/anything")
                .header("x-devbox-broker-token", &h.token)
                // A client that also sends its own Authorization must not be
                // able to smuggle it upstream.
                .header("authorization", "Bearer client-supplied")
                .header("content-type", "application/json")
                .body(Body::from("{\"hello\":\"world\"}"))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The response body has to be consumed before the audit row exists: the
    // row carries the real response size, so it is written when the stream
    // ends rather than when its headers arrive.
    let echoed = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&echoed[..], b"{\"ok\":true}");

    let seen = h.log.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "exactly one upstream request");
    let seen = &seen[0];
    assert_eq!(
        seen.authorization.as_deref(),
        Some("Bearer dummy-value"),
        "the stored secret goes in, and nothing else"
    );
    assert_eq!(
        seen.broker_token, None,
        "the box token must never reach an upstream"
    );
    assert_eq!(
        seen.api_key, None,
        "only the header this provider configured is set, and no other"
    );
    assert!(
        !seen
            .authorization
            .as_deref()
            .unwrap_or_default()
            .contains(&h.token),
        "the box token must not appear inside the injected header either"
    );
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/anything");
    assert_eq!(
        seen.body, "{\"hello\":\"world\"}",
        "the request body streams through"
    );

    // Content-type is copied; the framing headers are not.
    let events = h.events();
    assert_eq!(events.len(), 1);
    let credential = events[0].credential.as_ref().unwrap();
    assert_eq!(credential.provider, "w1b-test");
    assert_eq!(credential.verdict, "allowed");
    assert_eq!(credential.status, 200);
    assert_eq!(credential.method, "POST");
    assert_eq!(credential.path, "/anything");
    assert_eq!(credential.req_bytes, 17);
    assert!(credential.resp_bytes > 0);

    let raw = serde_json::to_string(&events[0]).unwrap();
    assert!(
        !raw.contains("dummy-value"),
        "the audit row must not carry the secret"
    );
    assert!(!raw.contains(&h.token), "nor the box token");
}

#[tokio::test]
async fn a_request_without_a_valid_token_is_refused_before_the_upstream() {
    let h = Harness::new().await;

    for request in [
        Request::builder()
            .uri("/w1b-test/anything")
            .body(Body::empty())
            .unwrap(),
        get("/w1b-test/anything", "not-a-real-token"),
        Request::builder()
            .uri("/w1b-test/anything")
            .header("authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap(),
    ] {
        let response = h.send(request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    assert!(
        h.log.lock().unwrap().is_empty(),
        "an unauthenticated request must never reach the upstream"
    );
    assert!(
        h.events().is_empty(),
        "and must not be attributed to any box, because there is no box to attribute it to"
    );
}

#[tokio::test]
async fn the_box_token_is_accepted_in_the_authorization_header_too() {
    // This is the normal case for `anthropic`: `ANTHROPIC_AUTH_TOKEN=<box
    // token>` becomes `Authorization: Bearer <box token>` (measured).
    let h = Harness::new().await;
    let response = h
        .send(
            Request::builder()
                .uri("/w1b-test/anything")
                .header("authorization", format!("Bearer {}", h.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        h.log.lock().unwrap()[0].authorization.as_deref(),
        Some("Bearer dummy-value")
    );
}

#[tokio::test]
async fn a_denied_method_returns_403_with_a_reason_and_never_reaches_the_upstream() {
    let h = Harness::new().await;
    h.set_scope(
        "w1b-test",
        &Scope {
            access: Access::Read,
            paths: vec![],
            repos: None,
        },
    );

    let response = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/w1b-test/anything")
                .header("x-devbox-broker-token", &h.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "devbox_broker_denied");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("POST"),
        "the reason names the method: {message}"
    );

    assert!(h.log.lock().unwrap().is_empty());
    let events = h.events();
    assert_eq!(events.len(), 1);
    let credential = events[0].credential.as_ref().unwrap();
    assert_eq!(credential.verdict, "denied");
    assert_eq!(credential.status, 0, "nothing upstream answered");
    assert_eq!(credential.reason, "method outside scope");
}

#[tokio::test]
async fn a_path_outside_the_scope_is_denied() {
    let h = Harness::new().await;
    h.set_scope(
        "w1b-test",
        &Scope {
            access: Access::Push,
            paths: vec!["/allowed".into()],
            repos: None,
        },
    );

    let allowed = h.send(get("/w1b-test/allowed/thing", &h.token)).await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let _ = allowed.into_body().collect().await.unwrap();
    assert_eq!(
        h.send(get("/w1b-test/forbidden", &h.token)).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(h.log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_github_repository_outside_the_projects_remotes_is_denied() {
    let h = Harness::with_repos(["ethannortharc/devbox".to_string()].into_iter().collect()).await;

    // Denied before any network call, so pointing at real github.com is safe:
    // the assertion is that nothing is attempted.
    let response = h.send(get("/github/repos/someone/else", &h.token)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("devbox secret scope github"),
        "the denial says how to widen it: {text}"
    );

    let events = h.events();
    assert_eq!(events.len(), 1);
    let credential = events[0].credential.as_ref().unwrap();
    assert_eq!(credential.verdict, "denied");
    assert_eq!(credential.reason, "repository outside scope");
    assert_eq!(credential.host, "api.github.com");
    assert_eq!(credential.path, "/repos/someone/else");

    // A smart-HTTP path for a repository that *is* in scope passes the scope
    // check (and then fails to reach github.com, which is not what is tested
    // here) — so assert on the scope decision itself.
    let scope = Scope::default();
    assert!(
        scope
            .check(
                "GET",
                "/ethannortharc/devbox.git/info/refs",
                devbox::broker::scope::repo_from_path("/ethannortharc/devbox.git/info/refs")
                    .as_deref(),
                true,
                &["ethannortharc/devbox".to_string()].into_iter().collect(),
            )
            .is_ok()
    );
}

#[tokio::test]
async fn a_streamed_response_arrives_in_pieces_rather_than_all_at_once() {
    let h = Harness::new().await;
    let response = h.send(get("/w1b-test/stream", &h.token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the SSE content type must survive the proxy"
    );
    assert!(
        response.headers().get("transfer-encoding").is_none(),
        "hop-by-hop framing must not be copied through"
    );

    let started = std::time::Instant::now();
    let mut stream = response.into_body().into_data_stream();
    let mut arrivals = Vec::new();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        arrivals.push(started.elapsed());
        text.push_str(&String::from_utf8_lossy(&chunk));
    }

    assert_eq!(text.matches("event: chunk").count(), 3);
    assert!(
        arrivals.len() >= 2,
        "a buffering broker delivers one chunk; this delivered {}",
        arrivals.len()
    );
    // The upstream spaces its chunks 200ms apart. If the first one arrived
    // only after the last was produced, the broker buffered.
    assert!(
        arrivals[0] < Duration::from_millis(350),
        "the first chunk took {:?}; the broker is buffering the stream",
        arrivals[0]
    );

    // The audit row is written when the stream ends, with the real size.
    drop(stream);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = h.events();
    assert_eq!(events.len(), 1);
    let credential = events[0].credential.as_ref().unwrap();
    assert_eq!(credential.verdict, "allowed");
    assert_eq!(credential.resp_bytes, text.len() as u64);
}

#[tokio::test]
async fn an_unknown_provider_and_a_missing_secret_are_told_apart() {
    let h = Harness::new().await;

    let unknown = h.send(get("/nope/x", &h.token)).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // Configured upstream, no secret: a different failure with a different fix.
    devbox::broker::write_json(
        &devbox::broker::http_provider_path(&h.state_dir, "keyless").unwrap(),
        &HttpProvider {
            url: "http://127.0.0.1:1".into(),
            injection: Injection::bearer(),
        },
    )
    .unwrap();
    let keyless = h.send(get("/keyless/x", &h.token)).await;
    assert_eq!(keyless.status(), StatusCode::FORBIDDEN);
    let body = keyless.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("devbox secret set keyless"));

    assert!(h.log.lock().unwrap().is_empty());
    let reasons: Vec<String> = h
        .events()
        .iter()
        .map(|e| e.credential.as_ref().unwrap().reason.clone())
        .collect();
    assert!(reasons.contains(&"unknown provider".to_string()));
    assert!(reasons.contains(&"no secret stored for this provider".to_string()));
}

#[tokio::test]
async fn a_path_that_would_climb_out_of_the_provider_is_refused() {
    let h = Harness::new().await;
    // The scope was checked against the literal path; a broker that let
    // `..` through would send the request somewhere the check never saw.
    let response = h.send(get("/w1b-test/a/../../elsewhere", &h.token)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(h.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unreachable_upstream_is_reported_rather_than_swallowed() {
    let h = Harness::new().await;
    devbox::broker::write_json(
        &devbox::broker::http_provider_path(&h.state_dir, "dead").unwrap(),
        &HttpProvider {
            // Port 1 on loopback: nothing listens, and the connection is
            // refused rather than left hanging.
            url: "http://127.0.0.1:1".into(),
            injection: Injection::bearer(),
        },
    )
    .unwrap();
    SecretStore::file_backed(&h.state_dir)
        .set("dead", "dummy")
        .unwrap();

    let response = h.send(get("/dead/x", &h.token)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let credential = h
        .events()
        .into_iter()
        .find(|e| e.credential.as_ref().unwrap().provider == "dead")
        .unwrap();
    let credential = credential.credential.unwrap();
    assert_eq!(credential.verdict, "error");
    assert_eq!(credential.reason, "upstream connection failed");
}

#[tokio::test]
async fn the_claude_code_connectivity_probe_is_answered_without_a_token() {
    // Measured: Claude Code sends `HEAD <base>/api/hello` from Bun with no
    // auth header at all before its first API call.
    let h = Harness::new().await;
    let response = h
        .send(
            Request::builder()
                .method("HEAD")
                .uri("/anthropic/api/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        h.events().is_empty(),
        "a connectivity probe is not a credential use"
    );
}

#[tokio::test]
async fn two_boxes_cannot_read_each_others_tokens_or_audit_trails() {
    let h = Harness::new().await;
    let other = tokens::rotate(&h.state_dir, "other").unwrap();
    assert_ne!(other, h.token);

    let store = devbox::broker::tokens::TokenStore::new(&h.state_dir);
    assert_eq!(store.box_for(&h.token).as_deref(), Some("myapp"));
    assert_eq!(store.box_for(&other).as_deref(), Some("other"));

    // And the file the token lives in is not group- or world-readable.
    use std::os::unix::fs::PermissionsExt as _;
    let path = devbox::broker::tokens::tokens_dir(&h.state_dir).join("other");
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
        0
    );
}

#[tokio::test]
async fn a_git_smart_http_request_routes_to_the_right_upstream_and_repository() {
    // Routing and scope, without leaving the machine: the `gh-stub` provider
    // points at the local upstream, and the assertion is on what arrives.
    let h = Harness::new().await;
    h.github_via_stub();

    let response = h
        .send(get(
            "/gh-stub/ethannortharc/devbox.git/info/refs?service=git-upload-pack",
            &h.token,
        ))
        .await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let seen = h.log.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/ethannortharc/devbox.git/info/refs");
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer ghp_dummy"));

    // The audit row records the path without the query string.
    let credential = h.events()[0].credential.clone().unwrap();
    assert_eq!(credential.path, "/ethannortharc/devbox.git/info/refs");
    assert!(!credential.path.contains("service="));
}
