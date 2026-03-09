use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Sandbox name (default: current directory's sandbox)
    #[arg(long)]
    pub name: Option<String>,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub async fn run(args: ExecArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let exit_code = manager.exec_in_sandbox(&name, &args.command, false).await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
