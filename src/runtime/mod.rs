pub mod cmd;
pub mod detect;
pub mod docker;
pub mod incus;
pub mod lima;
pub mod multipass;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

/// Sandbox status as reported by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    Running,
    Stopped,
    NotFound,
    Unknown(String),
}

/// Information about a sandbox instance.
#[derive(Debug, Clone)]
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
pub struct CreateOpts {
    pub name: String,
    pub mounts: Vec<Mount>,
    pub cpu: u32,
    pub memory: String,
    pub env: HashMap<String, String>,
    pub env_file: Option<PathBuf>,
    pub sets: Vec<String>,
    pub tools: Vec<String>,
    pub layout: String,
    pub bare: bool,
    pub writable: bool,
    /// Base image type: "nixos" or "ubuntu"
    pub image: String,
}

/// A host-to-VM mount point.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: String,
    pub read_only: bool,
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
    async fn exec_cmd(
        &self,
        name: &str,
        cmd: &[&str],
        interactive: bool,
    ) -> Result<ExecResult>;

    /// Destroy a sandbox permanently.
    async fn destroy(&self, name: &str) -> Result<()>;

    /// Get sandbox status.
    async fn status(&self, name: &str) -> Result<SandboxStatus>;

    /// List all devbox sandboxes managed by this runtime.
    async fn list(&self) -> Result<Vec<SandboxInfo>>;

    /// Create a named snapshot.
    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()>;

    /// Restore a named snapshot.
    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()>;

    /// List snapshots for a sandbox.
    async fn snapshot_list(&self, name: &str) -> Result<Vec<SnapshotInfo>>;

    /// Add tools/sets to an existing sandbox.
    async fn upgrade(&self, name: &str, tools: &[String]) -> Result<()>;
}
