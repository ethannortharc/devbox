//! Box lab — multi-machine networking (§9).
//!
//! A lab is many nodes wired into a real virtual network. Crucially it is
//! **not** N heavyweight VMs: nodes are network namespaces inside one Linux
//! substrate, joined by veth pairs (§4). Dozens start in seconds on one
//! kernel, the networking is genuine L2/L3, and one `devbox-obsd` sees the
//! whole lab.
//!
//! The pipeline is a chain of pure transformations, each independently
//! testable, with exactly one impure step at the end:
//!
//! ```text
//! lab.toml → Topology → Plan (ipam) → commands (wiring) + configs (frr)
//!                                              ↓
//!                                     run inside the substrate
//! ```

pub mod fault;
pub mod frr;
pub mod ipam;
pub mod scenarios;
pub mod straggler;
pub mod topology;
pub mod wiring;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;

use self::ipam::Plan;
use self::topology::Topology;

/// A lab, resolved and planned, ready to bring up.
#[derive(Debug, Clone)]
pub struct Lab {
    pub topology: Topology,
    pub plan: Plan,
}

impl Lab {
    /// Resolve a scenario name or a `lab.toml` path into a planned lab.
    pub fn resolve(arg: &str) -> Result<Self> {
        let topology = scenarios::resolve(arg)?;
        Self::from_topology(topology)
    }

    /// Plan an already-parsed topology.
    pub fn from_topology(topology: Topology) -> Result<Self> {
        topology.validate()?;
        let plan = ipam::allocate(&topology).context("failed to allocate the lab address plan")?;
        Ok(Self { topology, plan })
    }

    pub fn name(&self) -> &str {
        &self.topology.lab.name
    }

    /// Commands that bring the lab up inside the substrate.
    pub fn up_commands(&self) -> Result<Vec<Vec<String>>> {
        wiring::up_commands(&self.topology, &self.plan)
    }

    /// Commands that tear it down.
    pub fn down_commands(&self) -> Vec<Vec<String>> {
        wiring::down_commands(&self.topology)
    }

    /// Per-router `frr.conf`.
    pub fn router_configs(&self) -> Vec<(String, String)> {
        frr::render_all(&self.topology, &self.plan)
    }

    /// Every node's reachable address — its loopback for a router, its first
    /// interface address otherwise.
    ///
    /// This is what the reachability matrix pings: a loopback proves *routing*
    /// works, where an interface address only proves the wire does.
    pub fn reachable_addrs(&self) -> BTreeMap<String, std::net::Ipv4Addr> {
        self.topology
            .nodes
            .iter()
            .filter_map(|node| {
                let addr = self
                    .plan
                    .loopbacks
                    .get(&node.name)
                    .copied()
                    .or_else(|| self.plan.addrs_of(&node.name).first().map(|a| a.addr))?;
                Some((node.name.clone(), addr))
            })
            .collect()
    }

    /// Every ordered pair of nodes that should be able to reach each other.
    pub fn reachability_pairs(&self) -> Vec<(String, String)> {
        let names: Vec<String> = self.reachable_addrs().keys().cloned().collect();
        let mut pairs = Vec::new();
        for from in &names {
            for to in &names {
                if from != to {
                    pairs.push((from.clone(), to.clone()));
                }
            }
        }
        pairs
    }

    /// The commands that measure reachability, one per ordered pair.
    pub fn reachability_commands(&self) -> Vec<(String, String, Vec<String>)> {
        let addrs = self.reachable_addrs();
        self.reachability_pairs()
            .into_iter()
            .filter_map(|(from, to)| {
                let addr = addrs.get(&to)?;
                Some((
                    from.clone(),
                    to.clone(),
                    wiring::ping(self.name(), &from, addr),
                ))
            })
            .collect()
    }

    /// A summary of the lab, for `devbox lab status` and the console.
    pub fn summary(&self) -> Summary {
        Summary {
            name: self.name().to_string(),
            substrate: self.topology.lab.substrate.clone(),
            nodes: self
                .topology
                .nodes
                .iter()
                .map(|n| NodeSummary {
                    name: n.name.clone(),
                    role: n.role.to_string(),
                    asn: self.plan.asns.get(&n.name).copied(),
                    loopback: self.plan.loopbacks.get(&n.name).map(|a| a.to_string()),
                    interfaces: self
                        .plan
                        .addrs_of(&n.name)
                        .into_iter()
                        .map(|a| InterfaceSummary {
                            name: a.iface.clone(),
                            addr: a.cidr(),
                            peer: self
                                .plan
                                .peer_of(&n.name, &a.iface)
                                .map(|p| format!("{}:{}", p.node, p.iface)),
                        })
                        .collect(),
                })
                .collect(),
            links: self
                .plan
                .links
                .iter()
                .map(|l| LinkSummary {
                    subnet: l.subnet.clone(),
                    a: format!("{}:{}", l.a.node, l.a.iface),
                    b: format!("{}:{}", l.b.node, l.b.iface),
                })
                .collect(),
        }
    }
}

/// A lab as the CLI and the console present it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Summary {
    pub name: String,
    pub substrate: String,
    pub nodes: Vec<NodeSummary>,
    pub links: Vec<LinkSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NodeSummary {
    pub name: String,
    pub role: String,
    pub asn: Option<u32>,
    pub loopback: Option<String>,
    pub interfaces: Vec<InterfaceSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InterfaceSummary {
    pub name: String,
    pub addr: String,
    pub peer: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LinkSummary {
    pub subnet: String,
    pub a: String,
    pub b: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_and_plans_a_scenario() {
        let lab = Lab::resolve("clos-3node").unwrap();
        assert_eq!(lab.name(), "clos-3node");
        assert_eq!(lab.plan.links.len(), 2);
        assert_eq!(lab.router_configs().len(), 3);
        assert!(!lab.up_commands().unwrap().is_empty());
        // Two per node: stop the daemons, then delete the namespace.
        assert_eq!(lab.down_commands().len(), 6);
    }

    #[test]
    fn routers_are_reachable_by_loopback_not_by_interface() {
        // A loopback ping proves routing works; an interface ping only proves
        // the wire does.
        let lab = Lab::resolve("clos-3node").unwrap();
        let addrs = lab.reachable_addrs();

        assert_eq!(addrs.len(), 3);
        for (node, addr) in &addrs {
            assert_eq!(
                Some(addr),
                lab.plan.loopbacks.get(node),
                "{node} should be reached at its loopback"
            );
        }
    }

    #[test]
    fn a_host_falls_back_to_its_interface_address() {
        let lab = Lab::resolve("client-proxy-server").unwrap();
        let addrs = lab.reachable_addrs();

        assert!(addrs.contains_key("client"), "a host is still reachable");
        assert!(!lab.plan.loopbacks.contains_key("client"));
    }

    #[test]
    fn the_reachability_matrix_covers_every_ordered_pair() {
        let lab = Lab::resolve("clos-3node").unwrap();
        let pairs = lab.reachability_pairs();

        // 3 nodes → 3 × 2 ordered pairs.
        assert_eq!(pairs.len(), 6);
        assert!(pairs.iter().all(|(a, b)| a != b), "no node pings itself");
        assert!(pairs.contains(&("leaf1".into(), "leaf2".into())));
        assert!(pairs.contains(&("leaf2".into(), "leaf1".into())));
    }

    #[test]
    fn reachability_commands_target_the_right_addresses() {
        let lab = Lab::resolve("clos-3node").unwrap();
        let cmds = lab.reachability_commands();

        assert_eq!(cmds.len(), 6);
        for (from, to, cmd) in &cmds {
            let text = wiring::render(cmd);
            assert!(text.contains(&format!("devbox-clos-3node-{from}")));
            let addr = lab.reachable_addrs()[to].to_string();
            assert!(text.ends_with(&addr), "{text} should ping {to} at {addr}");
        }
    }

    #[test]
    fn the_summary_describes_the_whole_lab() {
        let lab = Lab::resolve("clos-3node").unwrap();
        let s = lab.summary();

        assert_eq!(s.name, "clos-3node");
        assert_eq!(s.nodes.len(), 3);
        assert_eq!(s.links.len(), 2);

        let spine = s.nodes.iter().find(|n| n.name == "spine1").unwrap();
        assert_eq!(spine.role, "frr-router");
        assert!(spine.asn.is_some());
        assert!(spine.loopback.is_some());
        assert_eq!(spine.interfaces.len(), 2);
        assert_eq!(
            spine.interfaces[0].peer.as_deref(),
            Some("leaf1:eth1"),
            "each interface names what is on the other end"
        );
    }

    #[test]
    fn a_summary_serializes_for_the_api() {
        let lab = Lab::resolve("clos-3node").unwrap();
        let json = serde_json::to_value(lab.summary()).unwrap();

        assert_eq!(json["name"], "clos-3node");
        assert_eq!(json["nodes"][0]["role"], "frr-router");
        assert!(json["links"][0]["subnet"].is_string());
    }

    #[test]
    fn an_invalid_topology_never_becomes_a_lab() {
        let broken = topology::Topology::from_toml(
            "[lab]\nname = \"x\"\n[[nodes]]\nname = \"a\"\n[[nodes]]\nname = \"b\"\n",
        );
        // Validation catches the unconnected node at parse time.
        assert!(broken.is_err());
    }

    #[test]
    fn every_scenario_produces_a_coherent_lab() {
        for scenario in scenarios::SCENARIOS {
            let lab = Lab::resolve(scenario.name)
                .unwrap_or_else(|e| panic!("{} does not resolve: {e}", scenario.name));

            let summary = lab.summary();
            assert_eq!(summary.nodes.len(), lab.topology.nodes.len());
            assert!(
                !lab.reachability_pairs().is_empty() || lab.topology.nodes.len() < 2,
                "{} has nothing to check reachability between",
                scenario.name
            );
        }
    }
}
