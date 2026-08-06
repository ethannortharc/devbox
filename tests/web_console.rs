//! HTTP-level tests for the web console.
//!
//! These drive the real axum router through `tower::ServiceExt::oneshot`, so
//! auth, routing, rendering, and serialization are all exercised — without
//! binding a socket or needing a runtime.
//!
//! The fixture boxes use a runtime name no runtime claims, so status probes
//! fail fast and the tests never shell out to `docker`/`limactl`.

use std::path::PathBuf;
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
        SandboxState {
            name: (*name).to_string(),
            runtime: "test-null".to_string(),
            project_dir: PathBuf::from(format!("/tmp/projects/{name}")),
            created_at: "2026-08-06T00:00:00Z".to_string(),
            mount_mode: "overlay".to_string(),
            layout: "default".to_string(),
            sets: vec!["system".into(), "git".into()],
            languages: vec!["rust".into()],
            image: "nixos".to_string(),
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

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_authed(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::COOKIE, format!("devbox_console={TOKEN}"))
        .body(Body::empty())
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
    assert!(cookie.contains(&format!("devbox_console={TOKEN}")));
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
