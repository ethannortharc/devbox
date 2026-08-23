//! Reading a running fabric's provisioning state, for the console.
//!
//! The state machine's answers live behind `ztpd`'s *operator* listener, and
//! that listener binds loopback inside the service namespace on purpose: a ZTP
//! server is multi-homed by definition, so a separate port is not separation
//! (`docs/ztp.md`). The console therefore cannot dial it — it asks the
//! substrate to, over the same exec channel every other lab operation uses.
//!
//! Which is why, until now, ZTP was a thing you could watch in a terminal and
//! not in the console that exists to make a box's behaviour visible.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime::Runtime;

/// What `GET /status` on the operator listener answers.
///
/// Deserialized loosely: this is a running service's reply, and a field it
/// stops sending should degrade the view rather than fail the page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FabricStatus {
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
    #[serde(default)]
    pub healthy: usize,
    #[serde(default)]
    pub failed: usize,
    /// Serials the source of truth names, not the ones seen. A node that never
    /// booted is invisible to the registry and must still count against
    /// convergence.
    #[serde(default)]
    pub expected: usize,
    #[serde(default)]
    pub missing: Vec<String>,
    #[serde(default)]
    pub converged: bool,
    #[serde(default)]
    pub p95_secs: f64,
}

/// One node as the provisioning registry knows it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeStatus {
    pub serial: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub config_hash: String,
}

/// Where the operator listener answers, inside the service namespace.
pub const OPERATOR_STATUS_URL: &str = "http://127.0.0.1:9090/status";

/// How long to wait for a substrate to answer.
///
/// The command runs a `wget` against a service inside a namespace, and a
/// `ztpd` that accepts the connection and then says nothing leaves it — and
/// the request that started it — waiting forever.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest status reply this will parse.
///
/// The reply comes from a process inside the box, which is the side of the
/// boundary devbox exists to distrust. A fabric of a thousand nodes is well
/// under this; a reply that is not is not a status.
pub const MAX_STATUS_BYTES: usize = 4 * 1024 * 1024;

/// Nodes one fabric view will render.
pub const MAX_NODES: usize = 1_000;

/// Ask a substrate for one fabric's provisioning status.
///
/// `Ok(None)` when nothing answers: a lab that has not been brought up has no
/// ZTP server, and that is a state to render rather than an error to report.
pub async fn read(
    runtime: &dyn Runtime,
    substrate: &str,
    service_namespace: &str,
) -> Result<Option<FabricStatus>> {
    // Bounded. Everything past this point is a reply from inside the box.
    //
    // The argv is a fixed vector with the namespace as one whole element, so
    // no request value is ever interpolated into shell syntax.
    let argv = [
        "sudo",
        "ip",
        "netns",
        "exec",
        service_namespace,
        "wget",
        "-qO-",
        OPERATOR_STATUS_URL,
    ];
    let exec = runtime.exec_cmd(substrate, &argv, false);
    let Ok(result) = tokio::time::timeout(READ_TIMEOUT, exec).await else {
        // A substrate that did not answer in time is a fabric that is not
        // reporting, which is a state to render rather than an error to raise.
        tracing::debug!(
            substrate,
            seconds = READ_TIMEOUT.as_secs(),
            "ZTP status read timed out"
        );
        return Ok(None);
    };
    let result = result.context("ask the substrate for the ZTP status")?;
    if result.exit_code != 0 {
        return Ok(None);
    }
    if result.stdout.len() > MAX_STATUS_BYTES {
        tracing::warn!(
            substrate,
            bytes = result.stdout.len(),
            "ZTP status reply is too large to be a status"
        );
        return Ok(None);
    }
    Ok(serde_json::from_str(&result.stdout).ok())
}

/// One node, ready to render.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NodeView {
    pub serial: String,
    /// Empty until the node has identified itself against the source of truth.
    pub name: String,
    pub role: String,
    pub state: String,
    /// `waiting`, `working`, `healthy`, or `failed` — what colours the node.
    pub phase: &'static str,
    pub attempts: u32,
    pub reason: String,
    /// First eight characters of the config hash, or empty.
    pub config: String,
}

/// The whole fabric, ready to render.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct FabricView {
    pub nodes: Vec<NodeView>,
    pub healthy: usize,
    pub failed: usize,
    pub expected: usize,
    pub missing: Vec<String>,
    pub converged: bool,
    /// p95 provisioning time, already rounded for display.
    pub p95: String,
    /// True when a ZTP server answered at all.
    pub live: bool,
    /// True when no box could be resolved, so nothing was asked.
    ///
    /// Separate from `live`, which says a server did not answer. Collapsing
    /// the two rendered "Not running" over a lab that was running perfectly
    /// well on a box the console had simply not worked out — the console
    /// picks automatically only when there is exactly one box, and a second
    /// box is enough to make it decline. A definite negative is the one
    /// answer this must not give when the truth is that it does not know.
    pub unknown_substrate: bool,
}

/// The node a catalogued serial names, when the registry cannot say.
///
/// `devbox:<lab>:<node>` is how the ZTP plan builds them. Anything else yields
/// nothing rather than a guess: a wrong name on a topology node is worse than
/// an uncoloured one.
fn node_name_from_serial(serial: &str) -> String {
    match serial.split(':').collect::<Vec<_>>().as_slice() {
        ["devbox", _lab, node] if !node.is_empty() => (*node).to_string(),
        _ => String::new(),
    }
}

/// Which colour a state earns.
///
/// Four groups rather than seven states: the reader is asking "is it done, is
/// it moving, or is it stuck", and a palette with one entry per state answers
/// a question nobody has.
pub fn phase(state: &str) -> &'static str {
    match state {
        "healthy" => "healthy",
        "failed" => "failed",
        // Seen but not yet doing anything: it has knocked and no more.
        "discovered" => "waiting",
        // Everything between identification and health is the machine working.
        _ => "working",
    }
}

/// Build the console's view of a fabric.
///
/// Nodes the source of truth expects but has never heard from are added as
/// `missing`. They are the ones a convergence signal must not ignore — a node
/// that never boots is invisible to the registry, and a fabric reporting
/// nineteen of twenty healthy as converged is the one answer this must never
/// give.
pub fn view(status: Option<FabricStatus>) -> FabricView {
    let Some(status) = status else {
        return FabricView::default();
    };
    let mut nodes: Vec<NodeView> = status
        .nodes
        .iter()
        .take(MAX_NODES)
        .map(|node| NodeView {
            serial: node.serial.clone(),
            name: node.name.clone(),
            role: node.role.clone(),
            phase: phase(&node.state),
            state: node.state.clone(),
            attempts: node.attempts,
            reason: node.reason.clone(),
            config: node.config_hash.chars().take(8).collect(),
        })
        .collect();

    for serial in &status.missing {
        if nodes.iter().any(|node| &node.serial == serial) {
            continue;
        }
        nodes.push(NodeView {
            serial: serial.clone(),
            // Derived, because the registry has never heard from this node and
            // so has no name for it — and without one the topology cannot
            // colour it, which is exactly the node a reader is looking for.
            // The catalog builds serials as `devbox:<lab>:<node>`.
            name: node_name_from_serial(serial),
            role: String::new(),
            state: "not seen".into(),
            phase: "waiting",
            attempts: 0,
            reason: "this serial has never contacted the provisioning server".into(),
            config: String::new(),
        });
    }

    // Unhealthy first, then by name: a converged fabric reads as a list, and
    // one that is not puts what needs attention at the top.
    nodes.sort_by(|a, b| {
        let rank = |phase: &str| match phase {
            "failed" => 0,
            "waiting" => 1,
            "working" => 2,
            _ => 3,
        };
        rank(a.phase)
            .cmp(&rank(b.phase))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.serial.cmp(&b.serial))
    });

    FabricView {
        nodes,
        healthy: status.healthy,
        failed: status.failed,
        expected: status.expected,
        missing: status.missing,
        converged: status.converged,
        p95: if status.p95_secs > 0.0 {
            format!("{:.1}s", status.p95_secs)
        } else {
            String::new()
        },
        live: true,
        unknown_substrate: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: &str) -> Option<FabricStatus> {
        serde_json::from_str(json).ok()
    }

    #[test]
    fn every_state_lands_in_one_of_four_groups() {
        assert_eq!(phase("healthy"), "healthy");
        assert_eq!(phase("failed"), "failed");
        assert_eq!(phase("discovered"), "waiting");
        for working in ["identified", "rendering", "pushing", "verifying"] {
            assert_eq!(phase(working), "working", "{working}");
        }
        // A state this build has never heard of is the machine working, not a
        // failure: an older or newer ztpd must not paint the fabric red.
        assert_eq!(phase("something-new"), "working");
    }

    #[test]
    fn a_lab_that_was_never_brought_up_renders_as_not_live() {
        let view = view(None);
        assert!(!view.live);
        assert!(!view.converged);
        assert!(view.nodes.is_empty());
    }

    #[test]
    fn a_serial_that_never_booted_is_shown_rather_than_omitted() {
        // The registry only knows nodes that have identified. Rendering only
        // those is how a fabric with a dead node reads as complete.
        let view = view(status(
            r#"{"nodes":[{"serial":"SN-1","name":"leaf1","state":"healthy"}],
                "healthy":1,"failed":0,"expected":2,"missing":["SN-2"],
                "converged":false,"p95_secs":12.5}"#,
        ));
        assert_eq!(view.nodes.len(), 2);
        let ghost = view.nodes.iter().find(|n| n.serial == "SN-2").unwrap();
        assert_eq!(ghost.phase, "waiting");
        assert!(ghost.reason.contains("never contacted"));
        // Named from the serial, so the topology can still colour it.
        assert_eq!(ghost.name, "");
        assert!(!view.converged);
        assert_eq!(view.p95, "12.5s");
    }

    #[test]
    fn what_needs_attention_sorts_to_the_top() {
        let view = view(status(
            r#"{"nodes":[
                 {"serial":"SN-3","name":"leaf3","state":"healthy"},
                 {"serial":"SN-1","name":"leaf1","state":"failed","reason":"no route"},
                 {"serial":"SN-2","name":"leaf2","state":"pushing"}],
               "healthy":1,"failed":1,"expected":3,"missing":[],"converged":false}"#,
        ));
        assert_eq!(view.nodes[0].name, "leaf1", "a failure reads first");
        assert_eq!(view.nodes.last().unwrap().name, "leaf3");
    }

    #[test]
    fn a_catalogued_serial_names_its_node_even_when_unheard_from() {
        let catalogued = view(status(
            r#"{"nodes":[],"healthy":0,"expected":1,
                "missing":["devbox:ztp-fabric:leaf4"],"converged":false}"#,
        ));
        assert_eq!(catalogued.nodes[0].name, "leaf4");

        // A serial in some other shape yields nothing rather than a guess: a
        // wrong name on a topology node is worse than an uncoloured one.
        let opaque = view(status(
            r#"{"nodes":[],"missing":["SN-ACME-0042"],"converged":false}"#,
        ));
        assert_eq!(opaque.nodes[0].name, "");
    }

    #[test]
    fn a_reply_missing_fields_degrades_instead_of_failing() {
        // A running service's answer, and a field it stops sending should cost
        // a column rather than the page.
        let view = view(status(r#"{"nodes":[{"serial":"SN-1","state":"healthy"}]}"#));
        assert!(view.live);
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].name, "");
        assert_eq!(view.p95, "");
    }
}
