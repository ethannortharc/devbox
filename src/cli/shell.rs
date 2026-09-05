use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::cli::run::SimpleRun;
use crate::obs::run::RunKind;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ShellArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,
}

pub async fn run(args: ShellArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;

    // Recorded, not reported (§4.6), for the same reason `exec` is: the Runs
    // tab should account for a box's whole day, and a shell session is most of
    // some days. `attach` chooses the shell itself, so the argv recorded here
    // is the intent rather than the command line.
    let record = SimpleRun::start(manager, &name, RunKind::Shell, &["shell".to_string()]);
    let outcome = manager.attach(&name).await;
    if let Some(record) = record {
        record.finish(manager, &name, outcome.as_ref().ok().map(|_| 0));
    }
    outcome
}
