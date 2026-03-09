use anyhow::{Result, bail};
use clap::Args;

use crate::runtime::Mount;
use crate::sandbox::SandboxManager;
use crate::sandbox::provision;

#[derive(Args, Debug)]
pub struct UseArgs {
    /// Sandbox name
    pub name: String,

    /// Mount in writable mode (no overlay)
    #[arg(long)]
    pub writable: bool,
}

pub async fn run(args: UseArgs, manager: &SandboxManager) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let name = &args.name;

    if !manager.sandbox_exists(name) {
        bail!("Sandbox '{}' not found.", name);
    }

    let mut state = manager.get_sandbox(name)?;
    let mount_mode = if args.writable { "writable" } else { "overlay" };

    // If already pointing at same dir with same mode and running, just attach
    let same_dir = state.project_dir.canonicalize().unwrap_or_else(|_| state.project_dir.clone())
        == cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    let same_mode = state.mount_mode == mount_mode;

    if same_dir && same_mode {
        let runtime = manager.runtime_for_sandbox(&state)?;
        let status = runtime.status(name).await?;
        if status == crate::runtime::SandboxStatus::Running {
            println!("Already using '{}' with {}. Attaching...", cwd.display(), mount_mode);
            return manager.attach(name, None).await;
        }
    }

    // Build new mounts for this directory
    let is_overlay = mount_mode == "overlay";
    let (container_path, read_only) = if is_overlay {
        ("/mnt/host".to_string(), true)
    } else {
        ("/workspace".to_string(), false)
    };

    let mounts = vec![Mount {
        host_path: cwd.clone(),
        container_path,
        read_only,
    }];

    // Update mounts via runtime (stop, edit config, start)
    let runtime = manager.runtime_for_sandbox(&state)?;
    println!(
        "Switching sandbox '{}' to '{}' (mode: {})...",
        name,
        cwd.display(),
        mount_mode,
    );
    runtime.update_mounts(name, &mounts).await?;

    // If overlay mode, set up the overlay mount inside the VM
    if is_overlay {
        println!("Setting up OverlayFS mount...");
        if let Err(e) = provision::setup_overlay_mount(runtime.as_ref(), name).await {
            eprintln!("Warning: overlay mount setup failed: {e}");
            eprintln!("You may need to run overlay setup manually inside the VM.");
        }
    }

    // Update sandbox state
    state.project_dir = cwd;
    state.mount_mode = mount_mode.to_string();
    state.save(&manager.state_dir)?;

    println!("Sandbox '{}' updated. Attaching...", name);
    manager.attach(name, None).await
}
