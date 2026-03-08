use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct DiscardArgs {
    /// Only discard specific paths
    #[arg(long)]
    pub path: Option<Vec<String>>,

    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: DiscardArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;

    if state.mount_mode == "writable" {
        println!("Sandbox '{}' uses writable mode — no overlay to discard.", name);
        return Ok(());
    }

    if args.path.is_none() {
        println!("This will discard ALL overlay changes in sandbox '{name}'.");
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

    let runtime = manager.runtime_for_sandbox(&state)?;
    let paths = args.path.as_deref();

    overlay::discard(runtime.as_ref(), &name, paths).await?;

    Ok(())
}
