//! Lab topology — the `lab.toml` schema (§9.1).
//!
//! A topology is a set of nodes and the links between them. Everything else —
//! addresses, routing config, wiring commands — is *derived*, so the file
//! stays something a person can read and edit without knowing how veth pairs
//! work.
//!
//! Validation is strict and specific. A topology that half-brings-up is far
//! worse than one that refuses to start: the first wastes an hour of debugging
//! a network that was never going to work.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What a node is for, which decides what services it runs (§9.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// FRR router: participates in BGP/OSPF.
    FrrRouter,
    /// Plain host: an endpoint that generates and receives traffic.
    #[default]
    Host,
    /// A node with no configuration, waiting to be provisioned by ZTP (§10).
    ZtpBlank,
    /// Runs a lab service: DNS, DHCP, NTP, or the ZTP server.
    Service,
}

impl Role {
    pub const ALL: &'static [Role] = &[Role::FrrRouter, Role::Host, Role::ZtpBlank, Role::Service];

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::FrrRouter => "frr-router",
            Role::Host => "host",
            Role::ZtpBlank => "ztp-blank",
            Role::Service => "service",
        }
    }

    /// Whether this role runs a routing daemon.
    pub fn routes(&self) -> bool {
        matches!(self, Role::FrrRouter)
    }

    /// Whether this node starts with no configuration at all.
    ///
    /// A ZTP node must boot blank — pre-configuring it would make the whole
    /// zero-touch demonstration a lie.
    pub fn blank(&self) -> bool {
        matches!(self, Role::ZtpBlank)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Role::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| anyhow::anyhow!("unknown node role '{s}'"))
    }
}

/// One node in the topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    #[serde(default)]
    pub role: Role,
    /// Nix sets to install on this node.
    #[serde(default)]
    pub sets: Vec<String>,
    /// BGP autonomous system number, assigned by IPAM when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn: Option<u32>,
}

/// One link between two node interfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Exactly two endpoints, each `node:iface`.
    pub endpoints: Vec<String>,
    /// Subnet for the link, assigned by IPAM when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
}

/// One endpoint of a link, parsed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub node: String,
    pub iface: String,
}

impl Endpoint {
    /// Parse `node:iface`.
    pub fn parse(text: &str) -> Result<Self> {
        let (node, iface) = text
            .split_once(':')
            .with_context(|| format!("endpoint '{text}' must be written node:interface"))?;
        if node.is_empty() || iface.is_empty() {
            bail!("endpoint '{text}' has an empty node or interface");
        }
        Ok(Self {
            node: node.to_string(),
            iface: iface.to_string(),
        })
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.node, self.iface)
    }
}

impl Link {
    /// Both endpoints, parsed and validated.
    pub fn parse_endpoints(&self) -> Result<(Endpoint, Endpoint)> {
        if self.endpoints.len() != 2 {
            bail!(
                "a link needs exactly two endpoints, got {}: {:?}",
                self.endpoints.len(),
                self.endpoints
            );
        }
        Ok((
            Endpoint::parse(&self.endpoints[0])?,
            Endpoint::parse(&self.endpoints[1])?,
        ))
    }
}

/// Core services the lab runs (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Services {
    #[serde(default = "yes")]
    pub dns: bool,
    #[serde(default)]
    pub dhcp: bool,
    #[serde(default = "yes")]
    pub ntp: bool,
}

fn yes() -> bool {
    true
}

impl Default for Services {
    fn default() -> Self {
        Self {
            dns: true,
            dhcp: false,
            ntp: true,
        }
    }
}

/// Lab-level settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabSection {
    pub name: String,
    /// `auto` | `lima` | `incus` | `host`.
    #[serde(default = "auto")]
    pub substrate: String,
    /// Base prefix every link subnet is carved from.
    #[serde(default = "default_base")]
    pub base: String,
    /// First ASN handed out to routers that do not declare one.
    #[serde(default = "default_asn_base")]
    pub asn_base: u32,
}

fn auto() -> String {
    "auto".to_string()
}
fn default_base() -> String {
    // RFC 1918, and deliberately not 192.168/16 — that is what home routers
    // use, and a lab that collides with the host's own LAN is a bad afternoon.
    "10.0.0.0/16".to_string()
}
fn default_asn_base() -> u32 {
    // RFC 6996 private ASN range.
    65000
}

/// A complete lab topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub lab: LabSection,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub services: Services,
}

impl Topology {
    /// Parse a topology from TOML.
    pub fn from_toml(text: &str) -> Result<Self> {
        let topology: Self = toml::from_str(text).context("failed to parse the lab topology")?;
        topology.validate()?;
        Ok(topology)
    }

    /// Load from a file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_toml(&text)
    }

    /// Reject anything that would bring up a broken network.
    ///
    /// Strict on purpose: a topology that half-comes-up wastes an hour of
    /// debugging a network that was never going to work.
    pub fn validate(&self) -> Result<()> {
        // The lab name becomes a network-namespace name and a path component
        // in the substrate, so it is held to the same rule as a node name.
        // Without this, a hostile `lab.toml` could put a quote or a shell
        // metacharacter somewhere it would be interpreted.
        if self.lab.name.trim().is_empty() {
            bail!("the lab needs a name");
        }
        if !is_valid_name(&self.lab.name) {
            bail!(
                "lab name '{}' must be lowercase letters, digits, or '-'; it \
                 becomes a namespace name and a path component",
                self.lab.name
            );
        }
        if self.nodes.is_empty() {
            bail!("a lab with no nodes has nothing to bring up");
        }

        let mut names = BTreeSet::new();
        for node in &self.nodes {
            if node.name.trim().is_empty() {
                bail!("a node has an empty name");
            }
            if !is_valid_name(&node.name) {
                bail!(
                    "node name '{}' must be lowercase letters, digits, or '-'",
                    node.name
                );
            }
            if !names.insert(node.name.clone()) {
                bail!("two nodes are both called '{}'", node.name);
            }
        }

        // An interface can only be one end of one link. Catching this here
        // beats discovering it as a veth that silently replaced another.
        let mut used: BTreeMap<Endpoint, usize> = BTreeMap::new();
        for (i, link) in self.links.iter().enumerate() {
            let (a, b) = link.parse_endpoints()?;

            for end in [&a, &b] {
                if !names.contains(&end.node) {
                    bail!("link {i} refers to unknown node '{}'", end.node);
                }
                if let Some(previous) = used.insert(end.clone(), i) {
                    bail!(
                        "{end} is used by both link {previous} and link {i}; \
                         an interface can only be one end of one link"
                    );
                }
            }
            if a.node == b.node {
                bail!("link {i} connects '{}' to itself", a.node);
            }
            if let Some(subnet) = &link.subnet
                && crate::policy::parse_cidr(subnet).is_none()
            {
                bail!("link {i} has an invalid subnet '{subnet}'");
            }
        }

        if let Some(node) = self.isolated_node() {
            bail!(
                "node '{node}' has no links; a lab node that cannot reach anything \
                 is almost always a typo in an endpoint"
            );
        }

        if crate::policy::parse_cidr(&self.lab.base).is_none() {
            bail!(
                "the lab base prefix '{}' is not a valid CIDR",
                self.lab.base
            );
        }

        Ok(())
    }

    /// The first node with no links, if any.
    ///
    /// Only meaningful when there is more than one node: a single-node lab is
    /// legitimately link-free.
    fn isolated_node(&self) -> Option<&str> {
        if self.nodes.len() < 2 {
            return None;
        }
        let connected: BTreeSet<String> = self
            .links
            .iter()
            .filter_map(|l| l.parse_endpoints().ok())
            .flat_map(|(a, b)| [a.node, b.node])
            .collect();

        self.nodes
            .iter()
            .find(|n| !connected.contains(&n.name))
            .map(|n| n.name.as_str())
    }

    /// Look up a node by name.
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// Every interface a node has, in link order.
    pub fn interfaces_of(&self, node: &str) -> Vec<String> {
        self.links
            .iter()
            .filter_map(|l| l.parse_endpoints().ok())
            .flat_map(|(a, b)| [a, b])
            .filter(|e| e.node == node)
            .map(|e| e.iface)
            .collect()
    }

    /// Nodes that run a routing daemon.
    pub fn routers(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.role.routes()).collect()
    }

    /// Every pair of nodes that share a link.
    pub fn adjacencies(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .links
            .iter()
            .filter_map(|l| l.parse_endpoints().ok())
            .map(|(a, b)| {
                if a.node <= b.node {
                    (a.node, b.node)
                } else {
                    (b.node, a.node)
                }
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// A lab node name: lowercase, digits, and `-`.
///
/// These become Linux interface and namespace names, where anything else is a
/// portability problem waiting to happen.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §9.1 example, verbatim.
    const CLOS: &str = r#"
[lab]
name = "clos-3node"
substrate = "auto"

[[nodes]]
name = "leaf1"
role = "frr-router"
sets = ["network"]

[[nodes]]
name = "leaf2"
role = "frr-router"

[[nodes]]
name = "spine1"
role = "frr-router"

[[links]]
endpoints = ["leaf1:eth1", "spine1:eth1"]
subnet = "10.0.12.0/31"

[[links]]
endpoints = ["leaf2:eth1", "spine1:eth2"]
subnet = "10.0.22.0/31"

[services]
dns = true
dhcp = false
ntp = true
"#;

    #[test]
    fn parses_the_design_example() {
        let t = Topology::from_toml(CLOS).expect("the §9.1 example must parse");

        assert_eq!(t.lab.name, "clos-3node");
        assert_eq!(t.nodes.len(), 3);
        assert_eq!(t.links.len(), 2);
        assert!(t.services.dns);
        assert!(!t.services.dhcp);
        assert_eq!(t.nodes[0].sets, vec!["network"]);
        assert_eq!(t.routers().len(), 3);
    }

    #[test]
    fn endpoints_parse_and_round_trip() {
        let e = Endpoint::parse("leaf1:eth1").unwrap();
        assert_eq!(e.node, "leaf1");
        assert_eq!(e.iface, "eth1");
        assert_eq!(e.to_string(), "leaf1:eth1");

        for bad in ["leaf1", ":eth1", "leaf1:", ""] {
            assert!(Endpoint::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn roles_round_trip_and_describe_themselves() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), *role);
        }
        assert!("nonsense".parse::<Role>().is_err());

        assert!(Role::FrrRouter.routes());
        assert!(!Role::Host.routes());
        assert!(Role::ZtpBlank.blank());
        assert!(!Role::FrrRouter.blank());
    }

    fn topology(nodes: &[(&str, Role)], links: &[(&str, &str)]) -> Topology {
        Topology {
            lab: LabSection {
                name: "test".into(),
                substrate: "auto".into(),
                base: "10.0.0.0/16".into(),
                asn_base: 65000,
            },
            nodes: nodes
                .iter()
                .map(|(name, role)| Node {
                    name: (*name).into(),
                    role: *role,
                    sets: vec![],
                    asn: None,
                })
                .collect(),
            links: links
                .iter()
                .map(|(a, b)| Link {
                    endpoints: vec![(*a).into(), (*b).into()],
                    subnet: None,
                })
                .collect(),
            services: Services::default(),
        }
    }

    #[test]
    fn a_well_formed_topology_validates() {
        let t = topology(
            &[("leaf1", Role::FrrRouter), ("spine1", Role::FrrRouter)],
            &[("leaf1:eth1", "spine1:eth1")],
        );
        assert!(t.validate().is_ok());
    }

    #[test]
    fn duplicate_node_names_are_rejected() {
        let t = topology(
            &[("leaf1", Role::Host), ("leaf1", Role::Host)],
            &[("leaf1:eth1", "leaf1:eth2")],
        );
        assert!(t.validate().unwrap_err().to_string().contains("leaf1"));
    }

    #[test]
    fn a_link_to_an_unknown_node_is_rejected() {
        let t = topology(
            &[("leaf1", Role::Host), ("spine1", Role::Host)],
            &[("leaf1:eth1", "ghost:eth1")],
        );
        assert!(t.validate().unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn an_interface_used_twice_is_rejected() {
        // This is the failure that would otherwise show up as a veth silently
        // replacing another, hours later.
        let t = topology(
            &[("a", Role::Host), ("b", Role::Host), ("c", Role::Host)],
            &[("a:eth1", "b:eth1"), ("a:eth1", "c:eth1")],
        );
        let err = t.validate().unwrap_err().to_string();
        assert!(err.contains("a:eth1"), "{err}");
        assert!(err.contains("one end of one link"), "{err}");
    }

    #[test]
    fn a_self_link_is_rejected() {
        let t = topology(
            &[("a", Role::Host), ("b", Role::Host)],
            &[("a:eth1", "a:eth2"), ("a:eth3", "b:eth1")],
        );
        assert!(t.validate().unwrap_err().to_string().contains("itself"));
    }

    #[test]
    fn an_unconnected_node_is_rejected() {
        // Almost always a typo in an endpoint.
        let t = topology(
            &[("a", Role::Host), ("b", Role::Host), ("orphan", Role::Host)],
            &[("a:eth1", "b:eth1")],
        );
        assert!(t.validate().unwrap_err().to_string().contains("orphan"));
    }

    #[test]
    fn a_single_node_lab_needs_no_links() {
        let t = topology(&[("solo", Role::Host)], &[]);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn a_lab_with_no_nodes_is_rejected() {
        let t = topology(&[], &[]);
        assert!(t.validate().unwrap_err().to_string().contains("no nodes"));
    }

    #[test]
    fn the_lab_name_is_held_to_the_same_rule_as_a_node_name() {
        // It becomes a namespace name and a path component in the substrate.
        for bad in ["Lab One", "lab'; rm -rf /; '", "lab/../etc", "", "-lab"] {
            let mut t = topology(
                &[("a", Role::Host), ("b", Role::Host)],
                &[("a:eth1", "b:eth1")],
            );
            t.lab.name = bad.into();
            assert!(t.validate().is_err(), "lab name {bad:?} should be rejected");
        }
    }

    #[test]
    fn node_names_must_be_interface_safe() {
        for good in ["leaf1", "spine-1", "a", "node-42"] {
            assert!(is_valid_name(good), "{good} should be valid");
        }
        for bad in ["", "Leaf1", "leaf_1", "-leaf", "leaf-", "leaf 1", "leaf.1"] {
            assert!(!is_valid_name(bad), "{bad} should be rejected");
        }

        let t = topology(
            &[("Leaf1", Role::Host), ("b", Role::Host)],
            &[("Leaf1:eth1", "b:eth1")],
        );
        assert!(t.validate().is_err());
    }

    #[test]
    fn a_bad_subnet_or_base_is_rejected() {
        let mut t = topology(
            &[("a", Role::Host), ("b", Role::Host)],
            &[("a:eth1", "b:eth1")],
        );
        t.links[0].subnet = Some("not-a-subnet".into());
        assert!(t.validate().unwrap_err().to_string().contains("subnet"));

        t.links[0].subnet = None;
        t.lab.base = "nonsense".into();
        assert!(t.validate().unwrap_err().to_string().contains("base"));
    }

    #[test]
    fn a_link_needs_exactly_two_endpoints() {
        for endpoints in [vec!["a:eth1"], vec!["a:eth1", "b:eth1", "c:eth1"]] {
            let link = Link {
                endpoints: endpoints.iter().map(|s| s.to_string()).collect(),
                subnet: None,
            };
            assert!(link.parse_endpoints().is_err());
        }
    }

    #[test]
    fn derives_interfaces_routers_and_adjacencies() {
        let t = Topology::from_toml(CLOS).unwrap();

        assert_eq!(t.interfaces_of("spine1"), vec!["eth1", "eth2"]);
        assert_eq!(t.interfaces_of("leaf1"), vec!["eth1"]);
        assert!(t.interfaces_of("nobody").is_empty());

        assert_eq!(t.routers().len(), 3);

        let adj = t.adjacencies();
        assert_eq!(adj.len(), 2);
        assert!(adj.contains(&("leaf1".into(), "spine1".into())));
        assert!(adj.contains(&("leaf2".into(), "spine1".into())));
    }

    #[test]
    fn the_default_base_avoids_the_range_home_routers_use() {
        let t = Topology::from_toml("[lab]\nname = \"x\"\n[[nodes]]\nname = \"solo\"\n").unwrap();
        assert!(
            !t.lab.base.starts_with("192.168"),
            "a lab that collides with the host's LAN is a bad afternoon"
        );
        assert_eq!(t.lab.asn_base, 65000, "RFC 6996 private range");
    }
}
