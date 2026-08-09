//! `devbox behavior` — the run summary and the behaviour diff (§7.6).
//!
//! `devbox diff` says what files changed. This says what the box *did*: the
//! domains it contacted, the processes it ran, the files it wrote, and — with
//! `diff` — what it did this time that it did not do last time.

use anyhow::{Context, Result, bail};
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

    // A summary that silently covers only part of a window is worse than one
    // that says so: it reports "no violations" for a run that had them.
    if events.len() >= Query::MAX_LIMIT {
        eprintln!(
            "warning: this window has at least {} events, which is the query ceiling. \
             The summary below covers the oldest {} only — narrow it with --since.",
            Query::MAX_LIMIT,
            Query::MAX_LIMIT
        );
    }

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

    // The boundary has to fall *between* two runs. Defaulting it to "now" put
    // every stored event in the earlier window and left the later one empty —
    // a diff that always reports "everything disappeared". There is no honest
    // default here, so ask for one.
    let Some(boundary) = args.at.clone() else {
        bail!(
            // `devbox behavior list` was never a command — `behavior` has only
            // `summary` and `diff` — so following this produced an
            // unknown-command error and still no timestamp. `watch` prints the
            // wall clock beside every event, which is where a boundary
            // actually comes from.
            "`devbox behavior diff` needs `--at <timestamp>`: the boundary between \
             the run you are comparing against and the run you are judging.\n\n  \
             Find one with `devbox watch` — it prints the timestamp of every \
             event, and the first event of a run is a good boundary — then:\n    \
             devbox behavior diff --at 2026-08-06T14:30:00Z"
        );
    };
    let boundary = Some(boundary);

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

    // A capped window is not a window.
    //
    // `MAX_LIMIT` rows back means the query stopped, not that the box stopped:
    // `diff` compares two summaries and reports what is in one and not the
    // other, so a domain, process, or violation past the cap is reported as
    // *absent*. That is the one answer this command must never give wrongly —
    // "this run did nothing new" is what someone acts on.
    //
    // Refused rather than warned. A diff nobody can trust is worth less than
    // no diff, and the fix is a narrower window, which the message names.
    for (label, window) in [("--from", &earlier), ("the later window", &later)] {
        if window.len() >= Query::MAX_LIMIT {
            bail!(
                "{label} holds at least {} events, which is where the query stops \
                 — so anything after that point would be reported as absent, and \
                 a diff that says 'nothing new' when there is would be worse than \
                 none.\n\n  \
                 Narrow it with `--from` and `--at`, or summarize the halves \
                 separately with `devbox behavior summary --since`.",
                Query::MAX_LIMIT
            );
        }
    }

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
