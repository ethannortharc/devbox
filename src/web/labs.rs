//! Lab console view models: planned topology plus live stored traffic.

use std::collections::BTreeMap;
use std::f64::consts::TAU;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::lab::{Lab, scenarios};
use crate::obs::store::Query;
use crate::sandbox::SandboxManager;

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioCard {
    pub name: &'static str,
    pub summary: &'static str,
    pub demonstrates: &'static str,
    pub nodes: usize,
    pub links: usize,
}

pub fn scenarios() -> Vec<ScenarioCard> {
    scenarios::SCENARIOS
        .iter()
        .map(|scenario| {
            let topology = scenarios::load(scenario.name).expect("embedded scenario is valid");
            ScenarioCard {
                name: scenario.name,
                summary: scenario.summary,
                demonstrates: scenario.demonstrates,
                nodes: topology.nodes.len(),
                links: topology.links.len(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct LabView {
    pub name: String,
    pub summary: &'static str,
    pub demonstrates: &'static str,
    pub substrate: String,
    pub nodes: Vec<NodeView>,
    pub links: Vec<LinkView>,
    pub total_events: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub name: String,
    pub role: String,
    pub asn: Option<u32>,
    pub loopback: Option<String>,
    pub x: i32,
    pub y: i32,
    /// Provisioning phase for a blank node, empty for everything else.
    ///
    /// The point of the whole scenario is watching a node with no
    /// configuration acquire one, and a topology that draws it identically
    /// throughout shows nothing happening.
    pub phase: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkView {
    pub key: String,
    pub a: String,
    pub b: String,
    pub subnet: String,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub label_x: i32,
    pub label_y: i32,
    pub events: u64,
    pub bytes: u64,
}

/// Read a fabric's provisioning state from the substrate that runs it.
///
/// `None` for a scenario with no ZTP in it, or for one that has not been
/// brought up. The status lives behind an operator listener bound to loopback
/// inside the service namespace, so this asks the substrate to fetch it (see
/// [`crate::lab::ztp_status`]).
pub async fn ztp(
    manager: &Arc<SandboxManager>,
    name: &str,
    substrate: Option<&str>,
) -> Result<Option<crate::lab::ztp_status::FabricView>> {
    // Built-in scenarios only, as every other web route into the lab code
    // does. `scenarios::resolve` falls back to *reading a file* for a name it
    // does not recognise — which the CLI wants and a request must never get:
    // `/api/labs/%2Fdev%2Fzero/ztp` would have been a read that never returns.
    if scenarios::find(name).is_none() {
        return Ok(None);
    }
    let lab = Lab::resolve(name)?;
    let Some(plan) = crate::lab::services::ZtpPlan::from_lab(&lab)? else {
        // Not a ZTP scenario. Nothing to show, and nothing wrong.
        return Ok(None);
    };

    // The box the lab was brought up on.
    //
    // An empty choice means "auto", which is what the control above the page
    // offers and what `lab up` acts on — so an empty one has to be resolved
    // the same way here, or a lab brought up on an automatically chosen box
    // reports itself as never running.
    let substrate = match substrate.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => match auto_substrate(manager, &lab) {
            Some(name) => name,
            None => {
                return Ok(Some(crate::lab::ztp_status::FabricView {
                    unknown_substrate: true,
                    ..Default::default()
                }));
            }
        },
    };
    let substrate = substrate.as_str();
    let state = manager.get_sandbox(substrate)?;
    let runtime = manager.runtime_for_sandbox(&state)?;
    let namespace = crate::lab::services::namespace(lab.name(), &plan.service.node);

    let status = crate::lab::ztp_status::read(runtime.as_ref(), substrate, &namespace).await?;
    Ok(Some(crate::lab::ztp_status::view(status)))
}

/// The box a lab would be brought up on when nobody names one.
///
/// A narrower rule than the CLI's `resolve_substrate`: the topology's own
/// setting when it names a box, otherwise the only registered box if there is
/// exactly one. Ambiguity yields nothing — a status page guessing which of two
/// boxes to reach into is worse than one saying it does not know.
fn auto_substrate(manager: &Arc<SandboxManager>, lab: &Lab) -> Option<String> {
    const RUNTIME_KINDS: &[&str] = &["lima", "incus", "multipass", "docker", "host", "auto"];

    let configured = lab.summary().substrate;
    if !configured.is_empty() && !RUNTIME_KINDS.contains(&configured.as_str()) {
        return Some(configured);
    }
    let mut boxes = manager.list_sandboxes().ok()?;
    match boxes.len() {
        1 => Some(boxes.remove(0).name),
        _ => None,
    }
}

/// Colour a topology's nodes by what provisioning has done to them.
///
/// Kept apart from [`load`], which is pure and synchronous: the phases come
/// from a service inside the substrate, and a topology that cannot be drawn
/// without reaching one would be a topology nobody could see before bringing
/// the lab up.
pub fn apply_phases(view: &mut LabView, fabric: &crate::lab::ztp_status::FabricView) {
    for node in &mut view.nodes {
        if let Some(status) = fabric.nodes.iter().find(|status| status.name == node.name) {
            node.phase = status.phase;
        }
    }
}

/// Build the topology and enrich each link with traffic in the local stores.
pub fn load(manager: &Arc<SandboxManager>, name: &str) -> Result<LabView> {
    let scenario = scenarios::find(name)
        .ok_or_else(|| anyhow::anyhow!("unknown built-in scenario '{name}'"))?;
    let lab = Lab::resolve(name)?;
    let positions = positions(&lab);
    let traffic = traffic(manager, &lab);

    let nodes = lab
        .summary()
        .nodes
        .into_iter()
        .map(|node| {
            let (x, y) = positions[&node.name];
            NodeView {
                name: node.name,
                role: node.role,
                asn: node.asn,
                loopback: node.loopback,
                x,
                y,
                phase: "",
            }
        })
        .collect();
    let links = lab
        .plan
        .links
        .iter()
        .map(|link| {
            let (x1, y1) = positions[&link.a.node];
            let (x2, y2) = positions[&link.b.node];
            let (events, bytes) = traffic
                .get(&(link.a.addr.to_string(), link.b.addr.to_string()))
                .copied()
                .unwrap_or_default();
            LinkView {
                key: format!("{}-{}", link.a.node, link.b.node),
                a: format!("{}:{}", link.a.node, link.a.iface),
                b: format!("{}:{}", link.b.node, link.b.iface),
                subnet: link.subnet.clone(),
                x1,
                y1,
                x2,
                y2,
                label_x: (x1 + x2) / 2,
                label_y: (y1 + y2) / 2 - 8,
                events,
                bytes,
            }
        })
        .collect::<Vec<_>>();
    let total_events = links.iter().map(|link| link.events).sum();
    let total_bytes = links.iter().map(|link| link.bytes).sum();

    Ok(LabView {
        name: name.to_string(),
        summary: scenario.summary,
        demonstrates: scenario.demonstrates,
        substrate: lab.topology.lab.substrate,
        nodes,
        links,
        total_events,
        total_bytes,
    })
}

fn positions(lab: &Lab) -> BTreeMap<String, (i32, i32)> {
    let count = lab.topology.nodes.len().max(1) as f64;
    lab.topology
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let angle = index as f64 * TAU / count - TAU / 4.0;
            let x = 400.0 + angle.cos() * 285.0;
            let y = 190.0 + angle.sin() * 125.0;
            (node.name.clone(), (x.round() as i32, y.round() as i32))
        })
        .collect()
}

fn traffic(manager: &Arc<SandboxManager>, lab: &Lab) -> BTreeMap<(String, String), (u64, u64)> {
    let mut by_link: BTreeMap<(String, String), (u64, u64)> = lab
        .plan
        .links
        .iter()
        .map(|link| ((link.a.addr.to_string(), link.b.addr.to_string()), (0, 0)))
        .collect();

    let Ok(boxes) = manager.list_sandboxes() else {
        return by_link;
    };
    for state in boxes {
        let path = crate::obs::collector::store_path(&manager.state_dir, &state.name);
        if !path.exists() {
            continue;
        }
        let Ok(store) = crate::obs::Store::open(&path) else {
            continue;
        };
        let Ok(events) = store.query(&Query {
            limit: Some(5_000),
            newest_first: true,
            ..Default::default()
        }) else {
            continue;
        };
        for event in events {
            let Some(net) = event.net else { continue };
            for ((a, b), (count, bytes)) in &mut by_link {
                let touches =
                    (net.saddr == *a || net.saddr == *b) && (net.daddr == *a || net.daddr == *b);
                if touches {
                    *count += 1;
                    *bytes = bytes.saturating_add(net.bytes_tx.saturating_add(net.bytes_rx));
                    break;
                }
            }
        }
    }
    by_link
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_becomes_a_graph_inside_the_canvas() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        for scenario in scenarios::SCENARIOS {
            let view = load(&manager, scenario.name).unwrap();
            assert!(!view.nodes.is_empty());
            assert!(!view.links.is_empty());
            assert!(view.nodes.iter().all(|node| (0..=800).contains(&node.x)));
            assert!(view.nodes.iter().all(|node| (0..=380).contains(&node.y)));
        }
    }

    #[test]
    fn scenario_picker_has_the_whole_library() {
        assert_eq!(scenarios().len(), crate::lab::scenarios::SCENARIOS.len());
    }
}
