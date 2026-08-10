use anyhow::Result;
use clap::Args;

use crate::nix;
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;

#[derive(Args, Debug)]
pub struct UpgradeArgs {
    /// Tools/sets to add (comma-separated)
    #[arg(long, value_delimiter = ',', required = true)]
    pub tools: Vec<String>,

    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: UpgradeArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    // Build config from current state
    // The target box's config, not the caller's current directory.
    //
    // `load_or_default` on the CWD gave whatever project the operator happened
    // to be standing in — and, failing that, the *defaults*, which enable
    // `shell`, `tools` and `editor`. `apply_tools` only ever turns sets on, so
    // a box that had deliberately disabled one had it silently rebuilt and
    // persisted as enabled: the upgrade undid a choice it was never asked
    // about. ADR-0012 made those sets optional precisely so unchecking them
    // means something.
    let mut config = DevboxConfig::load_or_default(&state.project_dir);
    // Cleared, not defaulted. `SetsSection::default()` *enables* shell, tools,
    // editor, git, container and ai-code — it is the selection a new box
    // starts from — so using it to mean "start from nothing" re-enabled every
    // set the box had turned off. That is the bug this block exists to fix,
    // implemented by the fix itself.
    config.sets = crate::sandbox::config::SetsSection::none();
    config.languages = Default::default();
    for set_name in &state.sets {
        // By canonical name first. `apply_tools` is an *alias* table — it knows
        // `claude` and `mosh` and has no case for `shell`, `tools`, `editor`,
        // `git` or `container`, so feeding recorded set names through it
        // cleared those five and restored nothing. A routine upgrade rebuilt
        // the box without them.
        if !config.enable_set(set_name) {
            let tool_name = set_name.strip_prefix("lang-").unwrap_or(set_name);
            config.apply_tools(&[tool_name.to_string()]);
        }
    }

    // The box's ad-hoc packages, and *only* the box's.
    //
    // `load_or_default` above read the current directory's devbox.toml, which
    // is whatever project the operator happened to be standing in. Inserting
    // the box's packages on top of that left the other project's still in the
    // map — and since `apply_config` now writes the whole map into guest
    // state, `devbox upgrade --name box-b` run from project A rebuilt A's
    // packages into B, where nothing recorded them and nothing would remove
    // them. Clearing first is what makes the box its own authority.
    config.custom_packages.clear();
    let project = DevboxConfig::load_or_default(&state.project_dir);

    // Read through the selection rather than off `state.packages` directly,
    // because a v3 box has no `packages` at all and the difference is
    // destructive here.
    //
    // The loop this replaces iterated an empty vector for such a box, so the
    // rebuild dropped every custom package it had. That was survivable while
    // the absence was still legible: `from_state_and_project` would read them
    // back out of `devbox.toml` on the next render. It stopped being
    // survivable when `save` below began stamping `schema`, which is the very
    // signal that fallback keys on — an upgrade would have recorded "this file
    // is current and has no packages" about a box whose packages it had just
    // discarded, and nothing could have recovered them afterwards.
    //
    // The marker means "every field was written by code that writes them all".
    // A path that does not populate them must not stamp it.
    let selection = packages_to_carry(&state, &project);
    for pkg in &selection.packages {
        // The source the box recorded comes first, and the selection has
        // already applied that precedence. After `devbox use` the project file
        // has never heard of an aliased package, and falling straight through
        // to `nixpkgs` is how the alias is lost.
        let source = selection
            .sources
            .get(pkg)
            .cloned()
            .unwrap_or_else(|| "nixpkgs".to_string());
        config.custom_packages.insert(pkg.clone(), source);
    }

    // Apply new tools
    println!("Adding tools: {}", args.tools.join(", "));
    // Every path that rewrites a box's generated configuration takes the same
    // claim. The Sets paths took it first and these did not, so a rebuild
    // started here could still interleave with one from the console and leave
    // the active generation and the recorded selection describing different
    // things.
    let claim = crate::web::build::claim_box(&manager.state_dir, &name)?;
    // Re-read under the claim. The copy above was taken for validation, long
    // before this claim existed, and a `devbox use` completing in the gap
    // releases its own claim — so this one succeeds over a snapshot naming the
    // project the box has just stopped belonging to.
    let state = manager.get_sandbox(&name)?;

    // Not `?`. A failure here can happen *after* `nixos-rebuild switch`, and a
    // failed switch attempts a rollback — which is another network-generation
    // change. Either way devbox's nftables table is already gone, so returning
    // now leaves an isolated or allowlisted box with no firewall at all, which
    // is a worse outcome than the upgrade failing.
    let rebuilt = nix::upgrade_sets(runtime.as_ref(), &name, &mut config, &args.tools).await;
    if let Err(e) = rebuilt {
        // Best effort, and reported: the box may be unreachable, in which case
        // saying so is more use than a second error about the firewall.
        if let Err(restore) =
            crate::policy::enforce::restore_after_rebuild(manager, &name, &claim).await
        {
            eprintln!(
                "devbox: WARNING — the upgrade failed *and* the egress posture could \
                 not be restored, so box '{name}' may be running unrestricted: {restore}"
            );
        }
        return Err(e);
    }

    // A rebuild restarts the network stack and removes devbox's nftables
    // table, so the saved posture has to go back on — the same reason
    // `reprovision`, `use`, and the Sets paths do it.
    // Deferred: the rebuild happened, so the recorded sets must match the box
    // whether or not the firewall came back. Returning here first left
    // state.json describing the *old* selection for a box that already has the
    // new one — a second, quieter inconsistency layered on the first.
    let restored = crate::policy::enforce::restore_after_rebuild(manager, &name, &claim).await;

    // Update saved state with new sets/languages
    let mut updated_state = state;
    updated_state.sets = config.active_sets();
    updated_state.languages = config.active_languages();
    // And with the packages that were just built into the guest.
    //
    // Recovering them above only fixed the rebuild; the state file is what the
    // next render reads. Saving the old, empty vector under a stamped schema
    // would have told every later caller that a box with packages had none —
    // authoritatively, which is worse than the silence it replaced.
    updated_state.packages = selection.packages.iter().cloned().collect();
    updated_state.package_sources = selection.sources.clone();
    updated_state.save(&manager.state_dir)?;

    // Now the restore result. State is recorded either way — the rebuild
    // really did happen — but a failed restore still fails the command, so a
    // script cannot read "upgraded" over an unrestricted box.
    restored?;

    println!("Upgrade complete.");
    Ok(())
}

/// The packages an upgrade must carry into the rebuild *and* back into state.
///
/// A named function because the defect it prevents is one of omission: the loop
/// that reads them and the state that records them are seventy lines apart, and
/// the version that read `state.packages` directly was correct-looking at both
/// ends while silently discarding a v3 box's packages in between.
fn packages_to_carry(
    state: &crate::sandbox::state::SandboxState,
    project: &DevboxConfig,
) -> crate::nix::compose::Selection {
    crate::nix::compose::Selection::from_state_and_project(state, project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::{SCHEMA, SandboxState};

    fn v3_box() -> SandboxState {
        SandboxState {
            // No `packages`, no `schema` — a box created before either existed.
            schema: 0,
            packages: vec![],
            package_sources: Default::default(),
            name: "old".into(),
            runtime: "docker".into(),
            project_dir: "/tmp/p".into(),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec!["system".into()],
            languages: vec![],
            image: "nixos".into(),
        }
    }

    #[test]
    fn upgrading_a_v3_box_carries_the_packages_it_had() {
        let mut project = DevboxConfig::default();
        project
            .custom_packages
            .insert("ripgrep".into(), "nixpkgs".into());
        project
            .custom_packages
            .insert("my-tf".into(), "nixpkgs#terraform".into());

        let carried = packages_to_carry(&v3_box(), &project);
        assert!(
            carried.packages.contains("ripgrep"),
            "{:?}",
            carried.packages
        );
        assert_eq!(
            carried.attr_path("my-tf"),
            "terraform",
            "the alias's source has to survive, or the rebuild fails"
        );
    }

    #[test]
    fn what_is_carried_is_what_gets_persisted() {
        // The half that made this permanent. Reading the packages fixed the
        // rebuild; writing the *old, empty* vector under a stamped schema still
        // told every later caller that a box with packages had none — and the
        // stamp is exactly what the migration fallback keys on, so nothing
        // could recover them afterwards.
        let mut project = DevboxConfig::default();
        project
            .custom_packages
            .insert("ripgrep".into(), "nixpkgs".into());

        let carried = packages_to_carry(&v3_box(), &project);

        let mut updated = v3_box();
        updated.packages = carried.packages.iter().cloned().collect();
        updated.package_sources = carried.sources.clone();

        let dir = tempfile::tempdir().unwrap();
        updated.save(dir.path()).unwrap();
        let reloaded = SandboxState::load(dir.path(), "old").unwrap();

        assert_eq!(reloaded.schema, SCHEMA, "saving stamps the schema");
        assert!(
            reloaded.packages.contains(&"ripgrep".to_string()),
            "so the packages must be there to be stamped over: {:?}",
            reloaded.packages
        );

        // With the state now authoritative, the fallback correctly stops
        // firing — which is only safe because the packages really are recorded.
        let alone = crate::nix::compose::Selection::from_state_and_project(
            &reloaded,
            &DevboxConfig::default(),
        );
        assert!(alone.packages.contains("ripgrep"));
    }
}
