//! Control-plane service layer.
//!
//! The web console and the CLI are two clients of one control plane (§6.4 of
//! the v4 design). Everything reusable between them lives here: view models,
//! status mapping, and the concurrent status fan-out. HTTP handlers stay thin
//! and this layer stays testable without a server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;
use crate::sandbox::state::SandboxState;

/// How long to wait on a runtime status probe before calling it unknown.
///
/// Runtime CLIs (`limactl`, `incus`, `docker`) occasionally hang on a stale
/// socket. The dashboard must still render, so a slow probe degrades to
/// `unknown` rather than blocking the page.
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

/// A box as the console and the JSON API present it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BoxSummary {
    pub name: String,
    pub runtime: String,
    pub project_dir: String,
    /// One of `running`, `stopped`, `missing`, `unknown`.
    pub status: String,
    pub mount_mode: String,
    pub sets: Vec<String>,
    pub languages: Vec<String>,
    pub image: String,
    pub created_at: String,
}

/// Map a runtime status onto the stable vocabulary used by the API and CSS.
///
/// `NotFound` becomes `missing`: the box is registered in devbox state but the
/// runtime no longer has it, which is a distinct (and actionable) condition
/// from "stopped".
pub fn status_label(status: &SandboxStatus) -> &'static str {
    match status {
        SandboxStatus::Running => "running",
        SandboxStatus::Stopped => "stopped",
        SandboxStatus::NotFound => "missing",
        SandboxStatus::Unknown(_) => "unknown",
    }
}

/// Build a summary from persisted state plus an optional live status.
///
/// `None` means the probe failed or timed out, which is reported as `unknown`.
pub fn summarize(state: &SandboxState, status: Option<&SandboxStatus>) -> BoxSummary {
    BoxSummary {
        name: state.name.clone(),
        runtime: state.runtime.clone(),
        project_dir: state.project_dir.display().to_string(),
        status: status.map(status_label).unwrap_or("unknown").to_string(),
        mount_mode: state.mount_mode.clone(),
        sets: state.sets.clone(),
        languages: state.languages.clone(),
        image: state.image.clone(),
        created_at: state.created_at.clone(),
    }
}

/// Probe one box's status, degrading to `None` on error or timeout.
async fn probe_status(manager: &SandboxManager, state: &SandboxState) -> Option<SandboxStatus> {
    let runtime = manager.runtime_for_sandbox(state).ok()?;
    match tokio::time::timeout(STATUS_TIMEOUT, runtime.status(&state.name)).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            tracing::debug!(box_id = %state.name, error = %e, "status probe failed");
            None
        }
        Err(_) => {
            tracing::warn!(box_id = %state.name, "status probe timed out");
            None
        }
    }
}

/// List every registered box with its live status.
///
/// Status probes run concurrently: one slow runtime must not serialize the
/// whole dashboard behind it.
pub async fn list_boxes(manager: &Arc<SandboxManager>) -> Result<Vec<BoxSummary>> {
    let mut states = manager.list_sandboxes()?;
    states.sort_by(|a, b| a.name.cmp(&b.name));

    let probes = states.iter().map(|s| probe_status(manager, s));
    let statuses = futures::future::join_all(probes).await;

    Ok(states
        .iter()
        .zip(statuses.iter())
        .map(|(state, status)| summarize(state, status.as_ref()))
        .collect())
}

/// Fetch a single box summary by name.
pub async fn get_box(manager: &Arc<SandboxManager>, name: &str) -> Result<BoxSummary> {
    let state = manager.get_sandbox(name)?;
    let status = probe_status(manager, &state).await;
    Ok(summarize(&state, status.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state() -> SandboxState {
        SandboxState {
            name: "myapp".into(),
            runtime: "lima".into(),
            project_dir: PathBuf::from("/Users/test/code/myapp"),
            created_at: "2026-08-06T00:00:00Z".into(),
            mount_mode: "overlay".into(),
            layout: "default".into(),
            sets: vec!["system".into(), "git".into()],
            languages: vec!["rust".into()],
            image: "nixos".into(),
        }
    }

    #[test]
    fn maps_every_runtime_status() {
        assert_eq!(status_label(&SandboxStatus::Running), "running");
        assert_eq!(status_label(&SandboxStatus::Stopped), "stopped");
        assert_eq!(status_label(&SandboxStatus::NotFound), "missing");
        assert_eq!(
            status_label(&SandboxStatus::Unknown("weird".into())),
            "unknown"
        );
    }

    #[test]
    fn summarize_carries_state_through() {
        let s = summarize(&state(), Some(&SandboxStatus::Running));
        assert_eq!(s.name, "myapp");
        assert_eq!(s.runtime, "lima");
        assert_eq!(s.status, "running");
        assert_eq!(s.project_dir, "/Users/test/code/myapp");
        assert_eq!(s.sets, vec!["system", "git"]);
        assert_eq!(s.languages, vec!["rust"]);
    }

    #[test]
    fn failed_probe_reads_as_unknown() {
        let s = summarize(&state(), None);
        assert_eq!(s.status, "unknown");
    }

    #[test]
    fn summary_serializes_with_stable_field_names() {
        let s = summarize(&state(), Some(&SandboxStatus::Stopped));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["name"], "myapp");
        assert_eq!(json["status"], "stopped");
        assert_eq!(json["mount_mode"], "overlay");
    }
}
