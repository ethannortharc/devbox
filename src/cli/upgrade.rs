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

    // Apply new tools
    println!("Adding tools: {}", args.tools.join(", "));
    nix::upgrade_sets(runtime.as_ref(), &name, &mut config, &args.tools).await?;

    // Update saved state with new sets/languages
    let mut updated_state = state;
    updated_state.sets = config.active_sets();
    updated_state.languages = config.active_languages();
    updated_state.save(&manager.state_dir)?;

    println!("Upgrade complete.");
    Ok(())
}
