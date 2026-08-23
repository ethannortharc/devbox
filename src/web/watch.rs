//! Box status watcher.
//!
//! Keeps the dashboard live without the page polling: a background task
//! re-lists boxes on an interval and publishes a re-rendered grid whenever
//! anything actually changed.
//!
//! It only runs while a console is connected. Each poll shells out to a
//! runtime CLI (`limactl`, `docker`, …) per box, and doing that forever in the
//! background of a long-lived `devbox web` with no browser open would be pure
//! waste — so the loop idles until [`broadcast::Sender::receiver_count`] says
//! someone is listening.

use std::collections::BTreeSet;
use std::time::Duration;

use askama::Template;

use super::routes::dashboard_subtitle;
use super::service::{self, BoxSummary};
use super::state::{AppState, ConsoleEvent};

/// How often to re-check box status while a console is open.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often to re-check for a connected console while idle.
const IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// Capture bars one SSE replay will carry.
const MAX_REPLAYED_CAPTURE_BARS: usize = 64;

#[derive(Template)]
#[template(path = "_box_grid.html")]
struct BoxGridFragment {
    boxes: Vec<BoxSummary>,
}

#[derive(Template)]
#[template(path = "_box_card.html")]
struct BoxCardFragment {
    b: BoxSummary,
    notice: String,
}

/// The scoped SSE event that carries one detail page's live control card.
pub fn box_card_event(name: &str) -> String {
    format!("box-card-{}", crate::web::encode_segment(name))
}

/// Publish what a status probe just saw, for readers that must not probe.
fn record_statuses(state: &AppState, boxes: &[BoxSummary]) {
    state.record_box_statuses(
        &boxes
            .iter()
            .map(|b| (b.name.clone(), b.status.clone()))
            .collect::<Vec<_>>(),
    );
}

fn box_names(boxes: &[BoxSummary]) -> BTreeSet<String> {
    boxes.iter().map(|b| b.name.clone()).collect()
}

fn removed_box_fragment(name: &str) -> String {
    format!(
        "<div class=\"inline-feedback warning\" data-box-removed=\"true\" role=\"status\">Box <strong>{}</strong> was removed. Returning to Boxes…</div>",
        super::build::escape_html(name)
    )
}

/// A cheap change key for a box list.
///
/// Only the fields the grid actually renders take part, so an unrelated state
/// change does not cause a needless DOM swap (which would blow away hover and
/// focus in the browser).
pub fn fingerprint(boxes: &[BoxSummary]) -> String {
    boxes
        .iter()
        .map(|b| {
            format!(
                "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                b.name,
                b.status,
                b.runtime,
                b.mount_mode,
                b.sets.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("\u{2}")
}

/// Render a complete recoverable view of the console's live state.
///
/// A broadcast receiver can miss events while a tab is suspended, and a
/// reconnect starts with an empty channel history. This replay is generated
/// after subscribing, so any transition that happens while it is rendered is
/// queued behind it and wins in order.
pub async fn snapshot_events(state: &AppState) -> Vec<ConsoleEvent> {
    let boxes = match service::list_boxes(&state.manager).await {
        Ok(boxes) => boxes,
        Err(error) => {
            tracing::warn!(%error, "could not render console SSE snapshot");
            return Vec::new();
        }
    };
    let mut events = Vec::new();

    // This probe is not free, and it is the freshest reading anyone has. The
    // capture bars below read it back rather than reporting "unknown", which
    // is what a bar rendered before the watcher's first poll used to do.
    record_statuses(state, &boxes);

    // Detail pages use this to detect a box that disappeared while their SSE
    // connection was down. JSON is consumed by script and never swapped.
    let names: Vec<String> = boxes.iter().map(|b| b.name.clone()).collect();
    if let Ok(inventory) = serde_json::to_string(&names) {
        events.push(ConsoleEvent::new("box-inventory", inventory));
    }

    for b in &boxes {
        match (BoxCardFragment {
            b: b.clone(),
            notice: String::new(),
        })
        .render()
        {
            Ok(html) => events.push(ConsoleEvent::new(
                box_card_event(&b.name),
                collapse_newlines(&html),
            )),
            Err(error) => tracing::error!(
                box_id = %b.name,
                %error,
                "box card SSE snapshot failed to render"
            ),
        }
    }

    let subtitle = dashboard_subtitle(&boxes);
    match (BoxGridFragment { boxes }).render() {
        Ok(html) => {
            events.push(ConsoleEvent::new("boxes", collapse_newlines(&html)));
            events.push(ConsoleEvent::new("box-subtitle", subtitle));
        }
        Err(error) => tracing::error!(%error, "box grid SSE snapshot failed to render"),
    }

    for (name, html) in state.retained_build_statuses() {
        events.push(ConsoleEvent::new(super::build::status_event(&name), html));
    }

    // Capture bars belong in the replay too. The tail loop sends one only when
    // it changes, so a tab that was disconnected across the change — or whose
    // broadcast lagged — would sit on a stale "Capturing" indefinitely with
    // nothing left to correct it.
    //
    // Bounded: a replay is built and broadcast on every reconnect and every
    // lagged subscriber, and one per box means its cost grows with the
    // registry. Past this many boxes a page relies on the live channel and on
    // its own reload, which is the same thing a page that missed one bar
    // already does.
    for name in names.iter().take(MAX_REPLAYED_CAPTURE_BARS) {
        if let Some(html) = super::tail::capture_bar_html(state, name) {
            events.push(ConsoleEvent::new(super::tail::capture_event(name), html));
        }
    }
    if names.len() > MAX_REPLAYED_CAPTURE_BARS {
        tracing::debug!(
            boxes = names.len(),
            limit = MAX_REPLAYED_CAPTURE_BARS,
            "capture bars omitted from the SSE replay"
        );
    }
    events
}

/// Run the watch loop until the process exits.
pub async fn run(state: AppState) {
    // Seed with the current view so the first swap reflects a real change
    // rather than re-pushing what the page already rendered.
    let (mut last, mut previous_names) = match service::list_boxes(&state.manager).await {
        Ok(boxes) => {
            // Seeded here as well as in the loop. Discarding this probe left
            // every reader that asks for a status with nothing for the first
            // few seconds of a console's life — which is exactly when someone
            // is looking at it.
            record_statuses(&state, &boxes);
            (fingerprint(&boxes), box_names(&boxes))
        }
        Err(e) => {
            tracing::warn!(error = %e, "box watcher could not read initial state");
            (String::new(), BTreeSet::new())
        }
    };

    loop {
        if state.events.receiver_count() == 0 {
            tokio::time::sleep(IDLE_INTERVAL).await;
            continue;
        }
        tokio::time::sleep(POLL_INTERVAL).await;

        let boxes = match service::list_boxes(&state.manager).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "box watcher poll failed");
                continue;
            }
        };

        // Recorded on every poll, not only on a change: the activity tail
        // reads this instead of probing, and a first poll that happened to
        // match the seed would otherwise leave it with nothing.
        record_statuses(&state, &boxes);

        let next = fingerprint(&boxes);
        if next == last {
            continue;
        }
        let next_names = box_names(&boxes);

        // A detail page has no grid cell to remove. Send a scoped tombstone
        // before publishing the new collection so an open page for a box
        // destroyed from the CLI or another tab cannot keep presenting stale
        // controls and a false "running" badge indefinitely.
        for removed in previous_names.difference(&next_names) {
            state.publish(ConsoleEvent::new(
                box_card_event(removed),
                removed_box_fragment(removed),
            ));
        }

        last = next;
        previous_names = next_names;

        let subtitle = dashboard_subtitle(&boxes);

        // A detail page does not contain the dashboard grid, so give its one
        // card a scoped stream as well. The path-segment encoding is also a
        // stable event-name encoding for box names containing spaces or `#`.
        for b in &boxes {
            match (BoxCardFragment {
                b: b.clone(),
                notice: String::new(),
            })
            .render()
            {
                Ok(html) => {
                    state.publish(ConsoleEvent::new(
                        box_card_event(&b.name),
                        collapse_newlines(&html),
                    ));
                }
                Err(e) => {
                    tracing::error!(box_id = %b.name, error = %e, "box card fragment failed to render")
                }
            };
        }

        match (BoxGridFragment { boxes }).render() {
            Ok(html) => {
                // SSE data may not contain bare newlines: each would be read as
                // a separate `data:` line and the fragment would be reassembled
                // with them re-inserted. Collapsing here keeps the HTML intact.
                state.publish(ConsoleEvent::new("boxes", collapse_newlines(&html)));
                state.publish(ConsoleEvent::new("box-subtitle", subtitle));
            }
            Err(e) => tracing::error!(error = %e, "box grid fragment failed to render"),
        }
    }
}

/// Flatten a rendered fragment into a single SSE-safe line.
pub fn collapse_newlines(html: &str) -> String {
    html.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn summary(name: &str, status: &str) -> BoxSummary {
        BoxSummary {
            name: name.into(),
            runtime: "docker".into(),
            project_dir: "/tmp/x".into(),
            status: status.into(),
            mount_mode: "overlay".into(),
            sets: vec!["system".into()],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            created_at: String::new(),
        }
    }

    #[test]
    fn fingerprint_changes_when_status_changes() {
        let a = vec![summary("x", "stopped")];
        let b = vec![summary("x", "running")];
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_is_stable_for_identical_lists() {
        let a = vec![summary("x", "running"), summary("y", "stopped")];
        let b = vec![summary("x", "running"), summary("y", "stopped")];
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_ignores_fields_the_grid_does_not_render() {
        let mut a = summary("x", "running");
        let mut b = summary("x", "running");
        a.created_at = "2026-01-01".into();
        b.created_at = "2026-09-09".into();
        assert_eq!(fingerprint(&[a]), fingerprint(&[b]));
    }

    #[test]
    fn fingerprint_separator_resists_name_collisions() {
        // Two boxes whose fields concatenate to the same string must not
        // fingerprint alike.
        let one = vec![summary("a", "running"), summary("b", "stopped")];
        let two = vec![summary("a", "running\u{1}docker\u{1}overlay\u{1}system")];
        assert_ne!(fingerprint(&one), fingerprint(&two));
    }

    #[test]
    fn card_events_are_safe_and_stable_for_non_url_names() {
        assert_eq!(box_card_event("my box#1"), "box-card-my%20box%231");
    }

    #[test]
    fn a_removed_box_tombstone_is_visible_and_escaped() {
        let html = removed_box_fragment("<gone>");
        assert!(html.contains("data-box-removed=\"true\""));
        assert!(html.contains("&lt;gone&gt;"));
        assert!(!html.contains("<gone>"));
    }

    #[test]
    fn collapse_newlines_produces_one_line() {
        let html = "<div>\n  <span>hi</span>\n\n</div>\n";
        let out = collapse_newlines(html);
        assert!(!out.contains('\n'));
        assert!(out.contains("<span>hi</span>"));
    }

    #[test]
    fn grid_fragment_renders_cards_without_page_chrome() {
        let html = (BoxGridFragment {
            boxes: vec![summary("alpha", "running")],
        })
        .render()
        .unwrap();
        assert!(html.contains("alpha"));
        assert!(html.contains("status-running"));
        assert!(!html.contains("<html"));
    }

    #[test]
    fn grid_fragment_renders_the_empty_state() {
        let html = (BoxGridFragment { boxes: vec![] }).render().unwrap();
        assert!(html.contains("No boxes yet"));
    }

    #[tokio::test]
    async fn reconnect_snapshot_replays_inventory_grid_and_terminal_build_status() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Arc::new(crate::sandbox::SandboxManager {
                state_dir: dir.path().to_path_buf(),
            }),
            "token",
            "key",
        );
        state.publish(ConsoleEvent::new(
            super::super::build::status_event("alpha"),
            "<span>done</span>",
        ));

        let events = snapshot_events(&state).await;
        let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
        assert!(kinds.contains(&"box-inventory"));
        assert!(kinds.contains(&"boxes"));
        assert!(kinds.contains(&"box-subtitle"));
        assert!(kinds.contains(&"build-status-alpha"));
    }
}
