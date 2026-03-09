use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Zellij layout to use (default, ai-pair, tdd, plain, etc.)
    /// If a session already exists, this kills it and starts fresh with the new layout.
    #[arg(long)]
    pub layout: Option<String>,

    /// Kill existing zellij session and start fresh
    #[arg(long)]
    pub restart: bool,
}

pub async fn run(args: ShellArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let force_new = args.restart || args.layout.is_some();
    manager.attach(&name, args.layout.as_deref(), force_new).await
}
