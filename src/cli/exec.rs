use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;

/// `devbox exec [NAME] -- CMD…`.
///
/// `command` is `last`, so it is only reachable after `--`. That is what keeps
/// the optional box name unambiguous: everything before `--` is the box,
/// everything after it is the command, and `devbox exec -- ls` still means
/// "the current directory's box" because clap jumps straight to the trailing
/// argument when it sees `--`.
#[derive(Args, Debug)]
pub struct ExecArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub async fn run(args: ExecArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    let exit_code = manager.exec_in_sandbox(&name, &args.command, false).await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
