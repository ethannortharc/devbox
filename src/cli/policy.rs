//! `devbox policy` — read and change a box's egress posture (§8, §6.4).

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::policy::{Policy, Posture, Target, mirrors};
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;

#[derive(Args, Debug)]
pub struct PolicyArgs {
    #[command(subcommand)]
    pub command: PolicyCommand,
}

#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// Show a box's current posture and allowlist
    Show(ShowArgs),

    /// Change the posture
    Set(SetArgs),

    /// Add entries to the allowlist
    Allow(AllowArgs),

    /// Ask what the policy would do about a target, without connecting
    Test(TestArgs),

    /// Print the nftables ruleset the posture generates
    Rules(ShowArgs),
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// One of: open, allowlist, mirror-only, isolated
    pub posture: String,

    #[command(flatten)]
    pub boxarg: BoxArg,
}

/// The one command that keeps a visible `--name`.
///
/// Everywhere else the box is a positional. Here `entries` is variadic and
/// required, so it swallows every positional: a leading `[NAME]` would capture
/// the first domain, and a trailing one is unreachable. Rather than invent a
/// third shape, `allow` keeps the flag and says so in `--help`.
#[derive(Args, Debug)]
pub struct AllowArgs {
    /// Domains or CIDRs to permit
    #[arg(required = true)]
    pub entries: Vec<String>,

    /// Box name; defaults to the box registered for the current directory
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct TestArgs {
    /// Domain to test, e.g. `pypi.org`
    pub domain: String,

    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Address it resolves to
    #[arg(long, default_value = "203.0.113.1")]
    pub addr: String,

    /// Destination port
    #[arg(long, default_value_t = 443)]
    pub port: u16,
}

pub async fn run(args: PolicyArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        PolicyCommand::Show(a) => show(a, manager),
        PolicyCommand::Set(a) => set(a, manager).await,
        PolicyCommand::Allow(a) => allow(a, manager).await,
        PolicyCommand::Test(a) => test(a, manager),
        PolicyCommand::Rules(a) => rules(a, manager),
    }
}

/// Load a box's project config, which is where the policy lives (§12.1).
fn load(
    manager: &SandboxManager,
    name: Option<&str>,
) -> Result<(
    String,
    DevboxConfig,
    std::path::PathBuf,
    // *Both* claims, returned so both outlive this function.
    //
    // The box claim used to be a local here, so it was dropped the moment this
    // returned and the editor held nothing while it saved and applied — the
    // guard was written, committed, and did nothing. It was invisible because
    // both claims were the same type and returning one of them looked like
    // returning the lock.
    crate::web::build::BoxClaim,
    crate::web::build::ProjectClaim,
)> {
    let name = manager.resolve_name(name)?;
    // Claim, then read. Reading first left `path` and `project_dir` naming the
    // project the box belonged to *before* a concurrent `devbox use` — so `set`
    // and `allow` edited that old file and applied its posture to a box now
    // serving another project, which is how an `open` policy lands on a box
    // recorded as `isolated`.
    let (box_claim, state) = manager.claim_and_read(&name)?;
    let path = state.project_dir.join("devbox.toml");
    // The claim comes first and is returned to the caller, so it is held from
    // this read to the matching write.
    //
    // These two commands rewrite the whole file from the copy they read, and
    // took no claim at all — so `devbox policy set` overlapping a Sets rebuild
    // or a console policy save could overwrite a newly persisted selection, or
    // be overwritten itself and silently lose the posture just requested.
    // The per-box claim, taken *before* the project claim.
    //
    // Two locks exist: this one is per box and refuses immediately, the project
    // one waits. Every other path takes them in this order — box, then project
    // — and the order is not a style choice. A path that took the project claim
    // first and then waited on the box claim could sit forever holding a lock
    // that only blocks; because the box claim refuses instead of waiting,
    // nothing ever waits while holding the project lock, and the cycle cannot
    // close.
    //
    // What it fixes: policy editors locked by *project directory*, and
    // `devbox use` changes which project a box belongs to. An edit still
    // applying against the old project could land after the switch and
    // reinstall the old posture — switching from an open project to an isolated
    // one and ending up open. The box is the thing both operations are about,
    // so the box is what they have to agree on.
    let edit = crate::web::build::claim_project(&manager.state_dir, &state.project_dir)?;
    // `load_for_edit`, not `load_or_default`: every caller here may go on to
    // write the file, and falling back to defaults would erase the rest of it.
    let config = DevboxConfig::load_for_edit(&state.project_dir)?;
    Ok((name, config, path, box_claim, edit))
}

fn save(config: &DevboxConfig, path: &std::path::Path) -> Result<()> {
    config
        .save(path)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn show(args: ShowArgs, manager: &SandboxManager) -> Result<()> {
    let (name, config, _, _box_claim, _edit) = load(manager, args.boxarg.name())?;
    let policy = &config.policy;

    println!("Egress policy for '{name}':\n");
    println!("  posture: {}", policy.egress);
    println!("  {}", policy.egress.describe());
    println!(
        "  alert on violation: {}",
        if policy.alert_on_violation {
            "yes"
        } else {
            "no"
        }
    );

    if policy.allow.is_empty() {
        println!("  allowlist: (empty)");
    } else {
        println!("  allowlist:");
        for entry in &policy.allow {
            println!("    - {entry}");
        }
    }

    if policy.egress == Posture::MirrorOnly {
        println!(
            "\n  mirror-only additionally permits {} package and source hosts.",
            mirrors::all_hosts().len()
        );
        println!("  Run `devbox policy test <domain>` to check a specific one.");
    }

    Ok(())
}

async fn set(args: SetArgs, manager: &SandboxManager) -> Result<()> {
    let posture: Posture = args.posture.parse()?;
    // `edit` is held to the end of this function, so the apply below happens
    // under the same claim as the write. Releasing it in between let two
    // overlapping edits finish their *applies* in the opposite order from
    // their file writes — devbox.toml ending at `isolated` while a delayed
    // earlier `open` cleared the live table.
    let (name, mut config, path, _box_claim, _edit) = load(manager, args.boxarg.name())?;

    let previous = config.policy.egress;
    config.policy.egress = posture;
    config.policy.validate()?;
    save(&config, &path)?;

    println!("Box '{name}': egress posture {previous} → {posture}");
    println!("  {}", posture.describe());

    if posture == Posture::Allowlist && config.policy.allow.is_empty() {
        println!(
            "\n  Warning: the allowlist is empty, so nothing can be reached.\n  \
             Add entries with `devbox policy allow <domain>`."
        );
    }
    // Apply it now. The old text told the user to run `devbox reprovision`,
    // which never generated or loaded a ruleset — so an `isolated` box kept
    // full egress while reporting otherwise.
    println!();
    reapply(manager, &name, &config.policy).await
}

async fn allow(args: AllowArgs, manager: &SandboxManager) -> Result<()> {
    // Held to the end, so the reapply below runs under the same claim.
    let (name, mut config, path, _box_claim, _edit) = load(manager, args.name.as_deref())?;

    let mut added = Vec::new();
    for entry in &args.entries {
        let entry = entry.trim().to_string();
        if config.policy.allow.contains(&entry) {
            continue;
        }
        config.policy.allow.push(entry.clone());
        added.push(entry);
    }

    // Validate after adding so a bad entry is reported by name rather than
    // silently written and rejected at apply time.
    config.policy.validate()?;
    save(&config, &path)?;

    if added.is_empty() {
        println!("Box '{name}': nothing new to allow.");
        return Ok(());
    }
    println!("Box '{name}': allowed {}", added.join(", "));

    // Reapply. A new entry that only lands in `devbox.toml` stays blocked on
    // the running box until something else happens to rebuild the ruleset —
    // which reads as the allowlist simply not working.
    //
    // Unconditionally, and `apply` decides what to do with it. Skipping `open`
    // was right while `open` always meant "no table"; it stopped being right
    // when an `open` posture with alerts started installing audit rules — and
    // this is the command that *creates* that situation, by adding the first
    // allowlist entry to a policy whose alerts are already on. The audit table
    // was never installed, so the mode came into being switched off.
    reapply(manager, &name, &config.policy).await?;
    Ok(())
}

/// Push a policy to the box, if the box is running.
///
/// Shared by `set` and `allow`. A stopped box picks it up at start
/// (`service::start_box`), so nothing is silently dropped either way.
async fn reapply(
    manager: &SandboxManager,
    name: &str,
    policy: &crate::policy::Policy,
) -> Result<()> {
    let Ok(state) = manager.get_sandbox(name) else {
        println!("  No box yet; the posture applies when one is created.");
        return Ok(());
    };
    let runtime = manager.runtime_for_sandbox(&state)?;
    // Only an explicit `Stopped` defers. A probe that *failed* says nothing
    // about the box, and treating it as stopped exits 0 with the new posture
    // saved and the running firewall untouched — the exact confusion between
    // "configured" and "enforced" this command's exit status has to resolve.
    match runtime
        .status(name)
        .await
        .with_context(|| format!("could not determine whether box '{name}' is running"))?
    {
        crate::runtime::SandboxStatus::Stopped => {
            println!("  Box is not running; the posture applies when it starts.");
        }
        crate::runtime::SandboxStatus::Running => {
            // The error is returned, not printed. A script that runs
            // `devbox policy set isolated` and gets exit 0 is entitled to
            // believe the box is isolated; saving the file is not the same
            // thing as enforcing it, and only the exit status can say which
            // happened.
            crate::policy::enforce::apply(runtime.as_ref(), name, policy)
                .await
                .with_context(|| {
                    format!(
                        "posture saved to devbox.toml, but not applied to running box \
                         '{name}' — it is still using its previous egress"
                    )
                })?;
            println!("  Applied to the running box.");
        }
        other => bail!(
            "box '{name}' is in state '{other:?}'; the posture was saved but not \
             applied. Start the box, or fix its runtime state, then re-run this."
        ),
    }
    Ok(())
}

fn test(args: TestArgs, manager: &SandboxManager) -> Result<()> {
    let (name, config, _, _box_claim, _edit) = load(manager, args.boxarg.name())?;

    let target = Target {
        domain: args.domain.clone(),
        addr: args.addr.clone(),
        port: args.port,
    };
    let decision = config.policy.evaluate(&target);

    println!(
        "Box '{name}' ({}): {} → {}",
        config.policy.egress, args.domain, decision.verdict
    );
    println!("  {}", decision.reason);
    if let Some(ecosystem) = mirrors::ecosystem_of(&args.domain) {
        println!("  ({ecosystem} package host)");
    }

    // Non-zero exit for a *denial*, so this is usable in a script. A `flag`
    // verdict is permitted traffic that is merely recorded, so it must not
    // read as a denial.
    if decision.verdict == crate::policy::Verdict::Block {
        bail!("{} is not permitted by the current policy", args.domain);
    }
    Ok(())
}

fn rules(args: ShowArgs, manager: &SandboxManager) -> Result<()> {
    let (_, config, _, _box_claim, _edit) = load(manager, args.boxarg.name())?;
    print!("{}", crate::policy::nftables::ruleset(&config.policy));
    Ok(())
}

/// Merge new allowlist entries into a policy, rejecting invalid ones.
///
/// Shared with the console's policy editor so both paths validate identically.
pub fn apply_edit(policy: &mut Policy, posture: Posture, allow: Vec<String>) -> Result<()> {
    let candidate = Policy {
        egress: posture,
        allow: allow
            .into_iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect(),
        alert_on_violation: policy.alert_on_violation,
    };
    candidate.validate()?;
    *policy = candidate;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_replaces_the_whole_allowlist() {
        // The console posts the full list, so an edit is a replacement, not a
        // merge — otherwise removing an entry in the UI would do nothing.
        let mut policy = Policy {
            egress: Posture::Open,
            allow: vec!["old.example".into()],
            alert_on_violation: true,
        };

        apply_edit(
            &mut policy,
            Posture::Allowlist,
            vec!["github.com".into(), "10.0.0.0/8".into()],
        )
        .unwrap();

        assert_eq!(policy.egress, Posture::Allowlist);
        assert_eq!(policy.allow, vec!["github.com", "10.0.0.0/8"]);
        assert!(
            policy.alert_on_violation,
            "unrelated settings are preserved"
        );
    }

    #[test]
    fn an_edit_drops_blank_entries() {
        let mut policy = Policy::default();
        apply_edit(
            &mut policy,
            Posture::Allowlist,
            vec!["  ".into(), "github.com".into(), String::new()],
        )
        .unwrap();
        assert_eq!(policy.allow, vec!["github.com"]);
    }

    #[test]
    fn an_invalid_edit_leaves_the_policy_untouched() {
        let mut policy = Policy {
            egress: Posture::Allowlist,
            allow: vec!["github.com".into()],
            alert_on_violation: true,
        };

        let err = apply_edit(&mut policy, Posture::Allowlist, vec!["no-dot".into()]).unwrap_err();
        assert!(err.to_string().contains("no-dot"));
        assert_eq!(
            policy.allow,
            vec!["github.com"],
            "a rejected edit must not partially apply"
        );
    }
}
