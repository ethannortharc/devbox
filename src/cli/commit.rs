use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct CommitArgs {
    /// Only sync specific paths
    #[arg(long)]
    pub path: Option<Vec<String>>,

    /// Preview what would be synced
    #[arg(long)]
    pub dry_run: bool,

    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: CommitArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;

    if state.mount_mode == "writable" {
        println!(
            "Sandbox '{}' uses writable mode — changes are already on host.",
            name
        );
        return Ok(());
    }

    let runtime = manager.runtime_for_sandbox(&state)?;
    let paths = args.path.as_deref();

    overlay::commit(runtime.as_ref(), &name, paths, args.dry_run).await?;

    Ok(())
}
