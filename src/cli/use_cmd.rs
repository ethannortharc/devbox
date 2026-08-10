use anyhow::{Context, Result, bail};
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
    let same_dir = state
        .project_dir
        .canonicalize()
        .unwrap_or_else(|_| state.project_dir.clone())
        == cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    let same_mode = state.mount_mode == mount_mode;

    if same_dir && same_mode {
        let runtime = manager.runtime_for_sandbox(&state)?;
        let status = runtime.status(name).await?;
        if status == crate::runtime::SandboxStatus::Running {
            println!(
                "Already using '{}' with {}. Attaching...",
                cwd.display(),
                mount_mode
            );
            return manager.attach(name).await;
        }
    }

    // Read the target project's config before anything is disturbed.
    //
    // `restore_after_rebuild` at the end is the first thing that opens it, and
    // by then the box has been reprovisioned or — on Lima — stopped and
    // restarted, which takes devbox's nftables table with it. A malformed
    // `devbox.toml` in the directory being moved to therefore failed *after*
    // the firewall was already gone, leaving a live box with no posture and a
    // command that reported an error about a file. Discovering it here costs
    // nothing and changes nothing.
    crate::sandbox::config::DevboxConfig::load_for_edit(&cwd).with_context(|| {
        format!(
            "refusing to move box '{name}' to {}: its devbox.toml cannot be read, \
             and the move would restart the box before finding that out",
            cwd.display()
        )
    })?;

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
    // Claimed before the runtime is touched, and for every mode.
    //
    // It used to be taken inside the overlay branch, *after* `update_mounts`
    // had already stopped Lima, rewritten its configuration and started it
    // again. A claim that fails at that point fails having already done the
    // disruptive half: mounts moved, state still naming the old project. And
    // `--writable` never took it at all, so that path could restart the guest
    // underneath a running rebuild with nothing refusing it.
    //
    // Held for the whole command, because every part of it — the mounts, the
    // reprovision, the state write — is a change a concurrent rebuild must not
    // interleave with.
    let claim = crate::web::build::claim_box(&manager.state_dir, name)
        .context("cannot switch this box to another project while a rebuild is in progress")?;

    runtime.update_mounts(name, &mounts).await?;

    // Resolved while `state.project_dir` still names the project these came
    // from, and used for both the rebuild and the state saved after it.
    //
    // A v3 box has no `packages`, so they were only ever recoverable from that
    // project file — and this is the command that changes which file that is.
    // Reading them after the move would ask a project that has never heard of
    // them; not recording them at all stamped the schema over an empty list.
    // Either way the packages become unrecoverable, which is why this happens
    // here and is written back below.
    let packages = provision::resolved_packages(&state);

    // If overlay mode, reprovision so NixOS module sets up the overlay mount
    if is_overlay {
        println!("Setting up OverlayFS mount via NixOS...");
        // The box's ad-hoc packages come along. The three-argument wrapper
        // rebuilds with an empty list, so switching a project silently removed
        // every package added through the Sets tab while `state.packages` went
        // on reporting them as selected.
        if let Err(e) = provision::provision_vm_full(
            runtime.as_ref(),
            name,
            &state.sets,
            &state.languages,
            &state.image,
            "overlay",
            &packages.0,
        )
        .await
        {
            eprintln!("Warning: overlay mount setup failed: {e}");
        }
    }

    // Record the new project *before* restoring the posture, and restore once
    // for every path.
    //
    // Two bugs sat here. The restore was inside the overlay branch, so
    // `--writable` — which stops and restarts the VM on Lima — lost the
    // firewall entirely. And in overlay mode it ran while `state.project_dir`
    // still pointed at the *old* project, so it reinstalled the previous
    // project's posture onto a box that had just been switched to a new one.
    //
    // The posture belongs to the project the box is now serving, so the state
    // update has to come first.
    state.packages = packages.1.packages.iter().cloned().collect();
    state.package_sources = packages.1.sources.clone();

    state.project_dir = cwd;
    state.mount_mode = mount_mode.to_string();
    state.save(&manager.state_dir)?;

    // Unconditional: both branches disturb the box — one reprovisions, the
    // other restarts the VM — and both take devbox's nftables table with them.
    crate::policy::enforce::restore_after_rebuild(manager, &state, name, &claim).await?;

    println!("Sandbox '{}' updated. Attaching...", name);
    manager.attach(name).await
}
