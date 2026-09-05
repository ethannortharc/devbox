//! How the guest reaches a listener on the host — §6.6.
//!
//! This is the part of the broker that is runtime-specific and that the design
//! says must be *verified, not assumed*. It is verified by opening a TCP
//! connection from inside the guest to the broker's port and seeing whether it
//! completes — not by resolving a name, not by pinging, and not by trusting a
//! documented address.
//!
//! Measured on this host (Lima 2.x, `vmType: vz`, macOS 25.5, 2026-09-05):
//! with the broker bound to `127.0.0.1:18080` **only**, the guest reached it at
//! both `host.lima.internal` and the default gateway `192.168.5.2`, and the
//! host saw the connection arrive from `127.0.0.1`. Lima's user-mode network
//! NATs guest→gateway traffic onto the host's loopback, so the broker never
//! needs to bind wider than loopback — which is also why the box token is the
//! only thing separating one box from another, and why it is required on every
//! request.
//!
//! Incus and Docker are unverified here (neither runtime is installed on the
//! machine this was built on); their candidate lists follow §6.6 and are
//! probed with the same code, so a wrong first choice degrades to the next
//! candidate rather than to a silent failure.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::runtime::Runtime;

/// A verified way for one box to reach the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReach {
    /// The host part of the URL the guest should use.
    pub host: String,
    /// The port, echoed back so callers can build a URL from this alone.
    pub port: u16,
    /// How the address was found, for `doctor`.
    pub how: &'static str,
}

impl HostReach {
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// How long a single candidate gets before the next one is tried.
///
/// Short on purpose: this runs on every session start, and a candidate that
/// needs more than two seconds to complete a TCP handshake to the host it is
/// running on is not the address to use.
const PROBE_TIMEOUT_SECS: u32 = 2;

/// Try each candidate from inside the guest and return the first that answers.
///
/// The probe is bash's `/dev/tcp`, so it needs no tool installed in the guest —
/// `curl`, `nc`, and `python` are all absent from some images and all three
/// were considered. `timeout` bounds each attempt; a guest without `timeout`
/// still works, the attempt is just bounded by the kernel's connect timeout.
pub async fn probe(
    runtime: &dyn Runtime,
    name: &str,
    port: u16,
    candidates: &[String],
    how: &'static str,
) -> Result<HostReach> {
    if candidates.is_empty() {
        bail!("no candidate host address for runtime '{}'", runtime.name());
    }
    let script = probe_script(port, candidates);
    let result = runtime
        .exec_cmd(name, &["bash", "-lc", &script], false)
        .await
        .with_context(|| format!("probe host reachability from box '{name}'"))?;
    let found = parse_probe_output(&result.stdout).unwrap_or_default();
    let found = found.as_str();
    if result.exit_code != 0 || found.is_empty() {
        bail!(
            "box '{name}' could not reach the devbox broker on port {port} at any of: {}. \
             The broker is bound to loopback on the host; if this runtime does not NAT the \
             guest's default gateway onto host loopback, a reverse tunnel is needed.",
            candidates.join(", ")
        );
    }
    if !candidates.iter().any(|c| c == found) {
        bail!("host reachability probe returned an unexpected address {found:?}");
    }
    Ok(HostReach {
        host: found.to_string(),
        port,
        how,
    })
}

/// The prefix the probe prints its answer behind.
///
/// A bare address on stdout is not parseable: this runs under a *login* shell
/// (needed for a PATH that has `bash` and `timeout` on NixOS), and a login
/// shell's profile prints things — on the box this was first run against, a
/// terminal-title escape sequence arrived after the address and became the
/// "answer". The prefix means only a line this script wrote is ever read.
const PROBE_MARKER: &str = "DEVBOX_REACH=";

/// The shell the probe runs. Kept as a pure function so the quoting is tested
/// rather than eyeballed — this string is interpolated into a shell.
fn probe_script(port: u16, candidates: &[String]) -> String {
    let list = candidates
        .iter()
        .map(|c| shell_quote(c))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "for h in {list}; do \
           if timeout {PROBE_TIMEOUT_SECS} bash -c \"exec 3<>/dev/tcp/$h/{port}\" 2>/dev/null; \
           then printf '{PROBE_MARKER}%s\\n' \"$h\"; exit 0; fi; \
         done; exit 1"
    )
}

/// The address the probe reported, ignoring everything a profile printed.
fn parse_probe_output(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(PROBE_MARKER))
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Single-quote a value for `sh`. Candidates come from `ip route` output in
/// the guest, so they are not trusted input even though they should be
/// addresses.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The guest's default gateway, which is the host on every user-mode network.
pub async fn default_gateway(runtime: &dyn Runtime, name: &str) -> Option<String> {
    let result = runtime
        .exec_cmd(
            name,
            &["sh", "-c", "ip -4 route show default 2>/dev/null"],
            false,
        )
        .await
        .ok()?;
    if result.exit_code != 0 {
        return None;
    }
    parse_default_gateway(&result.stdout)
}

/// Pull the gateway address out of `ip -4 route show default`.
pub fn parse_default_gateway(text: &str) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        while let Some(field) = fields.next() {
            if field == "via"
                && let Some(address) = fields.next()
                && is_plain_ipv4(address)
            {
                return Some(address.to_string());
            }
        }
    }
    None
}

/// Whether a string is a bare IPv4 address.
///
/// Public because the policy exemption needs the same answer: nftables takes
/// addresses, so `host.lima.internal` has to be resolved first while
/// `192.168.5.2` does not.
pub fn is_ipv4_literal(value: &str) -> bool {
    is_plain_ipv4(value)
}

/// Accept only a dotted quad. Anything else came from a line we do not
/// understand, and interpolating it into the probe script would be the bug.
fn is_plain_ipv4(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
        && parts
            .iter()
            .all(|p| p.parse::<u16>().is_ok_and(|n| n <= 255))
}

/// How long the whole reach probe may take before the caller gives up.
pub const REACH_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reach_renders_the_url_the_guest_uses() {
        let reach = HostReach {
            host: "host.lima.internal".into(),
            port: 7879,
            how: "lima user-mode network",
        };
        assert_eq!(reach.base_url(), "http://host.lima.internal:7879");
    }

    #[test]
    fn the_probe_tries_candidates_in_order_and_quotes_them() {
        let script = probe_script(
            7879,
            &["host.lima.internal".to_string(), "192.168.5.2".to_string()],
        );
        assert!(script.contains("'host.lima.internal' '192.168.5.2'"));
        assert!(script.contains("/dev/tcp/$h/7879"));
        assert!(script.contains(PROBE_MARKER));
        assert!(script.contains("timeout 2"));
        // Ordering is the contract: the documented name is tried before the
        // gateway, so `doctor` reports the address a human would recognise.
        let name_at = script.find("host.lima.internal").unwrap();
        let gw_at = script.find("192.168.5.2").unwrap();
        assert!(name_at < gw_at);
    }

    #[test]
    fn a_candidate_containing_a_quote_cannot_break_out_of_the_script() {
        let script = probe_script(80, &["a'; rm -rf /; echo '".to_string()]);
        assert!(!script.contains("rm -rf /;\n"));
        assert!(script.contains(r"'a'\''; rm -rf /; echo '\'''"));
    }

    /// Measured failure: a login shell's profile wrote a terminal-title
    /// escape sequence after the address, and reading the last line of stdout
    /// turned that escape into the "reachable host".
    #[test]
    fn a_profile_that_prints_cannot_be_mistaken_for_the_answer() {
        assert_eq!(
            parse_probe_output("DEVBOX_REACH=host.lima.internal\n").as_deref(),
            Some("host.lima.internal")
        );
        assert_eq!(
            parse_probe_output("motd line\nDEVBOX_REACH=192.168.5.2\n\u{1b}]0;\u{7}").as_deref(),
            Some("192.168.5.2"),
            "the marker is the answer, not the last line"
        );
        assert_eq!(parse_probe_output("\u{1b}]0;\u{7}"), None);
        assert_eq!(parse_probe_output(""), None);
        assert_eq!(parse_probe_output("DEVBOX_REACH=\n"), None);
    }

    #[test]
    fn the_gateway_comes_out_of_the_measured_ip_route_output() {
        // Copied verbatim from `ip -4 route` inside devbox-devtest.
        let text = "default via 192.168.5.2 dev enp0s1 proto dhcp src 192.168.5.15 metric 100 \n";
        assert_eq!(parse_default_gateway(text).as_deref(), Some("192.168.5.2"));

        assert_eq!(parse_default_gateway(""), None);
        assert_eq!(parse_default_gateway("default dev tun0 scope link"), None);
        // Anything that is not a dotted quad is refused rather than
        // interpolated into a shell command.
        assert_eq!(parse_default_gateway("default via $(id) dev eth0"), None);
        assert_eq!(
            parse_default_gateway("default via 999.1.1.1 dev eth0"),
            None
        );
        assert_eq!(parse_default_gateway("default via fe80::1 dev eth0"), None);
    }
}
