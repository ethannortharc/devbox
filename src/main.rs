mod cli;
mod nix;
mod runtime;
mod sandbox;
mod tools;
mod tui;

use anyhow::Result;

use cli::Cli;
use sandbox::SandboxManager;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_smart();
    let manager = SandboxManager::new()?;

    match cli.command {
        Some(cmd) => cmd.run(&manager).await,
        None => {
            // Smart default: create-or-attach
            let tools = cli.tools.as_deref();
            manager.create_or_attach(tools).await
        }
    }
}
