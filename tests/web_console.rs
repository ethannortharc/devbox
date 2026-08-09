//! HTTP-level tests for the web console.
//!
//! These drive the real axum router through `tower::ServiceExt::oneshot`, so
//! auth, routing, rendering, and serialization are all exercised — without
//! binding a socket or needing a runtime.
//!
//! The fixture boxes use a runtime name no runtime claims, so status probes
//! fail fast and the tests never shell out to `docker`/`limactl`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use devbox::sandbox::SandboxManager;
use devbox::sandbox::state::SandboxState;
use devbox::web::routes;
use devbox::web::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789";

/// A state dir seeded with two boxes, plus a router wired to it.
fn console_with_boxes(names: &[&str]) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().expect("temp dir");
    for name in names {
        // A real project directory: the policy editor writes devbox.toml there,
        // so a fake path would make that path untestable.
        let project = dir.path().join("projects").join(name);
        std::fs::create_dir_all(&project).expect("project dir");

        SandboxState {
            package_sources: Default::default(),
            name: (*name).to_string(),
            runtime: "test-null".to_string(),
            project_dir: project,
            created_at: "2026-08-06T00:00:00Z".to_string(),
            mount_mode: "overlay".to_string(),
            sets: vec!["system".into(), "git".into()],
            languages: vec!["rust".into()],
            image: "nixos".to_string(),
            packages: vec![],
        }
        .save(dir.path())
        .expect("seed sandbox state");
    }

    let manager = Arc::new(SandboxManager {
        state_dir: dir.path().to_path_buf(),
    });
    let router = routes::router(AppState::new(manager, TOKEN));
    (dir, router)
}

/// A request with a loopback `Host`, which every real browser request carries
/// and the console requires (the DNS-rebinding guard).
fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::empty())
        .unwrap()
}

fn get_authed(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn post_authed(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ── auth ─────────────────────────────────────────────────

#[tokio::test]
async fn dashboard_without_a_token_is_rejected() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get("/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get("/?t=not-the-token")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn launch_token_is_exchanged_for_a_session_cookie() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get(&format!("/?t={TOKEN}"))).await.unwrap();

    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    // Redirect target must be the clean path — the token never reappears.
    let location = res.headers().get(header::LOCATION).unwrap();
    assert_eq!(location, "/");

    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains(&format!("devbox_console_7878={TOKEN}")));
    assert!(cookie.contains("HttpOnly"), "cookie must be HttpOnly");
    assert!(cookie.contains("SameSite=Strict"), "cookie must be strict");
}

#[tokio::test]
async fn launch_token_preserves_other_query_parameters() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app
        .oneshot(get(&format!("/?t={TOKEN}&tab=activity")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers().get(header::LOCATION).unwrap(),
        "/?tab=activity"
    );
}

#[tokio::test]
async fn a_rebound_host_name_is_refused() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    // An attacker pointing evil.example at 127.0.0.1 reaches the socket, but
    // the Host header still names them — and that is what we reject.
    let req = Request::builder()
        .uri("/")
        .header(header::HOST, "evil.example:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn a_request_with_no_host_header_is_refused() {
    let (_dir, app) = console_with_boxes(&[]);
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn assets_and_health_need_no_token() {
    let (_dir, app) = console_with_boxes(&[]);

    let res = app
        .clone()
        .oneshot(get("/assets/js/htmx.min.js"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/javascript"
    );

    let res = app.oneshot(get("/healthz")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_string(res).await, "ok");
}

// ── dashboard ────────────────────────────────────────────

#[tokio::test]
async fn dashboard_lists_boxes_from_v3_state() {
    let (_dir, app) = console_with_boxes(&["alpha", "beta"]);
    let res = app.oneshot(get_authed("/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(html.contains("alpha"), "dashboard must list alpha");
    assert!(html.contains("beta"), "dashboard must list beta");
    assert!(html.contains("2 boxes"));
    // Live channel is wired up.
    assert!(html.contains("sse-connect=\"/api/stream\""));
    // Assets are local, never a CDN.
    assert!(!html.contains("http://unpkg"));
    assert!(!html.contains("https://"));
}

#[tokio::test]
async fn dashboard_shows_an_empty_state_with_no_boxes() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(html.contains("No boxes yet"));
    assert!(html.contains("devbox create"));
}

// ── json api ─────────────────────────────────────────────

#[tokio::test]
async fn api_boxes_returns_json_sorted_by_name() {
    let (_dir, app) = console_with_boxes(&["zeta", "alpha"]);
    let res = app.oneshot(get_authed("/api/boxes")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    let arr = json.as_array().expect("array of boxes");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "alpha");
    assert_eq!(arr[1]["name"], "zeta");
    assert_eq!(arr[0]["mount_mode"], "overlay");
    // No runtime claims "test-null", so the probe degrades to unknown.
    assert_eq!(arr[0]["status"], "unknown");
}

#[tokio::test]
async fn api_single_box_and_404() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    let res = app
        .clone()
        .oneshot(get_authed("/api/boxes/alpha"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(json["name"], "alpha");

    let res = app.oneshot(get_authed("/api/boxes/ghost")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_requires_a_token_too() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app.oneshot(get("/api/boxes")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ── box detail ───────────────────────────────────────────

#[tokio::test]
async fn box_detail_renders_each_tab() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for (query, needle) in [
        ("", "mount mode"),
        ("?tab=files", "/api/boxes/alpha/files"),
        ("?tab=overview", "mount mode"),
        // An unknown tab falls back to overview rather than 404ing.
        ("?tab=bogus", "mount mode"),
    ] {
        let res = app
            .clone()
            .oneshot(get_authed(&format!("/boxes/alpha{query}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "tab query {query:?}");
        let html = body_string(res).await;
        assert!(html.contains(needle), "tab {query:?} missing {needle}");
        assert!(html.contains("alpha"));
    }
}

#[tokio::test]
async fn box_detail_404s_for_an_unknown_box() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/boxes/ghost")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn overlay_changes_fragment_renders_for_a_box_that_is_not_running() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/api/boxes/alpha/files"))
        .await
        .unwrap();

    // The runtime is unavailable, so there is nothing to diff — the tab must
    // still render rather than error.
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("No uncommitted changes"));
}

// ── lifecycle ────────────────────────────────────────────

#[tokio::test]
async fn lifecycle_actions_report_a_conflict_when_the_runtime_is_missing() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for action in ["start", "stop", "destroy"] {
        let res = app
            .clone()
            .oneshot(post_authed(&format!("/api/boxes/alpha/{action}")))
            .await
            .unwrap();
        // "test-null" is not a real runtime, so every action fails — but with
        // an actionable 409 and an explanatory body, never a 500 or a panic.
        assert_eq!(
            res.status(),
            StatusCode::CONFLICT,
            "{action} should report a conflict"
        );
        assert!(body_string(res).await.contains("alpha"));
    }
}

#[tokio::test]
async fn lifecycle_actions_require_a_token() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/boxes/alpha/destroy")
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ── sets ─────────────────────────────────────────────────

#[tokio::test]
async fn sets_tab_renders_the_checklist() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/boxes/alpha?tab=sets"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(html.contains("hx-post=\"/api/boxes/alpha/sets\""));
    // The box's own sets are pre-checked; the rest are not.
    assert!(html.contains("value=\"git\""));
    assert!(html.contains("value=\"lang-rust\""));
    assert!(html.contains("Apply &amp; rebuild"));
}

#[tokio::test]
async fn applying_a_selection_is_accepted_and_returns_a_live_log_panel() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            "/api/boxes/alpha/sets",
            "set=system&set=git&packages=hyperfine",
        ))
        .await
        .unwrap();

    // Accepted, not OK: the rebuild runs in the background and streams.
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let html = body_string(res).await;
    assert!(html.contains("sse-swap=\"build-alpha\""));
    assert!(html.contains("2 set(s), 1 extra package(s)"), "got: {html}");
}

#[tokio::test]
async fn an_injectable_package_name_is_rejected_before_anything_runs() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            "/api/boxes/alpha/sets",
            "set=system&packages=hyperfine%5D%3B%20evil",
        ))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(res).await.contains("Invalid selection"));
}

#[tokio::test]
async fn an_unknown_set_is_rejected() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            "/api/boxes/alpha/sets",
            "set=system&set=not-a-set",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn applying_a_selection_to_an_unknown_box_is_404() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app
        .oneshot(post_form("/api/boxes/ghost/sets", "set=system"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ── policy ───────────────────────────────────────────────

fn put_form(uri: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

#[tokio::test]
async fn policy_tab_renders_every_posture() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/boxes/alpha?tab=policy"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    for posture in ["open", "allowlist", "mirror-only", "isolated"] {
        assert!(
            html.contains(&format!("value=\"{posture}\"")),
            "{posture} is missing from the editor"
        );
    }
    assert!(html.contains("hx-put=\"/api/boxes/alpha/policy\""));
    // Open is the default and must be the one checked.
    let open = html.split("value=\"open\"").nth(1).unwrap();
    assert!(open.split("/>").next().unwrap().contains("checked"));
}

#[tokio::test]
async fn policy_is_editable_live_and_persists() {
    let (dir, app) = console_with_boxes(&["alpha"]);

    let res = app
        .clone()
        .oneshot(put_form(
            "/api/boxes/alpha/policy",
            "posture=allowlist&allow=github.com%0A10.0.0.0%2F8&alert=on",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("allowlist"), "got: {body}");
    assert!(body.contains("2 allowlist entries"), "got: {body}");

    // Read it back through the JSON endpoint.
    let res = app
        .oneshot(get_authed("/api/boxes/alpha/policy"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_string(res).await).unwrap();
    assert_eq!(json["egress"], "allowlist");
    assert_eq!(json["allow"][0], "github.com");
    assert_eq!(json["alert_on_violation"], true);

    // And it really landed in the project's devbox.toml, not just in memory.
    let toml = std::fs::read_to_string(dir.path().join("projects/alpha/devbox.toml"))
        .expect("devbox.toml was written");
    assert!(toml.contains("allowlist"), "devbox.toml: {toml}");
    assert!(toml.contains("github.com"), "devbox.toml: {toml}");
}

#[tokio::test]
async fn an_invalid_policy_is_rejected_without_saving() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for form in [
        "posture=nonsense",
        "posture=allowlist&allow=no-dot",
        "posture=allowlist&allow=10.0.0.0%2F99",
    ] {
        let res = app
            .clone()
            .oneshot(put_form("/api/boxes/alpha/policy", form))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "{form} should be rejected"
        );
        assert!(body_string(res).await.contains("Invalid policy"));
    }
}

#[tokio::test]
async fn policy_editing_requires_a_token() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let req = Request::builder()
        .method("PUT")
        .uri("/api/boxes/alpha/policy")
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::from("posture=isolated"))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ── activity, behaviour, metrics ─────────────────────────

#[tokio::test]
async fn activity_tab_renders_before_any_capture() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/boxes/alpha?tab=activity"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(html.contains("No observability data yet"));
    assert!(html.contains("devbox-obsd"));
}

#[tokio::test]
async fn behavior_endpoint_is_404_before_any_capture() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/api/boxes/alpha/behavior"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_are_scrapeable_without_a_token() {
    let (_dir, app) = console_with_boxes(&["alpha", "beta"]);

    // Prometheus scrapes with no cookie; /metrics exposes counts and
    // statuses, never box contents.
    let req = Request::builder()
        .uri("/metrics")
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );

    let body = body_string(res).await;
    assert!(body.contains("# TYPE devbox_events_dropped_total counter"));
    assert!(body.contains("devbox_events_dropped_total 0"));
    assert!(body.contains("devbox_build_info{version="));
    // Two boxes, both with an unresolvable runtime, so both report unknown.
    assert!(
        body.contains("devbox_boxes{status=\"unknown\"} 2"),
        "got: {body}"
    );
}

// ── help ─────────────────────────────────────────────────

#[tokio::test]
async fn help_index_lists_topics() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/help")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(html.contains("href=\"/help/lazygit\""));
    assert!(html.contains("href=\"/help/git\""));
    assert!(html.contains("cheat sheets"));
}

#[tokio::test]
async fn help_topic_renders_markdown_as_html() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/help/git")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let html = body_string(res).await;
    assert!(
        html.contains("<h1>"),
        "markdown must be rendered, not escaped"
    );
    assert!(html.contains("class=\"prose\""));
}

#[tokio::test]
async fn help_topic_404s_for_an_unknown_sheet() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/help/nonexistent")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ── sse ──────────────────────────────────────────────────

#[tokio::test]
async fn stream_emits_a_heartbeat_tick() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get_authed("/api/stream")).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );

    let mut body = res.into_body().into_data_stream();
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), {
        use futures::StreamExt;
        body.next()
    })
    .await
    .expect("a tick within 5s")
    .expect("stream is not empty")
    .expect("chunk is readable");

    let text = String::from_utf8_lossy(&chunk);
    assert!(text.contains("event: tick"), "got: {text}");
    assert!(text.contains("data: live · "), "got: {text}");
}

// ── escaping ─────────────────────────────────────────────

/// The payload every one of these tests pushes through a different door.
const HOSTILE: &str = "<script>alert(1)</script>";

fn assert_not_executable(where_: &str, body: &str) {
    assert!(
        !body.contains("<script>"),
        "{where_} returned an unescaped script tag, which htmx inserts into \
         the console DOM:\n{body}"
    );
    assert!(
        body.contains("&lt;script&gt;"),
        "{where_} should still show the user what they typed, escaped:\n{body}"
    );
}

// Round 22 configured htmx to swap 4xx bodies, because the console's actionable
// errors are 4xx fragments and none of them were reaching the user. It did not
// ask what those bodies contain. They quote the thing that was rejected —
// a package name, an allowlist entry, a box name off the path — straight from
// the input, so the swap turned every rejection into script execution in a page
// holding an authenticated session.
//
// One test per door, because the escaping being right in the handler I was
// looking at is exactly the assumption that has failed in this codebase before.

#[tokio::test]
async fn a_rejected_package_name_comes_back_escaped() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            "/api/boxes/alpha/sets",
            &format!("packages={}", urlencode(HOSTILE)),
        ))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_not_executable("the selection validator", &body_string(res).await);
}

#[tokio::test]
async fn a_rejected_set_name_comes_back_escaped() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            "/api/boxes/alpha/sets",
            &format!("set={}", urlencode(HOSTILE)),
        ))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_not_executable("the set validator", &body_string(res).await);
}

#[tokio::test]
async fn a_rejected_allowlist_entry_comes_back_escaped() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(put_form(
            "/api/boxes/alpha/policy",
            &format!("posture=allowlist&allow={}", urlencode(HOSTILE)),
        ))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_not_executable("the policy validator", &body_string(res).await);
}

#[tokio::test]
async fn an_unknown_box_name_comes_back_escaped() {
    // 404 is a 4xx too, and this one echoes a path segment. A `text/plain`
    // body is no defence: htmx inserts the response text as HTML whatever the
    // content type says.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_form(
            &format!("/api/boxes/{}/sets", urlencode(HOSTILE)),
            "set=system",
        ))
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_not_executable("the unknown-box path", &body_string(res).await);
}

/// Percent-encode enough for these payloads to survive a form body and a path.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── template selectors ───────────────────────────────────

#[test]
fn no_template_builds_a_css_selector_from_a_box_name() {
    // `#box-{{ b.name }}` reads as obviously correct and breaks on a name the
    // console itself accepts. A box name is a directory name, and
    // `is_safe_name` permits dots and spaces — both of which mean something
    // else in a selector. `#box-a_b.c` parses as the id `box-a_b` carrying the
    // class `c`, matches nothing, and every lifecycle button on that card
    // silently does nothing at all.
    //
    // Checked against the template source rather than a rendered page, because
    // there is nothing wrong with the rendered page: the markup is well-formed
    // and only the browser's selector parser disagrees. Nothing that renders
    // HTML can see this.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/templates");
    let mut offenders = Vec::new();

    for entry in std::fs::read_dir(&dir).expect("templates directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable template");
        for (n, line) in text.lines().enumerate() {
            let targets = line.contains("hx-target=") || line.contains("hx-indicator=");
            if targets && line.contains("#box-{{") {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these build a CSS selector out of a box name; use a relative target \
         such as `closest .card`, which cannot be got wrong:\n{}",
        offenders.join("\n")
    );
}

// ── fetch metadata ───────────────────────────────────────

/// An authenticated request, with the browser's account of who initiated it.
fn get_initiated(uri: &str, site: &str, dest: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_7878={TOKEN}"))
        .header("sec-fetch-site", site)
        .header("sec-fetch-dest", dest)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_navigation_another_page_caused_is_refused() {
    // The gap the Origin check cannot close by itself. It has to allow a
    // missing `Origin`, since that is what a top-level navigation looks like —
    // so a page on another 127.0.0.1 port could send the browser to
    // `/boxes/alpha?tab=terminal` and let the rendered page's own load-fired
    // POST start the box. `same-site` is the case that matters, because a site
    // ignores the port: that other page *is* same-site with this console.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    for site in ["same-site", "cross-site"] {
        let res = app
            .clone()
            .oneshot(get_initiated("/boxes/alpha?tab=terminal", site, "document"))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "a {site} navigation must not be served"
        );
    }
}

#[tokio::test]
async fn the_user_opening_the_console_is_still_served() {
    // Typed, bookmarked, or opened by `devbox web`: `none`. And the console
    // navigating within itself: `same-origin`. Refusing these would make the
    // guard worse than the hole.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    for site in ["none", "same-origin"] {
        let res = app
            .clone()
            .oneshot(get_initiated("/boxes/alpha", site, "document"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{site} must be served");
    }
}

#[tokio::test]
async fn the_console_refuses_to_be_framed() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_initiated("/boxes/alpha", "same-origin", "iframe"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn every_served_response_forbids_framing() {
    // The other half, for a browser that does not send Fetch Metadata: say it
    // in the response instead, where the browser enforces it unprompted.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app.oneshot(get_authed("/boxes/alpha")).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
        "frame-ancestors 'none'"
    );
    assert_eq!(res.headers().get("x-frame-options").unwrap(), "DENY");
}

#[tokio::test]
async fn two_consoles_on_different_ports_do_not_evict_each_other() {
    // Cookies are not scoped by port, so both consoles shared one name: the
    // second to open overwrote the first's token, and every request from the
    // first page came back 401 — a console that had been working and simply
    // stopped, with nothing on the page able to say why.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    // A cookie minted by a console on another port is not this one's.
    let other_port = Request::builder()
        .uri("/")
        .header(header::HOST, "127.0.0.1:7878")
        .header(header::COOKIE, format!("devbox_console_9999={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(other_port).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "a cookie for another console must not authenticate this one"
    );

    // And both can be held at once, because the names differ.
    let both = Request::builder()
        .uri("/")
        .header(header::HOST, "127.0.0.1:7878")
        .header(
            header::COOKIE,
            format!("devbox_console_9999=other; devbox_console_7878={TOKEN}"),
        )
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(both).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "two consoles must be usable in one browser at the same time"
    );
}

#[tokio::test]
async fn the_launch_exchange_names_the_cookie_for_this_console() {
    // The write side has to agree with the read side, or the exchange hands
    // back a cookie the next request will not recognise.
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get(&format!("/?t={TOKEN}"))).await.unwrap();

    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cookie.starts_with(&format!("devbox_console_7878={TOKEN}")),
        "got: {cookie}"
    );
}
