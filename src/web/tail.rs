//! Live activity tail.
//!
//! The collector is a separate process ([`crate::obs::daemon`]), so its
//! in-process fan-out cannot reach the console. The event store is the shared
//! medium instead, and this loop watches it: one indexed `SELECT MAX(id)` per
//! box per tick, which costs nothing on an idle box and notices a burst within
//! half a second.
//!
//! What it publishes is a *signal*, not the rows. Pushing rendered rows to
//! every console would be cheaper by one round trip and wrong in two ways: the
//! anchor would be the server's rather than each page's, so a page that
//! painted at a different moment sees duplicates or gaps; and the server would
//! have to render one payload for readers whose filters differ. A page that
//! hears the signal asks for what *it* is missing, filtered the way *it* asked,
//! and the two problems do not arise.
//!
//! Like the box watcher, it idles entirely while no console is connected.

use std::collections::HashMap;
use std::time::Duration;

use askama::Template;

use super::activity::{self, CaptureView};
use super::state::{AppState, ConsoleEvent};

/// How often the store is checked for new events while a console is open.
///
/// Fast enough that a live stream reads as live, slow enough that an idle box
/// costs one primary-key lookup twice a second.
pub const TAIL_INTERVAL: Duration = Duration::from_millis(500);

/// How often to re-check for a connected console while idle.
const IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// Capture health changes on human timescales, not event timescales.
const HEALTH_EVERY: u32 = 4;

#[derive(Template)]
#[template(path = "_capture_bar.html")]
struct CaptureBarFragment {
    capture: CaptureView,
}

/// The scoped SSE event that tells one box's Activity tab there is more.
pub fn activity_event(name: &str) -> String {
    format!("activity-{}", crate::web::encode_segment(name))
}

/// The scoped SSE event carrying one box's capture status bar.
pub fn capture_event(name: &str) -> String {
    format!("capture-{}", crate::web::encode_segment(name))
}

/// Render one box's capture status bar, if the collector has a record for it.
///
/// Shared with the SSE replay in [`super::watch::snapshot_events`]. The live
/// loop suppresses a bar it has already sent, so a tab that was disconnected
/// while the state changed would never be told — the replay is what closes
/// that, and it has to render the same bar the loop would.
pub(crate) fn capture_bar_html(state: &AppState, name: &str) -> Option<String> {
    // A missing record is a reading too. Returning `None` for it left a tab
    // showing the *previous* box's "Capturing" bar after a same-name
    // recreation cleared the record, with nothing scheduled to correct it.
    let record = crate::obs::health::load(&state.manager.state_dir, name)
        .ok()
        .flatten();
    let daemon_running = crate::obs::daemon::status(&state.manager)
        .map(|owner| owner.is_some())
        .unwrap_or(false);
    let status = state.known_box_status(name);
    let view = activity::capture_view(daemon_running, &status, record.as_ref());
    (CaptureBarFragment { capture: view })
        .render()
        .ok()
        .map(|html| super::watch::collapse_newlines(&html))
}

/// Run the tail loop until the process exits.
pub async fn run(state: AppState) {
    // Seeded from the current high-water mark, so opening a console does not
    // announce every event the box produced before anyone was watching.
    let mut anchors: HashMap<String, i64> = HashMap::new();
    let mut health: HashMap<String, String> = HashMap::new();
    // Stores are kept open across ticks. Reopening one is not free: it runs
    // the busy-timeout and journal pragmas and the `CREATE TABLE IF NOT EXISTS`
    // schema check, and paying that per box twice a second — on the same
    // database the collector is writing to — is contention bought for nothing.
    // Keyed by the file behind the store as well as by its name. A handle
    // stays attached to the inode it opened, so a box destroyed and recreated
    // under the same name leaves the cached handle reading a deleted database
    // — which reports no new events, forever, in complete silence.
    //
    // The inode is enough here and would not be enough for a cursor: a reused
    // one costs this loop one missed invalidation, which the anchor-went-
    // backwards branch below also catches.
    let mut stores: HashMap<String, (u64, crate::obs::Store)> = HashMap::new();
    let mut tick: u32 = 0;

    loop {
        if state.events.receiver_count() == 0 {
            tokio::time::sleep(IDLE_INTERVAL).await;
            continue;
        }
        tokio::time::sleep(TAIL_INTERVAL).await;
        tick = tick.wrapping_add(1);

        let boxes = match state.manager.list_sandboxes() {
            Ok(boxes) => boxes,
            Err(error) => {
                tracing::warn!(%error, "activity tail could not list boxes");
                continue;
            }
        };

        // Forget boxes that no longer exist. Otherwise a long-running console
        // that creates and destroys uniquely named boxes accumulates one entry
        // per box for the life of the process — including the open store.
        let live: std::collections::HashSet<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
        anchors.retain(|name, _| live.contains(name.as_str()));
        health.retain(|name, _| live.contains(name.as_str()));
        stores.retain(|name, _| live.contains(name.as_str()));

        for sandbox in &boxes {
            let name = &sandbox.name;
            let identity = activity::store_file_id(&state.manager, name);
            if identity == 0 {
                stores.remove(name);
                continue;
            }
            if stores.get(name).is_none_or(|(seen, _)| *seen != identity) {
                match activity::open_store(&state.manager, name) {
                    Ok(Some(store)) => {
                        stores.insert(name.clone(), (identity, store));
                        // A replacement store is a new box under an old name.
                        // Drop the anchor with it, or the seed below is skipped
                        // and the first tick compares two unrelated id spaces.
                        anchors.remove(name);
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::debug!(box_id = %name, %error, "activity tail could not open a store");
                        continue;
                    }
                }
            }
            let Some((_, store)) = stores.get(name) else {
                continue;
            };
            let Ok(newest) = store.max_id() else {
                // A handle that has stopped answering is not one to keep.
                stores.remove(name);
                continue;
            };

            match anchors.get(name) {
                // First sight of this box's store. Signal as well as seed: a
                // page can be open from before capture started, and if the
                // first batch lands before this tick it is inside the seed and
                // invisible until the twenty-second net. The page asks from
                // its own cursor, so a signal it did not need costs one empty
                // request.
                None => {
                    anchors.insert(name.clone(), newest);
                    if newest > 0 {
                        state.publish(ConsoleEvent::new(activity_event(name), newest.to_string()));
                    }
                }
                Some(&seen) if newest > seen => {
                    anchors.insert(name.clone(), newest);
                    state.publish(ConsoleEvent::new(activity_event(name), newest.to_string()));
                }
                // A store that went backwards was rebuilt under us. The
                // identity check above normally catches this first; signal
                // anyway, because an open page is holding a cursor from the
                // previous incarnation and only finds out by asking.
                Some(&seen) if newest < seen => {
                    anchors.insert(name.clone(), newest);
                    state.publish(ConsoleEvent::new(activity_event(name), newest.to_string()));
                }
                Some(_) => {}
            }
        }

        if !tick.is_multiple_of(HEALTH_EVERY) {
            continue;
        }
        for sandbox in &boxes {
            let name = &sandbox.name;
            // Only boxes the collector has written a record for. Without one,
            // the bar's reading depends on a runtime status probe this loop
            // deliberately does not run, and pushing a guess would replace an
            // accurate first paint with a worse one.
            // The watcher's latest reading, not a probe of our own: resolving
            // a status means shelling out to a runtime CLI, which is its job
            // on its slower cadence. Reading it rather than passing "unknown"
            // is what keeps this bar from contradicting the one the page
            // rendered with the same information.
            let Some(html) = capture_bar_html(&state, name) else {
                continue;
            };

            // Compared on the rendered bar, not on a summary of it. A
            // fingerprint of level and headline alone held a reconnected
            // agent's page on the previous build's version and backend list,
            // because those live in the facts row and the headline had not
            // changed.
            if health.get(name) == Some(&html) {
                continue;
            }
            health.insert(name.clone(), html.clone());
            state.publish(ConsoleEvent::new(capture_event(name), html));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_events_encode_awkward_box_names() {
        assert_eq!(activity_event("alpha"), "activity-alpha");
        assert_eq!(capture_event("alpha"), "capture-alpha");
        // Same encoding as the path segment, so a name with a space cannot
        // produce an event name htmx will not match.
        assert!(!activity_event("my box").contains(' '));
    }
}
