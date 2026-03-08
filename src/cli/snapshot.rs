use anyhow::Result;
use clap::{Args, Subcommand};

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub action: SnapshotAction,
}

#[derive(Subcommand, Debug)]
pub enum SnapshotAction {
    /// Create a named snapshot
    Save {
        /// Snapshot name
        name: String,
        /// Sandbox name
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Restore a snapshot
    Restore {
        /// Snapshot name
        name: String,
        /// Sandbox name
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// List all snapshots
    List {
        /// Sandbox name
        #[arg(long)]
        sandbox: Option<String>,
    },
}

pub async fn run(args: SnapshotArgs, manager: &SandboxManager) -> Result<()> {
    match args.action {
        SnapshotAction::Save { name, sandbox } => {
            let sandbox_name = manager.resolve_name(sandbox.as_deref())?;
            if !manager.sandbox_exists(&sandbox_name) {
                anyhow::bail!("Sandbox '{}' not found.", sandbox_name);
            }
            let state = manager.get_sandbox(&sandbox_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            println!("Creating snapshot '{name}' for sandbox '{sandbox_name}'...");
            runtime.snapshot_create(&sandbox_name, &name).await?;
            println!("Snapshot '{name}' created.");
            Ok(())
        }
        SnapshotAction::Restore { name, sandbox } => {
            let sandbox_name = manager.resolve_name(sandbox.as_deref())?;
            if !manager.sandbox_exists(&sandbox_name) {
                anyhow::bail!("Sandbox '{}' not found.", sandbox_name);
            }
            let state = manager.get_sandbox(&sandbox_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            println!("Restoring snapshot '{name}' for sandbox '{sandbox_name}'...");
            runtime.snapshot_restore(&sandbox_name, &name).await?;
            println!("Snapshot '{name}' restored.");
            Ok(())
        }
        SnapshotAction::List { sandbox } => {
            let sandbox_name = manager.resolve_name(sandbox.as_deref())?;
            if !manager.sandbox_exists(&sandbox_name) {
                anyhow::bail!("Sandbox '{}' not found.", sandbox_name);
            }
            let state = manager.get_sandbox(&sandbox_name)?;
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
