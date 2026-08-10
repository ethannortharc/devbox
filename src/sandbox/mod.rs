pub mod config;
pub mod global_config;
pub mod overlay;
pub mod provision;
pub mod state;

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::config::DevboxConfig;
use self::global_config::GlobalConfig;
use self::state::SandboxState;
use crate::runtime::detect::{detect_runtime, select_runtime};
use crate::runtime::{CreateOpts, Mount, Runtime, SandboxStatus};
use crate::tools::detect::detect_languages;

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

        // Check for name conflicts
        if self.sandbox_exists(name) {
            bail!(
                "Sandbox '{}' already exists. Use `devbox destroy {}` first.",
                name,
                name
            );
        }

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
        crate::sandbox::provision::check_packages_supported(
            &config.sandbox.image,
            &config
                .custom_packages
                .iter()
                .map(|(n, s)| (n.clone(), s.clone()))
                .collect::<Vec<_>>(),
        )?;

        // Build mounts from config + extra
        let is_overlay = config.sandbox.mount_mode == "overlay";
        let mut mounts: Vec<Mount> = config
            .mounts
            .values()
            .map(|m| {
                let host = if m.host == "." {
                    cwd.clone()
                } else {
                    PathBuf::from(&m.host)
                };
                // In overlay mode, redirect the workspace mount to /mnt/host
                // and force it read-only. The OverlayFS mount will provide
                // /workspace as a writable overlay on top.
                let (container_path, read_only) = if is_overlay && m.target == "/workspace" {
                    ("/mnt/host".to_string(), true)
                } else {
                    (m.target.clone(), m.readonly)
                };
                Mount {
                    host_path: host,
                    container_path,
                    read_only,
                }
            })
            .collect();
        mounts.extend_from_slice(extra_mounts);

        let opts = CreateOpts {
            name: name.to_string(),
            mounts,
            cpu: config.resources.cpu,
            memory: config.resources.memory.clone(),
            env: env_vars.clone(),
            env_file,
            sets: config.active_sets(),
            tools: vec![],
            bare,
            writable: config.sandbox.mount_mode == "writable",
            image: config.sandbox.image.clone(),
        };

        // Create via runtime
        let info = runtime.create(&opts).await?;

        // Provision tools in the VM based on selected sets
        let active_sets = config.active_sets();
        let active_langs = config.active_languages();
        let image = config.sandbox.image.as_str();
        // Provision tools — pass mount_mode so NixOS module sets up overlay
        let mount_mode = &config.sandbox.mount_mode;

        if let Err(e) = provision::provision_vm_full(
            runtime,
            name,
            &active_sets,
            &active_langs,
            image,
            mount_mode,
            // Names *and* sources, so each provisioning path can use what it
            // needs. NixOS writes `[custom_packages]` keys that the module
            // resolves as attribute paths under `pkgs`, so it wants the key;
            // Ubuntu runs `nix profile install`, so it wants the complete
            // reference. Handing both paths the same string was wrong for one
            // of them either way — first by dropping flake packages, then by
            // turning `terraform` into a `nixpkgs#terraform` key that resolves
            // to nothing.
            &config
                .custom_packages
                .iter()
                .map(|(name, source)| (name.clone(), source.clone()))
                .collect::<Vec<_>>(),
        )
        .await
        {
            eprintln!("Warning: provisioning incomplete: {e}");
        }

        // Save state
        let state = SandboxState {
            schema: crate::sandbox::state::SCHEMA,
            name: name.to_string(),
            runtime: runtime.name().to_string(),
            project_dir: cwd,
            created_at: info.created_at.unwrap_or_default(),
            mount_mode: config.sandbox.mount_mode.clone(),
            sets: config.active_sets(),
            languages: config.active_languages(),
            image: config.sandbox.image.clone(),
            // Every declared package, whatever its source. This is devbox's
            // record of what the box is *meant* to have; filtering by source
            // here would make a flake-sourced package vanish from `devbox
            // list` and from the Sets checklist.
            packages: config.custom_packages.keys().cloned().collect(),
            // Recorded on the box, so a later `devbox use` cannot lose it.
            package_sources: config
                .custom_packages
                .iter()
                .filter(|(_, source)| source.as_str() != "nixpkgs")
                .map(|(name, source)| (name.clone(), source.clone()))
                .collect(),
        };
        state.save(&self.state_dir)?;

        println!(
            "Sandbox '{}' created successfully (runtime: {})",
            name,
            runtime.name()
        );
        Ok(())
    }

    /// Attach to a sandbox: start it if stopped, then hand the user a shell.
    ///
    /// v3 launched a Zellij session here. v4 retires the multiplexer from the
    /// default path (§5): the console is the multi-pane experience now, and
    /// `devbox shell` is a plain, predictable login shell. Anyone who wants a
    /// multiplexer can still install and run one inside the box.
    pub async fn attach(&self, name: &str) -> Result<()> {
        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;

        // Check status, start if stopped
        let status = runtime.status(name).await?;
        match status {
            // Running too. `create_sandbox` hands back a box that is already
            // up and goes straight to attach, so gating enforcement on the
            // Stopped arm meant a brand-new box with `isolated` in its
            // devbox.toml ran unrestricted until its first restart. Applying
            // is idempotent — the ruleset destroys its table before rebuilding
            // it — so doing it on every attach costs one exec and closes the
            // window for good.
            SandboxStatus::Running => {
                crate::policy::enforce::apply_saved_or_step_aside(self, name).await?;
            }
            SandboxStatus::Stopped => {
                println!("Starting sandbox '{name}'...");
                runtime.start(name).await?;
                crate::policy::enforce::apply_saved_or_step_aside(self, name).await?;
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
            SandboxStatus::Unknown(s) => {
                bail!("Sandbox '{}' is in unknown state: {}", name, s);
            }
        }

        // Auto-snapshot on entry (best-effort, ignore failures)
        let snap_name = format!(
            "auto-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        if let Err(e) = runtime.snapshot_create(name, &snap_name).await {
            let _ = e;
        }

        // Check for host-side changes and prompt for refresh (overlay mode only)
        if state.mount_mode != "writable" {
            Self::check_and_prompt_refresh(runtime.as_ref(), name).await;
        }

        println!("Attaching to sandbox '{name}'...");
        let shell = Self::probe_shell(runtime.as_ref(), name).await;
        runtime.exec_cmd(name, &[&shell, "-l"], true).await?;
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

    /// Probe for zsh in the VM, fall back to bash.
    async fn probe_shell(runtime: &dyn crate::runtime::Runtime, name: &str) -> String {
        let probe = runtime.exec_cmd(name, &["which", "zsh"], false).await;
        if probe.is_ok() && probe.unwrap().exit_code == 0 {
            "zsh".to_string()
        } else {
            "bash".to_string()
        }
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

        let state = self.get_sandbox(name);
        if let Ok(state) = &state {
            let runtime = self.runtime_for_sandbox(state)?;

            // Check for uncommitted overlay changes before destroying
            if state.mount_mode == "overlay" && !force {
                let vm_status = runtime
                    .status(name)
                    .await
                    .unwrap_or(SandboxStatus::NotFound);
                if vm_status == SandboxStatus::Running {
                    let changes = overlay::diff(runtime.as_ref(), name).await;
                    if let Ok(changes) = changes {
                        let file_count = changes.iter().filter(|c| !c.is_dir).count();
                        if file_count > 0 {
                            eprintln!(
                                "Warning: {} uncommitted overlay change(s) in sandbox '{}'.",
                                file_count, name
                            );
                            eprintln!(
                                "  Run `devbox layer commit --name {}` to save them first,",
                                name
                            );
                            eprintln!(
                                "  or use `devbox destroy --force --name {}` to discard.",
                                name
                            );
                            bail!("Aborting destroy due to uncommitted changes.");
                        }
                    }
                }
            }

            // Attempt runtime destroy (may fail if already removed)
            if let Err(e) = runtime.destroy(name).await {
                eprintln!("Warning: runtime destroy failed: {e}");
            }
        }
        // Always clean up state
        SandboxState::remove(&self.state_dir, name)?;
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
        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;

        // Start if stopped
        let status = runtime.status(name).await?;
        if status == SandboxStatus::NotFound {
            bail!("Sandbox '{}' not found in runtime", name);
        }
        if status == SandboxStatus::Stopped {
            runtime.start(name).await?;
        }
        // Whether or not this call started it. A box already running may have
        // been started outside devbox, and running a command in it is exactly
        // the moment its posture has to be true.
        crate::policy::enforce::apply_saved_or_step_aside(self, name).await?;

        let cmd_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
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
    pub async fn prune_sandboxes(&self) -> Result<usize> {
        let sandboxes = self.list_sandboxes()?;
        let mut removed = 0;

        for state in &sandboxes {
            let runtime = match self.runtime_for_sandbox(state) {
                Ok(r) => r,
                Err(_) => {
                    // Runtime not available, just remove state
                    SandboxState::remove(&self.state_dir, &state.name)?;
                    removed += 1;
                    continue;
                }
            };

            let status = runtime
                .status(&state.name)
                .await
                .unwrap_or(SandboxStatus::NotFound);
            if matches!(status, SandboxStatus::Stopped | SandboxStatus::NotFound) {
                if let Err(e) = runtime.destroy(&state.name).await {
                    eprintln!("Warning: failed to destroy '{}': {e}", state.name);
                }
                SandboxState::remove(&self.state_dir, &state.name)?;
                println!("Pruned '{}'", state.name);
                removed += 1;
            }
        }

        Ok(removed)
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
        if let Some(existing) = self.find_by_project_dir(dir)? {
            return Ok(Some(existing.name));
        }
        Ok(None)
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
            Ok(SandboxInfo {
                name: opts.name.clone(),
                status: SandboxStatus::Running,
                runtime: "recording".into(),
                created_at: Some("now".into()),
                ip_address: None,
            })
        }
        // Provisioning runs commands in the box it just made. There is no box,
        // so this fails — which is the honest answer and the one `create`
        // already knows how to handle: it prints the failure as a warning.
        async fn exec_cmd(&self, _: &str, _: &[&str], _: bool) -> Result<ExecResult> {
            bail!("no box to run commands in")
        }
        // Nothing below is reachable in these tests.
        async fn start(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn stop(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn argv(&self, _: &str, _: &[&str], _: bool) -> Vec<String> {
            unimplemented!()
        }
        async fn destroy(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn status(&self, _: &str) -> Result<SandboxStatus> {
            unimplemented!()
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
        async fn update_mounts(&self, _: &str, _: &[Mount]) -> Result<()> {
            unimplemented!()
        }
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
        };

        let mut config = DevboxConfig::default();
        config.sandbox.image = "nixos".into();
        config
            .custom_packages
            .insert("ripgrep".into(), "nixpkgs".into());

        // Provisioning fails against this runtime, which `create` reports as a
        // warning rather than an error — so the call succeeds and what matters
        // is that it got as far as creating.
        let _ = manager
            .create_sandbox("t", &runtime, &config, &[], &HashMap::new(), None, false)
            .await;
        assert!(
            runtime.created.load(Ordering::SeqCst),
            "a package that NixOS can install must not be refused"
        );
    }
}
