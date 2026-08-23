//! Server-Sent Events — the console's live channel.
//!
//! One SSE connection per open page carries every live signal: the heartbeat
//! that drives the "live" indicator, box status changes, build progress, and
//! (from Phase 3) the observability event feed. WebSockets are reserved for
//! the interactive terminal, which is the only genuinely bidirectional view.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};

use super::state::AppState;

/// Heartbeat interval. Fast enough that a dead server is obvious within a few
/// seconds, slow enough to be invisible in CPU terms.
pub const HEARTBEAT: Duration = Duration::from_secs(2);

/// Render the heartbeat payload — the small fragment htmx swaps into the
/// status pill in the header.
pub fn tick_payload(now: chrono::DateTime<chrono::Local>) -> String {
    format!("live · {}", now.format("%H:%M:%S"))
}

/// `GET /api/stream` — the console's live event stream.
pub async fn stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before rendering the replay. Any state transition that lands
    // during the runtime probes is queued and follows the snapshot, so the
    // newer event always wins.
    let receiver = state.events.subscribe();

    let ticks = IntervalStream::new(tokio::time::interval(HEARTBEAT)).map(|_| {
        Ok(Event::default()
            .event("tick")
            .data(tick_payload(chrono::Local::now())))
    });

    let initial_state = state.clone();
    let replay = stream::once(async move { super::watch::snapshot_events(&initial_state).await })
        .flat_map(|events| stream::iter(events.into_iter().map(console_event)));
    let recovery_state = state.clone();
    let events = BroadcastStream::new(receiver)
        .then(move |item| {
            let state = recovery_state.clone();
            async move {
                match item {
                    Ok(event) => vec![event],
                    Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                        tracing::warn!(
                            dropped = n,
                            "console SSE subscriber lagged; replaying current state"
                        );
                        super::watch::snapshot_events(&state).await
                    }
                }
            }
        })
        .flat_map(|events| stream::iter(events.into_iter().map(console_event)));
    let state_events = replay.chain(events);

    // Ends when the console is asked to stop.
    //
    // Without this the stream had no reason to complete — a heartbeat forever
    // — and axum's graceful shutdown waits for every accepted connection once
    // its signal resolves. With a dashboard open, which `devbox web` opens by
    // default, Ctrl-C waited on a stream that was never going to end.
    let live = futures::stream::select(ticks, state_events).take_until(state.shutting_down());
    Sse::new(live).keep_alive(KeepAlive::default())
}

fn console_event(event: super::state::ConsoleEvent) -> Result<Event, Infallible> {
    Ok(Event::default().event(event.kind).data(event.data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::state::ConsoleEvent;
    use chrono::TimeZone;

    #[test]
    fn tick_payload_is_a_wall_clock_time() {
        let t = chrono::Local
            .with_ymd_and_hms(2026, 8, 6, 22, 14, 7)
            .unwrap();
        assert_eq!(tick_payload(t), "live · 22:14:07");
    }

    #[test]
    fn heartbeat_is_a_couple_of_seconds() {
        // Guards against a fat-fingered unit change making the console spin.
        assert!(HEARTBEAT >= Duration::from_secs(1));
        assert!(HEARTBEAT <= Duration::from_secs(10));
    }

    #[tokio::test]
    async fn broadcast_events_reach_a_subscriber_stream() {
        let (tx, _) = tokio::sync::broadcast::channel::<ConsoleEvent>(8);
        let mut s = BroadcastStream::new(tx.subscribe());
        tx.send(ConsoleEvent::new("box-status", "<span>running</span>"))
            .unwrap();
        let got = s.next().await.unwrap().unwrap();
        assert_eq!(got.kind, "box-status");
    }
}
