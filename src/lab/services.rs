//! Service and zero-touch plans derived from a lab topology.
//!
//! The output is deliberately just files and values.  The CLI owns the
//! runtime side effects; this module keeps the DHCP ranges, serial catalog,
//! and node bootstrap inputs deterministic and unit-testable.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::topology::Role;
use super::{Lab, wiring};

pub const ZTP_DNS_NAME: &str = "ztp.devbox";

/// The domain [`ZTP_DNS_NAME`] sits in, declared local so the lab's DNS
/// answers for it authoritatively instead of trying to forward.
pub const ZTP_DOMAIN: &str = "devbox";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePlan {
    pub node: String,
    pub address: Ipv4Addr,
    pub interfaces: Vec<String>,
    pub dnsmasq: Option<String>,
    pub chrony: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZtpPlan {
    pub service: ServicePlan,
    pub boot_url: String,
    pub serials_json: String,
    pub blanks: Vec<BlankPlan>,
    pub dhcp_servers: Vec<DhcpServerPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlankPlan {
    pub node: String,
    pub serial: String,
    pub iface: String,
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub router: String,
    pub frr_config: String,
    pub dhcp_hook: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpServerPlan {
    pub node: String,
    pub config: String,
}

#[derive(Serialize)]
struct Identity<'a> {
    #[serde(rename = "Name")]
    name: &'a str,
    #[serde(rename = "Role")]
    role: &'static str,
}

impl ServicePlan {
    pub fn from_lab(lab: &Lab) -> Result<Option<Self>> {
        if !lab.topology.services.dns
            && !lab.topology.services.dhcp
            && !lab.topology.services.ntp
            && !lab
                .topology
                .nodes
                .iter()
                .any(|node| node.role == Role::ZtpBlank)
        {
            return Ok(None);
        }
        let Some(node) = lab
            .topology
            .nodes
            .iter()
            .find(|node| node.role == Role::Service)
            .or_else(|| {
                // The design's Clos example enables shared DNS/NTP without a
                // service role. Pick a deterministic configured node instead
                // of silently ignoring the declaration. ZTP remains stricter
                // and validation requires its dedicated service node.
                lab.topology
                    .nodes
                    .iter()
                    .find(|node| node.role != Role::ZtpBlank)
            })
        else {
            return Ok(None);
        };
        let addresses = lab.plan.addrs_of(&node.name);
        let address = addresses
            .first()
            .context("the service node has no address")?
            .addr;
        let mut interfaces: Vec<String> = addresses
            .iter()
            .map(|address| address.iface.clone())
            .collect();
        interfaces.sort();
        interfaces.dedup();

        let run = format!("/run/devbox/lab/{}/{}", lab.name(), node.name);
        let dnsmasq = lab.topology.services.dns.then(|| {
            let mut config =
                String::from("no-resolv\nno-hosts\nbind-interfaces\ndomain-needed\nbogus-priv\n");
            for iface in &interfaces {
                config.push_str(&format!("interface={iface}\n"));
            }
            config.push_str(&format!("listen-address={address}\n"));

            // `host-record` and `local`, not `address`.
            //
            // Between them they are what makes a query for a name the lab
            // knows behave the way DNS is supposed to. `address=/name/v4`
            // answers A and then *forwards* every other type — and this server
            // has `no-resolv` with no upstream, so an AAAA query came back
            // REFUSED. Any resolver that asks for A and AAAA together, which
            // is every stock one, took the refusal as failure and reported a
            // name it had just successfully resolved as missing. That is how
            // three healthy nodes failed ZTP on `DNS self-check failed`.
            //
            // `local` alone turns the refusal into NXDOMAIN — still wrong, and
            // still fatal to a dual query: the name does exist. `host-record`
            // makes it a real host record, so a type the name has no data for
            // answers NODATA, which is the true answer and the one every
            // resolver handles.
            for domain in ["lab", ZTP_DOMAIN] {
                config.push_str(&format!("local=/{domain}/\n"));
            }
            for (node, node_address) in lab.reachable_addrs() {
                config.push_str(&format!("host-record={node}.lab,{node_address}\n"));
            }
            config.push_str(&format!(
                "host-record={ZTP_DNS_NAME},{address}\n\
                 pid-file={run}/dnsmasq.pid\nlog-facility={run}/dnsmasq.log\n"
            ));
            config
        });
        let chrony = lab.topology.services.ntp.then(|| {
            format!(
                "local stratum 10\nallow {}\nbindaddress {address}\n\
                 pidfile {run}/chronyd.pid\ndriftfile {run}/chrony.drift\n\
                 logdir {run}\nmakestep 1.0 3\n",
                lab.topology.lab.base
            )
        });

        Ok(Some(Self {
            node: node.name.clone(),
            address,
            interfaces,
            dnsmasq,
            chrony,
        }))
    }
}

impl ZtpPlan {
    pub fn from_lab(lab: &Lab) -> Result<Option<Self>> {
        let blank_nodes: Vec<_> = lab
            .topology
            .nodes
            .iter()
            .filter(|node| node.role == Role::ZtpBlank)
            .collect();
        if blank_nodes.is_empty() {
            return Ok(None);
        }
        let service = ServicePlan::from_lab(lab)?.context("a ZTP topology has no service node")?;
        let boot_url = format!("http://{}:8080/bootstrap.sh", service.address);
        let configs: BTreeMap<String, String> = lab.router_configs().into_iter().collect();
        let mut identities = BTreeMap::new();
        let mut blanks = Vec::with_capacity(blank_nodes.len());

        for node in blank_nodes {
            let link = lab
                .plan
                .links
                .iter()
                .find(|link| link.a.node == node.name || link.b.node == node.name)
                .with_context(|| format!("ZTP node '{}' has no link plan", node.name))?;
            let (blank, peer) = if link.a.node == node.name {
                (&link.a, &link.b)
            } else {
                (&link.b, &link.a)
            };
            if blank.prefix_len >= 31 {
                bail!(
                    "ZTP link {} needs at least a /30: DHCP needs distinct network, router, client, and broadcast addresses",
                    link.subnet
                );
            }
            let serial = format!("devbox:{}:{}", lab.name(), node.name);
            identities.insert(
                serial.clone(),
                Identity {
                    name: &node.name,
                    role: "leaf",
                },
            );
            let frr_config = configs
                .get(&node.name)
                .with_context(|| format!("ZTP node '{}' has no rendered FRR config", node.name))?
                .clone();
            let lease_file = format!("/run/devbox/lab/{}/{}/dhcp.env", lab.name(), node.name);
            let dhcp_hook = format!(
                "#!/bin/sh\nset -eu\ncase \"$1\" in\n  bound|renew)\n\
                 [ \"${{ip:-}}\" = \"{}\" ] || {{ echo \"unexpected DHCP address ${{ip:-missing}}\" >&2; exit 1; }}\n\
                 ip -4 addr flush dev {}\n  ip addr add {}/{} dev {}\n\
                 ip route replace default via {} dev {}\n\
                 umask 077\n  printf 'boot_url=%s\\nserver=%s\\n' \"${{boot_file:-${{bootfile:-}}}}\" \"${{siaddr:-${{serverid:-}}}}\" > {}\n\
                 ;;\n  deconfig) ip -4 addr flush dev {} ;;\nesac\n",
                blank.addr,
                blank.iface,
                blank.addr,
                blank.prefix_len,
                blank.iface,
                peer.addr,
                blank.iface,
                lease_file,
                blank.iface,
            );
            blanks.push(BlankPlan {
                node: node.name.clone(),
                serial,
                iface: blank.iface.clone(),
                address: blank.addr,
                prefix_len: blank.prefix_len,
                gateway: peer.addr,
                router: peer.node.clone(),
                frr_config,
                dhcp_hook,
            });
        }

        let serials_json = serde_json::to_string_pretty(&identities)? + "\n";
        let mut routers = BTreeSet::new();
        routers.extend(blanks.iter().map(|blank| blank.router.clone()));
        let dhcp_servers = routers
            .into_iter()
            .map(|router| DhcpServerPlan {
                config: dhcp_config(lab, &service, &boot_url, &router, &blanks),
                node: router,
            })
            .collect();

        Ok(Some(Self {
            service,
            boot_url,
            serials_json,
            blanks,
            dhcp_servers,
        }))
    }
}

fn dhcp_config(
    lab: &Lab,
    service: &ServicePlan,
    boot_url: &str,
    router: &str,
    blanks: &[BlankPlan],
) -> String {
    let run = format!("/run/devbox/lab/{lab}/{router}", lab = lab.name());
    let mut config = format!(
        "port=0\nbind-interfaces\ndhcp-authoritative\nno-ping\n\
         pid-file={run}/dnsmasq-dhcp.pid\n\
         dhcp-leasefile={run}/dnsmasq.leases\n\
         log-facility={run}/dnsmasq-dhcp.log\n"
    );
    for blank in blanks.iter().filter(|blank| blank.router == router) {
        let router_iface = lab
            .plan
            .links
            .iter()
            .find_map(|link| {
                [&link.a, &link.b]
                    .into_iter()
                    .find(|end| end.node == router && end.addr == blank.gateway)
            })
            .expect("validated blank link has a router endpoint");
        let tag = &blank.node;
        config.push_str(&format!(
            "interface={}\n\
             dhcp-range=set:{tag},{},{},{},1h\n\
             dhcp-option=tag:{tag},3,{}\n\
             dhcp-option=tag:{tag},6,{}\n\
             dhcp-option=tag:{tag},42,{}\n\
             dhcp-option=tag:{tag},66,{}\n\
             dhcp-option=tag:{tag},67,{boot_url}\n",
            router_iface.iface,
            blank.address,
            blank.address,
            netmask(blank.prefix_len),
            blank.gateway,
            service.address,
            service.address,
            service.address,
        ));
    }
    config
}

fn netmask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ipv4Addr::from(bits)
}

pub fn config_dir(lab: &str, node: &str) -> String {
    format!("/etc/devbox/lab/{lab}/{node}")
}

pub fn run_dir(lab: &str, node: &str) -> String {
    format!("/run/devbox/lab/{lab}/{node}")
}

pub fn namespace(lab: &str, node: &str) -> String {
    wiring::netns(lab, node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ztp_plan_contains_real_dhcp_and_every_blank_config() {
        let lab = Lab::resolve("ztp-fabric").unwrap();
        let plan = ZtpPlan::from_lab(&lab).unwrap().unwrap();

        assert_eq!(plan.blanks.len(), 3);
        assert_eq!(plan.dhcp_servers.len(), 1);
        assert!(plan.serials_json.contains("devbox:ztp-fabric:leaf1"));
        assert!(plan.serials_json.contains("\"Name\": \"leaf1\""));
        for blank in &plan.blanks {
            assert!(
                blank
                    .frr_config
                    .contains(&format!("hostname {}", blank.node))
            );
            assert!(blank.dhcp_hook.contains("unexpected DHCP address"));
            assert!(blank.prefix_len < 31);
        }
        let dhcp = &plan.dhcp_servers[0].config;
        assert!(dhcp.contains("dhcp-option=tag:leaf1,66,"));
        assert!(dhcp.contains("dhcp-option=tag:leaf1,67,http://"));
        assert_eq!(dhcp.matches("dhcp-range=").count(), 3);
    }

    #[test]
    fn service_plan_wires_dns_and_ntp_to_the_service_address() {
        let lab = Lab::resolve("ztp-fabric").unwrap();
        let plan = ServicePlan::from_lab(&lab).unwrap().unwrap();

        assert!(plan.dnsmasq.unwrap().contains(ZTP_DNS_NAME));
        assert!(plan.chrony.unwrap().contains("local stratum 10"));
    }

    /// A name the lab knows must answer, and must answer honestly.
    ///
    /// `address=/name/v4` answers A and forwards everything else — and this
    /// server has no upstream to forward to, so AAAA came back REFUSED and
    /// every stock resolver, which asks for both at once, called a resolvable
    /// name missing. `host-record` plus `local` gives A for A and NODATA for
    /// AAAA, which is what the zone actually is.
    #[test]
    fn dns_answers_a_and_says_nothing_rather_than_refusing() {
        let lab = Lab::resolve("ztp-fabric").unwrap();
        let plan = ServicePlan::from_lab(&lab).unwrap().unwrap();
        let config = plan.dnsmasq.expect("the scenario runs DNS");

        assert!(
            !config.contains("address=/"),
            "`address=` forwards every type it does not answer, and there is \
             no upstream to forward to:\n{config}"
        );
        assert!(
            config.contains(&format!("host-record={ZTP_DNS_NAME},")),
            "the ZTP name needs a host record:\n{config}"
        );
        for domain in ["lab", ZTP_DOMAIN] {
            assert!(
                config.contains(&format!("local=/{domain}/")),
                "`{domain}` must be declared local or an unanswerable type is \
                 refused rather than answered:\n{config}"
            );
        }
    }
}
