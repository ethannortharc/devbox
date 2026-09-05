pub mod cmd;
pub mod detect;
pub mod docker;
pub mod incus;
pub mod lima;
pub mod multipass;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

/// Current time in the stable, human-readable format persisted in box state.
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Sandbox status as reported by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    Running,
    /// The runtime process is up, but its guest control channel is not.
    ///
    /// Treating this as `Running` makes shell, policy, and file operations
    /// hang or fail after the UI has already promised that the box is ready.
    /// It is distinct from `Unknown`: we know the box must be stopped (and
    /// possibly force-stopped) before it can be started cleanly again.
    Unreachable(String),
    Stopped,
    NotFound,
    Unknown(String),
}

/// Information about a sandbox instance.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxInfo {
    pub name: String,
    pub status: SandboxStatus,
    pub runtime: String,
    pub created_at: Option<String>,
    pub ip_address: Option<String>,
}

/// Information about a snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub name: String,
    pub created_at: String,
}

/// Options for creating a new sandbox.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CreateOpts {
    pub name: String,
    pub mounts: Vec<Mount>,
    pub cpu: u32,
    pub memory: String,
    pub env: HashMap<String, String>,
    pub env_file: Option<PathBuf>,
    pub sets: Vec<String>,
    pub tools: Vec<String>,
    pub bare: bool,
    pub writable: bool,
    /// Base image type: "nixos" or "ubuntu"
    pub image: String,
    /// If set, create from this cached image instead of the base image.
    /// For Incus: an image alias; for Lima: a path to a cached disk file.
    pub cached_image: Option<String>,
}

/// A host-to-VM mount point.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: String,
    pub read_only: bool,
}

/// Runtime-owned rollback data for a mount update that has not yet been
/// committed to sandbox state.
///
/// Keeping the runtime's exact original mount description in the caller makes
/// `devbox use` a two-phase operation: state persistence either succeeds, or
/// the runtime mounts are restored.
#[derive(Debug)]
pub enum MountUpdate {
    Lima {
        yaml_path: PathBuf,
        original: Vec<u8>,
    },
    Incus {
        original: Vec<(String, BTreeMap<String, String>)>,
    },
}

/// Result of executing a command inside a sandbox.
#[derive(Debug)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runtime trait — abstraction over Incus, Lima, Multipass, Docker.
///
/// All runtime interactions are via safe subprocess invocation
/// (no native SDK dependencies).
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Runtime name (e.g., "incus", "lima", "docker").
    fn name(&self) -> &str;

    /// Whether this runtime is available on the current system.
    fn is_available(&self) -> bool;

    /// Priority for auto-detection. Higher = preferred.
    fn priority(&self) -> u32;

    /// Create a new sandbox.
    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo>;

    /// Start a stopped sandbox.
    async fn start(&self, name: &str) -> Result<()>;

    /// Stop a running sandbox.
    async fn stop(&self, name: &str) -> Result<()>;

    /// Execute a command inside a sandbox.
    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult>;

    /// Build the host-side argv that runs `cmd` inside a sandbox.
    ///
    /// [`Runtime::exec_cmd`] owns the process it spawns: it either captures
    /// the output or inherits the parent's stdio. Both are wrong for the web
    /// tier, which needs to run the command under a *pty* it controls (the
    /// browser terminal) or with piped stdio it can stream line by line (a
    /// Nix rebuild). Exposing the argv keeps process ownership in the caller
    /// while each runtime still owns how its instances are addressed.
    ///
    /// The returned vector is `[program, arg, ...]` and is never empty.
    fn argv(&self, name: &str, cmd: &[&str], interactive: bool) -> Vec<String>;

    /// Destroy a sandbox permanently.
    async fn destroy(&self, name: &str) -> Result<()>;

    /// Get sandbox status.
    async fn status(&self, name: &str) -> Result<SandboxStatus>;

    /// List all devbox sandboxes managed by this runtime.
    #[allow(dead_code)]
    async fn list(&self) -> Result<Vec<SandboxInfo>>;

    /// Create a named snapshot.
    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()>;

    /// Restore a named snapshot.
    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()>;

    /// List snapshots for a sandbox.
    async fn snapshot_list(&self, name: &str) -> Result<Vec<SnapshotInfo>>;

    /// Add tools/sets to an existing sandbox.
    #[allow(dead_code)]
    async fn upgrade(&self, name: &str, tools: &[String]) -> Result<()>;

    /// Update mount points for an existing sandbox.
    ///
    /// An implementation returning `Err` must have either made no runtime
    /// change or restored the original mounts and running state itself. This
    /// lets callers distinguish an operation failure from a committed update
    /// without powering off a box that was never touched.
    fn supports_mount_updates(&self) -> bool {
        false
    }

    async fn update_mounts(&self, name: &str, mounts: &[Mount]) -> Result<MountUpdate>;

    /// Roll back an update whose corresponding sandbox state could not be
    /// persisted. Implementations must leave the next start on the old mounts.
    async fn rollback_mounts(&self, name: &str, update: &MountUpdate) -> Result<()>;

    /// Execute an interactive command as the non-root user.
    /// Used for shell attach — defaults to exec_cmd with interactive=true.
    /// Runtimes like Incus override this to set --user, HOME, and CWD.
    async fn exec_as_user(&self, name: &str, cmd: &[&str]) -> Result<ExecResult> {
        self.exec_cmd(name, cmd, true).await
    }

    /// Whether exec_cmd runs as root by default.
    /// Incus: true (incus exec defaults to root)
    /// Lima: false (limactl shell runs as the configured user)
    fn exec_runs_as_root(&self) -> bool {
        false
    }

    /// Check if a cached provisioned image exists for the given cache key.
    /// Returns the image alias/path if found.
    async fn cached_image(&self, _cache_key: &str) -> Option<String> {
        None
    }

    /// Cache the current VM as a provisioned image for reuse.
    /// Called after successful provisioning to speed up future creates.
    async fn cache_image(&self, _name: &str, _cache_key: &str) -> Result<()> {
        Ok(())
    }

    /// Execute a shell command as root with a login shell.
    ///
    /// This is the correct abstraction for running privileged commands:
    /// - Incus: `bash -lc <cmd>` (already root, login shell for PATH)
    /// - Lima:  `sudo bash -lc <cmd>` (elevate, login shell for PATH)
    ///
    /// Unlike a simple `sudo` prefix, this wraps the ENTIRE command inside
    /// the sudo boundary, so environment variables set within `cmd` (like
    /// `export NIX_PATH=...`) are preserved for the privileged process.
    async fn run_as_root(&self, name: &str, cmd: &str, interactive: bool) -> Result<ExecResult> {
        if self.exec_runs_as_root() {
            self.exec_cmd(name, &["bash", "-lc", cmd], interactive)
                .await
        } else {
            self.exec_cmd(name, &["sudo", "bash", "-lc", cmd], interactive)
                .await
        }
    }
}
