use anyhow::Result;
use clap::{Args, Subcommand};

use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub action: LayerAction,

    /// Sandbox name
    #[arg(long, global = true)]
    pub name: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum LayerAction {
    /// Show overlay status (like git status)
    Status,
    /// Show file diffs in overlay
    Diff,
    /// Sync overlay changes to host
    Commit {
        /// Only sync specific paths
        #[arg(long)]
        path: Option<Vec<String>>,
        /// Preview what would be synced
        #[arg(long)]
        dry_run: bool,
    },
    /// Discard overlay changes
    Discard {
        /// Only discard specific paths
        #[arg(long)]
        path: Option<Vec<String>>,
    },
    /// Stash overlay changes
    Stash,
    /// Restore stashed changes
    #[command(name = "stash-pop")]
    StashPop,
}

pub async fn run(args: LayerArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

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
        LayerAction::Status => {
            overlay::status(runtime.as_ref(), &name).await?;
        }
        LayerAction::Diff => {
            let changes = overlay::diff(runtime.as_ref(), &name).await?;

            if changes.is_empty() {
                println!("No changes (overlay is clean).");
                return Ok(());
            }

            let added = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Added).count();
            let modified = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Modified).count();
            let deleted = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Deleted).count();

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
        LayerAction::Commit { path, dry_run } => {
            let paths = path.as_deref();
            overlay::commit(runtime.as_ref(), &name, paths, dry_run).await?;
        }
        LayerAction::Discard { path } => {
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
        LayerAction::Stash => {
            overlay::stash(runtime.as_ref(), &name).await?;
        }
        LayerAction::StashPop => {
            overlay::stash_pop(runtime.as_ref(), &name).await?;
        }
    }

    Ok(())
}
