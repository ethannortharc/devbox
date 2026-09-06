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

    // Before the run row exists, for the reason spelled out in `exec`: a
    // recorded run is what makes a pending repair keep waiting, and a shell
    // session is the longest-lived run there is — a box whose agent update
    // waited for one could wait all day.
    let prepared = manager.prepare_running_for_use(&name).await?;

    // Recorded, not reported (§4.6), for the same reason `exec` is: the Runs
    // tab should account for a box's whole day, and a shell session is most of
    // some days. `attach` chooses the shell itself, so the argv recorded here
    // is the intent rather than the command line.
    let record = SimpleRun::start(
        &prepared,
        manager,
        &name,
        RunKind::Shell,
        &["shell".to_string()],
    );
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
    let outcome = manager.attach_prepared(prepared, &name, &extra).await;
    if let Some(record) = record {
        record.finish(manager, &name, outcome.as_ref().ok().map(|_| 0));
    }
    outcome
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

    /// Same ordering, same reason, longer run.
    ///
    /// A shell session is the longest-lived run a box gets, so `shell`
    /// recording one before preparing meant a pending repair waited for the
    /// user to close the shell — and the next `shell` opened another one.
    #[tokio::test]
    async fn no_run_is_recorded_until_the_box_has_been_prepared() {
        let dir = tempfile::tempdir().unwrap();
        unpreparable_box(dir.path(), "alpha");
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };

        let error = run(
            ShellArgs {
                boxarg: BoxArg {
                    name_pos: Some("alpha".into()),
                    ..Default::default()
                },
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
            "`shell` recorded {} run(s) before preparing the box; a repair waiting for a quiet moment would have deferred to one of them",
            runs.len()
        );
    }
}
