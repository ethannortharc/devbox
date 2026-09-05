use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ShellArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,
}

pub async fn run(args: ShellArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    manager.attach(&name).await
}
