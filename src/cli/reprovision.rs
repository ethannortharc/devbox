use anyhow::{Context, Result};
use clap::Args;

use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;
use crate::sandbox::provision;

#[derive(Args, Debug)]
pub struct ReprovisionArgs {
    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: ReprovisionArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    // Ensure VM is running
    let status = runtime.status(&name).await?;
    match status {
        SandboxStatus::Running => {}
        SandboxStatus::Stopped => {
            println!("Starting sandbox '{name}'...");
            runtime.start(&name).await?;
            crate::policy::enforce::apply_saved_or_step_aside(manager, &name).await?;
        }
        SandboxStatus::NotFound => {
            anyhow::bail!(
                "Sandbox '{}' exists in state but not in runtime '{}'. \
                 Run `devbox destroy {}` to clean up.",
                name,
                state.runtime,
                name
            );
        }
        SandboxStatus::Unknown(s) => {
            anyhow::bail!("Sandbox '{}' is in unknown state: {}", name, s);
        }
    }

    // Read the posture *before* provisioning — as a precondition, not as the
    // value to reinstate.
    //
    // The rebuild removes the box's firewall, so discovering an unreadable
    // devbox.toml afterwards leaves a live box unrestricted with no posture to
    // restore. That check is worth keeping. What is *not* safe is carrying this
    // copy forward: provisioning takes minutes, and applying a posture captured
    // before it lets an edit in that window be silently overwritten. The
    // restore below re-reads under the editors' claim instead.
    crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir).context(
        "refusing to reprovision: this box's devbox.toml cannot be read, and \
         rebuilding would remove its firewall with no posture to restore",
    )?;

    println!("Re-provisioning sandbox '{name}'...");
    println!("This will push all config files and rebuild the system.");

    // Migrate old set names (e.g., "ai" → "ai-code" + "ai-infra")
    let sets = migrate_sets(&state.sets);

    // Deliberately no "ensure ai-code is present" fallback here.
    //
    // It predates the set being optional, and it silently reinstated a set the
    // user had unchecked — on every reprovision, persisted afterwards, so the
    // choice could not be made to stick. `migrate_sets` still maps the legacy
    // `ai` name onto `ai-code`, which is the only case that genuinely needs
    // filling in; an absent `ai-code` in a modern state means absent.

    // Re-run full provisioning with the (migrated) sets/languages
    // Pass mount_mode so NixOS module sets up overlay declaratively
    let image = state.image.as_str();
    // The same claim the Sets paths take: this rewrites the box's generated
    // configuration too, so a console rebuild running beside it would
    // interleave writes and leave the active generation and the recorded
    // selection describing different things.
    let claim = crate::web::build::claim_box(&manager.state_dir, &name)?;
    // Re-read under the claim. The copy above was taken for validation, long
    // before this claim existed, and a `devbox use` completing in the gap
    // releases its own claim — so this one succeeds over a snapshot naming the
    // project the box has just stopped belonging to.
    let state = manager.get_sandbox(&name)?;

    // Resolved once, and used for both the rebuild and the state written after
    // it. A v3 box has no `packages`, so these come out of its project file —
    // and the save below stamps the schema, which is what makes recording them
    // the difference between a recoverable gap and a permanent one.
    let packages = provision::resolved_packages(&state);

    // Not `?`. On NixOS `provision_vm_full` runs `nixos-rebuild switch` before
    // its later shell and helper-file steps, so a failure in one of those
    // returns with the network generation already switched and devbox's
    // nftables table already gone. A restrictive box would be left open.
    let provisioned = provision::provision_vm_full(
        runtime.as_ref(),
        &name,
        &sets,
        &state.languages,
        image,
        &state.mount_mode,
        &packages.0,
    )
    .await;
    if let Err(e) = provisioned {
        if let Err(restore) =
            crate::policy::enforce::restore_after_rebuild(manager, &name, &claim).await
        {
            eprintln!(
                "devbox: WARNING — reprovisioning failed *and* the egress posture could \
                 not be restored, so box '{name}' may be running unrestricted: {restore}"
            );
        }
        return Err(e);
    }

    // Update saved state with migrated sets
    let mut updated_state = state.clone();
    updated_state.sets = sets;
    updated_state.packages = packages.1.packages.iter().cloned().collect();
    updated_state.package_sources = packages.1.sources.clone();
    // Unconditionally: `apply` clears or installs as the posture requires, and
    // an `open` posture that audits requires a table. Testing the posture here
    // meant a reprovision silently dropped observe-and-warn.
    // Re-read rather than replay. `saved_policy` was captured before a
    // provisioning run that takes minutes, and applying it here without the
    // editor's lock let a policy edit save and apply posture B in the gap and
    // then be overwritten by stale A — leaving the file saying B while nftables
    // enforced A. `restore_after_rebuild` loads and applies under the same
    // claim the editors take, which is the only way the two can be ordered.
    let restored = crate::policy::enforce::restore_after_rebuild(manager, &name, &claim).await;

    // The bookkeeping happens whether or not the firewall came back.
    //
    // The rebuild already ran, and it migrated legacy set names — `ai` became
    // `ai-code` — so returning the restore error first left `state.json`
    // naming a set the catalog no longer has, for a box whose active
    // generation had already changed. Later set operations then reject the
    // stale name or report a selection the box does not have. The posture
    // failure is the more urgent news, so it is still what the command exits
    // with; it is just no longer a reason to lose the record.
    updated_state.save(&manager.state_dir)?;
    restored?;

    // Re-apply the posture read before the rebuild. Provisioning rebuilds the
    // box's network stack, so whatever was enforced is gone; without this the
    // box comes back open no matter what `devbox.toml` says. Using the value
    // captured up front means a file that became unreadable *during* the
    // rebuild cannot leave the box unrestricted either.

    println!("Re-provisioning complete. Run `devbox shell --name {name}` to attach.");
    Ok(())
}

/// Migrate old set names to current names.
/// "ai" → "ai-code" (ai-infra stays off unless explicitly added).
fn migrate_sets(sets: &[String]) -> Vec<String> {
    let mut result: Vec<String> = sets
        .iter()
        .map(|s| {
            match s.as_str() {
                "ai" => "ai-code".to_string(), // old "ai" → "ai-code"
                other => other.to_string(),
            }
        })
        .collect();

    // Deduplicate
    result.sort();
    result.dedup();
    result
}
