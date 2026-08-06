//! Wiring — turning a topology into `ip` commands (§9.2).
//!
//! Lab nodes are network namespaces inside one Linux substrate, joined by veth
//! pairs (§4, "substrate model"). This module generates the exact command
//! sequence that builds and tears down that wiring; the orchestrator runs it
//! inside the substrate.
//!
//! Generating commands rather than shelling out inline is what makes the
//! interesting part testable: `devbox lab up --dry-run` prints precisely what
//! would run, and the tests assert the ordering constraints that actually
//! break real setups (address before link-up, namespace before interface).

use anyhow::Result;

use super::ipam::Plan;
use super::topology::Topology;

/// Prefix for every namespace devbox creates, so a stray `ip netns` listing
/// makes it obvious what owns them.
pub const NS_PREFIX: &str = "devbox-";

/// Namespace name for a lab node.
pub fn netns(lab: &str, node: &str) -> String {
    format!("{NS_PREFIX}{lab}-{node}")
}

/// Host-side name for one end of a veth pair.
///
/// Linux caps interface names at 15 characters, so the visible half keeps the
/// readable name and the host half is a hash-free truncation — collisions are
/// prevented by the topology validator, which already rejects duplicate
/// node names and reused interfaces.
pub fn veth_name(node: &str, iface: &str) -> String {
    let raw = format!("{node}-{iface}");
    if raw.len() <= 15 {
        raw
    } else {
        raw[..15].to_string()
    }
}

/// The commands that bring a lab's wiring up.
///
/// Ordering is the whole point:
/// 1. namespaces, before anything can be moved into them;
/// 2. veth pairs, created in the root namespace;
/// 3. each end moved into its node's namespace;
/// 4. addresses assigned;
/// 5. interfaces brought up **last**, so nothing is carrying traffic while it
///    is still half-configured.
pub fn up_commands(topology: &Topology, plan: &Plan) -> Result<Vec<Vec<String>>> {
    let lab = &topology.lab.name;
    let mut cmds: Vec<Vec<String>> = Vec::new();

    // 1. namespaces
    for node in &topology.nodes {
        let ns = netns(lab, &node.name);
        cmds.push(vec!["ip".into(), "netns".into(), "add".into(), ns.clone()]);
        // Loopback inside a fresh namespace starts down, which breaks
        // everything that binds to 127.0.0.1 in surprising ways.
        cmds.push(vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            ns,
            "ip".into(),
            "link".into(),
            "set".into(),
            "lo".into(),
            "up".into(),
        ]);
    }

    // 2 + 3. veth pairs, then moved into place
    for link in &topology.links {
        let (a, b) = link.parse_endpoints()?;
        let (va, vb) = (veth_name(&a.node, &a.iface), veth_name(&b.node, &b.iface));

        cmds.push(vec![
            "ip".into(),
            "link".into(),
            "add".into(),
            va.clone(),
            "type".into(),
            "veth".into(),
            "peer".into(),
            "name".into(),
            vb.clone(),
        ]);

        for (veth, end) in [(&va, &a), (&vb, &b)] {
            let ns = netns(lab, &end.node);
            cmds.push(vec![
                "ip".into(),
                "link".into(),
                "set".into(),
                veth.clone(),
                "netns".into(),
                ns.clone(),
            ]);
            // Rename to the topology's interface name once it is inside the
            // namespace, where it cannot collide with anything.
            cmds.push(vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                ns,
                "ip".into(),
                "link".into(),
                "set".into(),
                veth.clone(),
                "name".into(),
                end.iface.clone(),
            ]);
        }
    }

    // 4. addresses
    for link in &plan.links {
        for end in [&link.a, &link.b] {
            cmds.push(vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                netns(lab, &end.node),
                "ip".into(),
                "addr".into(),
                "add".into(),
                end.cidr(),
                "dev".into(),
                end.iface.clone(),
            ]);
        }
    }

    // Router loopbacks, which BGP uses as its router-id.
    for (node, addr) in &plan.loopbacks {
        cmds.push(vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            netns(lab, node),
            "ip".into(),
            "addr".into(),
            "add".into(),
            format!("{addr}/32"),
            "dev".into(),
            "lo".into(),
        ]);
    }

    // 5. interfaces up, last
    for link in &plan.links {
        for end in [&link.a, &link.b] {
            cmds.push(vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                netns(lab, &end.node),
                "ip".into(),
                "link".into(),
                "set".into(),
                end.iface.clone(),
                "up".into(),
            ]);
        }
    }

    // Routers must forward, or a correct topology still cannot route.
    for node in topology.routers() {
        cmds.push(vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            netns(lab, &node.name),
            "sysctl".into(),
            "-w".into(),
            "net.ipv4.ip_forward=1".into(),
        ]);
    }

    Ok(cmds)
}

/// The commands that tear a lab down.
///
/// Deleting a namespace takes its veth ends with it, so this is short — and it
/// must tolerate a namespace that is already gone, because a teardown after a
/// partial bring-up is the common case.
pub fn down_commands(topology: &Topology) -> Vec<Vec<String>> {
    topology
        .nodes
        .iter()
        .map(|node| {
            vec![
                "ip".into(),
                "netns".into(),
                "del".into(),
                netns(&topology.lab.name, &node.name),
            ]
        })
        .collect()
}

/// A command that runs inside a node's namespace.
pub fn in_node(lab: &str, node: &str, argv: &[&str]) -> Vec<String> {
    let mut cmd = vec![
        "ip".to_string(),
        "netns".to_string(),
        "exec".to_string(),
        netns(lab, node),
    ];
    cmd.extend(argv.iter().map(|s| s.to_string()));
    cmd
}

/// A ping from one node to an address, for the reachability matrix (§9.2).
pub fn ping(lab: &str, from: &str, to: &std::net::Ipv4Addr) -> Vec<String> {
    in_node(lab, from, &["ping", "-c", "1", "-W", "2", &to.to_string()])
}

/// Render a command for display, the way `--dry-run` prints it.
pub fn render(cmd: &[String]) -> String {
    cmd.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::ipam;
    use crate::lab::topology::{LabSection, Link, Node, Role, Services};

    fn clos() -> Topology {
        Topology {
            lab: LabSection {
                name: "clos".into(),
                substrate: "auto".into(),
                base: "10.0.0.0/16".into(),
                asn_base: 65000,
            },
            nodes: ["leaf1", "leaf2", "spine1"]
                .iter()
                .map(|n| Node {
                    name: (*n).into(),
                    role: Role::FrrRouter,
                    sets: vec![],
                    asn: None,
                })
                .collect(),
            links: vec![
                Link {
                    endpoints: vec!["leaf1:eth1".into(), "spine1:eth1".into()],
                    subnet: None,
                },
                Link {
                    endpoints: vec!["leaf2:eth1".into(), "spine1:eth2".into()],
                    subnet: None,
                },
            ],
            services: Services::default(),
        }
    }

    fn commands() -> Vec<String> {
        let t = clos();
        let plan = ipam::allocate(&t).unwrap();
        up_commands(&t, &plan)
            .unwrap()
            .iter()
            .map(|c| render(c))
            .collect()
    }

    #[test]
    fn namespaces_are_named_so_their_owner_is_obvious() {
        assert_eq!(netns("clos", "leaf1"), "devbox-clos-leaf1");
        assert!(netns("clos", "leaf1").starts_with(NS_PREFIX));
    }

    #[test]
    fn veth_names_fit_the_kernel_limit() {
        assert_eq!(veth_name("leaf1", "eth1"), "leaf1-eth1");
        let long = veth_name("a-very-long-node-name", "eth1");
        assert!(long.len() <= 15, "Linux caps interface names at 15: {long}");
    }

    #[test]
    fn a_namespace_exists_before_anything_moves_into_it() {
        let cmds = commands();
        let add_ns = cmds
            .iter()
            .position(|c| c == "ip netns add devbox-clos-leaf1")
            .expect("leaf1's namespace is created");
        let move_in = cmds
            .iter()
            .position(|c| c.contains("netns devbox-clos-leaf1"))
            .expect("something moves into leaf1's namespace");
        assert!(add_ns < move_in, "namespace must exist first:\n{cmds:#?}");
    }

    #[test]
    fn addresses_are_assigned_before_interfaces_come_up() {
        // Otherwise an interface briefly carries traffic while unaddressed,
        // which shows up as a flaky first ping and nothing else.
        let cmds = commands();
        let last_addr = cmds
            .iter()
            .rposition(|c| c.contains("ip addr add") && c.contains("dev eth"))
            .expect("addresses are assigned");
        let first_up = cmds
            .iter()
            .position(|c| c.contains("ip link set eth") && c.ends_with(" up"))
            .expect("interfaces are brought up");
        assert!(last_addr < first_up, "address before link-up:\n{cmds:#?}");
    }

    #[test]
    fn loopback_is_brought_up_in_every_namespace() {
        // A fresh namespace's `lo` starts down, which breaks anything binding
        // to 127.0.0.1 in confusing ways.
        let cmds = commands();
        for node in ["leaf1", "leaf2", "spine1"] {
            assert!(
                cmds.iter()
                    .any(|c| c == &format!("ip netns exec devbox-clos-{node} ip link set lo up")),
                "{node} must bring lo up"
            );
        }
    }

    #[test]
    fn every_link_creates_exactly_one_veth_pair() {
        let cmds = commands();
        let pairs = cmds.iter().filter(|c| c.contains("type veth peer")).count();
        assert_eq!(pairs, 2, "two links, two pairs");
    }

    #[test]
    fn interfaces_are_renamed_to_the_topology_names() {
        let cmds = commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("ip link set leaf1-eth1 name eth1")),
            "the veth end takes the topology's interface name:\n{cmds:#?}"
        );
    }

    #[test]
    fn routers_get_a_loopback_address_and_forwarding() {
        let cmds = commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("ip addr add 10.0") && c.contains("dev lo")),
            "a router's loopback is its BGP router-id"
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| c.contains("net.ipv4.ip_forward=1"))
                .count(),
            3,
            "all three routers must forward"
        );
    }

    #[test]
    fn teardown_deletes_every_namespace() {
        let t = clos();
        let cmds: Vec<String> = down_commands(&t).iter().map(|c| render(c)).collect();

        assert_eq!(cmds.len(), 3);
        for node in ["leaf1", "leaf2", "spine1"] {
            assert!(cmds.contains(&format!("ip netns del devbox-clos-{node}")));
        }
    }

    #[test]
    fn commands_run_inside_a_node_are_namespace_scoped() {
        let cmd = in_node("clos", "leaf1", &["vtysh", "-c", "show bgp summary"]);
        assert_eq!(
            render(&cmd),
            "ip netns exec devbox-clos-leaf1 vtysh -c show bgp summary"
        );
    }

    #[test]
    fn ping_is_bounded_so_a_reachability_matrix_terminates() {
        let cmd = ping("clos", "leaf1", &"10.0.0.3".parse().unwrap());
        let text = render(&cmd);
        assert!(text.contains("-c 1"), "one packet");
        assert!(text.contains("-W 2"), "and a timeout: {text}");
        assert!(text.ends_with("10.0.0.3"));
    }

    #[test]
    fn a_single_node_lab_produces_only_its_namespace() {
        let t = Topology {
            lab: LabSection {
                name: "solo".into(),
                substrate: "auto".into(),
                base: "10.0.0.0/16".into(),
                asn_base: 65000,
            },
            nodes: vec![Node {
                name: "only".into(),
                role: Role::Host,
                sets: vec![],
                asn: None,
            }],
            links: vec![],
            services: Services::default(),
        };
        let plan = ipam::allocate(&t).unwrap();
        let cmds = up_commands(&t, &plan).unwrap();

        assert!(cmds.iter().all(|c| !render(c).contains("type veth")));
        assert!(cmds.iter().any(|c| render(c).contains("netns add")));
    }
}
