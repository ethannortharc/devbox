pub mod agent_sync;
pub mod checkpoint;
pub mod config;
pub mod global_config;
pub mod overlay;
pub mod provision;
pub mod state;

use std::collections::HashMap;
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::config::DevboxConfig;
use self::global_config::GlobalConfig;
use self::state::SandboxState;
use crate::runtime::detect::{detect_runtime, select_runtime};
use crate::runtime::{CreateOpts, Mount, Runtime, SandboxStatus};
use crate::tools::detect::detect_languages;

/// Decide whether a non-forced destroy can safely inspect an overlay.
///
/// Any guest whose filesystem cannot be inspected is a dangerous case. The
/// overlay upper and stash live on the guest disk, so stopping a VM does not
/// make either one disappear. Only a running guest can be checked safely; all
/// other extant states must require an explicit `--force`.
fn overlay_diff_required(name: &str, status: SandboxStatus) -> Result<bool> {
    match status {
        SandboxStatus::Running => Ok(true),
        SandboxStatus::Unreachable(reason) => bail!(
            "cannot verify overlay changes for sandbox '{name}' because its guest shell is unreachable: {reason}; refusing to destroy it without --force"
        ),
        SandboxStatus::Unknown(status) => bail!(
            "cannot verify overlay changes for sandbox '{name}' while its runtime state is unknown ({status}); refusing to destroy it without --force"
        ),
        SandboxStatus::Stopped => bail!(
            "cannot verify overlay changes for sandbox '{name}' while it is stopped; start it to inspect or save its changes, or use --force to discard them"
        ),
        // There is no guest disk left to inspect once the runtime object is
        // confirmed absent. Host-side registration can be cleaned up safely.
        SandboxStatus::NotFound => Ok(false),
    }
}

/// Destroy a runtime object without ever orphaning a live guest.
///
/// A failed delete is only equivalent to success when a follow-up status
/// probe proves that the object is already gone. Otherwise local state must
/// remain registered so the user can retry and the console can still manage
/// the guest.
async fn destroy_runtime_or_confirm_absent(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let destroy_error = match runtime.destroy(name).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match runtime.status(name).await {
        Ok(SandboxStatus::NotFound) => Ok(()),
        Ok(status) => bail!(
            "failed to destroy sandbox '{name}': {destroy_error:#}; runtime still reports {status:?}; local state was preserved"
        ),
        Err(status_error) => bail!(
            "failed to destroy sandbox '{name}': {destroy_error:#}; could not confirm that the runtime object is gone: {status_error:#}; local state was preserved"
        ),
    }
}

fn runtime_recovery_hint(runtime: &str, name: &str) -> String {
    let object = format!("devbox-{name}");
    match runtime {
        "docker" => format!("docker rm -f {object}"),
        "lima" => format!("limactl stop {object}; limactl delete {object}"),
        "incus" => format!("incus stop {object} --force; incus delete {object} --force"),
        "multipass" => format!("multipass stop {object}; multipass delete {object} --purge"),
        other => format!("use the {other} runtime CLI to stop and delete object '{object}'"),
    }
}

/// Refuse runtime/image/mount combinations whose advertised data-safety
/// contract is not implemented.
///
/// This runs before `runtime.create`: unsupported product surface must not
/// leave a guest, mount, or host directory behind merely to explain that it
/// was never supported.
pub(crate) fn validate_create_contract(
    runtime: &str,
    config: &DevboxConfig,
    bare: bool,
) -> Result<()> {
    let image = config.sandbox.image.as_str();
    let mount_mode = config.sandbox.mount_mode.as_str();

    if runtime == "multipass" {
        bail!(
            "Multipass creation is currently disabled: it cannot yet guarantee the selected image or read-only host mounts. Use Lima or Incus instead"
        );
    }
    if matches!(runtime, "lima" | "incus" | "docker")
        && image == "ubuntu"
        && mount_mode == "overlay"
    {
        bail!(
            "Ubuntu overlay workspaces are not implemented; choose `--writable` (Web: mount mode ‘writable’) or use the NixOS image"
        );
    }
    if matches!(runtime, "lima" | "incus" | "docker") && bare && mount_mode == "overlay" {
        bail!(
            "bare boxes skip the guest setup that creates /workspace OverlayFS; choose `--writable`"
        );
    }
    if runtime == "docker" && !(image == "ubuntu" && mount_mode == "writable" && bare) {
        bail!(
            "Docker creation currently supports only a bare Ubuntu box with a writable mount. Use `--runtime docker --image ubuntu --writable --bare`; for protected overlay workspaces use Lima or Incus"
        );
    }
    Ok(())
}

/// Resolve the project's configured mounts exactly once for both create and
/// `devbox use`.
///
/// Relative hosts are rooted in the project, and only the workspace target is
/// redirected through `/mnt/host` for OverlayFS. Sorting by the configuration
/// key keeps runtime `mountN` device assignment stable across processes.
pub(crate) fn resolve_project_mounts(
    project_dir: &Path,
    config: &DevboxConfig,
    is_overlay: bool,
    extra_mounts: &[Mount],
) -> Vec<Mount> {
    let mut configured: Vec<_> = config.mounts.iter().collect();
    configured.sort_by_key(|(name, _)| *name);

    let mut mounts: Vec<Mount> = configured
        .into_iter()
        .map(|(_, mount)| {
            let configured = PathBuf::from(&mount.host);
            let host_path = if configured == Path::new(".") {
                project_dir.to_path_buf()
            } else if configured.is_relative() {
                project_dir.join(configured)
            } else {
                configured
            };
            let (container_path, read_only) = if is_overlay && mount.target == "/workspace" {
                ("/mnt/host".to_string(), true)
            } else {
                (mount.target.clone(), mount.readonly)
            };
            Mount {
                host_path,
                container_path,
                read_only,
            }
        })
        .collect();
    mounts.extend(extra_mounts.iter().cloned().map(|mut mount| {
        if mount.host_path.is_relative() {
            mount.host_path = project_dir.join(&mount.host_path);
        }
        mount
    }));
    mounts
}

/// What the box will be able to see, said out loud before it is created.
///
/// A box's mounts used to be invisible until someone looked inside it and
/// found `/workspace` empty. They are the one part of a create that cannot be
/// inspected afterwards without entering the box, and the one part a typo in
/// `devbox.toml` silently removes — so they are printed.
///
/// `/mnt/host` is spelled as `/workspace (read-only lower layer)` because that
/// is where the user will look for it: `/mnt/host` is an implementation detail
/// of the overlay and naming it here would send them to the wrong path.
pub(crate) fn describe_mounts(mounts: &[Mount]) -> String {
    if mounts.is_empty() {
        return concat!(
            "  (nothing — this box will have no project files in it. ",
            "An empty [mounts] table in devbox.toml is what asks for that.)"
        )
        .to_string();
    }
    let mut lines = Vec::with_capacity(mounts.len());
    for mount in mounts {
        let target = if mount.container_path == "/mnt/host" {
            "/workspace (read-only lower layer)"
        } else {
            &mount.container_path
        };
        let access = if mount.read_only { "ro" } else { "rw" };
        lines.push(format!(
            "  {} → {target} ({access})",
            mount.host_path.display()
        ));
    }
    lines.join("\n")
}

/// Central manager for sandbox lifecycle.
pub struct SandboxManager {
    /// Path to ~/.devbox/
    pub state_dir: PathBuf,
}

impl SandboxManager {
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let state_dir = home.join(".devbox");

        if !state_dir.exists() {
            std::fs::create_dir_all(&state_dir).context("Failed to create ~/.devbox/")?;
        }

        Ok(Self { state_dir })
    }

    // ── Runtime Resolution ───────────────────────────────

    /// Resolve the runtime for a new sandbox.
    /// Uses explicit choice, project config, global config, or auto-detection.
    pub fn resolve_runtime(&self, explicit: Option<&str>) -> Result<Box<dyn Runtime>> {
        if let Some(name) = explicit {
            return select_runtime(name);
        }

        // Check global config
        let global = self.load_global_config().unwrap_or_default();
        if global.default.runtime != "auto" {
            return select_runtime(&global.default.runtime);
        }

        detect_runtime()
    }

    /// Resolve the runtime for an existing sandbox from its saved state.
    pub fn runtime_for_sandbox(&self, state: &SandboxState) -> Result<Box<dyn Runtime>> {
        select_runtime(&state.runtime)
    }

    // ── Lifecycle ────────────────────────────────────────

    /// Claim a box and read its state under that claim, in that order.
    ///
    /// The order is the whole point. Every caller that took the claim *after*
    /// reading the state had a correct-looking claim over a stale snapshot: a
    /// `devbox use` completing in the gap releases its own claim, so this one
    /// succeeds — and then the command proceeds against the project the box
    /// used to belong to. It writes the selection into the old project's
    /// `devbox.toml`, or applies the old project's policy to a box now serving
    /// the new one, which is how an `open` posture lands on a box recorded as
    /// `isolated`.
    ///
    /// Round 45 fixed that in `sets`. Round 46 found it in the console rebuild,
    /// both policy editors, and the enforcement helper. Pairing the two here
    /// means the pairing cannot be got wrong at a call site, because there is
    /// no call site left to get it wrong at.
    pub fn claim_and_read(
        &self,
        name: &str,
    ) -> Result<(crate::web::build::BoxClaim, SandboxState)> {
        let claim = crate::web::build::claim_box(&self.state_dir, name)?;
        let state = self.get_sandbox(name)?;
        Ok((claim, state))
    }

    /// Commit `devbox.toml` and `state.json` as one recoverable selection update.
    ///
    /// Callers must hold both the box claim and the project claim. Each file is
    /// replaced atomically; if the state write fails, the exact previous
    /// project config bytes are restored (or a newly-created config is
    /// removed), so the two host-side authorities cannot permanently disagree.
    pub(crate) fn save_config_and_state(
        &self,
        config: &DevboxConfig,
        state: &SandboxState,
    ) -> Result<()> {
        let config_path = state.project_dir.join("devbox.toml");
        let original_config = match std::fs::read(&config_path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {} before update", config_path.display()));
            }
        };

        config.save(&config_path)?;
        if let Err(state_error) = state.save(&self.state_dir) {
            let rollback = match original_config {
                Some(content) => crate::sandbox::state::write_atomically(
                    &config_path,
                    &content,
                    "devbox config rollback",
                ),
                None => match std::fs::remove_file(&config_path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error)
                        .with_context(|| format!("remove newly-created {}", config_path.display())),
                },
            };
            if let Err(rollback_error) = rollback {
                bail!(
                    "could not save sandbox state: {state_error:#}; additionally, devbox.toml rollback failed: {rollback_error:#}"
                );
            }
            return Err(state_error)
                .context("could not save sandbox state; devbox.toml was rolled back");
        }
        Ok(())
    }

    /// Create a new sandbox end-to-end.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_sandbox(
        &self,
        name: &str,
        runtime: &dyn Runtime,
        config: &DevboxConfig,
        extra_mounts: &[Mount],
        env_vars: &HashMap<String, String>,
        env_file: Option<PathBuf>,
        bare: bool,
    ) -> Result<()> {
        let cwd = env::current_dir().context("Cannot determine current directory")?;
        self.create_sandbox_at(
            &cwd,
            name,
            runtime,
            config,
            extra_mounts,
            env_vars,
            env_file,
            bare,
            None,
        )
        .await
    }

    /// Create a sandbox for an explicit project directory.
    ///
    /// The web console cannot change the process-wide current directory: two
    /// simultaneous create requests would race every relative mount and could
    /// record one project while building the other. CLI creation delegates
    /// here with its current directory, so both clients use one lifecycle.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_sandbox_at(
        &self,
        project_dir: &Path,
        name: &str,
        runtime: &dyn Runtime,
        config: &DevboxConfig,
        extra_mounts: &[Mount],
        env_vars: &HashMap<String, String>,
        env_file: Option<PathBuf>,
        bare: bool,
        provision_reporter: Option<provision::ProvisionReporter<'_>>,
    ) -> Result<()> {
        let cwd = project_dir
            .canonicalize()
            .with_context(|| format!("Cannot open project directory {}", project_dir.display()))?;

        // Refuse a name that cannot be saved, before anything is made under it.
        //
        // `save` enforces this, and `save` runs after `runtime.create`. Docker
        // accepts a 65-character name and devbox does not, so a name of that
        // length built the container and then failed to record it — leaving a
        // live runtime object with no state file. The retry then passed
        // `sandbox_exists`, because nothing was written, and collided with the
        // object still sitting there; `destroy` could not find it either,
        // because it looks the name up in state. The box had to be removed with
        // the runtime's own CLI.
        //
        // This is the third check to be moved above `runtime.create` for the
        // same reason (see `check_packages_supported` below). The rule they all
        // follow: anything that depends only on the arguments has no business
        // running after the side effect.
        if !crate::sandbox::state::is_safe_name(name) {
            bail!(
                "Refusing to create sandbox {name:?}: a box name must be 1-64 characters, \
                 not a path component, and free of control characters — otherwise \
                 `devbox destroy` cannot remove it again"
            );
        }

        // Serialize create with every other lifecycle operation for this name.
        // The old existence check was a time-of-check/time-of-use race: two
        // requests could both see no state and both create the same runtime
        // object, leaving at least one of them untracked.
        let _claim = crate::web::build::claim_box(&self.state_dir, name)
            .context("cannot create this box while another lifecycle operation is in progress")?;

        // Reserve the project before checking whether it is already mounted,
        // and keep that reservation until state.json records this box. A
        // per-box claim cannot serialize two different names racing to create
        // against the same directory: both used to observe it as free and
        // both could launch a runtime with writable access to it.
        //
        // Acquisition is off-worker because Web creation also comes through
        // this async path, and a contended OS file lock must not block a Tokio
        // worker that the current lock holder needs in order to finish.
        let lock_dir = self.state_dir.clone();
        let lock_project = cwd.clone();
        let project_claim = crate::web::build::claim_project_off_worker(move || {
            crate::web::build::claim_project(&lock_dir, &lock_project)
        })
        .await
        .context("cannot reserve this project for sandbox creation")?;

        // Check for name conflicts
        if self.sandbox_exists(name) {
            bail!(
                "Sandbox '{}' already exists. Use `devbox destroy {}` first.",
                name,
                name
            );
        }

        validate_create_contract(runtime.name(), config, bare)?;

        // Check for mount conflicts
        if let Some(existing) = self.check_mount_conflict(&cwd)? {
            bail!(
                "Directory already mounted by sandbox '{}'. Use `devbox shell` to attach.",
                existing
            );
        }

        // Refuse an unsupported package before anything exists to clean up.
        //
        // This check has now been moved twice. It began inside
        // `provision_vm_full`, where the box was already made and `create` only
        // printed the failure as a warning before saving state and reporting
        // success. Round 23 moved it out — but to just above `provision_vm_full`
        // and *below* `runtime.create`, while the comment claimed it ran "before
        // the box exists". It did not: a refusal there left an orphan VM with no
        // state file, and the retry then passed `sandbox_exists` and collided
        // with the runtime object still sitting there.
        //
        // It depends on nothing but the config, so there was never a reason for
        // it to run late. Here it is genuinely before the box exists.
        if !bare {
            crate::sandbox::provision::check_packages_supported(
                &config.sandbox.image,
                &config
                    .custom_packages
                    .iter()
                    .map(|(n, s)| (n.clone(), s.clone()))
                    .collect::<Vec<_>>(),
            )?;
        }

        // Build mounts from config + extra through the same resolver used by
        // `devbox use`, so switching projects cannot silently lose a target
        // project's configured disks.
        let is_overlay = config.sandbox.mount_mode == "overlay";
        let mut mounts = resolve_project_mounts(&cwd, config, is_overlay, extra_mounts);
        println!("Mounts for '{name}':\n{}", describe_mounts(&mounts));

        // The host collector owns one endpoint per box. Mount its private
        // directory before the box is created so the in-guest agent has a
        // stable path even when `devbox web` (and therefore the listener)
        // starts later.
        if crate::obs::uses_host_socket(runtime.name()) {
            let obs_dir = crate::obs::collector::endpoint_dir(&self.state_dir, name);
            std::fs::create_dir_all(&obs_dir)
                .with_context(|| format!("create observability directory {}", obs_dir.display()))?;
            std::fs::set_permissions(&obs_dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| {
                    format!("protect observability directory {}", obs_dir.display())
                })?;
            mounts.push(Mount {
                host_path: obs_dir,
                container_path: "/run/devbox-host".to_string(),
                read_only: false,
            });
        }

        // A bare box is deliberately just the runtime image plus mounts and
        // resource limits. Do not tell either the runtime or persisted state
        // that tool selections were installed when guest provisioning was
        // explicitly skipped.
        let selected_sets = if bare {
            Vec::new()
        } else {
            config.active_sets()
        };
        let selected_languages = if bare {
            Vec::new()
        } else {
            config.active_languages()
        };
        let selected_packages = if bare {
            Vec::new()
        } else {
            config.custom_packages.keys().cloned().collect()
        };

        // Names *and* sources, so each provisioning path can use what it
        // needs. NixOS writes `[custom_packages]` keys that the module
        // resolves as attribute paths under `pkgs`, so it wants the key;
        // Ubuntu runs `nix profile install`, so it wants the complete
        // reference. Handing both paths the same string was wrong for one
        // of them either way — first by dropping flake packages, then by
        // turning `terraform` into a `nixpkgs#terraform` key that resolves
        // to nothing.
        let package_pairs: Vec<(String, String)> = if bare {
            Vec::new()
        } else {
            config
                .custom_packages
                .iter()
                .map(|(name, source)| (name.clone(), source.clone()))
                .collect()
        };

        // A cached image is a fully provisioned guest for exactly this
        // selection; the key covers everything provisioning bakes in, so a
        // stale cache invalidates itself. A bare box asks for no
        // provisioning and therefore never launches from, or publishes, one.
        let cache_key = provision::cache_key(
            config.sandbox.image.as_str(),
            &selected_sets,
            &selected_languages,
            &config.sandbox.mount_mode,
            &package_pairs,
        );
        let cached = if bare {
            None
        } else {
            runtime.cached_image(&cache_key).await
        };

        let opts = CreateOpts {
            name: name.to_string(),
            mounts,
            cpu: config.resources.cpu,
            memory: config.resources.memory.clone(),
            env: env_vars.clone(),
            env_file: env_file.map(|path| {
                if path.is_relative() {
                    cwd.join(path)
                } else {
                    path
                }
            }),
            sets: selected_sets.clone(),
            tools: vec![],
            bare,
            writable: config.sandbox.mount_mode == "writable",
            image: config.sandbox.image.clone(),
            cached_image: cached.clone(),
        };

        // Prepare the recovery record before the runtime side effect. Runtime
        // create methods can fail after launching an object (for example, a
        // readiness timeout or a later mount mutation), so every confirmed or
        // possible partial object needs enough local state for normal
        // Stop/Destroy recovery.
        let mut state = SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: name.to_string(),
            runtime: runtime.name().to_string(),
            project_dir: cwd,
            created_at: crate::runtime::now_rfc3339(),
            mount_mode: config.sandbox.mount_mode.clone(),
            sets: selected_sets.clone(),
            languages: selected_languages.clone(),
            image: config.sandbox.image.clone(),
            packages: selected_packages.clone(),
            package_sources: if bare {
                Default::default()
            } else {
                config
                    .custom_packages
                    .iter()
                    .filter(|(_, source)| source.as_str() != "nixpkgs")
                    .map(|(name, source)| (name.clone(), source.clone()))
                    .collect()
            },
        };

        // Create via runtime. If it reports failure, first distinguish a clean
        // pre-create failure from a partial side effect. An inconclusive probe
        // is treated conservatively: stop and register the possible object.
        let info = match runtime.create(&opts).await {
            Ok(info) => info,
            Err(create_error) => match runtime.status(name).await {
                Ok(SandboxStatus::NotFound) => return Err(create_error),
                Ok(status) => {
                    let stop_result = runtime.stop(name).await;
                    let state_result = state.save(&self.state_dir);
                    match (stop_result, state_result) {
                        (Ok(()), Ok(())) => bail!(
                            "runtime create failed after leaving a recoverable object: {create_error:#}; runtime reported {status:?}; it was stopped for safety and registered locally so `devbox destroy {name} --force` can remove it"
                        ),
                        (Err(stop_error), Ok(())) => bail!(
                            "runtime create failed after leaving a recoverable object: {create_error:#}; runtime reported {status:?}; stopping it failed: {stop_error:#}. It may still be running, but was registered locally. Run `devbox stop {name}` immediately, or `devbox destroy {name} --force` to remove it"
                        ),
                        (Ok(()), Err(state_error)) => bail!(
                            "runtime create failed after a side effect: {create_error:#}; runtime reported {status:?}; it was stopped, but recovery state could not be registered: {state_error:#}. Inspect/remove it with `{}`",
                            runtime_recovery_hint(runtime.name(), name)
                        ),
                        (Err(stop_error), Err(state_error)) => bail!(
                            "runtime create failed after a side effect: {create_error:#}; runtime reported {status:?}; stopping it failed: {stop_error:#}; recovery state could not be registered: {state_error:#}. The unregistered object may still be running; stop/remove it immediately with `{}`",
                            runtime_recovery_hint(runtime.name(), name)
                        ),
                    }
                }
                Err(status_error) => match runtime.stop(name).await {
                    Ok(()) => {
                        if let Err(state_error) = state.save(&self.state_dir) {
                            bail!(
                                "runtime create failed and status was inconclusive: {create_error:#}; status probe failed: {status_error:#}; the possible object was stopped, but recovery state could not be registered: {state_error:#}. Inspect/remove it with `{}`",
                                runtime_recovery_hint(runtime.name(), name)
                            );
                        }
                        bail!(
                            "runtime create failed and status was inconclusive: {create_error:#}; status probe failed: {status_error:#}; the possible object was stopped and registered locally so `devbox destroy {name} --force` can remove it"
                        );
                    }
                    Err(stop_error) => match state.save(&self.state_dir) {
                        Ok(()) => bail!(
                            "runtime create failed and devbox could neither verify nor stop a possible partial object: {create_error:#}; status probe failed: {status_error:#}; stop failed: {stop_error:#}. It may still be running, but was registered locally. Run `devbox stop {name}` immediately, or `devbox destroy {name} --force` to remove it"
                        ),
                        Err(state_error) => bail!(
                            "runtime create failed and devbox could neither verify nor stop a possible partial object: {create_error:#}; status probe failed: {status_error:#}; stop failed: {stop_error:#}; recovery state could not be registered: {state_error:#}. The unregistered object may still be running; stop/remove it immediately with `{}`",
                            runtime_recovery_hint(runtime.name(), name)
                        ),
                    },
                },
            },
        };
        if let Some(created_at) = info.created_at {
            state.created_at = created_at;
        }

        // Register immediately after successful creation, before provisioning
        // or any other fallible post-create operation. A later error therefore
        // leaves an inspectable, destroyable box rather than an orphan.
        if let Err(state_error) = state.save(&self.state_dir) {
            match runtime.stop(name).await {
                Ok(()) => bail!(
                    "sandbox runtime object was created but could not be registered: {state_error:#}. It was stopped for safety. Inspect/remove it with `{}`",
                    runtime_recovery_hint(runtime.name(), name)
                ),
                Err(stop_error) => bail!(
                    "sandbox runtime object was created but could not be registered: {state_error:#}; stopping it failed: {stop_error:#}. The unregistered object may still be running; stop/remove it immediately with `{}`",
                    runtime_recovery_hint(runtime.name(), name)
                ),
            }
        }

        // Registration is the durable reservation. A later create/use that
        // acquires this project claim will now see the state above and refuse.
        drop(project_claim);

        // Provision tools in the VM based on selected sets
        let active_sets = selected_sets.clone();
        let active_langs = selected_languages.clone();
        let image = config.sandbox.image.as_str();
        // Provision tools — pass mount_mode so NixOS module sets up overlay
        let mount_mode = &config.sandbox.mount_mode;

        let provisioned = if bare {
            Ok(())
        } else if cached.is_some() {
            // Launched from a cached, fully provisioned guest: skip the rebuild
            // and apply only what is specific to this box and this host.
            println!("Using cached image — skipping provisioning.");
            provision::post_cache_setup(
                runtime,
                name,
                &active_sets,
                &active_langs,
                image,
                mount_mode,
                &package_pairs,
            )
            .await
        } else {
            let result = provision::provision_vm_full_reported(
                runtime,
                name,
                &active_sets,
                &active_langs,
                image,
                mount_mode,
                &package_pairs,
                provision_reporter,
            )
            .await;
            // Publish only a guest that provisioned completely. A cache built
            // from a half-provisioned box would hand every later create the
            // same failure with a success message in front of it.
            if result.is_ok()
                && let Err(error) = runtime.cache_image(name, &cache_key).await
            {
                eprintln!("Warning: could not cache the provisioned image: {error}");
            }
            result
        };

        // A runtime object without its selected tools is not a successful
        // box. Persist it first so it remains inspectable and destroyable,
        // then stop it for safety and return the provisioning failure to both
        // CLI and web callers. The old warning-only path made the web console
        // announce "Box created" while setup had failed off-screen.
        if let Err(error) = provisioned {
            let safety = match runtime.stop(name).await {
                Ok(()) => " The incomplete box was stopped for safety.".to_string(),
                Err(stop_error) => format!(
                    " The incomplete box could not be stopped and may still be running: {stop_error}."
                ),
            };
            bail!("sandbox '{name}' exists, but provisioning did not complete: {error}.{safety}");
        }

        println!(
            "Sandbox '{}' created successfully (runtime: {})",
            name,
            runtime.name()
        );
        Ok(())
    }

    /// Claim a box, make it ready for use, and enforce its saved posture.
    ///
    /// The returned claim deliberately stays alive until the caller is ready
    /// to launch the user's command. That closes the race where `devbox use`
    /// could stop the VM and rewrite its mounts between our state read and our
    /// start. Callers must drop it immediately before the interactive process
    /// so a long-lived shell does not block legitimate lifecycle operations.
    pub(crate) async fn prepare_running_for_use(
        &self,
        name: &str,
    ) -> Result<(SandboxState, Box<dyn Runtime>, crate::web::build::BoxClaim)> {
        let (claim, state) = self.claim_and_read(name).with_context(|| {
            format!("cannot use box '{name}' while another lifecycle operation is in progress")
        })?;
        let runtime = self.runtime_for_sandbox(&state)?;

        let mut just_started = false;
        match runtime.status(name).await? {
            SandboxStatus::Running => {}
            SandboxStatus::Stopped => {
                println!("Starting sandbox '{name}'...");
                runtime.start(name).await?;
                just_started = true;
            }
            SandboxStatus::NotFound => {
                bail!(
                    "Sandbox '{}' exists in state but not in runtime '{}'. \
                     It may have been removed externally. Run `devbox destroy {}` to clean up.",
                    name,
                    state.runtime,
                    name
                );
            }
            SandboxStatus::Unreachable(reason) => {
                bail!(
                    "Sandbox '{name}' is powered on but its guest shell is unreachable: {reason}. Run `devbox stop {name}`, then try again."
                );
            }
            SandboxStatus::Unknown(status) => {
                bail!("Sandbox '{name}' is in unknown state: {status}");
            }
        }

        // Enforce for both paths. A running VM may have come back outside
        // devbox after a host reboot and therefore have no firewall installed.
        if let Err(policy_error) = crate::policy::enforce::apply_saved(self, name, &claim).await {
            match runtime.stop(name).await {
                Ok(()) => bail!(
                    "sandbox '{name}' cannot be used because its saved egress posture could not be applied: {policy_error:#}. It was stopped for safety"
                ),
                Err(stop_error) => bail!(
                    "sandbox '{name}' cannot be used because its saved egress posture could not be applied: {policy_error:#}. It could not be stopped and may still be running without that policy: {stop_error:#}. Run `devbox stop {name}` immediately"
                ),
            }
        }

        // The box may have been provisioned by an older devbox, or by a build
        // of *this* version that embedded a different agent. Both hand back a
        // box whose handshake passes and whose capture is quietly degraded,
        // because the collector compares version strings and these agents
        // share one. This is the point where the host holds the lifecycle
        // claim, so it is the point that may also regenerate the service.
        //
        // Never fatal. A box that cannot have its agent refreshed is still a
        // box the user asked to enter.
        match crate::sandbox::agent_sync::ensure_current(
            self,
            runtime.as_ref(),
            name,
            &state.image,
            crate::sandbox::agent_sync::Scope::Full,
            &claim,
        )
        .await
        {
            Ok(refresh) => {
                // Named for what actually changed. This used to say the agent
                // had been replaced whatever the reason was, so a box that was
                // only rebuilt to move its passwd home reported a swap that
                // never happened — and one whose ssh keys had just been made
                // findable again said nothing about it.
                if let Some(summary) = refresh.summary() {
                    println!("Box '{name}': {summary}.");
                }
            }
            // One failure here is not like the others. Regenerating the unit
            // rebuilds the box, and a rebuild removes its firewall; if the
            // saved posture did not come back, the box is running open while
            // every surface still says otherwise. Same fail-closed condition
            // as the apply above, and the same answer.
            Err(error) if crate::sandbox::agent_sync::lost_the_posture(&error) => {
                match runtime.stop(name).await {
                    Ok(()) => bail!(
                        "sandbox '{name}' cannot be used: {error:#}. It was stopped for safety"
                    ),
                    Err(stop_error) => bail!(
                        "sandbox '{name}' cannot be used: {error:#}. It could not be stopped and may still be running without that policy: {stop_error:#}. Run `devbox stop {name}` immediately"
                    ),
                }
            }
            Err(error) => eprintln!(
                "Warning: could not bring box '{name}'s observability agent up to date: {error:#}"
            ),
        }

        // The broker token is per box and rotated at box start (§6.3), so a
        // token that leaked out of a box stops working the next time that box
        // comes up. Rotating on every entry instead would cut off a shell that
        // is still running in the same box, which is why this is keyed on an
        // actual start.
        if (just_started || crate::broker::tokens::current(&self.state_dir, name).is_none())
            && let Err(error) = crate::broker::tokens::rotate(&self.state_dir, name)
        {
            tracing::warn!(box_id = %name, %error, "could not mint a broker token");
        }
        crate::broker::daemon::ensure_running(self);
        if just_started {
            self.refresh_guest_gitconfig(runtime.as_ref(), name).await;
            self.refresh_code_ssh_env(runtime.as_ref(), name).await;
        }

        Ok((state, runtime, claim))
    }

    /// The environment a devbox-started session gets for the credential
    /// broker — §6.3.
    ///
    /// This is the integration point: `run`, `exec`, `shell`, and `mcp run`
    /// all wrap their command with these pairs, and no other path puts them
    /// in a guest. Best effort throughout: a host with no secrets, a broker
    /// that is not running, or a box with no verified route to the host all
    /// mean "no variables", never "the command fails".
    pub async fn broker_env(&self, runtime: &dyn Runtime, name: &str) -> Vec<(String, String)> {
        crate::broker::guest_env(&self.state_dir, runtime, name).await
    }

    /// Refresh the `SetEnv` lines in this box's `devbox code` ssh block.
    ///
    /// The block is what carries the broker variables into a Remote SSH
    /// session, and the token in it is rotated by the start this is called
    /// from — so without this, the first thing a user does after restarting a
    /// box in their editor is authenticate with last session's token.
    ///
    /// Never creates a block. `devbox code` owns creating it; a box that has
    /// never been opened in an editor gets nothing written to the user's ssh
    /// configuration.
    async fn refresh_code_ssh_env(&self, runtime: &dyn Runtime, name: &str) {
        let host = format!("devbox-{name}");
        // Cheap first: reading the file and finding no block costs nothing,
        // while resolving the environment probes the guest.
        match crate::cli::code::has_managed_block(&host) {
            Ok(false) | Err(_) => return,
            Ok(true) => {}
        }
        let env = self.broker_env(runtime, name).await;
        match crate::cli::code::refresh_broker_env(&host, &env) {
            Ok(true) => tracing::debug!(box_id = %name, "refreshed the ssh broker environment"),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(box_id = %name, %error, "could not refresh the ssh broker environment")
            }
        }
    }

    /// Rewrite the devbox-managed `insteadOf` stanza in the guest gitconfig.
    ///
    /// The broker's address is a function of the runtime's host-reach *and*
    /// the port it bound, and both can change across a restart. An append
    /// would leave the stale rewrite above the fresh one, and git honours the
    /// last match — so the section is replaced, in the guest, from the guest's
    /// own copy of the file.
    async fn refresh_guest_gitconfig(&self, runtime: &dyn Runtime, name: &str) {
        let wants_github = crate::broker::configured_providers(&self.state_dir)
            .iter()
            .any(|p| p == crate::broker::providers::GITHUB);
        let base = if wants_github {
            let Some(endpoint) = crate::broker::endpoint(&self.state_dir) else {
                return;
            };
            match tokio::time::timeout(
                crate::broker::reach::REACH_TIMEOUT,
                runtime.host_reach(name, endpoint.port),
            )
            .await
            {
                Ok(Ok(reach)) => Some(reach.base_url()),
                _ => return,
            }
        } else {
            // Nothing to broker: strip any stanza an earlier configuration
            // left behind, rather than leaving git pointed at a dead address.
            None
        };

        let existing = runtime
            .exec_cmd(name, &["sh", "-c", "cat ~/.gitconfig 2>/dev/null"], false)
            .await
            .ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout)
            .unwrap_or_default();
        if base.is_none() && !existing.contains(crate::broker::GITCONFIG_BEGIN) {
            return;
        }
        let updated = crate::broker::apply_gitconfig_section(&existing, base.as_deref());
        // Base64 through argv, the same way provisioning writes guest files:
        // the content has newlines, tabs, and a URL in it, and none of that
        // survives a naive shell interpolation.
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(updated.as_bytes());
        let write = format!("printf %s '{encoded}' | base64 -d > ~/.gitconfig");
        if let Err(error) = runtime.exec_cmd(name, &["sh", "-c", &write], false).await {
            tracing::debug!(box_id = %name, %error, "could not refresh the guest gitconfig");
        }
    }

    /// Attach to a sandbox: start it if stopped, then hand the user a shell.
    ///
    /// v3 launched a Zellij session here. v4 retires the multiplexer from the
    /// default path (§5): the console is the multi-pane experience now, and
    /// `devbox shell` is a plain, predictable login shell. Anyone who wants a
    /// multiplexer can still install and run one inside the box.
    pub async fn attach(&self, name: &str) -> Result<()> {
        self.attach_with_env(name, &[]).await
    }

    /// Attach, with extra environment on the shell's command line.
    ///
    /// `extra` is how `devbox shell` gets `DEVBOX_RUN_ID` into the session: a
    /// shell is recorded as a run like anything else, and it has no wrapper to
    /// export it from.
    pub async fn attach_with_env(&self, name: &str, extra: &[(String, String)]) -> Result<()> {
        let (state, runtime, claim) = self.prepare_running_for_use(name).await?;

        // Auto-snapshot on entry (best-effort, ignore failures)
        let snap_name = format!(
            "auto-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        match runtime.snapshot_create(name, &snap_name).await {
            Ok(()) => {
                if let Err(error) = self.save_snapshot_metadata(name, &snap_name, &state) {
                    eprintln!(
                        "Warning: automatic snapshot '{snap_name}' was created, but its metadata could not be recorded: {error:#}"
                    );
                }
            }
            Err(error) => {
                tracing::debug!(box_id = %name, %error, "automatic snapshot is unavailable");
            }
        }

        // The snapshot and its metadata are one lifecycle operation. Keep the
        // claim until both have finished so stop/destroy/restore cannot race
        // the runtime snapshot or leave metadata describing another disk.
        // Do not, however, retain it through a user prompt or shell session.
        drop(claim);

        // Check for host-side changes and prompt for refresh (overlay mode only)
        if state.mount_mode != "writable" {
            Self::check_and_prompt_refresh(runtime.as_ref(), name).await;
        }

        println!("Attaching to sandbox '{name}'...");
        let shell = crate::web::service::detect_shell(runtime.as_ref(), name).await;
        let mut env = self.broker_env(runtime.as_ref(), name).await;
        env.extend_from_slice(extra);
        let cmd = crate::web::service::login_shell_command(shell, &env);
        let cmd_refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        runtime.exec_as_user(name, &cmd_refs).await?;
        Ok(())
    }

    /// Check if the host (lower layer) has changed and prompt user to refresh.
    async fn check_and_prompt_refresh(runtime: &dyn crate::runtime::Runtime, name: &str) {
        use std::io::Write;

        let changed = match overlay::lower_layer_changes(runtime, name).await {
            Ok(c) => c,
            Err(_) => return,
        };

        if changed.is_empty() {
            return;
        }

        println!(
            "\n  Host files changed since last mount ({} file{}):",
            changed.len(),
            if changed.len() == 1 { "" } else { "s" }
        );
        for (i, path) in changed.iter().enumerate() {
            if i >= 10 {
                println!("  ... and {} more", changed.len() - 10);
                break;
            }
            println!("  \x1b[33m~\x1b[0m {path}");
        }

        // Check for conflicts (files modified on both sides)
        if let Ok(conflicts) = overlay::conflicts_quiet(runtime, name).await
            && !conflicts.is_empty()
        {
            println!(
                "\n  \x1b[31m{} conflict(s)\x1b[0m (modified on both host and sandbox):",
                conflicts.len()
            );
            for c in &conflicts {
                println!("  \x1b[31m!\x1b[0m {}", c.path);
            }
            println!("  Your sandbox version takes precedence after refresh.");
        }

        print!("\n  Refresh overlay to pick up host changes? [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let answer = input.trim();
            if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
                match overlay::refresh(runtime, name).await {
                    Ok(()) => {}
                    Err(e) => eprintln!("  Warning: refresh failed: {e}"),
                }
            } else {
                println!("  Skipped. Run `devbox layer refresh` later to pick up changes.");
            }
        }
        println!();
    }

    /// Ensure a box exists for the current directory, creating one if needed.
    ///
    /// Returns the box's name. This is the half of the old `create_or_attach`
    /// that v4 still wants: the "attach" half is now the console (§5).
    pub async fn ensure_box_for_cwd(&self, tools: Option<&[String]>) -> Result<String> {
        let cwd = env::current_dir().context("Cannot determine current directory")?;
        let name = self.name_from_dir(&cwd);

        if !self.sandbox_exists(&name) {
            let runtime = self.resolve_runtime(None)?;
            let mut config = self.generate_config(&cwd);
            if let Some(t) = tools {
                config.apply_tools(t);
            }
            self.create_sandbox(
                &name,
                runtime.as_ref(),
                &config,
                &[],
                &HashMap::new(),
                None,
                false,
            )
            .await?;
        }

        Ok(name)
    }

    /// Smart default: if a sandbox exists for the current directory, attach.
    /// Otherwise, create one.
    pub async fn create_or_attach(&self, tools: Option<&[String]>) -> Result<()> {
        let cwd = env::current_dir().context("Cannot determine current directory")?;
        let name = self.name_from_dir(&cwd);

        if self.sandbox_exists(&name) {
            self.attach(&name).await
        } else {
            let runtime = self.resolve_runtime(None)?;
            let mut config = self.generate_config(&cwd);
            if let Some(t) = tools {
                config.apply_tools(t);
            }
            self.create_sandbox(
                &name,
                runtime.as_ref(),
                &config,
                &[],
                &HashMap::new(),
                None,
                false,
            )
            .await?;
            self.attach(&name).await
        }
    }

    /// Stop a sandbox.
    pub async fn stop_sandbox(&self, name: &str) -> Result<()> {
        // The same claim the console's stop takes, here because the CLI does
        // not go through it.
        //
        // `devbox stop` calls this directly, so the guard added for the console
        // protected one of the two ways to stop a box. Stopping the guest
        // mid-rebuild fails that rebuild, and its rollback of the generated
        // files and its posture restore both run *inside* the guest — so
        // neither happens. What is left is a box on a selection nobody chose
        // with its firewall down, and nothing recording either.
        let _lock = crate::web::build::claim_box(&self.state_dir, name)
            .context("cannot stop this box while a rebuild is in progress")?;

        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;
        runtime.stop(name).await?;
        println!("Sandbox '{}' stopped.", name);
        Ok(())
    }

    /// Destroy a sandbox permanently.
    /// Warns if there are uncommitted overlay changes.
    pub async fn destroy_sandbox(&self, name: &str, force: bool) -> Result<()> {
        // The same per-box claim a rebuild takes, held for the whole teardown.
        //
        // Destroying during a Sets rebuild removed the runtime and the state
        // while the detached rebuild task was still running — and that task
        // finishes by writing `state.json`, so it recreated state for a box
        // that no longer existed, and the next `create` under that name
        // collided with a record of something already destroyed.
        //
        // Here rather than in the console route, so `devbox destroy` is
        // covered by the same rule. The lock helper lives in `web::build`
        // alongside the rest of the rebuild mechanics, which the CLI paths
        // already reach into for the same reason.
        let _lock = crate::web::build::claim_box(&self.state_dir, name)?;

        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;

        // Check for uncommitted overlay changes before destroying. Both the
        // live upper layer and a saved stash are user data.
        if state.mount_mode == "overlay" && !force {
            let vm_status = runtime.status(name).await.with_context(|| {
                format!(
                    "cannot verify overlay changes for sandbox '{name}'; refusing to destroy it without --force"
                )
            })?;
            if overlay_diff_required(name, vm_status)? {
                let changes = overlay::diff(runtime.as_ref(), name).await.with_context(|| {
                    format!(
                        "cannot verify overlay changes for sandbox '{name}'; refusing to destroy it without --force"
                    )
                })?;
                let change_count = overlay::meaningful_changes(&changes).len();
                if change_count > 0 {
                    eprintln!(
                        "Warning: {} uncommitted overlay change(s) in sandbox '{}'.",
                        change_count, name
                    );
                    eprintln!("  Run `devbox layer commit {}` to save them first,", name);
                    eprintln!("  or use `devbox destroy {} --force` to discard.", name);
                    bail!("Aborting destroy due to uncommitted changes.");
                }

                let has_stash = overlay::has_stash(runtime.as_ref(), name)
                    .await
                    .with_context(|| {
                        format!(
                            "cannot verify saved overlay changes for sandbox '{name}'; refusing to destroy it without --force"
                        )
                    })?;
                if has_stash {
                    eprintln!("Warning: sandbox '{name}' has saved overlay changes in its stash.");
                    eprintln!("  Run `devbox layer stash-pop {name}` to restore them,");
                    eprintln!(
                        "  then commit them, or use `devbox destroy {name} --force` to discard."
                    );
                    bail!("Aborting destroy due to stashed changes.");
                }
            }
        }

        destroy_runtime_or_confirm_absent(runtime.as_ref(), name).await?;
        // The audit database is intentionally outside sandbox state so the
        // guest cannot write it. Destroy is therefore responsible for that
        // second tree too; otherwise a later box reusing this name inherits
        // the destroyed box's activity and behaviour history.
        // It is removed first: if that fails, state remains registered and a
        // later destroy can retry instead of making the orphan unreachable.
        self.remove_sandbox_records(name)?;
        println!("Sandbox '{}' destroyed.", name);
        Ok(())
    }

    /// Exec a one-off command in a sandbox.
    pub async fn exec_in_sandbox(
        &self,
        name: &str,
        cmd: &[String],
        interactive: bool,
    ) -> Result<i32> {
        let (_state, runtime, claim) = self.prepare_running_for_use(name).await?;

        let env = self.broker_env(runtime.as_ref(), name).await;
        let wrapped = crate::broker::with_env(&env, cmd);
        let cmd_refs: Vec<&str> = wrapped.iter().map(|s| s.as_str()).collect();
        // The command can be arbitrarily long-lived. The claim protects the
        // preparation boundary, not the process lifetime.
        drop(claim);
        let result = runtime.exec_cmd(name, &cmd_refs, interactive).await?;

        // For non-interactive commands, print captured output
        if !interactive {
            if !result.stdout.is_empty() {
                print!("{}", result.stdout);
            }
            if !result.stderr.is_empty() {
                eprint!("{}", result.stderr);
            }
        }

        Ok(result.exit_code)
    }

    /// Prune all stopped sandboxes.
    pub async fn prune_sandboxes(&self, discard_guest_data: bool) -> Result<usize> {
        let sandboxes = self.list_sandboxes()?;
        let mut removed = 0;

        for state in &sandboxes {
            let Some(_claim) = crate::web::build::try_claim_box(&self.state_dir, &state.name)?
            else {
                eprintln!(
                    "Skipping '{}': another devbox process is rebuilding it.",
                    state.name
                );
                continue;
            };
            let runtime = match self.runtime_for_sandbox(state) {
                Ok(r) => r,
                Err(error) => {
                    eprintln!(
                        "Skipping '{}': runtime is unavailable, so its guest cannot be confirmed absent: {error}",
                        state.name
                    );
                    continue;
                }
            };

            let status = match runtime.status(&state.name).await {
                Ok(status) => status,
                Err(error) => {
                    eprintln!(
                        "Skipping '{}': could not determine runtime status: {error}",
                        state.name
                    );
                    continue;
                }
            };
            match status {
                SandboxStatus::Stopped => {
                    if state.mount_mode == "overlay" && !discard_guest_data {
                        eprintln!(
                            "Skipping '{}': stopped overlay and stash data cannot be inspected; explicit discard authorization is required.",
                            state.name
                        );
                        continue;
                    }
                    if let Err(error) =
                        destroy_runtime_or_confirm_absent(runtime.as_ref(), &state.name).await
                    {
                        eprintln!("Skipping '{}': {error:#}", state.name);
                        continue;
                    }
                }
                SandboxStatus::NotFound => {}
                _ => continue,
            }

            if let Err(error) = self.remove_sandbox_records(&state.name) {
                eprintln!("Skipping '{}': cleanup failed: {error}", state.name);
                continue;
            }
            println!("Pruned '{}'", state.name);
            removed += 1;
        }

        Ok(removed)
    }

    /// Remove the external audit tree before the registry entry.
    ///
    /// If audit cleanup fails, keeping state makes the failure discoverable
    /// and retryable. Reversing the order strands data under a name no command
    /// can list, and the next box with that name adopts it.
    fn remove_sandbox_records(&self, name: &str) -> Result<()> {
        // A token outliving its box would authenticate the next box created
        // under that name into the previous one's audit trail.
        crate::broker::tokens::forget(&self.state_dir, name);
        crate::obs::collector::remove_box_data(&self.state_dir, name)?;
        SandboxState::remove(&self.state_dir, name)
    }

    /// Persist the host-side selection that belongs to a runtime snapshot.
    ///
    /// Runtime snapshots roll back `/etc/devbox/devbox-state.toml` as well as
    /// packages and files. Keeping the matching host state is what lets a
    /// later restore reconcile the console's selection with the restored
    /// guest instead of claiming that the pre-restore selection is installed.
    pub fn save_snapshot_metadata(
        &self,
        name: &str,
        snapshot: &str,
        state: &SandboxState,
    ) -> Result<()> {
        let path = self.snapshot_metadata_path(name, snapshot)?;
        let parent = path.parent().context("snapshot metadata has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create snapshot metadata directory {}", parent.display()))?;
        let mut recorded = state.clone();
        if recorded.schema < state::SCHEMA {
            let project = DevboxConfig::load_for_edit(&recorded.project_dir).context(
                "read the project configuration needed to migrate legacy snapshot metadata",
            )?;
            let selection =
                crate::nix::compose::Selection::from_state_and_project(&recorded, &project);
            recorded.packages = selection.packages.into_iter().collect();
            recorded.package_sources = selection.sources;
        }
        recorded.schema = state::SCHEMA;
        let content =
            serde_json::to_string_pretty(&recorded).context("serialize snapshot metadata")?;
        let pending = parent.join(format!(".{snapshot}.{}.pending", std::process::id()));
        std::fs::write(&pending, content)
            .with_context(|| format!("write snapshot metadata {}", pending.display()))?;
        std::fs::rename(&pending, &path)
            .with_context(|| format!("publish snapshot metadata {}", path.display()))?;
        Ok(())
    }

    /// Load the host-side state recorded when a snapshot was created.
    pub fn load_snapshot_metadata(&self, name: &str, snapshot: &str) -> Result<SandboxState> {
        let path = self.snapshot_metadata_path(name, snapshot)?;
        let content = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "snapshot '{snapshot}' has no matching devbox metadata at {}; refusing to restore it because the host selection could not be reconciled",
                path.display()
            )
        })?;
        serde_json::from_str(&content)
            .with_context(|| format!("parse snapshot metadata {}", path.display()))
    }

    fn snapshot_metadata_path(&self, name: &str, snapshot: &str) -> Result<PathBuf> {
        if !state::is_safe_name(name) || !state::is_safe_name(snapshot) {
            bail!(
                "snapshot and sandbox names must be 1-64 characters, not path components, and free of control characters"
            );
        }
        Ok(self
            .state_dir
            .join("sandboxes")
            .join(name)
            .join("snapshots")
            .join(format!("{snapshot}.json")))
    }

    // ── Naming ──────────────────────────────────────────

    /// Derive sandbox name from directory.
    pub fn name_from_dir(&self, dir: &Path) -> String {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("devbox")
            .to_string()
    }

    /// Resolve sandbox name: explicit name, or derive from current directory.
    pub fn resolve_name(&self, name: Option<&str>) -> Result<String> {
        match name {
            Some(n) => Ok(n.to_string()),
            None => {
                let cwd = env::current_dir().context("Cannot determine current directory")?;
                Ok(self.name_from_dir(&cwd))
            }
        }
    }

    // ── Registry ────────────────────────────────────────

    /// Check if a sandbox with this name exists in state.
    pub fn sandbox_exists(&self, name: &str) -> bool {
        self.state_dir.join("sandboxes").join(name).exists()
    }

    /// Load a sandbox's state by name.
    pub fn get_sandbox(&self, name: &str) -> Result<SandboxState> {
        SandboxState::load(&self.state_dir, name)
    }

    /// List all registered sandboxes.
    pub fn list_sandboxes(&self) -> Result<Vec<SandboxState>> {
        SandboxState::list_all(&self.state_dir)
    }

    /// Find a sandbox by its project directory.
    pub fn find_by_project_dir(&self, dir: &Path) -> Result<Option<SandboxState>> {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let sandboxes = self.list_sandboxes()?;
        Ok(sandboxes.into_iter().find(|s| {
            s.project_dir
                .canonicalize()
                .unwrap_or_else(|_| s.project_dir.clone())
                == canonical
        }))
    }

    /// Check if another sandbox already mounts this directory.
    pub fn check_mount_conflict(&self, dir: &Path) -> Result<Option<String>> {
        self.check_mount_conflict_except(dir, None)
    }

    /// Check if a sandbox other than `except_name` already mounts a directory.
    ///
    /// The caller must hold the directory's project claim when this result is
    /// used to authorize a later mutation; otherwise two different box claims
    /// can both observe the directory as free.
    pub fn check_mount_conflict_except(
        &self,
        dir: &Path,
        except_name: Option<&str>,
    ) -> Result<Option<String>> {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        Ok(self
            .list_sandboxes()?
            .into_iter()
            .find(|existing| {
                Some(existing.name.as_str()) != except_name
                    && existing
                        .project_dir
                        .canonicalize()
                        .unwrap_or_else(|_| existing.project_dir.clone())
                        == canonical
            })
            .map(|existing| existing.name))
    }

    // ── Config ──────────────────────────────────────────

    /// Load global config from ~/.devbox/config.toml.
    pub fn load_global_config(&self) -> Result<GlobalConfig> {
        GlobalConfig::load(&self.state_dir)
    }

    /// Save global config.
    pub fn save_global_config(&self, config: &GlobalConfig) -> Result<()> {
        config.save(&self.state_dir)
    }

    /// Generate a DevboxConfig for a directory with auto-detection.
    pub fn generate_config(&self, dir: &Path) -> DevboxConfig {
        let mut config = DevboxConfig::default();
        let detected = detect_languages(dir);

        config.languages.go = detected.go;
        config.languages.rust = detected.rust;
        config.languages.python = detected.python;
        config.languages.node = detected.node;
        config.languages.java = detected.java;
        config.languages.ruby = detected.ruby;

        // Apply global defaults if available
        if let Ok(global) = self.load_global_config()
            && global.default.runtime != "auto"
        {
            config.sandbox.runtime = global.default.runtime;
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_fails_closed_when_an_existing_overlay_cannot_be_inspected() {
        assert!(overlay_diff_required("box", SandboxStatus::Running).unwrap());
        assert!(!overlay_diff_required("box", SandboxStatus::NotFound).unwrap());

        for status in [
            SandboxStatus::Stopped,
            SandboxStatus::Unreachable("ssh refused".into()),
            SandboxStatus::Unknown("starting".into()),
        ] {
            let error = overlay_diff_required("box", status)
                .unwrap_err()
                .to_string();
            assert!(error.contains("cannot verify overlay changes"));
            assert!(error.contains("--force"));
        }
    }

    #[test]
    fn multipass_recovery_hint_only_purges_the_named_instance() {
        let hint = runtime_recovery_hint("multipass", "alpha");
        assert_eq!(
            hint,
            "multipass stop devbox-alpha; multipass delete devbox-alpha --purge"
        );
        assert!(!hint.ends_with("multipass purge"));
    }

    #[test]
    fn mount_conflict_can_exclude_only_the_box_being_switched() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().join("state"),
        };
        SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: "owner".into(),
            runtime: "lima".into(),
            project_dir: project.clone(),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            package_sources: Default::default(),
        }
        .save(&manager.state_dir)
        .unwrap();

        assert_eq!(
            manager.check_mount_conflict(&project).unwrap(),
            Some("owner".into())
        );
        assert_eq!(
            manager
                .check_mount_conflict_except(&project, Some("other"))
                .unwrap(),
            Some("owner".into())
        );
        assert_eq!(
            manager
                .check_mount_conflict_except(&project, Some("owner"))
                .unwrap(),
            None
        );

        // A legacy/inconsistent registry may already contain duplicates. Do
        // not let finding the excluded current box first hide another owner.
        SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: "other-owner".into(),
            runtime: "incus".into(),
            project_dir: project.clone(),
            created_at: String::new(),
            mount_mode: "writable".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            package_sources: Default::default(),
        }
        .save(&manager.state_dir)
        .unwrap();
        assert_eq!(
            manager
                .check_mount_conflict_except(&project, Some("owner"))
                .unwrap(),
            Some("other-owner".into())
        );
    }

    #[test]
    fn create_contract_exposes_only_implemented_runtime_shapes() {
        let mut config = DevboxConfig::default();
        assert!(validate_create_contract("lima", &config, false).is_ok());
        assert!(validate_create_contract("incus", &config, false).is_ok());
        assert!(validate_create_contract("multipass", &config, false).is_err());
        assert!(validate_create_contract("docker", &config, false).is_err());

        config.sandbox.image = "ubuntu".into();
        assert!(validate_create_contract("lima", &config, false).is_err());
        config.sandbox.mount_mode = "writable".into();
        assert!(validate_create_contract("lima", &config, false).is_ok());
        assert!(validate_create_contract("docker", &config, false).is_err());
        assert!(validate_create_contract("docker", &config, true).is_ok());
    }

    #[test]
    fn create_and_use_share_project_relative_mount_resolution() {
        let mut config = DevboxConfig::default();
        config.mounts.insert(
            "cache".into(),
            config::MountEntry {
                host: "var/cache".into(),
                target: "/cache".into(),
                readonly: true,
            },
        );
        let extra = Mount {
            host_path: PathBuf::from("artifacts"),
            container_path: "/artifacts".into(),
            read_only: false,
        };

        let mounts = resolve_project_mounts(Path::new("/project"), &config, true, &[extra]);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].host_path, PathBuf::from("/project/var/cache"));
        assert_eq!(mounts[0].container_path, "/cache");
        assert!(mounts[0].read_only);
        assert_eq!(mounts[1].host_path, PathBuf::from("/project"));
        assert_eq!(mounts[1].container_path, "/mnt/host");
        assert!(mounts[1].read_only);
        assert_eq!(mounts[2].host_path, PathBuf::from("/project/artifacts"));
    }

    /// The mount list is the one part of a create nobody can check afterwards
    /// without entering the box, so it is printed — and the overlay's lower
    /// layer is named for where the user will look for it.
    #[test]
    fn the_mount_summary_names_the_path_the_user_will_look_in() {
        let summary = describe_mounts(&[
            Mount {
                host_path: PathBuf::from("/project"),
                container_path: "/mnt/host".to_string(),
                read_only: true,
            },
            Mount {
                host_path: PathBuf::from("/project/var/cache"),
                container_path: "/cache".to_string(),
                read_only: false,
            },
        ]);
        assert!(
            summary.contains("/project → /workspace (read-only lower layer) (ro)"),
            "{summary}"
        );
        assert!(
            summary.contains("/project/var/cache → /cache (rw)"),
            "{summary}"
        );
        assert!(!summary.contains("→ /mnt/host"), "{summary}");
    }

    /// A box with nothing mounted is legal — an explicit empty `[mounts]`
    /// asks for it — but it must not be reported as though it were normal.
    #[test]
    fn a_box_with_nothing_mounted_says_so_in_words() {
        let summary = describe_mounts(&[]);
        assert!(summary.contains("no project files"), "{summary}");
        assert!(summary.contains("[mounts]"), "{summary}");
    }

    use crate::runtime::{ExecResult, SandboxInfo, SnapshotInfo};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A runtime that makes nothing and remembers whether it was asked to.
    ///
    /// The lifecycle in `create_sandbox` has never had a test, because every
    /// implementation of this trait shells out to a real hypervisor. That is
    /// the same "no test can reach it" pattern that has now produced a serious
    /// defect in the nft path, the bootstrap script, and here — so this is the
    /// smallest thing that makes the *ordering* observable, which is the part
    /// that keeps being wrong.
    struct RecordingRuntime {
        created: AtomicBool,
        stopped: AtomicBool,
        destroyed: AtomicBool,
        fail_create: bool,
        fail_destroy: bool,
        fail_stop: bool,
        fail_status: bool,
        status: SandboxStatus,
    }

    #[async_trait::async_trait]
    impl Runtime for RecordingRuntime {
        fn name(&self) -> &str {
            "recording"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn priority(&self) -> u32 {
            0
        }
        async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo> {
            self.created.store(true, Ordering::SeqCst);
            if self.fail_create {
                bail!("runtime failed after launch");
            }
            Ok(SandboxInfo {
                name: opts.name.clone(),
                status: SandboxStatus::Running,
                runtime: "recording".into(),
                created_at: Some("now".into()),
                ip_address: None,
            })
        }
        // Model the way a real runtime reports a guest-command failure: the
        // subprocess launched successfully, but its exit status is non-zero.
        // A transport-level `Err` would miss the failure channel that used to
        // be downgraded to a warning by the provisioner.
        async fn exec_cmd(&self, _: &str, _: &[&str], _: bool) -> Result<ExecResult> {
            Ok(ExecResult {
                exit_code: 1,
                stdout: String::new(),
                stderr: "guest command failed".into(),
            })
        }
        // Nothing below is reachable in these tests.
        async fn start(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn stop(&self, _: &str) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            if self.fail_stop {
                bail!("runtime refused stop");
            }
            Ok(())
        }
        fn argv(&self, _: &str, _: &[&str], _: bool) -> Vec<String> {
            unimplemented!()
        }
        async fn destroy(&self, _: &str) -> Result<()> {
            self.destroyed.store(true, Ordering::SeqCst);
            if self.fail_destroy {
                bail!("runtime refused delete");
            }
            Ok(())
        }
        async fn status(&self, _: &str) -> Result<SandboxStatus> {
            if self.fail_status {
                bail!("runtime status unavailable");
            }
            Ok(self.status.clone())
        }
        async fn list(&self) -> Result<Vec<SandboxInfo>> {
            unimplemented!()
        }
        async fn snapshot_create(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn snapshot_restore(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn snapshot_list(&self, _: &str) -> Result<Vec<SnapshotInfo>> {
            unimplemented!()
        }
        async fn upgrade(&self, _: &str, _: &[String]) -> Result<()> {
            unimplemented!()
        }
        async fn update_mounts(&self, _: &str, _: &[Mount]) -> Result<crate::runtime::MountUpdate> {
            unimplemented!()
        }
        async fn rollback_mounts(&self, _: &str, _: &crate::runtime::MountUpdate) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn failed_runtime_destroy_is_not_success_while_the_guest_still_exists() {
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: true,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Stopped,
        };

        let error = destroy_runtime_or_confirm_absent(&runtime, "box")
            .await
            .expect_err("a failed delete of an extant guest must remain a failure");
        assert!(runtime.destroyed.load(Ordering::SeqCst));
        assert!(error.to_string().contains("local state was preserved"));
    }

    #[tokio::test]
    async fn failed_runtime_destroy_is_success_when_the_guest_is_already_absent() {
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: true,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::NotFound,
        };

        destroy_runtime_or_confirm_absent(&runtime, "box")
            .await
            .expect("an already absent guest is safe to forget");
        assert!(runtime.destroyed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_partial_runtime_create_is_stopped_and_registered_for_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: true,
            fail_destroy: false,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Running,
        };

        let error = manager
            .create_sandbox(
                "partial",
                &runtime,
                &DevboxConfig::default(),
                &[],
                &HashMap::new(),
                None,
                true,
            )
            .await
            .expect_err("a runtime error after launch is not a successful create");

        assert!(runtime.created.load(Ordering::SeqCst));
        assert!(runtime.stopped.load(Ordering::SeqCst));
        assert!(manager.get_sandbox("partial").is_ok());
        assert!(error.to_string().contains("registered locally"));
    }

    #[tokio::test]
    async fn an_uncertain_unstoppable_partial_create_is_still_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: true,
            fail_destroy: false,
            fail_stop: true,
            fail_status: true,
            status: SandboxStatus::Unknown("not observable".into()),
        };

        let error = manager
            .create_sandbox(
                "uncertain",
                &runtime,
                &DevboxConfig::default(),
                &[],
                &HashMap::new(),
                None,
                true,
            )
            .await
            .expect_err("an uncertain runtime side effect cannot be successful");

        assert!(runtime.created.load(Ordering::SeqCst));
        assert!(runtime.stopped.load(Ordering::SeqCst));
        assert!(manager.get_sandbox("uncertain").is_ok());
        let message = error.to_string();
        assert!(message.contains("may still be running"));
        assert!(message.contains("registered locally"));
        assert!(!message.contains("stopped for safety"));
    }

    #[test]
    fn config_and_state_update_rolls_back_config_when_state_save_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let config_path = project.path().join("devbox.toml");
        let original = b"# exact original bytes\n[sandbox]\nruntime = \"docker\"\n";
        std::fs::write(&config_path, original).unwrap();

        let state = SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: "..".into(), // rejected by the state writer after config save
            runtime: "docker".into(),
            project_dir: project.path().to_path_buf(),
            created_at: "now".into(),
            mount_mode: "writable".into(),
            sets: vec![],
            languages: vec![],
            image: "ubuntu".into(),
            packages: vec![],
            package_sources: Default::default(),
        };

        manager
            .save_config_and_state(&DevboxConfig::default(), &state)
            .expect_err("invalid state must fail after the config write");
        assert_eq!(std::fs::read(config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn an_unsupported_package_is_refused_before_the_box_is_made() {
        // This check has been moved twice and been wrong twice. It started
        // inside `provision_vm_full`, where `create` downgraded the failure to
        // a printed warning and then saved state and reported success. Round 23
        // moved it up — but to below `runtime.create`, while writing a comment
        // that said "before the box exists". The comment was the only thing
        // that made it look fixed.
        //
        // What that left behind is worse than the original warning: a VM with
        // no state file, which `sandbox_exists` cannot see, so the obvious
        // retry walks into a collision with a runtime object nobody is
        // tracking. So the assertion here is not "it returns an error" — it
        // did that before — but that nothing was created.
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: false,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Running,
        };

        let mut config = DevboxConfig::default();
        config.sandbox.image = "nixos".into();
        config.custom_packages.insert(
            "my-tool".into(),
            "github:someone/their-flake#my-tool".into(),
        );

        let err = manager
            .create_sandbox("t", &runtime, &config, &[], &HashMap::new(), None, false)
            .await
            .expect_err("a flake package on the NixOS image must be refused");
        assert!(
            err.to_string().contains("flake"),
            "the refusal should say why: {err}"
        );
        assert!(
            !runtime.created.load(Ordering::SeqCst),
            "the box was created before the package check ran, so the refusal \
             leaves an orphan runtime object behind"
        );
        assert!(
            !tmp.path().join("boxes").join("t").exists(),
            "no state should be written for a box that was refused"
        );
    }

    #[tokio::test]
    async fn an_unsaveable_name_is_refused_before_the_box_is_made() {
        // The third check to need moving above `runtime.create`, and the same
        // shape as the two before it: `save` enforces the 64-character limit
        // and `save` runs last, so Docker happily built a container under a
        // 65-character name that devbox then refused to record. That leaves a
        // live runtime object with no state file — invisible to
        // `sandbox_exists`, so the retry collides with it, and invisible to
        // `destroy`, which looks names up in state.
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: false,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Running,
        };

        let too_long = "n".repeat(65);
        let err = manager
            .create_sandbox(
                &too_long,
                &runtime,
                &DevboxConfig::default(),
                &[],
                &HashMap::new(),
                None,
                false,
            )
            .await
            .expect_err("a name that cannot be saved must not be created");
        assert!(
            err.to_string().contains("1-64"),
            "the refusal should say what the limit is: {err}"
        );
        assert!(
            !runtime.created.load(Ordering::SeqCst),
            "the runtime object was made under a name nothing can clean up"
        );
    }

    #[tokio::test]
    async fn a_supported_package_still_reaches_the_runtime() {
        // The other half of the same question. A check that runs early is only
        // an improvement if it still lets the ordinary case through — the
        // round-20 firewall guard refused every legitimate call and made the
        // outcome it was preventing unreachable, which is the failure this
        // asserts against.
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: false,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Running,
        };

        let mut config = DevboxConfig::default();
        config.sandbox.image = "nixos".into();
        config
            .custom_packages
            .insert("ripgrep".into(), "nixpkgs".into());

        // Provisioning fails against this runtime after create. The runtime
        // must remain registered for inspection/cleanup, but the overall
        // result is an error and the incomplete box is stopped for safety.
        let error = manager
            .create_sandbox("t", &runtime, &config, &[], &HashMap::new(), None, false)
            .await
            .expect_err("incomplete provisioning must not be reported as success");
        assert!(
            runtime.created.load(Ordering::SeqCst),
            "a package that NixOS can install must not be refused"
        );
        assert!(error.to_string().contains("provisioning did not complete"));
        assert!(runtime.stopped.load(Ordering::SeqCst));
        assert!(manager.get_sandbox("t").is_ok());
    }

    #[tokio::test]
    async fn a_bare_box_skips_guest_provisioning_and_records_only_the_base_image() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let runtime = RecordingRuntime {
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            fail_create: false,
            fail_destroy: false,
            fail_stop: false,
            fail_status: false,
            status: SandboxStatus::Running,
        };

        // Defaults select several sets, and this source is unsupported by the
        // NixOS provisioner. Neither may matter when the user explicitly asks
        // for the unmodified base image.
        let mut config = DevboxConfig::default();
        config.custom_packages.insert(
            "my-tool".into(),
            "github:someone/their-flake#my-tool".into(),
        );

        manager
            .create_sandbox("bare", &runtime, &config, &[], &HashMap::new(), None, true)
            .await
            .expect("bare creation must not execute guest provisioning commands");

        assert!(runtime.created.load(Ordering::SeqCst));
        assert!(!runtime.stopped.load(Ordering::SeqCst));
        let state = manager.get_sandbox("bare").unwrap();
        assert!(state.sets.is_empty());
        assert!(state.languages.is_empty());
        assert!(state.packages.is_empty());
        assert!(state.package_sources.is_empty());
    }

    #[tokio::test]
    async fn prune_preserves_records_when_the_runtime_cannot_be_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: "orphan".into(),
            runtime: "missing-runtime".into(),
            project_dir: tmp.path().join("project"),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            package_sources: Default::default(),
        }
        .save(tmp.path())
        .unwrap();
        let database = crate::obs::collector::store_path(tmp.path(), "orphan");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(&database, b"old timeline").unwrap();

        assert_eq!(manager.prune_sandboxes(false).await.unwrap(), 0);
        assert!(manager.get_sandbox("orphan").is_ok());
        assert!(database.exists());
    }

    #[test]
    fn snapshot_metadata_round_trips_and_cannot_escape_the_box_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: tmp.path().to_path_buf(),
        };
        let state = SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: "box".into(),
            runtime: "lima".into(),
            project_dir: tmp.path().join("project"),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec!["system".into()],
            languages: vec!["rust".into()],
            image: "nixos".into(),
            packages: vec!["ripgrep".into()],
            package_sources: Default::default(),
        };

        manager
            .save_snapshot_metadata("box", "before-edit", &state)
            .unwrap();
        let loaded = manager
            .load_snapshot_metadata("box", "before-edit")
            .unwrap();
        assert_eq!(loaded.sets, state.sets);
        assert_eq!(loaded.languages, state.languages);
        assert_eq!(loaded.packages, state.packages);

        assert!(
            manager
                .save_snapshot_metadata("box", "../escape", &state)
                .is_err()
        );
        assert!(!tmp.path().join("escape.json").exists());
    }
}
