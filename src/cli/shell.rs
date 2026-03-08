use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Zellij layout to use
    #[arg(long)]
    pub layout: Option<String>,
}

pub async fn run(args: ShellArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    manager.attach(&name).await
}
