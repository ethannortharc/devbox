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
    /// The last terminal build status per box, for replay.
    ///
    /// The broadcast channel has no history, and the request that starts a
    /// rebuild is the same one that returns the replacement panel — so a fast
    /// failure can publish its status before htmx has installed the element
    /// that would show it, leaving the panel on "Rebuilding…" forever.
    ///
    /// A previous attempt waited for a subscriber, which does not work: the
    /// page-level `/api/stream` subscription already makes the receiver count
    /// nonzero, and `sse-swap` elements create no server-side receiver. There
    /// is nothing to wait *for*, so the fix is to keep the answer instead of
    /// trying to time its delivery.
    pub build_status: Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
    /// Boxes with a rebuild in flight.
    ///
    /// Two submissions from a double-click or two tabs would otherwise both
    /// overwrite the same `/etc/devbox/devbox.nix`, rebuild, and then each
    /// persist *its own* selection — leaving the recorded state describing a
    /// generation that was never built.
    pub rebuilding: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// Notified when the last in-flight rebuild finishes.
    ///
    /// A rebuild runs in a detached task, because a `nixos-rebuild` takes
    /// minutes and the request that starts it has to return the log panel
    /// immediately. Detached meant the runtime dropped it the moment `serve`
    /// returned: Ctrl-C partway through left the generated files already
    /// replaced and `apply_selection` never reaching its rollback, its posture
    /// restore, or its state write. `kill_on_drop` ends the child process and
    /// runs none of that cleanup — the box is left mid-selection with its
    /// firewall down and nothing recording it.
    rebuilds_idle: Arc<tokio::sync::Notify>,
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
            rebuilds_idle: Arc::new(tokio::sync::Notify::new()),
            build_status: Arc::new(std::sync::Mutex::new(Default::default())),
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
            idle: self.rebuilds_idle.clone(),
        })
    }

    /// Wait until no rebuild is in flight, or until `limit` elapses.
    ///
    /// Called from the shutdown path. Waiting is the right default even though
    /// it delays exit: the alternative is dropping a rebuild between replacing
    /// a box's generated files and putting them back, which leaves the box on
    /// a selection nobody chose with its posture not restored. The bound is
    /// there so a wedged rebuild cannot make Ctrl-C do nothing at all.
    ///
    /// Returns whether it drained.
    pub async fn wait_for_rebuilds(&self, limit: std::time::Duration) -> bool {
        // Subscribe before checking, or a rebuild that finishes in between is
        // a notification nobody is waiting for and this blocks until the
        // timeout on an idle console.
        let idle = self.rebuilds_idle.clone();
        let notified = idle.notified();
        tokio::pin!(notified);

        if self.rebuilding.lock().is_ok_and(|s| s.is_empty()) {
            return true;
        }
        tokio::time::timeout(limit, notified).await.is_ok()
    }

    /// Publish an event to every connected console.
    ///
    /// Returns the number of receivers reached. Zero is normal — it just means
    /// no browser is open — so this never errors.
    pub fn publish(&self, event: ConsoleEvent) -> usize {
        // Retain terminal build statuses so a panel that missed the broadcast
        // can render it on arrival.
        if let Some(box_name) = event.kind.strip_prefix("build-status-")
            && let Ok(mut retained) = self.build_status.lock()
        {
            retained.insert(box_name.to_string(), event.data.clone());
        }
        self.events.send(event).unwrap_or(0)
    }

    /// The last build status for a box, if one finished without being seen.
    pub fn retained_build_status(&self, box_name: &str) -> Option<String> {
        self.build_status.lock().ok()?.get(box_name).cloned()
    }

    /// Forget a box's retained status, when a new build starts.
    pub fn clear_build_status(&self, box_name: &str) {
        if let Ok(mut retained) = self.build_status.lock() {
            retained.remove(box_name);
        }
    }
}

/// Holds a box's rebuild slot until dropped.
pub struct RebuildGuard {
    slots: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    box_name: String,
    /// Woken when the last rebuild finishes, so shutdown can wait for it.
    idle: Arc<tokio::sync::Notify>,
}

impl Drop for RebuildGuard {
    fn drop(&mut self) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.remove(&self.box_name);
            if slots.is_empty() {
                self.idle.notify_waiters();
            }
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
