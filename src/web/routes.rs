//! HTTP routes and page handlers.
//!
//! Handlers stay thin: they call [`super::service`] and render. Anything with
//! real logic belongs in the service layer so the CLI can reuse it.

use askama::Template;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::Deserialize;

use super::activity::{self, Activity, Flow, Lookup, StreamRow, TreeRow};
use super::help::{self, Topic};
use super::service::{self, BoxSummary, FileChange, PolicyView, SetGroup};
use super::state::AppState;
use super::{assets, auth, build, sse, term};
use crate::nix::compose::Selection;

/// Build the console router. Exposed so tests can drive it without binding a
/// socket.
pub fn router(state: AppState) -> Router {
    Router::new()
        // pages
        .route("/", get(dashboard))
        .route("/boxes/{name}", get(box_detail))
        .route("/help", get(help_index))
        .route("/help/{topic}", get(help_topic))
        // json + fragments
        .route("/api/boxes", get(api_boxes))
        .route("/api/boxes/{name}", get(api_box))
        .route("/api/boxes/{name}/start", post(start_box))
        .route("/api/boxes/{name}/stop", post(stop_box))
        .route("/api/boxes/{name}/destroy", post(destroy_box))
        .route("/api/boxes/{name}/sets", post(apply_sets))
        .route("/api/boxes/{name}/policy", get(get_policy).put(put_policy))
        .route("/api/boxes/{name}/files", get(box_files))
        .route("/api/boxes/{name}/term", get(box_terminal))
        .route("/api/boxes/{name}/activity", get(box_activity))
        .route("/api/boxes/{name}/behavior", get(box_behavior))
        .route("/api/stream", get(sse::stream))
        // static
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
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

// ── dashboard ────────────────────────────────────────────

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

    render(DashboardTemplate {
        version: state.version,
        nav: "dashboard",
        subtitle: dashboard_subtitle(&boxes),
        boxes,
    })
}

// ── box detail ───────────────────────────────────────────

#[derive(Template)]
#[template(path = "box_detail.html")]
struct BoxDetailTemplate {
    version: &'static str,
    nav: &'static str,
    tab: &'static str,
    boxinfo: BoxSummary,
    groups: Vec<SetGroup>,
    extra_packages: String,
    policy: PolicyView,
    stream: Vec<StreamRow>,
    flows: Vec<Flow>,
    lookups: Vec<Lookup>,
    tree: Vec<TreeRow>,
    behavior: String,
    has_store: bool,
    /// A build status published before this page loaded.
    retained_build: String,
}

#[derive(Debug, Deserialize)]
pub struct TabQuery {
    tab: Option<String>,
}

/// Resolve the requested tab, defaulting to `overview`.
///
/// An unknown tab name falls back rather than 404ing: a stale bookmark should
/// land on the box, not on an error page.
pub fn resolve_tab(requested: Option<&str>) -> &'static str {
    match requested {
        Some("activity") => "activity",
        Some("sets") => "sets",
        Some("policy") => "policy",
        Some("files") => "files",
        Some("terminal") => "terminal",
        _ => "overview",
    }
}

async fn box_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TabQuery>,
) -> Response {
    let tab = resolve_tab(q.tab.as_deref());

    // No lazy start here.
    //
    // §6.3 wants opening the terminal to start the box, and this was the
    // obvious place — but it made a GET have a side effect, and the auth
    // middleware permits a request with no `Origin` because that is what an
    // ordinary top-level navigation looks like. A subresource or iframe GET
    // from a hostile page on another 127.0.0.1 port also carries no Origin,
    // is same-site enough for the browser to attach the session cookie, and
    // could therefore start any box whose name it could guess.
    //
    // The terminal panel now starts the box through an origin-checked POST
    // once it loads, so the behaviour §6.3 asks for survives without a
    // side-effecting GET.

    let boxinfo = match service::get_box(&state.manager, &name).await {
        Ok(b) => b,
        Err(e) => return not_found(&name, &e),
    };

    // Rebuild the selection from persisted state, packages included — the
    // form posts the *whole* selection back, so anything missing here is
    // silently dropped on the next apply.
    let selection = match state.manager.get_sandbox(&name) {
        Ok(s) => Selection::from_state(&s),
        Err(_) => Selection::default(),
    };

    // Only the Activity tab pays for reading the event store.
    let act = if tab == "activity" {
        activity::load(&state.manager, &name, activity::INITIAL_EVENTS).unwrap_or_else(|e| {
            tracing::warn!(box_id = %name, error = %e, "could not load activity");
            empty_activity()
        })
    } else {
        empty_activity()
    };

    render(BoxDetailTemplate {
        version: state.version,
        nav: "dashboard",
        tab,
        groups: service::set_groups(&selection),
        extra_packages: selection
            .packages
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        // `unwrap_or_default()` here rendered a malformed devbox.toml as the
        // default `open` posture, so the page showed "Open" for a box whose
        // firewall was still restrictive — the opposite of the truth, and the
        // one place the user goes to check.
        policy: match service::load_policy(&state.manager, &name) {
            Ok(policy) => service::policy_view(&policy),
            Err(e) => service::policy_view_error(&e.to_string()),
        },
        boxinfo,
        behavior: crate::obs::behavior::render_markdown(&act.summary),
        stream: act.stream,
        flows: act.flows,
        lookups: act.lookups,
        tree: act.tree,
        has_store: act.has_store,
        retained_build: state.retained_build_status(&name).unwrap_or_default(),
    })
}

fn empty_activity() -> Activity {
    Activity {
        events: vec![],
        stream: vec![],
        flows: vec![],
        lookups: vec![],
        tree: vec![],
        summary: Default::default(),
        has_store: false,
    }
}

// ── activity ─────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_activity_stream.html")]
struct ActivityStreamFragment {
    stream: Vec<StreamRow>,
}

/// `GET /api/boxes/{name}/activity` — the live stream fragment, refreshed by
/// htmx while the tab is open.
async fn box_activity(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match activity::load(&state.manager, &name, activity::INITIAL_EVENTS) {
        Ok(act) => render(ActivityStreamFragment { stream: act.stream }),
        Err(e) => server_error("failed to read the event store", &e),
    }
}

/// `GET /api/boxes/{name}/behavior` — the run summary (§7.6).
///
/// `?format=json|markdown|jsonl` selects the export; the default is JSON so
/// the endpoint is useful to a script without a flag.
#[derive(Debug, Deserialize)]
pub struct BehaviorQuery {
    format: Option<String>,
    since: Option<String>,
}

async fn box_behavior(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<BehaviorQuery>,
) -> Response {
    let store = match activity::open_store(&state.manager, &name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("no events recorded for box '{name}' yet"),
            )
                .into_response();
        }
        Err(e) => return server_error("failed to open the event store", &e),
    };

    let events = match store.query(&crate::obs::store::Query {
        since: q.since.clone(),
        limit: Some(crate::obs::store::Query::MAX_LIMIT),
        ..Default::default()
    }) {
        Ok(e) => e,
        Err(e) => return server_error("failed to query events", &e),
    };

    let summary = crate::obs::behavior::summarize(&name, &events);

    match q.format.as_deref() {
        Some("markdown") => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            crate::obs::behavior::render_markdown(&summary),
        )
            .into_response(),
        Some("jsonl") => match crate::obs::behavior::render_jsonl(&events) {
            Ok(body) => (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-ndjson; charset=utf-8",
                )],
                body,
            )
                .into_response(),
            Err(e) => server_error("failed to export events", &anyhow::Error::from(e)),
        },
        _ => Json(summary).into_response(),
    }
}

// ── metrics ──────────────────────────────────────────────

/// `GET /metrics` — Prometheus exposition (§7.7).
async fn metrics(State(state): State<AppState>) -> Response {
    let boxes = service::list_boxes(&state.manager)
        .await
        .unwrap_or_default();

    let mut by_status: std::collections::BTreeMap<String, u64> = Default::default();
    for b in &boxes {
        *by_status.entry(b.status.clone()).or_default() += 1;
    }

    // Sum stored events across every box that has a store, so one scrape
    // describes the whole host rather than a single box.
    let mut by_type: std::collections::BTreeMap<crate::obs::EventType, u64> = Default::default();
    for b in &boxes {
        if let Ok(Some(store)) = activity::open_store(&state.manager, &b.name)
            && let Ok(counts) = store.count_by_type()
        {
            for (kind, n) in counts {
                *by_type.entry(kind).or_default() += n;
            }
        }
    }

    let body = crate::metrics::render(&crate::metrics::Snapshot {
        collector: state.collector_stats.snapshot(),
        events_by_type: by_type.into_iter().collect(),
        boxes_by_status: by_status.into_iter().collect(),
        version: state.version.to_string(),
    });

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

// ── lifecycle ────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_box_card.html")]
struct BoxCardFragment {
    b: BoxSummary,
}

/// Re-render a box's card, which is what every lifecycle action swaps in.
async fn card_after_action(state: &AppState, name: &str) -> Response {
    match service::get_box(&state.manager, name).await {
        Ok(b) => render(BoxCardFragment { b }),
        Err(e) => not_found(name, &e),
    }
}

async fn start_box(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(e) = service::start_box(&state.manager, &name).await {
        return action_error("start", &name, &e);
    }
    card_after_action(&state, &name).await
}

async fn stop_box(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(e) = service::stop_box(&state.manager, &name).await {
        return action_error("stop", &name, &e);
    }
    card_after_action(&state, &name).await
}

async fn destroy_box(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(e) = service::destroy_box(&state.manager, &name, false).await {
        return action_error("destroy", &name, &e);
    }

    // The box is gone, so there is no card to swap in. Empty body removes the
    // card; the redirect header sends the detail page back to the dashboard.
    (
        StatusCode::OK,
        [(
            HeaderName::from_static("hx-redirect"),
            HeaderValue::from_static("/"),
        )],
        Html(String::new()),
    )
        .into_response()
}

// ── sets ─────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_build_panel.html")]
struct BuildPanelFragment {
    name: String,
    summary: String,
    /// A status published before this panel existed; empty in the usual case.
    retained: String,
}

/// `POST /api/boxes/{name}/sets` — apply a set selection and rebuild.
///
/// Returns immediately with the log panel; the rebuild runs in the background
/// and streams into it over SSE. A `nixos-rebuild` can take minutes, so
/// holding the request open would just time out somewhere in between.
async fn apply_sets(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    if state.manager.get_sandbox(&name).is_err() {
        return (StatusCode::NOT_FOUND, format!("no such box: {name}")).into_response();
    }

    let selection = service::parse_selection_form(&body);
    if let Err(e) = selection.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Html(format!(
                "<div class=\"notice error\">Invalid selection: {e}</div>"
            )),
        )
            .into_response();
    }

    let summary = format!(
        "{} set(s), {} extra package(s)",
        selection.sets.len(),
        selection.packages.len()
    );

    // One rebuild per box at a time: two concurrent ones would overwrite the
    // same generated config and then each persist its own selection.
    let Some(guard) = state.claim_rebuild(&name) else {
        return (
            StatusCode::CONFLICT,
            Html(
                "<div class=\"notice error\">A rebuild is already running for this box. \
                 Wait for it to finish.</div>"
                    .to_string(),
            ),
        )
            .into_response();
    };

    let bg = state.clone();
    let box_name = name.clone();
    // A new build supersedes whatever the last one ended with.
    state.clear_build_status(&name);

    tokio::spawn(async move {
        let _guard = guard;
        let manager = bg.manager.clone();
        if let Err(e) = build::apply_selection(&manager, &bg, &box_name, &selection).await {
            tracing::warn!(box_id = %box_name, error = ?e, "rebuild failed");
            bg.publish(crate::web::state::ConsoleEvent::new(
                build::status_event(&box_name),
                format!(
                    "<span class=\"term-err\">{}</span>",
                    build::escape_html(&e.to_string())
                ),
            ));
        }
    });

    (
        StatusCode::ACCEPTED,
        render(BuildPanelFragment {
            retained: state.retained_build_status(&name).unwrap_or_default(),
            name,
            summary,
        }),
    )
        .into_response()
}

// ── policy ───────────────────────────────────────────────

/// `GET /api/boxes/{name}/policy` — the policy as JSON.
async fn get_policy(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match service::load_policy(&state.manager, &name) {
        Ok(policy) => Json(policy).into_response(),
        Err(e) => not_found(&name, &e),
    }
}

/// `PUT /api/boxes/{name}/policy` — edit the policy live (§8).
///
/// The form posts the *whole* allowlist, so an edit is a replacement; a merge
/// would make removing an entry in the UI do nothing.
async fn put_policy(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    let current = match service::load_policy(&state.manager, &name) {
        Ok(p) => p,
        Err(e) => return not_found(&name, &e),
    };

    let updated = match service::parse_policy_form(&body, &current) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Html(format!(
                    "<div class=\"notice error\">Invalid policy: {e}</div>"
                )),
            )
                .into_response();
        }
    };

    let posture = updated.egress;
    let entries = updated.allow.len();
    let policy = updated.clone();
    if let Err(e) = service::save_policy(&state.manager, &name, updated) {
        return server_error("failed to save the policy", &e);
    }

    // And apply it. The tab used to save the file and report the posture set,
    // then tell the user to reprovision — which never applied it either. The
    // console said one thing and the firewall did another.
    //
    // Saving and applying are reported separately because they fail
    // separately: the policy really is saved, and a user who is told only
    // "error" would not know whether to re-enter it.
    let applied = service::apply_policy_now(&state.manager, &name, &policy).await;

    let note = match (&applied, posture.enforces()) {
        // Deferred is its own answer: the box is off, so nothing was
        // firewalled, and saying "applied" would be false.
        (Ok(service::Applied::OnNextStart), _) => format!(
            "Saved: <strong>{posture}</strong> with {entries} allowlist entr{}. \
             The box is stopped; it applies when the box next starts.",
            if entries == 1 { "y" } else { "ies" }
        ),
        // The count appears in both branches: what was saved is a fact either
        // way, and the user needs to know their entries are recorded even when
        // the box could not be reached.
        (Err(e), _) => format!(
            "<span class=\"term-err\">Saved <strong>{posture}</strong> with {entries} \
             allowlist entr{}, but it is not active on the box: {}</span>",
            if entries == 1 { "y" } else { "ies" },
            build::escape_html(&e.to_string())
        ),
        (Ok(_), true) => format!(
            "Saved and applied: <strong>{posture}</strong> with {entries} allowlist entr{}.",
            if entries == 1 { "y" } else { "ies" }
        ),
        (Ok(_), false) => {
            format!("Saved: <strong>{posture}</strong>. Nothing is blocked in this posture.")
        }
    };

    let class = if applied.is_err() {
        "notice error"
    } else {
        "notice"
    };
    (
        StatusCode::OK,
        Html(format!("<div class=\"{class}\">{note}</div>")),
    )
        .into_response()
}

// ── files (overlay) ──────────────────────────────────────

#[derive(Template)]
#[template(path = "_file_changes.html")]
struct FileChangesFragment {
    changes: Vec<FileChange>,
}

async fn box_files(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match service::list_changes(&state.manager, &name).await {
        Ok(changes) => render(FileChangesFragment { changes }),
        Err(e) => server_error("failed to read overlay changes", &e),
    }
}

// ── terminal ─────────────────────────────────────────────

async fn box_terminal(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    // The box must be live before a shell can attach.
    if let Err(e) = service::ensure_running(&state.manager, &name).await {
        return action_error("open a terminal in", &name, &e);
    }

    let argv = match service::terminal_argv(&state.manager, &name).await {
        Ok(argv) => argv,
        Err(e) => return action_error("open a terminal in", &name, &e),
    };

    ws.on_upgrade(move |socket| async move {
        if let Err(e) = term::run_session(socket, argv, term::Size::default()).await {
            tracing::warn!(box_id = %name, error = %e, "terminal session ended with an error");
        }
    })
}

// ── help ─────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "help.html")]
struct HelpTemplate {
    version: &'static str,
    nav: &'static str,
    subtitle: String,
    current: String,
    topics: Vec<Topic>,
    body: String,
}

async fn help_index(State(state): State<AppState>) -> Response {
    let topics = help::topics();
    let body = help::source("index").map(help::to_html).unwrap_or_default();

    render(HelpTemplate {
        version: state.version,
        nav: "help",
        subtitle: format!("{} cheat sheets, offline", topics.len()),
        current: "index".to_string(),
        topics,
        body,
    })
}

async fn help_topic(State(state): State<AppState>, Path(topic): Path<String>) -> Response {
    let Some(markdown) = help::source(&topic) else {
        return (
            StatusCode::NOT_FOUND,
            format!("no cheat sheet for '{topic}'"),
        )
            .into_response();
    };
    // The index is reachable at /help; treat /help/index as the same page.
    let topics = help::topics();

    render(HelpTemplate {
        version: state.version,
        nav: "help",
        subtitle: help::title_of(markdown).unwrap_or_else(|| topic.clone()),
        current: topic,
        topics,
        body: help::to_html(markdown),
    })
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
        Err(e) => not_found(&name, &e),
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

/// The manager reports both "no such box" and "unreadable state" as an error;
/// treating an unknown name as 404 is the useful distinction for a client, and
/// the detail is logged either way.
fn not_found(name: &str, err: &anyhow::Error) -> Response {
    tracing::debug!(box_id = %name, error = ?err, "box lookup failed");
    (StatusCode::NOT_FOUND, format!("no such box: {name}")).into_response()
}

/// A failed lifecycle action. The message is shown to the user, so it carries
/// the underlying cause — these are things like "uncommitted overlay changes"
/// that the person can actually act on.
fn action_error(verb: &str, name: &str, err: &anyhow::Error) -> Response {
    tracing::warn!(box_id = %name, error = ?err, "failed to {verb} box");
    (
        StatusCode::CONFLICT,
        Html(format!(
            "<div class=\"notice error\">Could not {verb} <strong>{name}</strong>: {err}</div>"
        )),
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
            packages: vec![],
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
    fn unknown_tabs_fall_back_to_overview() {
        assert_eq!(resolve_tab(None), "overview");
        assert_eq!(resolve_tab(Some("overview")), "overview");
        assert_eq!(resolve_tab(Some("files")), "files");
        assert_eq!(resolve_tab(Some("terminal")), "terminal");
        assert_eq!(resolve_tab(Some("activity")), "activity");
        assert_eq!(resolve_tab(Some("nonsense")), "overview");
        assert_eq!(resolve_tab(Some("../../etc/passwd")), "overview");
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

    #[test]
    fn start_button_is_disabled_only_when_running() {
        let running = BoxCardFragment {
            b: summary("a", "running"),
        }
        .render()
        .unwrap();
        // Start disabled, Stop enabled.
        assert!(running.contains("/api/boxes/a/start"));
        let start_idx = running.find("/api/boxes/a/start").unwrap();
        let stop_idx = running.find("/api/boxes/a/stop").unwrap();
        assert!(running[start_idx..stop_idx].contains("disabled"));
        assert!(
            !running[stop_idx..]
                .split("</button>")
                .next()
                .unwrap()
                .contains("disabled")
        );

        let stopped = BoxCardFragment {
            b: summary("a", "stopped"),
        }
        .render()
        .unwrap();
        let start_idx = stopped.find("/api/boxes/a/start").unwrap();
        let stop_idx = stopped.find("/api/boxes/a/stop").unwrap();
        assert!(!stopped[start_idx..stop_idx].contains("disabled"));
        assert!(stopped[stop_idx..].contains("disabled"));
    }

    fn detail(tab: &'static str) -> BoxDetailTemplate {
        let selection = Selection::new(["system".to_string()], std::iter::empty());
        BoxDetailTemplate {
            retained_build: String::new(),
            version: "0.1.3",
            nav: "dashboard",
            tab,
            groups: service::set_groups(&selection),
            extra_packages: String::new(),
            policy: service::policy_view(&crate::policy::Policy::default()),
            boxinfo: summary("alpha", "running"),
            stream: vec![],
            flows: vec![],
            lookups: vec![],
            tree: vec![],
            behavior: String::new(),
            has_store: false,
        }
    }

    /// A detail page whose Activity tab has data to show.
    fn detail_with_activity() -> BoxDetailTemplate {
        let mut page = detail("activity");
        page.has_store = true;
        page.stream = vec![StreamRow {
            ts: "22:14:07.412".into(),
            kind: "connect".into(),
            domain: "network",
            pid: 812,
            comm: "pip".into(),
            summary: "connect pypi.org:443".into(),
        }];
        page.flows = vec![Flow {
            peer: "pypi.org".into(),
            addr: "151.101.0.223".into(),
            port: 443,
            proto: "tcp".into(),
            sni: "pypi.org".into(),
            alpn: "h2".into(),
            bytes_tx: 4102,
            bytes_rx: 831_720,
            dur_ms: 690,
            pid: 812,
            comm: "pip".into(),
            ts: "2026-08-06T22:14:07.412Z".into(),
            direction: "out",
        }];
        page.lookups = vec![Lookup {
            ts: "22:14:07.400".into(),
            name: "pypi.org".into(),
            qtype: "A".into(),
            answers: vec!["151.101.0.223".into()],
            pid: 812,
            comm: "pip".into(),
        }];
        page.behavior = "# Behavior summary".into();
        page
    }

    #[test]
    fn detail_template_renders_each_tab() {
        for (tab, needle) in [
            ("overview", "mount mode"),
            ("sets", "hx-post=\"/api/boxes/alpha/sets\""),
            ("policy", "hx-put=\"/api/boxes/alpha/policy\""),
            ("files", "hx-get=\"/api/boxes/alpha/files\""),
            ("terminal", "data-endpoint=\"/api/boxes/alpha/term\""),
        ] {
            let html = detail(tab).render().expect("template renders");
            assert!(html.contains(needle), "tab {tab} missing {needle}");
        }
    }

    #[test]
    fn sets_tab_renders_the_checklist_with_locked_entries_disabled() {
        let html = detail("sets").render().unwrap();

        assert!(html.contains("value=\"system\""));
        assert!(html.contains("value=\"lang-go\""));
        assert!(html.contains("Languages"));
        // `system` is locked: rendered checked *and* disabled.
        let sys = html.split("value=\"system\"").nth(1).unwrap();
        let sys_input = sys.split("/>").next().unwrap();
        assert!(sys_input.contains("disabled"), "got: {sys_input}");
        // A toggleable set must not be disabled.
        let go = html.split("value=\"lang-go\"").nth(1).unwrap();
        assert!(!go.split("/>").next().unwrap().contains("disabled"));
        // The live build log is wired to this box's SSE stream.
        assert!(html.contains("sse-swap=\"build-alpha\""));
        assert!(html.contains("hx-swap=\"beforeend scroll:bottom\""));
    }

    #[test]
    fn activity_tab_shows_an_empty_state_before_any_capture() {
        let html = detail("activity").render().unwrap();
        assert!(html.contains("No observability data yet"));
        assert!(html.contains("devbox-obsd"));
    }

    #[test]
    fn activity_tab_renders_every_view() {
        let html = detail_with_activity().render().unwrap();

        // Live stream is refreshed by htmx, scoped to this box.
        assert!(html.contains("hx-get=\"/api/boxes/alpha/activity\""));
        // Flow table.
        assert!(html.contains("pypi.org"));
        assert!(html.contains("831720"));
        // DNS log.
        assert!(html.contains("151.101.0.223"));
        // Behaviour summary with its exports.
        assert!(html.contains("Behavior summary"));
        assert!(html.contains("behavior?format=markdown"));
        assert!(html.contains("behavior?format=jsonl"));
    }

    #[test]
    fn activity_stream_fragment_colour_codes_by_domain() {
        let html = ActivityStreamFragment {
            stream: vec![StreamRow {
                ts: "22:14:07.412".into(),
                kind: "dns".into(),
                domain: "network",
                pid: 812,
                comm: "pip".into(),
                summary: "dns pypi.org".into(),
            }],
        }
        .render()
        .unwrap();

        assert!(html.contains("ev-network"));
        assert!(html.contains("22:14:07.412"));
        assert!(html.contains("dns pypi.org"));
        assert!(!html.contains("<html"), "a fragment carries no page chrome");
    }

    #[test]
    fn activity_stream_fragment_has_an_empty_state() {
        let html = ActivityStreamFragment { stream: vec![] }.render().unwrap();
        assert!(html.contains("No activity recorded yet"));
    }

    #[test]
    fn terminal_tab_loads_xterm_only_when_shown() {
        assert!(!detail("overview").render().unwrap().contains("xterm.js"));

        let terminal = detail("terminal").render().unwrap();
        assert!(terminal.contains("/assets/js/xterm.js"));
        assert!(terminal.contains("/assets/js/term.js"));
    }

    #[test]
    fn build_panel_subscribes_to_the_right_box() {
        let html = BuildPanelFragment {
            retained: String::new(),
            name: "alpha".into(),
            summary: "3 set(s), 0 extra package(s)".into(),
        }
        .render()
        .unwrap();
        assert!(html.contains("sse-swap=\"build-alpha\""));
        assert!(html.contains("sse-swap=\"build-status-alpha\""));
        assert!(html.contains("3 set(s)"));
    }

    #[test]
    fn file_changes_fragment_renders_both_states() {
        let empty = FileChangesFragment { changes: vec![] }.render().unwrap();
        assert!(empty.contains("No uncommitted changes"));

        let some = FileChangesFragment {
            changes: vec![
                FileChange {
                    status: "added".into(),
                    path: "src/new.rs".into(),
                    is_dir: false,
                },
                FileChange {
                    status: "deleted".into(),
                    path: "old.txt".into(),
                    is_dir: false,
                },
            ],
        }
        .render()
        .unwrap();
        assert!(some.contains("src/new.rs"));
        assert!(some.contains("file-added"));
        assert!(some.contains("file-deleted"));
    }

    #[test]
    fn help_template_renders_topic_navigation() {
        let html = HelpTemplate {
            version: "0.1.3",
            nav: "help",
            subtitle: "13 cheat sheets, offline".into(),
            current: "git".into(),
            topics: help::topics(),
            body: help::to_html("# Git\n\nfetch things"),
        }
        .render()
        .expect("template renders");

        assert!(html.contains("href=\"/help/git\""));
        assert!(
            html.contains("<h1>Git</h1>"),
            "markdown must reach the page"
        );
        assert!(html.contains("fetch things"));
    }
}
