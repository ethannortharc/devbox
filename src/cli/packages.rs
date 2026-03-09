use anyhow::Result;
use clap::Args;

use crate::nix;
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;
use crate::tui::packages::run_packages_tui;

#[derive(Args, Debug)]
pub struct PackagesArgs {
    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: PackagesArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;

    // Run TUI — returns list of toggle actions
    let toggles = run_packages_tui(&state.sets)?;

    if toggles.is_empty() {
        return Ok(());
    }

    // Deduplicate toggles (last action wins)
    let mut final_toggles = std::collections::HashMap::new();
    for (set_name, enabled) in &toggles {
        final_toggles.insert(set_name.clone(), *enabled);
    }

    // Build tools list from toggle actions
    let tools_to_apply: Vec<String> = final_toggles
        .iter()
        .filter(|(_, enabled)| **enabled)
        .map(|(name, _)| {
            name.strip_prefix("lang-").unwrap_or(name).to_string()
        })
        .collect();

    if tools_to_apply.is_empty() && final_toggles.values().all(|v| !v) {
        println!("No sets enabled. Skipping rebuild.");
        return Ok(());
    }

    // Apply changes via nix rebuild
    let runtime = manager.runtime_for_sandbox(&state)?;
    let cwd = std::env::current_dir()?;
    let mut config = DevboxConfig::load_or_default(&cwd);

    // Re-apply current state
    for set_name in &state.sets {
        let tool_name = set_name.strip_prefix("lang-").unwrap_or(set_name);
        config.apply_tools(&[tool_name.to_string()]);
    }

    // Apply new toggles
    for (set_name, enabled) in &final_toggles {
        match set_name.as_str() {
            "editor" => config.sets.editor = *enabled,
            "git" => config.sets.git = *enabled,
            "container" => config.sets.container = *enabled,
            "network" => config.sets.network = *enabled,
            "ai-code" => config.sets.ai_code = *enabled,
            "ai-infra" => config.sets.ai_infra = *enabled,
            "lang-go" => config.languages.go = *enabled,
            "lang-rust" => config.languages.rust = *enabled,
            "lang-python" => config.languages.python = *enabled,
            "lang-node" => config.languages.node = *enabled,
            "lang-java" => config.languages.java = *enabled,
            "lang-ruby" => config.languages.ruby = *enabled,
            _ => {}
        }
    }

    println!("Applying package changes...");
    nix::apply_config(runtime.as_ref(), &name, &config).await?;

    // Update saved state
    let mut updated_state = state;
    updated_state.sets = config.active_sets();
    updated_state.languages = config.active_languages();
    updated_state.save(&manager.state_dir)?;

    println!("Package changes applied successfully.");
    Ok(())
}
