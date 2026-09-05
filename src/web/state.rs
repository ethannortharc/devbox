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
    /// Per-launch bootstrap token, handed out once in the printed URL.
    ///
    /// Buys exactly one thing: the page that installs [`AppState::key`]. It is
    /// never accepted as authority for anything else (see [`super::auth`]).
    pub token: Arc<str>,
    /// Per-launch console key — the credential everything else is judged by.
    ///
    /// Distinct from `token` on purpose. The two secrets travel differently:
    /// the token rides a URL the user was shown, the key lives in the
    /// browser's origin-scoped storage and is presented explicitly. Deriving
    /// one from the other, or reusing a single value, would mean recovering
    /// either one recovers both — which is the whole failure this separation
    /// exists to prevent.
    pub key: Arc<str>,
    /// Random per-launch browser host, e.g. `devbox-<random>.localhost`.
    ///
    /// The listener still binds only to `127.0.0.1`; this hostname gives each
    /// launch a fresh browser origin. Tabs on the current origin can therefore
    /// share the key without handing it to an unrelated page that previously
    /// occupied the console's predictable loopback port.
    pub browser_host: Arc<str>,
    /// Actual bound port used when redirecting a bare loopback navigation to
    /// [`AppState::browser_host`].
    pub browser_port: u16,
    /// Fan-out hub for live console events.
    pub events: broadcast::Sender<ConsoleEvent>,
    /// Binary version, shown in the header.
    pub version: &'static str,
    /// The box statuses the watcher last probed, for readers that must not
    /// probe themselves.
    ///
    /// A runtime status costs a shell-out to a runtime CLI. The watcher pays
    /// for one every few seconds; the activity tail runs four times as often
    /// and must not pay for it again — but without it, its capture bar
    /// contradicted the page's, which had the status. One prober, one answer.
    pub box_status: Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
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
    /// Flipped once, when the console is asked to stop.
    ///
    /// Live streams watch it. `/api/stream` emits a heartbeat forever, so its
    /// response never completes — and axum's graceful shutdown waits for every
    /// accepted connection after its signal resolves. With the dashboard open,
    /// which `devbox web` opens by default, Ctrl-C therefore waited on a
    /// stream that had no reason to end. The signal is what gives it one.
    shutdown: Arc<tokio::sync::watch::Sender<bool>>,
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
}

impl AppState {
    pub fn new(
        manager: Arc<SandboxManager>,
        token: impl Into<Arc<str>>,
        key: impl Into<Arc<str>>,
    ) -> Self {
        Self::new_with_browser_origin(manager, token, key, "127.0.0.1", 7878)
    }

    /// Construct state for a live console's randomized browser origin.
    pub fn new_with_browser_origin(
        manager: Arc<SandboxManager>,
        token: impl Into<Arc<str>>,
        key: impl Into<Arc<str>>,
        browser_host: impl Into<Arc<str>>,
        browser_port: u16,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            manager,
            token: token.into(),
            key: key.into(),
            browser_host: browser_host.into(),
            browser_port,
            events,
            version: env!("CARGO_PKG_VERSION"),
            rebuilding: Arc::new(std::sync::Mutex::new(Default::default())),
            shutdown: Arc::new(tokio::sync::watch::channel(false).0),
            rebuilds_idle: Arc::new(tokio::sync::Notify::new()),
            build_status: Arc::new(std::sync::Mutex::new(Default::default())),
            box_status: Arc::new(std::sync::Mutex::new(Default::default())),
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

    /// Whether an asynchronous create or rebuild operation already owns the
    /// slot. Used to reconnect a refreshed page to the live status stream
    /// instead of replacing that stream with a conflict notice.
    pub fn is_rebuilding(&self, box_name: &str) -> bool {
        self.rebuilding
            .lock()
            .is_ok_and(|in_flight| in_flight.contains(box_name))
    }

    /// Tell every live stream to finish.
    pub fn begin_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Resolves once [`AppState::begin_shutdown`] has been called.
    ///
    /// A `watch` rather than a `Notify`: a stream that subscribes *after* the
    /// signal must still see it, and a notification nobody was waiting for is
    /// simply lost.
    // `use<>`: the future owns its receiver and must not borrow `self`, or a
    // handler cannot hand it to a stream it returns.
    pub fn shutting_down(&self) -> impl std::future::Future<Output = ()> + Send + use<> {
        let mut rx = self.shutdown.subscribe();
        async move {
            if rx.wait_for(|stopping| *stopping).await.is_err() {
                // The sender is gone, which is not the same as being asked to
                // stop. Resolving here would end every live stream whenever
                // the last `AppState` clone dropped — a different event, with
                // the same visible effect, which is how a test that should
                // have caught nothing catches everything.
                std::future::pending::<()>().await;
            }
        }
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
        // Pinning is not registering. `Notify` only holds a permit for a
        // waiter that has been polled at least once, so a rebuild finishing
        // between the check below and the first poll notified nobody — and
        // shutdown then sat out the whole timeout with nothing running.
        notified.as_mut().enable();

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

    /// Record what the watcher's latest probe saw.
    pub fn record_box_statuses(&self, boxes: &[(String, String)]) {
        if let Ok(mut known) = self.box_status.lock() {
            known.clear();
            known.extend(boxes.iter().cloned());
        }
    }

    /// The watcher's latest reading for a box, or empty when it has none.
    ///
    /// Empty means "not probed", which every reader treats as undecided rather
    /// than as down.
    pub fn known_box_status(&self, name: &str) -> String {
        self.box_status
            .lock()
            .ok()
            .and_then(|known| known.get(name).cloned())
            .unwrap_or_default()
    }

    /// The last build status for a box, if one finished without being seen.
    pub fn retained_build_status(&self, box_name: &str) -> Option<String> {
        self.build_status.lock().ok()?.get(box_name).cloned()
    }

    /// Snapshot all terminal build states for an SSE replay.
    pub fn retained_build_statuses(&self) -> Vec<(String, String)> {
        self.build_status
            .lock()
            .map(|retained| {
                retained
                    .iter()
                    .map(|(name, html)| (name.clone(), html.clone()))
                    .collect()
            })
            .unwrap_or_default()
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
        AppState::new(manager, "token", "key")
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
