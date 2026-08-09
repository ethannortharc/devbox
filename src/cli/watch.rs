//! `devbox watch` — the CLI view of the observability plane.
//!
//! CLI ↔ web parity (§6.4): the console's Activity tab and this command are two
//! renderings of the same store and the same query API.

use anyhow::{Context, Result};
use clap::Args;

use crate::obs::correlate;
use crate::obs::event::EventType;
use crate::obs::store::{Query, Store};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Only these event types; repeat or comma-separate
    #[arg(long = "type", value_delimiter = ',')]
    pub types: Vec<String>,

    /// Only this process
    #[arg(long)]
    pub pid: Option<u32>,

    /// Substring match against the peer (domain, SNI, or address)
    #[arg(long)]
    pub peer: Option<String>,

    /// Substring match against the file path
    #[arg(long)]
    pub path: Option<String>,

    /// Only events at or after this RFC 3339 timestamp
    #[arg(long)]
    pub since: Option<String>,

    /// Maximum events to show
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Group by process instead of listing chronologically
    #[arg(long)]
    pub tree: bool,

    /// Emit JSON Lines instead of text
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: WatchArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let path = crate::obs::collector::store_path(&manager.state_dir, &name);

    if !path.exists() {
        println!("No events recorded for box '{name}' yet.");
        println!("The observability agent writes to {}.", path.display());
        return Ok(());
    }

    let kinds = args
        .types
        .iter()
        .map(|t| t.parse::<EventType>())
        .collect::<Result<Vec<_>>>()
        .context("unknown --type; valid values: exec, exit, connect, accept, dns, tls, file, syscall, api, policy")?;

    let store = Store::open(&path)?;

    // With `--peer`, *every* filter waits until after correlation.
    //
    // Not just the peer one. A connect event records the address it dialled
    // and the name lives in a separate DNS event, so `--type connect` or
    // `--path` in SQL discarded the very rows the enrichment needed —
    // `watch --type connect --peer pypi.org` matched nothing at all, because
    // the DNS answers that would have named those addresses were filtered out
    // before the map was built. Round 27 moved the peer filter out and left
    // its neighbours behind.
    //
    // The scan widens when the flag is used, so `--limit` still counts matches
    // shown rather than rows examined. That costs a bigger read on a filtered
    // query and nothing at all on an unfiltered one.
    let peer = args.peer.clone();
    let filtering_late = peer.is_some();
    let mut events = store.query(&Query {
        since: args.since.clone(),
        until: None,
        pid: args.pid,
        kinds: if filtering_late {
            Vec::new()
        } else {
            kinds.clone()
        },
        peer: None,
        path: if filtering_late {
            None
        } else {
            args.path.clone()
        },
        limit: Some(if filtering_late {
            Query::MAX_LIMIT
        } else {
            args.limit
        }),
        // Query newest-first so the limit keeps the *recent* events, then
        // present oldest-first so the output reads forwards.
        newest_first: true,
    })?;

    // Label addresses with the name that resolved them, so the output says
    // `pypi.org` rather than an address nobody recognizes (§7.3).
    let map = correlate::dns_map(&events);
    correlate::apply_dns_map(&mut events, &map);

    if let Some(peer) = &peer {
        let needle = peer.to_lowercase();
        let path = args.path.as_deref().map(str::to_lowercase);
        events.retain(|event| {
            matches_peer(event, &needle)
                && (kinds.is_empty() || kinds.contains(&event.kind))
                && path.as_deref().is_none_or(|want| {
                    event
                        .path()
                        .is_some_and(|p| p.to_lowercase().contains(want))
                })
        });
        // Still newest-first here, so this keeps the most recent matches.
        events.truncate(args.limit);
    }
    events.reverse();

    if args.json {
        for event in &events {
            println!("{}", serde_json::to_string(event)?);
        }
        return Ok(());
    }

    if events.is_empty() {
        println!("No events matched.");
        return Ok(());
    }

    if args.tree {
        print_tree(&events);
    } else {
        for event in &events {
            println!(
                "{}  {:<8} pid={:<6} {}",
                &event.ts_wall,
                event.kind.to_string(),
                event.pid,
                event.summary()
            );
        }
    }

    println!("\n{} event(s).", events.len());
    Ok(())
}

/// Does this event name the peer the user asked for?
///
/// Substring, case-insensitive, against every name an event can carry — the
/// same shape the SQL `LIKE` had, applied to the enriched row rather than the
/// stored one. `domain` is the field correlation fills in, and it is the whole
/// reason this cannot be done in the query.
fn matches_peer(event: &crate::obs::Event, needle: &str) -> bool {
    let Some(net) = event.net.as_ref() else {
        return false;
    };
    [&net.domain, &net.sni, &net.daddr, &net.qname]
        .iter()
        .any(|field| field.to_lowercase().contains(needle))
}

fn print_tree(events: &[crate::obs::Event]) {
    let chains = correlate::chains(events);
    for (i, depth) in correlate::tree(&chains) {
        let chain = &chains[i];
        let indent = "  ".repeat(depth);
        println!(
            "{indent}{} (pid {})  {}",
            chain.comm,
            chain.pid,
            chain.command.as_deref().unwrap_or("")
        );

        if !chain.peers.is_empty() {
            println!("{indent}  → {}", chain.peers.join(", "));
        }
        if !chain.files.is_empty() {
            let shown = chain.files.len().min(5);
            let more = chain.files.len() - shown;
            let suffix = if more > 0 {
                format!(" … and {more} more")
            } else {
                String::new()
            };
            println!("{indent}  ✎ {}{suffix}", chain.files[..shown].join(", "));
        }
        let (tx, rx) = chain.bytes();
        if tx > 0 || rx > 0 {
            println!("{indent}  ↑{} ↓{}", human_bytes(tx), human_bytes(rx));
        }
    }
}

/// Render a byte count the way a person reads it.
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1024 * 1024 * 1024, "GB"),
        (1024 * 1024, "MB"),
        (1024, "KB"),
    ];
    for (scale, unit) in UNITS {
        if n >= *scale {
            return format!("{:.1}{unit}", n as f64 / *scale as f64);
        }
    }
    format!("{n}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_read_naturally() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(999), "999B");
        assert_eq!(human_bytes(1024), "1.0KB");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(831_720), "812.2KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0GB");
    }
}
