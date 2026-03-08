use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct DestroyArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Skip confirmation prompt
    #[arg(long, short)]
    pub force: bool,
}

pub async fn run(args: DestroyArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    if !args.force {
        println!("This will permanently destroy sandbox '{name}' and all its data.");
        print!("Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    manager.destroy_sandbox(&name).await
}
