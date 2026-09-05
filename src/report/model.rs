//! What a run report contains (§4.4).
//!
//! One value assembled from five sources — the run row, the overlay (component
//! E's checkpoint diff after integration), the event store, the policy events
//! in it, and the collector's own health — and three renderers over it. The
//! model is the contract: the JSON *is* this struct, and component G's export
//! reads it rather than re-deriving anything.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::obs::behavior::{self, Summary, Violation};
use crate::obs::correlate;
use crate::obs::event::{Event, EventType};
use crate::obs::run::{Attribution, RunRecord, UNATTRIBUTED_PID};
use crate::sandbox::overlay::{ChangeStatus, OverlayChange};

/// Where the file changes in a report came from.
///
/// Wave 1 has no checkpoints, so the only honest answer is "everything the
/// overlay has accumulated since the box was created", which is a superset of
/// what this run did. Naming that in the model — and printing it in all three
/// renderers — is the difference between a caveat and a lie.
pub const SCOPE_BOX: &str = "box (not run-scoped until checkpoints land)";

/// Run-scoped, from a checkpoint taken at the start and one at the end.
pub const SCOPE_RUN: &str = "run";

/// How the file changes were obtained.
///
/// A closure rather than a call, because wave 1 and wave 2 answer it
/// differently and the seam is the whole point: `devbox run` passes the
/// box-wide `overlay::diff` today, and the supervisor swaps in
/// `checkpoint::diff(start, end)` at integration without the report module
/// learning that checkpoints exist.
pub type FileChangeSource = Box<dyn Fn() -> Result<Vec<OverlayChange>> + Send>;

/// The whole report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// The schema of this document, so a consumer can tell versions apart.
    pub report_version: u32,
    pub run: RunRecord,
    /// Milliseconds between `started_at` and `ended_at`, when both are known.
    pub duration_ms: Option<i64>,
    pub files: FileChanges,
    pub network: Network,
    pub processes: Vec<ProcessRow>,
    pub violations: Vec<Violation>,
    /// Component B, wired at integration. Empty until then, and rendered as
    /// "not recorded" rather than as "none happened".
    pub credential_use: Vec<CredentialUse>,
    pub coverage: Coverage,
}

/// This document's schema version.
pub const REPORT_VERSION: u32 = 1;

/// Files the run changed, and how confident the report is about "the run".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChanges {
    /// [`SCOPE_BOX`] or [`SCOPE_RUN`].
    pub scope: String,
    pub changes: Vec<FileChange>,
    /// Directories are counted but not listed: a hundred new `node_modules`
    /// subdirectories are one fact, not a hundred.
    pub directories: usize,
}

impl FileChanges {
    pub fn added(&self) -> usize {
        self.changes.iter().filter(|c| c.status == "added").count()
    }

    pub fn modified(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.status == "modified")
            .count()
    }

    pub fn deleted(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.status == "deleted")
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// `added`, `modified` or `deleted`.
    pub status: String,
    pub path: String,
}

/// What the run did on the network.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Network {
    /// Every peer reached, with how it was reached.
    pub domains: Vec<DomainRow>,
    /// DNS names looked up, whether or not anything connected to them.
    pub dns: Vec<String>,
    /// TLS server names, which is the one field a proxy cannot forge for free.
    pub tls: Vec<String>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
}

impl Network {
    pub fn tx_human(&self) -> String {
        human_bytes(self.bytes_tx)
    }

    pub fn rx_human(&self) -> String {
        human_bytes(self.bytes_rx)
    }
}

/// One peer and the connections to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainRow {
    pub peer: String,
    pub connections: usize,
    /// Destination ports seen, sorted.
    pub ports: Vec<u16>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    /// Whether a TLS handshake to this peer was observed.
    pub tls: bool,
}

impl DomainRow {
    // Rendering lives on the model, not in the template: Askama hands a field
    // access to a function as a reference, and the markdown renderer wants the
    // same string anyway. One definition, two renderers, no drift.
    pub fn tx_human(&self) -> String {
        human_bytes(self.bytes_tx)
    }

    pub fn rx_human(&self) -> String {
        human_bytes(self.bytes_rx)
    }

    pub fn ports_human(&self) -> String {
        self.ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One process in the run's tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessRow {
    pub depth: usize,
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    /// The command line, when an exec was captured for this pid.
    pub command: String,
    pub started: String,
    pub peers: Vec<String>,
    pub files: usize,
}

impl ProcessRow {
    /// The tree's indentation. Capped for the same reason `correlate::tree`
    /// caps depth: a pid/ppid cycle in a wrapped namespace is real, and
    /// thirty-two levels of indent is already past readable.
    pub fn indent(&self) -> String {
        "  ".repeat(self.depth.min(32))
    }

    /// What to print for this process: its command line, or its name when no
    /// exec was captured for the pid.
    pub fn display_command(&self) -> &str {
        if self.command.is_empty() {
            &self.comm
        } else {
            &self.command
        }
    }
}

/// A credential the broker handed out during the run (component B).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialUse {
    pub name: String,
    pub provider: String,
    pub uses: usize,
    pub first_use: String,
}

/// How much of the run the capture layer could actually see.
///
/// The badge at the top of the report. `ebpf+packet` with no drops and no
/// unattributed events means the report is the run; `proc+packet` with a
/// thousand unattributed events means it is a sketch, and the reader is
/// entitled to know which one they are holding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    /// The composed capture sources — `ebpf+packet+netfilter`, `proc+packet`.
    pub sources: String,
    pub agent_version: String,
    /// Events the collector dropped *during this run*, not since it started.
    pub dropped_events: u64,
    /// Events attributed to this run, by how they were attributed.
    pub attribution: BTreeMap<String, u64>,
    /// Events inside the run's window that belonged to no run at all.
    pub unattributed_in_window: u64,
    pub events: usize,
}

impl Coverage {
    /// Whether every source needed for a complete picture was present.
    pub fn is_full(&self) -> bool {
        self.sources.starts_with("ebpf") && self.dropped_events == 0
    }

    /// One word for the badge.
    pub fn badge(&self) -> &'static str {
        if self.sources.is_empty() {
            "unknown"
        } else if self.is_full() {
            "full"
        } else {
            "partial"
        }
    }
}

impl RunReport {
    /// Assemble a report from a run and the events attributed to it.
    ///
    /// `files` is called exactly once, and its failure is not the report's
    /// failure: a box that has been stopped can no longer be diffed, and a
    /// report without a file section is still the record of what the command
    /// reached on the network.
    pub fn build(
        run: RunRecord,
        events: &[Event],
        files: FileChangeSource,
        scope: &str,
        attribution: Vec<(Attribution, u64)>,
        unattributed_in_window: u64,
    ) -> Self {
        let summary = behavior::summarize(&run.box_id, events);
        let changes = files().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "run report has no file section");
            Vec::new()
        });

        let coverage = Coverage {
            sources: run.capture_sources.clone(),
            agent_version: run.agent_version.clone(),
            dropped_events: run.dropped_events,
            attribution: attribution
                .into_iter()
                .map(|(k, n)| (k.as_str().to_string(), n))
                .collect(),
            unattributed_in_window,
            events: events.len(),
        };

        Self {
            report_version: REPORT_VERSION,
            duration_ms: run.duration_ms(),
            files: file_changes(scope, &changes),
            network: network(events, &summary),
            processes: processes(events),
            violations: summary.violations.clone(),
            credential_use: Vec::new(),
            coverage,
            run,
        }
    }

    /// The directory a report is written to.
    pub fn directory(
        state_dir: &std::path::Path,
        box_id: &str,
        run_id: &str,
    ) -> std::path::PathBuf {
        state_dir.join("runs").join(box_id).join(run_id)
    }
}

fn file_changes(scope: &str, changes: &[OverlayChange]) -> FileChanges {
    let directories = changes.iter().filter(|c| c.is_dir).count();
    let mut rows: Vec<FileChange> = changes
        .iter()
        .filter(|c| !c.is_dir)
        .map(|c| FileChange {
            status: match c.status {
                ChangeStatus::Added => "added",
                ChangeStatus::Modified => "modified",
                ChangeStatus::Deleted => "deleted",
            }
            .to_string(),
            path: c.path.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    FileChanges {
        scope: scope.to_string(),
        changes: rows,
        directories,
    }
}

fn network(events: &[Event], summary: &Summary) -> Network {
    // The same DNS labelling the console and `behavior` do, so a connection
    // that only carried an address is filed under the name that resolved it.
    let mut labelled = events.to_vec();
    let map = correlate::dns_map(&labelled);
    correlate::apply_dns_map(&mut labelled, &map);

    let mut by_peer: BTreeMap<String, DomainRow> = BTreeMap::new();
    let mut tls: Vec<String> = Vec::new();
    for event in &labelled {
        let Some(peer) = event.peer() else { continue };
        match event.kind {
            EventType::Connect | EventType::Accept => {
                let row = by_peer.entry(peer.clone()).or_insert_with(|| DomainRow {
                    peer: peer.clone(),
                    ..Default::default()
                });
                row.connections += 1;
                if let Some(net) = &event.net {
                    if net.dport != 0 && !row.ports.contains(&net.dport) {
                        row.ports.push(net.dport);
                    }
                    row.bytes_tx = row.bytes_tx.saturating_add(net.bytes_tx);
                    row.bytes_rx = row.bytes_rx.saturating_add(net.bytes_rx);
                }
            }
            EventType::Tls => {
                let row = by_peer.entry(peer.clone()).or_insert_with(|| DomainRow {
                    peer: peer.clone(),
                    ..Default::default()
                });
                row.tls = true;
                if let Some(net) = &event.net
                    && !net.sni.is_empty()
                    && !tls.contains(&net.sni)
                {
                    tls.push(net.sni.clone());
                }
            }
            _ => {}
        }
    }
    for row in by_peer.values_mut() {
        row.ports.sort_unstable();
    }
    tls.sort();

    Network {
        domains: by_peer.into_values().collect(),
        dns: summary.dns_queries.iter().cloned().collect(),
        tls,
        bytes_tx: summary.bytes_tx,
        bytes_rx: summary.bytes_rx,
    }
}

fn processes(events: &[Event]) -> Vec<ProcessRow> {
    // Packet-derived observations have no process; feeding the sentinel pid to
    // the chainer would invent one enormous phantom process that "ran" every
    // blocked connection on the box.
    let owned: Vec<Event> = events
        .iter()
        .filter(|e| e.pid != UNATTRIBUTED_PID && e.pid != 0)
        .cloned()
        .collect();
    let chains = correlate::chains(&owned);
    correlate::tree(&chains)
        .into_iter()
        .map(|(index, depth)| {
            let chain = &chains[index];
            ProcessRow {
                depth,
                pid: chain.pid,
                ppid: chain.ppid,
                comm: chain.comm.clone(),
                command: chain.command.clone().unwrap_or_default(),
                started: chain.started.clone(),
                peers: chain.peers.clone(),
                files: chain.files.len(),
            }
        })
        .collect()
}

/// Bytes as a human reads them. Shared by all three renderers so the markdown
/// summary and the HTML page cannot disagree about the same number.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// A duration as a human reads it.
pub fn human_duration(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "—".to_string();
    };
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let minutes = (seconds / 60.0).floor() as i64;
    let rest = seconds - (minutes as f64) * 60.0;
    format!("{minutes}m{rest:.0}s")
}
