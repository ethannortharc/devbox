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
            crate::policy::enforce::apply_saved(manager, &state, &name).await?;
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

    // Read the posture *before* provisioning, not after. The rebuild removes
    // the box's firewall, so discovering an unreadable devbox.toml afterwards
    // leaves a live box unrestricted with no saved posture to restore.
    let saved_policy = crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
        .context(
            "refusing to reprovision: this box's devbox.toml cannot be read, and \
             rebuilding would remove its firewall with no posture to restore",
        )?
        .policy;

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
    provision::provision_vm_full(
        runtime.as_ref(),
        &name,
        &sets,
        &state.languages,
        image,
        &state.mount_mode,
        &provision::package_pairs(&state),
    )
    .await?;

    // Update saved state with migrated sets
    let mut updated_state = state.clone();
    updated_state.sets = sets;
    updated_state.save(&manager.state_dir)?;

    // Re-apply the posture read before the rebuild. Provisioning rebuilds the
    // box's network stack, so whatever was enforced is gone; without this the
    // box comes back open no matter what `devbox.toml` says. Using the value
    // captured up front means a file that became unreadable *during* the
    // rebuild cannot leave the box unrestricted either.
    if saved_policy.egress != crate::policy::Posture::Open {
        let runtime = manager.runtime_for_sandbox(&updated_state)?;
        crate::policy::enforce::apply(runtime.as_ref(), &name, &saved_policy).await?;
        println!("Egress posture '{}' re-applied.", saved_policy.egress);
    }

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
