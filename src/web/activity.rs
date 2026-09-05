//! Activity views — §7.5.
//!
//! Three layers over one store, all derived here as plain data so the
//! templates stay dumb and the derivations stay testable:
//!
//! 1. **Situation** — [`capture_view`] (is capture even working, and if not,
//!    why), [`timeline`] (event density over the window) and [`Totals`].
//! 2. **Views** — [`flows`], [`peers`], [`lookups`], [`process_tree`],
//!    [`violations`], and the file log.
//! 3. **Stream** — [`StreamRow`], filtered by [`Filter`].
//!
//! Filtering happens *after* DNS correlation, never in the SQL. A `connect`
//! event records the address it dialled and the name arrives in a separate
//! `dns` event, so a `type = connect` predicate in the query removes exactly
//! the rows that would have named the addresses — the trap `cli::watch`
//! documents having fallen into once already.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::obs::behavior::{self, Summary};
use crate::obs::correlate::{self, Chain};
use crate::obs::event::{Event, EventType};
use crate::obs::health::{CaptureHealth, CaptureState, DEGRADED_LOSS, capture_composition};
use crate::obs::store::{Query, Store};
use crate::sandbox::SandboxManager;

/// How many events the Activity tab loads on first paint.
///
/// Enough to see what just happened, small enough that the page renders
/// instantly; the live stream carries everything after that.
pub const INITIAL_EVENTS: usize = 200;

/// Most rows one live tail will deliver.
///
/// A box that produces a burst faster than the console drains it must not be
/// able to push a megabyte of HTML through one SSE message. The anchor is
/// still advanced past everything read, so a burst thins the stream rather
/// than backing it up — the same choice the collector's bounded queue makes.
pub const TAIL_LIMIT: usize = 250;

/// Largest stored event the console will render.
///
/// The transport accepts frames up to a megabyte, and the console reads two
/// hundred rows several times a minute — so without a size bound a box that
/// emits maximal events can make its own console allocate and render hundreds
/// of megabytes on a timer. Anything above this is still captured, still
/// exported, and still visible through `devbox watch`; it is only left out of
/// the views that re-read themselves.
pub const MAX_RENDERED_EVENT_BYTES: usize = 64 * 1024;

/// Longest free-text filter the console will act on.
pub const MAX_QUERY_CHARS: usize = 256;

/// A page's position in one box's event store.
///
/// The row id alone cannot express a position. A box destroyed and recreated
/// under the same name gets a *different* store whose ids start again at one,
/// and a page still holding id 10 from the old one either waits for the new
/// box to reach id 10 — skipping its first ten events — or, once it has, mixes
/// two boxes' events into one timeline with nothing to mark the seam. The
/// store's inode tells them apart, and is stable for as long as the file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub store: u64,
    pub row: i64,
}

impl Cursor {
    /// Read a cursor a page sent back. Anything unparseable reads as the
    /// beginning, which is safe: the caller resets rather than trusting it.
    pub fn parse(raw: &str) -> Self {
        let (store, row) = raw.split_once(':').unwrap_or(("0", "0"));
        Self {
            store: store.parse().unwrap_or(0),
            // Clamped at zero. This value arrives in a query string, so a
            // negative one is a request away — and `newest - row` on
            // `i64::MIN` overflows, which panics a checked build and silently
            // wraps a release one.
            row: row.parse().unwrap_or(0).max(0),
        }
    }

    pub fn encode(self) -> String {
        format!("{}:{}", self.store, self.row)
    }
}

/// A cheap fingerprint of the file behind a box's store, or zero for none.
///
/// Only for deciding whether a cached handle still points at the file its path
/// names — a handle keeps reading the inode it opened, so a box recreated
/// under the same name leaves the holder reading a deleted database in silence.
/// Not for the cursor: an inode is reusable, and asking for anything more
/// stable from the filesystem lands on `ctime`, which an ordinary SQLite
/// checkpoint moves. The cursor uses [`Store::generation`], which is a
/// property of the database rather than of the file holding it.
pub fn store_file_id(manager: &Arc<SandboxManager>, name: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if !crate::sandbox::state::is_safe_name(name) {
        return 0;
    }
    std::fs::metadata(crate::obs::collector::store_path(&manager.state_dir, name))
        .map(|meta| meta.ino())
        .unwrap_or(0)
}

/// Open a box's event store, if it has one yet.
///
/// Returns `None` rather than an error when no agent has ever connected —
/// "nothing recorded yet" is a state to render, not a failure.
pub fn open_store(manager: &Arc<SandboxManager>, name: &str) -> Result<Option<Store>> {
    // The activity fragments take the box name straight from the URL and never
    // look up a sandbox, so this is where the name is checked. axum
    // percent-decodes a path segment, which makes `..%2f..%2f` an ordinary
    // `../../` by the time it arrives — and `store_path` only joins.
    //
    // `None`, not an error: an unusable name has no store, which is what the
    // caller already knows how to render.
    if !crate::sandbox::state::is_safe_name(name) {
        return Ok(None);
    }
    let path = crate::obs::collector::store_path(&manager.state_dir, name);
    if !path.exists() {
        return Ok(None);
    }
    Store::open(&path).map(Some)
}

/// One row of the live event stream.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamRow {
    pub ts: String,
    pub kind: String,
    /// Colour-coding group: process, network, file, syscall, api, policy.
    pub domain: &'static str,
    pub pid: u32,
    pub comm: String,
    pub summary: String,
}

impl From<&Event> for StreamRow {
    fn from(e: &Event) -> Self {
        Self {
            // Time-of-day only: the date is the same for everything on screen.
            ts: e.ts_wall.get(11..23).unwrap_or(&e.ts_wall).to_string(),
            kind: e.kind.to_string(),
            domain: e.kind.domain(),
            pid: e.pid,
            comm: e.comm.clone(),
            summary: e.summary(),
        }
    }
}

/// One row of the flow table (§7.5).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Flow {
    pub peer: String,
    pub saddr: String,
    /// Source address worth carrying into a *future* packet capture.
    ///
    /// Inbound accept events can report the wildcard listener address here.
    /// `0.0.0.0` and `::` never appear as packet sources, so filtering on one
    /// guarantees an empty capture instead of narrowing it.
    pub capture_saddr: Option<String>,
    pub sport: u16,
    pub addr: String,
    pub port: u16,
    pub proto: String,
    pub sni: String,
    pub alpn: String,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub dur_ms: u64,
    pub pid: u32,
    pub comm: String,
    pub ts: String,
    pub direction: &'static str,
}

impl Flow {
    /// Bytes sent, as a person reads them.
    pub fn tx(&self) -> String {
        human_bytes(self.bytes_tx)
    }

    /// Bytes received, as a person reads them.
    pub fn rx(&self) -> String {
        human_bytes(self.bytes_rx)
    }
}

/// Collapse connect/accept/tls events into one row per flow.
///
/// A connection and its TLS handshake are two events about one thing; showing
/// them as separate rows is exactly the wall-of-events the flow table exists
/// to replace. They are joined on (pid, address, port).
pub fn flows(events: &[Event]) -> Vec<Flow> {
    use std::collections::BTreeMap;

    let mut by_key: BTreeMap<(u32, String, u16, String, u16), Flow> = BTreeMap::new();

    for event in events {
        let Some(net) = &event.net else { continue };
        let direction = match event.kind {
            EventType::Connect => "out",
            EventType::Accept => "in",
            // TLS enriches a flow it does not create.
            EventType::Tls => "out",
            _ => continue,
        };

        // Keyed on the full 4-tuple plus protocol. Dropping the source port
        // merged every sequential connection to the same destination into one
        // row, so its timestamp, duration, and byte counts described several
        // unrelated connections at once. TLS events are matched back to their
        // connect by the same tuple, which is why enrichment still lands.
        let key = (
            event.pid,
            net.proto.clone(),
            net.sport,
            net.daddr.clone(),
            net.dport,
        );
        let flow = by_key.entry(key).or_insert_with(|| Flow {
            peer: event.peer().unwrap_or_else(|| net.daddr.clone()),
            saddr: net.saddr.clone(),
            capture_saddr: (!matches!(net.saddr.as_str(), "" | "0.0.0.0" | "::"))
                .then(|| net.saddr.clone()),
            sport: net.sport,
            addr: net.daddr.clone(),
            port: net.dport,
            proto: net.proto.clone(),
            sni: String::new(),
            alpn: String::new(),
            bytes_tx: 0,
            bytes_rx: 0,
            dur_ms: 0,
            pid: event.pid,
            comm: event.comm.clone(),
            ts: event.ts_wall.clone(),
            direction,
        });

        if !net.sni.is_empty() {
            flow.sni = net.sni.clone();
            // An SNI is a better name than an address, and better than a
            // reverse-mapped guess.
            flow.peer = net.sni.clone();
        }
        if !net.alpn.is_empty() {
            flow.alpn = net.alpn.clone();
        }
        if !net.domain.is_empty() && flow.peer == flow.addr {
            flow.peer = net.domain.clone();
        }
        // Saturating: these are counters an agent sent, and nothing validates
        // them. One connection claiming `u64::MAX` in both directions would
        // otherwise panic a checked build and silently wrap a release one,
        // taking the flow table's sort order with it.
        flow.bytes_tx = flow.bytes_tx.saturating_add(net.bytes_tx);
        flow.bytes_rx = flow.bytes_rx.saturating_add(net.bytes_rx);
        flow.dur_ms = flow.dur_ms.max(net.dur_ms);
        if event.kind != EventType::Tls {
            flow.direction = direction;
        }
    }

    let mut out: Vec<Flow> = by_key.into_values().collect();
    // Busiest first: the flow you want to look at is almost always the one
    // moving the most data.
    out.sort_by(|a, b| {
        // Saturating like the accumulation above it: these totals came from an
        // agent, and sorting is no place to discover that one of them claimed
        // `u64::MAX`.
        let total = |f: &Flow| f.bytes_rx.saturating_add(f.bytes_tx);
        total(b).cmp(&total(a))
    });
    out
}

/// One DNS lookup, for the DNS log.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Lookup {
    pub ts: String,
    pub name: String,
    pub qtype: String,
    pub answers: Vec<String>,
    pub pid: u32,
    pub comm: String,
}

/// Every DNS lookup, newest first.
pub fn lookups(events: &[Event]) -> Vec<Lookup> {
    let mut out: Vec<Lookup> = events
        .iter()
        .filter(|e| e.kind == EventType::Dns)
        .filter_map(|e| {
            let net = e.net.as_ref()?;
            (!net.qname.is_empty()).then(|| Lookup {
                ts: e.ts_wall.get(11..23).unwrap_or(&e.ts_wall).to_string(),
                name: net.qname.clone(),
                qtype: net.qtype.clone(),
                answers: net.answers.clone(),
                pid: e.pid,
                comm: e.comm.clone(),
            })
        })
        .collect();
    out.reverse();
    out
}

/// A process-tree row: a chain plus its indent depth.
#[derive(Debug, Clone, Serialize)]
pub struct TreeRow {
    pub depth: usize,
    pub chain: Chain,
}

/// The process tree, ready to render.
pub fn process_tree(events: &[Event]) -> Vec<TreeRow> {
    let chains = correlate::chains(events);
    correlate::tree(&chains)
        .into_iter()
        .map(|(i, depth)| TreeRow {
            depth,
            chain: chains[i].clone(),
        })
        .collect()
}

// ── layer 1: is capture working at all ───────────────────────────────────

/// One labelled fact in the capture status bar.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Fact {
    pub label: &'static str,
    pub value: String,
}

/// What to do about a capture problem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Remedy {
    /// A command the user can run, when one fixes it.
    pub command: String,
    pub hint: String,
}

/// How the Activity tab reports the state of capture itself.
///
/// The tab used to render one empty state for four unrelated situations: the
/// host collector not running, the box not running, no agent ever connecting,
/// and an agent failing on every attempt. Only the last carries a diagnosis,
/// and it was reachable solely by tailing `logs/collector.log` — so the
/// console's answer to "why is this empty?" was, in practice, silence.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct CaptureView {
    /// `ok`, `warn`, or `down`. Drives the colour and nothing else.
    pub level: &'static str,
    pub headline: String,
    pub detail: String,
    pub remedy: Option<Remedy>,
    pub facts: Vec<Fact>,
    /// Whether more events are expected to arrive on their own.
    pub live: bool,
}

/// Resolve the four situations into one honest status line.
///
/// Deliberately a pure function of what the console already knows: the
/// probes belong to the caller, so every branch here is testable without a
/// runtime, a daemon, or a box.
///
/// `box_status` may be empty, meaning the caller did not probe it — the live
/// pusher deliberately does not, because a runtime probe shells out to a
/// runtime CLI and this runs on a timer.
pub fn capture_view(
    daemon_running: bool,
    box_status: &str,
    health: Option<&CaptureHealth>,
) -> CaptureView {
    // No total row count here. It was one `SELECT COUNT(*)` per status bar,
    // which on a store near its two-million-row retention limit is a full
    // scan — twice a second on the live path. The window's own event count
    // sits directly below the bar and is the number a reader is looking for.
    let mut facts = Vec::new();
    if let Some(health) = health {
        if !health.transport.is_empty() {
            facts.push(Fact {
                label: "transport",
                value: health.transport.clone(),
            });
        }
        // The domains, labelled as domains. This fact used to be called
        // "backends" and carried the same list, which reads as an answer to
        // "what is capturing" — and it is the answer to "what is being
        // captured". The backend composition is in the headline below, where a
        // degraded one can say what it costs.
        if health.state == CaptureState::Streaming && !health.capture.is_empty() {
            facts.push(Fact {
                label: "watching",
                value: health.capture.join(" + "),
            });
        }
        if !health.agent_version.is_empty() {
            facts.push(Fact {
                label: "agent",
                value: health.agent_version.clone(),
            });
        }
        if health.attempts > 1 {
            facts.push(Fact {
                label: "attempts",
                value: health.attempts.to_string(),
            });
        }
    }

    // The host collector outranks everything: when it is not running, no
    // box's record is being updated and every other reading is stale.
    if !daemon_running {
        return CaptureView {
            level: "down",
            headline: "Collector is not running".into(),
            detail: "No host collector owns the capture lock, so nothing is being \
                     recorded for any box. Anything below is the last thing it saw."
                .into(),
            remedy: Some(Remedy {
                command: "devbox doctor".into(),
                hint: "checks the collector daemon and the guest agent together".into(),
            }),
            facts,
            live: false,
        };
    }

    // A box the caller has probed and found *down* cannot be capturing,
    // whatever its last record says. The socket transport's listener is
    // host-side and survives its container, so its record can sit on
    // `streaming` indefinitely after the box stops.
    //
    // `unreachable` and `unknown` are deliberately not in this set. They mean
    // the probe could not decide — a Lima VM that is powered on with a slow
    // guest shell reads `unreachable` while its agent is streaming perfectly
    // well, and reporting "nothing to capture" over a live stream would be a
    // worse answer than saying nothing.
    let box_known_down = matches!(box_status, "stopped" | "missing");

    let Some(health) = health else {
        // No record at all. The box status is the only thing left to say.
        if box_known_down {
            return stopped_view(box_status, facts);
        }
        return CaptureView {
            level: "warn",
            headline: "No agent has connected yet".into(),
            detail: "The collector has not reached this box. Capture starts within \
                     a few seconds of the box running."
                .into(),
            remedy: None,
            facts,
            live: false,
        };
    };

    // A failure keeps its diagnosis even for a box that is now stopped: the
    // agent could not start while it *was* running, and that is still what
    // needs fixing.
    if box_known_down && health.state != CaptureState::Failed {
        return stopped_view(box_status, facts);
    }

    match health.state {
        CaptureState::Streaming => CaptureView {
            level: "ok",
            // The composition the agent actually kept, not a label for the
            // two cases. "proc + packet" was printed for every degraded box
            // whatever it was really running, and `devbox doctor` now prints
            // the same string from the same record.
            headline: format!("Capturing · {}", capture_composition(health)),
            detail: if health.ebpf {
                "Kernel probes are attached: every exec, connection, lookup and \
                 handshake is seen at the syscall boundary."
                    .into()
            } else {
                format!(
                    "No kernel probes here, so capture is degraded: activity is \
                     reconstructed from /proc and captured packets, which means \
                     {DEGRADED_LOSS}, and short-lived processes can be missed."
                )
            },
            remedy: None,
            facts,
            live: true,
        },
        CaptureState::Starting => CaptureView {
            level: "warn",
            headline: "Attaching the agent…".into(),
            detail: if health.detail.is_empty() {
                "The collector is starting the agent inside the box.".into()
            } else {
                // Carried over from the previous attempt, so a flapping agent
                // does not blank its own diagnosis between retries.
                format!("Retrying. The last attempt ended: {}", health.detail)
            },
            remedy: None,
            facts,
            live: false,
        },
        // A record can be older than the probe that just found the box up.
        // "Box is running — nothing to capture" is the one sentence this bar
        // must never produce.
        CaptureState::BoxStopped if box_status == "running" => CaptureView {
            level: "warn",
            headline: "Attaching the agent…".into(),
            detail: "The box is running again and the collector has not reached \
                     it yet. Capture attaches within a few seconds."
                .into(),
            remedy: None,
            facts,
            live: false,
        },
        CaptureState::BoxStopped => stopped_view(box_status, facts),
        CaptureState::Failed => CaptureView {
            level: "down",
            headline: "The agent could not start".into(),
            detail: health.detail.clone(),
            remedy: Some(diagnose(&health.detail)),
            facts,
            live: false,
        },
    }
}

/// The one reading for a box that is not running.
///
/// Reached from three places — a probed status, a `box_stopped` record, and a
/// box with no record at all — which is exactly why it is one function: three
/// wordings for one situation is how a reader learns to distrust the bar.
fn stopped_view(box_status: &str, facts: Vec<Fact>) -> CaptureView {
    CaptureView {
        level: "warn",
        // Only a status that actually means down is quoted. Anything else
        // would put the probe's word into a sentence that contradicts it.
        headline: if matches!(box_status, "stopped" | "missing") {
            format!("Box is {box_status} — nothing to capture")
        } else {
            "Box is not running — nothing to capture".into()
        },
        detail: "Events are captured while the box runs. Start it and capture \
                 attaches on its own."
            .into(),
        remedy: Some(Remedy {
            command: String::new(),
            hint: "start the box from the card above".into(),
        }),
        facts,
        live: false,
    }
}

/// Turn an agent failure into the command that fixes it.
///
/// Matching on the guest's own words rather than on a collector-side error
/// code, because the collector sees one symptom — a connection that closed —
/// for causes as different as a box provisioned before the agent existed and
/// a `sudo` that wants a password.
fn diagnose(detail: &str) -> Remedy {
    let lowered = detail.to_lowercase();
    if lowered.contains("not found") || lowered.contains("no such file") {
        return Remedy {
            command: "devbox reprovision".into(),
            hint: "this box has no devbox-obsd — boxes created before v4 never \
                   had one installed"
                .into(),
        };
    }
    if lowered.contains("sudo") || lowered.contains("permission denied") {
        return Remedy {
            command: "devbox doctor".into(),
            hint: "the agent needs passwordless sudo inside the box".into(),
        };
    }
    if lowered.contains("event layouts are pinned") || lowered.contains("protocol mismatch") {
        return Remedy {
            command: "devbox reprovision".into(),
            hint: "the agent in the box is from a different devbox release".into(),
        };
    }
    Remedy {
        command: "devbox doctor".into(),
        hint: "the full history is in ~/.devbox/logs/collector.log".into(),
    }
}

// ── layer 1: the timeline ────────────────────────────────────────────────

/// How many columns the density strip is drawn with.
pub const TIMELINE_BUCKETS: usize = 60;

/// One column of the activity timeline.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Bucket {
    /// Height as a percentage of the busiest bucket, so the strip is readable
    /// whether the window holds twelve events or twelve thousand.
    pub height: u8,
    pub total: usize,
    /// The domain that dominates this bucket, which colours the column.
    pub domain: &'static str,
    /// Time of day at the start of the bucket.
    pub label: String,
}

fn parse_ts(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Event density across the loaded window, oldest column first.
///
/// A count per column and a dominant domain, not a stacked chart: the strip
/// exists to show *when* something happened so a reader can go look, and a
/// legible shape beats an accurate one at sixty columns wide.
pub fn timeline(events: &[Event]) -> Vec<Bucket> {
    let stamped: Vec<(i64, &'static str)> = events
        .iter()
        .filter_map(|e| parse_ts(&e.ts_wall).map(|ms| (ms, e.kind.domain())))
        .collect();
    let (Some(first), Some(last)) = (
        stamped.iter().map(|(ms, _)| *ms).min(),
        stamped.iter().map(|(ms, _)| *ms).max(),
    ) else {
        return Vec::new();
    };

    // A window with no duration still has to render. One column is honest:
    // everything happened at the same instant, and pretending otherwise would
    // draw a slope out of a single moment.
    let span = (last - first).max(1);
    let columns = if last == first { 1 } else { TIMELINE_BUCKETS };
    let mut counts = vec![std::collections::BTreeMap::<&'static str, usize>::new(); columns];
    for (ms, domain) in &stamped {
        let index = (((ms - first) * columns as i64) / span).clamp(0, columns as i64 - 1) as usize;
        *counts[index].entry(domain).or_default() += 1;
    }

    let totals: Vec<usize> = counts.iter().map(|c| c.values().sum()).collect();
    let peak = totals.iter().copied().max().unwrap_or(0).max(1);
    counts
        .into_iter()
        .zip(totals)
        .enumerate()
        .map(|(index, (domains, total))| Bucket {
            height: ((total * 100) / peak) as u8,
            total,
            domain: domains
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map_or("idle", |(domain, _)| domain),
            label: bucket_label(first + (span * index as i64) / columns as i64),
        })
        .collect()
}

fn bucket_label(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

/// The headline numbers above the timeline.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub events: usize,
    pub peers: usize,
    pub processes: usize,
    /// Connections policy refused. The number this whole product exists for.
    pub blocked: usize,
    pub bytes_tx: String,
    pub bytes_rx: String,
    /// Time of day of the earliest and latest event, or empty for an empty
    /// window. One source for the heading and for both ends of the timeline's
    /// axis, which is the only way those three can be relied on to agree — a
    /// bucket's own label is its *start*, so the last bucket's label is not
    /// the end of the window.
    pub started: String,
    pub ended: String,
}

/// Summarize a window into the counters shown above the timeline.
///
/// `peers` is the rollup's own length rather than a second count of its own.
/// `Event::peer()` reads the network fields, so a destination policy refused
/// before a connection was ever made has none — and the figure disagreed with
/// the tab sitting directly beneath it by exactly the blocked peers, which are
/// the ones a reader is counting.
pub fn totals(events: &[Event], summary: &Summary, peers: &[Peer]) -> Totals {
    let mut processes = std::collections::BTreeSet::new();
    for event in events {
        if !event.comm.is_empty() {
            processes.insert(event.comm.clone());
        }
    }
    let (started, ended) = window_ends(events);
    Totals {
        events: events.len(),
        peers: peers.len(),
        processes: processes.len(),
        blocked: summary.violations.len(),
        bytes_tx: human_bytes(summary.bytes_tx),
        bytes_rx: human_bytes(summary.bytes_rx),
        // Earliest and latest by clock, not by row order. They coincide for
        // a live agent, which streams in time order — but a store that was
        // written out of order (a replayed capture, a backfill) would label
        // the window with whichever event happened to be inserted last, and
        // disagree with the timeline drawn directly beneath it.
        started,
        ended,
    }
}

/// Time of day of the earliest and latest event in a window.
///
/// Ordered by instant, not by the text of the timestamp. `ts_wall` is stored
/// as the agent wrote it, so a window spanning two offsets — `23:00+02:00` is
/// an hour *before* `22:00Z` — sorts one way as text and the other way in
/// time, and the heading would contradict the timeline drawn from the same
/// events by [`timeline`], which parses.
fn window_ends(events: &[Event]) -> (String, String) {
    let mut stamps: Vec<(i64, &str)> = events
        .iter()
        .filter_map(|e| parse_ts(&e.ts_wall).map(|ms| (ms, e.ts_wall.as_str())))
        .collect();
    stamps.sort_unstable_by_key(|(ms, _)| *ms);
    // Rendered from the parsed instant, so both ends of the heading are in the
    // same clock as the timeline's bucket labels beneath them.
    match (stamps.first(), stamps.last()) {
        (Some((first, _)), Some((last, _))) => (bucket_label(*first), bucket_label(*last)),
        _ => (String::new(), String::new()),
    }
}

/// Bytes as a person reads them.
///
/// `831720` is a number the reader has to decode; `812 KB` is one they can
/// compare at a glance, which is the only thing this column is ever used for.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

// ── layer 2: rollups ─────────────────────────────────────────────────────

/// Every flow to one peer, collapsed into one row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Peer {
    pub peer: String,
    pub ports: String,
    pub connections: usize,
    pub bytes_tx: String,
    pub bytes_rx: String,
    /// Sort key, so the template does not have to parse the rendered sizes.
    pub bytes_total: u64,
    pub processes: String,
    /// `allowed` or `blocked`.
    pub verdict: &'static str,
}

/// Collapse the flow table by peer, busiest first.
///
/// The flow table answers "which connection was that?"; this answers "who is
/// this box talking to?", which is the question someone opening the tab is
/// almost always asking first. Twenty sequential fetches from one host are
/// one relationship, not twenty.
///
/// Refused peers are included even though they produced no flow. A connection
/// policy blocked never reaches `connect`, so a table built only from flows
/// silently omits exactly the destinations someone came to this view to find —
/// it would answer "who is this box talking to?" with the half that succeeded.
pub fn peers(flows: &[Flow], summary: &Summary) -> Vec<Peer> {
    use std::collections::BTreeMap;

    let blocked: std::collections::BTreeSet<&str> = summary
        .violations
        .iter()
        .map(|v| v.target.as_str())
        .collect();

    #[derive(Default)]
    struct Accumulator {
        ports: Vec<u16>,
        connections: usize,
        tx: u64,
        rx: u64,
        processes: Vec<String>,
    }

    let mut by_peer: BTreeMap<String, Accumulator> = BTreeMap::new();
    for flow in flows {
        let entry = by_peer.entry(flow.peer.clone()).or_default();
        if !entry.ports.contains(&flow.port) {
            entry.ports.push(flow.port);
        }
        entry.connections += 1;
        entry.tx = entry.tx.saturating_add(flow.bytes_tx);
        entry.rx = entry.rx.saturating_add(flow.bytes_rx);
        if !entry.processes.contains(&flow.comm) {
            entry.processes.push(flow.comm.clone());
        }
    }

    for violation in &summary.violations {
        // Only ones with no flow of their own; a peer that was blocked *and*
        // connected keeps its real byte counts.
        by_peer.entry(violation.target.clone()).or_default();
    }

    let mut out: Vec<Peer> = by_peer
        .into_iter()
        .map(|(peer, mut acc)| {
            acc.ports.sort_unstable();
            Peer {
                verdict: if blocked.contains(peer.as_str()) {
                    "blocked"
                } else {
                    "allowed"
                },
                ports: acc
                    .ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                connections: acc.connections,
                bytes_tx: human_bytes(acc.tx),
                bytes_rx: human_bytes(acc.rx),
                bytes_total: acc.tx.saturating_add(acc.rx),
                processes: acc.processes.join(", "),
                peer,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        // Refused first. A blocked peer has no bytes to earn a position with,
        // and it is the row a reader is looking for.
        (a.verdict == "allowed")
            .cmp(&(b.verdict == "allowed"))
            .then_with(|| b.bytes_total.cmp(&a.bytes_total))
            .then_with(|| a.peer.cmp(&b.peer))
    });
    out
}

/// One file the box wrote.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileWrite {
    pub ts: String,
    pub path: String,
    pub op: String,
    pub pid: u32,
    pub comm: String,
}

/// Every file event, newest first.
///
/// `Summary` already collects the set of paths, but a set cannot say who
/// wrote one or when — and "which process touched this file" is the question
/// the set makes it impossible to answer.
pub fn file_writes(events: &[Event]) -> Vec<FileWrite> {
    let mut out: Vec<FileWrite> = events
        .iter()
        .filter(|e| e.kind == EventType::File)
        .filter_map(|e| {
            let file = e.file.as_ref()?;
            (!file.path.is_empty()).then(|| FileWrite {
                ts: e.ts_wall.get(11..23).unwrap_or(&e.ts_wall).to_string(),
                path: file.path.clone(),
                op: file.op.clone(),
                pid: e.pid,
                comm: e.comm.clone(),
            })
        })
        .collect();
    out.reverse();
    out
}

/// One connection policy refused, as the console shows it.
///
/// A view type rather than [`Violation`] itself, because the timestamp is
/// rendered: every other table on this tab shows time of day, and the export
/// formats still need the whole instant. Truncating the shared struct would
/// make the JSON export quietly lose its date.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Refusal {
    pub ts: String,
    pub target: String,
    pub verdict: String,
    pub reason: String,
}

/// Connections policy refused, newest first.
///
/// Promoted out of the behaviour summary's text dump: a blocked egress is the
/// single most consequential thing this console can show, and it was reachable
/// only by opening a collapsed `<details>` and reading a `<pre>`.
pub fn violations(summary: &Summary) -> Vec<Refusal> {
    let mut out = summary.violations.clone();
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out.into_iter()
        .map(|v| Refusal {
            ts: v.ts.get(11..23).unwrap_or(&v.ts).to_string(),
            target: v.target,
            verdict: v.verdict,
            reason: v.reason,
        })
        .collect()
}

/// One domain toggle above the stream, with how much it would show.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DomainChip {
    pub name: &'static str,
    pub count: usize,
    pub on: bool,
}

/// The domain toggles for a window.
///
/// Counted over the whole window rather than the filtered stream, so a chip
/// says how much turning it on would reveal — a count that shrank to match
/// the current selection would make every chip read zero.
///
/// Every domain always gets a chip, including the ones at zero. Two reasons,
/// and the second is the load-bearing one: a domain that only starts appearing
/// after the page loaded would otherwise have no control at all, and — because
/// the chip set is then fixed — only the *counts* need to travel when the
/// window is re-read, so a refresh can never restore a selection the reader
/// has since changed.
pub fn domain_chips(events: &[Event], filter: &Filter) -> Vec<DomainChip> {
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for event in events {
        *counts.entry(event.kind.domain()).or_default() += 1;
    }
    // Fixed order, so a chip does not move under the pointer when a burst of
    // one kind of activity overtakes another.
    const ORDER: [&str; 6] = ["process", "network", "file", "syscall", "api", "policy"];
    ORDER
        .iter()
        .map(|name| DomainChip {
            name,
            count: counts.get(name).copied().unwrap_or(0),
            on: filter.domains.iter().any(|d| d == name),
        })
        .collect()
}

// ── layer 3: filtering the stream ────────────────────────────────────────

/// What the reader has narrowed the stream to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Domains to keep. Empty means every domain.
    pub domains: Vec<String>,
    /// Free text, matched against process, summary, peer and path.
    ///
    /// Kept in the reader's own case, because it goes back into the search box.
    pub query: String,
    /// `query`, lowercased and trimmed once.
    ///
    /// [`Filter::matches`] runs per event, and normalizing the needle inside it
    /// made the work scale with the needle's length times the row limit — a
    /// query string is attacker-supplied and was not bounded. Set by
    /// [`Filter::from_query`] and [`Filter::searching`]; a `Filter` built by
    /// struct literal searches for nothing, which is what `Default` means.
    needle: String,
    pub pid: Option<u32>,
}

impl Filter {
    /// Read a filter out of a request's query string.
    ///
    /// Repeated `domain` keys accumulate, which `serde_urlencoded` (and so
    /// axum's `Query`) cannot express — a checkbox group posts exactly that
    /// shape, so it is parsed here rather than reshaped into something the
    /// form would have to work around.
    pub fn from_query(query: &str) -> Self {
        let mut filter = Filter::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "domain" => {
                    let value = value.into_owned();
                    // Only real domains, so a crafted value cannot silently
                    // filter everything out and look like an empty box.
                    if EventType::ALL.iter().any(|k| k.domain() == value) {
                        filter.domains.push(value);
                    }
                }
                "q" => {
                    // Bounded before it is stored. Nothing legible is longer,
                    // and an unbounded one is carried into every haystack
                    // comparison for every row of every refresh.
                    //
                    // By characters, not by bytes: `String::truncate` panics
                    // on an index that is not a character boundary, and this
                    // value comes out of a query string — so 255 bytes of
                    // ASCII followed by one emoji was a panic away.
                    filter.query = value.chars().take(MAX_QUERY_CHARS).collect();
                }
                "pid" => filter.pid = value.parse().ok(),
                _ => {}
            }
        }
        filter.domains.sort();
        filter.domains.dedup();
        filter.needle = filter.query.trim().to_lowercase();
        filter
    }

    /// A filter over free text alone. For tests and for callers that have a
    /// needle rather than a query string.
    pub fn searching(query: &str) -> Self {
        Self {
            query: query.to_string(),
            needle: query.trim().to_lowercase(),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty() && self.needle.is_empty() && self.pid.is_none()
    }

    /// Whether an event survives the filter.
    ///
    /// Runs against correlated events, so `pypi.org` matches the connection
    /// that only ever recorded `151.101.0.223`.
    pub fn matches(&self, event: &Event) -> bool {
        if !self.domains.is_empty() && !self.domains.iter().any(|d| d == event.kind.domain()) {
            return false;
        }
        if self.pid.is_some_and(|pid| event.pid != pid) {
            return false;
        }
        if self.needle.is_empty() {
            return true;
        }
        let haystacks = [
            event.comm.to_lowercase(),
            event.summary().to_lowercase(),
            event.kind.to_string(),
            event.peer().unwrap_or_default().to_lowercase(),
            event.path().unwrap_or_default().to_lowercase(),
        ];
        haystacks.iter().any(|h| h.contains(&self.needle))
    }
}

/// Everything the Activity tab needs, loaded in one pass.
pub struct Activity {
    pub events: Vec<Event>,
    pub stream: Vec<StreamRow>,
    pub timeline: Vec<Bucket>,
    pub totals: Totals,
    pub flows: Vec<Flow>,
    pub peers: Vec<Peer>,
    pub lookups: Vec<Lookup>,
    pub tree: Vec<TreeRow>,
    pub files: Vec<FileWrite>,
    pub violations: Vec<Refusal>,
    pub summary: Summary,
    /// Where a live tail resumes, as the page will send it back.
    pub cursor: String,
    /// False when no agent has ever connected for this box.
    pub has_store: bool,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            stream: Vec::new(),
            timeline: Vec::new(),
            totals: Totals::default(),
            flows: Vec::new(),
            peers: Vec::new(),
            lookups: Vec::new(),
            tree: Vec::new(),
            files: Vec::new(),
            violations: Vec::new(),
            summary: Summary::default(),
            // A real cursor rather than an empty string: it goes into the page
            // and comes back, and "the beginning of no store" is a position
            // the tail already knows how to answer.
            cursor: Cursor::default().encode(),
            has_store: false,
        }
    }
}

/// Load and derive every activity view for a box.
pub fn load(
    manager: &Arc<SandboxManager>,
    name: &str,
    limit: usize,
    filter: &Filter,
) -> Result<Activity> {
    let Some(store) = open_store(manager, name)? else {
        return Ok(Activity::default());
    };
    // From the handle the rows come from. This page leaves with a cursor
    // stamped with it, and a cursor naming a store its rows did not come from
    // is a position that looks valid and is not.
    let identity = store.generation()?;

    // The store's own high-water mark, not the newest row that decoded. A row
    // the console cannot read is still a row the tail must start after, or it
    // is re-delivered and re-discarded on every request forever.
    let anchor = store.max_id()?;

    // Newest-first so the limit keeps recent events, then reversed so the
    // views read forwards. Bounded by the mark this page's cursor will carry:
    // a row committed between reading the mark and running the query would
    // otherwise be rendered here *and* delivered again by the first tail.
    let mut events = store.query(&Query {
        before_id: Some(anchor),
        limit: Some(limit),
        max_bytes: Some(MAX_RENDERED_EVENT_BYTES),
        newest_first: true,
        ..Default::default()
    })?;
    events.reverse();

    let map = correlate::dns_map(&events);
    correlate::apply_dns_map(&mut events, &map);

    // Everything above the stream describes the whole window; only the stream
    // is narrowed. A filter that also emptied the timeline and the totals
    // would remove the very context that makes a narrowed stream readable.
    let stream = events
        .iter()
        .rev()
        .filter(|event| filter.matches(event))
        .map(StreamRow::from)
        .collect();

    let summary = behavior::summarize(name, &events);
    let flows = flows(&events);
    let peers = peers(&flows, &summary);
    Ok(Activity {
        stream,
        timeline: timeline(&events),
        totals: totals(&events, &summary, &peers),
        peers,
        flows,
        lookups: lookups(&events),
        tree: process_tree(&events),
        files: file_writes(&events),
        violations: violations(&summary),
        summary,
        events,
        cursor: Cursor {
            store: identity,
            row: anchor,
        }
        .encode(),
        has_store: true,
    })
}

/// One page of a live tail.
pub struct TailPage {
    pub rows: Vec<StreamRow>,
    /// Where the next request resumes.
    pub cursor: String,
    /// Events the page jumped over because a burst outran it.
    pub skipped: u64,
    /// The page is holding a position in a store that no longer exists, so
    /// what it has rendered describes a different box. It must be replaced,
    /// not appended to.
    pub reset: bool,
}

/// Events stored since `cursor`, ready to append to a live stream.
///
/// Correlation needs context the new rows do not carry on their own: a
/// `connect` arriving now is named by a `dns` event that may have been stored
/// minutes ago. So the map is built over a trailing window and applied to the
/// fresh rows, which is what keeps a live row saying `pypi.org` rather than
/// decaying to a bare address the moment it stops being the first paint.
pub fn tail(
    manager: &Arc<SandboxManager>,
    name: &str,
    cursor: Cursor,
    filter: &Filter,
) -> Result<TailPage> {
    let Some(store) = open_store(manager, name)? else {
        // No store at all. A cursor that named one must still tell the page to
        // drop what it is showing: those rows describe a box that is gone.
        return Ok(TailPage {
            rows: Vec::new(),
            cursor: Cursor::default().encode(),
            skipped: 0,
            reset: cursor.store != 0,
        });
    };
    // Read through the same handle the rows come from, so there is no window
    // in which one store's events could be paired with another's identity.
    let identity = store.generation()?;
    // One high-water mark for the whole request. Everything below — the
    // backlog decision, the page, and the cursor the page leaves with —
    // describes the store as it was at this instant, so a batch committed
    // mid-request cannot make any two of them disagree.
    let newest = store.max_id()?;

    // A different store, or a position ahead of this one, means the page is
    // holding a cursor from a box that no longer exists.
    let reset = cursor.store != identity || cursor.row > newest;
    let anchor = if reset { 0 } else { cursor.row };

    let next = Cursor {
        store: identity,
        row: newest,
    }
    .encode();
    if newest <= anchor {
        // Carries `reset` too: a replacement store with nothing in it yet is
        // exactly the case where the page is still showing the previous box.
        return Ok(TailPage {
            rows: Vec::new(),
            cursor: next,
            skipped: 0,
            reset,
        });
    }

    // The id span, not a row count.
    //
    // Ids are consecutive except where retention has deleted rows — and those
    // rows are events the reader missed too. Counting surviving rows reported
    // a tab suspended across three million arrivals as having skipped the two
    // million still on disk, quietly dropping the million already evicted from
    // the number. The span counts both, and it costs no query.
    let backlog = (newest - anchor) as u64;

    let (mut fresh, skipped) = if backlog > TAIL_LIMIT as u64 {
        // A live stream must not fall behind reality. Delivering the *oldest*
        // page of a ten-thousand-event burst and waiting for the next request
        // makes the stream lag by minutes; a tail shows the end of the file.
        //
        let mut newest_first = store.query(&Query {
            after_id: Some(anchor),
            before_id: Some(newest),
            limit: Some(TAIL_LIMIT),
            max_bytes: Some(MAX_RENDERED_EVENT_BYTES),
            newest_first: true,
            ..Default::default()
        })?;
        newest_first.reverse();
        // A reset reports no skip: the page has never seen any of this store's
        // events, so there is nothing it is missing.
        let skipped = if reset {
            0
        } else {
            backlog.saturating_sub(newest_first.len() as u64)
        };
        (newest_first, skipped)
    } else {
        let (events, _) = store.tail_scan(anchor, newest, TAIL_LIMIT, MAX_RENDERED_EVENT_BYTES)?;
        (events, 0)
    };

    let context = store.query(&Query {
        limit: Some(INITIAL_EVENTS),
        max_bytes: Some(MAX_RENDERED_EVENT_BYTES),
        newest_first: true,
        ..Default::default()
    })?;
    let map = correlate::dns_map(&context);
    correlate::apply_dns_map(&mut fresh, &map);

    Ok(TailPage {
        rows: fresh
            .iter()
            .filter(|event| filter.matches(event))
            .map(StreamRow::from)
            .collect(),
        cursor: next,
        skipped,
        reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::behavior::Violation;
    use crate::obs::event::{Exec, Net};

    fn base(pid: u32, mono: u64, kind: EventType) -> Event {
        Event {
            ts_wall: format!("2026-08-06T22:14:{:02}.412Z", mono / 1_000_000_000),
            ts_mono_ns: mono,
            box_id: "myapp".into(),
            cgroup_id: 1,
            pid,
            tid: pid,
            ppid: 640,
            comm: "pip".into(),
            uid: 1000,
            kind,
            net: None,
            exec: None,
            file: None,
            api: None,
            policy: None,
            credential: None,
        }
    }

    fn connect(pid: u32, mono: u64, addr: &str, port: u16, tx: u64, rx: u64) -> Event {
        let mut e = base(pid, mono, EventType::Connect);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: addr.into(),
            dport: port,
            bytes_tx: tx,
            bytes_rx: rx,
            dur_ms: 690,
            ..Default::default()
        });
        e
    }

    #[test]
    fn stream_rows_show_time_of_day_and_a_colour_group() {
        let mut e = base(812, 1_000_000_000, EventType::Exec);
        e.exec = Some(Exec {
            path: "/bin/pip".into(),
            argv: vec!["pip".into(), "install".into()],
            ..Default::default()
        });

        let row = StreamRow::from(&e);
        assert_eq!(row.ts, "22:14:01.412", "the date is redundant on screen");
        assert_eq!(row.domain, "process");
        assert_eq!(row.pid, 812);
        assert!(row.summary.contains("pip install"));
    }

    #[test]
    fn a_connection_and_its_tls_handshake_are_one_flow() {
        let mut tls = base(812, 2_000_000_000, EventType::Tls);
        tls.net = Some(Net {
            proto: "tcp".into(),
            daddr: "151.101.0.223".into(),
            dport: 443,
            sni: "pypi.org".into(),
            alpn: "h2".into(),
            ..Default::default()
        });

        let rows = flows(&[
            connect(812, 1_000_000_000, "151.101.0.223", 443, 4102, 831_720),
            tls,
        ]);

        assert_eq!(rows.len(), 1, "two events about one connection is one row");
        let f = &rows[0];
        assert_eq!(f.peer, "pypi.org", "the SNI names the flow");
        assert_eq!(f.addr, "151.101.0.223");
        assert_eq!(f.alpn, "h2");
        assert_eq!(f.bytes_tx, 4102);
        assert_eq!(f.bytes_rx, 831_720);
        assert_eq!(f.direction, "out");
    }

    #[test]
    fn flows_are_sorted_by_traffic() {
        let rows = flows(&[
            connect(812, 1_000_000_000, "10.0.0.1", 80, 1, 1),
            connect(812, 2_000_000_000, "10.0.0.2", 80, 1000, 9000),
            connect(812, 3_000_000_000, "10.0.0.3", 80, 50, 50),
        ]);
        assert_eq!(rows[0].addr, "10.0.0.2");
        assert_eq!(rows[2].addr, "10.0.0.1");
    }

    #[test]
    fn different_processes_to_the_same_peer_are_different_flows() {
        let rows = flows(&[
            connect(812, 1_000_000_000, "10.0.0.1", 443, 1, 1),
            connect(900, 2_000_000_000, "10.0.0.1", 443, 1, 1),
        ]);
        assert_eq!(rows.len(), 2, "a flow belongs to a process");
    }

    #[test]
    fn accepts_are_marked_inbound() {
        let mut e = base(900, 1_000_000_000, EventType::Accept);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: "10.0.0.9".into(),
            dport: 51999,
            ..Default::default()
        });
        let flow = &flows(&[e])[0];
        assert_eq!(flow.direction, "in");
        assert_eq!(flow.capture_saddr, None);
    }

    #[test]
    fn dns_log_is_newest_first_and_carries_answers() {
        let mut a = base(812, 1_000_000_000, EventType::Dns);
        a.net = Some(Net {
            qname: "pypi.org".into(),
            qtype: "A".into(),
            answers: vec!["151.101.0.223".into()],
            ..Default::default()
        });
        let mut b = base(812, 2_000_000_000, EventType::Dns);
        b.net = Some(Net {
            qname: "files.pythonhosted.org".into(),
            qtype: "A".into(),
            answers: vec![],
            ..Default::default()
        });

        let log = lookups(&[a, b]);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].name, "files.pythonhosted.org", "newest first");
        assert_eq!(log[1].answers, vec!["151.101.0.223"]);
    }

    #[test]
    fn non_dns_events_do_not_appear_in_the_dns_log() {
        assert!(lookups(&[connect(1, 1, "10.0.0.1", 80, 0, 0)]).is_empty());
    }

    #[test]
    fn process_tree_nests_children_under_parents() {
        let mut parent = base(640, 1_000_000_000, EventType::Exec);
        parent.ppid = 1;
        parent.comm = "zsh".into();
        parent.exec = Some(Exec {
            path: "/bin/zsh".into(),
            ..Default::default()
        });

        let mut child = base(812, 2_000_000_000, EventType::Exec);
        child.ppid = 640;
        child.exec = Some(Exec {
            path: "/bin/pip".into(),
            ..Default::default()
        });

        let rows = process_tree(&[parent, child]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].chain.pid, 812);
    }

    #[test]
    fn every_view_is_empty_for_an_empty_window() {
        assert!(flows(&[]).is_empty());
        assert!(lookups(&[]).is_empty());
        assert!(process_tree(&[]).is_empty());
        assert!(timeline(&[]).is_empty());
        assert!(file_writes(&[]).is_empty());
    }

    // ── capture health ───────────────────────────────────────────────

    fn health(state: CaptureState) -> CaptureHealth {
        CaptureHealth::new("alpha", state).with_transport("exec")
    }

    #[test]
    fn a_dead_collector_outranks_every_other_reading() {
        // Its record is whatever it managed to write before it stopped, so
        // reporting "capturing" off a stale file would be the one answer that
        // sends someone looking in the wrong place.
        let record = health(CaptureState::Streaming);
        let view = capture_view(false, "running", Some(&record));
        assert_eq!(view.level, "down");
        assert!(view.headline.contains("Collector is not running"));
        assert!(!view.live);
    }

    #[test]
    fn a_stopped_box_is_reported_as_stopped_not_as_missing_data() {
        let record = health(CaptureState::BoxStopped);
        let view = capture_view(true, "stopped", Some(&record));
        assert_eq!(view.level, "warn");
        assert_eq!(view.headline, "Box is stopped — nothing to capture");
        assert!(view.remedy.is_some(), "an empty tab must say what to do");

        // Reached three ways, worded once.
        assert_eq!(
            capture_view(true, "stopped", None).headline,
            view.headline,
            "a box with no record at all reads the same"
        );
        assert_eq!(
            capture_view(true, "", Some(&record)).headline,
            "Box is not running — nothing to capture",
            "and so does one whose status was not probed"
        );
    }

    #[test]
    fn an_undecided_probe_does_not_contradict_a_live_stream() {
        // `unreachable` means the guest shell did not answer in time, which a
        // powered-on Lima VM does under load while its agent streams happily.
        let mut record = health(CaptureState::Streaming);
        record.ebpf = true;
        for status in ["unreachable", "unknown", ""] {
            let view = capture_view(true, status, Some(&record));
            assert_eq!(view.level, "ok", "status {status:?}");
            assert!(view.live, "status {status:?}");
        }
    }

    #[test]
    fn a_stale_stopped_record_never_says_a_running_box_is_stopped() {
        // The probe is newer than the record for the few seconds between
        // starting a box and the collector reaching it.
        let record = health(CaptureState::BoxStopped);
        let view = capture_view(true, "running", Some(&record));
        assert!(view.headline.contains("Attaching"), "{}", view.headline);
        assert!(!view.headline.contains("running — nothing"));
    }

    #[test]
    fn a_stopped_box_is_never_reported_as_capturing() {
        // The host-side socket listener outlives the container that was
        // dialling it, so its record can sit on `streaming` after the box
        // stops. A probed status is the more recent fact and wins.
        let mut record = health(CaptureState::Streaming);
        record.ebpf = true;
        let view = capture_view(true, "stopped", Some(&record));
        assert_eq!(view.level, "warn");
        assert!(!view.live);
        assert!(view.headline.contains("stopped"), "{}", view.headline);

        // …but a failure still names what failed, because that is what needs
        // fixing before the box is worth starting again.
        let failed = health(CaptureState::Failed).with_detail("devbox-obsd: not found");
        assert!(
            capture_view(true, "stopped", Some(&failed))
                .detail
                .contains("not found")
        );
    }

    #[test]
    fn streaming_says_which_backends_actually_attached() {
        let mut record = health(CaptureState::Streaming);
        record.ebpf = true;
        record.source = "ebpf+packet+netfilter".into();
        record.capture = vec!["exec".into(), "connect".into(), "dns".into()];
        let view = capture_view(true, "running", Some(&record));
        assert_eq!(view.level, "ok");
        assert_eq!(view.headline, "Capturing · ebpf+packet+netfilter");
        assert!(view.live);
        // What is capturing and what is being captured are different facts,
        // and the bar used to print the second under the first one's label.
        assert!(
            view.facts
                .iter()
                .any(|f| f.label == "watching" && f.value == "exec + connect + dns"),
            "the reader cannot infer coverage from anywhere else"
        );

        // Proc+packet is a materially weaker guarantee and has to say so:
        // a short-lived process can be missed entirely.
        let mut degraded = health(CaptureState::Streaming);
        degraded.ebpf = false;
        degraded.source = "proc+packet".into();
        let view = capture_view(true, "running", Some(&degraded));
        assert_eq!(view.headline, "Capturing · proc+packet");
        assert!(view.detail.contains("missed"));
        assert!(
            view.detail.contains("no process attribution"),
            "a degraded bar has to name what it cannot see: {}",
            view.detail
        );
    }

    #[test]
    fn the_console_and_doctor_name_the_same_composition() {
        // Two readouts of one record. A reader who checks both should not have
        // to work out whether "eBPF" and "ebpf+packet" are the same box.
        // `doctor` puts composition and verdict on one line; the bar splits
        // them across headline and detail, and between them says the same.
        for (ebpf, source) in [(true, "ebpf+packet"), (false, "proc+packet")] {
            let mut record = health(CaptureState::Streaming);
            record.ebpf = ebpf;
            record.source = source.into();
            let view = capture_view(true, "running", Some(&record));
            let doctor = crate::obs::health::capture_source(&record);

            assert!(view.headline.ends_with(source), "{}", view.headline);
            for word in doctor.split_whitespace() {
                let word = word.trim_matches(|c: char| !c.is_alphanumeric());
                assert!(
                    view.headline.contains(word) || view.detail.contains(word),
                    "the bar drops {word:?} from doctor's {doctor:?}"
                );
            }
        }
    }

    #[test]
    fn a_missing_agent_is_told_apart_from_a_missing_sudo() {
        let missing = health(CaptureState::Failed)
            .with_detail("agent said: sh: /usr/local/bin/devbox-obsd: not found");
        let view = capture_view(true, "running", Some(&missing));
        assert_eq!(view.level, "down");
        assert_eq!(view.remedy.unwrap().command, "devbox reprovision");

        let denied = health(CaptureState::Failed).with_detail("sudo: a password is required");
        let view = capture_view(true, "running", Some(&denied));
        assert!(view.remedy.unwrap().hint.contains("sudo"));

        let drifted =
            health(CaptureState::Failed).with_detail("event layouts are pinned per release");
        assert_eq!(
            capture_view(true, "running", Some(&drifted))
                .remedy
                .unwrap()
                .command,
            "devbox reprovision"
        );
    }

    #[test]
    fn a_retry_keeps_showing_the_previous_diagnosis() {
        // Otherwise a flapping agent blanks its own explanation every few
        // seconds, and the reason is only ever visible to whoever happens to
        // be looking during the failed half of the cycle.
        let retrying = health(CaptureState::Starting)
            .with_detail("agent closed the connection before saying hello")
            .with_attempts(4);
        let view = capture_view(true, "running", Some(&retrying));
        assert!(view.detail.contains("before saying hello"));
        assert!(view.facts.iter().any(|f| f.label == "attempts"));
    }

    #[test]
    fn a_box_with_no_record_still_gets_a_useful_line() {
        let running = capture_view(true, "running", None);
        assert!(running.headline.contains("No agent has connected"));
        let stopped = capture_view(true, "stopped", None);
        assert!(stopped.headline.contains("stopped"));
    }

    // ── timeline ─────────────────────────────────────────────────────

    fn at(seconds: u64, kind: EventType) -> Event {
        let mut e = base(1, seconds * 1_000_000_000, kind);
        e.ts_wall = format!("2026-08-06T22:14:{seconds:02}.000Z");
        if kind == EventType::Connect {
            e.net = Some(Net {
                proto: "tcp".into(),
                daddr: "10.0.0.1".into(),
                dport: 443,
                ..Default::default()
            });
        }
        e
    }

    #[test]
    fn the_timeline_scales_to_its_busiest_column() {
        let mut events: Vec<Event> = (0..10).map(|s| at(s, EventType::Exec)).collect();
        // A burst at the end, so one column dominates.
        events.extend((0..40).map(|_| at(59, EventType::Connect)));

        let strip = timeline(&events);
        assert_eq!(strip.len(), TIMELINE_BUCKETS);
        assert_eq!(
            strip.last().unwrap().height,
            100,
            "the busiest column is full height whatever the absolute count"
        );
        assert_eq!(strip.last().unwrap().domain, "network");
        assert!(strip.first().unwrap().label.starts_with("22:14"));
    }

    #[test]
    fn the_window_label_agrees_with_the_timeline_beneath_it() {
        // Inserted newest-first, so row order and clock order disagree — the
        // shape a replayed or backfilled store has.
        let events = vec![
            at(17, EventType::Exec),
            at(1, EventType::Exec),
            at(9, EventType::Exec),
        ];
        let totals = totals(&events, &Summary::default(), &[]);
        assert_eq!(totals.started, "22:14:01");
        assert_eq!(totals.ended, "22:14:17");

        // A bucket label is the bucket's *start*, so the last one is before
        // the window's end. The axis is drawn from the window, not from the
        // buckets, which is the whole reason these are separate fields.
        let strip = timeline(&events);
        assert_eq!(strip.first().unwrap().label, totals.started);
        assert_ne!(strip.last().unwrap().label, totals.ended);
    }

    #[test]
    fn a_window_with_no_duration_draws_one_column_not_a_slope() {
        let events: Vec<Event> = (0..5).map(|_| at(7, EventType::Exec)).collect();
        let strip = timeline(&events);
        assert_eq!(strip.len(), 1, "one instant is one column");
        assert_eq!(strip[0].total, 5);
    }

    #[test]
    fn events_with_unparseable_timestamps_do_not_break_the_strip() {
        let mut broken = at(1, EventType::Exec);
        broken.ts_wall = "not a timestamp".into();
        assert!(timeline(&[broken]).is_empty());
    }

    // ── rollups and sizes ────────────────────────────────────────────

    #[test]
    fn sizes_are_rendered_for_comparison_not_for_arithmetic() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(831_720), "812 KB");
        assert_eq!(human_bytes(5_368_709_120), "5.0 GB");
    }

    #[test]
    fn peers_collapse_repeated_connections_into_one_relationship() {
        // Distinct source ports, which is what makes the flow table treat two
        // sequential fetches from one host as two connections.
        let mut first = connect(812, 1_000_000_000, "151.101.0.223", 443, 100, 900);
        first.net.as_mut().unwrap().sport = 42001;
        let mut second = connect(812, 2_000_000_000, "151.101.0.223", 443, 100, 900);
        second.net.as_mut().unwrap().sport = 42002;

        let rows = flows(&[
            first,
            second,
            connect(900, 3_000_000_000, "10.0.0.9", 80, 1, 1),
        ]);
        assert_eq!(rows.len(), 3);

        let rolled = peers(&rows, &Summary::default());
        assert_eq!(rolled.len(), 2, "twenty fetches from one host is one host");
        assert_eq!(rolled[0].connections, 2, "busiest first");
        assert_eq!(rolled[0].bytes_total, 2000);
        assert_eq!(rolled[0].verdict, "allowed");
    }

    #[test]
    fn a_refused_peer_is_marked_in_the_rollup() {
        let summary = Summary {
            violations: vec![Violation {
                target: "api.openai.com".into(),
                verdict: "blocked".into(),
                reason: "not in the allow list".into(),
                ts: "2026-08-06T22:14:09.000Z".into(),
            }],
            ..Default::default()
        };
        let mut flow = connect(812, 1_000_000_000, "1.2.3.4", 443, 0, 0);
        flow.net.as_mut().unwrap().sni = "api.openai.com".into();

        let rolled = peers(&flows(&[flow]), &summary);
        assert_eq!(rolled[0].peer, "api.openai.com");
        assert_eq!(rolled[0].verdict, "blocked");
    }

    #[test]
    fn the_peer_count_agrees_with_the_peers_view() {
        // They disagreed by exactly the blocked peers, which is the subset a
        // reader is most likely to be counting.
        let summary = Summary {
            violations: vec![Violation {
                target: "evil.example.net".into(),
                verdict: "blocked".into(),
                reason: String::new(),
                ts: "2026-08-06T22:14:09.000Z".into(),
            }],
            ..Default::default()
        };
        let events = vec![connect(812, 1, "10.0.0.9", 443, 1, 1)];
        let rolled = peers(&flows(&events), &summary);

        assert_eq!(rolled.len(), 2);
        assert_eq!(totals(&events, &summary, &rolled).peers, rolled.len());
    }

    #[test]
    fn a_peer_that_was_refused_before_connecting_still_appears() {
        // The case a flow-only table cannot show: policy blocked the
        // connection, so there is no connect event and no flow — and the
        // destination someone opened this view to find would be missing.
        let summary = Summary {
            violations: vec![Violation {
                target: "api.openai.com".into(),
                verdict: "blocked".into(),
                reason: "not in the allow list".into(),
                ts: "2026-08-06T22:14:09.000Z".into(),
            }],
            ..Default::default()
        };
        let allowed = flows(&[connect(812, 1, "10.0.0.9", 443, 1_000, 900_000)]);

        let rolled = peers(&allowed, &summary);
        assert_eq!(rolled.len(), 2);
        assert_eq!(rolled[0].peer, "api.openai.com", "refused peers sort first");
        assert_eq!(rolled[0].verdict, "blocked");
        assert_eq!(rolled[0].connections, 0, "it never got a connection");
        assert_eq!(rolled[1].verdict, "allowed");
    }

    #[test]
    fn violations_are_newest_first() {
        let summary = Summary {
            violations: vec![
                Violation {
                    target: "a".into(),
                    verdict: "blocked".into(),
                    reason: String::new(),
                    ts: "2026-08-06T22:14:01.000Z".into(),
                },
                Violation {
                    target: "b".into(),
                    verdict: "blocked".into(),
                    reason: String::new(),
                    ts: "2026-08-06T22:14:09.000Z".into(),
                },
            ],
            ..Default::default()
        };
        let shown = violations(&summary);
        assert_eq!(shown[0].target, "b");
        // Rendered like every other table on the tab; the export keeps the
        // whole instant.
        assert_eq!(shown[0].ts, "22:14:09.000");
    }

    // ── filtering ────────────────────────────────────────────────────

    #[test]
    fn a_domain_filter_keeps_only_that_domain() {
        let filter = Filter {
            domains: vec!["network".into()],
            ..Default::default()
        };
        assert!(filter.matches(&connect(1, 1, "10.0.0.1", 80, 0, 0)));
        assert!(!filter.matches(&base(1, 1, EventType::Exec)));
    }

    #[test]
    fn free_text_matches_the_correlated_name_not_only_the_address() {
        // The reason filtering happens after correlation: this event stores
        // `151.101.0.223` and is named `pypi.org` only by the DNS join.
        let mut event = connect(812, 1, "151.101.0.223", 443, 0, 0);
        event.net.as_mut().unwrap().domain = "pypi.org".into();

        let filter = Filter::searching("pypi");
        assert!(filter.matches(&event));
    }

    // ── the cursor ───────────────────────────────────────────────────

    #[test]
    fn a_skipped_count_includes_what_retention_already_evicted() {
        // Counting surviving rows reported a tab suspended across a very long
        // burst as having missed only what is still on disk, quietly dropping
        // the evicted ones from the number — which is the opposite of what the
        // notice is for. The id span counts both.
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        let path = crate::obs::collector::store_path(dir.path(), "alpha");
        let store = crate::obs::Store::open(&path).unwrap();

        let cursor = Cursor {
            store: store.generation().unwrap(),
            row: 0,
        };
        for i in 0..900u32 {
            let mut e = base(i + 1, u64::from(i) + 1, EventType::Exec);
            e.box_id = "alpha".into();
            e.exec = Some(Exec {
                path: "/bin/true".into(),
                ..Default::default()
            });
            store.insert(&e).unwrap();
        }
        store
            .enforce_retention(crate::obs::Retention {
                max_events: 300,
                ..crate::obs::Retention::default()
            })
            .unwrap();
        assert!(store.count().unwrap() <= 300, "retention did evict");

        let page = tail(&manager, "alpha", cursor, &Filter::default()).unwrap();
        assert_eq!(page.rows.len(), TAIL_LIMIT);
        assert_eq!(
            page.skipped,
            900 - TAIL_LIMIT as u64,
            "the evicted events were missed too"
        );
    }

    #[test]
    fn a_cursor_survives_a_round_trip_and_refuses_a_hostile_one() {
        let cursor = Cursor {
            store: 987_654,
            row: 41,
        };
        assert_eq!(Cursor::parse(&cursor.encode()), cursor);

        // It arrives in a query string, so both halves are attacker-chosen.
        // A negative row reaches `newest - row`, which overflows on
        // `i64::MIN`: a panic in a checked build, a wrapped comparison in a
        // release one.
        assert_eq!(Cursor::parse("0:-9223372036854775808").row, 0);
        assert_eq!(Cursor::parse("nonsense").row, 0);
        assert_eq!(Cursor::parse("").store, 0);
        assert_eq!(Cursor::parse("1:2:3").row, 0, "trailing junk is not a row");
    }

    #[test]
    fn a_free_text_filter_is_normalized_once_and_bounded() {
        // The needle is compared against every haystack of every row of every
        // refresh, and the query arrives in a URL — so it is trimmed and
        // lowercased once at construction, and it is not unbounded.
        let filter = Filter::from_query(&format!("q=%20PyPI%20&q2=x&{}", "&".repeat(0)));
        assert_eq!(filter.query, " PyPI ");
        assert!(filter.matches(&{
            let mut e = connect(1, 1, "1.2.3.4", 443, 0, 0);
            e.net.as_mut().unwrap().domain = "pypi.org".into();
            e
        }));

        let huge = "a".repeat(100_000);
        let bounded = Filter::from_query(&format!("q={huge}"));
        assert_eq!(bounded.query.chars().count(), MAX_QUERY_CHARS);

        // The bound is in characters. Cutting at byte 256 of this one lands
        // inside the emoji, which `String::truncate` answers with a panic.
        let boundary = format!("q={}%F0%9F%98%80", "a".repeat(MAX_QUERY_CHARS - 1));
        assert_eq!(
            Filter::from_query(&boundary).query.chars().count(),
            MAX_QUERY_CHARS
        );
    }

    #[test]
    fn a_filter_built_by_literal_searches_for_nothing() {
        // The normalized needle is what `matches` reads, so a struct literal
        // without one must not silently match everything *and* nothing —
        // `Default` means no filter, and that is what it does.
        let literal = Filter {
            query: "pypi".into(),
            ..Default::default()
        };
        assert!(literal.is_empty(), "an unnormalized query is not a filter");
        assert!(literal.matches(&base(1, 1, EventType::Exec)));
    }

    #[test]
    fn the_activity_views_refuse_a_name_that_escapes_the_state_directory() {
        // These routes take the box name straight from the URL and never look
        // up a sandbox, and axum has already percent-decoded the segment — so
        // `..%2f..%2f` is an ordinary `../../` by the time it lands here.
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        for name in ["../../etc", "..", "a/../../b", "/absolute"] {
            assert!(
                open_store(&manager, name).unwrap().is_none(),
                "opened a store for {name:?}"
            );
            assert_eq!(store_file_id(&manager, name), 0, "identified {name:?}");
        }
    }

    #[test]
    fn a_filter_reads_repeated_domain_keys_and_refuses_invented_ones() {
        let filter = Filter::from_query("domain=network&domain=process&q=pip&pid=812");
        assert_eq!(filter.domains, vec!["network", "process"]);
        assert_eq!(filter.query, "pip");
        assert_eq!(filter.pid, Some(812));

        // A value that is not a real domain must not silently match nothing.
        let bogus = Filter::from_query("domain=everything");
        assert!(bogus.domains.is_empty());
        assert!(bogus.is_empty(), "an unusable filter is no filter");
    }

    #[test]
    fn chips_count_the_window_not_the_filtered_stream() {
        let events = vec![
            base(1, 1, EventType::Exec),
            base(1, 2, EventType::Exec),
            connect(1, 3, "10.0.0.1", 80, 0, 0),
        ];
        let filter = Filter {
            domains: vec!["network".into()],
            ..Default::default()
        };
        let chips = domain_chips(&events, &filter);
        let process = chips.iter().find(|c| c.name == "process").unwrap();
        assert_eq!(process.count, 2, "a chip says what turning it on reveals");
        assert!(!process.on);
        assert!(chips.iter().find(|c| c.name == "network").unwrap().on);

        // Every domain, always — a control that appears only once its first
        // event has been seen is a control nobody can use to go looking, and a
        // fixed set is what lets a refresh send counts without touching state.
        assert_eq!(chips.len(), 6);
        assert_eq!(chips.iter().find(|c| c.name == "api").unwrap().count, 0);
    }
}
