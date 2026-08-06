//! `devbox behavior` — the run summary and the behaviour diff (§7.6).
//!
//! `devbox diff` says what files changed. This says what the box *did*: the
//! domains it contacted, the processes it ran, the files it wrote, and — with
//! `diff` — what it did this time that it did not do last time.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::obs::behavior;
use crate::obs::store::{Query, Store};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct BehaviorArgs {
    #[command(subcommand)]
    pub command: BehaviorCommand,
}

#[derive(Subcommand, Debug)]
pub enum BehaviorCommand {
    /// Summarize what a box has done
    Summary(SummaryArgs),

    /// Compare two windows of a box's activity
    Diff(DiffArgs),
}

#[derive(Args, Debug)]
pub struct SummaryArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Only events at or after this RFC 3339 timestamp
    #[arg(long)]
    pub since: Option<String>,

    /// Emit JSON instead of Markdown
    #[arg(long)]
    pub json: bool,

    /// Emit the raw events as JSON Lines
    #[arg(long)]
    pub jsonl: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Start of the earlier window (RFC 3339)
    #[arg(long)]
    pub from: String,

    /// Boundary between the two windows (RFC 3339). Defaults to now.
    #[arg(long)]
    pub at: Option<String>,

    /// Emit JSON instead of Markdown
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: BehaviorArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        BehaviorCommand::Summary(a) => summary(a, manager),
        BehaviorCommand::Diff(a) => diff(a, manager),
    }
}

/// Open a box's store, with a message rather than an error when there is none.
fn open(manager: &SandboxManager, name: &str) -> Result<Option<Store>> {
    let path = crate::obs::collector::store_path(&manager.state_dir, name);
    if !path.exists() {
        println!("No events recorded for box '{name}' yet.");
        println!("Observability writes to {}.", path.display());
        return Ok(None);
    }
    Store::open(&path).map(Some)
}

fn summary(args: SummaryArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let Some(store) = open(manager, &name)? else {
        return Ok(());
    };

    let events = store
        .query(&Query {
            since: args.since.clone(),
            limit: Some(Query::MAX_LIMIT),
            ..Default::default()
        })
        .context("failed to query the event store")?;

    if args.jsonl {
        print!("{}", behavior::render_jsonl(&events)?);
        return Ok(());
    }

    let summary = behavior::summarize(&name, &events);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{}", behavior::render_markdown(&summary));
    }
    Ok(())
}

fn diff(args: DiffArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let Some(store) = open(manager, &name)? else {
        return Ok(());
    };

    let boundary = args.at.clone();

    let earlier = store.query(&Query {
        since: Some(args.from.clone()),
        until: boundary.clone(),
        limit: Some(Query::MAX_LIMIT),
        ..Default::default()
    })?;
    let later = store.query(&Query {
        since: boundary,
        limit: Some(Query::MAX_LIMIT),
        ..Default::default()
    })?;

    let before = behavior::summarize(&name, &earlier);
    let after = behavior::summarize(&name, &later);
    let d = behavior::diff(&before, &after);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&d)?);
        return Ok(());
    }

    print!("{}", behavior::render_diff_markdown(&d));

    // A run that starts doing something new is the case worth flagging; a run
    // that merely does less is not.
    if d.has_new_behavior() {
        eprintln!("\nThis run did something the earlier one did not.");
    }
    Ok(())
}
