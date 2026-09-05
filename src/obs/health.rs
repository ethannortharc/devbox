//! Per-box capture health — the answer to "why is Activity empty?".
//!
//! The collector runs in its own long-lived process (see [`super::daemon`]),
//! so the console cannot ask it anything directly. It publishes what it knows
//! per box the same way it publishes its counters: a small JSON file replaced
//! atomically, which any number of readers can pick up.
//!
//! The point is to make four situations distinguishable that the console
//! previously rendered identically as "no data yet": the collector daemon is
//! not running, the box is not running, no agent has ever connected, and an
//! agent tried and failed. Only the last one carries a diagnosis, and it is
//! the one that used to be reachable solely by tailing `logs/collector.log`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where a box's capture health is published.
pub fn health_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join("boxes").join(name).join("capture.json")
}

/// How far the collector got with one box's agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// The box is not running, so there is nothing to attach to.
    BoxStopped,
    /// A collector is attaching and the handshake has not completed.
    Starting,
    /// An agent completed the handshake and is streaming events.
    Streaming,
    /// The last attempt ended. `detail` says how.
    Failed,
}

impl CaptureState {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureState::BoxStopped => "box_stopped",
            CaptureState::Starting => "starting",
            CaptureState::Streaming => "streaming",
            CaptureState::Failed => "failed",
        }
    }
}

/// What the collector last knew about one box's agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureHealth {
    pub box_id: String,
    pub state: CaptureState,
    /// When this state was entered, RFC3339.
    pub since: String,
    /// `exec` for runtimes reached through the runtime CLI, `socket` for a
    /// bind-mounted host socket.
    #[serde(default)]
    pub transport: String,
    /// Whether the connected agent attached the eBPF probes.
    #[serde(default)]
    pub ebpf: bool,
    /// Capture backends the agent reported in its hello.
    #[serde(default)]
    pub capture: Vec<String>,
    /// The composed capture sources the agent's preflight actually kept —
    /// `ebpf+packet+netfilter`, `proc+packet`. Empty for a record written
    /// before this field existed, or by an agent that predates it; readers
    /// go through [`capture_source`], which falls back to `ebpf`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub agent_version: String,
    /// Why the last attempt ended, empty while it has not.
    ///
    /// Carries the agent's own last words when it had any. A collector-side
    /// "agent closed the connection before saying hello" names the symptom;
    /// the guest's `devbox-obsd: not found` names the cause, and only one of
    /// the two tells anyone what to do next.
    #[serde(default)]
    pub detail: String,
    /// Consecutive failed attempts. Zero once an agent is streaming.
    #[serde(default)]
    pub attempts: u32,
}

impl CaptureHealth {
    /// A record for a box the collector has decided not to attach to yet.
    pub fn new(box_id: &str, state: CaptureState) -> Self {
        Self {
            box_id: box_id.to_string(),
            state,
            since: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            transport: String::new(),
            ebpf: false,
            capture: Vec::new(),
            source: String::new(),
            agent_version: String::new(),
            detail: String::new(),
            attempts: 0,
        }
    }

    pub fn with_transport(mut self, transport: &str) -> Self {
        self.transport = transport.to_string();
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }
}

/// What a degraded capture cannot see.
///
/// Named rather than implied. `proc + packet` was previously reported as a
/// bare backend list, which reads like a different implementation of the same
/// coverage — and the difference is not cosmetic: /proc/net/tcp has no pid
/// column, so every connection arrives unattributed, and nothing polls file
/// access at all.
pub const DEGRADED_LOSS: &str = "no process attribution, no file events";

/// The capture source of one box, as a line a human can act on.
///
/// One function so `devbox doctor` and the console's capture bar cannot drift
/// apart — they answer the same question from the same record, and a reader
/// who checks both should not have to reconcile two phrasings.
///
/// The record's own `source` when the agent sent one; otherwise reconstructed
/// from `ebpf`, because an agent from before the field existed still knows
/// whether it attached the probes.
pub fn capture_source(health: &CaptureHealth) -> String {
    let composition = capture_composition(health);
    if health.ebpf {
        composition
    } else {
        format!("{composition} (degraded: {DEGRADED_LOSS})")
    }
}

/// Just the composition — `ebpf+packet`, `proc+packet` — with no verdict.
///
/// For callers with somewhere else to put the verdict: the console's status bar
/// names the composition in its headline and spends a whole sentence on what a
/// degraded one costs, where `doctor` has one line for both.
pub fn capture_composition(health: &CaptureHealth) -> String {
    if health.source.is_empty() {
        // An agent from before the field. It still knows whether it attached
        // the probes, so this is a reconstruction, not a guess.
        if health.ebpf { "ebpf" } else { "proc" }.to_string()
    } else {
        health.source.clone()
    }
}

/// Publish a box's capture health, replacing any previous record.
///
/// Written through a temporary file and renamed, so a reader never observes a
/// half-written record — the same discipline the daemon uses for its counters.
pub fn publish(state_dir: &Path, health: &CaptureHealth) -> Result<()> {
    if !crate::sandbox::state::is_safe_name(&health.box_id) {
        anyhow::bail!("refusing to publish capture health for {:?}", health.box_id);
    }
    let path = health_path(state_dir, &health.box_id);
    let parent = path.parent().context("capture health path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create capture health directory {}", parent.display()))?;
    // The same protection the rest of the observability state carries. This
    // record can hold a runtime's error text, and it is written before the
    // store's own directory permissions are established — so on a multi-user
    // host it was the one observability file readable by everyone.
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect capture health directory {}", parent.display()))?;
    // A unique temporary per publish. A single shared `capture.json.tmp` is
    // not atomic between publishers: two agents completing their handshakes
    // together open the same inode, and one can still be writing it when the
    // other renames it into place — which publishes a spliced record, or
    // fails the second rename outright.
    let temporary = parent.join(format!(
        ".capture-{}-{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let body = serde_json::to_vec(health).context("encode capture health")?;
    let published = (|| -> Result<()> {
        std::fs::write(&temporary, body)
            .with_context(|| format!("write capture health {}", temporary.display()))?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protect capture health {}", temporary.display()))?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("publish capture health {}", path.display()))?;
        Ok(())
    })();
    if published.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    published
}

/// Read a box's last published capture health.
///
/// `None` means no collector has ever reached this box — which is itself one
/// of the four states the console has to be able to tell apart.
pub fn load(state_dir: &Path, name: &str) -> Result<Option<CaptureHealth>> {
    let path = health_path(state_dir, name);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read(&path).with_context(|| format!("read capture health {}", path.display()))?;
    // A record from an older schema is not a reason to fail the Activity tab.
    // Treat it as "nothing published", which degrades to the honest "no agent
    // has connected yet" rather than an error page.
    Ok(serde_json::from_slice(&raw).ok())
}

/// Forget a box's record, when its collector is retired.
pub fn clear(state_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(health_path(state_dir, name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_record_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let health = CaptureHealth::new("alpha", CaptureState::Streaming)
            .with_transport("exec")
            .with_detail("");
        publish(dir.path(), &health).unwrap();

        let back = load(dir.path(), "alpha").unwrap().unwrap();
        assert_eq!(back, health);
    }

    #[test]
    fn a_published_record_is_not_readable_by_other_users() {
        // It can carry a runtime's error text, and it is written before the
        // store's own directory permissions exist.
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            &CaptureHealth::new("alpha", CaptureState::Failed).with_detail("secret path"),
        )
        .unwrap();

        let file = std::fs::metadata(health_path(dir.path(), "alpha")).unwrap();
        assert_eq!(
            file.permissions().mode() & 0o077,
            0,
            "group or other can read"
        );
        let parent = std::fs::metadata(dir.path().join("boxes").join("alpha")).unwrap();
        assert_eq!(parent.permissions().mode() & 0o077, 0);
    }

    #[test]
    fn a_box_with_no_record_reads_as_nothing_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "never-seen").unwrap(), None);
    }

    #[test]
    fn an_undecodable_record_degrades_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = health_path(dir.path(), "alpha");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"schema\":\"from the future\"}").unwrap();
        assert_eq!(load(dir.path(), "alpha").unwrap(), None);
    }

    #[test]
    fn an_unsafe_box_name_cannot_be_published() {
        let dir = tempfile::tempdir().unwrap();
        let health = CaptureHealth::new("../../escape", CaptureState::Failed);
        assert!(publish(dir.path(), &health).is_err());
    }

    #[test]
    fn a_kernel_capture_names_its_composition_and_nothing_else() {
        let mut health = CaptureHealth::new("alpha", CaptureState::Streaming);
        health.ebpf = true;
        health.source = "ebpf+packet+netfilter".into();
        assert_eq!(capture_source(&health), "ebpf+packet+netfilter");
    }

    #[test]
    fn a_degraded_capture_says_what_it_costs() {
        let mut health = CaptureHealth::new("alpha", CaptureState::Streaming);
        health.source = "proc+packet".into();
        assert_eq!(
            capture_source(&health),
            "proc+packet (degraded: no process attribution, no file events)"
        );
    }

    #[test]
    fn an_agent_from_before_the_source_field_still_reports_a_source() {
        // The field is new; the agent that omits it is not broken, and
        // reporting nothing would be worse than reporting what `ebpf` implies.
        let mut kernel = CaptureHealth::new("alpha", CaptureState::Streaming);
        kernel.ebpf = true;
        assert_eq!(capture_source(&kernel), "ebpf");

        let polling = CaptureHealth::new("alpha", CaptureState::Streaming);
        assert!(capture_source(&polling).starts_with("proc (degraded:"));
    }

    #[test]
    fn a_failure_carries_its_diagnosis() {
        let health = CaptureHealth::new("alpha", CaptureState::Failed)
            .with_detail("sh: /usr/local/bin/devbox-obsd: not found")
            .with_attempts(3);
        assert!(health.detail.contains("not found"));
        assert_eq!(health.attempts, 3);
        assert_eq!(health.state.as_str(), "failed");
    }
}
