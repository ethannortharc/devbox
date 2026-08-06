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
/// Linux caps interface names at 15 characters, and both ends of every pair
/// exist in the root namespace at once — so a truncated `node-iface` would
/// collide the moment two long node names shared a prefix, and `ip link add`
/// would fail (or worse, silently attach to the wrong pair).
///
/// These names are transient: each end is renamed to its topology interface
/// name as soon as it is inside its namespace. So they are indexed rather than
/// descriptive, which makes them unique by construction.
pub fn veth_name(link_index: usize, end: VethEnd) -> String {
    format!("dvb{link_index}{}", end.suffix())
}

/// Which half of a veth pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VethEnd {
    A,
    B,
}

impl VethEnd {
    fn suffix(self) -> &'static str {
        match self {
            VethEnd::A => "a",
            VethEnd::B => "b",
        }
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
        cmds.push(privileged(vec![
            "ip".into(),
            "netns".into(),
            "add".into(),
            ns.clone(),
        ]));
        // Loopback inside a fresh namespace starts down, which breaks
        // everything that binds to 127.0.0.1 in surprising ways.
        cmds.push(privileged(vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            ns,
            "ip".into(),
            "link".into(),
            "set".into(),
            "lo".into(),
            "up".into(),
        ]));
    }

    // 2 + 3. veth pairs, then moved into place
    for (index, link) in topology.links.iter().enumerate() {
        let (a, b) = link.parse_endpoints()?;
        let (va, vb) = (veth_name(index, VethEnd::A), veth_name(index, VethEnd::B));

        cmds.push(privileged(vec![
            "ip".into(),
            "link".into(),
            "add".into(),
            va.clone(),
            "type".into(),
            "veth".into(),
            "peer".into(),
            "name".into(),
            vb.clone(),
        ]));

        for (veth, end) in [(&va, &a), (&vb, &b)] {
            let ns = netns(lab, &end.node);
            cmds.push(privileged(vec![
                "ip".into(),
                "link".into(),
                "set".into(),
                veth.clone(),
                "netns".into(),
                ns.clone(),
            ]));
            // Rename to the topology's interface name once it is inside the
            // namespace, where it cannot collide with anything.
            cmds.push(privileged(vec![
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
            ]));
        }
    }

    // 4. addresses
    for link in &plan.links {
        for end in [&link.a, &link.b] {
            cmds.push(privileged(vec![
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
            ]));
        }
    }

    // A host's default route.
    //
    // A fresh namespace has only the /31 it was just given, so a host in a
    // routed scenario (`wan-lossy`, `client-proxy-server`) can reach its
    // directly attached router and nothing beyond it — the far site stays
    // unreachable however well BGP converges. Routers do not get one: they
    // learn everything from BGP, and a default would mask a fabric that has
    // not converged behind a route that always resolves.
    for link in &plan.links {
        for (end, peer) in [(&link.a, &link.b), (&link.b, &link.a)] {
            if topology.node(&end.node).is_none_or(|n| n.role.routes()) {
                continue;
            }
            cmds.push(privileged(vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                netns(lab, &end.node),
                "ip".into(),
                "route".into(),
                "replace".into(),
                "default".into(),
                "via".into(),
                peer.addr.to_string(),
                "dev".into(),
                end.iface.clone(),
            ]));
        }
    }

    // Router loopbacks, which BGP uses as its router-id.
    for (node, addr) in &plan.loopbacks {
        cmds.push(privileged(vec![
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
        ]));
    }

    // 5. interfaces up, last
    for link in &plan.links {
        for end in [&link.a, &link.b] {
            cmds.push(privileged(vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                netns(lab, &end.node),
                "ip".into(),
                "link".into(),
                "set".into(),
                end.iface.clone(),
                "up".into(),
            ]));
        }
    }

    // Routers must forward, or a correct topology still cannot route.
    for node in topology.routers() {
        cmds.push(privileged(vec![
            "ip".into(),
            "netns".into(),
            "exec".into(),
            netns(lab, &node.name),
            "sysctl".into(),
            "-w".into(),
            "net.ipv4.ip_forward=1".into(),
        ]));
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
        .flat_map(|node| {
            let ns = netns(&topology.lab.name, &node.name);
            // Stop the routing daemons first. Deleting a namespace out from
            // under a running zebra/bgpd leaves them alive with no interfaces,
            // holding their pidfiles and sockets — so the next `lab up`
            // starts a second set that cannot bind, and the lab comes up
            // half-wired with no obvious cause.
            vec![
                privileged(vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "ip netns pids {ns} 2>/dev/null | xargs -r kill 2>/dev/null; \
                         rm -rf /run/devbox/lab/{}/{}; exit 0",
                        topology.lab.name, node.name
                    ),
                ]),
                privileged(vec!["ip".into(), "netns".into(), "del".into(), ns]),
            ]
        })
        .collect()
}

/// Every wiring command needs `CAP_NET_ADMIN`.
///
/// Runtime `exec` runs as the ordinary VM user on Lima and Multipass, so a
/// bare `ip netns add` fails on the very first command. Prefixing here rather
/// than at each call site means no generator can forget.
pub const PRIVILEGED: &str = "sudo";

/// Prefix a command with the privilege escalation the substrate needs.
pub fn privileged(argv: Vec<String>) -> Vec<String> {
    let mut cmd = vec![PRIVILEGED.to_string()];
    cmd.extend(argv);
    cmd
}

/// A command that runs inside a node's namespace.
pub fn in_node(lab: &str, node: &str, argv: &[&str]) -> Vec<String> {
    let mut cmd = vec![
        PRIVILEGED.to_string(),
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
    fn veth_names_are_short_and_unique_by_construction() {
        // Both ends of every pair exist in the root namespace at once, so a
        // truncated `node-iface` would collide as soon as two long node names
        // shared a prefix.
        assert_eq!(veth_name(0, VethEnd::A), "dvb0a");
        assert_eq!(veth_name(0, VethEnd::B), "dvb0b");
        assert_ne!(veth_name(0, VethEnd::A), veth_name(1, VethEnd::A));

        let mut seen = std::collections::BTreeSet::new();
        for index in 0..500 {
            for end in [VethEnd::A, VethEnd::B] {
                let name = veth_name(index, end);
                assert!(name.len() <= 15, "Linux caps names at 15: {name}");
                assert!(seen.insert(name.clone()), "{name} collided");
            }
        }
    }

    #[test]
    fn long_node_names_do_not_collide() {
        let long = |n: usize| -> Topology {
            Topology {
                lab: LabSection {
                    name: "long".into(),
                    substrate: "auto".into(),
                    base: "10.0.0.0/16".into(),
                    asn_base: 65000,
                },
                nodes: (0..n)
                    .map(|i| Node {
                        name: format!("a-very-long-node-name-{i}"),
                        role: Role::Host,
                        sets: vec![],
                        asn: None,
                    })
                    .collect(),
                links: (0..n - 1)
                    .map(|i| Link {
                        endpoints: vec![
                            format!("a-very-long-node-name-{i}:eth1"),
                            format!("a-very-long-node-name-{}:eth2", i + 1),
                        ],
                        subnet: None,
                    })
                    .collect(),
                services: Services::default(),
            }
        };

        let t = long(4);
        t.validate().expect("the topology is valid");
        let plan = ipam::allocate(&t).unwrap();
        let cmds = up_commands(&t, &plan).unwrap();

        // Every `ip link add ... type veth peer name ...` must name two
        // interfaces nothing else in the root namespace is using.
        let mut created = std::collections::BTreeSet::new();
        for cmd in &cmds {
            if cmd.get(2).map(String::as_str) == Some("link")
                && cmd.get(3).map(String::as_str) == Some("add")
            {
                assert!(created.insert(cmd[4].clone()), "{} collided", cmd[4]);
                let peer = cmd.last().expect("peer name");
                assert!(created.insert(peer.clone()), "{peer} collided");
            }
        }
        assert_eq!(created.len(), 6, "three links, six ends");
    }

    #[test]
    fn a_namespace_exists_before_anything_moves_into_it() {
        let cmds = commands();
        let add_ns = cmds
            .iter()
            .position(|c| c == "sudo ip netns add devbox-clos-leaf1")
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
                    .any(|c| c
                        == &format!("sudo ip netns exec devbox-clos-{node} ip link set lo up")),
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
                .any(|c| c.contains("ip link set dvb0a name eth1")),
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

        // Two per node: stop what is running in the namespace, then delete it.
        assert_eq!(cmds.len(), 6);
        for node in ["leaf1", "leaf2", "spine1"] {
            let del = format!("sudo ip netns del devbox-clos-{node}");
            let kill = cmds
                .iter()
                .position(|c| c.contains(&format!("ip netns pids devbox-clos-{node}")))
                .expect("daemons must be stopped");
            let delete = cmds
                .iter()
                .position(|c| *c == del)
                .expect("namespace deleted");
            assert!(
                kill < delete,
                "deleting the namespace first strands zebra/bgpd holding their \
                 pidfiles, so the next `lab up` cannot bind"
            );
        }
    }

    #[test]
    fn commands_run_inside_a_node_are_namespace_scoped() {
        let cmd = in_node("clos", "leaf1", &["vtysh", "-c", "show bgp summary"]);
        assert_eq!(
            render(&cmd),
            "sudo ip netns exec devbox-clos-leaf1 vtysh -c show bgp summary"
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
    fn every_command_is_privileged() {
        // Runtime exec runs as the ordinary VM user on Lima and Multipass, so
        // an unprivileged `ip netns add` fails on the very first command.
        let t = clos();
        let plan = ipam::allocate(&t).unwrap();
        for cmd in up_commands(&t, &plan).unwrap() {
            assert_eq!(cmd[0], PRIVILEGED, "unprivileged: {}", render(&cmd));
        }
        for cmd in down_commands(&t) {
            assert_eq!(cmd[0], PRIVILEGED, "unprivileged: {}", render(&cmd));
        }
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
