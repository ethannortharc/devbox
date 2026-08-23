use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// Skip confirmation prompt
    #[arg(long, short)]
    pub force: bool,
}

pub async fn run(args: PruneArgs, manager: &SandboxManager) -> Result<()> {
    let sandboxes = manager.list_sandboxes()?;
    if sandboxes.is_empty() {
        println!("No sandboxes to prune.");
        return Ok(());
    }

    let mut discard_guest_data = args.force;
    if !discard_guest_data {
        println!(
            "This will permanently remove all stopped sandboxes, including any uncommitted or stashed guest overlay data. Host project directories are untouched."
        );
        print!("Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
        discard_guest_data = true;
    }

    let removed = manager.prune_sandboxes(discard_guest_data).await?;
    println!("Pruned {} sandbox(es).", removed);
    Ok(())
}
