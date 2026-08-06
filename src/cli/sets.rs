//! `devbox sets` — the CLI half of the set checklist.
//!
//! §6.4 requires parity: every console action has a CLI equivalent, because
//! headless and scripted use must not need a browser. This is the counterpart
//! of the console's Sets tab.

use std::collections::BTreeSet;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::nix::compose::{LOCKED_SETS, Selection, describe_change};
use crate::nix::sets::NIX_SETS;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct SetsArgs {
    #[command(subcommand)]
    pub command: SetsCommand,
}

#[derive(Subcommand, Debug)]
pub enum SetsCommand {
    /// Show which sets a box has, and what else is available
    List(ListArgs),

    /// Replace a box's selection and rebuild it
    Apply(ApplyArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// Sandbox name (default: current directory's sandbox)
    #[arg(long)]
    pub name: Option<String>,

    /// Set to enable; repeat or comma-separate. Anything not listed is disabled.
    #[arg(long = "set", value_delimiter = ',')]
    pub sets: Vec<String>,

    /// Extra nixpkgs attribute path; repeat or comma-separate
    #[arg(long = "package", value_delimiter = ',')]
    pub packages: Vec<String>,

    /// Print what would change without rebuilding
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: SetsArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        SetsCommand::List(a) => list(a, manager),
        SetsCommand::Apply(a) => apply(a, manager).await,
    }
}

fn list(args: ListArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let state = manager.get_sandbox(&name)?;
    let current = Selection::from_state(&state);

    println!("Sets for '{name}':\n");
    for set in NIX_SETS {
        let mark = if current.sets.contains(set.name) {
            "\u{2713}"
        } else {
            " "
        };
        let locked = if LOCKED_SETS.contains(&set.name) {
            "  (always on)"
        } else {
            ""
        };
        println!(
            "  [{mark}] {:<14} {:>3} packages{locked}",
            set.name,
            set.packages.len()
        );
    }
    println!(
        "\n{} of {} sets enabled, {} packages total.",
        current.sets.len(),
        NIX_SETS.len(),
        current.resolved_packages().len()
    );
    println!(
        "\nApply a different selection with:\n  devbox sets apply --set system --set git --set lang-go"
    );
    Ok(())
}

async fn apply(args: ApplyArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let state = manager.get_sandbox(&name)?;

    let before = Selection::from_state(&state);
    let after = Selection::new(args.sets.clone(), args.packages.clone());
    after.validate()?;

    println!("Selection change: {}", describe_change(&before, &after));

    let added = added_packages(&before, &after);
    let removed = removed_packages(&before, &after);
    if !added.is_empty() {
        println!("  + {} package(s): {}", added.len(), preview(&added));
    }
    if !removed.is_empty() {
        println!("  - {} package(s): {}", removed.len(), preview(&removed));
    }

    if args.dry_run {
        println!("\nDry run — nothing was changed.");
        return Ok(());
    }

    let runtime = manager.runtime_for_sandbox(&state)?;
    crate::nix::write_set_modules(runtime.as_ref(), &name, &after).await?;
    crate::nix::rebuild::nixos_rebuild(runtime.as_ref(), &name).await?;

    let config = after.to_config(&crate::sandbox::config::DevboxConfig::load_or_default(
        &state.project_dir,
    ));
    let mut state = state;
    state.sets = config.active_sets();
    state.languages = config.active_languages();
    state.save(&manager.state_dir)?;

    println!("Box '{name}' rebuilt with the new selection.");
    Ok(())
}

/// Packages present after but not before.
pub fn added_packages(before: &Selection, after: &Selection) -> Vec<String> {
    let old: BTreeSet<String> = before.resolved_packages().into_iter().collect();
    after
        .resolved_packages()
        .into_iter()
        .filter(|p| !old.contains(p))
        .collect()
}

/// Packages present before but not after.
pub fn removed_packages(before: &Selection, after: &Selection) -> Vec<String> {
    let new: BTreeSet<String> = after.resolved_packages().into_iter().collect();
    before
        .resolved_packages()
        .into_iter()
        .filter(|p| !new.contains(p))
        .collect()
}

/// First few entries of a list, with a count of the rest.
fn preview(items: &[String]) -> String {
    const SHOWN: usize = 6;
    if items.len() <= SHOWN {
        return items.join(", ");
    }
    format!(
        "{}, … and {} more",
        items[..SHOWN].join(", "),
        items.len() - SHOWN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(names: &[&str]) -> Selection {
        Selection::new(names.iter().map(|s| s.to_string()), std::iter::empty())
    }

    #[test]
    fn diffs_the_package_closure_both_ways() {
        let before = sel(&["system", "network"]);
        let after = sel(&["system", "git"]);

        let added = added_packages(&before, &after);
        let removed = removed_packages(&before, &after);

        assert!(added.contains(&"lazygit".to_string()));
        assert!(removed.contains(&"nmap".to_string()));
        // Packages in both selections appear in neither list.
        assert!(!added.contains(&"coreutils".to_string()));
        assert!(!removed.contains(&"coreutils".to_string()));
    }

    #[test]
    fn an_unchanged_selection_diffs_to_nothing() {
        let s = sel(&["system", "git"]);
        assert!(added_packages(&s, &s).is_empty());
        assert!(removed_packages(&s, &s).is_empty());
    }

    #[test]
    fn preview_truncates_long_lists() {
        let few: Vec<String> = (0..3).map(|i| format!("p{i}")).collect();
        assert_eq!(preview(&few), "p0, p1, p2");

        let many: Vec<String> = (0..10).map(|i| format!("p{i}")).collect();
        let text = preview(&many);
        assert!(text.contains("and 4 more"), "got: {text}");
        assert!(!text.contains("p9"));
    }
}
