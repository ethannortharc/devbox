use anyhow::{Context, Error, Result, anyhow, bail};
use clap::{Args, Subcommand};

use crate::cli::box_arg::SnapshotBoxArg;
use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;
use crate::sandbox::state::is_safe_name;

async fn stop_after_restore_failure(
    runtime: &dyn crate::runtime::Runtime,
    sandbox_name: &str,
    error: Error,
) -> Error {
    match runtime.stop(sandbox_name).await {
        Ok(()) => error.context(format!(
            "sandbox '{sandbox_name}' was stopped for safety because snapshot reconciliation did not complete"
        )),
        Err(stop_error) => anyhow!(
            "snapshot reconciliation failed: {error:#}; additionally, sandbox '{sandbox_name}' could not be stopped for safety: {stop_error:#}"
        ),
    }
}

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub action: SnapshotAction,
}

/// The snapshot's own name comes first and the box second, because the box is
/// the optional one: `devbox snapshot save nightly` has to keep meaning "this
/// directory's box", which it cannot if the leading positional is the box.
#[derive(Subcommand, Debug)]
pub enum SnapshotAction {
    /// Create a named snapshot
    Save {
        /// Snapshot name
        #[arg(value_name = "SNAPSHOT")]
        snapshot: String,
        #[command(flatten)]
        boxarg: SnapshotBoxArg,
    },
    /// Restore a snapshot
    Restore {
        /// Snapshot name
        #[arg(value_name = "SNAPSHOT")]
        snapshot: String,
        #[command(flatten)]
        boxarg: SnapshotBoxArg,
    },
    /// List all snapshots
    List {
        #[command(flatten)]
        boxarg: SnapshotBoxArg,
    },
}

pub async fn run(args: SnapshotArgs, manager: &SandboxManager) -> Result<()> {
    match args.action {
        SnapshotAction::Save {
            snapshot: name,
            boxarg,
        } => {
            let sandbox_name = manager.resolve_name(boxarg.name())?;
            if !is_safe_name(&name) {
                bail!(
                    "Snapshot name must be 1-64 characters, not a path component, and free of control characters."
                );
            }
            let (_claim, state) = manager
                .claim_and_read(&sandbox_name)
                .context("cannot save a snapshot while another lifecycle operation is running")?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            println!("Creating snapshot '{name}' for sandbox '{sandbox_name}'...");
            runtime.snapshot_create(&sandbox_name, &name).await?;
            manager
                .save_snapshot_metadata(&sandbox_name, &name, &state)
                .with_context(|| {
                    format!(
                        "snapshot '{name}' was created, but its selection metadata could not be recorded"
                    )
                })?;
            println!("Snapshot '{name}' created.");
            Ok(())
        }
        SnapshotAction::Restore {
            snapshot: name,
            boxarg,
        } => {
            let sandbox_name = manager.resolve_name(boxarg.name())?;
            let (claim, state) = manager.claim_and_read(&sandbox_name).context(
                "cannot restore a snapshot while another lifecycle operation is running",
            )?;
            let runtime = manager.runtime_for_sandbox(&state)?;
            let snapshot_state = manager.load_snapshot_metadata(&sandbox_name, &name)?;
            if snapshot_state.name != sandbox_name || snapshot_state.runtime != state.runtime {
                bail!(
                    "snapshot '{name}' metadata belongs to a different sandbox or runtime; refusing to restore it"
                );
            }

            let status = runtime.status(&sandbox_name).await?;
            let resume = match status {
                SandboxStatus::Running | SandboxStatus::Unreachable(_) => {
                    runtime.stop(&sandbox_name).await.with_context(|| {
                        format!("stop sandbox '{sandbox_name}' before snapshot restore")
                    })?;
                    true
                }
                SandboxStatus::Stopped => false,
                SandboxStatus::NotFound => {
                    bail!("Sandbox '{sandbox_name}' no longer exists in its runtime")
                }
                SandboxStatus::Unknown(status) => bail!(
                    "Sandbox '{sandbox_name}' is in unknown state ({status}); refusing to restore a snapshot"
                ),
            };

            println!("Restoring snapshot '{name}' for sandbox '{sandbox_name}'...");
            if let Err(restore_error) = runtime.snapshot_restore(&sandbox_name, &name).await {
                return Err(stop_after_restore_failure(
                    runtime.as_ref(),
                    &sandbox_name,
                    restore_error,
                )
                .await);
            }

            // Every failure after runtime restore is funnelled through one
            // fail-closed path. A restore can roll back the guest firewall, so
            // the box must never remain powered on unless strict policy
            // reconciliation has completed.
            let reconciliation: Result<()> = async {
                // Start even when the box was originally stopped: restoring
                // the runtime disk can roll back or remove its firewall.
                match runtime.status(&sandbox_name).await? {
                    SandboxStatus::Running => {}
                    SandboxStatus::Stopped => runtime
                        .start(&sandbox_name)
                        .await
                        .context("start restored sandbox for reconciliation")?,
                    SandboxStatus::Unreachable(_) => {
                        runtime
                            .stop(&sandbox_name)
                            .await
                            .context("recover restored sandbox before reconciliation")?;
                        runtime
                            .start(&sandbox_name)
                            .await
                            .context("start restored sandbox for reconciliation")?;
                    }
                    SandboxStatus::NotFound => {
                        bail!("snapshot restore removed sandbox '{sandbox_name}' from its runtime")
                    }
                    SandboxStatus::Unknown(status) => bail!(
                        "snapshot restored, but sandbox '{sandbox_name}' is in unknown state ({status})"
                    ),
                }

                let persistence = (|| -> Result<()> {
                    let _project_claim =
                        crate::web::build::claim_project(&manager.state_dir, &state.project_dir)?;
                    let base =
                        crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
                            .context("read devbox.toml before reconciling the restored selection")?;
                    let selection = crate::nix::compose::Selection::from_state(&snapshot_state);
                    let restored_config = selection.to_config(&base);
                    let mut restored_state = state.clone();
                    restored_state.sets = snapshot_state.sets.clone();
                    restored_state.languages = snapshot_state.languages.clone();
                    restored_state.image = snapshot_state.image.clone();
                    restored_state.packages = snapshot_state.packages.clone();
                    restored_state.package_sources = snapshot_state.package_sources.clone();
                    manager
                        .save_config_and_state(&restored_config, &restored_state)
                        .context("record the restored selection in devbox.toml and sandbox state")
                })();
                persistence.context(
                    "the runtime snapshot was restored, but devbox could not reconcile its saved selection",
                )?;

                crate::policy::enforce::restore_after_rebuild(manager, &sandbox_name, &claim).await
                    .context("snapshot restored, but the saved egress posture could not be re-applied")?;
                Ok(())
            }
            .await;
            if let Err(error) = reconciliation {
                return Err(
                    stop_after_restore_failure(runtime.as_ref(), &sandbox_name, error).await,
                );
            }

            if !resume {
                runtime
                    .stop(&sandbox_name)
                    .await
                    .context("return restored sandbox to its previous stopped state")?;
            }
            println!("Snapshot '{name}' restored.");
            Ok(())
        }
        SnapshotAction::List { boxarg } => {
            let sandbox_name = manager.resolve_name(boxarg.name())?;
            let (_claim, state) = manager
                .claim_and_read(&sandbox_name)
                .context("cannot list snapshots while another lifecycle operation is running")?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            let snapshots = runtime.snapshot_list(&sandbox_name).await?;

            if snapshots.is_empty() {
                println!("No snapshots for sandbox '{sandbox_name}'.");
                return Ok(());
            }

            println!("{:<30} {:<30}", "NAME", "CREATED");
            println!("{}", "-".repeat(60));
            for snap in &snapshots {
                println!("{:<30} {:<30}", snap.name, snap.created_at);
            }
            println!("\n{} snapshot(s)", snapshots.len());
            Ok(())
        }
    }
}
