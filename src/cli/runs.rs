//! `devbox runs` — what this box has been asked to do (§4.6).

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;

use crate::cli::box_arg::BoxArg;
use crate::obs::Store;
use crate::obs::collector::store_path;
use crate::obs::run::RunStatus;
use crate::report::model::human_duration;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct RunsArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Maximum runs to show
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Emit JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: RunsArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    let path = store_path(&manager.state_dir, &name);
    if !path.exists() {
        println!("Box '{name}' has no event store yet, so no runs.");
        return Ok(());
    }

    let store = Store::open(&path).context("failed to open the box's event store")?;
    let runs = store.list_runs(args.limit.clamp(1, 1000))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }

    if runs.is_empty() {
        println!("No runs recorded for '{name}' yet. Try `devbox run {name} -- <cmd>`.");
        return Ok(());
    }

    println!(
        "{:<26}  {:<6}  {:<9}  {:>8}  {:>4}  {}",
        "RUN".bold(),
        "KIND".bold(),
        "STATUS".bold(),
        "TIME".bold(),
        "EXIT".bold(),
        "COMMAND".bold()
    );
    for record in &runs {
        let status = match record.status_enum() {
            RunStatus::Running => record.status.yellow(),
            RunStatus::Finished if record.exit_code == Some(0) => record.status.green(),
            RunStatus::Finished => record.status.red(),
            RunStatus::Aborted => record.status.red(),
        };
        let exit = record
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into());
        let label = if record.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", record.label)
        };
        // One line per run, so a long command is truncated rather than allowed
        // to wrap the table into unreadability. The report has the whole of it.
        let command = truncate(&format!("{label}{}", record.command_line()), 60);
        println!(
            "{:<26}  {:<6}  {:<9}  {:>8}  {:>4}  {}",
            record.run_id,
            record.kind,
            status,
            human_duration(record.duration_ms()),
            exit,
            command,
        );
    }
    Ok(())
}

/// Truncate on a character boundary, because a command line can hold anything.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncation_does_not_split_a_character() {
        // A command line is guest-influenced text; slicing it by byte index
        // panics the moment someone runs `echo 日本語`.
        let value = "日本語".repeat(40);
        let cut = super::truncate(&value, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
        assert_eq!(super::truncate("short", 10), "short");
    }
}
