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
use crate::sandbox::overlay::{ChangeStatus, LOWER, OverlayChange, WORKSPACE};

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
    /// Writes the run made outside the workspace overlay.
    ///
    /// The overlay diff answers "what would `devbox commit` sync", which is
    /// the question about the *host*. It is not the question about the run: a
    /// command that downloaded twenty megabytes of wheels into `~/.cache/uv`
    /// changed nothing in the upper layer and was reported as "No file
    /// changes" — which is true of the workspace and false of the box.
    pub outside: Vec<OutsideWrites>,
}

/// Writes under one top-level directory that the overlay does not carry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutsideWrites {
    /// The directory, with `$HOME` folded back to `~` where it applies.
    pub prefix: String,
    pub writes: usize,
    /// Distinct paths seen under it, which is the number that says whether
    /// this was one file rewritten or a tree unpacked.
    pub paths: usize,
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

    /// Whether anything at all was written outside the overlay.
    pub fn has_outside(&self) -> bool {
        !self.outside.is_empty()
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
    /// Connection *starts* observed: `connect` and `accept`.
    pub connections: usize,
    /// Connection *ends* observed: `close`. Not the same number, and the
    /// difference is information — a run that inherited an open socket has a
    /// close with no connect, and a run still holding one has the reverse.
    pub closes: usize,
    /// Destination ports seen, sorted.
    pub ports: Vec<u16>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    /// Summed connection lifetime, from `close`.
    pub dur_ms: u64,
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

    /// The connection count, and the closes that had no start in this window.
    ///
    /// Quiet in the ordinary case, where the two are equal. Loud exactly when
    /// the run's window clipped a connection — which is the case where a bare
    /// count would understate what the peer was actually used for.
    pub fn conns_human(&self) -> String {
        if self.closes > self.connections {
            format!(
                "{} (+{} closed)",
                self.connections,
                self.closes - self.connections
            )
        } else {
            self.connections.to_string()
        }
    }

    pub fn dur_human(&self) -> String {
        if self.dur_ms == 0 {
            String::new()
        } else {
            human_duration(Some(self.dur_ms as i64))
        }
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
    /// Devbox's own plumbing rather than the user's command.
    ///
    /// Serialized, so a consumer of the JSON can make the same distinction the
    /// rendered tree makes rather than re-deriving it from the argv.
    #[serde(default)]
    pub wrapper: bool,
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
    /// The broker provider: `anthropic`, `github`, `http:<name>`.
    pub provider: String,
    /// The upstream the broker spoke to on the run's behalf.
    pub host: String,
    /// HTTP methods seen, sorted, comma-joined.
    pub method: String,
    pub uses: usize,
    /// How many of those the broker refused. A row where this equals `uses` is
    /// a policy working, not a credential being used.
    pub denied: usize,
    /// Wall clock of the most recent request, which is what someone reading a
    /// report next to a log wants to line up.
    pub last_use: String,
}

impl CredentialUse {
    /// `3` normally, `3 (2 denied)` when any were refused.
    pub fn uses_human(&self) -> String {
        if self.denied == 0 {
            self.uses.to_string()
        } else {
            format!("{} ({} denied)", self.uses, self.denied)
        }
    }
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
        // `$HOME` inside the guest, so `~/.cache/uv` reads as itself rather
        // than as `/home/ethan.guest/.cache`. Taken from the run's own exec
        // events, which carry the cwd the wrapper set; empty is fine and
        // simply means no path gets folded to `~`.
        let home = guest_home(events);

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
            files: file_changes(scope, &changes, outside_writes(events, &home)),
            network: network(events, &summary),
            processes: processes(events),
            violations: summary.violations.clone(),
            credential_use: credential_use(events),
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

/// The guest's `$HOME`, from the wrapper's own argv.
///
/// Only the wrapper knows it: the host cannot ask, because `$HOME` inside a
/// box is the box's user's, and the exec events are the only place it is
/// written down. Empty when there is no wrapper exec in the run, which folds
/// no path to `~` and is only a cosmetic loss.
fn guest_home(events: &[Event]) -> String {
    events
        .iter()
        .filter(|e| e.kind == EventType::Exec)
        .filter_map(|e| e.exec.as_ref())
        .find_map(|exec| crate::obs::run::home_from_wrapper(&exec.argv))
        .unwrap_or_default()
        .to_string()
}

/// The broker requests this run made, one row per provider and upstream host.
///
/// Grouped rather than listed: an agent session makes hundreds of calls to one
/// host, and a report that prints them all is a log, not a report. The counts
/// and the last timestamp are what a reader lines up against the broker's own
/// audit when they want the individual requests.
///
/// These events have no pid — the broker is a host process, not something in
/// the box — so they reach a run through the window rule (§4.2).
fn credential_use(events: &[Event]) -> Vec<CredentialUse> {
    /// What one (provider, upstream) pair accumulates on the way through.
    #[derive(Default)]
    struct Tally {
        uses: usize,
        denied: usize,
        methods: std::collections::BTreeSet<String>,
        last_use: String,
    }

    let mut by_key: BTreeMap<(String, String), Tally> = BTreeMap::new();
    for event in events {
        if event.kind != EventType::Credential {
            continue;
        }
        let Some(credential) = &event.credential else {
            continue;
        };
        let key = (credential.provider.clone(), credential.host.clone());
        let tally = by_key.entry(key).or_default();
        tally.uses += 1;
        // Anything that is not an outright allow. `denied` and `error` are
        // different things to the broker and the same thing to a reader
        // checking whether the credential was actually used.
        if credential.verdict != "allowed" {
            tally.denied += 1;
        }
        if !credential.method.is_empty() {
            tally.methods.insert(credential.method.clone());
        }
        if event.ts_wall > tally.last_use {
            tally.last_use = event.ts_wall.clone();
        }
    }

    let mut rows: Vec<CredentialUse> = by_key
        .into_iter()
        .map(|((provider, host), tally)| CredentialUse {
            provider,
            host,
            method: tally.methods.into_iter().collect::<Vec<_>>().join(", "),
            uses: tally.uses,
            denied: tally.denied,
            last_use: tally.last_use,
        })
        .collect();
    // Busiest first, and a refused provider ahead of a quiet one at the same
    // count — a denial is the row someone is looking for.
    rows.sort_by(|a, b| {
        b.uses
            .cmp(&a.uses)
            .then_with(|| b.denied.cmp(&a.denied))
            .then_with(|| a.provider.cmp(&b.provider))
    });
    rows
}

/// How many outside-write prefixes a report lists.
///
/// Five, and the tail is summarised rather than dropped. The point of the
/// section is "this run wrote somewhere `devbox commit` will not see"; a
/// reader needs the shape of that, not an inventory.
const OUTSIDE_PREFIXES: usize = 5;

/// Writes the run made outside the workspace overlay, by top-level directory.
///
/// `/workspace` and `/mnt/host` are excluded because the overlay diff above
/// already accounts for them, and everything ephemeral to the guest's own
/// machinery is excluded too — a report whose largest "finding" is that a
/// command wrote to `/proc/self/fd` has buried the one that matters.
fn outside_writes(events: &[Event], home: &str) -> Vec<OutsideWrites> {
    use std::collections::BTreeSet;

    // Pseudo-filesystems and the tmpfs devbox uses for its own bookkeeping.
    // These are writes in the kernel's sense and noise in every other one.
    const IGNORED: [&str; 6] = ["/proc/", "/sys/", "/dev/", "/run/", "/tmp/", "/var/log/"];

    let mut by_prefix: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    for event in events {
        if event.kind != EventType::File {
            continue;
        }
        let Some(file) = &event.file else { continue };
        if !matches!(file.op.as_str(), "write" | "create") {
            continue;
        }
        let path = file.path.as_str();
        if !path.starts_with('/') {
            continue;
        }
        if path.starts_with(&format!("{WORKSPACE}/"))
            || path == WORKSPACE
            || path.starts_with(&format!("{LOWER}/"))
            || path == LOWER
        {
            continue;
        }
        if IGNORED.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        let entry = by_prefix
            .entry(prefix_of(path, home))
            .or_insert((0, BTreeSet::new()));
        entry.0 += 1;
        entry.1.insert(path.to_string());
    }

    let mut rows: Vec<OutsideWrites> = by_prefix
        .into_iter()
        .map(|(prefix, (writes, paths))| OutsideWrites {
            prefix,
            writes,
            paths: paths.len(),
        })
        .collect();
    // Busiest first: the reader wants the twenty-megabyte cache, not the
    // alphabetically-first dotfile.
    rows.sort_by(|a, b| {
        b.writes
            .cmp(&a.writes)
            .then_with(|| a.prefix.cmp(&b.prefix))
    });
    if rows.len() > OUTSIDE_PREFIXES {
        let tail: Vec<OutsideWrites> = rows.split_off(OUTSIDE_PREFIXES);
        rows.push(OutsideWrites {
            prefix: format!(
                "… and {} more director{}",
                tail.len(),
                if tail.len() == 1 { "y" } else { "ies" }
            ),
            writes: tail.iter().map(|r| r.writes).sum(),
            paths: tail.iter().map(|r| r.paths).sum(),
        });
    }
    rows
}

/// The directory a path is filed under: two levels below `$HOME`, one level
/// below `/`.
///
/// `~/.cache/uv` rather than `~` or the full path to every wheel — a home
/// directory is where everything lives, so one level of it says nothing, and
/// the whole path says too much.
fn prefix_of(path: &str, home: &str) -> String {
    let (base, rest) = if !home.is_empty() && home != "/" && path.starts_with(&format!("{home}/")) {
        ("~".to_string(), &path[home.len() + 1..])
    } else {
        (String::new(), path.trim_start_matches('/'))
    };
    let depth = if base == "~" { 2 } else { 1 };
    let head: Vec<&str> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .take(depth)
        .collect();
    if head.is_empty() {
        return if base.is_empty() {
            "/".to_string()
        } else {
            base
        };
    }
    if base.is_empty() {
        format!("/{}", head.join("/"))
    } else {
        format!("{base}/{}", head.join("/"))
    }
}

fn file_changes(
    scope: &str,
    changes: &[OverlayChange],
    outside: Vec<OutsideWrites>,
) -> FileChanges {
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
        outside,
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
                if let Some(net) = &event.net
                    && net.dport != 0
                    && !row.ports.contains(&net.dport)
                {
                    row.ports.push(net.dport);
                }
            }
            EventType::Close => {
                // Traffic is counted here and only here — the same rule
                // `behavior::summarize` follows, and for the same reason: a
                // `connect` fires from a probe that runs before any payload
                // has crossed the socket, so its counters are zero by
                // construction. Adding both ends would start double-counting
                // the day a source begins filling them.
                let row = by_peer.entry(peer.clone()).or_insert_with(|| DomainRow {
                    peer: peer.clone(),
                    ..Default::default()
                });
                row.closes += 1;
                if let Some(net) = &event.net {
                    if net.dport != 0 && !row.ports.contains(&net.dport) {
                        row.ports.push(net.dport);
                    }
                    // Saturating: the agent supplies these and nothing
                    // validates them, so two events claiming most of a `u64`
                    // between them would panic a checked build.
                    row.bytes_tx = row.bytes_tx.saturating_add(net.bytes_tx);
                    row.bytes_rx = row.bytes_rx.saturating_add(net.bytes_rx);
                    row.dur_ms = row.dur_ms.saturating_add(net.dur_ms);
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

/// What a folded run of devbox's own processes renders as.
pub const WRAPPER_ROW: &str = "[devbox wrapper]";

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
    let rows: Vec<ProcessRow> = correlate::tree(&chains)
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
                wrapper: is_wrapper(chain),
            }
        })
        .collect();
    fold_wrappers(rows)
}

/// Whether a chain is devbox's own plumbing.
///
/// Asked of the chain's `argv` when there is one, because that is the form the
/// generators produce and the predicates match. A chain with no exec — a
/// process the capture layer only ever saw doing something else — is the
/// user's until proven otherwise.
fn is_wrapper(chain: &correlate::Chain) -> bool {
    chain
        .events
        .iter()
        .filter(|e| e.kind == EventType::Exec)
        .filter_map(|e| e.exec.as_ref())
        .any(|exec| {
            crate::obs::run::is_wrapper_command(&exec.argv)
                || crate::mcp::shim::is_wrapper_command(&exec.argv)
                // The runtime's own login shell, which nothing in devbox
                // writes: `limactl shell` builds it on the guest side, and it
                // arrives before anything devbox asked for.
                || crate::runtime::is_login_wrapper(&exec.argv)
        })
}

/// Collapse each run of devbox's own processes into one line, and re-root what
/// is left so the user's command is the tree.
///
/// Not "drop them": the wrapper is how the report knows what it knows, and a
/// tree that silently omits three processes is a tree nobody can reconcile
/// with `devbox watch --tree`, which shows the raw view on purpose. One line
/// says they were there and gets out of the way.
fn fold_wrappers(rows: Vec<ProcessRow>) -> Vec<ProcessRow> {
    if !rows.iter().any(|r| r.wrapper) {
        return rows;
    }

    let mut out: Vec<ProcessRow> = Vec::with_capacity(rows.len());
    let mut folding: Option<ProcessRow> = None;
    for row in rows {
        if row.wrapper {
            match &mut folding {
                // Keep the first one's pid and start: it is the process that
                // actually began the wrapping, and the timestamp is what lines
                // the report up against `devbox watch`.
                Some(open) => open.files += row.files,
                None => {
                    folding = Some(ProcessRow {
                        depth: 0,
                        comm: "devbox".to_string(),
                        command: WRAPPER_ROW.to_string(),
                        peers: Vec::new(),
                        ..row
                    })
                }
            }
            continue;
        }
        if let Some(open) = folding.take() {
            out.push(open);
        }
        out.push(row);
    }
    if let Some(open) = folding.take() {
        out.push(open);
    }

    // Re-root. A user process whose parent was folded away kept the depth that
    // parent gave it, so the tree opened one indent in from nothing.
    let shallowest = out
        .iter()
        .filter(|r| r.command != WRAPPER_ROW)
        .map(|r| r.depth)
        .min()
        .unwrap_or(0);
    for row in &mut out {
        if row.command != WRAPPER_ROW {
            row.depth -= shallowest.min(row.depth);
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Exec, File as FileDetail};
    use crate::obs::run::{RUN_ID_ENV, SCOPED_FLAG, bootstrap, wrapper_script};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    fn exec_event(pid: u32, ppid: u32, comm: &str, ts: &str, words: &[&str]) -> Event {
        Event {
            ts_wall: ts.to_string(),
            ts_mono_ns: ts.len() as u64,
            box_id: "b".into(),
            cgroup_id: 1,
            pid,
            tid: pid,
            ppid,
            comm: comm.into(),
            uid: 1000,
            kind: EventType::Exec,
            net: None,
            exec: Some(Exec {
                path: format!("/bin/{comm}"),
                argv: argv(words),
                cwd: "/workspace".into(),
            }),
            file: None,
            api: None,
            policy: None,
            credential: None,
        }
    }

    fn write_event(pid: u32, ts: &str, path: &str) -> Event {
        Event {
            file: Some(FileDetail {
                path: path.into(),
                op: "write".into(),
                flags: 0,
            }),
            kind: EventType::File,
            exec: None,
            ..exec_event(pid, 1, "sh", ts, &["sh"])
        }
    }

    // ----------------------------------------------------------- wrappers

    #[test]
    fn every_shape_of_devbox_plumbing_is_recognised() {
        use crate::mcp::shim;
        use crate::obs::run;

        // 1. The bootstrap, exactly as `devbox run` generates it.
        let boot = bootstrap("01ABCDEFGHJKMNPQRSTVWXYZ00", "/workspace");
        assert!(run::is_wrapper_command(&boot), "bootstrap");
        assert!(boot[2].contains(SCOPED_FLAG), "the script carries the flag");
        assert!(wrapper_script().contains(SCOPED_FLAG));

        // 2. The wrapper re-execing itself into its scope.
        assert!(run::is_wrapper_command(&argv(&[
            "/bin/sh",
            "/tmp/.devbox-run-01ABCDEFGHJKMNPQRSTVWXYZ00.sh",
            SCOPED_FLAG,
            "systemd-user",
            "01ABCDEFGHJKMNPQRSTVWXYZ00",
            "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json",
            "/workspace",
            "/home/dev",
            "dev",
            "sh",
            "-c",
            "true",
        ])));

        // 3. systemd-run, and the two helpers the wrapper spawns.
        assert!(run::is_wrapper_command(&argv(&[
            "systemd-run",
            "--user",
            "--scope",
            "--unit=devbox-run-01ABCDEFGHJKMNPQRSTVWXYZ00",
            "--quiet",
        ])));
        assert!(run::is_wrapper_command(&argv(&[
            "stat",
            "-c",
            "%i",
            "/sys/fs/cgroup/user.slice/devbox-run-01ABCDEFGHJKMNPQRSTVWXYZ00.scope",
        ])));
        assert!(run::is_wrapper_command(&argv(&[
            "mv",
            "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json.tmp",
            "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json",
        ])));

        // 4. The environment shim.
        assert!(run::is_wrapper_command(&argv(&[
            "env",
            "--",
            "DEVBOX_BROKER_URL=http://h:9",
            &format!("{RUN_ID_ENV}=01ABC"),
            "sh",
            "-c",
            "true",
        ])));

        // 5. The MCP shim and its reaper.
        let mcp = shim::wrap_guest_command(
            "/tmp/devbox-mcp-fetch-0123456789abcdef.pgid",
            std::iter::empty(),
            &["uvx".to_string(), "mcp-server-fetch".to_string()],
        );
        assert!(shim::is_wrapper_command(&mcp), "mcp wrapper: {mcp:?}");
        assert!(shim::is_wrapper_command(&shim::reaper_script(
            "/tmp/devbox-mcp-fetch-0123456789abcdef.pgid"
        )));
    }

    #[test]
    fn a_users_command_that_merely_mentions_devbox_is_not_folded() {
        use crate::obs::run;
        // The trap a bare substring match falls into: the variable's *name*
        // appears in a command the user wrote, and folding it would delete
        // their command from their own report.
        assert!(!run::is_wrapper_command(&argv(&[
            "sh",
            "-c",
            "echo $DEVBOX_RUN_ID",
        ])));
        assert!(!run::is_wrapper_command(&argv(&["env"])));
        assert!(!run::is_wrapper_command(&argv(&[
            "curl",
            "-s",
            "https://x"
        ])));
        assert!(!run::is_wrapper_command(&[]));
        assert!(!crate::mcp::shim::is_wrapper_command(&argv(&[
            "uvx", "srv"
        ])));
    }

    #[test]
    fn the_wrapper_folds_to_one_line_and_the_command_becomes_the_root() {
        // The shape a real run produces: bootstrap, the scoped re-exec, two
        // helpers, then the user's command and its child.
        let events = vec![
            exec_event(
                900,
                1,
                "sh",
                "2026-09-05T10:00:00.000Z",
                &[
                    "/bin/sh",
                    "/tmp/.devbox-run-01ABCDEFGHJKMNPQRSTVWXYZ00.sh",
                    SCOPED_FLAG,
                    "systemd-user",
                    "01ABCDEFGHJKMNPQRSTVWXYZ00",
                    "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json",
                    "/workspace",
                    "/home/dev",
                    "dev",
                    "sh",
                    "-c",
                    "curl https://example.com",
                ],
            ),
            exec_event(
                901,
                900,
                "stat",
                "2026-09-05T10:00:00.100Z",
                &[
                    "stat",
                    "-c",
                    "%i",
                    "/sys/fs/cgroup/user.slice/devbox-run-01ABCDEFGHJKMNPQRSTVWXYZ00.scope",
                ],
            ),
            exec_event(
                902,
                900,
                "mv",
                "2026-09-05T10:00:00.110Z",
                &[
                    "mv",
                    "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json.tmp",
                    "/run/devbox/runs/01ABCDEFGHJKMNPQRSTVWXYZ00.json",
                ],
            ),
            exec_event(
                900,
                1,
                "sh",
                "2026-09-05T10:00:00.200Z",
                &["sh", "-c", "curl https://example.com"],
            ),
            exec_event(
                903,
                900,
                "curl",
                "2026-09-05T10:00:00.300Z",
                &["curl", "https://example.com"],
            ),
        ];

        let rows = processes(&events);
        let folded: Vec<&ProcessRow> = rows.iter().filter(|r| r.wrapper).collect();
        assert_eq!(
            folded.len(),
            1,
            "three wrapper processes, one line: {rows:#?}"
        );
        assert_eq!(folded[0].command, WRAPPER_ROW);
        assert_eq!(folded[0].pid, 900, "the first wrapper's pid is kept");
        assert_eq!(
            folded[0].started, "2026-09-05T10:00:00.000Z",
            "and its timestamp"
        );

        // The user's command is the root, and its child is under it.
        let user: Vec<&ProcessRow> = rows.iter().filter(|r| !r.wrapper).collect();
        assert_eq!(user.len(), 2, "{rows:#?}");
        assert_eq!(user[0].command, "sh -c curl https://example.com");
        assert_eq!(user[0].depth, 0, "the user's command is the tree root");
        assert_eq!(user[1].comm, "curl");
        assert!(user[1].depth > 0, "its child is under it");
    }

    #[test]
    fn a_run_with_no_wrapper_is_left_exactly_as_it_was() {
        // `exec` and `shell` runs have no wrapper. Folding must be a no-op
        // there rather than re-rooting a tree that was already rooted.
        let events = vec![
            exec_event(
                900,
                1,
                "sh",
                "2026-09-05T10:00:00.000Z",
                &["sh", "-c", "true"],
            ),
            exec_event(901, 900, "true", "2026-09-05T10:00:00.100Z", &["true"]),
        ];
        let rows = processes(&events);
        assert!(rows.iter().all(|r| !r.wrapper));
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows.len(), 2);
    }

    // ---------------------------------------------------- outside writes

    #[test]
    fn writes_outside_the_overlay_are_grouped_by_directory() {
        let mut events = vec![
            // Inside the workspace: the overlay diff already has these.
            write_event(900, "2026-09-05T10:00:00.000Z", "/workspace/w2-5.txt"),
            write_event(900, "2026-09-05T10:00:00.001Z", "/mnt/host/src/main.rs"),
            // Pseudo-filesystems and devbox's own tmpfs bookkeeping.
            write_event(900, "2026-09-05T10:00:00.002Z", "/proc/self/uid_map"),
            write_event(900, "2026-09-05T10:00:00.003Z", "/run/devbox/runs/x.json"),
            write_event(900, "2026-09-05T10:00:00.004Z", "/dev/null"),
        ];
        // The thing the section exists for.
        for i in 0..40 {
            events.push(write_event(
                901,
                "2026-09-05T10:00:01.000Z",
                &format!("/home/dev/.cache/uv/wheel-{i}.whl"),
            ));
        }
        for i in 0..3 {
            events.push(write_event(
                901,
                "2026-09-05T10:00:02.000Z",
                &format!("/home/dev/.npm/_cacache/{i}"),
            ));
        }
        events.push(write_event(
            901,
            "2026-09-05T10:00:03.000Z",
            "/etc/hosts.new",
        ));

        let rows = outside_writes(&events, "/home/dev");
        let prefixes: Vec<&str> = rows.iter().map(|r| r.prefix.as_str()).collect();
        assert_eq!(prefixes, vec!["~/.cache/uv", "~/.npm/_cacache", "/etc"]);
        assert_eq!(rows[0].writes, 40);
        assert_eq!(rows[0].paths, 40);
        assert_eq!(rows[1].writes, 3);

        // With no `$HOME` known, nothing folds to `~` and the paths still
        // group — a worse-looking report, not a wrong one.
        let rows = outside_writes(&events, "");
        assert_eq!(rows[0].prefix, "/home");

        // Nothing outside means no section at all.
        let inside = vec![write_event(900, "2026-09-05T10:00:00.000Z", "/workspace/a")];
        assert!(outside_writes(&inside, "/home/dev").is_empty());
    }

    #[test]
    fn only_five_directories_are_listed_and_the_rest_are_counted() {
        let mut events = Vec::new();
        // Seven directories, descending in size, so the order is decided.
        for (dir, n) in [
            ("a", 7),
            ("b", 6),
            ("c", 5),
            ("d", 4),
            ("e", 3),
            ("f", 2),
            ("g", 1),
        ] {
            for i in 0..n {
                events.push(write_event(
                    900,
                    "2026-09-05T10:00:00.000Z",
                    &format!("/opt/{dir}/{i}"),
                ));
            }
        }
        let rows = outside_writes(&events, "/home/dev");
        // Everything under /opt collapses to one prefix — one level below `/`.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefix, "/opt");
        assert_eq!(rows[0].writes, 28);

        // Now seven genuinely different top-level directories.
        let events: Vec<Event> = "abcdefg"
            .chars()
            .enumerate()
            .flat_map(|(i, c)| {
                (0..(7 - i)).map(move |j| {
                    write_event(900, "2026-09-05T10:00:00.000Z", &format!("/{c}/{j}"))
                })
            })
            .collect();
        let rows = outside_writes(&events, "");
        assert_eq!(rows.len(), OUTSIDE_PREFIXES + 1, "five plus a tail");
        assert_eq!(rows[0].prefix, "/a");
        assert!(rows[5].prefix.starts_with("… and 2 more"), "{:?}", rows[5]);
        assert_eq!(rows[5].writes, 2 + 1);
    }

    // ------------------------------------------------------- credentials

    #[test]
    fn credential_use_groups_by_provider_and_upstream() {
        use crate::obs::event::Credential;

        let credential =
            |provider: &str, host: &str, method: &str, verdict: &str, ts: &str| Event {
                ts_wall: ts.to_string(),
                kind: EventType::Credential,
                pid: crate::obs::run::UNATTRIBUTED_PID,
                exec: None,
                credential: Some(Credential {
                    provider: provider.into(),
                    method: method.into(),
                    host: host.into(),
                    path: "/v1/x".into(),
                    status: 200,
                    verdict: verdict.into(),
                    ..Default::default()
                }),
                ..exec_event(1, 1, "broker", ts, &["x"])
            };

        let events = vec![
            credential(
                "anthropic",
                "api.anthropic.com",
                "POST",
                "allowed",
                "2026-09-05T10:00:00.000Z",
            ),
            credential(
                "anthropic",
                "api.anthropic.com",
                "GET",
                "allowed",
                "2026-09-05T10:00:01.000Z",
            ),
            credential(
                "anthropic",
                "api.anthropic.com",
                "POST",
                "denied",
                "2026-09-05T10:00:02.000Z",
            ),
            credential(
                "github",
                "github.com",
                "GET",
                "allowed",
                "2026-09-05T10:00:03.000Z",
            ),
        ];

        let rows = credential_use(&events);
        assert_eq!(rows.len(), 2, "one row per provider and upstream");
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[0].host, "api.anthropic.com");
        assert_eq!(rows[0].method, "GET, POST", "methods are merged, sorted");
        assert_eq!(rows[0].uses, 3);
        assert_eq!(rows[0].denied, 1);
        assert_eq!(
            rows[0].last_use, "2026-09-05T10:00:02.000Z",
            "the most recent, not the first"
        );
        assert_eq!(rows[0].uses_human(), "3 (1 denied)");
        assert_eq!(rows[1].provider, "github");
        assert_eq!(rows[1].uses_human(), "1", "a clean row stays quiet");

        assert!(credential_use(&[]).is_empty());
    }
}
