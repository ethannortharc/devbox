//! Applying a policy to a live box — the other half of §8.
//!
//! [`super::nftables::ruleset`] decides *what* the rules are; this pushes them
//! into the guest and loads them. Keeping the two apart is what lets the hard
//! part — the rules — be unit-tested on a Mac with no kernel involved, while
//! this module stays thin enough to read in one sitting.
//!
//! Until this existed, `devbox policy set isolated` wrote the posture to
//! `devbox.toml`, printed "apply it with `devbox reprovision`", and nothing in
//! any provisioning path ever generated or loaded a ruleset. A box could report
//! `isolated` and still reach the whole internet. A posture that is displayed
//! but not enforced is worse than no posture at all, because it is believed.

use anyhow::{Context, Result, bail};

use super::{Policy, Posture};
use crate::runtime::Runtime;

/// Where the generated ruleset lands inside the box.
pub const RULESET_PATH: &str = "/etc/devbox/devbox.nft";

/// Where the agent reads the policy from.
///
/// An allowlist names *domains*; a firewall matches addresses. The agent
/// bridges the two from the DNS it already captures, so it needs the domain
/// list — not just the compiled ruleset.
pub const POLICY_PATH: &str = "/etc/devbox/policy.json";

/// Push the policy into the box and load it.
///
/// Idempotent, because the generated ruleset destroys the devbox table before
/// recreating it: applying twice leaves what applying once leaves.
pub async fn apply(runtime: &dyn Runtime, sandbox_name: &str, policy: &Policy) -> Result<()> {
    // `open` is the absence of a policy, not a policy of its own. Loading a
    // ruleset for it would leave an empty devbox table sitting in the box
    // suggesting something is being enforced.
    if policy.egress == Posture::Open {
        return clear(runtime, sandbox_name).await;
    }

    // A domain-based allowlist is enforced by nftables against a set that only
    // the agent can fill, from the DNS it captures. No agent means the deny
    // half applies and the allow half never does — the box ends up *more*
    // restricted than the posture says, silently.
    //
    // Refusing is the honest move. Loading a ruleset that blocks what the user
    // just allowlisted, and reporting success, is worse than not applying it.
    let domains: Vec<&str> = policy
        .allow
        .iter()
        .filter(|entry| super::parse_cidr(entry).is_none())
        .map(String::as_str)
        .collect();
    let needs_agent = !domains.is_empty() || policy.egress == Posture::MirrorOnly;
    if needs_agent && !agent_resolves_dns(runtime, sandbox_name).await {
        bail!(
            "box '{sandbox_name}' has no DNS-capturing devbox-obsd, so a domain-based \
             posture cannot be enforced: nftables matches addresses, and only the \
             agent turns the allowlisted names into addresses as they resolve. \
             (An agent in the degraded `-no-ebpf` mode sees no DNS either.) \
             Applying it anyway would block {}.\n\n  \
             Use CIDRs instead, or `isolated`, both of which need no agent.",
            if domains.is_empty() {
                "the package mirrors".to_string()
            } else {
                domains.join(", ")
            }
        );
    }

    let ruleset = super::nftables::ruleset(policy);

    // Hand the agent the domains too. Without this the ruleset is default-deny
    // with a permanently empty allow set: `allowlist` and `mirror-only` would
    // block precisely the traffic they promise to permit.
    let spec = agent_policy_json(policy, &ruleset);
    let write_policy = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", &elevated(&policy_command(&spec))],
            false,
        )
        .await?;
    if write_policy.exit_code != 0 {
        bail!(
            "failed to write the policy into box '{sandbox_name}': {}",
            write_policy.stderr.trim()
        );
    }

    let write = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", &elevated(&write_command(&ruleset))],
            false,
        )
        .await?;
    if write.exit_code != 0 {
        bail!(
            "failed to write the ruleset into box '{sandbox_name}': {}",
            write.stderr.trim()
        );
    }

    let load = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", &elevated(&load_command())],
            false,
        )
        .await?;
    if load.exit_code != 0 {
        // The most common cause by far, worth naming rather than making the
        // user decode an nft error: the box has no nftables at all.
        bail!(
            "failed to load the egress ruleset in box '{sandbox_name}': {}\n\n  \
             If nftables is missing, enable the `network` set \
             (`devbox sets enable network`) and rebuild.",
            load.stderr.trim()
        );
    }

    Ok(())
}

/// Remove devbox's table, returning the box to whatever egress it had before.
///
/// `destroy` rather than `delete` so this succeeds on a box that never had a
/// policy applied — the case that runs every time someone sets `open` on a
/// fresh box.
pub async fn clear(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", &elevated(&clear_command())],
            false,
        )
        .await?;
    if result.exit_code != 0 {
        bail!(
            "failed to clear the egress ruleset in box '{sandbox_name}': {}",
            result.stderr.trim()
        );
    }
    Ok(())
}

/// Wrap a script so it runs with privilege wherever privilege is needed.
///
/// `sudo` unconditionally is wrong in both directions: a container exec is
/// already root and may not have sudo installed at all, while a VM's user
/// needs it. Deciding inside the guest is the only place that knows.
fn elevated(script: &str) -> String {
    format!(
        "if [ \"$(id -u)\" -eq 0 ]; then sh -c '{}'; else sudo sh -c '{}'; fi",
        shell_quote(script),
        shell_quote(script)
    )
}

/// Escape a script for embedding in a single-quoted shell string.
fn shell_quote(script: &str) -> String {
    script.replace('\'', "'\\''")
}

/// The shell that writes the ruleset into the guest.
///
/// Split out so the command is testable without a runtime: what goes wrong
/// here is quoting, and quoting is exactly what a unit test can pin.
fn write_command(ruleset: &str) -> String {
    format!(
        "mkdir -p /etc/devbox && cat > {RULESET_PATH} << 'DEVBOX_NFT_EOF'\n{ruleset}\nDEVBOX_NFT_EOF"
    )
}

/// The shell that removes devbox's table and its generated files.
fn clear_command() -> String {
    format!(
        // No nft means no devbox table, so there is nothing to clear and this
        // is not a failure. Clearing runs on every start of an `open` box,
        // including boxes that have never had a policy and images that ship no
        // firewall at all — erroring there would refuse to start them.
        "command -v nft >/dev/null 2>&1 && \
         nft destroy table inet {} 2>/dev/null; \
         rm -f {RULESET_PATH} {POLICY_PATH} 2>/dev/null; \
         exit 0",
        super::nftables::TABLE
    )
}

/// Can this box turn allowlisted names into firewall entries?
///
/// The question is not "is the agent running" but "is it capturing DNS". The
/// degraded proc source (§13) sees processes and sockets and no DNS at all, so
/// an agent started with `-no-ebpf` passes a liveness check and still cannot
/// populate a single allow-set entry — leaving a default-deny ruleset that
/// blocks every domain it promised to permit.
///
/// Probed rather than assumed: the agent is not yet part of provisioning, so
/// on most boxes the answer is no.
async fn agent_resolves_dns(runtime: &dyn Runtime, sandbox_name: &str) -> bool {
    // The agent's own command line is the authority on which source it chose.
    // `-no-ebpf` and `-fixture` both mean no DNS; anything else means the eBPF
    // source, whose domains include it.
    let Ok(result) = runtime
        .exec_cmd(sandbox_name, &["pgrep", "-a", "-x", "devbox-obsd"], false)
        .await
    else {
        return false;
    };
    if result.exit_code != 0 {
        return false;
    }
    let cmdline = result.stdout;
    !cmdline.contains("-no-ebpf") && !cmdline.contains("-fixture")
}

/// The shell that loads the ruleset and drops connections it no longer allows.
///
/// The ruleset accepts `ct state established,related` first, so a connection
/// opened before a tightening keeps flowing under the old policy — a box moved
/// from `open` to `isolated` stays connected to everything it had already
/// reached, for as long as those connections live. Someone tightening a policy
/// means it to take effect now, not at the next reconnect.
///
/// Flushing runs *after* the load, so the new rules are what the reopened
/// connections are judged against. `conntrack` is best-effort: not every box
/// has the tool, and a ruleset that loaded is worth more than one that failed
/// because a helper was missing.
fn load_command() -> String {
    format!(
        "nft -f {RULESET_PATH} && \
         (conntrack -F 2>/dev/null || true)"
    )
}

/// The shell that writes the agent's policy file.
fn policy_command(spec: &str) -> String {
    format!(
        "mkdir -p /etc/devbox && cat > {POLICY_PATH} << 'DEVBOX_POLICY_EOF'\n\
         {spec}\nDEVBOX_POLICY_EOF"
    )
}

/// What the agent reads: the posture, the domains, and the compiled ruleset.
///
/// Hand-built rather than derived, because this is a wire format between two
/// languages and it should be obvious from one file what crosses the boundary.
fn agent_policy_json(policy: &Policy, ruleset: &str) -> String {
    let domains: Vec<String> = policy
        .allow
        .iter()
        .filter(|entry| super::parse_cidr(entry).is_none())
        .map(|entry| format!("\"{}\"", json_escape(entry)))
        .collect();

    format!(
        "{{\"egress\":\"{}\",\"allow\":[{}],\"ruleset\":\"{}\"}}",
        policy.egress,
        domains.join(","),
        json_escape(ruleset)
    )
}

/// Escape a string for a JSON double-quoted scalar.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Load the box's saved egress posture, if it has one.
///
/// A firewall does not survive a box restart, so a posture that is only applied
/// when it is *set* is enforced until the first reboot and then silently gone —
/// while `devbox.toml` and the console both keep reporting it. This is the
/// shared start path, so every route into a running box goes through it.
///
/// Failure is an error, not a warning. A box that starts with a restrictive
/// posture saved and no firewall installed is exactly the situation the
/// posture exists to prevent, and reporting it quietly in a log leaves the user
/// believing the opposite of what is true. Callers that must start the box
/// regardless can catch it — but they have to decide that explicitly.
pub async fn apply_saved(
    manager: &crate::sandbox::SandboxManager,
    state: &crate::sandbox::state::SandboxState,
    name: &str,
) -> Result<()> {
    // Loaded fallibly. `load_or_default` turns a malformed `devbox.toml` into
    // the *default* config, whose posture is `open` — so a corrupted file would
    // silently unfirewall a box that had been isolated, and report nothing.
    // Corruption is not consent.
    let config = match crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                box_id = %name,
                error = ?e,
                "devbox.toml could not be read; refusing to guess at the egress posture"
            );
            return Err(e).with_context(|| {
                format!(
                    "box '{name}' has an unreadable devbox.toml, so its egress posture is \
                     unknown. Fix the file, or set a posture explicitly with \
                     `devbox policy set <posture>`."
                )
            });
        }
    };
    let runtime = manager.runtime_for_sandbox(state)?;
    if config.policy.egress == Posture::Open {
        // Not a no-op. A box that was `isolated` and is now `open` still has
        // devbox's table in whatever state the guest kept across the restart;
        // returning early left those rules in force while every surface
        // reported the box unrestricted.
        return clear(runtime.as_ref(), name).await;
    }
    apply(runtime.as_ref(), name, &config.policy)
        .await
        .with_context(|| {
            format!(
                "box '{name}' started, but its '{}' egress posture could not be applied — \
                 the box is running with whatever egress it had",
                config.policy.egress
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ruleset_is_written_then_loaded_from_a_stable_path() {
        let policy = Policy {
            egress: Posture::Isolated,
            ..Default::default()
        };
        let cmd = write_command(&super::super::nftables::ruleset(&policy));

        // A quoted heredoc delimiter: the ruleset contains `$` and `{`, and an
        // unquoted heredoc would have the guest shell expand them.
        assert!(cmd.contains("<< 'DEVBOX_NFT_EOF'"));
        assert!(cmd.contains(RULESET_PATH));
        assert!(cmd.contains("drop"), "isolated must actually drop traffic");
        assert!(!cmd.contains("DEVBOX_NFT_EOF\nDEVBOX_NFT_EOF"));
    }

    #[test]
    fn the_agent_receives_the_domains_not_just_the_ruleset() {
        // The bug this pins: a ruleset whose allow set nothing can populate is
        // default-deny with no way out.
        let policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["github.com".into(), "10.0.0.0/8".into()],
            ..Default::default()
        };
        let spec = agent_policy_json(&policy, "table inet devbox {}");

        assert!(spec.contains("\"egress\":\"allowlist\""));
        assert!(
            spec.contains("\"github.com\""),
            "domains must cross: {spec}"
        );
        // CIDRs are already in the generated ruleset; the agent only needs the
        // names it has to resolve.
        assert!(!spec.contains("10.0.0.0/8"), "CIDRs need no DNS: {spec}");
        // The ruleset is embedded as a JSON scalar, so its newlines are escaped.
        assert!(spec.contains("\"ruleset\":\"table inet devbox {}\""));
    }

    #[test]
    fn cidrs_and_isolated_need_no_agent() {
        // The distinction the guard rests on: what nftables can match on its
        // own versus what needs DNS to become an address.
        let cidr_only = Policy {
            egress: Posture::Allowlist,
            allow: vec!["10.0.0.0/8".into(), "192.168.1.1/32".into()],
            ..Default::default()
        };
        assert!(
            cidr_only
                .allow
                .iter()
                .all(|e| crate::policy::parse_cidr(e).is_some()),
            "a CIDR allowlist is enforceable without the agent"
        );

        let with_domain = Policy {
            egress: Posture::Allowlist,
            allow: vec!["10.0.0.0/8".into(), "github.com".into()],
            ..Default::default()
        };
        assert!(
            with_domain
                .allow
                .iter()
                .any(|e| crate::policy::parse_cidr(e).is_none()),
            "one domain is enough to need the agent"
        );
    }

    #[test]
    fn json_escaping_survives_a_multiline_ruleset() {
        let spec = agent_policy_json(&Policy::default(), "line one\nline \"two\"");
        assert!(spec.contains("line one\\nline \\\"two\\\""), "{spec}");
    }

    #[test]
    fn tightening_a_policy_drops_the_connections_it_no_longer_allows() {
        let cmd = load_command();
        // Order matters: flush after the load, so what reconnects is judged
        // against the new rules rather than the old ones.
        let load = cmd.find("nft -f").expect("the ruleset must load");
        let flush = cmd.find("conntrack -F").expect("conntrack must be flushed");
        assert!(load < flush);
        // A box without the tool still gets its ruleset.
        assert!(cmd.contains("|| true"));
        // And a *failed* load must not be papered over by the flush.
        assert!(cmd.contains("&&"));
    }

    #[test]
    fn clearing_succeeds_on_a_box_that_never_had_a_policy() {
        let cmd = clear_command();
        // `destroy` and the `|| true` are both load-bearing: `delete` on a
        // missing table is an error, and this runs on every `policy set open`.
        assert!(cmd.contains("destroy table inet devbox"));
        // Exits 0 whatever it finds: this runs on every start of an open box,
        // including images with no firewall at all.
        assert!(cmd.contains("command -v nft"));
        // Privilege is decided in the guest: a container exec is already root
        // and may have no sudo at all, while a VM's user needs it.
        let wrapped = elevated(&cmd);
        assert!(wrapped.contains("id -u"));
        assert!(wrapped.contains("sudo sh -c"));
        assert!(cmd.trim_end().ends_with("exit 0"));
        assert!(cmd.contains(RULESET_PATH));
    }
}
