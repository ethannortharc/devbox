//! nftables ruleset generation — the enforcement half of §8.
//!
//! The Rust control plane decides *what* the rules should be; the agent inside
//! the box applies them. Generating the ruleset as text here means the hard
//! part — the rules themselves — is unit-tested on any host, and the agent's
//! job shrinks to `nft -f -` plus keeping a named set in sync with DNS answers.
//!
//! The DNS-driven part is why sets exist: the allowlist names domains, and a
//! domain's addresses change. The agent adds each resolved answer to
//! `allow_v4`/`allow_v6` as it sees it, so the ruleset itself never has to be
//! regenerated for a CDN rotation.

use std::fmt::Write as _;

use super::{Policy, Posture, parse_cidr};

/// The nftables table devbox owns.
///
/// Its own table, never `filter`: devbox must be able to flush and rebuild its
/// rules without touching anything the box's own configuration installed.
pub const TABLE: &str = "devbox";

/// The rules that implement a posture, emitted into whichever chain.
///
/// Shared by `output` and `forward` so the two can never drift: a rule added
/// to one and forgotten in the other is a hole shaped exactly like the
/// container-egress bypass this was factored out to fix.
fn emit_policy_rules(nft: &mut String, policy: &Policy, ctx: &Context) {
    // Established traffic first: a reply to a connection we already allowed
    // must not be re-evaluated, and putting this rule anywhere but first costs
    // a lookup on every packet.
    let _ = writeln!(nft, "    ct state established,related accept");

    // Loopback and link-local are never egress. Blocking them breaks the box's
    // own services and its neighbour discovery — and `Policy::evaluate` already
    // treats both as local, so omitting them here made `devbox policy test`
    // report "allowed" for an address the box would then drop.
    let _ = writeln!(nft, "    oifname \"lo\" accept");
    let _ = writeln!(nft, "    ip daddr 127.0.0.0/8 accept");
    let _ = writeln!(nft, "    ip6 daddr ::1 accept");
    let _ = writeln!(nft, "    ip daddr 169.254.0.0/16 accept");
    let _ = writeln!(nft, "    ip6 daddr fe80::/10 accept");

    // DNS must survive every posture except `isolated`: the allowlist is
    // resolved by name, so blocking resolution would make the allowlist
    // unenforceable rather than strict.
    //
    // Scoped to the resolvers the box actually uses. Accepting port 53 to any
    // destination made the port itself a bypass: a process could reach an
    // unlisted endpoint by speaking anything at all to :53. With no resolver
    // discovered, nothing is exempted — a policy that cannot resolve is
    // visibly broken, which is safer than one that quietly is not enforced.
    if policy.egress != Posture::Isolated {
        for resolver in &ctx.resolvers {
            let family = if resolver.contains(':') { "ip6" } else { "ip" };
            let _ = writeln!(nft, "    {family} daddr {resolver} udp dport 53 accept");
            let _ = writeln!(nft, "    {family} daddr {resolver} tcp dport 53 accept");
        }
        if ctx.resolvers.is_empty() {
            let _ = writeln!(
                nft,
                "    # no resolver found in /etc/resolv.conf; DNS is not exempted"
            );
        }
    }

    match policy.egress {
        Posture::Open => {
            let _ = writeln!(nft, "    # posture is open: nothing is blocked");
        }
        Posture::Isolated => {
            // This lab's subnets stay reachable: the posture means "no
            // egress", not "no networking". *This lab's* — the previous
            // blanket RFC 1918 exemption let an isolated box reach the home or
            // corporate LAN behind it, which is egress by any reading.
            for prefix in &ctx.lab_prefixes {
                let family = if prefix.contains(':') { "ip6" } else { "ip" };
                let _ = writeln!(nft, "    {family} daddr {prefix} accept");
            }
            if ctx.lab_prefixes.is_empty() {
                let _ = writeln!(nft, "    # no lab on this box: loopback only");
            }
            let _ = writeln!(nft, "    log prefix \"devbox-blocked \" level info");
        }
        Posture::Allowlist | Posture::MirrorOnly => {
            let _ = writeln!(nft, "    ip daddr @{SET_STATIC_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_STATIC_V6} accept");
            let _ = writeln!(nft, "    ip daddr @{SET_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_V6} accept");
            // Logging is what turns a dropped packet into a `policy` event:
            // the agent tails these and emits one per blocked connection.
            let _ = writeln!(nft, "    log prefix \"devbox-blocked \" level info");
        }
    }
}

/// Named sets the agent populates from resolved DNS answers.
pub const SET_V4: &str = "allow_v4";
pub const SET_V6: &str = "allow_v6";

/// Sets holding the CIDRs the user stated, which never expire.
pub const SET_STATIC_V4: &str = "static_v4";
pub const SET_STATIC_V6: &str = "static_v6";

/// Chain holding the egress verdict for forwarded traffic.
pub const FORWARD_EGRESS: &str = "forward_egress";

/// How long a DNS-derived allow-set entry lives.
///
/// Long enough that an active session is not interrupted by an expiry between
/// two requests, short enough that a reassigned address stops being reachable
/// within an hour. Every fresh resolution refreshes it, so a domain in steady
/// use never lapses.
pub const ALLOW_TTL_SECS: u64 = 3600;

/// Generate the full ruleset for a policy.
///
/// The output is idempotent: it deletes the devbox table first, so applying it
/// twice leaves the same state as applying it once.
/// What the ruleset needs to know about the box it is being generated for.
///
/// Two exemptions used to be written as blanket rules because the generator
/// had no way to know the specifics — and a blanket exemption in a
/// default-deny firewall is a hole, not a convenience:
///
/// * DNS was accepted to *any* destination on port 53, so a process reached
///   an unlisted endpoint simply by using that port.
/// * `isolated` accepted all of RFC 1918 and ULA, so a box with a route to a
///   home or corporate LAN could reach the LAN router while the posture
///   promised lab-internal traffic only.
///
/// Both are now derived from the box. Empty means "none of these exist", which
/// is the strict reading and the right default.
#[derive(Debug, Default, Clone)]
pub struct Context {
    /// The resolvers this box is configured to use, from `/etc/resolv.conf`.
    pub resolvers: Vec<String>,
    /// Prefixes belonging to a lab running on this box.
    pub lab_prefixes: Vec<String>,
    /// Networks nested containers send from.
    ///
    /// Forwarded traffic is policed only when it *originates* here: everything
    /// else crossing the forward hook is inbound or lab-internal routing, and
    /// egress control is about where the box can reach, not who can reach it.
    pub container_prefixes: Vec<String>,
}

pub fn ruleset(policy: &Policy) -> String {
    ruleset_with(policy, &Context::default())
}

/// Generate the ruleset for a policy against a known box.
pub fn ruleset_with(policy: &Policy, ctx: &Context) -> String {
    let mut nft = String::new();

    let _ = writeln!(
        nft,
        "# Generated by devbox — egress posture: {}",
        policy.egress
    );
    let _ = writeln!(nft, "# Do not edit; regenerated on every policy change.");
    // `destroy` rather than `delete` so a first run does not fail on a table
    // that does not exist yet.
    let _ = writeln!(nft, "destroy table inet {TABLE}");
    let _ = writeln!(nft, "table inet {TABLE} {{");

    // Sets exist in every posture so the agent can populate them without
    // caring which posture is in force.
    // Four sets, not two.
    //
    // A set-level `timeout` is the *default* for elements that do not state
    // one — including the initializer elements below. Putting stated CIDRs and
    // DNS-derived addresses in one timed set therefore expired the CIDRs after
    // an hour, and nothing repopulates them: a CIDR-only allowlist would start
    // working and then quietly stop. Static and inferred entries have opposite
    // lifetimes, so they get their own sets.
    let _ = writeln!(nft, "  set {SET_STATIC_V4} {{");
    let _ = writeln!(nft, "    type ipv4_addr");
    let _ = writeln!(nft, "    flags interval");
    if !cidrs_v4(policy).is_empty() {
        let _ = writeln!(nft, "    elements = {{ {} }}", cidrs_v4(policy).join(", "));
    }
    let _ = writeln!(nft, "  }}");

    let _ = writeln!(nft, "  set {SET_STATIC_V6} {{");
    let _ = writeln!(nft, "    type ipv6_addr");
    let _ = writeln!(nft, "    flags interval");
    if !cidrs_v6(policy).is_empty() {
        let _ = writeln!(nft, "    elements = {{ {} }}", cidrs_v6(policy).join(", "));
    }
    let _ = writeln!(nft, "  }}");

    // The agent's sets: every element ages out unless a fresh DNS answer
    // refreshes it. No `interval`, because these hold single addresses.
    let _ = writeln!(nft, "  set {SET_V4} {{");
    let _ = writeln!(nft, "    type ipv4_addr");
    let _ = writeln!(nft, "    flags timeout");
    let _ = writeln!(nft, "    timeout {ALLOW_TTL_SECS}s");
    let _ = writeln!(nft, "  }}");

    let _ = writeln!(nft, "  set {SET_V6} {{");
    let _ = writeln!(nft, "    type ipv6_addr");
    let _ = writeln!(nft, "    flags timeout");
    let _ = writeln!(nft, "    timeout {ALLOW_TTL_SECS}s");
    let _ = writeln!(nft, "  }}");

    let verdict = if policy.egress.enforces() {
        "drop"
    } else {
        "accept"
    };
    // Two hooks, one policy.
    //
    // `output` covers packets the box itself sends. It does not cover packets
    // it *forwards* — and with the `container` set enabled, everything a
    // nested Docker container sends is forwarded, not output. So `docker run
    // … curl` walked straight past `isolated` and every allowlist: the one
    // command a developer is most likely to run inside a sandboxed box was the
    // one the sandbox did not cover.
    // `output` covers packets the box sends. `forward` covers packets it
    // routes — which, with the `container` set, is everything a nested Docker
    // container sends, and is why an output-only ruleset let `docker run …
    // curl` ignore every posture.
    //
    // But `forward` also carries *inbound* traffic: a published container port
    // arrives DNATed and traverses this hook as `ct state new` toward an
    // address that is not in any egress allow set. Filtering it identically
    // dropped the first SYN of every inbound connection to a published
    // service. Egress control is about where the box can *reach*, not who can
    // reach it, so the forward chain polices only what leaves.
    let _ = writeln!(nft, "  chain output {{");
    let _ = writeln!(
        nft,
        "    type filter hook output priority filter; policy {verdict};"
    );
    emit_policy_rules(&mut nft, policy, ctx);
    let _ = writeln!(nft, "  }}");

    let _ = writeln!(nft, "  chain forward {{");
    let _ = writeln!(
        nft,
        "    type filter hook forward priority filter; policy accept;"
    );
    let _ = writeln!(nft, "    ct state established,related accept");

    // Matched by *interface*, not by subnet.
    //
    // A subnet snapshot is only true at the moment it is taken: `docker
    // compose up` after the policy was applied creates a bridge the ruleset
    // has never heard of, and its traffic then missed every jump and was
    // accepted by the chain's default. Docker's bridges are named `docker0`
    // and `br-<id>`, and an interface pattern covers the ones that do not
    // exist yet — which is the whole population that mattered.
    //
    // The discovered subnets stay as a second match: a user-defined network
    // with a custom bridge name would otherwise escape the pattern.
    let _ = writeln!(nft, "    iifname \"docker0\" jump {FORWARD_EGRESS}");
    let _ = writeln!(nft, "    iifname \"br-*\" jump {FORWARD_EGRESS}");
    for prefix in &ctx.container_prefixes {
        let family = if prefix.contains(':') { "ip6" } else { "ip" };
        let _ = writeln!(nft, "    {family} saddr {prefix} jump {FORWARD_EGRESS}");
    }
    let _ = writeln!(nft, "  }}");

    // The egress verdict for forwarded traffic, reached only from the jumps
    // above so inbound connections never see it.
    let _ = writeln!(nft, "  chain {FORWARD_EGRESS} {{");
    emit_policy_rules(&mut nft, policy, ctx);
    if policy.egress.enforces() {
        let _ = writeln!(nft, "    drop");
    }
    let _ = writeln!(nft, "  }}");

    let _ = writeln!(nft, "}}");
    nft
}

/// Commands that add resolved addresses to the allow sets.
///
/// This is the DNS-driven half: the agent calls this for each answer it sees
/// for an allowlisted name, so a CDN rotation needs no ruleset regeneration.
pub fn add_elements(addrs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for addr in addrs {
        match addr.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(a)) => {
                out.push(format!("add element inet {TABLE} {SET_V4} {{ {a} }}"))
            }
            Ok(std::net::IpAddr::V6(a)) => {
                out.push(format!("add element inet {TABLE} {SET_V6} {{ {a} }}"))
            }
            // Anything unparseable is skipped rather than interpolated: this
            // string is handed to a command that runs as root.
            Err(_) => continue,
        }
    }
    out
}

fn cidrs_v4(policy: &Policy) -> Vec<String> {
    policy
        .cidrs()
        .into_iter()
        .filter(|c| parse_cidr(c).is_some_and(|(a, _)| a.is_ipv4()))
        .collect()
}

fn cidrs_v6(policy: &Policy) -> Vec<String> {
    policy
        .cidrs()
        .into_iter()
        .filter(|c| parse_cidr(c).is_some_and(|(a, _)| a.is_ipv6()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(egress: Posture, allow: &[&str]) -> Policy {
        Policy {
            egress,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            alert_on_violation: true,
        }
    }

    #[test]
    fn open_accepts_by_default() {
        let nft = ruleset(&policy(Posture::Open, &[]));
        assert!(nft.contains("policy accept;"));
        assert!(!nft.contains("policy drop;"));
        assert!(nft.contains("posture is open"));
    }

    #[test]
    fn enforcing_postures_default_to_drop() {
        for posture in [Posture::Allowlist, Posture::MirrorOnly, Posture::Isolated] {
            let nft = ruleset(&policy(posture, &[]));
            assert!(nft.contains("policy drop;"), "{posture} must default-deny");
        }
    }

    #[test]
    fn the_ruleset_is_idempotent() {
        // Applying twice must leave the same state as applying once.
        let nft = ruleset(&policy(Posture::Allowlist, &["github.com"]));
        assert!(
            nft.contains(&format!("destroy table inet {TABLE}")),
            "the table must be torn down before it is rebuilt"
        );
    }

    #[test]
    fn devbox_owns_its_own_table() {
        // Never `filter`: devbox must be able to flush its rules without
        // touching whatever the box's own configuration installed.
        let nft = ruleset(&policy(Posture::Allowlist, &[]));
        assert!(nft.contains("table inet devbox {"));
        assert!(!nft.contains("table inet filter"));
    }

    #[test]
    fn established_traffic_is_accepted_first() {
        let nft = ruleset(&policy(Posture::Allowlist, &[]));
        let chain = nft.split("chain output {").nth(1).unwrap();
        let ct = chain.find("ct state established").unwrap();
        let set = chain.find("@allow_v4").unwrap();
        assert!(
            ct < set,
            "conntrack must come before the set lookup, or every packet pays for it"
        );
    }

    #[test]
    fn loopback_survives_every_posture() {
        for posture in Posture::ALL {
            let nft = ruleset(&policy(*posture, &[]));
            assert!(
                nft.contains("oifname \"lo\" accept"),
                "{posture} must not break the box's own services"
            );
            assert!(nft.contains("ip daddr 127.0.0.0/8 accept"));
        }
    }

    #[test]
    fn dns_survives_every_posture_except_isolated() {
        let ctx = Context {
            resolvers: vec!["192.0.2.53".into()],
            ..Default::default()
        };
        for posture in [Posture::Open, Posture::Allowlist, Posture::MirrorOnly] {
            let nft = ruleset_with(&policy(posture, &[]), &ctx);
            assert!(
                nft.contains("ip daddr 192.0.2.53 udp dport 53 accept"),
                "{posture} resolves the allowlist by name, so DNS must work"
            );
            // To the resolver, not to the port. Accepting :53 anywhere made
            // the port itself an exit.
            assert!(
                !nft.contains("    udp dport 53 accept"),
                "{posture} must not exempt port 53 to arbitrary destinations"
            );
        }
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        assert!(
            !nft.contains("udp dport 53 accept"),
            "isolated means isolated"
        );
    }

    #[test]
    fn container_egress_is_policed_but_inbound_is_not() {
        // Two failures, one chain. An output-only ruleset let `docker run …
        // curl` bypass every posture; filtering `forward` identically then
        // dropped the first SYN of every inbound connection to a published
        // container port, which arrives DNATed as `ct state new` toward an
        // address no egress rule matches.
        //
        // Egress control is about where the box can reach, not who can reach
        // it — so only traffic *originating* in a container is judged.
        let ctx = Context {
            resolvers: vec!["192.0.2.53".into()],
            container_prefixes: vec!["172.17.0.0/16".into()],
            ..Default::default()
        };
        let nft = ruleset_with(&policy(Posture::Allowlist, &["10.0.0.0/8"]), &ctx);

        let forward = nft.split("chain forward {").nth(1).unwrap();
        let forward = forward.split("  }").next().unwrap();

        // The chain itself must not default-deny, or inbound dies.
        assert!(forward.contains("policy accept;"));
        assert!(forward.contains("ct state established,related accept"));
        // Container-sourced traffic is sent to the egress verdict.
        assert!(forward.contains("ip saddr 172.17.0.0/16 jump forward_egress"));

        // And that verdict really does deny: an allowlist that only accepts is
        // not an allowlist.
        let egress = nft.split("chain forward_egress {").nth(1).unwrap();
        let egress = egress.split("  }").next().unwrap();
        assert!(egress.contains("@static_v4 accept"));
        assert!(egress.contains("@allow_v4 accept"));
        assert!(egress.contains("drop"));
    }

    #[test]
    fn container_bridges_are_policed_before_they_exist() {
        // A subnet snapshot is true only when it is taken: `docker compose up`
        // after the policy was applied creates a bridge the ruleset has never
        // seen, and its traffic then met no jump at all. Interface patterns
        // cover the networks that do not exist yet, which was the entire
        // population that mattered.
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        let forward = nft.split("chain forward {").nth(1).unwrap();
        let forward = forward.split("  }").next().unwrap();

        assert!(forward.contains("iifname \"docker0\" jump forward_egress"));
        assert!(
            forward.contains("iifname \"br-*\" jump forward_egress"),
            "user-defined networks get br-<id> bridges: {forward}"
        );
        // Inbound is still not egress.
        assert!(forward.contains("policy accept;"));
        assert!(forward.contains("ct state established,related accept"));
    }

    #[test]
    fn isolated_ignores_the_allowlist_entirely() {
        // docs/observability.md promises `isolated` blocks everything
        // "including for hosts you explicitly allowlisted". That is a strong
        // claim about a security control, and it held only because this match
        // arm happens not to mention the allow sets — nothing stated it. An
        // edit that let isolated fall through to the allowlist arm would break
        // the promise silently, which is exactly how the round-8 CIDR-expiry
        // bug got in.
        let nft = ruleset(&policy(
            Posture::Isolated,
            &["github.com", "203.0.113.0/24"],
        ));
        let chain = nft.split("chain output {").nth(1).unwrap();

        assert!(
            !chain.contains("@allow_v4"),
            "isolated consulted DNS answers"
        );
        assert!(
            !chain.contains("@static_v4"),
            "isolated consulted the CIDRs"
        );
        assert!(
            !chain.contains("203.0.113.0/24"),
            "the allowlisted CIDR leaked into the chain"
        );
    }

    #[test]
    fn isolated_permits_this_labs_subnets_and_no_others() {
        let ctx = Context {
            lab_prefixes: vec!["10.99.0.0/16".into()],
            ..Default::default()
        };
        let nft = ruleset_with(&policy(Posture::Isolated, &[]), &ctx);
        assert!(nft.contains("ip daddr 10.99.0.0/16 accept"));

        // Not all of RFC 1918. A box with a route to a home or corporate LAN
        // could otherwise reach its router while the posture promised
        // lab-internal traffic only — which is egress by any reading.
        assert!(!nft.contains("172.16.0.0/12"));
        assert!(!nft.contains("192.168.0.0/16"));
        assert!(!nft.contains("fc00::/7"));
    }

    #[test]
    fn isolated_without_a_lab_permits_nothing_beyond_loopback() {
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        let chain = nft.split("chain output {").nth(1).unwrap();
        assert!(
            chain.contains("127.0.0.0/8 accept"),
            "loopback is not egress"
        );
        assert!(chain.contains("no lab on this box"));
        assert!(!chain.contains("10.0.0.0/8"));
    }

    #[test]
    fn stated_cidrs_never_expire_but_resolved_addresses_do() {
        // A set-level timeout is the default for its initializer elements, so
        // one shared set made stated CIDRs expire after an hour with nothing
        // to repopulate them: a CIDR-only allowlist that stopped working.
        let nft = ruleset(&policy(Posture::Allowlist, &["10.0.0.0/8", "github.com"]));

        let statics = nft.split("set static_v4 {").nth(1).unwrap();
        let statics = statics.split('}').next().unwrap();
        assert!(statics.contains("10.0.0.0/8"));
        assert!(
            !statics.contains("timeout"),
            "a stated CIDR is a decision, not an observation: {statics}"
        );

        let resolved = nft.split("set allow_v4 {").nth(1).unwrap();
        let resolved = resolved.split('}').next().unwrap();
        assert!(resolved.contains("timeout"));
        assert!(
            !resolved.contains("elements"),
            "the agent fills this set; nothing is seeded into it: {resolved}"
        );

        // Both are consulted, or half the allowlist silently does nothing.
        let chain = nft.split("chain output {").nth(1).unwrap();
        assert!(chain.contains("@static_v4 accept"));
        assert!(chain.contains("@allow_v4 accept"));
    }

    #[test]
    fn cidrs_seed_the_right_set_by_family() {
        let nft = ruleset(&policy(
            Posture::Allowlist,
            &["10.0.0.0/8", "2001:db8::/32", "github.com"],
        ));

        // Stated CIDRs go in the *static* sets, by family.
        let v4 = nft.split("set static_v4 {").nth(1).unwrap();
        let v4 = v4.split('}').next().unwrap();
        assert!(v4.contains("10.0.0.0/8"));
        assert!(!v4.contains("2001:db8::/32"));

        let v6 = nft.split("set static_v6 {").nth(1).unwrap();
        let v6 = v6.split('}').next().unwrap();
        assert!(v6.contains("2001:db8::/32"));
        assert!(!v6.contains("10.0.0.0/8"));

        // A domain is not a set element; the agent adds its answers at runtime.
        assert!(!nft.contains("github.com"));
    }

    #[test]
    fn sets_exist_even_when_empty() {
        // The agent populates them without caring which posture is in force.
        let nft = ruleset(&policy(Posture::Open, &[]));
        assert!(nft.contains("set allow_v4 {"));
        assert!(nft.contains("set allow_v6 {"));
        assert!(nft.contains("flags interval"), "CIDRs need interval sets");
    }

    #[test]
    fn blocked_packets_are_logged_so_they_can_become_events() {
        for posture in [Posture::Allowlist, Posture::MirrorOnly, Posture::Isolated] {
            let nft = ruleset(&policy(posture, &[]));
            assert!(
                nft.contains("log prefix \"devbox-blocked \""),
                "{posture} must log drops; that is what becomes a policy event"
            );
        }
        assert!(!ruleset(&policy(Posture::Open, &[])).contains("devbox-blocked"));
    }

    #[test]
    fn resolved_answers_become_set_elements_by_family() {
        let cmds = add_elements(&[
            "151.101.0.223".into(),
            "2606:4700::1".into(),
            "not-an-address".into(),
        ]);

        assert_eq!(cmds.len(), 2, "garbage is skipped, not interpolated");
        assert!(cmds[0].contains("allow_v4"));
        assert!(cmds[0].contains("151.101.0.223"));
        assert!(cmds[1].contains("allow_v6"));
        assert!(cmds[1].contains("2606:4700::1"));
    }

    #[test]
    fn a_malicious_dns_answer_cannot_inject_a_command() {
        // These strings would be handed to a command running as root.
        let cmds = add_elements(&[
            "1.2.3.4; nft flush ruleset".into(),
            "$(reboot)".into(),
            "}; add rule inet devbox output accept; #".into(),
        ]);
        assert!(cmds.is_empty(), "nothing unparseable may reach nft");
    }

    #[test]
    fn the_ruleset_is_balanced_and_parseable_looking() {
        let nft = ruleset(&policy(Posture::Allowlist, &["10.0.0.0/8"]));
        let opens = nft.matches('{').count();
        let closes = nft.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces:\n{nft}");
        assert!(nft.ends_with("}\n"));
    }
}
