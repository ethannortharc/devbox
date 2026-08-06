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

use std::time::Duration;

use askama::Template;

use super::routes::dashboard_subtitle;
use super::service::{self, BoxSummary};
use super::state::{AppState, ConsoleEvent};

/// How often to re-check box status while a console is open.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often to re-check for a connected console while idle.
const IDLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Template)]
#[template(path = "_box_grid.html")]
struct BoxGridFragment {
    boxes: Vec<BoxSummary>,
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

/// Run the watch loop until the process exits.
pub async fn run(state: AppState) {
    // Seed with the current view so the first swap reflects a real change
    // rather than re-pushing what the page already rendered.
    let mut last = match service::list_boxes(&state.manager).await {
        Ok(boxes) => fingerprint(&boxes),
        Err(e) => {
            tracing::warn!(error = %e, "box watcher could not read initial state");
            String::new()
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

        let next = fingerprint(&boxes);
        if next == last {
            continue;
        }
        last = next;

        let subtitle = dashboard_subtitle(&boxes);
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
}
