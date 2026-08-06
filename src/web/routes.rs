//! HTTP routes and page handlers.
//!
//! Handlers stay thin: they call [`super::service`] and render. Anything with
//! real logic belongs in the service layer so the CLI can reuse it.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};

use super::service::{self, BoxSummary};
use super::state::AppState;
use super::{assets, auth, sse};

/// Build the console router. Exposed so tests can drive it without binding a
/// socket.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/boxes", get(api_boxes))
        .route("/api/boxes/{name}", get(api_box))
        .route("/api/stream", get(sse::stream))
        .route("/healthz", get(healthz))
        .route("/assets/{*path}", get(assets::handler))
        // Browsers request /favicon.ico unprompted; serve the embedded icon
        // rather than logging a 404 on every page load.
        .route("/favicon.ico", get(assets::favicon))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .with_state(state)
}

// ── pages ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    version: &'static str,
    nav: &'static str,
    subtitle: String,
    boxes: Vec<BoxSummary>,
}

/// Human-readable summary line under the page title.
pub fn dashboard_subtitle(boxes: &[BoxSummary]) -> String {
    let running = boxes.iter().filter(|b| b.status == "running").count();
    match boxes.len() {
        0 => "nothing registered yet".to_string(),
        1 if running == 1 => "1 box · running".to_string(),
        1 => "1 box · not running".to_string(),
        n => format!("{n} boxes · {running} running"),
    }
}

async fn dashboard(State(state): State<AppState>) -> Response {
    let boxes = match service::list_boxes(&state.manager).await {
        Ok(b) => b,
        Err(e) => return server_error("failed to list boxes", &e),
    };

    let page = DashboardTemplate {
        version: state.version,
        nav: "dashboard",
        subtitle: dashboard_subtitle(&boxes),
        boxes,
    };
    render(page)
}

// ── json api ─────────────────────────────────────────────

async fn api_boxes(State(state): State<AppState>) -> Response {
    match service::list_boxes(&state.manager).await {
        Ok(boxes) => Json(boxes).into_response(),
        Err(e) => server_error("failed to list boxes", &e),
    }
}

/// `GET /api/boxes/{name}` — one box, or 404 if it is not registered.
async fn api_box(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match service::get_box(&state.manager, &name).await {
        Ok(b) => Json(b).into_response(),
        // The manager reports both "no such box" and "unreadable state" as an
        // error; treating an unknown name as 404 is the useful distinction for
        // a client, and the detail is logged either way.
        Err(e) => {
            tracing::debug!(box_id = %name, error = ?e, "box lookup failed");
            (StatusCode::NOT_FOUND, format!("no such box: {name}")).into_response()
        }
    }
}

async fn healthz() -> &'static str {
    "ok"
}

// ── helpers ──────────────────────────────────────────────

/// Render an askama template, turning a template failure into a 500 rather
/// than a panic. Templates are compile-time checked, so this path is for
/// runtime formatting errors only.
fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template render failed").into_response()
        }
    }
}

fn server_error(context: &str, err: &anyhow::Error) -> Response {
    tracing::error!(error = ?err, "{context}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{context}: {err}"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, status: &str) -> BoxSummary {
        BoxSummary {
            name: name.into(),
            runtime: "docker".into(),
            project_dir: "/tmp/x".into(),
            status: status.into(),
            mount_mode: "overlay".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn subtitle_reads_naturally_at_each_count() {
        assert_eq!(dashboard_subtitle(&[]), "nothing registered yet");
        assert_eq!(
            dashboard_subtitle(&[summary("a", "running")]),
            "1 box · running"
        );
        assert_eq!(
            dashboard_subtitle(&[summary("a", "stopped")]),
            "1 box · not running"
        );
        assert_eq!(
            dashboard_subtitle(&[summary("a", "running"), summary("b", "stopped")]),
            "2 boxes · 1 running"
        );
    }

    #[test]
    fn dashboard_template_renders_boxes() {
        let page = DashboardTemplate {
            version: "0.1.3",
            nav: "dashboard",
            subtitle: "2 boxes · 1 running".into(),
            boxes: vec![summary("alpha", "running"), summary("beta", "stopped")],
        };
        let html = page.render().expect("template renders");
        assert!(html.contains("alpha"));
        assert!(html.contains("beta"));
        assert!(html.contains("status-running"));
        assert!(html.contains("status-stopped"));
        assert!(html.contains("sse-connect=\"/api/stream\""));
    }

    #[test]
    fn dashboard_template_renders_empty_state() {
        let page = DashboardTemplate {
            version: "0.1.3",
            nav: "dashboard",
            subtitle: "nothing registered yet".into(),
            boxes: vec![],
        };
        let html = page.render().expect("template renders");
        assert!(html.contains("No boxes yet"));
        assert!(html.contains("devbox create"));
    }

    #[test]
    fn template_escapes_box_names() {
        let mut evil = summary("<script>alert(1)</script>", "running");
        evil.project_dir = "/tmp/<img onerror=x>".into();
        let page = DashboardTemplate {
            version: "0.1.3",
            nav: "dashboard",
            subtitle: String::new(),
            boxes: vec![evil],
        };
        let html = page.render().expect("template renders");
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&#60;script&#62;"));
        assert!(!html.contains("<img onerror=x>"));
    }
}
