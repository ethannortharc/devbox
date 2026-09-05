//! Egress and activity control — §8.
//!
//! Observation becomes governance. A box has a **posture**; every outbound
//! connection is evaluated against it and either allowed, blocked, or flagged.
//! The evaluation is pure and lives here; enforcement (nftables, cgroup hooks)
//! lives in the agent, driven by the ruleset this module generates.
//!
//! The split matters: a policy decision that can only be tested by making a
//! real connection through a real firewall is a policy decision nobody tests.

pub mod enforce;
pub mod mirrors;
pub mod nftables;

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A box's egress posture (§8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Posture {
    /// Full network. Everything observed, nothing blocked.
    #[default]
    Open,
    /// Default-deny; only listed domains and CIDRs permitted.
    Allowlist,
    /// Package mirrors and declared sources only — "build, don't phone home".
    MirrorOnly,
    /// No egress. Loopback, plus private prefixes the box itself declares.
    Isolated,
}

impl Posture {
    /// Every posture, weakest first.
    pub const ALL: &'static [Posture] = &[
        Posture::Open,
        Posture::Allowlist,
        Posture::MirrorOnly,
        Posture::Isolated,
    ];

    /// The config and API spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Posture::Open => "open",
            Posture::Allowlist => "allowlist",
            Posture::MirrorOnly => "mirror-only",
            Posture::Isolated => "isolated",
        }
    }

    /// One line explaining what this posture does, for the UI.
    pub fn describe(&self) -> &'static str {
        match self {
            Posture::Open => "Full network. Everything is observed; nothing is blocked.",
            Posture::Allowlist => {
                "Default-deny. Only the domains and CIDRs you list can be reached."
            }
            Posture::MirrorOnly => {
                "Package mirrors and git hosts only. Builds work; nothing phones home."
            }
            Posture::Isolated => {
                "No egress at all. Loopback only, plus any private prefixes a \
                 service hosted in the box declares."
            }
        }
    }

    /// Whether this posture blocks anything.
    pub fn enforces(&self) -> bool {
        !matches!(self, Posture::Open)
    }
}

impl fmt::Display for Posture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Posture {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Posture::ALL
            .iter()
            .copied()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown egress posture '{s}'; expected one of: open, allowlist, \
                     mirror-only, isolated"
                )
            })
    }
}

/// A box's full policy, as stored in `devbox.toml` (§12.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub egress: Posture,
    /// Domains and CIDRs permitted in `allowlist` mode.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Raise an event for a denied connection even when the posture would not
    /// block it. In `open` this is the "observe and warn" mode.
    #[serde(default = "yes")]
    pub alert_on_violation: bool,
}

fn yes() -> bool {
    true
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            egress: Posture::Open,
            allow: Vec::new(),
            alert_on_violation: true,
        }
    }
}

/// What a policy decided about one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Permitted by the posture.
    Allow,
    /// Refused: the connection does not happen.
    Block,
    /// Permitted, but recorded as outside policy. `open` + `alert_on_violation`
    /// is the "tell me, don't stop me" mode.
    Flag,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Block => "block",
            Verdict::Flag => "flag",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A decision with its reasoning, so the event and the UI can both explain it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: String,
}

impl Decision {
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Allow,
            reason: reason.into(),
        }
    }
    fn deny(policy: &Policy, reason: impl Into<String>) -> Self {
        Self {
            // `open` never blocks; it only flags, and only if asked to.
            verdict: if policy.egress.enforces() {
                Verdict::Block
            } else {
                Verdict::Flag
            },
            reason: reason.into(),
        }
    }
}

/// What is being connected to.
#[derive(Debug, Clone, Default)]
pub struct Target {
    /// The name, if one is known — from DNS, SNI, or the request.
    pub domain: String,
    /// The address, always known.
    pub addr: String,
    pub port: u16,
}

impl Policy {
    /// Decide whether a connection is permitted.
    pub fn evaluate(&self, target: &Target) -> Decision {
        // Loopback only. Blocking it would break the box's own services and it
        // cannot leave the host anyway — which is exactly what stopped being
        // true of the link-local range that used to be lumped in with it.
        if is_local(&target.addr) {
            return Decision::allow("local address");
        }

        match self.egress {
            Posture::Open => {
                if self.alert_on_violation && !self.allow.is_empty() && !self.permits(target) {
                    Decision {
                        verdict: Verdict::Flag,
                        reason: "outside the declared allowlist (posture is open, so not blocked)"
                            .to_string(),
                    }
                } else {
                    Decision::allow("posture is open")
                }
            }

            Posture::Isolated => {
                // Only what the ruleset actually permits. This used to allow
                // every RFC 1918 address, while the generated ruleset (since
                // ADR-0046) permits only the prefixes a service *running* in
                // the box has declared under `/etc/devbox/prefixes/` — so
                // `policy test 192.168.1.1` answered "allowed" for traffic the
                // box would drop.
                //
                // Whether anything has declared them is a property of the box,
                // and this runs offline against devbox.toml. Reporting the
                // stricter of the two possible answers is the right way to
                // be wrong: a tool that says "denied" about something
                // permitted causes a second look, and one that says "allowed"
                // about something dropped causes an outage nobody connects to
                // the policy.
                //
                // The verdict line above already names the posture, so the
                // reason does not repeat it.
                if is_private_range(&target.addr) {
                    Decision::deny(
                        self,
                        "private range; permitted only while a box-hosted service declares it",
                    )
                } else {
                    Decision::deny(self, "posture is isolated: no egress")
                }
            }

            Posture::Allowlist => {
                if self.permits(target) {
                    Decision::allow("matches the allowlist")
                } else {
                    Decision::deny(self, "not in the allowlist")
                }
            }

            Posture::MirrorOnly => {
                if mirrors::permits(&target.domain) {
                    Decision::allow("a known package mirror or source host")
                } else if self.permits(target) {
                    Decision::allow("matches the allowlist")
                } else {
                    Decision::deny(self, "not a package mirror or an allowlisted host")
                }
            }
        }
    }

    /// Whether the explicit allowlist covers this target.
    pub fn permits(&self, target: &Target) -> bool {
        self.allow.iter().any(|entry| matches(entry, target))
    }

    /// Validate the allowlist, rejecting entries that would never match.
    pub fn validate(&self) -> Result<()> {
        for entry in &self.allow {
            let entry = entry.trim();
            if entry.is_empty() {
                bail!("allowlist contains an empty entry");
            }
            if entry.contains('/') {
                parse_cidr(entry)
                    .ok_or_else(|| anyhow::anyhow!("'{entry}' is not a valid CIDR"))?;
            } else if !is_plausible_domain(entry) {
                bail!("'{entry}' is not a valid domain or CIDR");
            }
        }
        Ok(())
    }

    /// Every domain the allowlist names (excluding CIDRs).
    ///
    /// The agent resolves these and feeds the answers into the nftables set —
    /// the "DNS-driven allowlist" of §8.
    pub fn domains(&self) -> BTreeSet<String> {
        self.allow
            .iter()
            .map(|e| e.trim_start_matches("*.").trim().to_string())
            .filter(|e| !e.contains('/') && !e.is_empty())
            .collect()
    }

    /// Every CIDR the allowlist names.
    pub fn cidrs(&self) -> Vec<String> {
        self.allow
            .iter()
            .map(|e| e.trim().to_string())
            .filter(|e| e.contains('/'))
            .collect()
    }
}

/// Whether an allowlist entry matches a target.
///
/// An entry is a domain (`github.com`), a wildcard (`*.githubusercontent.com`),
/// or a CIDR (`10.0.0.0/8`). A bare domain also matches its subdomains, because
/// allowlisting `github.com` and then being surprised by
/// `codeload.github.com` is a papercut nobody wants twice.
pub fn matches(entry: &str, target: &Target) -> bool {
    let entry = entry.trim();
    if entry.is_empty() {
        return false;
    }

    if entry.contains('/') {
        return match_cidr(entry, &target.addr);
    }

    // A wildcard entry means subdomains, and only subdomains — which is what
    // the console's help text promises. Stripping `*.` and forgetting it was
    // there made `*.example.com` permit `example.com` as well, a wider grant
    // than the user wrote. The Go enforcer applies the same rule; a Go test
    // pins the two together.
    let wildcard = entry.starts_with("*.");
    let pattern = entry.strip_prefix("*.").unwrap_or(entry);
    let domain = target.domain.trim_end_matches('.');
    if domain.is_empty() {
        return false;
    }
    if wildcard && domain.eq_ignore_ascii_case(pattern) {
        return false;
    }

    suffix_match(domain, pattern)
}

/// `domain` equals `suffix`, or ends with `.suffix`.
///
/// Label-boundary matching is the whole point: `evilgithub.com` must not match
/// `github.com`, and a plain `ends_with` says that it does.
pub fn suffix_match(domain: &str, suffix: &str) -> bool {
    if domain.eq_ignore_ascii_case(suffix) {
        return true;
    }
    let Some(cut) = domain.len().checked_sub(suffix.len()) else {
        return false;
    };
    if cut == 0 {
        return false;
    }
    domain.as_bytes()[cut - 1] == b'.'
        && domain
            .get(cut..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Parse `a.b.c.d/len` into its parts.
pub fn parse_cidr(entry: &str) -> Option<(std::net::IpAddr, u8)> {
    let (addr, len) = entry.split_once('/')?;
    let addr: std::net::IpAddr = addr.parse().ok()?;
    let len: u8 = len.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (len <= max).then_some((addr, len))
}

/// Whether an address falls inside a CIDR.
pub fn match_cidr(entry: &str, addr: &str) -> bool {
    let Some((net, len)) = parse_cidr(entry) else {
        return false;
    };
    let Ok(addr) = addr.parse::<std::net::IpAddr>() else {
        return false;
    };

    match (net, addr) {
        (std::net::IpAddr::V4(net), std::net::IpAddr::V4(addr)) => {
            prefix_eq(&net.octets(), &addr.octets(), len)
        }
        (std::net::IpAddr::V6(net), std::net::IpAddr::V6(addr)) => {
            prefix_eq(&net.octets(), &addr.octets(), len)
        }
        // A v4 CIDR never covers a v6 address, or the reverse.
        _ => false,
    }
}

/// Compare the first `bits` bits of two addresses.
fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let whole = (bits / 8) as usize;
    let rest = bits % 8;

    if a[..whole] != b[..whole] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - rest);
    (a[whole] & mask) == (b[whole] & mask)
}

/// Addresses the generated ruleset accepts whatever the posture is.
///
/// Deliberately narrow, and it must stay in step with the unconditional
/// accepts in `nftables::emit_policy_rules` — this function is what `devbox
/// policy test` answers from, and the two disagreeing is how the tool ends up
/// confidently wrong about what the box will do.
///
/// Link-local was here once, on the reasoning that it is "never egress". It is:
/// it leaves the interface, and `169.254.169.254` in particular answers with
/// cloud instance credentials. It is now judged by posture like any other
/// destination, and only ICMPv6 neighbour discovery is exempted, in the
/// output chain. Broadcast went the same way: DHCP is permitted by port in the
/// host-control rules, not by destination, so a generic packet to
/// `255.255.255.255` is dropped and "allowed" was the wrong answer.
pub fn is_local(addr: &str) -> bool {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.is_loopback() || a.is_unspecified(),
        Ok(std::net::IpAddr::V6(a)) => a.is_loopback() || a.is_unspecified(),
        Err(_) => false,
    }
}

/// RFC 1918 and ULA — the ranges a service standing its own subnets up inside
/// a box allocates from (§9).
///
/// `isolated` permits a declared prefix out of these so two endpoints on such
/// a subnet can still reach each other — the posture means "no egress", not
/// "no networking". Membership here is necessary, never sufficient: the
/// ruleset accepts only what the box has actually declared (ADR-0046).
pub fn is_private_range(addr: &str) -> bool {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.is_private(),
        // fc00::/7, the v6 unique-local range.
        Ok(std::net::IpAddr::V6(a)) => (a.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => false,
    }
}

/// A cheap plausibility check for a domain in the allowlist.
pub fn is_plausible_domain(name: &str) -> bool {
    let name = name.strip_prefix("*.").unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(domain: &str, addr: &str) -> Target {
        Target {
            domain: domain.into(),
            addr: addr.into(),
            port: 443,
        }
    }

    #[test]
    fn postures_round_trip_through_their_names() {
        for p in Posture::ALL {
            assert_eq!(p.as_str().parse::<Posture>().unwrap(), *p);
            assert!(!p.describe().is_empty());
        }
        assert!("nonsense".parse::<Posture>().is_err());
        assert_eq!(Posture::default(), Posture::Open);
    }

    #[test]
    fn only_open_leaves_traffic_alone() {
        assert!(!Posture::Open.enforces());
        assert!(Posture::Allowlist.enforces());
        assert!(Posture::MirrorOnly.enforces());
        assert!(Posture::Isolated.enforces());
    }

    #[test]
    fn open_allows_everything() {
        let policy = Policy::default();
        let d = policy.evaluate(&target("anything.example", "93.184.216.34"));
        assert_eq!(d.verdict, Verdict::Allow);
    }

    #[test]
    fn open_with_an_allowlist_flags_without_blocking() {
        // "Tell me, don't stop me": the useful first step before enforcing.
        let policy = Policy {
            egress: Posture::Open,
            allow: vec!["github.com".into()],
            alert_on_violation: true,
        };

        assert_eq!(
            policy
                .evaluate(&target("github.com", "140.82.121.4"))
                .verdict,
            Verdict::Allow
        );
        let d = policy.evaluate(&target("telemetry.example", "1.2.3.4"));
        assert_eq!(d.verdict, Verdict::Flag, "flagged, not blocked");
        assert!(d.reason.contains("not blocked"), "{}", d.reason);
    }

    #[test]
    fn allowlist_blocks_what_it_does_not_name() {
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["github.com".into(), "api.anthropic.com".into()],
            alert_on_violation: true,
        };

        assert_eq!(
            policy
                .evaluate(&target("github.com", "140.82.121.4"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            policy
                .evaluate(&target("api.anthropic.com", "160.79.104.10"))
                .verdict,
            Verdict::Allow
        );

        let d = policy.evaluate(&target("telemetry.example.com", "1.2.3.4"));
        assert_eq!(d.verdict, Verdict::Block);
        assert!(d.reason.contains("not in the allowlist"));
    }

    #[test]
    fn a_bare_domain_covers_its_subdomains() {
        // Allowlisting `github.com` and then being surprised by
        // `codeload.github.com` is a papercut nobody wants twice.
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["github.com".into()],
            alert_on_violation: true,
        };

        for host in ["github.com", "codeload.github.com", "api.github.com"] {
            assert_eq!(
                policy.evaluate(&target(host, "140.82.121.4")).verdict,
                Verdict::Allow,
                "{host} should match"
            );
        }
        // But a lookalike must not.
        for host in ["notgithub.com", "github.com.evil.example", "evilgithub.com"] {
            assert_eq!(
                policy.evaluate(&target(host, "1.2.3.4")).verdict,
                Verdict::Block,
                "{host} must not match"
            );
        }
    }

    #[test]
    fn wildcards_match_subdomains_only() {
        let t = |h: &str| target(h, "1.2.3.4");
        assert!(matches(
            "*.githubusercontent.com",
            &t("raw.githubusercontent.com")
        ));
        // Not the apex. The test's own name said "subdomains only" while
        // asserting the opposite — and the console's help text says the same
        // thing the name does, so this is the behaviour both promised.
        assert!(!matches(
            "*.githubusercontent.com",
            &t("githubusercontent.com")
        ));
        // A bare entry still covers both, which is how `github.com` reaches
        // `codeload.github.com`.
        assert!(matches(
            "githubusercontent.com",
            &t("githubusercontent.com")
        ));
        assert!(matches(
            "githubusercontent.com",
            &t("raw.githubusercontent.com")
        ));
        assert!(!matches(
            "*.githubusercontent.com",
            &t("evilgithubusercontent.com")
        ));
    }

    #[test]
    fn domain_matching_is_case_insensitive_and_ignores_a_trailing_dot() {
        let t = target("GitHub.COM.", "1.2.3.4");
        assert!(matches("github.com", &t));
    }

    #[test]
    fn cidrs_match_on_the_address() {
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["10.0.0.0/8".into(), "192.168.1.0/24".into()],
            alert_on_violation: true,
        };

        assert_eq!(
            policy.evaluate(&target("", "10.5.5.5")).verdict,
            Verdict::Allow
        );
        assert_eq!(
            policy.evaluate(&target("", "192.168.1.7")).verdict,
            Verdict::Allow
        );
        assert_eq!(
            policy.evaluate(&target("", "192.168.2.7")).verdict,
            Verdict::Block
        );
        assert_eq!(
            policy.evaluate(&target("", "11.0.0.1")).verdict,
            Verdict::Block
        );
    }

    #[test]
    fn cidr_prefixes_that_are_not_byte_aligned() {
        assert!(match_cidr("10.0.0.0/31", "10.0.0.1"));
        assert!(!match_cidr("10.0.0.0/31", "10.0.0.2"));
        assert!(match_cidr("192.168.0.0/20", "192.168.15.255"));
        assert!(!match_cidr("192.168.0.0/20", "192.168.16.0"));
        assert!(match_cidr("0.0.0.0/0", "8.8.8.8"), "/0 covers everything");
    }

    #[test]
    fn a_v4_cidr_never_covers_a_v6_address() {
        assert!(!match_cidr("10.0.0.0/8", "::1"));
        assert!(!match_cidr("::/0", "10.0.0.1"));
        assert!(match_cidr("2001:db8::/32", "2001:db8::1"));
    }

    #[test]
    fn garbage_cidrs_never_match() {
        for entry in [
            "not/a/cidr",
            "10.0.0.0/33",
            "10.0.0.0/abc",
            "/8",
            "10.0.0.0/",
        ] {
            assert!(!match_cidr(entry, "10.0.0.1"), "{entry} should not match");
        }
    }

    #[test]
    fn mirror_only_lets_package_managers_work() {
        let policy = Policy {
            egress: Posture::MirrorOnly,
            allow: vec![],
            alert_on_violation: true,
        };

        // The acceptance criterion: pip, npm, cargo, and nix keep working.
        for host in [
            "pypi.org",
            "files.pythonhosted.org",
            "registry.npmjs.org",
            "crates.io",
            "static.crates.io",
            "cache.nixos.org",
            "github.com",
            "proxy.golang.org",
        ] {
            assert_eq!(
                policy.evaluate(&target(host, "1.2.3.4")).verdict,
                Verdict::Allow,
                "{host} is a package source and must work"
            );
        }

        // And arbitrary hosts do not.
        for host in ["telemetry.example.com", "evil.example", "api.openai.com"] {
            assert_eq!(
                policy.evaluate(&target(host, "1.2.3.4")).verdict,
                Verdict::Block,
                "{host} is not a package source"
            );
        }
    }

    #[test]
    fn mirror_only_still_honours_an_explicit_allowlist() {
        let policy = Policy {
            egress: Posture::MirrorOnly,
            allow: vec!["api.anthropic.com".into()],
            alert_on_violation: true,
        };
        assert_eq!(
            policy
                .evaluate(&target("api.anthropic.com", "1.2.3.4"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            policy.evaluate(&target("evil.example", "1.2.3.4")).verdict,
            Verdict::Block
        );
    }

    #[test]
    fn isolated_blocks_everything_except_local_and_lab_traffic() {
        let policy = Policy {
            egress: Posture::Isolated,
            allow: vec!["github.com".into()],
            alert_on_violation: true,
        };

        // Even an explicitly allowlisted host is blocked: isolated means
        // isolated, and a posture that quietly makes exceptions is a lie.
        assert_eq!(
            policy
                .evaluate(&target("github.com", "140.82.121.4"))
                .verdict,
            Verdict::Block
        );
        // The box can still talk to itself.
        assert_eq!(
            policy.evaluate(&target("", "127.0.0.1")).verdict,
            Verdict::Allow
        );
        // A private address is *not* unconditionally allowed. The ruleset
        // permits only the prefixes the box has declared (ADR-0046), and this
        // evaluation is offline — so it reports the stricter answer and says
        // why, rather than promising reachability the box will refuse.
        let private = policy.evaluate(&target("", "192.168.5.5"));
        assert_eq!(private.verdict, Verdict::Block);
        assert!(
            private.reason.contains("declares it"),
            "the reason must point at the declaration condition: {}",
            private.reason
        );
    }

    #[test]
    fn loopback_is_never_egress_under_any_posture() {
        for posture in Posture::ALL {
            let policy = Policy {
                egress: *posture,
                allow: vec![],
                alert_on_violation: true,
            };
            assert_eq!(
                policy.evaluate(&target("", "127.0.0.1")).verdict,
                Verdict::Allow,
                "{posture} must not break the box's own services"
            );
        }
    }

    #[test]
    fn local_and_lab_ranges_are_classified() {
        assert!(is_local("127.0.0.1"));
        assert!(is_local("::1"));
        assert!(is_local("0.0.0.0"));
        assert!(!is_local("8.8.8.8"));
        assert!(!is_local("not-an-address"));

        // Link-local is judged by posture, not waved through. The cloud
        // metadata address is the one that matters: it hands out instance
        // credentials, and it sat behind an unconditional accept.
        assert!(!is_local("169.254.169.254"));
        assert!(!is_local("169.254.1.1"));
        assert!(!is_local("fe80::1"));
        // Broadcast likewise: DHCP is exempted by port, not by destination.
        assert!(!is_local("255.255.255.255"));

        assert!(is_private_range("10.0.0.1"));
        assert!(is_private_range("172.16.0.1"));
        assert!(is_private_range("192.168.1.1"));
        assert!(is_private_range("fd00::1"));
        assert!(!is_private_range("8.8.8.8"));
    }

    #[test]
    fn validation_rejects_entries_that_could_never_match() {
        let ok = Policy {
            egress: Posture::Allowlist,
            allow: vec![
                "github.com".into(),
                "*.githubusercontent.com".into(),
                "10.0.0.0/8".into(),
                "2001:db8::/32".into(),
            ],
            alert_on_violation: true,
        };
        assert!(ok.validate().is_ok());

        for bad in [
            "",
            "  ",
            "no-dot",
            "10.0.0.0/99",
            "bad..domain",
            "-lead.com",
        ] {
            let policy = Policy {
                egress: Posture::Allowlist,
                allow: vec![bad.into()],
                alert_on_violation: true,
            };
            assert!(policy.validate().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn splits_the_allowlist_into_names_and_cidrs() {
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec![
                "github.com".into(),
                "*.githubusercontent.com".into(),
                "10.0.0.0/8".into(),
            ],
            alert_on_violation: true,
        };

        let domains = policy.domains();
        assert!(domains.contains("github.com"));
        assert!(
            domains.contains("githubusercontent.com"),
            "the wildcard prefix is stripped for resolution: {domains:?}"
        );
        assert_eq!(domains.len(), 2);
        assert_eq!(policy.cidrs(), vec!["10.0.0.0/8"]);
    }

    #[test]
    fn a_target_with_no_name_cannot_match_a_domain_entry() {
        // An unnamed connection under `allowlist` is denied unless a CIDR
        // covers it — matching on "no name" would defeat the whole posture.
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["github.com".into()],
            alert_on_violation: true,
        };
        assert_eq!(
            policy.evaluate(&target("", "140.82.121.4")).verdict,
            Verdict::Block
        );
    }
}
