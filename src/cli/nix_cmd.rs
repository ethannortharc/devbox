use anyhow::Result;
use clap::{Args, Subcommand};

use crate::nix;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct NixArgs {
    #[command(subcommand)]
    pub action: NixAction,
}

#[derive(Subcommand, Debug)]
pub enum NixAction {
    /// Add a Nix package (nixpkgs#pkg or github:user/flake#pkg)
    Add {
        /// Package reference
        package: String,
        /// Sandbox name
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a previously added Nix package
    Remove {
        /// Package name
        package: String,
        /// Sandbox name
        #[arg(long)]
        name: Option<String>,
    },
}

pub async fn run(args: NixArgs, manager: &SandboxManager) -> Result<()> {
    match args.action {
        NixAction::Add { package, name } => {
            let sandbox_name = manager.resolve_name(name.as_deref())?;
            if !manager.sandbox_exists(&sandbox_name) {
                anyhow::bail!("Sandbox '{}' not found.", sandbox_name);
            }
            let state = manager.get_sandbox(&sandbox_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;
            nix::add_package(runtime.as_ref(), &sandbox_name, &package).await
        }
        NixAction::Remove { package, name } => {
            let sandbox_name = manager.resolve_name(name.as_deref())?;
            if !manager.sandbox_exists(&sandbox_name) {
                anyhow::bail!("Sandbox '{}' not found.", sandbox_name);
            }
            let state = manager.get_sandbox(&sandbox_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;
            nix::remove_package(runtime.as_ref(), &sandbox_name, &package).await
        }
    }
}
