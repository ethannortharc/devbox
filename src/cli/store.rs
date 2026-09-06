//! `devbox store` — maintenance on a box's event database.
//!
//! Everything here rewrites or reports on the audit store itself, which is why
//! it is a command rather than something that happens on the way past. The
//! store is append-only by design; a subcommand that changes it is a decision
//! someone makes, not a side effect of an upgrade.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::obs::Store;
use crate::obs::collector::store_path;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommand,
}

#[derive(Subcommand, Debug)]
pub enum StoreCommand {
    /// Remove credentials from the argv of events recorded before redaction
    Redact(RedactArgs),
}

#[derive(Args, Debug)]
pub struct RedactArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Report what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: StoreArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        StoreCommand::Redact(args) => redact(args, manager),
    }
}

/// Rewrite stored argvs that predate redaction.
///
/// Everything devbox *renders* has been redacted since 0.2.0, on three
/// separate paths — so this changes nothing about what leaves the machine. It
/// changes what is on the host's disk, which matters for a database somebody
/// wants to hand over, archive, or simply not keep.
fn redact(args: RedactArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Box '{name}' not found.");
    }
    let path = store_path(&manager.state_dir, &name);
    if !path.exists() {
        println!("Box '{name}' has no event store, so there is nothing to redact.");
        return Ok(());
    }

    let mut store = Store::open(&path).context("failed to open the box's event store")?;
    let sweep = store
        .redact_stored_argv(args.dry_run)
        .context("failed to sweep the store for stored credentials")?;

    println!(
        "Scanned {} exec event(s) in '{name}'; {} hold a credential in their argv.",
        sweep.scanned, sweep.matched
    );
    if args.dry_run {
        if sweep.matched > 0 {
            println!("Nothing was written. Re-run without --dry-run to rewrite them.");
        }
        return Ok(());
    }
    if sweep.matched == 0 {
        return Ok(());
    }
    println!("Rewrote {}.", sweep.rewritten);
    // The bytes are gone from the store; they may not be gone from the file.
    // SQLite reuses freed pages rather than shredding them, so the honest
    // claim is about what a reader gets back, not about what is on the disk.
    println!(
        "The rows now read as they render. SQLite reuses freed pages rather than\n\
         overwriting them, so `VACUUM` is what compacts the file afterwards."
    );
    Ok(())
}
