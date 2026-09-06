use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::cli::watch::human_bytes;
use crate::obs::Store;
use crate::sandbox::SandboxManager;
use crate::sandbox::checkpoint::{self, CheckpointId, Target};
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub action: LayerAction,
}

/// The box is named once per action (`devbox layer status [NAME]`), not once
/// for `layer` as a whole.
///
/// It used to be a `global = true` flag on `LayerArgs`, which is the only way
/// clap can share a *flag* across subcommands — but a global cannot be a
/// positional, and the positional is the shape every other command now has.
#[derive(Subcommand, Debug)]
pub enum LayerAction {
    /// Show overlay status (like git status)
    Status {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Show file diffs in overlay
    ///
    /// With no `--from` this is the live upper against the host. With
    /// `--from <id>` it is that checkpoint against the box now, or against a
    /// second checkpoint with `--to <id>`.
    Diff {
        #[command(flatten)]
        boxarg: BoxArg,
        /// Compare from this checkpoint instead of from the host files
        #[arg(long, value_name = "ID")]
        from: Option<String>,
        /// Compare against this checkpoint instead of against the box now
        #[arg(long, value_name = "ID", requires = "from")]
        to: Option<String>,
    },
    /// Sync overlay changes to host
    Commit {
        #[command(flatten)]
        boxarg: BoxArg,
        /// Only sync specific paths
        #[arg(long)]
        path: Option<Vec<String>>,
        /// Preview what would be synced
        #[arg(long)]
        dry_run: bool,
    },
    /// Discard overlay changes
    Discard {
        #[command(flatten)]
        boxarg: BoxArg,
        /// Only discard specific paths
        #[arg(long)]
        path: Option<Vec<String>>,
    },
    /// Refresh overlay (pick up host-side changes)
    Refresh {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Show files modified on both sides (potential conflicts)
    Conflicts {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Stash overlay changes
    Stash {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Restore stashed changes
    #[command(name = "stash-pop")]
    StashPop {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Save the overlay upper layer as a checkpoint
    Checkpoint {
        #[command(flatten)]
        boxarg: BoxArg,
        /// A name to remember this checkpoint by
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
    },
    /// List saved checkpoints
    Checkpoints {
        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Put the overlay back to a checkpoint
    ///
    /// The id comes first because clap cannot place a required positional
    /// after an optional one — the same reason `devbox snapshot restore
    /// <SNAPSHOT> [NAME]` reads the way it does.
    Restore {
        /// The checkpoint to restore; a unique prefix is enough
        #[arg(value_name = "ID")]
        id: String,

        #[command(flatten)]
        boxarg: BoxArg,
    },
    /// Delete a checkpoint
    ///
    /// The id comes first for the same reason it does on `restore`.
    #[command(name = "checkpoint-rm")]
    CheckpointRm {
        /// The checkpoint to delete; a unique prefix is enough
        #[arg(value_name = "ID")]
        id: String,

        #[command(flatten)]
        boxarg: BoxArg,

        /// Delete it even though a run's report is built on it
        #[arg(long)]
        force: bool,
    },
    /// Delete old checkpoints
    ///
    /// By default this only touches checkpoints nobody has claimed: the ones a
    /// run pinned are its report's evidence and are kept. `--runs-older-than`
    /// lets those go too, once the run has ended, ended long enough ago, and
    /// its report has actually been written.
    Prune {
        #[command(flatten)]
        boxarg: BoxArg,

        /// How many unclaimed checkpoints to keep
        #[arg(long, value_name = "N", default_value_t = checkpoint::DEFAULT_KEEP)]
        keep: usize,

        /// Also drop the checkpoints of runs that ended longer ago than this
        /// and whose report has been written (7d, 24h, 90m, 3600s)
        #[arg(long, value_name = "AGE")]
        runs_older_than: Option<String>,

        /// Say what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
}

impl LayerAction {
    /// The box this action names. Every variant carries one, so the shared
    /// preamble below can resolve it before dispatching.
    pub fn boxarg(&self) -> &BoxArg {
        match self {
            Self::Status { boxarg }
            | Self::Diff { boxarg, .. }
            | Self::Commit { boxarg, .. }
            | Self::Discard { boxarg, .. }
            | Self::Refresh { boxarg }
            | Self::Conflicts { boxarg }
            | Self::Stash { boxarg }
            | Self::StashPop { boxarg }
            | Self::Checkpoint { boxarg, .. }
            | Self::Checkpoints { boxarg }
            | Self::Restore { boxarg, .. }
            | Self::CheckpointRm { boxarg, .. }
            | Self::Prune { boxarg, .. } => boxarg,
        }
    }
}

pub async fn run(args: LayerArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.action.boxarg().name())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;

    if state.mount_mode == "writable" {
        println!(
            "Sandbox '{}' uses writable mode — changes go directly to host. No overlay layer active.",
            name,
        );
        return Ok(());
    }

    let runtime = manager.runtime_for_sandbox(&state)?;

    match args.action {
        LayerAction::Status { .. } => {
            overlay::status(runtime.as_ref(), &name).await?;
        }
        LayerAction::Diff { from, to, .. } => {
            let (changes, empty_message) = match from {
                None => (
                    overlay::diff(runtime.as_ref(), &name).await?,
                    "No changes (overlay is clean).".to_string(),
                ),
                Some(from) => {
                    let from = CheckpointId::parse(&from)?;
                    let target = match to {
                        Some(to) => Target::Checkpoint(CheckpointId::parse(&to)?),
                        None => Target::Live,
                    };
                    let against = match &target {
                        Target::Live => "the box now".to_string(),
                        Target::Checkpoint(id) => format!("checkpoint {id}"),
                    };
                    (
                        checkpoint::diff(runtime.as_ref(), &name, &from, target).await?,
                        format!("No changes between checkpoint {from} and {against}."),
                    )
                }
            };

            if changes.is_empty() {
                println!("{empty_message}");
                return Ok(());
            }

            let added = changes
                .iter()
                .filter(|c| c.status == overlay::ChangeStatus::Added)
                .count();
            let modified = changes
                .iter()
                .filter(|c| c.status == overlay::ChangeStatus::Modified)
                .count();
            let deleted = changes
                .iter()
                .filter(|c| c.status == overlay::ChangeStatus::Deleted)
                .count();

            for c in &changes {
                if !c.is_dir {
                    println!("  {} {}", c.status.symbol(), c.path);
                }
            }

            println!(
                "\n{} file(s): {} added, {} modified, {} deleted",
                changes.iter().filter(|c| !c.is_dir).count(),
                added,
                modified,
                deleted,
            );
        }
        LayerAction::Commit { path, dry_run, .. } => {
            let paths = path.as_deref();
            overlay::commit(runtime.as_ref(), &name, paths, dry_run).await?;
        }
        LayerAction::Discard { path, .. } => {
            if path.is_none() {
                println!("This will discard ALL overlay changes in sandbox '{name}'.");
                print!("Continue? [y/N] ");
                use std::io::Write;
                std::io::stdout().flush()?;

                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let paths = path.as_deref();
            overlay::discard(runtime.as_ref(), &name, paths).await?;
        }
        LayerAction::Refresh { .. } => {
            overlay::refresh(runtime.as_ref(), &name).await?;
        }
        LayerAction::Conflicts { .. } => {
            overlay::conflicts(runtime.as_ref(), &name).await?;
        }
        LayerAction::Stash { .. } => {
            overlay::stash(runtime.as_ref(), &name).await?;
        }
        LayerAction::StashPop { .. } => {
            overlay::stash_pop(runtime.as_ref(), &name).await?;
        }
        LayerAction::Checkpoint { label, .. } => {
            let saved = checkpoint::create(runtime.as_ref(), &name, label.as_deref()).await?;
            println!(
                "Checkpoint {} saved — {} file(s), {}{}",
                saved.id,
                saved.files,
                human_bytes(saved.bytes),
                saved
                    .label
                    .as_deref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default(),
            );
        }
        LayerAction::Checkpoints { .. } => {
            let saved = checkpoint::list(runtime.as_ref(), &name).await?;
            if saved.is_empty() {
                println!(
                    "No checkpoints on '{name}'. Take one with `devbox layer checkpoint {name}`."
                );
                return Ok(());
            }
            println!(
                "{:<16} {:<22} {:>6} {:>9}  LABEL",
                "ID", "CREATED", "FILES", "BYTES"
            );
            for c in &saved {
                println!(
                    "{:<16} {:<22} {:>6} {:>9}  {}",
                    c.id,
                    c.created_at,
                    c.files,
                    human_bytes(c.bytes),
                    c.label.as_deref().unwrap_or("-"),
                );
            }
        }
        LayerAction::Restore { id, .. } => {
            let id = CheckpointId::parse(&id)?;
            // The store is how `restore` finds out whether a run is still
            // going. A box with no store has never been observed, which is the
            // same answer as "no live runs" — so an absent database is not a
            // reason to refuse the restore.
            let store = Store::open(&crate::obs::collector::store_path(
                &manager.state_dir,
                &name,
            ))?;
            checkpoint::restore(runtime.as_ref(), &store, &name, &id).await?;
        }
        LayerAction::CheckpointRm { id, force, .. } => {
            let typed = CheckpointId::parse(&id)?;
            // Listed once here rather than left to `delete`: the guard needs
            // the manifest, not just the id, and resolving a prefix twice
            // could in principle land on two different checkpoints.
            let known = checkpoint::list(runtime.as_ref(), &name).await?;
            let resolved = checkpoint::resolve_id(&known, &typed)?;
            let record = known
                .iter()
                .find(|c| c.id == resolved)
                .expect("resolve_id returns an id from the list it was given");

            if !force {
                checkpoint::refuse_pinned_delete(&name, record)?;
            }

            checkpoint::delete(runtime.as_ref(), &name, &resolved).await?;
            println!(
                "Deleted checkpoint {resolved}{}.",
                record
                    .label
                    .as_deref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default(),
            );
        }

        LayerAction::Prune {
            keep,
            runs_older_than,
            dry_run,
            ..
        } => {
            prune(
                manager,
                runtime.as_ref(),
                &name,
                keep,
                runs_older_than.as_deref(),
                dry_run,
            )
            .await?;
        }
    }

    Ok(())
}

/// `devbox layer prune`.
///
/// Two passes, because they answer different questions. The unclaimed
/// checkpoints go by count — keep the last `keep`, drop the rest — and the
/// ones a run pinned go by age, and only once that run's report exists as a
/// file. A run's report links to its start and end trees; until the report is
/// written, deleting them destroys the only thing that could have produced it.
async fn prune(
    manager: &SandboxManager,
    runtime: &dyn crate::runtime::Runtime,
    name: &str,
    keep: usize,
    runs_older_than: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let checkpoints = checkpoint::list(runtime, name).await?;
    let unclaimed = checkpoint::prune_plan(&checkpoints, keep);

    // Read the runs before deleting anything, so a failure part-way leaves the
    // store agreeing with the guest rather than ahead of it.
    let expired = match runs_older_than {
        None => Vec::new(),
        Some(age) => {
            let span = checkpoint::parse_age(age)?;
            let cutoff =
                (chrono::Utc::now() - span).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let store = Store::open(&crate::obs::collector::store_path(&manager.state_dir, name))?;
            let pins = run_pins(manager, &store, name)?;
            checkpoint::expired_run_pins(&pins, &cutoff)
        }
    };

    if unclaimed.is_empty() && expired.is_empty() {
        println!(
            "Nothing to prune: {} checkpoints, all claimed or within the keep budget.",
            checkpoints.len()
        );
        return Ok(());
    }

    if dry_run {
        for id in &unclaimed {
            println!("would delete {id} (unclaimed)");
        }
        for pin in &expired {
            for id in [pin.start.as_ref(), pin.end.as_ref()].into_iter().flatten() {
                println!("would delete {id} (run {}, report written)", pin.run_id);
            }
        }
        return Ok(());
    }

    let dropped = checkpoint::prune(runtime, name, keep).await?;
    for id in &dropped {
        println!("Deleted checkpoint {id}.");
    }

    if !expired.is_empty() {
        let store = Store::open(&crate::obs::collector::store_path(&manager.state_dir, name))?;
        let released = checkpoint::drop_run_pins(runtime, name, &expired).await?;
        for pin in &expired {
            // The run stays in `devbox runs`; only its claim on two trees is
            // released, and the row must stop naming them.
            store.forget_run_checkpoints(&pin.run_id)?;
        }
        for id in &released {
            println!("Deleted checkpoint {id} (run evidence, report kept).");
        }
        println!(
            "Released {} run{} holding {} checkpoint{}.",
            expired.len(),
            if expired.len() == 1 { "" } else { "s" },
            released.len(),
            if released.len() == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

/// Every run's claim on its checkpoints, with whether its report exists.
fn run_pins(
    manager: &SandboxManager,
    store: &Store,
    name: &str,
) -> Result<Vec<checkpoint::RunPin>> {
    // Every run, not a page of them: this is the one caller that has to see
    // the old ones, because old is exactly what it is looking for.
    let runs = store.list_runs(usize::MAX)?;
    let mut pins = Vec::new();
    for run in runs {
        let reported = crate::report::RunReport::directory(&manager.state_dir, name, &run.run_id)
            .join("report.json")
            .exists();
        pins.push(checkpoint::RunPin {
            run_id: run.run_id.clone(),
            ended_at: run.ended_at.clone(),
            start: run
                .checkpoint_start
                .as_deref()
                .and_then(|id| CheckpointId::parse(id).ok()),
            end: run
                .checkpoint_end
                .as_deref()
                .and_then(|id| CheckpointId::parse(id).ok()),
            reported,
        });
    }
    Ok(pins)
}
