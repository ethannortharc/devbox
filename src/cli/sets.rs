//! `devbox sets` — the CLI half of the set checklist.
//!
//! §6.4 requires parity: every console action has a CLI equivalent, because
//! headless and scripted use must not need a browser. This is the counterpart
//! of the console's Sets tab.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
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
    let project = crate::sandbox::config::DevboxConfig::load_or_default(&state.project_dir);
    let current = Selection::from_state_and_project(&state, &project);

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

    // Falls back to the project file for a box predating the state fields;
    // without it `before` shows no packages and the apply removes them all.
    let project = crate::sandbox::config::DevboxConfig::load_or_default(&state.project_dir);
    let before = Selection::from_state_and_project(&state, &project);
    // Sources come off the box, not off the command line: `--packages` names
    // what to have, and where an aliased package comes from is already
    // recorded. Without this the alias resolves to itself and the rebuild
    // fails on an undefined variable.
    //
    // From `before` rather than from state directly, because on a box that
    // predates the state fields those are empty — taking them raw would drop
    // the source one line after recovering it.
    let after = Selection::new(args.sets.clone(), args.packages.clone())
        .with_sources(before.sources.clone());
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

    // A NixOS-only path: an Ubuntu box has no `nixos-rebuild`, and running it
    // would fail with a much less helpful message than this one.
    if state.image != "nixos" {
        bail!(
            "box '{name}' uses the '{}' image, which has no nixos-rebuild. \
             Use `devbox nix add <pkg>` / `devbox nix remove <pkg>` there instead.",
            state.image
        );
    }

    let runtime = manager.runtime_for_sandbox(&state)?;
    // Both the file writes and the rebuild run *inside* the guest, so a stopped
    // box fails on the first exec. Every other live-box action starts it first.
    crate::web::service::ensure_running(manager, &name).await?;

    // Validate the project config *before* touching the box: discovering it is
    // malformed after the rebuild has switched the generation would leave the
    // guest on the new selection and the host unable to write down what
    // happened. The value is discarded — it is read again below, after the
    // rebuild, which is what this comment already claimed and the code did
    // not: `to_config` was projecting onto this minutes-old copy and writing
    // it back over anything the console had saved in between.
    crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)?;

    // Snapshot for the same reason the console path does: a failed rebuild
    // leaves the active generation alone but the generated *sources* already
    // replaced, so a later manual rebuild would apply a selection this command
    // reported as failed.
    let backup = crate::web::build::snapshot_generated(runtime.as_ref(), &name).await;

    // Every step past the snapshot rolls back, not only the rebuild. A write
    // that fails partway leaves some modules replaced and some not, and a
    // later manual rebuild would apply that half-selection — which is the same
    // hole the console path had, on the path that was fixed second.
    let applied = async {
        crate::nix::write_set_modules(runtime.as_ref(), &name, &after).await?;
        crate::nix::rebuild::nixos_rebuild(runtime.as_ref(), &name).await
    }
    .await;

    if let Err(e) = applied {
        if crate::web::build::restore_generated(runtime.as_ref(), &name, &backup).await {
            eprintln!("devbox: generated files restored to the last good selection");
        }
        // Before returning. A failed rebuild can still have restarted the
        // network stack — activation gets far enough to tear the old one down
        // and then fails — so the posture has to go back on whether the
        // rebuild worked or not. Returning the rebuild error first left the
        // box unrestricted on exactly the path where something already went
        // wrong.
        crate::policy::enforce::apply_saved(manager, &state, &name).await?;
        return Err(e);
    }

    // The firewall first, then the bookkeeping.
    //
    // A read-only devbox.toml returned through `?` and the posture was never
    // reattempted, so a successful rebuild left the box live and unrestricted
    // while reporting only a write error. The box is already running the new
    // configuration; getting its firewall back matters more than recording
    // what it is running.
    crate::policy::enforce::restore_after_rebuild(manager, &state, &name).await?;

    let base = crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
        .context("rebuilt the box, but its devbox.toml can no longer be read")?;
    let config = after.to_config(&base);
    let mut state = state;
    state.sets = config.active_sets();
    state.languages = config.active_languages();
    state.packages = after.packages.iter().cloned().collect();
    // And their sources, from the config this selection was composed against.
    // Keeping them on the box is what survives a later `devbox use`.
    state.package_sources = config
        .custom_packages
        .iter()
        .filter(|(_, source)| source.as_str() != "nixpkgs")
        .map(|(name, source)| (name.clone(), source.clone()))
        .collect();
    // devbox.toml first, then state — the order the console path uses, and for
    // the same reason: a failure here leaves the two agreeing on the old
    // selection, which the user can see and re-apply. The other order leaves
    // devbox reporting the new selection while the project file describes the
    // old one, and a later recreate silently reverts the box.
    config
        .save(&state.project_dir.join("devbox.toml"))
        .context("rebuilt the box, but could not record the selection in devbox.toml")?;
    state.save(&manager.state_dir)?;

    // Same as the console path: the rebuild can take the firewall with it.

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
