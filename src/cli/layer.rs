use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;
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
    Diff {
        #[command(flatten)]
        boxarg: BoxArg,
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
}

impl LayerAction {
    /// The box this action names. Every variant carries one, so the shared
    /// preamble below can resolve it before dispatching.
    pub(crate) fn boxarg(&self) -> &BoxArg {
        match self {
            Self::Status { boxarg }
            | Self::Diff { boxarg }
            | Self::Commit { boxarg, .. }
            | Self::Discard { boxarg, .. }
            | Self::Refresh { boxarg }
            | Self::Conflicts { boxarg }
            | Self::Stash { boxarg }
            | Self::StashPop { boxarg } => boxarg,
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
        LayerAction::Diff { .. } => {
            let changes = overlay::diff(runtime.as_ref(), &name).await?;

            if changes.is_empty() {
                println!("No changes (overlay is clean).");
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
    }

    Ok(())
}
