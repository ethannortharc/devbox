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
    // `open` with an allowlist and alerts on is not the absence of a policy:
    // it is observe-and-warn, and it needs a table to do the observing. Every
    // other `open` posture has nothing to install.
    if policy.egress == Posture::Open && !super::nftables::audits(policy) {
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
    // ...but not on the first application, or nothing could ever be applied.
    //
    // The guard and the agent were a deadlock: the agent exits without
    // `/etc/devbox/policy.json`, and this refused to write that file until a
    // qualifying agent was running. Moving from `open` to an allowlist was
    // therefore impossible — the command always bailed, and the box stayed
    // open, which is the failure the guard exists to prevent.
    //
    // So the file is staged either way; the refusal is about *loading a
    // default-deny ruleset* the agent cannot populate, which is the part that
    // strands traffic. With the policy written and no agent, the posture is
    // saved and inert, and the message says exactly that.
    // Only where the ruleset denies. The refusal below exists because a
    // default-deny table with an allow set nothing can fill strands exactly
    // the traffic the user permitted — under an auditing `open` posture
    // nothing is denied, so an unpopulated set means over-reporting rather
    // than a box cut off, and refusing to install would be the worse answer.
    let needs_agent =
        policy.egress.enforces() && (!domains.is_empty() || policy.egress == Posture::MirrorOnly);
    let agent_ready = !needs_agent || agent_resolves_dns(runtime, sandbox_name).await;

    if !agent_ready {
        // Stage the policy so an agent started next can read it, then refuse
        // to install the ruleset.
        let spec = agent_policy_json(policy, "");
        let staged = runtime
            .exec_cmd(
                sandbox_name,
                &["sh", "-c", &elevated(&policy_command(&spec))],
                false,
            )
            .await;
        // Checked, because the message below tells the user to restart the
        // agent — and an agent restarted without this file exits again. A
        // discarded failure here leaves exactly the deadlock the staging was
        // added to break, with instructions that cannot work.
        match staged {
            Ok(r) if r.exit_code == 0 => {}
            Ok(r) => bail!(
                "could not stage the policy in box '{sandbox_name}': {}\n  \
                 Without it the agent has nothing to enforce and will exit \
                 again, so restarting it will not help.",
                r.stderr.trim()
            ),
            Err(e) => {
                return Err(e).context(format!(
                    "could not reach box '{sandbox_name}' to stage the policy"
                ));
            }
        }
    }

    if !agent_ready {
        bail!(
            "box '{sandbox_name}' has no DNS-capturing devbox-obsd, so a domain-based \
             posture cannot be enforced: nftables matches addresses, and only the \
             agent turns the allowlisted names into addresses as they resolve. \
             (An agent in `-no-ebpf` mode sees no DNS, and one started without \
             `-policy` never reads the allowlist — restart it after the first \
             policy is written.) \
             Applying it anyway would block {}.\n\n  \
             Use CIDRs instead, or `isolated`, both of which need no agent.",
            if domains.is_empty() {
                "the package mirrors".to_string()
            } else {
                domains.join(", ")
            }
        );
    }

    // Built from the box, not assumed. Blanket DNS and RFC-1918 exemptions
    // were holes; these are the specific addresses this box actually needs.
    let ctx = discover_context(runtime, sandbox_name).await;
    if ctx.resolvers.is_empty() {
        // Worth saying out loud: with no resolver exempted, name resolution
        // stops under an enforcing posture, and the user needs to know that is
        // the cause rather than a mysterious network failure.
        eprintln!(
            "devbox: no resolver found in {sandbox_name}:/etc/resolv.conf — DNS is \
             not exempted, so name resolution will fail under this posture"
        );
    }
    let ruleset = super::nftables::ruleset_with(policy, &ctx);

    // Retire the old policy before the new table goes in.
    //
    // Ordering alone is not enough. With the file left in place across the
    // switch, the running agent keeps its *previous* domain list and can
    // insert an answer for a domain the new policy no longer permits — into
    // the new table, where it survives until its TTL. Removing the file first
    // makes the agent's next reload find nothing and stop enforcing anything,
    // which is the correct behaviour for the moment between two policies: it
    // adds no addresses, and the table's own default-deny still applies.
    let _ = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", &elevated(&format!("rm -f {POLICY_PATH}"))],
            false,
        )
        .await;

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
             nftables ships in the locked `system` set, so a box that lacks it \
             predates that change — `devbox reprovision` will install it.",
            load.stderr.trim()
        );
    }

    // The agent's policy goes in *after* the table exists, not before.
    //
    // The other order had a window: the agent notices the new policy file,
    // resolves an allowlisted domain, and inserts the address into the *old*
    // table — which `nft -f` then destroys and rebuilds empty. The agent has
    // already recorded that address as seen, so it will not re-add it until
    // the refresh window elapses, and the domain stays blocked for most of an
    // hour with nothing in any log to explain it.
    //
    // Without this file at all the ruleset is default-deny with a permanently
    // empty allow set, so it still has to be written — just second.
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
pub fn elevated(script: &str) -> String {
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
    let table = super::nftables::TABLE;
    let removed = format!("{RULESET_PATH} {POLICY_PATH}");
    format!(
        // Two cases that look alike and must not be treated alike.
        //
        // *No nft, or no table* — nothing to clear, and not a failure. This
        // runs on every start of an `open` box, including boxes that never had
        // a policy and images with no firewall at all; erroring there would
        // refuse to start them.
        //
        // *nft present and the destroy failed* — a permission problem, an
        // unsupported version, something else. The old restrictive table is
        // still live while every surface reports the box open, which is the
        // dangerous direction. A trailing `exit 0` used to swallow both.
        //
        // The generated files are removed only once the table is really gone,
        // so a failed clear does not additionally strand the agent without its
        // configuration.
        "if ! command -v nft >/dev/null 2>&1; then exit 0; fi; \
         probe=$(nft list table inet {table} 2>&1) || {{ \
           case \"$probe\" in \
             *\"No such file or directory\"*|*\"does not exist\"*) \
               rm -f {removed} 2>/dev/null; exit 0 ;; \
             *) exit 1 ;; \
           esac; \
         }}; \
         nft destroy table inet {table} || exit 1; \
         rm -f {removed} 2>/dev/null; \
         exit 0"
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
    // Three things have to be true, not one. The agent must be up, capturing
    // DNS (the proc source sees none), *and* started with `-policy` — an agent
    // launched before the policy file existed has an empty `cfg.policy` and
    // will never add a single element, however healthy it looks.
    cmdline.contains("-policy") && !cmdline.contains("-no-ebpf") && !cmdline.contains("-fixture")
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

/// Read the box's resolvers and any lab prefixes it hosts.
///
/// Best effort: a box that cannot be read yields an empty context, which
/// generates the *strictest* ruleset rather than the most permissive one. The
/// direction matters — the failure mode of a wrong guess here is a policy that
/// silently does not enforce.
async fn discover_context(runtime: &dyn Runtime, sandbox_name: &str) -> super::nftables::Context {
    let mut resolvers = runtime
        .exec_cmd(sandbox_name, &["cat", "/etc/resolv.conf"], false)
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| parse_resolvers(&r.stdout))
        .unwrap_or_default();

    // Behind a local stub, the address in resolv.conf is not the one that
    // leaves the box.
    //
    // Ubuntu with systemd-resolved lists only `127.0.0.53`. The query to the
    // stub is loopback and always permitted, but resolved's onward query to
    // the *real* server hits the output chain's default drop — so DNS breaks
    // under an enforcing posture while resolv.conf looks exempted. The stub's
    // own configuration lists the upstreams.
    if resolvers.iter().all(|r| is_loopback_resolver(r)) {
        let upstream = runtime
            .exec_cmd(
                sandbox_name,
                &["cat", "/run/systemd/resolve/resolv.conf"],
                false,
            )
            .await
            .ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| parse_resolvers(&r.stdout))
            .unwrap_or_default();
        // Keep the stub too: something may query it directly, and loopback is
        // permitted anyway, so listing it costs nothing and documents intent.
        resolvers.extend(upstream.into_iter().filter(|r| !is_loopback_resolver(r)));
    }

    // Labs record their prefixes under /etc/devbox/lab/<name>/prefixes when
    // they come up, and remove them on teardown. Reading the directory here is
    // the handoff: the ruleset generator runs on the host and cannot otherwise
    // know a lab exists. A box with no lab gets none, and `isolated` then
    // means loopback only — which is what the posture says.
    let lab_prefixes = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", "cat /etc/devbox/lab/*/prefixes 2>/dev/null"],
            false,
        )
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| parse_prefixes(&r.stdout))
        .unwrap_or_default();

    // Docker's bridge networks, which is where nested containers send from.
    // `docker network inspect` is the authority; a box without Docker returns
    // nothing and no forwarded traffic is policed, which is correct because
    // there is none to police.
    let container_prefixes = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "sh",
                "-c",
                "docker network inspect $(docker network ls -q) \
                 --format '{{range .IPAM.Config}}{{.Subnet}}\n{{end}}' 2>/dev/null",
            ],
            false,
        )
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| parse_prefixes(&r.stdout))
        .unwrap_or_default();

    // No interface exemptions. `dvb*` named devbox's veths, but wiring moves
    // both ends into node namespaces and renames them, so nothing at the root
    // ever carries that name — while a Docker network created as `dvb0` would
    // have inherited the exemption.
    let internal_ifaces = Vec::new();

    super::nftables::Context {
        resolvers,
        lab_prefixes,
        container_prefixes,
        internal_ifaces,
    }
}

/// Parse lab prefixes, one per line.
///
/// Validated as CIDRs for the same reason resolvers are: the result is
/// interpolated into a ruleset that runs as root.
fn parse_prefixes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let Some((addr, len)) = line.split_once('/') else {
                return false;
            };
            addr.parse::<std::net::IpAddr>().is_ok() && len.parse::<u8>().is_ok_and(|n| n <= 128)
        })
        .map(str::to_string)
        .collect()
}

/// Is this a local stub rather than a real upstream?
fn is_loopback_resolver(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Pull nameserver addresses out of a resolv.conf.
fn parse_resolvers(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let rest = line.strip_prefix("nameserver")?;
            // Parsed as an address, because this becomes part of a ruleset
            // loaded as root.
            rest.trim()
                .parse::<std::net::IpAddr>()
                .ok()
                .map(|a| a.to_string())
        })
        .collect()
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
/// **Who is asking decides what failure means.**
///
/// ADR-0034 said this must not be fatal, then ADR-0037 made it fatal, and both
/// were half right — which produced a box the user could not get into. The
/// resolution is that the two callers are asking different questions:
///
/// * `policy set` / `policy allow` — "make this true." Failure is the answer
///   to the question, and it must reach the exit status, or a script cannot
///   tell a saved posture from an enforced one. Those call [`apply`] directly.
/// * start / attach / exec / console — "let me in." The user is not asking
///   about policy at all. Locking them out of a *running* box because its
///   firewall could not be installed strands them with no way to fix the very
///   thing that failed — and the box is no more exposed than it was a moment
///   earlier, when it was running without the posture and nobody was blocked.
///
/// So this returns `Ok` after reporting loudly. It never returns `Ok` silently:
/// the warning names the posture that is *not* in force, so the failure cannot
/// be mistaken for enforcement.
/// Restore a box's posture after a rebuild, strictly.
///
/// [`apply_saved`] is lenient on purpose (ADR-0044): a user asking for *access*
/// must never be locked out of a running box because its firewall could not be
/// installed. A rebuild is the opposite situation — nobody is waiting at a
/// prompt, a command is about to print a verdict or a script is about to read
/// an exit status, and "rebuilt successfully" for a box whose firewall is gone
/// is the lie this whole subsystem exists to prevent.
///
/// The two callers want opposite things from the same failure, so the choice
/// is named once here rather than re-decided at each call site — which is how
/// three rebuild paths came to use the lenient form and report success over an
/// unrestricted box.
pub async fn restore_after_rebuild(
    manager: &crate::sandbox::SandboxManager,
    state: &crate::sandbox::state::SandboxState,
    name: &str,
) -> Result<()> {
    let config = crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
        .with_context(|| {
            format!(
                "box '{name}' rebuilt, but its devbox.toml could not be read, so \
                     the egress posture it should have is unknown"
            )
        })?;
    let runtime = manager.runtime_for_sandbox(state)?;

    if config.policy.egress == Posture::Open {
        return clear(runtime.as_ref(), name).await;
    }
    apply(runtime.as_ref(), name, &config.policy)
        .await
        .with_context(|| {
            format!(
                "box '{name}' rebuilt, but its '{}' egress posture could not be \
                 restored — the box is running unrestricted",
                config.policy.egress
            )
        })
}

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
            eprintln!(
                "\ndevbox: WARNING — box '{name}' has an unreadable devbox.toml, so its \
                 egress posture is unknown and none was applied.\n  {e}\n  Fix the \
                 file, or set one explicitly with `devbox policy set <posture>`.\n"
            );
            return Ok(());
        }
    };
    let runtime = manager.runtime_for_sandbox(state)?;
    let outcome = if config.policy.egress == Posture::Open {
        // Not a no-op. A box that was `isolated` and is now `open` still has
        // devbox's table in whatever state the guest kept across the restart;
        // returning early left those rules in force while every surface
        // reported the box unrestricted.
        clear(runtime.as_ref(), name).await
    } else {
        apply(runtime.as_ref(), name, &config.policy).await
    };

    if let Err(e) = outcome {
        let posture = config.policy.egress;
        tracing::error!(box_id = %name, %posture, error = ?e, "egress posture not applied");
        // stderr as well as the log: the log is off by default, and a user who
        // is about to type into this box needs to know its posture is not in
        // force. Loud, and not fatal — see the note above.
        eprintln!(
            "\ndevbox: WARNING — box '{name}' is running WITHOUT its '{posture}' egress \
             posture.\n  {e}\n  Traffic is unrestricted. Fix the cause, then \
             `devbox policy set {posture}` to apply it.\n"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_prefixes_are_parsed_and_validated() {
        assert_eq!(
            parse_prefixes("10.99.0.0/16\n\n2001:db8::/32\n"),
            vec!["10.99.0.0/16", "2001:db8::/32"]
        );
        // Nothing unvalidated reaches a root-loaded ruleset.
        assert!(parse_prefixes("$(reboot)").is_empty());
        assert!(parse_prefixes("10.0.0.0").is_empty());
        assert!(parse_prefixes("10.0.0.0/999").is_empty());
    }

    #[test]
    fn a_local_stub_is_recognised_as_needing_an_upstream() {
        // Ubuntu with systemd-resolved lists only 127.0.0.53. Exempting that
        // and stopping there breaks DNS under an enforcing posture: the stub's
        // onward query is what actually leaves the box.
        assert!(is_loopback_resolver("127.0.0.53"));
        assert!(is_loopback_resolver("127.0.0.1"));
        assert!(is_loopback_resolver("::1"));
        assert!(!is_loopback_resolver("8.8.8.8"));
        assert!(!is_loopback_resolver("192.168.1.1"));
        assert!(!is_loopback_resolver("not-an-address"));
    }

    #[test]
    fn resolvers_are_parsed_and_validated() {
        let conf = "# generated\nnameserver 192.0.2.53\nnameserver 2001:db8::53\n\
                    nameserver not-an-address\nsearch example.com\n";
        assert_eq!(parse_resolvers(conf), vec!["192.0.2.53", "2001:db8::53"]);

        // Nothing unparseable reaches a root-loaded ruleset.
        assert!(parse_resolvers("nameserver $(reboot)").is_empty());
        assert!(parse_resolvers("").is_empty());
    }

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
        // A box with no nft, or no devbox table, has nothing to clear and must
        // still start — this runs on every start of an `open` box.
        assert!(cmd.contains("command -v nft"));
        assert!(cmd.contains("nft list table inet devbox"));
        // But a destroy that *fails* must not report success. The old
        // restrictive table would still be live while every surface reads the
        // box as open, which is the dangerous direction to be wrong in.
        assert!(cmd.contains("nft destroy table inet devbox || exit 1"));
        // Privilege is decided in the guest: a container exec is already root
        // and may have no sudo at all, while a VM's user needs it.
        let wrapped = elevated(&cmd);
        assert!(wrapped.contains("id -u"));
        assert!(wrapped.contains("sudo sh -c"));
        assert!(cmd.contains(RULESET_PATH));
    }
}
