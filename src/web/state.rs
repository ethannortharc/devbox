//! Shared server state.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::sandbox::SandboxManager;

/// How many events the broadcast hub buffers per subscriber before a slow
/// browser starts losing them. Lag is reported, never silent (§7.3).
pub const EVENT_BUFFER: usize = 512;

/// One message on the console's Server-Sent Events channel.
///
/// `kind` becomes the SSE `event:` name that htmx matches with `sse-swap`;
/// `data` is the payload — an HTML fragment for htmx targets, JSON for
/// script-driven consumers.
#[derive(Debug, Clone, Serialize)]
pub struct ConsoleEvent {
    pub kind: String,
    pub data: String,
}

impl ConsoleEvent {
    pub fn new(kind: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            data: data.into(),
        }
    }
}

/// State shared by every handler. Cheap to clone — everything inside is either
/// an `Arc` or a channel handle.
#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<SandboxManager>,
    /// Per-launch console token (see [`super::auth`]).
    pub token: Arc<str>,
    /// Fan-out hub for live console events.
    pub events: broadcast::Sender<ConsoleEvent>,
    /// Binary version, shown in the header.
    pub version: &'static str,
    /// Boxes with a rebuild in flight.
    ///
    /// Two submissions from a double-click or two tabs would otherwise both
    /// overwrite the same `/etc/devbox/devbox.nix`, rebuild, and then each
    /// persist *its own* selection — leaving the recorded state describing a
    /// generation that was never built.
    pub rebuilding: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// Collector counters, surfaced by `/metrics` (§7.7).
    ///
    /// Shared with the collector task when one is running; a console started
    /// without a collector simply reports zeroes, which is honest — no agent
    /// has connected.
    pub collector_stats: Arc<crate::obs::collector::Stats>,
}

impl AppState {
    pub fn new(manager: Arc<SandboxManager>, token: impl Into<Arc<str>>) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            manager,
            token: token.into(),
            events,
            version: env!("CARGO_PKG_VERSION"),
            rebuilding: Arc::new(std::sync::Mutex::new(Default::default())),
            collector_stats: Arc::new(crate::obs::collector::Stats::default()),
        }
    }

    /// Claim the rebuild slot for a box, if it is free.
    ///
    /// Returns `None` when a rebuild is already running; the guard releases the
    /// slot when dropped, including on a panic.
    pub fn claim_rebuild(&self, box_name: &str) -> Option<RebuildGuard> {
        let mut in_flight = self.rebuilding.lock().ok()?;
        if !in_flight.insert(box_name.to_string()) {
            return None;
        }
        Some(RebuildGuard {
            slots: self.rebuilding.clone(),
            box_name: box_name.to_string(),
        })
    }

    /// Publish an event to every connected console.
    ///
    /// Returns the number of receivers reached. Zero is normal — it just means
    /// no browser is open — so this never errors.
    pub fn publish(&self, event: ConsoleEvent) -> usize {
        self.events.send(event).unwrap_or(0)
    }
}

/// Holds a box's rebuild slot until dropped.
pub struct RebuildGuard {
    slots: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    box_name: String,
}

impl Drop for RebuildGuard {
    fn drop(&mut self) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.remove(&self.box_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let manager = Arc::new(SandboxManager {
            state_dir: std::path::PathBuf::from("/tmp/devbox-test-state"),
        });
        AppState::new(manager, "token")
    }

    #[test]
    fn publish_with_no_subscribers_is_not_an_error() {
        let s = state();
        assert_eq!(s.publish(ConsoleEvent::new("tick", "hi")), 0);
    }

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let s = state();
        let mut rx = s.events.subscribe();
        assert_eq!(s.publish(ConsoleEvent::new("tick", "hello")), 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.kind, "tick");
        assert_eq!(got.data, "hello");
    }
}
