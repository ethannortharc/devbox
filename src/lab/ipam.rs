//! IP address management — the lab's address plan (§9.2, §10.2).
//!
//! A topology names nodes and links; this turns that into addresses. Every
//! link gets a /31, every router gets a loopback and an ASN, and the
//! allocation is **deterministic**: the same topology always produces the same
//! plan, so a `lab up` after a `lab down` is the same lab, and a config
//! golden-file test is possible at all.
//!
//! /31 for point-to-point links (RFC 3021) rather than /30: two usable
//! addresses instead of two wasted ones, and it is what real fabrics do.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::topology::{Endpoint, Topology};

/// The address plan for a whole lab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Per-link subnet and the address at each end.
    pub links: Vec<LinkPlan>,
    /// Router loopbacks, keyed by node name.
    pub loopbacks: BTreeMap<String, Ipv4Addr>,
    /// BGP ASNs, keyed by node name.
    pub asns: BTreeMap<String, u32>,
}

/// One link's addressing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkPlan {
    pub subnet: String,
    pub a: InterfaceAddr,
    pub b: InterfaceAddr,
}

/// One interface's address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterfaceAddr {
    pub node: String,
    pub iface: String,
    pub addr: Ipv4Addr,
    pub prefix_len: u8,
}

impl InterfaceAddr {
    /// `10.0.12.0/31`, the form `ip addr add` wants.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.addr, self.prefix_len)
    }
}

impl Plan {
    /// The plan for one node's interface, if any.
    pub fn addr_of(&self, node: &str, iface: &str) -> Option<&InterfaceAddr> {
        self.links.iter().find_map(|l| {
            [&l.a, &l.b]
                .into_iter()
                .find(|a| a.node == node && a.iface == iface)
        })
    }

    /// Every address assigned to a node.
    pub fn addrs_of(&self, node: &str) -> Vec<&InterfaceAddr> {
        self.links
            .iter()
            .flat_map(|l| [&l.a, &l.b])
            .filter(|a| a.node == node)
            .collect()
    }

    /// The neighbour on the other end of a node's interface.
    pub fn peer_of(&self, node: &str, iface: &str) -> Option<&InterfaceAddr> {
        self.links.iter().find_map(|l| {
            if l.a.node == node && l.a.iface == iface {
                Some(&l.b)
            } else if l.b.node == node && l.b.iface == iface {
                Some(&l.a)
            } else {
                None
            }
        })
    }

    /// Every address in the plan, for overlap checking.
    pub fn all_addrs(&self) -> Vec<Ipv4Addr> {
        let mut out: Vec<Ipv4Addr> = self
            .links
            .iter()
            .flat_map(|l| [l.a.addr, l.b.addr])
            .chain(self.loopbacks.values().copied())
            .collect();
        out.sort();
        out
    }
}

/// Point-to-point prefix length. RFC 3021: a /31 has two usable addresses.
pub const P2P_PREFIX: u8 = 31;

/// The base prefix is split in half: link subnets come from the lower half,
/// loopbacks from the upper. Two allocators that cannot reach each other's
/// range cannot collide, whatever the base prefix happens to be — and an
/// absolute offset would fall outside a small base entirely.
fn loopback_base(base: Ipv4Addr, base_len: u8) -> Ipv4Addr {
    let half = host_capacity(base_len).map(|c| c / 2).unwrap_or(1 << 31);
    offset_addr(base, half)
}

/// Allocate addresses for a topology.
///
/// Deterministic: the same topology always yields the same plan. Explicit
/// subnets in the topology are honoured; everything else is derived in link
/// order.
pub fn allocate(topology: &Topology) -> Result<Plan> {
    let (base, base_len) = parse_v4_cidr(&topology.lab.base)
        .with_context(|| format!("lab base '{}' is not an IPv4 CIDR", topology.lab.base))?;

    if base_len > 24 {
        bail!(
            "lab base '{}' is too small: a /{} leaves no room for link subnets",
            topology.lab.base,
            base_len
        );
    }

    let mut plan = Plan::default();
    let mut next_link: u32 = 0;

    // Prefixes an explicit link has taken. Automatic allocation walks past
    // these instead of handing out a prefix that is already spoken for — the
    // mixed explicit/automatic topology used to allocate a collision and then
    // fail its own verification.
    let mut claimed: Vec<(Ipv4Addr, u8)> = topology
        .links
        .iter()
        .filter_map(|link| link.subnet.as_deref())
        .filter_map(parse_v4_cidr)
        .map(|(addr, len)| {
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            (Ipv4Addr::from(u32::from(addr) & mask), len)
        })
        .collect();

    for (index, link) in topology.links.iter().enumerate() {
        let (a, b) = link.parse_endpoints()?;

        let (network, prefix_len) = match &link.subnet {
            Some(explicit) => {
                let (addr, len) = parse_v4_cidr(explicit)
                    .with_context(|| format!("link {index} has an invalid subnet '{explicit}'"))?;
                if len > P2P_PREFIX {
                    bail!("link {index} declares '{explicit}': a /{len} cannot address two ends");
                }
                // Canonicalize: `192.0.2.1/31` names a host, not a network, and
                // deriving both ends from it would put them in different
                // subnets.
                let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
                let canonical = Ipv4Addr::from(u32::from(addr) & mask);
                if canonical != addr {
                    bail!(
                        "link {index} declares '{explicit}', which is a host inside \
                         {canonical}/{len}, not the network itself"
                    );
                }
                (canonical, len)
            }
            None => {
                // Each derived link takes the next *free* /31 inside the base.
                // Links live in the lower half of the base prefix.
                //
                // "Free" is the point: a mixed topology, where one link names
                // its prefix and the rest are derived, used to hand the
                // derived allocation the same /31 the explicit one had taken —
                // and then fail its own verification.
                let link_capacity = host_capacity(base_len).map(|c| c / 2);
                loop {
                    let offset = next_link
                        .checked_mul(2)
                        .filter(|o| link_capacity.is_none_or(|cap| *o + 1 < cap))
                        .with_context(|| {
                            format!(
                                "the lab base '{}' has no room for link {index}",
                                topology.lab.base
                            )
                        })?;
                    next_link += 1;
                    let candidate = offset_addr(base, offset);
                    if !claimed
                        .iter()
                        .any(|(net, len)| overlaps((candidate, P2P_PREFIX), (*net, *len)))
                    {
                        break (candidate, P2P_PREFIX);
                    }
                }
            }
        };

        claimed.push((network, prefix_len));

        // With a /31 the two addresses are the network address and the one
        // after it; with anything wider, skip the network address.
        let (first, second) = if prefix_len >= P2P_PREFIX {
            (network, offset_addr(network, 1))
        } else {
            (offset_addr(network, 1), offset_addr(network, 2))
        };

        plan.links.push(LinkPlan {
            subnet: format!("{network}/{prefix_len}"),
            a: InterfaceAddr {
                node: a.node.clone(),
                iface: a.iface.clone(),
                addr: first,
                prefix_len,
            },
            b: InterfaceAddr {
                node: b.node.clone(),
                iface: b.iface.clone(),
                addr: second,
                prefix_len,
            },
        });
    }

    // Loopbacks and ASNs for routers, in topology order so they are stable.
    // Explicit ASNs are claimed first, so a derived one never lands on top of
    // one the topology already asked for — two eBGP peers sharing an AS do not
    // peer at all.
    let claimed: std::collections::BTreeSet<u32> = topology
        .nodes
        .iter()
        .filter(|n| n.role.routes())
        .filter_map(|n| n.asn)
        .collect();

    let mut next_router: u32 = 0;
    let mut next_asn = topology.lab.asn_base;
    for node in &topology.nodes {
        if !node.role.routes() {
            continue;
        }
        plan.loopbacks.insert(
            node.name.clone(),
            offset_addr(loopback_base(base, base_len), next_router),
        );

        let asn = match node.asn {
            Some(explicit) => explicit,
            None => {
                while claimed.contains(&next_asn) {
                    next_asn += 1;
                }
                let derived = next_asn;
                next_asn += 1;
                derived
            }
        };
        plan.asns.insert(node.name.clone(), asn);
        next_router += 1;
    }

    verify(&plan)?;
    Ok(plan)
}

/// The property that must always hold: no address is assigned twice.
///
/// Checked after every allocation rather than trusted, because an overlapping
/// address plan produces a lab that comes up and then behaves inexplicably.
/// Do two IPv4 prefixes cover any address in common?
///
/// Comparing canonical subnet *strings* only catches exact duplicates. A `/29`
/// that contains a `/31` assigns different endpoint addresses, so it passed the
/// string check and installed two overlapping routes into the same fabric.
fn overlaps(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
    let shorter = a.1.min(b.1);
    let mask = if shorter == 0 {
        0
    } else {
        u32::MAX << (32 - shorter)
    };
    (u32::from(a.0) & mask) == (u32::from(b.0) & mask)
}

pub fn verify(plan: &Plan) -> Result<()> {
    let addrs = plan.all_addrs();
    for pair in addrs.windows(2) {
        if pair[0] == pair[1] {
            bail!("address {} is assigned twice", pair[0]);
        }
    }

    let mut seen: Vec<(Ipv4Addr, u8)> = Vec::new();
    for link in &plan.links {
        let (net, len) = parse_v4_cidr(&link.subnet)
            .with_context(|| format!("link subnet '{}' is not a prefix", link.subnet))?;
        if let Some((other, other_len)) = seen
            .iter()
            .find(|existing| overlaps((net, len), **existing))
        {
            bail!(
                "subnet {} overlaps {other}/{other_len}: overlapping links install \
                 ambiguous routes and the fabric comes up but never behaves",
                link.subnet
            );
        }
        seen.push((net, len));
    }

    // Two routers sharing an AS do not form an eBGP session, so the fabric
    // comes up and never converges.
    let mut asns = std::collections::BTreeMap::new();
    for (node, asn) in &plan.asns {
        if let Some(other) = asns.insert(*asn, node.clone()) {
            bail!("routers '{other}' and '{node}' share AS {asn}");
        }
    }
    Ok(())
}

/// Parse an IPv4 CIDR into `(network, prefix_len)`.
pub fn parse_v4_cidr(text: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, len) = text.split_once('/')?;
    let addr: Ipv4Addr = addr.parse().ok()?;
    let len: u8 = len.parse().ok()?;
    (len <= 32).then_some((addr, len))
}

/// Add `offset` to an address.
fn offset_addr(base: Ipv4Addr, offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(base).wrapping_add(offset))
}

/// How many addresses a prefix holds, or `None` for a /0 (which holds more
/// than a `u32` can count).
fn host_capacity(prefix_len: u8) -> Option<u32> {
    (prefix_len > 0).then(|| 1u32 << (32 - prefix_len))
}

/// Render an endpoint's address for display.
pub fn describe(plan: &Plan, endpoint: &Endpoint) -> String {
    plan.addr_of(&endpoint.node, &endpoint.iface)
        .map(|a| a.cidr())
        .unwrap_or_else(|| "unassigned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::topology::{LabSection, Link, Node, Role, Services};

    fn topology(nodes: &[(&str, Role)], links: &[(&str, &str, Option<&str>)]) -> Topology {
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
                .map(|(a, b, subnet)| Link {
                    endpoints: vec![(*a).into(), (*b).into()],
                    subnet: subnet.map(|s| s.to_string()),
                })
                .collect(),
            services: Services::default(),
        }
    }

    fn clos() -> Topology {
        topology(
            &[
                ("leaf1", Role::FrrRouter),
                ("leaf2", Role::FrrRouter),
                ("spine1", Role::FrrRouter),
            ],
            &[
                ("leaf1:eth1", "spine1:eth1", None),
                ("leaf2:eth1", "spine1:eth2", None),
            ],
        )
    }

    #[test]
    fn every_link_gets_a_p2p_subnet_and_both_ends_an_address() {
        let plan = allocate(&clos()).unwrap();

        assert_eq!(plan.links.len(), 2);
        for link in &plan.links {
            assert_eq!(link.a.prefix_len, P2P_PREFIX, "RFC 3021 /31 links");
            assert_eq!(link.b.prefix_len, P2P_PREFIX);
            assert_ne!(link.a.addr, link.b.addr);
            // The two ends of a /31 are consecutive.
            assert_eq!(u32::from(link.b.addr), u32::from(link.a.addr) + 1);
        }
    }

    #[test]
    fn allocation_is_deterministic() {
        // A `lab up` after a `lab down` must be the same lab.
        let a = allocate(&clos()).unwrap();
        let b = allocate(&clos()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn no_address_is_ever_assigned_twice() {
        // The property that matters: an overlapping plan produces a lab that
        // comes up and then behaves inexplicably.
        let big = topology(
            &(1..=20)
                .map(|i| {
                    (
                        Box::leak(format!("n{i}").into_boxed_str()) as &str,
                        Role::FrrRouter,
                    )
                })
                .collect::<Vec<_>>(),
            &(1..20)
                .map(|i| {
                    (
                        Box::leak(format!("n{i}:eth1").into_boxed_str()) as &str,
                        Box::leak(format!("n{}:eth2", i + 1).into_boxed_str()) as &str,
                        None,
                    )
                })
                .collect::<Vec<_>>(),
        );

        let plan = allocate(&big).unwrap();
        let addrs = plan.all_addrs();
        let unique: std::collections::BTreeSet<_> = addrs.iter().collect();
        assert_eq!(unique.len(), addrs.len(), "addresses must be unique");
        assert!(verify(&plan).is_ok());
    }

    #[test]
    fn explicit_subnets_are_honoured() {
        let t = topology(
            &[("a", Role::FrrRouter), ("b", Role::FrrRouter)],
            &[("a:eth1", "b:eth1", Some("192.0.2.0/31"))],
        );
        let plan = allocate(&t).unwrap();

        assert_eq!(plan.links[0].subnet, "192.0.2.0/31");
        assert_eq!(plan.links[0].a.addr.to_string(), "192.0.2.0");
        assert_eq!(plan.links[0].b.addr.to_string(), "192.0.2.1");
    }

    #[test]
    fn a_wider_explicit_subnet_skips_the_network_address() {
        let t = topology(
            &[("a", Role::Host), ("b", Role::Host)],
            &[("a:eth1", "b:eth1", Some("10.9.0.0/24"))],
        );
        let plan = allocate(&t).unwrap();

        assert_eq!(plan.links[0].a.addr.to_string(), "10.9.0.1");
        assert_eq!(plan.links[0].b.addr.to_string(), "10.9.0.2");
        assert_eq!(plan.links[0].a.prefix_len, 24);
    }

    #[test]
    fn routers_get_loopbacks_and_asns_hosts_do_not() {
        let t = topology(
            &[
                ("r1", Role::FrrRouter),
                ("r2", Role::FrrRouter),
                ("h1", Role::Host),
            ],
            &[("r1:eth1", "r2:eth1", None), ("r2:eth2", "h1:eth1", None)],
        );
        let plan = allocate(&t).unwrap();

        assert_eq!(plan.loopbacks.len(), 2);
        assert_eq!(plan.asns.len(), 2);
        assert!(!plan.loopbacks.contains_key("h1"), "a host does not route");

        assert_eq!(plan.asns["r1"], 65000);
        assert_eq!(plan.asns["r2"], 65001);
        assert_ne!(plan.loopbacks["r1"], plan.loopbacks["r2"]);
    }

    #[test]
    fn an_explicit_asn_is_honoured() {
        let mut t = topology(
            &[("r1", Role::FrrRouter), ("r2", Role::FrrRouter)],
            &[("r1:eth1", "r2:eth1", None)],
        );
        t.nodes[0].asn = Some(64512);

        let plan = allocate(&t).unwrap();
        assert_eq!(plan.asns["r1"], 64512);
        assert_eq!(plan.asns["r2"], 65000, "derivation starts at asn_base");
    }

    #[test]
    fn a_derived_asn_never_lands_on_an_explicit_one() {
        // Two eBGP peers sharing an AS do not peer at all, so the fabric comes
        // up and never converges — the worst kind of lab bug.
        let mut t = topology(
            &[
                ("r1", Role::FrrRouter),
                ("r2", Role::FrrRouter),
                ("r3", Role::FrrRouter),
            ],
            &[("r1:eth1", "r2:eth1", None), ("r2:eth2", "r3:eth1", None)],
        );
        t.nodes[0].asn = Some(65000); // exactly what derivation would pick

        let plan = allocate(&t).unwrap();
        assert_eq!(plan.asns["r1"], 65000);
        assert_ne!(plan.asns["r2"], 65000);
        assert_ne!(plan.asns["r3"], 65000);
        assert_ne!(plan.asns["r2"], plan.asns["r3"]);
    }

    #[test]
    fn verify_catches_duplicate_asns() {
        let mut plan = allocate(&clos()).unwrap();
        let first = plan.asns["leaf1"];
        plan.asns.insert("leaf2".into(), first);
        assert!(verify(&plan).unwrap_err().to_string().contains("share AS"));
    }

    #[test]
    fn an_explicit_subnet_must_be_a_network_that_fits_two_ends() {
        // `192.0.2.1/31` names a host, and deriving both ends from it would
        // put them in different subnets.
        for bad in ["192.0.2.1/31", "10.0.0.5/32", "10.0.0.1/24"] {
            let t = topology(
                &[("a", Role::Host), ("b", Role::Host)],
                &[("a:eth1", "b:eth1", Some(bad))],
            );
            assert!(allocate(&t).is_err(), "{bad} should be rejected");
        }
        // The canonical form is fine.
        let t = topology(
            &[("a", Role::Host), ("b", Role::Host)],
            &[("a:eth1", "b:eth1", Some("192.0.2.0/31"))],
        );
        assert!(allocate(&t).is_ok());
    }

    #[test]
    fn loopbacks_stay_inside_the_base_prefix() {
        // An absolute offset would fall outside a small base entirely, and a
        // loopback the fabric cannot route to is worse than none.
        for base in ["10.0.0.0/16", "10.0.0.0/24", "172.16.0.0/20", "10.0.0.0/8"] {
            let mut t = clos();
            t.lab.base = base.into();
            let plan = allocate(&t).unwrap();

            for (node, addr) in &plan.loopbacks {
                assert!(
                    crate::policy::match_cidr(base, &addr.to_string()),
                    "{node}'s loopback {addr} is outside {base}"
                );
            }
            for link in &plan.links {
                assert!(
                    crate::policy::match_cidr(base, &link.a.addr.to_string()),
                    "link address {} is outside {base}",
                    link.a.addr
                );
            }
        }
    }

    #[test]
    fn loopbacks_never_collide_with_link_subnets() {
        let plan = allocate(&clos()).unwrap();
        let link_addrs: std::collections::BTreeSet<Ipv4Addr> = plan
            .links
            .iter()
            .flat_map(|l| [l.a.addr, l.b.addr])
            .collect();

        for loopback in plan.loopbacks.values() {
            assert!(
                !link_addrs.contains(loopback),
                "{loopback} is both a loopback and a link address"
            );
        }
    }

    #[test]
    fn lookups_find_addresses_and_peers() {
        let plan = allocate(&clos()).unwrap();

        let leaf = plan
            .addr_of("leaf1", "eth1")
            .expect("leaf1:eth1 is planned");
        assert!(leaf.cidr().ends_with("/31"));
        assert_eq!(plan.addrs_of("spine1").len(), 2);
        assert!(plan.addr_of("leaf1", "eth9").is_none());

        let peer = plan.peer_of("leaf1", "eth1").expect("leaf1 has a peer");
        assert_eq!(peer.node, "spine1");
        assert_eq!(peer.addr, plan.links[0].b.addr);

        // And the reverse direction resolves too.
        let back = plan.peer_of("spine1", "eth1").unwrap();
        assert_eq!(back.node, "leaf1");
    }

    #[test]
    fn a_base_prefix_with_no_room_is_rejected() {
        let mut t = clos();
        t.lab.base = "10.0.0.0/30".into();
        let err = allocate(&t).unwrap_err().to_string();
        assert!(err.contains("too small"), "{err}");
    }

    #[test]
    fn a_non_ipv4_base_is_rejected() {
        let mut t = clos();
        t.lab.base = "2001:db8::/32".into();
        assert!(allocate(&t).is_err());
    }

    #[test]
    fn explicit_prefixes_are_skipped_by_automatic_allocation() {
        // The first link names the prefix automatic allocation would otherwise
        // hand to the second.
        let topology = topology(
            &[
                ("leaf1", Role::FrrRouter),
                ("leaf2", Role::FrrRouter),
                ("spine1", Role::FrrRouter),
            ],
            &[
                ("leaf1:eth1", "spine1:eth1", Some("10.0.0.0/31")),
                ("leaf2:eth1", "spine1:eth2", None),
            ],
        );

        let plan = allocate(&topology).expect("mixed explicit/automatic must allocate");
        assert_ne!(
            plan.links[0].subnet, plan.links[1].subnet,
            "the derived link must not reuse the explicit link's prefix"
        );
        verify(&plan).expect("the plan it produces must pass its own verification");
    }

    #[test]
    fn overlapping_prefixes_are_rejected_even_when_they_differ() {
        // A /29 and a /31 inside it are different strings that cover the same
        // addresses — the case a set-of-strings check misses entirely.
        assert!(overlaps(
            ("10.0.0.0".parse().unwrap(), 29),
            ("10.0.0.0".parse().unwrap(), 31)
        ));
        assert!(!overlaps(
            ("10.0.0.0".parse().unwrap(), 31),
            ("10.0.0.2".parse().unwrap(), 31)
        ));
    }

    #[test]
    fn verify_catches_a_hand_built_collision() {
        let mut plan = allocate(&clos()).unwrap();
        // Force the two links onto the same addresses.
        plan.links[1].a.addr = plan.links[0].a.addr;
        assert!(verify(&plan).is_err());

        let mut plan = allocate(&clos()).unwrap();
        plan.links[1].subnet = plan.links[0].subnet.clone();
        assert!(verify(&plan).unwrap_err().to_string().contains("overlaps"));
    }

    #[test]
    fn cidr_parsing_is_strict() {
        assert_eq!(
            parse_v4_cidr("10.0.0.0/16"),
            Some(("10.0.0.0".parse().unwrap(), 16))
        );
        for bad in ["10.0.0.0", "10.0.0.0/33", "nonsense/8", "10.0.0.0/x", ""] {
            assert!(parse_v4_cidr(bad).is_none(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn describes_an_endpoint_or_says_it_is_unassigned() {
        let plan = allocate(&clos()).unwrap();
        assert!(describe(&plan, &Endpoint::parse("leaf1:eth1").unwrap()).contains('/'));
        assert_eq!(
            describe(&plan, &Endpoint::parse("leaf1:eth9").unwrap()),
            "unassigned"
        );
    }
}
