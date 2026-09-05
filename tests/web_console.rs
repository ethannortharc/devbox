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
/// Distinct from `TOKEN` on purpose: the console must not accept one for the
/// other, and a shared constant here would let it and never say so.
const KEY: &str = "test-key-9876543210";

/// A state dir seeded with two boxes, plus a router wired to it.
fn console_with_boxes(names: &[&str]) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().expect("temp dir");
    for name in names {
        // A real project directory: the policy editor writes devbox.toml there,
        // so a fake path would make that path untestable.
        let project = dir.path().join("projects").join(name);
        std::fs::create_dir_all(&project).expect("project dir");

        SandboxState {
            schema: devbox::sandbox::state::SCHEMA,
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
    let router = routes::router(AppState::new(manager, TOKEN, KEY));
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
        .header("x-devbox-key", KEY)
        .body(Body::empty())
        .unwrap()
}

fn post_authed(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", KEY)
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", KEY)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn the_light_theme_keeps_the_green_product_identity_readable() {
    let css = include_str!("../src/web/assets/css/app.css");
    let start = css
        .find("@media (prefers-color-scheme: light)")
        .expect("light theme exists");
    let rest = &css[start..];
    let end = rest
        .find("\n}\n\n*")
        .expect("light theme closes before the universal selector");
    let light = &rest[..end];

    assert!(
        light.contains("--bg: #f5f8f3;") && light.contains("--bg-soft: #dfeadd;"),
        "the light theme must use its warm green surfaces"
    );
    assert!(
        light.contains("--accent: #2f7f4d;")
            && light.contains("--accent-hover: #286d43;")
            && light.contains("--accent-ink: #ffffff;"),
        "the light theme needs a readable green primary-control palette"
    );
}

#[test]
fn terminal_and_favicon_use_the_same_green_identity() {
    let terminal = include_str!("../src/web/assets/js/term.js");
    let favicon = include_str!("../src/web/assets/favicon.svg");

    assert!(terminal.contains(r##"cursor: "#a7ed69""##));
    assert!(terminal.contains(r##"cursor: "#2f7f4d""##));
    assert!(favicon.contains(r##"stroke="#a7ed69""##));
    for old_blue in ["#0969da", "#4493f8"] {
        assert!(!terminal.contains(old_blue), "terminal retained {old_blue}");
        assert!(!favicon.contains(old_blue), "favicon retained {old_blue}");
    }
}

#[test]
fn current_launch_origin_is_shared_across_independent_tabs() {
    let key_store = include_str!("../src/web/assets/js/key.js");

    assert!(
        key_store.contains("window.localStorage.getItem(NAME)")
            && key_store.contains("window.localStorage.setItem(NAME, value)")
            && key_store.contains("window.localStorage.removeItem(NAME)"),
        "independently opened tabs on the random launch origin must share the key"
    );
    assert!(
        !key_store.contains("window.sessionStorage.setItem")
            && !key_store.contains("document.cookie"),
        "the shared key must not fall back to a cookie or per-tab storage"
    );
}

#[test]
fn an_unconnected_browser_profile_gets_actionable_guidance() {
    let shell = include_str!("../src/web/assets/js/shell.js");
    assert!(shell.contains("This browser profile is not authorized"));
    assert!(shell.contains("Each browser profile must open the launch URL once"));
    assert!(shell.contains("after that, every tab in it can use the bound loopback address"));
}

#[tokio::test]
async fn every_authenticated_page_offers_the_multi_tab_launcher() {
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for uri in ["/", "/help", "/boxes/alpha"] {
        let page = body_string(app.clone().oneshot(get_authed(uri)).await.unwrap()).await;
        assert!(
            page.contains(r#"id="open-console-tab""#),
            "{uri} is missing the New tab control"
        );
        assert!(
            page.contains(r#"id="open-console-tab" href="" target="_blank""#)
                && page.contains(r#"rel="noopener""#),
            "{uri} does not expose a native, opener-free new-tab link"
        );
    }
}

#[tokio::test]
async fn lifecycle_controls_render_as_one_named_control_rail() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let page = body_string(app.oneshot(get_authed("/boxes/alpha")).await.unwrap()).await;

    assert!(page.contains(r#"class="actions box-actions" role="group""#));
    assert!(page.contains(r#"aria-label="Actions for alpha""#));

    let css = include_str!("../src/web/assets/css/app.css");
    assert!(css.contains("grid-template-columns: repeat(4, minmax(0, 1fr));"));
    assert!(css.contains(".box-actions .btn-danger"));
    assert!(css.contains("grid-template-columns: repeat(2, minmax(0, 1fr));"));
}

// ── auth ─────────────────────────────────────────────────

#[tokio::test]
async fn the_dashboard_itself_is_never_served_without_the_key() {
    // This used to assert a 401 on `GET /`, and a navigation now gets the
    // shell instead — so the assertion has to move to the thing that actually
    // mattered, which is that no box reaches an unkeyed caller. A status code
    // was only ever standing in for that.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    let unkeyed = body_string(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(!unkeyed.contains("alpha"), "shell leaked a box: {unkeyed}");

    let keyed = body_string(app.oneshot(get_authed("/")).await.unwrap()).await;
    assert!(keyed.contains("alpha"), "the real dashboard renders it");
}

#[tokio::test]
async fn a_token_that_is_not_the_token_buys_nothing() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get("/?t=not-the-token")).await.unwrap();

    // A shell, not a bootstrap: the request is shaped like a navigation, so it
    // is answered like one. What it must not contain is the key.
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(!body.contains(KEY), "a wrong token minted a key: {body}");
    assert!(body.contains("/assets/js/shell.js"));
}

#[tokio::test]
async fn launch_token_is_exchanged_for_the_console_key() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app.oneshot(get(&format!("/?t={TOKEN}"))).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    // No `Set-Cookie`, ever again. A cookie is scoped by host and not by port,
    // so the browser handed the console's to every other service on
    // 127.0.0.1 — a project's own dev server could read the credential out of
    // its inbound headers and drive the console with it.
    assert!(
        res.headers().get(header::SET_COOKIE).is_none(),
        "the console must not mint a cookie"
    );

    let body = body_string(res).await;
    assert!(
        body.contains(&format!(r#"content="{KEY}""#)),
        "the bootstrap page must carry the key: {body}"
    );
    assert!(body.contains(r#"content="/""#), "and the clean target");
    // The token buys this page and nothing else, so it must not be left in the
    // page for anything later to reuse.
    assert!(
        !body.contains(TOKEN),
        "the bootstrap page must not echo the token: {body}"
    );
}

fn console_on_random_browser_origin() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().expect("temp dir");
    let manager = Arc::new(SandboxManager {
        state_dir: dir.path().to_path_buf(),
    });
    let app = routes::router(AppState::new_with_browser_origin(
        manager,
        TOKEN,
        KEY,
        "devbox-0123456789abcdef0123456789abcdef.localhost",
        7878,
    ));
    (dir, app)
}

#[tokio::test]
async fn independently_opened_loopback_tabs_enter_the_current_launch_origin() {
    let (_dir, app) = console_on_random_browser_origin();

    let res = app
        .clone()
        .oneshot(get("/boxes/alpha?tab=terminal"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        res.headers().get(header::LOCATION).unwrap(),
        "http://devbox-0123456789abcdef0123456789abcdef.localhost:7878/boxes/alpha?tab=terminal"
    );
    assert_eq!(
        res.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    // Even an explicitly typed token URL first moves off the predictable
    // origin; the key must never be installed on 127.0.0.1:7878.
    let res = app
        .clone()
        .oneshot(get(&format!("/?t={TOKEN}")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        res.headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("http://devbox-0123456789abcdef0123456789abcdef.localhost:7878/?t={TOKEN}")
    );

    let launch = Request::builder()
        .uri(format!("/?t={TOKEN}"))
        .header(
            header::HOST,
            "devbox-0123456789abcdef0123456789abcdef.localhost:7878",
        )
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(launch).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        body_string(res)
            .await
            .contains(&format!(r#"content="{KEY}""#))
    );
}

#[tokio::test]
async fn another_random_localhost_name_cannot_rebind_the_console() {
    let (_dir, app) = console_on_random_browser_origin();
    let req = Request::builder()
        .uri("/")
        .header(
            header::HOST,
            "devbox-ffffffffffffffffffffffffffffffff.localhost:7878",
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::MISDIRECTED_REQUEST
    );
}

#[tokio::test]
async fn foreign_navigation_is_not_redirected_toward_an_authorized_launch_origin() {
    let (_dir, app) = console_on_random_browser_origin();
    let req = Request::builder()
        .uri("/boxes/alpha?tab=terminal")
        .header(header::HOST, "127.0.0.1:7878")
        .header("sec-fetch-site", "cross-site")
        .header("sec-fetch-dest", "document")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.headers().get(header::LOCATION).is_none());
}

#[tokio::test]
async fn a_bare_navigation_gets_a_shell_that_holds_nothing() {
    // A top-level navigation sets no headers, so it cannot present the key.
    // It is answered rather than refused — and what it is answered with is the
    // reason that is safe.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app.oneshot(get("/")).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("/assets/js/shell.js"));
    assert!(
        !body.contains("alpha"),
        "the shell must not name a box: {body}"
    );
    assert!(!body.contains(KEY), "nor carry the key: {body}");
}

#[tokio::test]
async fn the_shell_is_all_a_stolen_credential_could_ever_reach() {
    // The finding this replaced the cookie for. Whatever a hostile loopback
    // service manages to scrape, it must not be able to read a box or touch
    // one — so the data routes answer to the key and nothing else.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for uri in ["/api/boxes", "/api/boxes/alpha", "/api/boxes/alpha/files"] {
        let res = app.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} must demand the key"
        );
    }

    // Including the lifecycle verbs, which is what made this a P1 rather than
    // an information leak.
    let stop = Request::builder()
        .method("POST")
        .uri("/api/boxes/alpha/stop")
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(stop).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // And a page POST is not a navigation, so it gets no shell either.
    let post_page = Request::builder()
        .method("POST")
        .uri("/")
        .header(header::HOST, "127.0.0.1:7878")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(post_page).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_stale_key_is_refused_rather_than_handed_another_shell() {
    // Found in a browser, not here. A key from a previous launch fell into the
    // same branch as no key at all, so the shell's own fetch was answered with
    // a second shell, wrote it over itself, and left a blank page: no notice,
    // no console error, and the dead key still stored, so every reload
    // repeated it. The console looked broken in a way nothing could explain.
    //
    // Offering nothing is a navigation, which cannot present a header and gets
    // the shell. Offering the wrong thing is the shell reporting back, and has
    // to be told so.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    let stale = Request::builder()
        .uri("/boxes/alpha")
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", "a-key-from-a-console-that-has-restarted")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(stale).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "a presented-but-wrong key must be refused, not shelled"
    );

    // The navigation it must not be confused with still works.
    let res = app.oneshot(get("/boxes/alpha")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("/assets/js/shell.js"));
}

#[tokio::test]
async fn the_token_is_not_the_key_and_neither_stands_in_for_the_other() {
    // If either recovered the other, the separation would be decoration — and
    // that is exactly how the cookie failed, its value having been the token.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    let as_key = Request::builder()
        .uri("/api/boxes")
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", TOKEN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(as_key).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "the launch token must not work as the key"
    );

    let as_token = app.oneshot(get(&format!("/?t={KEY}"))).await.unwrap();
    assert_eq!(
        as_token.status(),
        StatusCode::OK,
        "an unusable token still yields a shell, not a bootstrap"
    );
    let body = body_string(as_token).await;
    assert!(
        !body.contains(KEY),
        "the key must not bootstrap itself: {body}"
    );
}

#[tokio::test]
async fn launch_token_preserves_other_query_parameters() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app
        .oneshot(get(&format!("/?t={TOKEN}&tab=activity")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(
        body.contains(r#"content="/?tab=activity""#),
        "the bootstrap page must land where the user was sent: {body}"
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
        .header("x-devbox-key", KEY)
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

#[tokio::test]
async fn create_and_pcap_routes_validate_real_http_requests() {
    let (dir, app) = console_with_boxes(&["alpha"]);

    let create_page = app.clone().oneshot(get_authed("/boxes/new")).await.unwrap();
    assert_eq!(create_page.status(), StatusCode::OK);
    let create_body = body_string(create_page).await;
    assert!(create_body.contains("/api/boxes"));
    assert!(create_body.contains("Docker — bare Ubuntu / writable only"));
    assert!(create_body.contains("id=\"docker-contract\""));
    assert!(create_body.contains("image.value = \"ubuntu\""));
    assert!(create_body.contains("mount.value = \"writable\""));
    assert!(create_body.contains("bare.checked = true"));
    assert!(
        !create_body.contains("value=\"multipass\""),
        "the create UI must not offer a runtime that cannot satisfy its contract"
    );

    let missing_project = app
        .clone()
        .oneshot(post_form("/api/boxes", "name=beta&runtime=docker"))
        .await
        .unwrap();
    assert_eq!(missing_project.status(), StatusCode::BAD_REQUEST);

    let duplicate_form = form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "project_dir",
            &dir.path().join("projects/alpha").display().to_string(),
        )
        .append_pair("name", "alpha")
        .append_pair("runtime", "docker")
        .append_pair("image", "ubuntu")
        .append_pair("mount_mode", "writable")
        .append_pair("bare", "on")
        .finish();
    let duplicate = app
        .clone()
        .oneshot(post_form("/api/boxes", &duplicate_form))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    // The network-lab console was removed in v5. An authenticated request for
    // what it used to serve must be an honest 404 — not a 500 from a
    // half-removed route, and not a blank 200 that reads as "there are none".
    for uri in ["/labs", "/labs/ztp-fabric", "/api/labs/ztp-fabric/view"] {
        let response = app.clone().oneshot(get_authed(uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }

    let invalid_capture = app
        .oneshot(post_authed(
            "/api/boxes/alpha/flows/pcap?proto=tcp&daddr=203.0.113.1&dport=443&duration=0",
        ))
        .await
        .unwrap();
    assert_eq!(invalid_capture.status(), StatusCode::CONFLICT);
    assert!(
        body_string(invalid_capture)
            .await
            .contains("capture duration")
    );
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
        let body = body_string(res).await;
        assert!(body.contains("alpha"));
        assert!(body.contains("/api/boxes/alpha/start"));
        assert!(body.contains("/api/boxes/alpha/destroy?force=true"));
        assert!(body.contains("uncommitted and stashed overlay changes"));
        assert!(body.contains("role=\"alert\""));
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

#[tokio::test]
async fn terminal_start_returns_an_actionable_card_the_page_can_reconcile() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(post_authed("/api/boxes/alpha/start?terminal=true"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = body_string(res).await;
    assert!(body.contains("Could not start"));
    assert!(body.contains("role=\"alert\""));
    assert!(body.contains("/api/boxes/alpha/destroy?force=true"));
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
    assert!(
        html.contains("id=\"build-panel\" role=\"status\" aria-live=\"polite\""),
        "rebuild completion and safety warnings must be announced"
    );
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
    assert!(html.contains("/api/operations/alpha/status"));
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
        .header("x-devbox-key", KEY)
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
async fn activity_tab_says_why_it_is_empty_rather_than_only_that_it_is() {
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let res = app
        .oneshot(get_authed("/boxes/alpha?tab=activity"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // No collector daemon owns this temporary state directory's lock, and that
    // outranks every other reading: nothing is being recorded for any box, so
    // "no data" is a fact about the host and not about this box.
    let html = body_string(res).await;
    assert!(html.contains("capture-down"), "{html}");
    assert!(html.contains("Collector is not running"));
    assert!(html.contains("devbox doctor"));
}

#[tokio::test]
async fn the_activity_fragments_refuse_a_box_that_is_not_registered() {
    // Observability files live outside `sandboxes/<name>` on purpose, so a
    // crash between removing a sandbox and removing its store leaves an
    // orphan. These routes read a store by name and never look a box up, so
    // without a registry check they would serve it while the box page 404s.
    let (dir, app) = console_with_boxes(&["alpha"]);
    let ghost = dir.path().join("boxes").join("ghost");
    std::fs::create_dir_all(&ghost).expect("orphan directory");
    devbox::obs::Store::open(&ghost.join("events.db")).expect("orphan store");

    for path in [
        "/api/boxes/ghost/activity",
        "/api/boxes/ghost/activity/live",
        "/api/boxes/ghost/activity/tail",
        // The export is the one that would hand over the whole record.
        "/api/boxes/ghost/behavior",
        "/api/boxes/ghost/behavior?format=jsonl",
    ] {
        let res = app.clone().oneshot(get_authed(path)).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_box_whose_agent_never_started_names_the_fix_on_the_page() {
    use devbox::obs::health::{self, CaptureHealth, CaptureState};

    let (dir, app) = console_with_boxes(&["alpha"]);
    // What the supervisor writes for a box provisioned before v4: the agent
    // binary is not in the guest, so the exec closes before the handshake.
    health::publish(
        dir.path(),
        &CaptureHealth::new("alpha", CaptureState::Failed)
            .with_transport("exec")
            .with_detail(
                "agent closed the connection before saying hello — the agent said: \
                 sh: /usr/local/bin/devbox-obsd: not found",
            )
            .with_attempts(9),
    )
    .expect("publish capture health");

    // A held lock plus a published identity is exactly what a running collector
    // daemon looks like from the outside, and the daemon check outranks the
    // record — without this the page would (correctly) report the host problem
    // instead of this box's.
    let locks = dir.path().join("locks");
    std::fs::create_dir_all(&locks).expect("locks dir");
    std::fs::write(
        locks.join("collector-daemon.owner"),
        format!("pid={} version=0.1.3\n", std::process::id()),
    )
    .expect("owner record");
    let held = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(locks.join("collector-daemon.lock"))
        .expect("daemon lock");
    held.lock().expect("hold the daemon lock");

    let html = body_string(
        app.oneshot(get_authed("/boxes/alpha?tab=activity"))
            .await
            .unwrap(),
    )
    .await;

    assert!(html.contains("The agent could not start"), "{html}");
    // The guest's own words, which used to live only in collector.log…
    assert!(html.contains("devbox-obsd: not found"));
    // …and what they imply.
    assert!(html.contains("devbox reprovision"));
    assert!(html.contains("capture-down"));

    std::fs::File::unlock(&held).expect("release the daemon lock");
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

#[tokio::test]
async fn an_unknown_help_topic_is_escaped_before_the_shell_writes_it() {
    let (_dir, app) = console_with_boxes(&[]);
    let res = app
        .oneshot(get_authed(&format!("/help/{}", urlencode(HOSTILE))))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_not_executable("the unknown help topic", &body_string(res).await);
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
    let text = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        use futures::StreamExt;
        let mut received = String::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.expect("chunk is readable");
            received.push_str(&String::from_utf8_lossy(&chunk));
            if received.contains("event: tick") {
                return received;
            }
        }
        panic!("stream ended before a heartbeat: {received}");
    })
    .await
    .expect("a tick within 5s");

    // A reconnect snapshot is deliberately sent before or alongside the
    // heartbeat. The assertion must inspect the stream, not assume the first
    // transport chunk contains a tick.
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
        .header("x-devbox-key", KEY)
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
async fn another_consoles_key_does_not_open_this_one() {
    // Two consoles side by side used to be a problem worth a test of its own:
    // cookies are not scoped by port, so both shared one name and the second
    // to open evicted the first, which then 401'd with nothing on the page
    // able to explain it. Port-suffixing the name fixed the eviction and left
    // the leak — the browser still *sent* the cookie to every other loopback
    // service, which is the finding that removed cookies altogether.
    //
    // Each live console uses its own random browser origin, so neither can see
    // the other's stored key and there is nothing left to evict. What remains
    // worth asserting is the server half: this console answers to its own key
    // and to no other.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    let foreign = Request::builder()
        .uri("/api/boxes")
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", "a-key-from-the-console-on-9999")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(foreign).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "another console's key must not authenticate this one"
    );

    assert_eq!(
        app.oneshot(get_authed("/api/boxes"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "and this console's own key must still work"
    );
}

#[test]
fn no_test_authenticates_with_a_cookie() {
    // The console accepts no cookie at all any more, so a test that sends one
    // is testing a door that is not there — and would pass by taking the
    // unauthenticated path while appearing to exercise the authenticated one.
    //
    // This runs unconditionally because the test it is really watching does
    // not. Round 31 scoped the cookie name to the port, updated the tests it
    // could see run — these — and missed `e2e_docker.rs`, which skips when
    // Docker is absent. The suite stayed green locally while every request in
    // that file would have 401'd on CI. The same trap is open now, one
    // credential later: a guard that runs whatever is installed is the only
    // kind that covers a test that does not.
    let mut offenders = Vec::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

    for entry in std::fs::read_dir(&dir).expect("tests directory") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable test");
        for (n, line) in text.lines().enumerate() {
            // Prose may name it; only code may not send it.
            if line.trim_start().starts_with("//") || line.trim_start().starts_with("///") {
                continue;
            }
            // Assembled, so this line is not itself an instance of what it
            // looks for.
            let sends_cookie = line.contains(concat!("header::", "COOKIE"))
                || line.contains(concat!("\"Cookie", ":"))
                || line.contains(concat!("devbox_console", "="));
            if sends_cookie {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these authenticate with a cookie, which the console no longer reads; \
         send `X-Devbox-Key`, or `?k=` where a header is impossible:\n{}",
        offenders.join("\n")
    );
}

/// Scripts that read the console key, discovered from the files themselves so
/// a new one is covered without anyone remembering to list it here.
fn key_consuming_scripts() -> Vec<String> {
    let js = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/assets/js");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&js).expect("js assets") {
        let path = entry.expect("entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "key.js" {
            continue;
        }
        if std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("devboxKey")
        {
            found.push(name);
        }
    }
    assert!(
        !found.is_empty(),
        "no script reads the key — check the scan"
    );
    found
}

/// Every document the console serves, by the route that produces it.
async fn every_served_document(app: &Router) -> Vec<(String, String)> {
    let mut docs = Vec::new();
    for (label, req) in [
        ("shell".to_string(), get("/")),
        ("bootstrap".to_string(), get(&format!("/?t={TOKEN}"))),
    ] {
        docs.push((
            label,
            body_string(app.clone().oneshot(req).await.unwrap()).await,
        ));
    }
    for uri in ["/", "/help", "/boxes/alpha", "/boxes/alpha?tab=terminal"] {
        let res = app.clone().oneshot(get_authed(uri)).await.unwrap();
        docs.push((uri.to_string(), body_string(res).await));
    }
    docs
}

#[tokio::test]
async fn every_document_parses_key_js_before_anything_that_reads_it() {
    // Asserted on what is served, not on the templates: `box_detail.html` loads
    // `term.js` and never mentions `key.js`, which arrives from the base it
    // extends. Checking the templates separately would call that a violation,
    // and checking only the base would miss the page that actually broke.
    let (_dir, app) = console_with_boxes(&["alpha"]);
    let consumers = key_consuming_scripts();

    for (name, doc) in every_served_document(&app).await {
        for c in &consumers {
            let Some(uses_at) = doc.find(&format!("/assets/js/{c}")) else {
                continue;
            };
            let key_at = doc.find("/assets/js/key.js").unwrap_or_else(|| {
                panic!("{name} loads {c}, which reads the key, but never loads key.js")
            });
            assert!(
                key_at < uses_at,
                "{name}: key.js must be parsed before {c}, which reads the key"
            );
        }
    }
}

#[tokio::test]
async fn no_console_script_is_deferred_or_async() {
    // `defer` is what broke the event stream, and it broke it silently: htmx
    // initialised `<body>` before `sse.js` registered the extension, htmx marks
    // a node initialised, and so the stream could never be connected at all.
    // The page rendered, every button worked, nothing was logged, and the
    // heartbeat simply never arrived.
    //
    // These documents are usually handed to the parser by the shell rather than
    // fetched by a navigation, and in that re-parse `defer` orders nothing.
    // Re-adding it would read as ordinary in a diff and fail no other test
    // here, which is exactly why this one scans for the attribute itself.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for (name, doc) in every_served_document(&app).await {
        for tag in doc.split("<script").skip(1) {
            let open = tag.split('>').next().unwrap_or_default();
            for attr in ["defer", "async"] {
                assert!(
                    !open.contains(attr),
                    "{name}: <script{open}> uses `{attr}`; the parser must do the ordering"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_foreign_navigation_is_refused_before_it_can_be_shelled() {
    // Round 40, and a CSRF this codebase had already fixed once. The terminal
    // tab starts its box with a POST *because* a side-effecting GET could be
    // provoked by a hostile page; putting the shell branch ahead of the
    // origin checks handed that attack straight back.
    //
    // Another site navigates the browser to the terminal tab. The navigation
    // carries no key, so it reached the shell — and `shell.js` then fetched
    // the page itself, same-origin and keyed, which is indistinguishable from
    // a real request. The shell laundered the foreign navigation, the rendered
    // page auto-posted `/start`, and the box came up unasked.
    let (_dir, app) = console_with_boxes(&["alpha"]);

    for site in ["cross-site", "same-site"] {
        let hostile = Request::builder()
            .uri("/boxes/alpha?tab=terminal")
            .header(header::HOST, "127.0.0.1:7878")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(hostile).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
            "a {site} navigation must not even get the shell"
        );
    }

    // Framing is refused for the same reason and by the same check.
    let framed = Request::builder()
        .uri("/boxes/alpha")
        .header(header::HOST, "127.0.0.1:7878")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-dest", "iframe")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(framed).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // What a real user does still works: typed or bookmarked is `none`, a link
    // inside the console is `same-origin`.
    for site in ["none", "same-origin"] {
        let genuine = Request::builder()
            .uri("/boxes/alpha?tab=terminal")
            .header(header::HOST, "127.0.0.1:7878")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(genuine).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{site} is the user");
        assert!(body_string(res).await.contains("/assets/js/shell.js"));
    }
}

#[tokio::test]
async fn the_printed_url_still_works_when_clicked_from_another_site() {
    // The bootstrap branch deliberately runs ahead of the origin checks, and
    // this is the case that forces it. Clicking the URL `devbox web` printed
    // out of a chat window or a webmail tab is a `cross-site` initiator —
    // which those checks refuse — so ordering them first would refuse the one
    // navigation the whole flow depends on, and the console would be
    // unreachable for anyone who did not paste the URL by hand.
    //
    // Safe because the branch is already gated on the launch token, and anyone
    // holding that can mint a key directly.
    let (_dir, app) = console_with_boxes(&[]);

    for site in ["cross-site", "same-site", "none"] {
        let clicked = Request::builder()
            .uri(format!("/?t={TOKEN}"))
            .header(header::HOST, "127.0.0.1:7878")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(clicked).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "the printed URL must survive a {site} initiator"
        );
        assert!(
            body_string(res)
                .await
                .contains(&format!(r#"content="{KEY}""#)),
            "and must actually install the key"
        );
    }

    // The guards still apply to everything the token does not cover: a
    // cross-site navigation without it gets nothing.
    let unprivileged = Request::builder()
        .uri("/")
        .header(header::HOST, "127.0.0.1:7878")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(unprivileged).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}
