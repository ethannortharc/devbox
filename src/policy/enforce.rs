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

/// Atomic effective-capability status published by the supervised agent.
pub const AGENT_STATUS_PATH: &str = "/run/devbox/obsd-status.json";

/// Push the policy into the box and load it.
///
/// Idempotent, because the generated ruleset destroys the devbox table before
/// recreating it: applying twice leaves what applying once leaves.
pub async fn apply(runtime: &dyn Runtime, sandbox_name: &str, policy: &Policy) -> Result<()> {
    // `open` is usually the absence of a policy rather than a policy of its
    // own, and loading a ruleset for it would leave an empty devbox table in
    // the box suggesting something is being enforced.
    //
    // Unless it audits. `open` with an allowlist and alerts on is
    // observe-and-warn, and it needs a table to do the observing with.
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
    //
    // And only where the ruleset denies at all: under an auditing `open`
    // posture an unpopulated allow set means over-reporting rather than a box
    // cut off, so refusing to install would be the worse answer.
    // Only the postures that actually consult the DNS-derived sets — a
    // question about what a posture requires, answered where the others are.
    let needs_agent = super::nftables::needs_dns_agent(policy, &domains);
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
             (The agent reports its effective capture after kernel preflight; \
             requested flags are not enough. Restart or reprovision an older \
             agent that has no status file.) \
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
            &[
                "sh",
                "-c",
                &elevated(&load_command(policy.egress.enforces())),
            ],
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
/// Probed rather than assumed: an older or manually started agent may be live
/// without the packet source the current provisioner enables explicitly.
async fn agent_resolves_dns(runtime: &dyn Runtime, sandbox_name: &str) -> bool {
    // Requested argv is not authority: AF_PACKET can fail after
    // `-packet=true` was parsed. The supervised agent atomically publishes the
    // effective Domains list, including transitions made by its in-process
    // packet retry loop.
    let Ok(result) = runtime
        .exec_cmd(sandbox_name, &["cat", AGENT_STATUS_PATH], false)
        .await
    else {
        return false;
    };
    if result.exit_code != 0 {
        return false;
    }
    let Some(status) = parse_agent_status(&result.stdout) else {
        return false;
    };
    if !status.policy_configured || !status.capture.iter().any(|domain| domain == "dns") {
        return false;
    }

    // A hard-killed process cannot remove its status file. Tie the snapshot
    // to the process which published it so the short systemd restart window
    // never admits a default-deny policy on stale capabilities.
    let proc_comm = format!("/proc/{}/comm", status.pid);
    runtime
        .exec_cmd(sandbox_name, &["cat", &proc_comm], false)
        .await
        .is_ok_and(|r| r.exit_code == 0 && r.stdout.trim() == "devbox-obsd")
}

#[derive(serde::Deserialize)]
struct AgentStatus {
    pid: u32,
    capture: Vec<String>,
    policy_configured: bool,
}

fn parse_agent_status(raw: &str) -> Option<AgentStatus> {
    let status: AgentStatus = serde_json::from_str(raw).ok()?;
    (status.pid > 0).then_some(status)
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
fn load_command(enforcing: bool) -> String {
    // Only where the new rules can refuse something.
    //
    // Flushing exists so that connections opened under a looser posture are
    // re-judged by a stricter one. An auditing `open` posture judges nothing —
    // it accepts everything and logs what the allowlist misses — so tearing
    // down conntrack there would drop established NAT state, and a nested
    // container's flows with it, in the one posture that promises not to
    // interrupt traffic. Turning on observe-only would have been more
    // disruptive than turning on enforcement.
    if !enforcing {
        return format!("nft -f {RULESET_PATH}");
    }
    format!(
        "nft -f {RULESET_PATH} && \
         (conntrack -F 2>/dev/null || true)"
    )
}

/// Read the box's resolvers and any hosted prefixes it declares.
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

    // Declared prefixes: a service that stands subnets up inside a box writes
    // them under /etc/devbox/prefixes/ on the way up — one file per owner, one
    // CIDR per line — and removes its file on teardown. Reading that directory
    // here is the whole handoff (ADR-0046): the ruleset generator runs on the
    // host and has no other way to learn those subnets exist.
    //
    // The glob is the contract. With no directory, or an empty one, `sh` passes
    // the pattern through unmatched and `cat` exits 1 (measured, not assumed —
    // `2>/dev/null` hides the message, not the status). The `exit_code == 0`
    // filter below turns that into an empty list, which is the answer we want:
    // a box that declares nothing gets nothing, and `isolated` then means
    // loopback only, which is what the posture says.
    //
    // Failing to empty is also the safe direction for a *partial* failure. An
    // unreadable entry loses every prefix in the directory, not just its own,
    // so the ruleset gets stricter rather than accidentally permissive.
    let declared_prefixes = runtime
        .exec_cmd(
            sandbox_name,
            &["sh", "-c", "cat /etc/devbox/prefixes/* 2>/dev/null"],
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
        declared_prefixes,
        container_prefixes,
        internal_ifaces,
    }
}

/// Parse hosted prefixes, one per line.
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

    // The generation the agent must insert under.
    //
    // Its allow sets carry it in their names, so an enforcer superseded while a
    // DNS answer was in flight names a set that no longer exists and its insert
    // fails — rather than landing an address only the *old* allowlist permitted
    // into the new table for the full hour of the TTL.
    format!(
        "{{\"egress\":\"{}\",\"allow\":[{}],\"generation\":\"{}\",\"ruleset\":\"{}\"}}",
        policy.egress,
        domains.join(","),
        super::nftables::generation(policy),
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

/// Restore a box's posture after a rebuild, strictly.
///
/// A successful rebuild must not be reported while the saved firewall posture
/// is absent. The caller already holds the box claim, so this variant restores
/// the posture without trying to claim the box a second time.
pub async fn restore_after_rebuild(
    manager: &crate::sandbox::SandboxManager,
    name: &str,
    claim: &crate::web::build::BoxClaim,
) -> Result<()> {
    debug_assert_eq!(
        claim.box_name(),
        name,
        "a claim on one box proves nothing about another"
    );
    // Read here, under the claim, rather than taken from the caller.
    //
    // Callers read a box's state long before they claim it — for validation,
    // for a preview, for the name. A `devbox use` completing in that gap
    // releases its own claim, so the caller's claim then succeeds over a
    // snapshot naming the project the box has just stopped belonging to, and
    // this would install that project's posture on the box now serving another.
    // With `open` on one side and `isolated` on the other, that clears a
    // firewall that the file still says is up.
    //
    // Not a parameter any more, because a parameter is a place for a stale
    // value to arrive from.
    let state = &manager.get_sandbox(name)?;
    // Read and apply under the same claim the editors take.
    //
    // Without it, this reads posture A, a Policy-tab save or `devbox policy
    // set` writes and applies B under the lock, and then this applies stale A
    // on top. The file says B and nftables enforces A — and when A is the more
    // open of the two, that silently reopens egress on a box whose recorded
    // posture says it is closed. Round 39 put the write side under this lock
    // and left the read side outside it, which makes the pair only half
    // serialised: a lock that one participant ignores orders nothing.
    let lock_dir = manager.state_dir.clone();
    let lock_project = state.project_dir.clone();
    let _edit = crate::web::build::claim_project_off_worker(move || {
        crate::web::build::claim_project(&lock_dir, &lock_project)
    })
    .await?;

    let config = crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
        .with_context(|| {
            format!(
                "box '{name}' rebuilt, but its devbox.toml could not be read, so \
                     the egress posture it should have is unknown"
            )
        })?;
    let runtime = manager.runtime_for_sandbox(state)?;

    // `apply` decides between installing and clearing: an `open` posture that
    // audits still needs its table, and duplicating that test here is what
    // turned every rebuild into a silent way to switch observing off.
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

/// Apply the saved posture if this box is not already owned by someone.
///
/// For the paths that merely *use* a box — `exec`, `attach`, `code` — rather
/// than changing it. They want the posture to be true, and they have no
/// business queueing behind a rebuild to say so.
///
/// A refused claim is not a failure here. The holder is a rebuild, and a
/// rebuild is already obliged to restore the posture before it returns; a
/// second application on top of that would be redundant at best and a stale
/// overwrite at worst. Stepping aside is the whole answer.
///
/// This existed as a naming convention before — nine of eighteen policy
/// applications ran with no claim at all, and nothing said which of them meant
/// to.
/// Restore the saved posture after a rebuild this process does not own.
///
/// Only for callers that rebuild *through another process* — a caller that
/// drives `nixos-rebuild` inside a box it does not hold a claim on. A caller
/// that does its own rebuilding holds the claim and must use
/// [`restore_after_rebuild`], which requires it.
pub async fn restore_after_rebuild_or_step_aside(
    manager: &crate::sandbox::SandboxManager,
    name: &str,
) -> Result<()> {
    let Some(claim) = crate::web::build::try_claim_box(&manager.state_dir, name)? else {
        tracing::debug!(
            box_id = %name,
            "a rebuild owns this box; leaving the egress posture to it"
        );
        return Ok(());
    };
    restore_after_rebuild(manager, name, &claim).await
}

pub async fn apply_saved_or_step_aside(
    manager: &crate::sandbox::SandboxManager,
    name: &str,
) -> Result<()> {
    // `?`, not a shrug. Contention is `Ok(None)`; anything else means the claim
    // could not be evaluated — an unwritable state directory, a filesystem that
    // will not lock — and treating that as "a rebuild is running" let `attach`,
    // `exec` and `code` carry on having applied no egress policy at all.
    let Some(claim) = crate::web::build::try_claim_box(&manager.state_dir, name)? else {
        tracing::debug!(
            box_id = %name,
            "a rebuild owns this box; leaving the egress posture to it"
        );
        return Ok(());
    };
    apply_saved(manager, name, &claim).await
}

/// Load and strictly apply the box's saved egress posture.
///
/// A firewall does not survive a restart. Every access path therefore restores
/// the saved posture before declaring the box usable. An unreadable config or
/// enforcement failure is returned to the caller; start/use paths then stop the
/// box fail-closed so the UI and CLI cannot expose an unrestricted shell.
pub async fn apply_saved(
    manager: &crate::sandbox::SandboxManager,
    name: &str,
    claim: &crate::web::build::BoxClaim,
) -> Result<()> {
    debug_assert_eq!(
        claim.box_name(),
        name,
        "a claim on one box proves nothing about another"
    );
    // Read here, under the claim, rather than taken from the caller.
    //
    // Callers read a box's state long before they claim it — for validation,
    // for a preview, for the name. A `devbox use` completing in that gap
    // releases its own claim, so the caller's claim then succeeds over a
    // snapshot naming the project the box has just stopped belonging to, and
    // this would install that project's posture on the box now serving another.
    // With `open` on one side and `isolated` on the other, that clears a
    // firewall that the file still says is up.
    //
    // Not a parameter any more, because a parameter is a place for a stale
    // value to arrive from.
    let state = &manager.get_sandbox(name)?;
    // Loaded fallibly. `load_or_default` turns a malformed `devbox.toml` into
    // the *default* config, whose posture is `open` — so a corrupted file would
    // silently unfirewall a box that had been isolated, and report nothing.
    // Corruption is not consent.
    // Read and apply under the same claim the editors take.
    //
    // Without it, this reads posture A, a Policy-tab save or `devbox policy
    // set` writes and applies B under the lock, and then this applies stale A
    // on top. The file says B and nftables enforces A — and when A is the more
    // open of the two, that silently reopens egress on a box whose recorded
    // posture says it is closed. Round 39 put the write side under this lock
    // and left the read side outside it, which makes the pair only half
    // serialised: a lock that one participant ignores orders nothing.
    let lock_dir = manager.state_dir.clone();
    let lock_project = state.project_dir.clone();
    let _edit = crate::web::build::claim_project_off_worker(move || {
        crate::web::build::claim_project(&lock_dir, &lock_project)
    })
    .await?;

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
            return Err(e).with_context(|| {
                format!(
                    "box '{name}' has an unreadable devbox.toml, so its egress posture is unknown"
                )
            });
        }
    };
    let runtime = manager.runtime_for_sandbox(state)?;
    // Straight to `apply`, which already knows that an `open` posture clears
    // unless it audits.
    //
    // This branched on `Open` itself and cleared, which was right until `open`
    // stopped always meaning "no table". Then it silently disabled
    // observe-and-warn on start, on attach, and on access — every routine
    // operation — until someone set the policy again. Restating a rule in a
    // second place is how it goes stale; there is one statement of it now.
    if let Err(e) = apply(runtime.as_ref(), name, &config.policy).await {
        let posture = config.policy.egress;
        tracing::error!(box_id = %name, %posture, error = ?e, "egress posture not applied");
        // stderr as well as the returned error: the log is off by default, and
        // a CLI user needs the same actionable explanation the browser card
        // receives. This used to return success after printing the warning,
        // which made Start and Terminal claim an unrestricted box was ready.
        eprintln!(
            "\ndevbox: WARNING — box '{name}' is running WITHOUT its '{posture}' egress \
             posture.\n  {e}\n  Traffic is unrestricted. Fix the cause, then \
             `devbox policy set {posture}` to apply it.\n"
        );
        return Err(e).with_context(|| {
            format!("box '{name}' could not apply its '{posture}' egress posture")
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A path that only *uses* a box must not fight a rebuild for it.
    ///
    /// This is the contention rule, and it is a behaviour rather than a
    /// convention now: `exec`, `attach` and `code` want the posture to be
    /// true, and when a rebuild owns the box the rebuild is already obliged
    /// to make it true before it returns. Applying on top would be redundant at
    /// best and a stale overwrite at worst.
    ///
    /// The failure this pins is not an error — it is the opposite. If this ever
    /// starts returning `Err`, every one of those commands begins failing for
    /// the minutes a `nixos-rebuild` takes.
    #[tokio::test]
    async fn using_a_box_someone_else_is_rebuilding_steps_aside() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manager = crate::sandbox::SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        // Someone else is mid-rebuild.
        let _held = crate::web::build::claim_box(dir.path(), "busy").expect("first claim");

        let outcome = apply_saved_or_step_aside(&manager, "busy").await;
        assert!(
            outcome.is_ok(),
            "a used box must not fail because a rebuild owns it: {:?}",
            outcome.err()
        );
    }

    /// And with nobody holding it, the same call really does try.
    ///
    /// Without this the test above passes just as well on a function that
    /// always returns `Ok`, which is the shape of a guard that proves nothing.
    #[tokio::test]
    async fn with_no_rebuild_in_flight_it_actually_applies() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manager = crate::sandbox::SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        // No claim held, so this proceeds — and reaches the runtime lookup for
        // a runtime that does not exist. Any outcome but "stepped quietly
        // aside" proves the claim was taken and the work attempted.
        let outcome = apply_saved_or_step_aside(&manager, "free").await;
        assert!(
            outcome.is_err(),
            "with the claim free this must do the work, not skip it"
        );
    }

    use super::*;

    #[test]
    fn hosted_prefixes_are_parsed_and_validated() {
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
    fn only_effective_live_dns_status_can_satisfy_the_policy_guard() {
        let ready = parse_agent_status(
            r#"{"pid":42,"capture":["exec","dns","tls"],"policy_configured":true}"#,
        )
        .expect("valid status");
        assert_eq!(ready.pid, 42);
        assert!(ready.capture.iter().any(|domain| domain == "dns"));
        assert!(ready.policy_configured);

        let degraded = parse_agent_status(
            r#"{"pid":42,"capture":["exec","connect"],"policy_configured":true}"#,
        )
        .expect("valid degraded status");
        assert!(!degraded.capture.iter().any(|domain| domain == "dns"));

        assert!(
            parse_agent_status(r#"{"pid":0,"capture":["dns"],"policy_configured":true}"#).is_none()
        );
        assert!(parse_agent_status("not json").is_none());
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
        let cmd = load_command(true);
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
