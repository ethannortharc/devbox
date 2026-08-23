//! HTTP routes and page handlers.
//!
//! Handlers stay thin: they call [`super::service`] and render. Anything with
//! real logic belongs in the service layer so the CLI can reuse it.

use anyhow::Context as _;
use askama::Template;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::Deserialize;

use super::activity::{
    self, Activity, Bucket, CaptureView, Cursor, DomainChip, FileWrite, Filter, Flow, Lookup, Peer,
    Refusal, StreamRow, Totals, TreeRow,
};
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
        .route("/boxes/new", get(create_box_page))
        .route("/boxes/{name}", get(box_detail))
        .route("/labs", get(lab_index))
        .route("/labs/{name}", get(lab_detail))
        .route("/help", get(help_index))
        .route("/help/{topic}", get(help_topic))
        // json + fragments
        .route("/api/boxes", get(api_boxes).post(create_box))
        .route("/api/boxes/{name}", get(api_box))
        .route("/api/boxes/{name}/start", post(start_box))
        .route("/api/boxes/{name}/stop", post(stop_box))
        .route("/api/boxes/{name}/destroy", post(destroy_box))
        .route("/api/boxes/{name}/sets", post(apply_sets))
        .route("/api/boxes/{name}/policy", get(get_policy).put(put_policy))
        .route("/api/boxes/{name}/files", get(box_files))
        .route("/api/boxes/{name}/term", get(box_terminal))
        .route("/api/boxes/{name}/activity", get(box_activity))
        .route("/api/boxes/{name}/activity/tail", get(box_activity_tail))
        .route("/api/boxes/{name}/activity/live", get(box_activity_live))
        .route("/api/boxes/{name}/behavior", get(box_behavior))
        .route("/api/operations/{name}/status", get(operation_status))
        .route("/api/boxes/{name}/flows/pcap", post(capture_flow_pcap))
        .route("/api/stream", get(sse::stream))
        .route("/api/labs/{name}/view", get(lab_view))
        .route("/api/labs/{name}/ztp", get(lab_ztp))
        .route("/api/labs/{name}/up", post(lab_up))
        .route("/api/labs/{name}/down", post(lab_down))
        .route("/api/labs/{name}/fault", post(lab_fault))
        .route("/api/labs/{name}/heal", post(lab_heal))
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
    let attention = boxes
        .iter()
        .filter(|b| matches!(b.status.as_str(), "unreachable" | "missing" | "unknown"))
        .count();
    match boxes.len() {
        0 => "nothing registered yet".to_string(),
        1 if running == 1 => "1 box · running".to_string(),
        1 if attention == 1 => "1 box · needs attention".to_string(),
        1 => "1 box · not running".to_string(),
        n if attention == 1 => format!("{n} boxes · {running} running · 1 needs attention"),
        n if attention > 1 => {
            format!("{n} boxes · {running} running · {attention} need attention")
        }
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

// ── create box ───────────────────────────────────────────

#[derive(Template)]
#[template(path = "create_box.html")]
struct CreateBoxTemplate {
    version: &'static str,
    nav: &'static str,
    groups: Vec<SetGroup>,
}

#[derive(Template)]
#[template(path = "_create_panel.html")]
struct CreatePanelFragment {
    name: String,
    summary: String,
    retained: String,
}

async fn create_box_page(State(state): State<AppState>) -> Response {
    let defaults = crate::sandbox::config::DevboxConfig::default();
    let selection = Selection::new(defaults.active_sets(), std::iter::empty());
    render(CreateBoxTemplate {
        version: state.version,
        nav: "dashboard",
        groups: service::set_groups(&selection),
    })
}

/// `POST /api/boxes` — validate synchronously, then create in the background.
///
/// Provisioning can take minutes. The response installs an SSE log panel and
/// the task holds the same drain guard as a Sets rebuild, so Ctrl-C cannot
/// silently cancel it between making the runtime object and saving state.
async fn create_box(State(state): State<AppState>, body: String) -> Response {
    let spec = match service::parse_create_form(&state.manager, &body) {
        Ok(spec) => spec,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                error_notice(&format!("Invalid box configuration: {error}")),
            )
                .into_response();
        }
    };
    if state.is_rebuilding(&spec.name) {
        return render_status(
            CreatePanelFragment {
                retained: state.retained_build_status(&spec.name).unwrap_or_default(),
                summary: format!("{} (creation already in progress)", spec.name),
                name: spec.name,
            },
            StatusCode::CONFLICT,
        );
    }
    if state.manager.sandbox_exists(&spec.name) {
        return (
            StatusCode::CONFLICT,
            error_notice(&format!("box '{}' already exists", spec.name)),
        )
            .into_response();
    }
    let runtime = match state.manager.resolve_runtime(spec.runtime.as_deref()) {
        Ok(runtime) => runtime,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                error_notice(&format!("No usable runtime: {error}")),
            )
                .into_response();
        }
    };
    let Some(guard) = state.claim_rebuild(&spec.name) else {
        return (
            StatusCode::CONFLICT,
            error_notice("A create operation is already running for that box name."),
        )
            .into_response();
    };

    let name = spec.name.clone();
    let summary = format!(
        "{} on {} with {} set(s)",
        name,
        runtime.name(),
        spec.config.active_sets().len()
    );
    state.clear_build_status(&name);

    let bg = state.clone();
    let task_name = name.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let line = |message: &str| {
            bg.publish(crate::web::state::ConsoleEvent::new(
                build::output_event(&task_name),
                build::line_fragment(message),
            ));
        };
        line(&format!("project: {}", spec.project_dir.to_string_lossy()));
        line(&format!("runtime: {}", runtime.name()));
        line("creating runtime and provisioning selected tools…");

        let result = bg
            .manager
            .create_sandbox_at(
                &spec.project_dir,
                &task_name,
                runtime.as_ref(),
                &spec.config,
                &[],
                &Default::default(),
                None,
                spec.bare,
                Some(&line),
            )
            .await;

        let status = match result {
            Ok(()) => {
                let href = format!("/boxes/{}", crate::web::encode_segment(&task_name));
                let policy_result = match build::claim_box(&bg.manager.state_dir, &task_name) {
                    Ok(claim) => {
                        crate::policy::enforce::restore_after_rebuild(
                            &bg.manager,
                            &task_name,
                            &claim,
                        )
                        .await
                    }
                    Err(error) => Err(error.context(
                        "the box was created, but its egress policy could not be applied",
                    )),
                };

                match policy_result {
                    Ok(()) => format!(
                        "<span class=\"term-ok\">Box created.</span> \
                         <a class=\"btn btn-primary\" href=\"{}\">Open {}</a>",
                        href,
                        build::escape_html(&task_name)
                    ),
                    Err(error) => {
                        // A saved restrictive posture that failed to load must
                        // not leave a fresh box running unrestricted. Stopping
                        // is reversible, preserves the completed create, and
                        // gives the user a safe place to repair the policy.
                        let stop_error = runtime.stop(&task_name).await.err();
                        tracing::warn!(
                            box_id = %task_name,
                            error = ?error,
                            stop_error = ?stop_error,
                            "web create completed but policy enforcement failed"
                        );
                        let safety = match stop_error {
                            None => " The box was stopped for safety.".to_string(),
                            Some(stop_error) => format!(
                                " The box could not be stopped and may still be running unrestricted: {}",
                                build::escape_html(&stop_error.to_string())
                            ),
                        };
                        format!(
                            "<span class=\"term-err\">Box created, but its egress policy \
                             could not be applied: {}{}</span> \
                             <a class=\"btn btn-primary\" href=\"{}\">Open {}</a>",
                            build::escape_html(&error.to_string()),
                            safety,
                            href,
                            build::escape_html(&task_name)
                        )
                    }
                }
            }
            Err(error) => {
                tracing::warn!(box_id = %task_name, error = ?error, "web create failed");
                if bg.manager.sandbox_exists(&task_name) {
                    let href = format!("/boxes/{}", crate::web::encode_segment(&task_name));
                    format!(
                        "<span class=\"term-err\">The box exists, but setup did not complete: \
                         {}</span> <a class=\"btn\" href=\"{}\">Inspect {}</a>",
                        build::escape_html(&error.to_string()),
                        href,
                        build::escape_html(&task_name)
                    )
                } else {
                    format!(
                        "<span class=\"term-err\">Create failed: {}</span>",
                        build::escape_html(&error.to_string())
                    )
                }
            }
        };
        bg.publish(crate::web::state::ConsoleEvent::new(
            build::status_event(&task_name),
            status,
        ));
    });

    (
        StatusCode::ACCEPTED,
        render(CreatePanelFragment {
            retained: state.retained_build_status(&name).unwrap_or_default(),
            name,
            summary,
        }),
    )
        .into_response()
}

// ── labs ─────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "labs.html")]
struct LabsTemplate {
    version: &'static str,
    nav: &'static str,
    scenarios: Vec<super::labs::ScenarioCard>,
}

#[derive(Template)]
#[template(path = "lab_detail.html")]
struct LabTemplate {
    version: &'static str,
    nav: &'static str,
    view: super::labs::LabView,
    scenarios: Vec<super::labs::ScenarioCard>,
    boxes: Vec<BoxSummary>,
    retained: String,
}

#[derive(Template)]
#[template(path = "_lab_view.html")]
struct LabViewFragment {
    view: super::labs::LabView,
}

async fn lab_index(State(state): State<AppState>) -> Response {
    render(LabsTemplate {
        version: state.version,
        nav: "labs",
        scenarios: super::labs::scenarios(),
    })
}

async fn lab_detail(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let view = match super::labs::load(&state.manager, &name) {
        Ok(view) => view,
        Err(error) => return not_found(&name, &error),
    };
    let boxes = service::list_boxes(&state.manager)
        .await
        .unwrap_or_default();
    render(LabTemplate {
        version: state.version,
        nav: "labs",
        view,
        scenarios: super::labs::scenarios(),
        boxes,
        retained: state
            .retained_build_status(&format!("lab-{name}"))
            .unwrap_or_default(),
    })
}

async fn lab_view(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let mut view = match super::labs::load(&state.manager, &name) {
        Ok(view) => view,
        Err(error) => return not_found(&name, &error),
    };

    // Best effort. A topology that refused to draw because a provisioning
    // service was unreachable would be useless exactly when someone is trying
    // to work out why it is unreachable.
    let substrate = substrate_from(query.as_deref());
    if let Some(fabric) = ztp_shared(&state, &name, &substrate).await {
        super::labs::apply_phases(&mut view, &fabric);
    }
    render(LabViewFragment { view })
}

/// One ZTP read, shared between the elements on a lab page that want it.
///
/// The topology and the provisioning panel refresh on their own triggers and
/// both need the same answer; without this, each open page made two round
/// trips into the substrate every two seconds for one status.
async fn ztp_shared(
    state: &AppState,
    name: &str,
    substrate: &str,
) -> Option<crate::lab::ztp_status::FabricView> {
    let key = format!("{name}\u{1}{substrate}");
    if let Some(cached) = state.cached_ztp(&key) {
        return Some(cached);
    }
    match super::labs::ztp(&state.manager, name, Some(substrate)).await {
        Ok(Some(view)) => {
            state.cache_ztp(&key, &view);
            Some(view)
        }
        // Not a ZTP scenario. There is no panel for this lab, and there never
        // will be — distinct from the case below, which is a panel with
        // nothing to report yet.
        Ok(None) => None,
        Err(error) => {
            // A substrate that cannot be reached is a state the panel shows
            // and keeps polling from. Folding this into the case above made
            // the whole panel vanish rather than say the lab is not running.
            // Deliberately not cached: the next tick should try again.
            tracing::debug!(lab = %name, %error, "could not read ZTP status");
            Some(crate::lab::ztp_status::FabricView::default())
        }
    }
}

/// Read the substrate a lab request names, or empty when it names none.
fn substrate_from(query: Option<&str>) -> String {
    query
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "substrate")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "_ztp_panel.html")]
struct ZtpPanelFragment {
    ztp_lab: String,
    ztp: crate::lab::ztp_status::FabricView,
}

/// `GET /api/labs/{name}/ztp` — one fabric's provisioning state.
///
/// Polled rather than driven by the collector's signal: this reads a service
/// inside the substrate, not a local store, and the thing being watched is a
/// state machine that moves on its own. Two seconds is fast enough to watch a
/// node go from discovered to healthy and slow enough to cost nothing.
async fn lab_ztp(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let substrate = substrate_from(query.as_deref());
    match ztp_shared(&state, &name, &substrate).await {
        Some(ztp) => render(ZtpPanelFragment { ztp_lab: name, ztp }),
        // Not a ZTP scenario, or one whose substrate could not answer. The
        // first has nothing to render at all; the second is a state the panel
        // shows and keeps polling from, not a page failure.
        None => Html(String::new()).into_response(),
    }
}

#[derive(Clone, Copy)]
enum LabAction {
    Up,
    Down,
    Fault,
    Heal,
}

async fn lab_up(State(state): State<AppState>, Path(name): Path<String>, body: String) -> Response {
    start_lab_action(state, name, body, LabAction::Up).await
}

async fn lab_down(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    start_lab_action(state, name, body, LabAction::Down).await
}

async fn lab_fault(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    start_lab_action(state, name, body, LabAction::Fault).await
}

async fn lab_heal(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    start_lab_action(state, name, body, LabAction::Heal).await
}

async fn start_lab_action(
    state: AppState,
    name: String,
    body: String,
    action: LabAction,
) -> Response {
    if crate::lab::scenarios::find(&name).is_none() {
        return (
            StatusCode::NOT_FOUND,
            error_notice(&format!("no such built-in scenario: {name}")),
        )
            .into_response();
    }
    let fields: std::collections::HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let field = |key: &str| fields.get(key).map(String::as_str).unwrap_or("").trim();
    let substrate = (!field("substrate").is_empty()).then(|| field("substrate").to_string());
    let link = field("link").to_string();
    if matches!(action, LabAction::Fault | LabAction::Heal) && link.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            error_notice("Choose a link before applying or healing a fault."),
        )
            .into_response();
    }
    let number = |key: &str| -> anyhow::Result<Option<u32>> {
        let value = field(key);
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.parse().with_context(|| {
                format!("{key} must be a non-negative integer")
            })?))
        }
    };
    let percentage = |key: &str| -> anyhow::Result<Option<f64>> {
        let value = field(key);
        if value.is_empty() {
            Ok(None)
        } else {
            let parsed: f64 = value
                .parse()
                .with_context(|| format!("{key} must be a percentage"))?;
            if !(0.0..=100.0).contains(&parsed) {
                anyhow::bail!("{key} must be between 0 and 100");
            }
            Ok(Some(parsed))
        }
    };
    let fault = if matches!(action, LabAction::Fault) {
        let parsed = (|| -> anyhow::Result<crate::cli::lab::FaultArgs> {
            Ok(crate::cli::lab::FaultArgs {
                target: name.clone(),
                link: link.clone(),
                delay: number("delay")?,
                jitter: number("jitter")?,
                loss: percentage("loss")?,
                reorder: percentage("reorder")?,
                duplicate: percentage("duplicate")?,
                rate: number("rate")?,
                partition: fields.contains_key("partition"),
                direction: match field("direction") {
                    "" => "both".to_string(),
                    value @ ("a" | "b" | "both") => value.to_string(),
                    value => anyhow::bail!("direction must be a, b, or both, got '{value}'"),
                },
                substrate: substrate.clone(),
                dry_run: false,
            })
        })();
        match parsed {
            Ok(fault) => Some(fault),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    error_notice(&format!("Invalid fault: {error}")),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let operation = format!("lab-{name}");
    let Some(guard) = state.claim_rebuild(&operation) else {
        let retained = state.retained_build_status(&operation).unwrap_or_default();
        let status_href = format!(
            "/api/operations/{}/status",
            crate::web::encode_segment(&operation)
        );
        return (
            StatusCode::CONFLICT,
            Html(format!(
                "<div class=\"notice error\" role=\"alert\">Another operation is already changing this lab.</div>\
                 <p class=\"muted\" data-build-status=\"{}\" hx-get=\"{}\" hx-trigger=\"load\" sse-swap=\"{}\">{}</p>",
                build::escape_html(&operation),
                status_href,
                build::status_event(&operation),
                retained
            )),
        )
            .into_response();
    };
    state.clear_build_status(&operation);
    let bg = state.clone();
    let task_name = name.clone();
    let operation_name = operation.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let result = match action {
            LabAction::Up => {
                crate::cli::lab::up(
                    crate::cli::lab::UpArgs {
                        target: task_name.clone(),
                        substrate,
                        dry_run: false,
                    },
                    &bg.manager,
                )
                .await
            }
            LabAction::Down => {
                crate::cli::lab::down(
                    crate::cli::lab::DownArgs {
                        target: task_name.clone(),
                        substrate,
                        dry_run: false,
                    },
                    &bg.manager,
                )
                .await
            }
            LabAction::Fault => crate::cli::lab::inject(fault.expect("parsed"), &bg.manager).await,
            LabAction::Heal => {
                crate::cli::lab::heal(
                    crate::cli::lab::HealArgs {
                        target: task_name.clone(),
                        link,
                        substrate,
                        dry_run: false,
                    },
                    &bg.manager,
                )
                .await
            }
        };
        let message = match result {
            Ok(()) => format!(
                "<span class=\"term-ok\">{} completed.</span>",
                match action {
                    LabAction::Up => "Lab bring-up",
                    LabAction::Down => "Lab teardown",
                    LabAction::Fault => "Fault injection",
                    LabAction::Heal => "Link heal",
                }
            ),
            Err(error) => format!(
                "<span class=\"term-err\">{}</span>",
                build::escape_html(&error.to_string())
            ),
        };
        bg.publish(crate::web::state::ConsoleEvent::new(
            build::status_event(&operation_name),
            message,
        ));
    });

    (
        StatusCode::ACCEPTED,
        Html(format!(
            "<p class=\"muted\" data-build-status=\"{}\" hx-get=\"/api/operations/{}/status\" hx-trigger=\"load\" sse-swap=\"{}\">Working…</p>",
            build::escape_html(&operation),
            crate::web::encode_segment(&operation),
            build::status_event(&operation)
        )),
    )
        .into_response()
}

// ── box detail ───────────────────────────────────────────

#[derive(Template)]
#[template(path = "box_detail.html")]
struct BoxDetailTemplate {
    version: &'static str,
    nav: &'static str,
    tab: &'static str,
    /// SSE event name for this card, encoded exactly as the watcher emits it.
    box_event: String,
    boxinfo: BoxSummary,
    groups: Vec<SetGroup>,
    extra_packages: String,
    policy: PolicyView,
    /// Repeated for the Activity partial, which is included rather than
    /// rendered on its own and so cannot reach `boxinfo`.
    boxname: String,
    capture: CaptureView,
    capture_event: String,
    activity_event: String,
    timeline: Vec<Bucket>,
    totals: Totals,
    stream: Vec<StreamRow>,
    peers: Vec<Peer>,
    flows: Vec<Flow>,
    lookups: Vec<Lookup>,
    tree: Vec<TreeRow>,
    files: Vec<FileWrite>,
    violations: Vec<Refusal>,
    egress_mode: Option<String>,
    domains: Vec<DomainChip>,
    filter_query: String,
    cursor: String,
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
    RawQuery(raw_query): RawQuery,
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
    // The project config is the fallback for a box created before `state.json`
    // carried packages: serde fills those fields with empty collections, so
    // this page would render an empty extra-packages field and post it back,
    // uninstalling everything the box had.
    let selection = match state.manager.get_sandbox(&name) {
        Ok(s) => {
            let project = crate::sandbox::config::DevboxConfig::load_or_default(&s.project_dir);
            Selection::from_state_and_project(&s, &project)
        }
        Err(_) => Selection::default(),
    };

    // Only the Activity tab pays for reading the event store.
    let filter = Filter::from_query(raw_query.as_deref().unwrap_or_default());
    let (act, capture) = if tab == "activity" {
        let act = activity::load(&state.manager, &name, activity::INITIAL_EVENTS, &filter)
            .unwrap_or_else(|e| {
                tracing::warn!(box_id = %name, error = %e, "could not load activity");
                Activity::default()
            });
        (act, capture_bar(&state, &name, &boxinfo.status))
    } else {
        // The bar is part of the Activity tab, so the probes it needs are too.
        (Activity::default(), CaptureView::default())
    };

    render(BoxDetailTemplate {
        version: state.version,
        nav: "dashboard",
        tab,
        box_event: super::watch::box_card_event(&name),
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
        behavior: crate::obs::behavior::render_markdown(&act.summary),
        boxname: boxinfo.name.clone(),
        capture,
        capture_event: super::tail::capture_event(&name),
        activity_event: super::tail::activity_event(&name),
        domains: activity::domain_chips(&act.events, &filter),
        filter_query: filter.query.clone(),
        cursor: act.cursor,
        timeline: act.timeline,
        totals: act.totals,
        stream: act.stream,
        peers: act.peers,
        flows: act.flows,
        lookups: act.lookups,
        tree: act.tree,
        files: act.files,
        violations: act.violations,
        egress_mode: act.summary.egress_mode.clone(),
        has_store: act.has_store,
        boxinfo,
        retained_build: state.retained_build_status(&name).unwrap_or_default(),
    })
}

/// Whether a box is registered, for the routes that never look one up.
///
/// The activity fragments and the behaviour export read a store by name, and
/// the observability files deliberately live outside `sandboxes/<name>` so a
/// compromised guest cannot reach its own audit record. That separation means
/// a crash between removing a sandbox and removing its store leaves an orphan
/// — and without this check `/api/boxes/ghost/behavior` hands over that box's
/// entire retained history while `/boxes/ghost` says it does not exist.
///
/// `is_safe_name` bounds *where* a name can point; this bounds *what* it may
/// name.
fn registered(state: &AppState, name: &str) -> bool {
    state.manager.get_sandbox(name).is_ok()
}

/// The first line of a truncated JSONL export.
///
/// A record rather than a comment, because JSON Lines has no comments and a
/// consumer that splits on newlines and parses each line must not choke on it.
const TRUNCATION_RECORD: &str = r#"{"devbox":"truncated","note":"the scan stopped before the end of this window; narrow it with `since`"}"#;

/// Largest export one request will read out of the store.
///
/// Not the request's peak memory: every format summarizes the decoded events
/// and then builds a body from them, so a request costs some multiple of this.
/// It is set low enough that the multiple is still bounded, and high enough
/// for a real audit trail — tens of thousands of ordinary events.
const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;

/// A 404 for a fragment route, which has no page to render.
fn fragment_not_found(name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<p class=\"muted\">No box named {}.</p>",
            build::escape_html(name)
        )),
    )
        .into_response()
}

/// Resolve the capture status bar for one box.
///
/// Two cheap reads — an advisory-lock probe and a small JSON file —
/// deliberately kept out of the other tabs, which have no bar to show and
/// should not pay for one.
fn capture_bar(state: &AppState, name: &str, box_status: &str) -> CaptureView {
    let daemon_running = crate::obs::daemon::status(&state.manager)
        .map(|owner| owner.is_some())
        .unwrap_or(false);
    let health = crate::obs::health::load(&state.manager.state_dir, name).unwrap_or_default();
    activity::capture_view(daemon_running, box_status, health.as_ref())
}

// ── activity ─────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_activity_stream.html")]
struct ActivityStreamFragment {
    stream: Vec<StreamRow>,
    cursor: String,
}

#[derive(Template)]
#[template(path = "_activity_tail.html")]
struct ActivityTailFragment {
    stream: Vec<StreamRow>,
    cursor: String,
    /// Events jumped over because a burst outran the page.
    skipped: u64,
}

/// `GET /api/boxes/{name}/activity` — the whole stream, re-read.
///
/// What a filter change asks for: the selection changed, so the window has to
/// be re-derived rather than appended to.
async fn box_activity(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if !registered(&state, &name) {
        return fragment_not_found(&name);
    }
    let filter = Filter::from_query(query.as_deref().unwrap_or_default());
    match activity::load(&state.manager, &name, activity::INITIAL_EVENTS, &filter) {
        Ok(act) => render(ActivityStreamFragment {
            stream: act.stream,
            cursor: act.cursor,
        }),
        Err(e) => server_error("failed to read the event store", &e),
    }
}

#[derive(Template)]
#[template(path = "_activity_live_response.html")]
struct ActivityLiveFragment {
    domains: Vec<DomainChip>,
    boxname: String,
    timeline: Vec<Bucket>,
    totals: Totals,
    peers: Vec<Peer>,
    flows: Vec<Flow>,
    lookups: Vec<Lookup>,
    tree: Vec<TreeRow>,
    files: Vec<FileWrite>,
    violations: Vec<Refusal>,
    egress_mode: Option<String>,
}

/// `GET /api/boxes/{name}/activity/live` — everything that describes the
/// loaded window: the density strip, the switcher's counts, and the six
/// analysis views.
///
/// One region and one read, because they have to agree. Kept out of the tail
/// so the stream is appended to while this is replaced — one swap cannot do
/// both, and replacing the stream is the behaviour this whole change exists
/// to remove. Throttled by the page, so an active box re-renders it every few
/// seconds rather than on every burst.
async fn box_activity_live(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !registered(&state, &name) {
        return fragment_not_found(&name);
    }
    match activity::load(
        &state.manager,
        &name,
        activity::INITIAL_EVENTS,
        &Filter::default(),
    ) {
        Ok(act) => render(ActivityLiveFragment {
            // Counts only: the fragment sends no checkbox state, so the
            // filter this request was made under does not matter.
            domains: activity::domain_chips(&act.events, &Filter::default()),
            boxname: name,
            timeline: act.timeline,
            totals: act.totals,
            peers: act.peers,
            flows: act.flows,
            lookups: act.lookups,
            tree: act.tree,
            files: act.files,
            violations: act.violations,
            egress_mode: act.summary.egress_mode.clone(),
        }),
        Err(e) => server_error("failed to read the event store", &e),
    }
}

/// `GET /api/boxes/{name}/activity/tail?after=N` — only what is new.
///
/// Answers the collector's signal. The anchor comes from the page and goes
/// back out-of-band, so two consoles opened minutes apart each resume from
/// their own position instead of sharing one cursor.
async fn box_activity_tail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if !registered(&state, &name) {
        return fragment_not_found(&name);
    }
    let query = query.unwrap_or_default();
    let filter = Filter::from_query(&query);
    let cursor = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "after")
        .map(|(_, value)| Cursor::parse(&value))
        .unwrap_or_default();

    match activity::tail(&state.manager, &name, cursor, &filter) {
        Ok(page) => {
            // Newest first: the block is prepended whole, so its own order is
            // what decides which row ends up on top.
            let mut stream = page.rows;
            stream.reverse();
            let reset = page.reset;
            let mut response = render(ActivityTailFragment {
                stream,
                cursor: page.cursor,
                skipped: page.skipped,
            });
            if reset {
                // The page is holding rows from a store that no longer exists.
                // Appending to them would interleave two different boxes'
                // events in one timeline with nothing to mark the seam, so
                // this response replaces the stream instead of extending it.
                response.headers_mut().insert(
                    HeaderName::from_static("hx-reswap"),
                    HeaderValue::from_static("innerHTML"),
                );
            }
            response
        }
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
    if !registered(&state, &name) {
        return fragment_not_found(&name);
    }
    let store = match activity::open_store(&state.manager, &name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_notice(&format!("no events recorded for box '{name}' yet")),
            )
                .into_response();
        }
        Err(e) => return server_error("failed to open the event store", &e),
    };

    // Bounded by rows and by bytes. A row limit alone does not bound memory:
    // the transport accepts frames up to a megabyte, so fifty thousand rows is
    // fifty gigabytes in the worst case, and a guest that emits maximal events
    // could end the console by asking it for an audit trail.
    let (events, truncated) = match store.export(
        q.since.as_deref(),
        crate::obs::store::Query::MAX_LIMIT,
        MAX_EXPORT_BYTES,
    ) {
        Ok(pair) => pair,
        Err(e) => return server_error("failed to query events", &e),
    };

    // `truncated` means the window was cut off — by the row limit, by the byte
    // budget, or by a row that no longer decodes. This endpoint serves both a
    // rendered summary and a JSONL export, and returning either as though it
    // covered the whole window lets a consumer mistake a partial audit for a
    // complete one: the same failure the CLI refuses outright, except an
    // export has no reader to warn.

    let summary = crate::obs::behavior::summarize(&name, &events);

    // On every representation, because each of the three is something a
    // consumer might archive as "what this box did".
    let mut response = match q.format.as_deref() {
        Some("markdown") => {
            let mut body = crate::obs::behavior::render_markdown(&summary);
            if truncated {
                body.insert_str(
                    0,
                    // Deliberately names no count. The scan stops at a row
                    // limit, at a byte budget, or at a row that no longer
                    // decodes, and a warning that always blamed the first was
                    // wrong for a window of a few hundred large events.
                    "> **Incomplete.** The scan stopped before the end of this \
                     window. What follows covers the oldest events it reached \
                     — narrow the window with `since`.\n\n",
                );
            }
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/markdown; charset=utf-8",
                )],
                body,
            )
                .into_response()
        }
        Some("jsonl") => match crate::obs::behavior::render_jsonl(&events) {
            Ok(mut body) => {
                // In the body, not only in a header. These exports are saved
                // by a keyed fetch that writes the bytes to a file — the
                // response headers do not survive it, so a partial window
                // reached the disk looking complete.
                if truncated {
                    body.insert_str(0, &format!("{}\n", TRUNCATION_RECORD));
                }
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/x-ndjson; charset=utf-8",
                    )],
                    body,
                )
                    .into_response()
            }
            Err(e) => server_error("failed to export events", &anyhow::Error::from(e)),
        },
        _ => {
            // Same reason as the JSONL case: the saved file has to carry it.
            let mut value = serde_json::to_value(&summary).unwrap_or_default();
            if truncated && let Some(object) = value.as_object_mut() {
                object.insert("truncated".into(), serde_json::Value::Bool(true));
            }
            Json(value).into_response()
        }
    };

    // A header as well as the prose: the export has no reader to warn, and a
    // script archiving it as a complete audit is the case that matters.
    if truncated {
        response.headers_mut().insert(
            HeaderName::from_static("x-devbox-truncated"),
            HeaderValue::from_static("true"),
        );
    }
    response
}

#[derive(Debug, Deserialize)]
struct PcapQuery {
    proto: String,
    saddr: Option<std::net::IpAddr>,
    sport: Option<u16>,
    daddr: std::net::IpAddr,
    dport: u16,
    duration: Option<u64>,
    packets: Option<u16>,
}

/// `POST /api/boxes/{name}/flows/pcap` — a bounded real packet capture.
async fn capture_flow_pcap(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<PcapQuery>,
) -> Response {
    let filter = crate::obs::pcap::FlowFilter {
        proto: query.proto,
        saddr: query.saddr,
        sport: query.sport.filter(|port| *port != 0),
        daddr: query.daddr,
        dport: query.dport,
        duration: std::time::Duration::from_secs(
            query
                .duration
                .unwrap_or(crate::obs::pcap::DEFAULT_DURATION.as_secs()),
        ),
        packets: query.packets.unwrap_or(crate::obs::pcap::DEFAULT_PACKETS),
    };
    let capture = match crate::obs::pcap::capture(&state.manager, &name, &filter).await {
        Ok(capture) => capture,
        Err(error) => return action_error("capture a flow from", &name, &error),
    };
    let filename = format!(
        "attachment; filename=\"devbox-{}-flow.pcap\"",
        crate::web::encode_segment(&name)
    );
    let mut response = (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.tcpdump.pcap",
            ),
            (axum::http::header::CONTENT_DISPOSITION, &filename),
        ],
        capture.bytes,
    )
        .into_response();
    response.headers_mut().insert(
        HeaderName::from_static("x-devbox-packets"),
        HeaderValue::from_str(&capture.packets.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    response
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
        collector: crate::obs::daemon::stats_snapshot(&state.manager).unwrap_or_default(),
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
    notice: String,
}

#[derive(Debug, Default, Deserialize)]
struct StartQuery {
    #[serde(default)]
    terminal: bool,
}

#[derive(Debug, Default, Deserialize)]
struct DestroyQuery {
    #[serde(default)]
    force: bool,
}

/// Re-render a box's card, which is what every lifecycle action swaps in.
async fn card_after_action(state: &AppState, name: &str) -> Response {
    match service::get_box(&state.manager, name).await {
        Ok(b) => render(BoxCardFragment {
            b,
            notice: String::new(),
        }),
        Err(e) => not_found(name, &e),
    }
}

/// Keep the lifecycle controls in place when an action fails. The htmx target
/// is the whole card, so returning only an error notice would erase every
/// button and leave the dashboard unusable until a full reload.
async fn card_action_error(
    state: &AppState,
    verb: &str,
    name: &str,
    err: &anyhow::Error,
) -> Response {
    tracing::warn!(box_id = %name, error = ?err, "failed to {verb} box");
    match service::get_box(&state.manager, name).await {
        Ok(b) => render_status(
            BoxCardFragment {
                b,
                // Alternate formatting includes the full anyhow chain. The
                // top-level context is often just "failed to start box"; the
                // actionable readiness timeout or recovery command is in its
                // source and must reach the person clicking the button.
                notice: format!("Could not {verb} {name}: {err:#}"),
            },
            StatusCode::CONFLICT,
        ),
        Err(_) => action_error(verb, name, err),
    }
}

async fn start_box(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<StartQuery>,
) -> Response {
    if let Err(e) = service::start_box(&state.manager, &name).await {
        if query.terminal {
            return card_action_error(&state, "start", &name, &e).await;
        }
        return card_action_error(&state, "start", &name, &e).await;
    }
    if query.terminal {
        // The terminal uses fetch rather than htmx. Return the same fresh card
        // as an explicit Start so the detail header and controls immediately
        // reflect a successful lazy-start instead of remaining "stopped".
        return card_after_action(&state, &name).await;
    }
    card_after_action(&state, &name).await
}

async fn stop_box(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(e) = service::stop_box(&state.manager, &name).await {
        return card_action_error(&state, "stop", &name, &e).await;
    }
    card_after_action(&state, &name).await
}

async fn destroy_box(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<DestroyQuery>,
) -> Response {
    // The claim is taken inside `destroy_sandbox`, so the CLI path is covered
    // by the same rule rather than by a second copy of it here.
    if let Err(e) = service::destroy_box(&state.manager, &name, query.force).await {
        return card_action_error(&state, "destroy", &name, &e).await;
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
        return (
            StatusCode::NOT_FOUND,
            error_notice(&format!("no such box: {name}")),
        )
            .into_response();
    }

    let selection = service::parse_selection_form(&body);
    if let Err(e) = selection.validate() {
        return (
            StatusCode::BAD_REQUEST,
            error_notice(&format!("Invalid selection: {e}")),
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
        return render_status(
            BuildPanelFragment {
                retained: state.retained_build_status(&name).unwrap_or_default(),
                name,
                summary: format!("{summary} (rebuild already in progress)"),
            },
            StatusCode::CONFLICT,
        );
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
            // `apply_selection` owns the terminal status: it knows whether the
            // failure also left the firewall down, and publishes a message
            // saying so. Publishing the same event key here overwrote that —
            // `publish` retains only the last value and htmx replaces the same
            // target — so the "this box is unrestricted" warning was replaced
            // by the plainer rebuild error. Only publish if nothing did.
            if bg.retained_build_status(&box_name).is_none() {
                bg.publish(crate::web::state::ConsoleEvent::new(
                    build::status_event(&box_name),
                    format!(
                        "<span class=\"term-err\">{}</span>",
                        build::escape_html(&e.to_string())
                    ),
                ));
            }
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
                error_notice(&format!("Invalid policy: {e}")),
            )
                .into_response();
        }
    };

    let posture = updated.egress;
    let entries = updated.allow.len();
    let policy = updated.clone();

    // One claim over the write *and* the apply.
    //
    // Releasing it after the save let two overlapping edits reach the firewall
    // in the opposite order from the file: `devbox.toml` could end at
    // `isolated` while a delayed earlier `open` cleared the live table, so the
    // box was running the posture nobody had asked for last.
    // Claim, then read. Reading the project first meant a `devbox use`
    // finishing in the gap left this request locking the *old* project while
    // `save_policy` re-resolved and wrote the new one — from a form derived
    // from the old policy, then applied live. An old `open` posture replaced
    // the newly selected project's restrictive one.
    let (_box_claim, sandbox) = match state.manager.claim_and_read(&name) {
        Ok(pair) => pair,
        Err(e) => return action_error("change the policy of", &name, &e),
    };
    let project_dir = sandbox.project_dir;

    let lock_dir = state.manager.state_dir.clone();
    let lock_project = project_dir.clone();
    let _edit = match build::claim_project_off_worker(move || {
        build::claim_project(&lock_dir, &lock_project)
    })
    .await
    {
        Ok(lock) => lock,
        Err(e) => return server_error("failed to claim devbox.toml", &e),
    };

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
    // Pure attach: the terminal page's authenticated Start POST is the sole
    // lifecycle transition. Auto-starting again here could undo an explicit
    // Stop that landed between the POST and WebSocket handshake.
    if let Err(e) = service::require_running(&state.manager, &name).await {
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
            error_notice(&format!("no cheat sheet for '{topic}'")),
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

/// Replay the latest terminal status after an htmx panel is installed.
///
/// SSE has no history. A fast background failure can publish between the HTTP
/// response being rendered and the browser attaching `sse-swap`; this load
/// hook closes that narrow gap without polling indefinitely.
async fn operation_status(State(state): State<AppState>, Path(name): Path<String>) -> Html<String> {
    Html(state.retained_build_status(&name).unwrap_or_default())
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

fn render_status<T: Template>(template: T, status: StatusCode) -> Response {
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
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
        error_notice(&format!("{context}: {err}")),
    )
        .into_response()
}

/// The manager reports both "no such box" and "unreadable state" as an error;
/// treating an unknown name as 404 is the useful distinction for a client, and
/// the detail is logged either way.
fn not_found(name: &str, err: &anyhow::Error) -> Response {
    tracing::debug!(box_id = %name, error = ?err, "box lookup failed");
    // htmx swaps 4xx, and this one echoes a path segment, so it goes through
    // the same escape as every other notice. A body being `text/plain` is no
    // defence: htmx inserts the response text as HTML whatever the header says.
    (
        StatusCode::NOT_FOUND,
        error_notice(&format!("no such box: {name}")),
    )
        .into_response()
}

/// An error notice for the console, with every dynamic part escaped.
///
/// Round 22 configured htmx to swap 4xx bodies, because the console's
/// actionable errors are 4xx fragments and none of them were reaching the
/// user. What that change did not ask is what those bodies *contain*: these
/// messages quote things the project controls — a set or package name straight
/// off the submitted form, an allowlist entry, a path out of `devbox.toml` —
/// and `Selection::validate` interpolates them into its message verbatim. So
/// turning on the swap turned every one of those quotes into script execution
/// in a console holding an authenticated session.
///
/// The escape belongs here rather than at each call site: the reason a handler
/// is safe must not be that whoever wrote it remembered.
fn error_notice(message: &str) -> Html<String> {
    Html(format!(
        "<div class=\"notice error\" role=\"alert\">{}</div>",
        build::escape_html(message)
    ))
}

/// A failed lifecycle action. The message is shown to the user, so it carries
/// the underlying cause — these are things like "uncommitted overlay changes"
/// that the person can actually act on.
fn action_error(verb: &str, name: &str, err: &anyhow::Error) -> Response {
    tracing::warn!(box_id = %name, error = ?err, "failed to {verb} box");
    // `verb` is a literal at every call site; the box name and the error are
    // not, so both are escaped.
    (
        StatusCode::CONFLICT,
        Html(format!(
            "<div class=\"notice error\" role=\"alert\">Could not {verb} <strong>{}</strong>: {}</div>",
            build::escape_html(name),
            build::escape_html(&format!("{err:#}"))
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
            dashboard_subtitle(&[summary("a", "unreachable")]),
            "1 box · needs attention"
        );
        assert_eq!(
            dashboard_subtitle(&[summary("a", "running"), summary("b", "stopped")]),
            "2 boxes · 1 running"
        );
        assert_eq!(
            dashboard_subtitle(&[summary("a", "running"), summary("b", "unknown")]),
            "2 boxes · 1 running · 1 needs attention"
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
        assert!(html.contains("data-box-status=\"running\""));
        assert!(html.contains("data-box-status=\"stopped\""));
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
    fn lifecycle_buttons_follow_operable_status() {
        let running = BoxCardFragment {
            b: summary("a", "running"),
            notice: String::new(),
        }
        .render()
        .unwrap();
        // Start disabled, Stop enabled.
        assert!(running.contains("/api/boxes/a/start"));
        let start_idx = running.find("/api/boxes/a/start").unwrap();
        let stop_idx = running.find("/api/boxes/a/stop").unwrap();
        assert!(running[start_idx..stop_idx].contains("disabled>Start"));
        assert!(
            !running[stop_idx..]
                .split("</button>")
                .next()
                .unwrap()
                .contains("disabled>Stop")
        );

        let stopped = BoxCardFragment {
            b: summary("a", "stopped"),
            notice: String::new(),
        }
        .render()
        .unwrap();
        let start_idx = stopped.find("/api/boxes/a/start").unwrap();
        let stop_idx = stopped.find("/api/boxes/a/stop").unwrap();
        assert!(!stopped[start_idx..stop_idx].contains("disabled>Start"));
        assert!(stopped[stop_idx..].contains("disabled>Stop"));

        let unreachable = BoxCardFragment {
            b: summary("a", "unreachable"),
            notice: String::new(),
        }
        .render()
        .unwrap();
        let start_idx = unreachable.find("/api/boxes/a/start").unwrap();
        let stop_idx = unreachable.find("/api/boxes/a/stop").unwrap();
        assert!(unreachable[start_idx..stop_idx].contains("disabled>Start"));
        assert!(
            !unreachable[stop_idx..]
                .split("</button>")
                .next()
                .unwrap()
                .contains("disabled>Stop")
        );

        // Unknown means the status probe could not prove the box healthy.
        // Starting would be unsafe, but Stop is the explicit recovery path.
        let unknown = BoxCardFragment {
            b: summary("a", "unknown"),
            notice: String::new(),
        }
        .render()
        .unwrap();
        let start_idx = unknown.find("/api/boxes/a/start").unwrap();
        let stop_idx = unknown.find("/api/boxes/a/stop").unwrap();
        assert!(unknown[start_idx..stop_idx].contains("disabled>Start"));
        assert!(
            !unknown[stop_idx..]
                .split("</button>")
                .next()
                .unwrap()
                .contains("disabled>Stop")
        );
    }

    fn detail(tab: &'static str) -> BoxDetailTemplate {
        let selection = Selection::new(["system".to_string()], std::iter::empty());
        BoxDetailTemplate {
            retained_build: String::new(),
            version: "0.1.3",
            nav: "dashboard",
            tab,
            box_event: "box-card-alpha".into(),
            groups: service::set_groups(&selection),
            extra_packages: String::new(),
            policy: service::policy_view(&crate::policy::Policy::default()),
            boxinfo: summary("alpha", "running"),
            boxname: "alpha".into(),
            capture: activity::capture_view(true, "running", None),
            capture_event: crate::web::tail::capture_event("alpha"),
            activity_event: crate::web::tail::activity_event("alpha"),
            timeline: vec![],
            totals: Totals::default(),
            stream: vec![],
            peers: vec![],
            flows: vec![],
            lookups: vec![],
            tree: vec![],
            files: vec![],
            violations: vec![],
            egress_mode: None,
            domains: vec![],
            filter_query: String::new(),
            cursor: "0:0".into(),
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
            saddr: "10.0.0.2".into(),
            capture_saddr: Some("10.0.0.2".into()),
            sport: 42000,
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
    fn behavior_exports_are_keyed_fetches_and_not_navigations() {
        // These were plain anchors to `/api/`. A navigation cannot present the
        // console key and `/api/` is not eligible for the shell, so from the
        // moment the credential stopped being a cookie every export returned
        // 401 instead of a file.
        //
        // The repair must not be to put the key in the href. `?k=` is accepted
        // under `/api/` for the channels that cannot set a header, and a
        // clicked link is not one of them — that URL would reach history, the
        // downloads list, and "Copy link address".
        let html = detail_with_activity().render().unwrap();

        let exports: Vec<&str> = html
            .split("<a ")
            .filter(|fragment| fragment.contains("/behavior"))
            .map(|fragment| fragment.split('>').next().unwrap_or_default())
            .collect();
        assert_eq!(exports.len(), 3, "expected three export links: {html}");

        for tag in exports {
            assert!(
                tag.contains("data-download="),
                "must be fetched with the key, not navigated to: {tag}"
            );
            assert!(
                !tag.contains("k="),
                "the key must never sit in a link the user can copy: {tag}"
            );
        }
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
            assert!(html.contains("id=\"detail-live-status\""));
        }

        let overview = detail("overview").render().unwrap();
        assert!(overview.contains("id=\"overview-live-status\""));
        assert!(overview.contains("sse-swap=\"box-card-alpha\""));
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
    fn an_empty_activity_tab_says_which_of_the_four_reasons_it_is_empty() {
        // The whole point of the capture bar. "Nothing here" used to be the
        // page's entire answer for a stopped daemon, a stopped box, a box that
        // never had an agent, and an agent failing every attempt.
        let mut page = detail("activity");
        page.capture = activity::capture_view(false, "running", None);
        let down = page.render().unwrap();
        assert!(down.contains("Collector is not running"));
        assert!(down.contains("capture-down"));

        let mut page = detail("activity");
        page.capture = activity::capture_view(true, "stopped", None);
        let stopped = page.render().unwrap();
        assert!(stopped.contains("Box is stopped"));
        assert!(stopped.contains("capture-warn"));
    }

    #[test]
    fn a_failed_agent_names_the_command_that_fixes_it() {
        use crate::obs::health::{CaptureHealth, CaptureState};

        let health = CaptureHealth::new("alpha", CaptureState::Failed).with_detail(
            "agent closed the connection before saying hello — the agent said: \
             sh: /usr/local/bin/devbox-obsd: not found",
        );
        let mut page = detail("activity");
        page.capture = activity::capture_view(true, "running", Some(&health));

        let html = page.render().unwrap();
        assert!(html.contains("The agent could not start"));
        // The guest's own words, not just the collector's symptom.
        assert!(html.contains("devbox-obsd: not found"));
        // And what to do about it.
        assert!(html.contains("devbox reprovision"));
    }

    #[test]
    fn activity_tab_renders_every_view() {
        let html = detail_with_activity().render().unwrap();

        // The stream is driven by the collector's signal, not by a clock.
        assert!(html.contains("hx-get=\"/api/boxes/alpha/activity/tail\""));
        assert!(html.contains("sse:activity-alpha"));
        // …and stops while paused, or a paused stream is not paused.
        assert!(html.contains("act-pause"));
        // Flow table, with sizes a person can compare at a glance.
        assert!(html.contains("pypi.org"));
        assert!(html.contains("812 KB"), "raw byte counts are not a size");
        // DNS log.
        assert!(html.contains("151.101.0.223"));
        // Behaviour summary with its exports.
        assert!(html.contains("Behavior summary"));
        assert!(html.contains("behavior?format=markdown"));
        assert!(html.contains("behavior?format=jsonl"));
        assert!(html.contains("data-method=\"POST\""));
        assert!(html.contains("flows/pcap?proto=tcp"));
        assert!(html.contains("saddr=10.0.0.2"));
        assert!(!html.contains("sport=42000"));
    }

    #[test]
    fn activity_capture_omits_an_unspecified_listener_address() {
        let mut page = detail_with_activity();
        page.flows[0].saddr = "0.0.0.0".into();
        page.flows[0].capture_saddr = None;

        let html = page.render().unwrap();
        let capture = html
            .split("flows/pcap?")
            .nth(1)
            .and_then(|tail| tail.split("\">pcap").next())
            .expect("capture link");
        assert!(!capture.contains("saddr="), "got: {capture}");
        assert!(!capture.contains("sport="), "got: {capture}");
        assert!(capture.contains("daddr=151.101.0.223"));
        assert!(capture.contains("dport=443"));
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
            cursor: "7:41".into(),
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
        let html = ActivityStreamFragment {
            stream: vec![],
            cursor: "0:0".into(),
        }
        .render()
        .unwrap();
        assert!(html.contains("Nothing matches the current filter"));
    }

    fn fabric(json: &str) -> crate::lab::ztp_status::FabricView {
        crate::lab::ztp_status::view(serde_json::from_str(json).ok())
    }

    #[test]
    fn a_lab_that_was_never_brought_up_invites_rather_than_alarms() {
        let html = ZtpPanelFragment {
            ztp_lab: "ztp-fabric".into(),
            ztp: crate::lab::ztp_status::FabricView::default(),
        }
        .render()
        .unwrap();

        assert!(html.contains("Not running"));
        assert!(html.contains("Bring this lab up"));
        // No verdict at all rather than "not converged": a fabric that was
        // never asked to converge has not failed to.
        assert!(!html.contains("not converged"));
    }

    #[test]
    fn a_provisioning_fabric_shows_what_is_still_moving() {
        let html = ZtpPanelFragment {
            ztp_lab: "ztp-fabric".into(),
            ztp: fabric(
                r#"{"nodes":[
                     {"serial":"SN-1","name":"leaf1","role":"leaf","state":"healthy",
                      "attempts":1,"config_hash":"deadbeefcafe"},
                     {"serial":"SN-2","name":"leaf2","role":"leaf","state":"pushing",
                      "attempts":3}],
                   "healthy":1,"failed":0,"expected":3,"missing":["SN-3"],
                   "converged":false,"p95_secs":8.25}"#,
            ),
        }
        .render()
        .unwrap();

        assert!(html.contains("Provisioning"));
        assert!(html.contains("not converged"));
        assert!(
            html.contains("8.2s"),
            "the p95 that the SLO is written against"
        );
        // The serial the source of truth expects and has never heard from.
        assert!(html.contains("SN-3"));
        assert!(html.contains("never seen"));
        // Attempts above one is the recovery the chaos test exists to prove.
        assert!(html.contains("<b>3</b>"));
        // A config hash, shortened — enough to compare two nodes at a glance.
        assert!(html.contains("deadbeef"));
        assert!(!html.contains("deadbeefcafe"));
    }

    #[test]
    fn a_converged_fabric_says_so_once_and_plainly() {
        let html = ZtpPanelFragment {
            ztp_lab: "ztp-fabric".into(),
            ztp: fabric(
                r#"{"nodes":[{"serial":"SN-1","name":"leaf1","state":"healthy","attempts":1}],
                    "healthy":1,"failed":0,"expected":1,"missing":[],
                    "converged":true,"p95_secs":4.0}"#,
            ),
        }
        .render()
        .unwrap();

        assert!(html.contains("Converged"));
        assert!(html.contains("verdict-allowed"));
        assert!(!html.contains("never seen"));
    }

    #[test]
    fn the_topology_colours_a_blank_node_by_what_provisioning_did_to_it() {
        // Three grey outlines going green is the demonstration; a topology
        // that draws them identically throughout shows nothing happening.
        let mut view = super::super::labs::load(
            &std::sync::Arc::new(crate::sandbox::SandboxManager {
                state_dir: std::path::PathBuf::from("/tmp/devbox-ztp-render-test"),
            }),
            "ztp-fabric",
        )
        .expect("the built-in scenario loads");
        assert!(view.nodes.iter().all(|node| node.phase.is_empty()));

        super::super::labs::apply_phases(
            &mut view,
            &fabric(
                r#"{"nodes":[{"serial":"SN-1","name":"leaf1","state":"healthy"},
                             {"serial":"SN-2","name":"leaf2","state":"failed"}]}"#,
            ),
        );

        let html = LabViewFragment { view }.render().unwrap();
        assert!(html.contains("topo-node node-healthy"));
        assert!(html.contains("topo-node node-failed"));
        // A node the fabric said nothing about keeps the plain class.
        assert!(html.contains("class=\"topo-node \""));
    }

    #[test]
    fn terminal_tab_loads_xterm_only_when_shown() {
        assert!(!detail("overview").render().unwrap().contains("xterm.js"));

        let terminal = detail("terminal").render().unwrap();
        assert!(terminal.contains("/assets/js/xterm.js"));
        assert!(terminal.contains("/assets/js/term.js"));
        assert!(terminal.contains("data-start-endpoint=\"/api/boxes/alpha/start?terminal=true\""));
        assert!(!terminal.contains("hx-swap=\"none\""));
        assert!(terminal.contains("devbox:box-status"));
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
        assert!(html.contains("data-build-status=\"alpha\""));
        assert!(html.contains("/api/operations/alpha/status"));
        assert!(html.contains("3 set(s)"));
    }

    #[test]
    fn create_panel_protects_its_completion_from_a_stale_replay() {
        let html = CreatePanelFragment {
            retained: String::new(),
            name: "alpha".into(),
            summary: "a bare box".into(),
        }
        .render()
        .unwrap();
        assert!(html.contains("data-build-status=\"alpha\""));
        assert!(html.contains("sse-swap=\"build-status-alpha\""));
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
