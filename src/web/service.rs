//! Control-plane service layer.
//!
//! The web console and the CLI are two clients of one control plane (§6.4 of
//! the v4 design). Everything reusable between them lives here: view models,
//! status mapping, and the concurrent status fan-out. HTTP handlers stay thin
//! and this layer stays testable without a server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;
use crate::sandbox::overlay::{ChangeStatus, OverlayChange};
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

// ── lifecycle ────────────────────────────────────────────

/// Start a box, or do nothing if it is already running.
///
/// Idempotent on purpose: the console calls this both from an explicit Start
/// button and from lazy-start when a Terminal tab is opened (§6.3), and those
/// can race.
pub async fn start_box(manager: &Arc<SandboxManager>, name: &str) -> Result<()> {
    let state = manager.get_sandbox(name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    match runtime.status(name).await? {
        SandboxStatus::Running => Ok(()),
        SandboxStatus::Stopped => runtime
            .start(name)
            .await
            .with_context(|| format!("failed to start box '{name}'")),
        SandboxStatus::NotFound => bail!(
            "box '{name}' is registered but runtime '{}' does not have it; \
             run `devbox destroy {name}` to clean up the stale entry",
            state.runtime
        ),
        SandboxStatus::Unknown(s) => bail!("box '{name}' is in an unknown state: {s}"),
    }
}

/// Stop a box, or do nothing if it is not running.
pub async fn stop_box(manager: &Arc<SandboxManager>, name: &str) -> Result<()> {
    let state = manager.get_sandbox(name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    match runtime.status(name).await? {
        SandboxStatus::Running => runtime
            .stop(name)
            .await
            .with_context(|| format!("failed to stop box '{name}'")),
        // Already stopped, or gone from the runtime: either way there is
        // nothing to stop, and reporting an error would make the button lie.
        _ => Ok(()),
    }
}

/// Destroy a box permanently.
///
/// Delegates to the manager so the uncommitted-overlay-changes guard applies
/// exactly as it does on the CLI — the console must not be a way to lose work
/// the CLI would have protected.
pub async fn destroy_box(manager: &Arc<SandboxManager>, name: &str, force: bool) -> Result<()> {
    manager.destroy_sandbox(name, force).await
}

/// Start the box if needed, so a view that requires a live box can open it.
pub async fn ensure_running(manager: &Arc<SandboxManager>, name: &str) -> Result<()> {
    start_box(manager, name).await
}

/// Shells the terminal will try, best first.
///
/// Probed rather than assumed: NixOS boxes built with the `shell` set have
/// zsh, minimal ones only have bash, and a bare container image may have
/// nothing but `sh`. Landing the user in a shell that does not exist is a
/// confusing first impression, and `sh` always exists.
pub const SHELL_PREFERENCE: &[&str] = &["zsh", "bash", "sh"];

/// Pick the first shell in `SHELL_PREFERENCE` that the box actually has.
pub async fn detect_shell(runtime: &dyn crate::runtime::Runtime, name: &str) -> &'static str {
    for shell in SHELL_PREFERENCE {
        let found = runtime
            .exec_cmd(name, &["which", shell], false)
            .await
            .map(|r| r.exit_code == 0)
            .unwrap_or(false);
        if found {
            return shell;
        }
    }
    // Nothing answered; `sh` is the least bad guess and the POSIX guarantee.
    "sh"
}

/// Host-side argv that opens an interactive shell inside a box.
pub async fn terminal_argv(manager: &Arc<SandboxManager>, name: &str) -> Result<Vec<String>> {
    let state = manager.get_sandbox(name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    let shell = detect_shell(runtime.as_ref(), name).await;
    Ok(runtime.interactive_argv(name, &[shell, "-l"]))
}

// ── overlay (the Files tab) ──────────────────────────────

/// One overlay change, as the console presents it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileChange {
    /// `added`, `modified`, or `deleted`.
    pub status: String,
    pub path: String,
    pub is_dir: bool,
}

/// Stable vocabulary for an overlay change status.
pub fn change_label(status: &ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "added",
        ChangeStatus::Modified => "modified",
        ChangeStatus::Deleted => "deleted",
    }
}

impl From<&OverlayChange> for FileChange {
    fn from(c: &OverlayChange) -> Self {
        Self {
            status: change_label(&c.status).to_string(),
            path: c.path.clone(),
            is_dir: c.is_dir,
        }
    }
}

/// List uncommitted overlay changes for a box.
///
/// A diff is only possible in overlay mount mode, with a reachable runtime,
/// and while the box is running. Every other case answers "nothing to show"
/// rather than erroring: the Files tab is informational, and a stopped box
/// showing an empty list is the truth, not a failure. An error is reserved
/// for a box that *should* have been diffable and was not.
pub async fn list_changes(manager: &Arc<SandboxManager>, name: &str) -> Result<Vec<FileChange>> {
    let state = manager.get_sandbox(name)?;
    if state.mount_mode != "overlay" {
        return Ok(vec![]);
    }

    let Ok(runtime) = manager.runtime_for_sandbox(&state) else {
        tracing::debug!(box_id = %name, runtime = %state.runtime, "runtime unavailable");
        return Ok(vec![]);
    };

    match runtime.status(name).await {
        Ok(SandboxStatus::Running) => {}
        Ok(_) => return Ok(vec![]),
        Err(e) => {
            tracing::debug!(box_id = %name, error = %e, "status probe failed");
            return Ok(vec![]);
        }
    }

    match crate::sandbox::overlay::diff(runtime.as_ref(), name).await {
        Ok(changes) => Ok(changes.iter().map(FileChange::from).collect()),
        // A box whose overlay was never provisioned — a failed or skipped
        // provision, or a bare image — has no upper layer to scan. That is a
        // "nothing to show" state, not a server error.
        Err(e) => {
            tracing::debug!(box_id = %name, error = %e, "overlay diff unavailable");
            Ok(vec![])
        }
    }
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
    fn maps_every_overlay_change_status() {
        assert_eq!(change_label(&ChangeStatus::Added), "added");
        assert_eq!(change_label(&ChangeStatus::Modified), "modified");
        assert_eq!(change_label(&ChangeStatus::Deleted), "deleted");
    }

    #[test]
    fn overlay_change_converts_to_a_view_model() {
        let c = OverlayChange {
            status: ChangeStatus::Modified,
            path: "src/main.rs".into(),
            is_dir: false,
        };
        let fc = FileChange::from(&c);
        assert_eq!(fc.status, "modified");
        assert_eq!(fc.path, "src/main.rs");
        assert!(!fc.is_dir);
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
