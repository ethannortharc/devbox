use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Sandbox name
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(args: DiffArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;

    if state.mount_mode == "writable" {
        println!("Sandbox '{}' uses writable mode — no overlay to diff.", name);
        return Ok(());
    }

    let runtime = manager.runtime_for_sandbox(&state)?;
    let changes = overlay::diff(runtime.as_ref(), &name).await?;

    if changes.is_empty() {
        println!("No changes (overlay is clean).");
        return Ok(());
    }

    let added = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Added).count();
    let modified = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Modified).count();
    let deleted = changes.iter().filter(|c| c.status == overlay::ChangeStatus::Deleted).count();

    for c in &changes {
        if !c.is_dir {
            println!("  {} {}", c.status.symbol(), c.path);
        }
    }

    println!(
        "\n{} file(s): {} added, {} modified, {} deleted",
        changes.iter().filter(|c| !c.is_dir).count(),
        added,
        modified,
        deleted
    );

    Ok(())
}
