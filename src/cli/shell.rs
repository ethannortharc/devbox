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
    let extra: Vec<(String, String)> = record
        .as_ref()
        .map(|r| {
            vec![(
                "DEVBOX_RUN_ID".to_string(),
                SimpleRun::run_id(r).to_string(),
            )]
        })
        .unwrap_or_default();
    // `attach` already puts the broker's environment on the shell's command
    // line; this adds the one variable it cannot know.
    let outcome = manager.attach_with_env(&name, &extra).await;
    if let Some(record) = record {
        record.finish(manager, &name, outcome.as_ref().ok().map(|_| 0));
    }
    outcome
}
