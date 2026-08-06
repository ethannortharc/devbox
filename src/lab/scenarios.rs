//! The scenario library — §9.4.
//!
//! Prebuilt topologies, each `devbox lab up <name>`. They are embedded rather
//! than shipped as files so `devbox lab up clos-3node` works from a clean
//! install with nothing to fetch.

use anyhow::{Result, bail};

use super::topology::Topology;

/// A named, ready-to-run topology.
pub struct Scenario {
    pub name: &'static str,
    /// One line for `devbox lab list`.
    pub summary: &'static str,
    /// What it demonstrates, for the docs and the console.
    pub demonstrates: &'static str,
    pub toml: &'static str,
}

/// Every built-in scenario (§9.4).
pub static SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "clos-3node",
        summary: "eBGP-unnumbered leaf/spine, ECMP, reachability",
        demonstrates: "The smallest fabric that is still a fabric: two leaves, \
                       one spine, every router its own AS, routes learned over \
                       unnumbered peering.",
        toml: include_str!("scenarios/clos-3node.toml"),
    },
    Scenario {
        name: "fat-tree-4x2",
        summary: "Two-tier fat-tree; the straggler demo lives here",
        demonstrates: "Four leaves, two spines, every leaf dual-homed. Enough \
                       ECMP to make a single lossy link visible as a straggler \
                       rather than a uniform slowdown.",
        toml: include_str!("scenarios/fat-tree-4x2.toml"),
    },
    Scenario {
        name: "partition-3",
        summary: "Three-node cluster; partition and heal",
        demonstrates: "A triangle, so cutting one link leaves a path and \
                       cutting two isolates a node. Reconvergence is \
                       observable in seconds.",
        toml: include_str!("scenarios/partition-3.toml"),
    },
    Scenario {
        name: "client-proxy-server",
        summary: "Egress and observability through a proxy hop",
        demonstrates: "The shape every policy question has: a client that \
                       cannot reach the server directly, and a middle box that \
                       can.",
        toml: include_str!("scenarios/client-proxy-server.toml"),
    },
    Scenario {
        name: "wan-lossy",
        summary: "Two sites across a high-latency, lossy link",
        demonstrates: "A link worth injecting delay and loss into, with a \
                       router at each end so the failure is routing-visible.",
        toml: include_str!("scenarios/wan-lossy.toml"),
    },
    Scenario {
        name: "ztp-fabric",
        summary: "Blank nodes provision themselves (the §10 flagship)",
        demonstrates: "A ZTP server, a DHCP/DNS service node, and blank nodes \
                       that boot with no configuration and converge with zero \
                       manual steps.",
        toml: include_str!("scenarios/ztp-fabric.toml"),
    },
];

/// Look up a scenario by name.
pub fn find(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|s| s.name == name)
}

/// Load a scenario's topology.
pub fn load(name: &str) -> Result<Topology> {
    let Some(scenario) = find(name) else {
        bail!(
            "unknown scenario '{name}'. Available: {}",
            SCENARIOS
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    Topology::from_toml(scenario.toml)
}

/// Resolve a `lab up` argument: a scenario name, or a path to a `lab.toml`.
pub fn resolve(arg: &str) -> Result<Topology> {
    if find(arg).is_some() {
        return load(arg);
    }
    let path = std::path::Path::new(arg);
    if path.exists() {
        return Topology::load(path);
    }
    bail!(
        "'{arg}' is neither a built-in scenario nor a file. Built-ins: {}",
        SCENARIOS
            .iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::{frr, ipam, wiring};

    #[test]
    fn every_scenario_from_the_design_is_present() {
        // §9.4 names these six.
        for name in [
            "clos-3node",
            "fat-tree-4x2",
            "partition-3",
            "client-proxy-server",
            "wan-lossy",
            "ztp-fabric",
        ] {
            assert!(find(name).is_some(), "{name} is missing from the library");
        }
    }

    #[test]
    fn every_scenario_parses_validates_and_allocates() {
        // The whole point of a library is that `devbox lab up <name>` works.
        // A scenario that does not even allocate is worse than no scenario.
        for scenario in SCENARIOS {
            let topology = load(scenario.name)
                .unwrap_or_else(|e| panic!("{} does not load: {e}", scenario.name));

            assert_eq!(topology.lab.name, scenario.name, "the lab names itself");

            let plan = ipam::allocate(&topology)
                .unwrap_or_else(|e| panic!("{} does not allocate: {e}", scenario.name));
            ipam::verify(&plan)
                .unwrap_or_else(|e| panic!("{} has an overlapping plan: {e}", scenario.name));

            wiring::up_commands(&topology, &plan)
                .unwrap_or_else(|e| panic!("{} does not wire: {e}", scenario.name));
        }
    }

    #[test]
    fn every_router_in_every_scenario_gets_a_config() {
        for scenario in SCENARIOS {
            let topology = load(scenario.name).unwrap();
            let plan = ipam::allocate(&topology).unwrap();
            let configs = frr::render_all(&topology, &plan);

            assert_eq!(
                configs.len(),
                topology.routers().len(),
                "{} has routers without a config",
                scenario.name
            );
            for (node, conf) in configs {
                assert!(
                    conf.contains("router bgp"),
                    "{}/{node} has no BGP block",
                    scenario.name
                );
            }
        }
    }

    #[test]
    fn every_scenario_documents_itself() {
        for s in SCENARIOS {
            assert!(!s.summary.is_empty(), "{} has no summary", s.name);
            assert!(
                s.demonstrates.len() > 40,
                "{} should say what it is for",
                s.name
            );
        }
    }

    #[test]
    fn clos_is_the_design_example() {
        let t = load("clos-3node").unwrap();
        assert_eq!(t.nodes.len(), 3);
        assert_eq!(t.links.len(), 2);
        assert_eq!(t.routers().len(), 3);
    }

    #[test]
    fn fat_tree_is_dual_homed_which_is_what_makes_ecmp_visible() {
        let t = load("fat-tree-4x2").unwrap();
        let leaves: Vec<_> = t
            .nodes
            .iter()
            .filter(|n| n.name.starts_with("leaf"))
            .collect();
        assert_eq!(leaves.len(), 4);

        for leaf in leaves {
            assert_eq!(
                t.interfaces_of(&leaf.name)
                    .iter()
                    .filter(|i| i.starts_with("eth"))
                    .count(),
                2,
                "{} must reach both spines, or there is no ECMP to lose",
                leaf.name
            );
        }
    }

    #[test]
    fn partition_3_is_a_triangle_so_one_cut_still_leaves_a_path() {
        let t = load("partition-3").unwrap();
        assert_eq!(t.nodes.len(), 3);
        assert_eq!(t.adjacencies().len(), 3, "a triangle, not a chain");
    }

    #[test]
    fn ztp_fabric_has_blank_nodes_and_a_server() {
        let t = load("ztp-fabric").unwrap();

        let blank = t.nodes.iter().filter(|n| n.role.blank()).count();
        assert!(blank >= 2, "a ZTP demo needs nodes that boot with nothing");

        assert!(
            t.nodes
                .iter()
                .any(|n| n.role == crate::lab::topology::Role::Service),
            "and something to provision them from"
        );
        assert!(t.services.dhcp, "DHCP options 66/67 are how ZTP starts");
    }

    #[test]
    fn an_unknown_scenario_lists_the_real_ones() {
        let err = load("nonsense").unwrap_err().to_string();
        assert!(err.contains("clos-3node"), "{err}");
    }

    #[test]
    fn resolve_takes_a_name_or_a_path() {
        assert!(resolve("clos-3node").is_ok());
        assert!(resolve("no-such-thing").is_err());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.toml");
        std::fs::write(&path, find("clos-3node").unwrap().toml).unwrap();
        assert!(resolve(&path.display().to_string()).is_ok());
    }
}
