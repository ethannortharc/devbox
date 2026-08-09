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
        let tool_name = set_name.strip_prefix("lang-").unwrap_or(set_name);
        config.apply_tools(&[tool_name.to_string()]);
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
    for pkg in &state.packages {
        // The source the box recorded comes first. After `devbox use` the
        // project file has never heard of an aliased package, and falling
        // straight through to `nixpkgs` is how the alias is lost.
        let source = state
            .package_sources
            .get(pkg)
            .or_else(|| project.custom_packages.get(pkg))
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
    let _lock = crate::web::build::lock_rebuild(&manager.state_dir, &name)?;

    nix::upgrade_sets(runtime.as_ref(), &name, &mut config, &args.tools).await?;

    // A rebuild restarts the network stack and removes devbox's nftables
    // table, so the saved posture has to go back on — the same reason
    // `reprovision`, `use`, and the Sets paths do it.
    // Deferred: the rebuild happened, so the recorded sets must match the box
    // whether or not the firewall came back. Returning here first left
    // state.json describing the *old* selection for a box that already has the
    // new one — a second, quieter inconsistency layered on the first.
    let restored = crate::policy::enforce::restore_after_rebuild(manager, &state, &name).await;

    // Update saved state with new sets/languages
    let mut updated_state = state;
    updated_state.sets = config.active_sets();
    updated_state.languages = config.active_languages();
    updated_state.save(&manager.state_dir)?;

    // Now the restore result. State is recorded either way — the rebuild
    // really did happen — but a failed restore still fails the command, so a
    // script cannot read "upgraded" over an unrestricted box.
    restored?;

    println!("Upgrade complete.");
    Ok(())
}
