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
    let cwd = std::env::current_dir()?;
    let mut config = DevboxConfig::load_or_default(&cwd);

    // Re-apply existing sets from state
    for set_name in &state.sets {
        let tool_name = set_name.strip_prefix("lang-").unwrap_or(set_name);
        config.apply_tools(&[tool_name.to_string()]);
    }

    // The box's ad-hoc packages, which `load_or_default` reads from the
    // *current directory's* devbox.toml — not necessarily this box's project.
    // Without them `apply_config` writes guest state with an empty
    // custom-package map, so the rebuild removes every package added through
    // the Sets tab while state.json goes on reporting them.
    let project = DevboxConfig::load_or_default(&state.project_dir);
    for pkg in &state.packages {
        let source = project
            .custom_packages
            .get(pkg)
            .cloned()
            .unwrap_or_else(|| "nixpkgs".to_string());
        config.custom_packages.insert(pkg.clone(), source);
    }

    // Apply new tools
    println!("Adding tools: {}", args.tools.join(", "));
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
