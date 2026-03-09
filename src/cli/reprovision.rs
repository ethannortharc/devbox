use anyhow::Result;
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
        }
        SandboxStatus::NotFound => {
            anyhow::bail!(
                "Sandbox '{}' exists in state but not in runtime '{}'. \
                 Run `devbox destroy {}` to clean up.",
                name, state.runtime, name
            );
        }
        SandboxStatus::Unknown(s) => {
            anyhow::bail!("Sandbox '{}' is in unknown state: {}", name, s);
        }
    }

    println!("Re-provisioning sandbox '{name}'...");
    println!("This will push all config files and rebuild the system.");

    // Migrate old set names (e.g., "ai" → "ai-code" + "ai-infra")
    let mut sets = migrate_sets(&state.sets);

    // Ensure ai-code is always present (default on)
    if !sets.iter().any(|s| s == "ai-code") {
        sets.push("ai-code".to_string());
    }

    // Re-run full provisioning with the (migrated) sets/languages
    // Pass mount_mode so NixOS module sets up overlay declaratively
    let image = state.image.as_str();
    provision::provision_vm_with_mode(
        runtime.as_ref(),
        &name,
        &sets,
        &state.languages,
        image,
        &state.mount_mode,
    )
    .await?;

    // Update saved state with migrated sets
    let mut updated_state = state.clone();
    updated_state.sets = sets;
    updated_state.save(&manager.state_dir)?;

    println!("Re-provisioning complete. Run `devbox shell --name {name}` to attach.");
    Ok(())
}

/// Migrate old set names to current names.
/// "ai" → "ai-code" (ai-infra stays off unless explicitly added).
fn migrate_sets(sets: &[String]) -> Vec<String> {
    let mut result: Vec<String> = sets
        .iter()
        .filter_map(|s| {
            match s.as_str() {
                "ai" => Some("ai-code".to_string()), // old "ai" → "ai-code"
                other => Some(other.to_string()),
            }
        })
        .collect();

    // Deduplicate
    result.sort();
    result.dedup();
    result
}
