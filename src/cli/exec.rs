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

    // First, and before anything is recorded. This is where a box gets the
    // repair it has been waiting for — and what a repair waits for is a run
    // being in flight, which is exactly what the row below looks like. In the
    // other order the repair deferred to a run that had not started yet and
    // whose only purpose was to record this command, so `exec` could never be
    // the thing that fixed a box: the release notes' "`devbox exec <box> --
    // true` will do it" was false for the whole of 0.2.1.
    //
    // The order also keeps the repair's agent restart out of the command's own
    // capture window, which is why this is a reordering and not a matter of
    // teaching the check to overlook one run id.
    let prepared = manager.prepare_running_for_use(&name).await?;

    // Recorded, not reported (§4.6). The Runs tab is meant to be the whole
    // story of a box; a box whose work is done through `exec` would otherwise
    // show an empty one. See `SimpleRun` for what these runs can and cannot
    // attribute — they get no wrapper, so no cgroup and no root pid.
    let record = SimpleRun::start(&prepared, manager, &name, RunKind::Exec, &args.command);

    // The broker's environment, and the run's own id, on the command line.
    // `exec` has no guest wrapper, so this is its only route to either — and
    // without it a command run through `exec` cannot reach the broker at all
    // while the same command through `devbox run` can, which is the kind of
    // difference nobody would guess from the help text.
    let env = crate::cli::run::run_env(
        manager,
        prepared.runtime(),
        &name,
        record.as_ref().map(SimpleRun::run_id).unwrap_or_default(),
    )
    .await;
    let command = crate::broker::with_env(&env, &args.command);
    let outcome = manager
        .exec_prepared(prepared, &name, &command, false)
        .await;
    if let Some(record) = record {
        record.finish(manager, &name, outcome.as_ref().ok().copied());
    }

    let exit_code = outcome?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::box_arg::BoxArg;
    use crate::sandbox::state::{SCHEMA, SandboxState};

    /// A box on a runtime that does not exist, so that preparing it fails and
    /// fails early — before anything the guest is asked to do, and before
    /// anything at all is written down.
    fn unpreparable_box(state_dir: &std::path::Path, name: &str) {
        SandboxState {
            schema: SCHEMA,
            name: name.into(),
            runtime: "nonesuch".into(),
            project_dir: state_dir.join("project"),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            package_sources: Default::default(),
        }
        .save(state_dir)
        .unwrap();
    }

    /// Every run this box has on record.
    fn recorded_runs(state_dir: &std::path::Path, name: &str) -> Vec<crate::obs::RunRecord> {
        let path = crate::obs::collector::store_path(state_dir, name);
        if !path.exists() {
            return Vec::new();
        }
        crate::obs::Store::open(&path)
            .expect("open the box's store")
            .list_runs(16)
            .expect("read the box's runs")
    }

    /// The ordering that made a pending repair unreachable.
    ///
    /// `exec` used to open its run row first and prepare the box second. By
    /// the time preparation asked whether a repair could go ahead, the store
    /// already held a run marked `running` — this command's own — and every
    /// repair politely stood aside for it. The box stayed unrepaired however
    /// many times `devbox exec` was run, each time printing that the fix would
    /// happen once the run ended.
    ///
    /// Testing that from the outside would take a live box: preparation
    /// resolves a real hypervisor. What it takes to show the order is a box
    /// that cannot be prepared at all — then the run row is present only if it
    /// was written before preparation was attempted, which is the whole bug.
    #[tokio::test]
    async fn no_run_is_recorded_until_the_box_has_been_prepared() {
        let dir = tempfile::tempdir().unwrap();
        unpreparable_box(dir.path(), "alpha");
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };

        let error = run(
            ExecArgs {
                boxarg: BoxArg {
                    name_pos: Some("alpha".into()),
                    ..Default::default()
                },
                command: vec!["true".to_string()],
            },
            &manager,
        )
        .await
        .expect_err("a box on an unknown runtime cannot be prepared");
        assert!(
            format!("{error:#}").contains("Unknown runtime"),
            "the box failed for some other reason, so this proves nothing: {error:#}"
        );

        let runs = recorded_runs(dir.path(), "alpha");
        assert!(
            runs.is_empty(),
            "`exec` recorded {} run(s) before preparing the box; a repair waiting for a quiet moment would have deferred to one of them",
            runs.len()
        );
    }
}
