use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Error, Result, anyhow, bail};
use clap::Args;

use crate::runtime::{Mount, Runtime, SandboxStatus};
use crate::sandbox::provision;
use crate::sandbox::{SandboxManager, overlay};

async fn stop_after_switch_failure(
    runtime: &dyn crate::runtime::Runtime,
    name: &str,
    error: Error,
) -> Error {
    match runtime.stop(name).await {
        Ok(()) => error.context(format!(
            "box '{name}' was stopped for safety because its project switch did not reconcile"
        )),
        Err(stop_error) => anyhow!(
            "project switch failed: {error:#}; additionally, box '{name}' could not be stopped for safety: {stop_error:#}"
        ),
    }
}

fn require_mount_updates(runtime: &dyn Runtime, name: &str) -> Result<()> {
    if !runtime.supports_mount_updates() {
        bail!(
            "`devbox use` is not supported for the {} runtime; box '{}' was not changed or stopped. Use Lima or Incus for switchable project mounts",
            runtime.name(),
            name
        );
    }
    Ok(())
}

fn validate_use_contract(image: &str, mount_mode: &str, name: &str) -> Result<()> {
    if image == "ubuntu" && mount_mode == "overlay" {
        bail!(
            "Ubuntu overlay workspaces are not implemented; rerun `devbox use {name} --writable` or use a NixOS box. Box '{name}' was not changed or stopped"
        );
    }
    Ok(())
}

fn needs_guest_mount_reconcile(image: &str) -> bool {
    image == "nixos"
}

fn same_project(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

fn overlay_departure_requires_clean(
    old_mode: &str,
    old_project: &std::path::Path,
    new_mode: &str,
    new_project: &std::path::Path,
) -> bool {
    old_mode == "overlay" && (new_mode != "overlay" || !same_project(old_project, new_project))
}

fn require_overlay_inspectable(name: &str, status: SandboxStatus) -> Result<()> {
    match status {
        SandboxStatus::Running => Ok(()),
        SandboxStatus::Stopped => bail!(
            "cannot switch box '{name}' away from its overlay while it is stopped because uncommitted or stashed work cannot be verified. Run `devbox shell {name}` to start it, exit the shell, resolve the layer, then retry; its project mounts were not changed"
        ),
        SandboxStatus::Unreachable(reason) => bail!(
            "cannot switch box '{name}' away from its overlay because its guest shell is unreachable ({reason}) and uncommitted or stashed work cannot be verified; its project mounts were not changed"
        ),
        SandboxStatus::Unknown(status) => bail!(
            "cannot switch box '{name}' away from its overlay while its runtime state is unknown ({status}); its project mounts were not changed"
        ),
        SandboxStatus::NotFound => bail!(
            "cannot switch box '{name}' away from its overlay because the runtime object is missing and its guest data cannot be inspected; its project mounts were not changed"
        ),
    }
}

async fn ensure_overlay_departure_is_clean(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let status = runtime.status(name).await.with_context(|| {
        format!(
            "cannot verify the old overlay for box '{name}'; its project mounts were not changed"
        )
    })?;
    require_overlay_inspectable(name, status)?;

    let changes = overlay::diff(runtime, name).await.with_context(|| {
        format!(
            "cannot verify uncommitted overlay work for box '{name}'; its project mounts were not changed"
        )
    })?;
    let change_count = overlay::meaningful_changes(&changes).len();
    if change_count > 0 {
        bail!(
            "box '{name}' has {change_count} uncommitted overlay change(s). Run `devbox layer commit {name}` to save them or `devbox discard {name}` to discard them before switching projects or mount mode; its project mounts were not changed"
        );
    }

    let has_stash = overlay::has_stash(runtime, name).await.with_context(|| {
        format!(
            "cannot verify stashed overlay work for box '{name}'; its project mounts were not changed"
        )
    })?;
    if has_stash {
        bail!(
            "box '{name}' has stashed overlay changes. Run `devbox layer stash-pop {name}`, then commit or discard them before switching projects or mount mode; a stash is never carried to another project and its mounts were not changed"
        );
    }

    Ok(())
}

#[derive(Args, Debug)]
pub struct UseArgs {
    /// Sandbox name
    pub name: String,

    /// Mount in writable mode (no overlay)
    #[arg(long)]
    pub writable: bool,
}

pub async fn run(args: UseArgs, manager: &SandboxManager) -> Result<()> {
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the target project directory")?;
    let name = &args.name;

    if !manager.sandbox_exists(name) {
        bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(name)?;
    let mount_mode = if args.writable { "writable" } else { "overlay" };
    validate_use_contract(&state.image, mount_mode, name)?;

    // If already pointing at same dir with same mode and running, just attach
    let same_dir = same_project(&state.project_dir, &cwd);
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
    // Re-read under the claim. The copy above was taken for validation, long
    // before this claim existed, and a `devbox use` completing in the gap
    // releases its own claim — so this one succeeds over a snapshot naming the
    // project the box has just stopped belonging to.
    let mut state = manager.get_sandbox(name)?;

    // The copy used by the no-op attach shortcut above is only a hint. Every
    // decision that authorizes a switch is made again from this state protected
    // by the box claim.
    validate_use_contract(&state.image, mount_mode, name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;
    require_mount_updates(runtime.as_ref(), name)?;

    // Overlay upper data and stashes belong to the old project. They must
    // never be carried over a new lower directory, and changing to writable
    // must not make state stop protecting data that remains on the guest.
    // Inspect while the box claim proves the runtime still has the old mounts,
    // and fail closed for every state that cannot be inspected.
    if overlay_departure_requires_clean(&state.mount_mode, &state.project_dir, mount_mode, &cwd) {
        ensure_overlay_departure_is_clean(runtime.as_ref(), name).await?;
    }

    // A target-directory reservation, distinct from the per-box claim. Two
    // different boxes otherwise hold different box claims and can both pass a
    // mount-conflict check before either writes state. Create uses this same
    // project claim, so the invariant is cross-command and cross-process.
    let lock_dir = manager.state_dir.clone();
    let lock_project = cwd.clone();
    let project_claim = crate::web::build::claim_project_off_worker(move || {
        crate::web::build::claim_project(&lock_dir, &lock_project)
    })
    .await
    .context("cannot reserve the target project for this box")?;

    if let Some(existing) = manager.check_mount_conflict_except(&cwd, Some(name))? {
        bail!(
            "Directory already mounted by sandbox '{}'. Use `devbox shell {}` to attach; box '{}' was not changed or stopped.",
            existing,
            existing,
            name
        );
    }

    // Read the target config under its project claim. A malformed file is
    // rejected before the runtime or auxiliary directories are touched, and a
    // concurrent config writer cannot replace it between this read and state
    // registration.
    let target_config =
        crate::sandbox::config::DevboxConfig::load_for_edit(&cwd).with_context(|| {
            format!(
                "refusing to move box '{name}' to {}: its devbox.toml cannot be read, \
                 and the move would restart the box before finding that out",
                cwd.display()
            )
        })?;

    // Resolve the target project's entire mount set through the same path as
    // create. This carries relative and extra configured mounts across the
    // switch instead of replacing them with workspace alone.
    let is_overlay = mount_mode == "overlay";
    let mut mounts = crate::sandbox::resolve_project_mounts(&cwd, &target_config, is_overlay, &[]);
    if crate::obs::uses_host_socket(runtime.name()) {
        let obs_dir = crate::obs::collector::endpoint_dir(&manager.state_dir, name);
        std::fs::create_dir_all(&obs_dir)
            .with_context(|| format!("create observability directory {}", obs_dir.display()))?;
        std::fs::set_permissions(&obs_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect observability directory {}", obs_dir.display()))?;
        mounts.push(Mount {
            host_path: obs_dir,
            container_path: "/run/devbox-host".to_string(),
            read_only: false,
        });
    }
    println!(
        "Switching sandbox '{}' to '{}' (mode: {})...",
        name,
        cwd.display(),
        mount_mode,
    );

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

    // Supported runtimes own rollback until they return a token: an error
    // means either nothing was changed or the old mounts/running state were
    // restored. Do not add a blanket `stop` here — preflight and unsupported
    // failures have not disturbed the box.
    let mount_update = runtime
        .update_mounts(name, &mounts)
        .await
        .with_context(|| {
            format!("box '{name}' could not switch projects; no new devbox state was committed")
        })?;

    // The runtime now points at the new project, so record that fact before
    // any guest provisioning. If a later step fails the stopped box remains
    // visible under the project/mode its mounts actually use.
    state.packages = packages.1.packages.iter().cloned().collect();
    state.package_sources = packages.1.sources.clone();
    state.project_dir = cwd;
    state.mount_mode = mount_mode.to_string();
    if let Err(error) = state.save(&manager.state_dir) {
        return match runtime.rollback_mounts(name, &mount_update).await {
            Ok(()) => Err(error.context(format!(
                "box '{name}' state could not record the project switch; the original runtime mounts were restored and the box was stopped"
            ))),
            Err(rollback_error) => Err(anyhow!(
                "box '{name}' state could not record the project switch: {error:#}; restoring its original runtime mounts also failed: {rollback_error:#}"
            )),
        };
    }

    // State now durably reserves the target. Release before policy restore,
    // which takes the same project lock while reading the new configuration.
    drop(project_claim);

    // NixOS owns the guest-side filesystem declaration for both directions.
    // Rebuilding only when entering overlay leaves the old OverlayFS unit in
    // place when switching back to writable; it can shadow the new direct
    // mount while host state incorrectly says writes are going to the host.
    if needs_guest_mount_reconcile(&state.image) {
        println!("Reconfiguring the NixOS workspace mount ({mount_mode})...");
        // The box's ad-hoc packages come along. The three-argument wrapper
        // rebuilds with an empty list, so switching a project silently removed
        // every package added through the Sets tab while `state.packages` went
        // on reporting them as selected.
        if let Err(error) = provision::provision_vm_full(
            runtime.as_ref(),
            name,
            &state.sets,
            &state.languages,
            &state.image,
            mount_mode,
            &packages.0,
        )
        .await
        {
            return Err(stop_after_switch_failure(runtime.as_ref(), name, error).await);
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
    // Unconditional: both branches disturb the box — one reprovisions, the
    // other restarts the VM — and both take devbox's nftables table with them.
    if let Err(error) = crate::policy::enforce::restore_after_rebuild(manager, name, &claim).await {
        return Err(stop_after_switch_failure(runtime.as_ref(), name, error).await);
    }

    // Released before the shell, not after it.
    //
    // The claim covers the switch — the mounts, the reprovision, the state
    // write, the posture restore — and every one of those is finished here.
    // Holding it through `attach` meant holding it for as long as the user kept
    // the shell open, during which every rebuild, policy edit, project switch,
    // stop and destroy of this box failed with "already rebuilding". An
    // interactive session is not a critical section.
    drop(claim);

    println!("Sandbox '{}' updated. Attaching...", name);
    manager.attach(name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        docker::DockerRuntime, incus::IncusRuntime, lima::LimaRuntime, multipass::MultipassRuntime,
    };

    #[test]
    fn unsupported_runtimes_are_rejected_before_the_update_path() {
        for runtime in [
            &DockerRuntime as &dyn Runtime,
            &MultipassRuntime as &dyn Runtime,
        ] {
            let error = require_mount_updates(runtime, "box").unwrap_err();
            assert!(error.to_string().contains("was not changed or stopped"));
        }
    }

    #[test]
    fn lima_and_incus_reach_the_transactional_update_path() {
        assert!(require_mount_updates(&LimaRuntime, "box").is_ok());
        assert!(require_mount_updates(&IncusRuntime, "box").is_ok());
    }

    #[test]
    fn ubuntu_overlay_is_rejected_before_any_runtime_operation() {
        let error = validate_use_contract("ubuntu", "overlay", "box").unwrap_err();
        assert!(error.to_string().contains("devbox use box --writable"));
        assert!(error.to_string().contains("was not changed or stopped"));
        assert!(validate_use_contract("ubuntu", "writable", "box").is_ok());
        assert!(validate_use_contract("nixos", "overlay", "box").is_ok());
    }

    #[test]
    fn nixos_reconciles_the_guest_for_overlay_and_writable_switches() {
        assert!(needs_guest_mount_reconcile("nixos"));
        assert!(!needs_guest_mount_reconcile("ubuntu"));
    }

    #[test]
    fn leaving_an_overlay_requires_a_clean_layer_but_a_noop_does_not() {
        let old = std::path::Path::new("/project/old");
        let other = std::path::Path::new("/project/other");

        assert!(overlay_departure_requires_clean(
            "overlay", old, "writable", old
        ));
        assert!(overlay_departure_requires_clean(
            "overlay", old, "overlay", other
        ));
        assert!(!overlay_departure_requires_clean(
            "overlay", old, "overlay", old
        ));
        assert!(!overlay_departure_requires_clean(
            "writable", old, "overlay", other
        ));
    }

    #[test]
    fn overlay_departure_fails_closed_when_guest_data_cannot_be_inspected() {
        assert!(require_overlay_inspectable("box", SandboxStatus::Running).is_ok());
        let stopped = require_overlay_inspectable("box", SandboxStatus::Stopped)
            .unwrap_err()
            .to_string();
        assert!(stopped.contains("devbox shell box"));
        assert!(!stopped.contains("devbox start"));
        for status in [
            SandboxStatus::NotFound,
            SandboxStatus::Unreachable("ssh refused".into()),
            SandboxStatus::Unknown("starting".into()),
        ] {
            let error = require_overlay_inspectable("box", status)
                .unwrap_err()
                .to_string();
            assert!(error.contains("project mounts were not changed"));
        }
    }
}
