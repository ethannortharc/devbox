//! `devbox behavior` — the run summary and the behaviour diff (§7.6).
//!
//! `devbox diff` says what files changed. This says what the box *did*: the
//! domains it contacted, the processes it ran, the files it wrote, and — with
//! `diff` — what it did this time that it did not do last time.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

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

    /// Capture a real pcap for one network flow
    Pcap(PcapArgs),
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

#[derive(Args, Debug)]
pub struct PcapArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Transport protocol
    #[arg(long, value_parser = ["tcp", "udp"])]
    pub proto: String,

    /// Optional local/source address
    #[arg(long)]
    pub saddr: Option<IpAddr>,

    /// Optional local/source port
    #[arg(long)]
    pub sport: Option<u16>,

    /// Remote/destination address
    #[arg(long)]
    pub daddr: IpAddr,

    /// Remote/destination port
    #[arg(long)]
    pub dport: u16,

    /// Capture window in seconds
    #[arg(long, default_value_t = crate::obs::pcap::DEFAULT_DURATION.as_secs())]
    pub seconds: u64,

    /// Maximum number of packets
    #[arg(long, default_value_t = crate::obs::pcap::DEFAULT_PACKETS)]
    pub packets: u16,

    /// Output path (default: <box>-flow.pcap)
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub async fn run(args: BehaviorArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        BehaviorCommand::Summary(a) => summary(a, manager),
        BehaviorCommand::Diff(a) => diff(a, manager),
        BehaviorCommand::Pcap(a) => pcap(a, manager).await,
    }
}

async fn pcap(args: PcapArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let filter = crate::obs::pcap::FlowFilter {
        proto: args.proto,
        saddr: args.saddr,
        sport: args.sport.filter(|port| *port != 0),
        daddr: args.daddr,
        dport: args.dport,
        duration: Duration::from_secs(args.seconds),
        packets: args.packets,
    };
    let capture = crate::obs::pcap::capture(manager, &name, &filter).await?;
    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{name}-flow.pcap")));
    std::fs::write(&output, &capture.bytes)
        .with_context(|| format!("write flow capture to {}", output.display()))?;
    println!(
        "Captured {} packet(s) to {}",
        capture.packets,
        output.display()
    );
    Ok(())
}

/// Largest window one behaviour command will read out of a store.
///
/// The row ceiling is not a size: at the transport's frame limit, fifty
/// thousand rows is fifty gigabytes. Generous for a real audit trail, and
/// finite — and when it binds, the command says so rather than pretending the
/// window ended there.
const MAX_SCAN_BYTES: usize = 256 * 1024 * 1024;

/// Open a box's store, with a message rather than an error when there is none.
fn open(manager: &SandboxManager, name: &str) -> Result<Option<Store>> {
    // `store_path` only joins, and this name can come from an explicit flag.
    if !crate::sandbox::state::is_safe_name(name) {
        anyhow::bail!("{name:?} is not a box name");
    }
    let path = crate::obs::collector::store_path(&manager.state_dir, name);
    if !path.exists() {
        println!("No collected events are available for box '{name}'.");
        println!("Start the box with a devbox command to collect its timeline.");
        println!("The collector store is {}.", path.display());
        return Ok(None);
    }
    let store = Store::open(&path)?;
    if store.count()? == 0 {
        println!("No collected events are available for box '{name}'.");
        println!("Start the box with a devbox command to collect its timeline.");
        println!("The collector store is {}.", path.display());
        return Ok(None);
    }
    Ok(Some(store))
}

fn summary(args: SummaryArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let Some(store) = open(manager, &name)? else {
        return Ok(());
    };

    // `export` rather than `query`: it is bounded by bytes as well as rows,
    // and it reports truncation from what the scan *reached* rather than from
    // how many rows happened to decode. Counting decoded rows meant one row
    // written by an older schema turned a cut-off window into a short one that
    // claimed to be complete.
    let (events, truncated) = store
        .export(args.since.as_deref(), Query::MAX_LIMIT, MAX_SCAN_BYTES)
        .context("failed to query the event store")?;

    // A summary that silently covers only part of a window is worse than one
    // that says so: it reports "no violations" for a run that had them.
    if truncated {
        eprintln!(
            "warning: the scan stopped before the end of this window. The summary \
             below covers the oldest events it reached — narrow it with --since."
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

    // Bounded and honest on both sides, for the reason spelled out below:
    // a window this command believes is complete when it is not produces the
    // one answer it must never get wrong.
    let (earlier, earlier_cut) =
        store.export(Some(&args.from), Query::MAX_LIMIT, MAX_SCAN_BYTES)?;
    let earlier: Vec<_> = match boundary.as_deref() {
        Some(at) => earlier
            .into_iter()
            .filter(|event| event.ts_wall.as_str() < at)
            .collect(),
        None => earlier,
    };
    let (later, later_cut) = store.export(boundary.as_deref(), Query::MAX_LIMIT, MAX_SCAN_BYTES)?;

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
    for (label, cut) in [("--from", earlier_cut), ("the later window", later_cut)] {
        if cut {
            bail!(
                "the scan of {label} stopped before the end of it — so anything \
                 after that point would be reported as absent, and a diff that \
                 says 'nothing new' when there is would be worse than none.\n\n  \
                 Narrow it with `--from` and `--at`, or summarize the halves \
                 separately with `devbox behavior summary --since`."
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
