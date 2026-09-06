use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct StopArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Stop even if the box will not be able to start again
    ///
    /// `stop` repairs a box that could not boot afterwards, and refuses to
    /// stop it if that repair fails. This says stop it anyway.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: StopArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    manager.stop_sandbox(&name, args.force).await
}
