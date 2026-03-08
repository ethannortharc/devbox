use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct StopArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,
}

pub async fn run(args: StopArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    manager.stop_sandbox(&name).await
}
