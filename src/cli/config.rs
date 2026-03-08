use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Set a default value
    Set {
        /// Key (e.g., default.runtime, default.tools, default.layout)
        key: String,
        /// Value
        value: String,
    },
    /// Get a config value
    Get {
        /// Key
        key: String,
    },
    /// Show all config
    Show,
}

pub async fn run(args: ConfigArgs, manager: &SandboxManager) -> Result<()> {
    match args.action {
        ConfigAction::Set { key, value } => {
            let mut config = manager.load_global_config()?;
            config.set(&key, &value)?;
            manager.save_global_config(&config)?;
            println!("{} {} = {}", "Set".green().bold(), key, value);
            Ok(())
        }
        ConfigAction::Get { key } => {
            let config = manager.load_global_config()?;
            match config.get(&key) {
                Some(value) => {
                    println!("{}", value);
                    Ok(())
                }
                None => {
                    bail!(
                        "Unknown config key '{}'. Available: default.runtime, default.layout, default.tools",
                        key
                    );
                }
            }
        }
        ConfigAction::Show => {
            let config = manager.load_global_config()?;
            println!("{}", "Global Config".bold());
            println!("  {} = {}", "default.runtime".cyan(), config.default.runtime);
            println!("  {} = {}", "default.layout".cyan(), config.default.layout);
            println!(
                "  {} = {}",
                "default.tools".cyan(),
                if config.default.tools.is_empty() {
                    "(none)".dimmed().to_string()
                } else {
                    config.default.tools.join(", ")
                }
            );
            println!();
            println!(
                "{} {}",
                "Config file:".dimmed(),
                manager.state_dir.join("config.toml").display()
            );
            Ok(())
        }
    }
}
