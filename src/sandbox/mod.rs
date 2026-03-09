pub mod config;
pub mod global_config;
pub mod overlay;
pub mod provision;
pub mod state;

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, Context, bail};

use crate::runtime::detect::{detect_runtime, select_runtime};
use crate::runtime::{CreateOpts, Mount, Runtime, SandboxStatus};
use crate::tools::detect::detect_languages;
use self::config::DevboxConfig;
use self::global_config::GlobalConfig;
use self::state::SandboxState;

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
            std::fs::create_dir_all(&state_dir)
                .context("Failed to create ~/.devbox/")?;
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

    /// Create a new sandbox end-to-end.
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

        // Check for name conflicts
        if self.sandbox_exists(name) {
            bail!("Sandbox '{}' already exists. Use `devbox destroy {}` first.", name, name);
        }

        // Check for mount conflicts
        if let Some(existing) = self.check_mount_conflict(&cwd)? {
            bail!(
                "Directory already mounted by sandbox '{}'. Use `devbox shell` to attach.",
                existing
            );
        }

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
            layout: config.sandbox.layout.clone(),
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
        if let Err(e) = provision::provision_vm(runtime, name, &active_sets, &active_langs, image).await {
            eprintln!("Warning: provisioning incomplete: {e}");
        }

        // Save state
        let state = SandboxState {
            name: name.to_string(),
            runtime: runtime.name().to_string(),
            project_dir: cwd,
            created_at: info.created_at.unwrap_or_default(),
            mount_mode: config.sandbox.mount_mode.clone(),
            layout: config.sandbox.layout.clone(),
            sets: config.active_sets(),
            languages: config.active_languages(),
            image: config.sandbox.image.clone(),
        };
        state.save(&self.state_dir)?;

        println!("Sandbox '{}' created successfully (runtime: {})", name, runtime.name());
        Ok(())
    }

    /// Attach to a sandbox (start if stopped, then launch Zellij or shell).
    pub async fn attach(&self, name: &str, layout_override: Option<&str>) -> Result<()> {
        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;

        // Check status, start if stopped
        let status = runtime.status(name).await?;
        match status {
            SandboxStatus::Running => {}
            SandboxStatus::Stopped => {
                println!("Starting sandbox '{name}'...");
                runtime.start(name).await?;
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

        // Determine layout: CLI flag > saved state > "default"
        let layout = layout_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| state.layout.clone());

        // "plain" layout = raw shell, no Zellij
        if layout == "plain" {
            println!("Attaching to sandbox '{name}'...");
            let shell = Self::probe_shell(runtime.as_ref(), name).await;
            runtime.exec_cmd(name, &[&shell, "-l"], true).await?;
            return Ok(());
        }

        // Check if Zellij is available in the VM
        let zellij_check = runtime.exec_cmd(name, &["which", "zellij"], false).await;
        let has_zellij = zellij_check.is_ok() && zellij_check.unwrap().exit_code == 0;

        if !has_zellij {
            // No Zellij — fall back to raw shell
            println!("Attaching to sandbox '{name}'...");
            let shell = Self::probe_shell(runtime.as_ref(), name).await;
            runtime.exec_cmd(name, &[&shell, "-l"], true).await?;
            return Ok(());
        }

        // Check for saved layout in VM (user customized via `devbox layout save`)
        // Priority: CLI --layout flag > saved layout in VM > state default > built-in default
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "dev".to_string());
        let saved_layout_path = format!("/home/{username}/.config/devbox/saved-layout.kdl");

        let use_saved = if layout_override.is_some() {
            // Explicit --layout flag always wins
            false
        } else {
            // Check if saved layout exists in VM
            let check = runtime
                .exec_cmd(name, &["test", "-f", &saved_layout_path], false)
                .await;
            check.is_ok() && check.unwrap().exit_code == 0
        };

        if use_saved {
            println!("Attaching to sandbox '{name}' (saved layout)...");
            runtime
                .exec_cmd(name, &["zellij", "--layout", &saved_layout_path], true)
                .await?;
        } else {
            // Push built-in layout KDL into the VM and launch Zellij
            let layout_content = crate::tui::lookup_layout_kdl(&layout);
            Self::push_layout_to_vm(runtime.as_ref(), name, &layout, layout_content).await?;

            let layout_path = format!("/tmp/devbox-layout-{layout}.kdl");
            println!("Attaching to sandbox '{name}' (layout: {layout})...");
            runtime
                .exec_cmd(name, &["zellij", "--layout", &layout_path], true)
                .await?;
        }
        Ok(())
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

    /// Push a Zellij layout KDL file into the VM at /tmp/.
    async fn push_layout_to_vm(
        runtime: &dyn crate::runtime::Runtime,
        name: &str,
        layout_name: &str,
        content: &str,
    ) -> Result<()> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let path = format!("/tmp/devbox-layout-{layout_name}.kdl");
        let cmd = format!("echo '{encoded}' | base64 -d > {path}");
        runtime.exec_cmd(name, &["bash", "-c", &cmd], false).await?;
        Ok(())
    }

    /// Smart default: if a sandbox exists for the current directory, attach.
    /// Otherwise, create one.
    pub async fn create_or_attach(&self, tools: Option<&[String]>) -> Result<()> {
        let cwd = env::current_dir().context("Cannot determine current directory")?;
        let name = self.name_from_dir(&cwd);

        if self.sandbox_exists(&name) {
            self.attach(&name, None).await
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
            self.attach(&name, None).await
        }
    }

    /// Stop a sandbox.
    pub async fn stop_sandbox(&self, name: &str) -> Result<()> {
        let state = self.get_sandbox(name)?;
        let runtime = self.runtime_for_sandbox(&state)?;
        runtime.stop(name).await?;
        println!("Sandbox '{}' stopped.", name);
        Ok(())
    }

    /// Destroy a sandbox permanently.
    /// Warns if there are uncommitted overlay changes.
    pub async fn destroy_sandbox(&self, name: &str, force: bool) -> Result<()> {
        let state = self.get_sandbox(name);
        if let Ok(state) = &state {
            let runtime = self.runtime_for_sandbox(state)?;

            // Check for uncommitted overlay changes before destroying
            if state.mount_mode == "overlay" && !force {
                let vm_status = runtime.status(name).await.unwrap_or(SandboxStatus::NotFound);
                if vm_status == SandboxStatus::Running {
                    let changes = overlay::diff(runtime.as_ref(), name).await;
                    if let Ok(changes) = changes {
                        let file_count = changes.iter().filter(|c| !c.is_dir).count();
                        if file_count > 0 {
                            eprintln!(
                                "Warning: {} uncommitted overlay change(s) in sandbox '{}'.",
                                file_count, name
                            );
                            eprintln!("  Run `devbox layer commit --name {}` to save them first,", name);
                            eprintln!("  or use `devbox destroy --force --name {}` to discard.", name);
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
        if status == SandboxStatus::Stopped {
            runtime.start(name).await?;
        } else if status == SandboxStatus::NotFound {
            bail!("Sandbox '{}' not found in runtime", name);
        }

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

            let status = runtime.status(&state.name).await.unwrap_or(SandboxStatus::NotFound);
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
            s.project_dir.canonicalize().unwrap_or_else(|_| s.project_dir.clone()) == canonical
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
        if let Ok(global) = self.load_global_config() {
            if global.default.runtime != "auto" {
                config.sandbox.runtime = global.default.runtime;
            }
            if global.default.layout != "default" {
                config.sandbox.layout = global.default.layout;
            }
        }

        config
    }
}
