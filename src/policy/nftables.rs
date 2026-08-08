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

/// Rules that keep the *host* on the network, for the output chain only.
///
/// DHCP renewal goes to broadcast and neighbour discovery to `ff02::/16`, so
/// neither matches the unicast loopback and link-local exemptions — without
/// these an enforcing box loses its lease and drops off the network hours
/// later, which reads as anything except a firewall rule.
///
/// Deliberately *not* in `emit_policy_rules`: that is shared with the chain
/// that judges forwarded traffic, so putting them there let a nested container
/// send DHCP-shaped packets straight past an isolated posture. A rule that
/// exists for the host has no business applying to what the host routes.
fn emit_host_control_rules(nft: &mut String) {
    // Scoped to where a lease actually comes from.
    //
    // Matching on the port pair alone accepted 68→67 to *any* address, and a
    // box with passwordless sudo can bind port 68 — so the exemption was a
    // general-purpose UDP tunnel out of `isolated` and out of every allowlist,
    // sitting above the rules that were supposed to decide. The ports are what
    // DHCP looks like; they are not what makes it DHCP.
    //
    // Discovery and rebinding are broadcast and multicast, so they still work.
    // What this gives up is *unicast renewal* to a lease server whose address
    // the ruleset does not know: a client in RENEWING gets no reply and falls
    // back to broadcast REBINDING at T2, which is permitted here. A renewal
    // that takes until T2 is a cost worth paying for closing an arbitrary
    // egress path; a lease that never renews would not have been.
    let _ = writeln!(
        nft,
        "    ip daddr 255.255.255.255 udp sport 68 udp dport 67 accept"
    );
    let _ = writeln!(
        nft,
        "    ip6 daddr {{ ff02::1:2, ff05::1:3 }} udp sport 546 udp dport 547 accept"
    );
    // Typed, not a blanket multicast accept. nftables rules are alternatives,
    // so `ip6 daddr ff02::/16 accept` on its own permitted *every* protocol to
    // link-local multicast and the ICMPv6 rule below constrained nothing.
    let _ = writeln!(
        nft,
        "    ip6 daddr ff02::/16 icmpv6 type {{ nd-router-solicit, \
         nd-router-advert, nd-neighbor-solicit, nd-neighbor-advert, \
         mld-listener-query, mld-listener-report }} accept"
    );
    // Unicast neighbour discovery, by type rather than by prefix. A blanket
    // `ip6 daddr fe80::/10 accept` used to sit in `emit_policy_rules` and
    // permitted every protocol and port to the whole link-local range; what the
    // box actually needs is to answer and solicit its neighbours.
    let _ = writeln!(
        nft,
        "    ip6 daddr fe80::/10 icmpv6 type {{ nd-neighbor-solicit, \
         nd-neighbor-advert }} accept"
    );
}

/// How many refused connections a second may be logged, and the burst allowed
/// above it.
///
/// A ceiling, not a target: ordinary use produces a handful of these, and the
/// numbers exist so that pathological traffic cannot turn the journal into the
/// problem. Losing a log line under a flood is the right trade — by then the
/// first ones have already said what is happening.
const BLOCK_LOG_RATE: u32 = 10;
const BLOCK_LOG_BURST: u32 = 20;

/// Does this policy watch without blocking?
///
/// `open` with an allowlist and alerts on. An `open` posture with no allowlist
/// has nothing to compare against, and one with alerts off has asked not to be
/// told — in both cases there is nothing to install and the table is cleared.
pub fn audits(policy: &Policy) -> bool {
    policy.egress == Posture::Open && policy.alert_on_violation && !policy.allow.is_empty()
}

/// Prefixes the kernel stamps on a logged packet, and the agent reads back.
///
/// Two, because a refusal and an observation are different things and the
/// console shows them differently. Deriving one from the other would mean the
/// agent inferring the verdict from a posture it reads out of a separate file,
/// and two places having to agree is how most of this file's defects happened.
///
/// Must match `capture.BlockedPrefix` / `capture.FlaggedPrefix` in the agent.
pub const BLOCK_LOG_PREFIX: &str = "devbox-blocked";
pub const FLAG_LOG_PREFIX: &str = "devbox-flagged";

/// The rule that turns a refused connection into something reportable.
///
/// `ct state new` and a rate limit, because this chain sees packets and the
/// contract next to it is one event per refused *connection*. A TCP client
/// retransmits its SYN and a UDP flow simply keeps sending, so the unqualified
/// rule logged the same refusal over and over — which is both a false event
/// count downstream and, under sustained denied traffic, a way to fill the
/// journal from a box that is behaving exactly as configured.
fn emit_block_log(nft: &mut String) {
    emit_log(nft, BLOCK_LOG_PREFIX);
}

/// The same, for a posture that watches without blocking.
fn emit_flag_log(nft: &mut String) {
    emit_log(nft, FLAG_LOG_PREFIX);
}

fn emit_log(nft: &mut String, prefix: &str) {
    let _ = writeln!(
        nft,
        "    ct state new limit rate {BLOCK_LOG_RATE}/second burst {BLOCK_LOG_BURST} packets \
         log prefix \"{prefix} \" level info"
    );
}

/// The rules that implement a posture, emitted into whichever chain.
///
/// Shared by `output` and `forward` so the two can never drift: a rule added
/// to one and forgotten in the other is a hole shaped exactly like the
/// container-egress bypass this was factored out to fix.
///
/// The flip side is that anything emitted here applies to what the box
/// *routes* as well as what it sends. A rule that exists for the host's own
/// sake belongs in `emit_host_control_rules`, and getting that wrong is
/// precisely how the DHCP exemptions became a way through an isolated posture.
fn emit_policy_rules(nft: &mut String, policy: &Policy, ctx: &Context) {
    // Established traffic first: a reply to a connection we already allowed
    // must not be re-evaluated, and putting this rule anywhere but first costs
    // a lookup on every packet.
    let _ = writeln!(nft, "    ct state established,related accept");

    // Loopback only. It genuinely cannot leave the host, so exempting it
    // constrains nothing.
    //
    // Link-local used to be exempted here alongside it, on the reasoning that
    // both are "never egress". That is true of loopback and false of
    // link-local: `169.254.0.0/16` and `fe80::/10` leave the interface and
    // reach whatever answers on the segment. As unconditional accepts for
    // every protocol and port they were a hole straight through `isolated`,
    // `allowlist` and `mirror-only` — and on a cloud instance the hole has a
    // well-known address, `169.254.169.254`, which hands out instance
    // credentials to anything that asks.
    //
    // The box's own neighbour discovery is real and still needed, so it is
    // permitted by ICMPv6 type in `emit_host_control_rules` — the output-only
    // emitter, because a nested container has no business discovering the
    // host's neighbours.
    let _ = writeln!(nft, "    oifname \"lo\" accept");
    let _ = writeln!(nft, "    ip daddr 127.0.0.0/8 accept");
    let _ = writeln!(nft, "    ip6 daddr ::1 accept");

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
        Posture::Open if audits(policy) => {
            // Observe and warn — the step before enforcing, which the Policy
            // tab offers and `Policy::evaluate` implements with a `Flag`
            // verdict. Nothing produced a record of it: the table was cleared
            // for every `open` posture, so the one mode whose entire purpose
            // is to report reported nothing.
            //
            // The allowlist accepts come first so a permitted destination is
            // not logged, and there is no `drop` anywhere below them. `open`
            // blocks nothing; that is what makes auditing in it useful.
            let _ = writeln!(nft, "    ip daddr @{SET_STATIC_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_STATIC_V6} accept");
            let _ = writeln!(nft, "    ip daddr @{SET_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_V6} accept");
            emit_flag_log(nft);
        }
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
            emit_block_log(nft);
        }
        Posture::Allowlist | Posture::MirrorOnly => {
            let _ = writeln!(nft, "    ip daddr @{SET_STATIC_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_STATIC_V6} accept");
            let _ = writeln!(nft, "    ip daddr @{SET_V4} accept");
            let _ = writeln!(nft, "    ip6 daddr @{SET_V6} accept");
            // Logging is what turns a dropped packet into a `policy` event:
            // the agent tails these and emits one per blocked connection.
            emit_block_log(nft);
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
    /// Retained for the allow-set seeding; the forward chain no longer keys on
    /// them, because a snapshot cannot cover a network created later.
    pub container_prefixes: Vec<String>,
    /// Retained for compatibility; the forward chain no longer exempts
    /// interfaces at all.
    ///
    /// Lab traffic between two namespaces does not traverse the root forward
    /// hook — both veth ends live inside namespaces — so there was nothing for
    /// this to legitimately name.
    pub internal_ifaces: Vec<String>,
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
    emit_host_control_rules(&mut nft);
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
    // Inbound published ports, before the verdict.
    //
    // A DNATed connection to a nested container's published port arrives here
    // as `ct state new` toward an address no egress rule matches, so the
    // verdict below drops its first SYN and the service is unreachable. This
    // has now been broken twice by changes to this chain, so it is stated as
    // its own rule rather than left implicit: `ct status dnat` is exactly
    // "something outside asked for a port this box publishes", which is not
    // egress by any reading.
    let _ = writeln!(nft, "    ct status dnat accept");

    // Everything else forwarded is egress, with no interface exemptions.
    //
    // Enumerating container bridges cannot work — `--opt
    // com.docker.network.bridge.name=foo` names a bridge anything at all — so
    // the previous version inverted the test and exempted `dvb*`, devbox's own
    // veth names. That was worse than useless: lab wiring *moves* both veth
    // ends into node namespaces and renames them, so no interface at the root
    // keeps that name, while a Docker network created as `dvb0` would have
    // bypassed the posture entirely. An exemption for something that does not
    // exist is a bypass with no beneficiary.
    let _ = writeln!(nft, "    jump {FORWARD_EGRESS}");
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
    fn forwarded_traffic_is_egress_except_inbound_dnat() {
        // Three attempts at this chain, three bugs, so the shape is pinned.
        //
        // Filtering forward like output dropped inbound published ports.
        // Enumerating container bridges missed every custom-named one.
        // Exempting `dvb*` exempted a name no root interface ever has — a
        // bypass with no beneficiary, since lab wiring moves both veth ends
        // into namespaces.
        let ctx = Context {
            resolvers: vec!["192.0.2.53".into()],
            lab_prefixes: vec!["10.99.0.0/16".into()],
            ..Default::default()
        };
        let nft = ruleset_with(&policy(Posture::Allowlist, &["10.0.0.0/8"]), &ctx);

        let forward = nft.split("chain forward {").nth(1).unwrap();
        let forward = forward.split("  }").next().unwrap();

        assert!(forward.contains("ct state established,related accept"));
        // Someone outside reaching a port this box publishes is not egress.
        assert!(forward.contains("ct status dnat accept"));
        // And no interface is exempt.
        assert!(
            !forward.contains("iifname"),
            "an interface exemption is a bypass for anything that can take \
             that name: {forward}"
        );
        assert!(forward.trim_end().ends_with("jump forward_egress"));

        // Order matters: the DNAT accept has to precede the verdict.
        let dnat = forward.find("ct status dnat").unwrap();
        let jump = forward.find("jump forward_egress").unwrap();
        assert!(dnat < jump);
    }

    #[test]
    fn a_box_with_no_lab_judges_all_forwarded_traffic() {
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        let forward = nft.split("chain forward {").nth(1).unwrap();
        let forward = forward.split("  }").next().unwrap();

        assert!(
            !forward.contains("iifname"),
            "nothing is internal: {forward}"
        );
        assert!(forward.contains("jump forward_egress"));
        // And the verdict chain really denies.
        let egress = nft.split("chain forward_egress {").nth(1).unwrap();
        assert!(egress.split("  }").next().unwrap().contains("drop"));
    }

    #[test]
    fn an_open_posture_with_alerts_watches_without_blocking() {
        // Observe-and-warn is what the Policy tab offers as the step before
        // enforcing, and what `Policy::evaluate` answers with a `Flag`
        // verdict. Nothing produced a record of it: every `open` posture
        // cleared the table, so the one mode whose entire purpose is to report
        // reported nothing at all.
        let mut p = policy(Posture::Open, &["github.com"]);
        p.alert_on_violation = true;
        assert!(audits(&p));

        let nft = ruleset(&p);
        let output = nft.split("chain output {").nth(1).unwrap();
        let output = output.split("\n  }").next().unwrap();

        assert!(
            output.contains(FLAG_LOG_PREFIX),
            "an audited posture must log what the allowlist does not cover:\n{output}"
        );
        assert!(
            output.contains(&format!("@{SET_V4} accept")),
            "an allowlisted destination must not be flagged:\n{output}"
        );
        // And nothing is blocked — that is what `open` means, and what makes
        // auditing in it worth having.
        assert!(
            !output.contains("drop"),
            "`open` must not block, however loudly it reports:\n{output}"
        );
        assert!(
            !output.contains(BLOCK_LOG_PREFIX),
            "nothing was blocked, so nothing may claim to have been:\n{output}"
        );
        assert!(
            nft.contains("policy accept"),
            "the chain's default must stay accept:\n{nft}"
        );
    }

    #[test]
    fn an_open_posture_without_alerts_installs_nothing() {
        // The other half. `open` with no allowlist has nothing to compare
        // against, and `open` with alerts off has asked not to be told; both
        // still mean "no table", which is what `enforce::apply` keys on.
        assert!(!audits(&policy(Posture::Open, &[])));

        let mut alerts_off = policy(Posture::Open, &["github.com"]);
        alerts_off.alert_on_violation = false;
        assert!(!audits(&alerts_off));

        let mut no_list = policy(Posture::Open, &[]);
        no_list.alert_on_violation = true;
        assert!(!audits(&no_list));

        assert!(!ruleset(&policy(Posture::Open, &[])).contains(FLAG_LOG_PREFIX));
    }

    #[test]
    fn the_log_prefixes_match_what_the_agent_reads() {
        // Two languages have to agree on a string, and every previous instance
        // of that in this codebase has drifted: `not-found` against `missing`
        // in round 21, the allow-set TTL, the Ubuntu package mapping. The
        // failure is always silent — the kernel logs one thing, the agent
        // watches for another, and violations simply never appear, which looks
        // exactly like a box that behaved.
        let src = include_str!("../../agent/capture/blocked.go");
        for (name, value) in [
            ("BlockedPrefix", BLOCK_LOG_PREFIX),
            ("FlaggedPrefix", FLAG_LOG_PREFIX),
        ] {
            let expected = format!("{name} = \"{value}\"");
            assert!(
                src.contains(&expected),
                "agent/capture/blocked.go must declare `{expected}`; the kernel \
                 stamps what this file says and the agent reads what that one does"
            );
        }
    }

    #[test]
    fn a_refusal_is_logged_once_per_connection_not_once_per_packet() {
        // The comment beside this rule promises the agent emits one event per
        // blocked connection. The chain sees packets: a TCP client retransmits
        // its SYN and a UDP flow just keeps sending, so an unqualified `log`
        // fired for every one of them — a false event count downstream, and a
        // way for a box behaving exactly as configured to fill the journal.
        for posture in [Posture::Allowlist, Posture::MirrorOnly, Posture::Isolated] {
            let nft = ruleset(&policy(posture, &[]));
            let logged: Vec<&str> = nft
                .lines()
                .filter(|l| l.contains("devbox-blocked"))
                .collect();
            assert!(!logged.is_empty(), "{posture} must still report refusals");
            for line in logged {
                assert!(
                    line.contains("ct state new"),
                    "{posture}: a retransmission is not a new refusal: {line}"
                );
                assert!(
                    line.contains("limit rate"),
                    "{posture}: sustained denied traffic must not be able to \
                     fill the journal: {line}"
                );
            }
        }
    }

    #[test]
    fn network_control_traffic_survives_every_posture() {
        // A box that cannot renew its DHCP lease drops off the network hours
        // after the policy is applied, which looks like anything except a
        // firewall rule. Neither renewal (broadcast) nor neighbour discovery
        // (ff02::/16) matches the unicast loopback and link-local rules.
        for posture in [Posture::Allowlist, Posture::MirrorOnly, Posture::Isolated] {
            let nft = ruleset(&policy(posture, &[]));
            let chain = nft.split("chain output {").nth(1).unwrap();
            let chain = chain.split("  }").next().unwrap();

            // Scoped to where a lease comes from, not to a pair of port
            // numbers. Matching the ports alone accepted 68→67 to *any*
            // address, and a box with passwordless sudo can bind port 68 — so
            // the exemption was a general-purpose UDP way out of `isolated`,
            // sitting above the rules meant to decide. The previous version of
            // this assertion looked for the port pair, which the scoped rule
            // still contains, so it would have passed either way.
            assert!(
                chain.contains("ip daddr 255.255.255.255 udp sport 68 udp dport 67 accept"),
                "{posture} must permit DHCPv4 discovery and rebinding: {chain}"
            );
            assert!(
                chain.contains("ff02::1:2") && chain.contains("udp sport 546 udp dport 547 accept"),
                "{posture} must permit DHCPv6 to the servers multicast group: {chain}"
            );
            for line in chain.lines().filter(|l| l.contains("dport 67")) {
                assert!(
                    line.contains("daddr"),
                    "{posture}: a DHCP exemption with no destination is an \
                     arbitrary UDP egress path: {line}"
                );
            }
            assert!(
                chain.contains("nd-neighbor-solicit"),
                "{posture} must permit IPv6 neighbour discovery"
            );
        }

        // And nowhere near forwarded traffic. The previous version of this
        // assertion checked `chain forward`, which never carried them — the
        // leak was into `forward_egress`, the chain that actually judges. The
        // test passed while the bug it was written for was live, which is a
        // worse outcome than not having written it.
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        for chain in ["chain forward {", "chain forward_egress {"] {
            let body = nft.split(chain).nth(1).unwrap();
            let body = body.split("\n  }").next().unwrap();
            assert!(
                !body.contains("dport 67") && !body.contains("dport 547"),
                "{chain} must not carry host DHCP exceptions — a container \
                 would send DHCP-shaped packets straight past the posture:\n{body}"
            );
            assert!(
                !body.contains("ff02::/16"),
                "{chain} must not carry the host's multicast exception:\n{body}"
            );
        }

        // The blanket multicast accept is gone: nftables rules are
        // alternatives, so `ip6 daddr ff02::/16 accept` permitted every
        // protocol and the ICMPv6 rule after it constrained nothing.
        let output = nft.split("chain output {").nth(1).unwrap();
        let output = output.split("\n  }").next().unwrap();
        assert!(!output.contains("ff02::/16 accept"));
        assert!(output.contains("ff02::/16 icmpv6 type"));
    }

    #[test]
    fn link_local_is_judged_by_posture_not_waved_through() {
        // `ip daddr 169.254.0.0/16 accept` and `ip6 daddr fe80::/10 accept`
        // sat with the loopback exemptions under the heading "never egress".
        // Loopback cannot leave the host; link-local leaves the interface and
        // reaches whatever answers on the segment. As unconditional accepts
        // they were a bypass of every enforcing posture for every protocol and
        // port — and on a cloud instance the bypass has a famous address that
        // serves instance credentials to anything that connects.
        for posture in [Posture::Allowlist, Posture::MirrorOnly, Posture::Isolated] {
            let nft = ruleset(&policy(posture, &[]));
            for chain in [
                "chain output {",
                "chain forward {",
                "chain forward_egress {",
            ] {
                let body = nft.split(chain).nth(1).unwrap();
                let body = body.split("\n  }").next().unwrap();
                assert!(
                    !body.contains("169.254.0.0/16 accept"),
                    "{posture}/{chain} must not exempt IPv4 link-local — that \
                     reaches cloud metadata:\n{body}"
                );
                assert!(
                    !body.contains("fe80::/10 accept"),
                    "{posture}/{chain} must not exempt all of IPv6 link-local:\n{body}"
                );
            }
        }

        // Neighbour discovery still works, by type, and only for the host.
        let nft = ruleset(&policy(Posture::Isolated, &[]));
        let output = nft.split("chain output {").nth(1).unwrap();
        let output = output.split("\n  }").next().unwrap();
        assert!(
            output.contains("fe80::/10 icmpv6 type"),
            "the box must still discover its neighbours:\n{output}"
        );
        for chain in ["chain forward {", "chain forward_egress {"] {
            let body = nft.split(chain).nth(1).unwrap();
            let body = body.split("\n  }").next().unwrap();
            assert!(
                !body.contains("fe80::/10"),
                "{chain} must not carry the host's neighbour discovery:\n{body}"
            );
        }
    }

    #[test]
    fn policy_test_agrees_with_the_ruleset_about_link_local() {
        // The two have to answer the same question the same way. `devbox policy
        // test` reads `is_local`; the box reads this ruleset. When the ruleset
        // stopped exempting link-local, an `is_local` that still did would have
        // reported "allowed" for traffic the box drops — the failure mode the
        // comment on `emit_policy_rules` has warned about since round 5, in the
        // opposite direction.
        for addr in ["169.254.169.254", "169.254.1.1", "fe80::1"] {
            assert!(
                !crate::policy::is_local(addr),
                "{addr} is judged by posture in the ruleset, so is_local must \
                 not short-circuit it to allowed"
            );
        }
        assert!(crate::policy::is_local("127.0.0.1"));
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
