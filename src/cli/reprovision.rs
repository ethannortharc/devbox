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

    // Re-run full provisioning with the saved sets/languages
    let image = state.image.as_str();
    provision::provision_vm(
        runtime.as_ref(),
        &name,
        &state.sets,
        &state.languages,
        image,
    )
    .await?;

    println!("Re-provisioning complete. Run `devbox shell --name {name}` to attach.");
    Ok(())
}
