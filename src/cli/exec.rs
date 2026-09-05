use anyhow::Result;
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::cli::run::SimpleRun;
use crate::obs::run::RunKind;
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

    // Recorded, not reported (§4.6). The Runs tab is meant to be the whole
    // story of a box; a box whose work is done through `exec` would otherwise
    // show an empty one. See `SimpleRun` for what these runs can and cannot
    // attribute — they get no wrapper, so no cgroup and no root pid.
    let record = SimpleRun::start(manager, &name, RunKind::Exec, &args.command);

    // The broker's environment, and the run's own id, on the command line.
    // `exec` has no guest wrapper, so this is its only route to either — and
    // without it a command run through `exec` cannot reach the broker at all
    // while the same command through `devbox run` can, which is the kind of
    // difference nobody would guess from the help text.
    let command = match manager
        .get_sandbox(&name)
        .ok()
        .and_then(|state| manager.runtime_for_sandbox(&state).ok())
    {
        Some(runtime) => {
            let env = crate::cli::run::run_env(
                manager,
                runtime.as_ref(),
                &name,
                record.as_ref().map(SimpleRun::run_id).unwrap_or_default(),
            )
            .await;
            crate::broker::with_env(&env, &args.command)
        }
        None => args.command.clone(),
    };
    let outcome = manager.exec_in_sandbox(&name, &command, false).await;
    if let Some(record) = record {
        record.finish(manager, &name, outcome.as_ref().ok().copied());
    }

    let exit_code = outcome?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
