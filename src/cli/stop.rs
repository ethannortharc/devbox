use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct StopArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,
}

pub async fn run(args: StopArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    manager.stop_sandbox(&name).await
}
