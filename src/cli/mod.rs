pub mod code;
pub mod commit;
pub mod config;
pub mod create;
pub mod destroy;
pub mod diff;
pub mod discard;
pub mod doctor;
pub mod exec;
pub mod help;
pub mod init;
pub mod layer;
pub mod layout;
pub mod list;
pub mod nix_cmd;
pub mod packages;
pub mod prune;
pub mod reprovision;
pub mod self_update;
pub mod shell;
pub mod snapshot;
pub mod status;
pub mod stop;
pub mod upgrade;
pub mod use_cmd;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::sandbox::SandboxManager;

/// Devbox — NixOS-powered developer VM
#[derive(Parser, Debug)]
#[command(name = "devbox", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Tools to install (e.g., claude-code,go,rust)
    #[arg(long, value_delimiter = ',')]
    pub tools: Option<Vec<String>>,

    /// Skip auto-detection, minimal install
    #[arg(long)]
    pub bare: bool,

    /// Output format
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

impl Cli {
    /// Parse CLI args with smart default behavior.
    /// Bare `devbox` (no subcommand) triggers create-or-attach.
    pub fn parse_smart() -> Self {
        Self::parse()
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new sandbox
    Create(create::CreateArgs),

    /// Attach to a sandbox (start if stopped)
    Shell(shell::ShellArgs),

    /// Run a one-off command in the sandbox
    Exec(exec::ExecArgs),

    /// Stop a sandbox (preserves state)
    Stop(stop::StopArgs),

    /// Remove a sandbox permanently
    Destroy(destroy::DestroyArgs),

    /// List all sandboxes
    List(list::ListArgs),

    /// Show detailed sandbox status
    Status(status::StatusArgs),

    /// Manage snapshots
    Snapshot(snapshot::SnapshotArgs),

    /// Add tools to an existing sandbox
    Upgrade(upgrade::UpgradeArgs),

    /// Get or set configuration
    Config(config::ConfigArgs),

    /// Diagnose issues
    Doctor(doctor::DoctorArgs),

    /// Manage Zellij layouts
    Layout(layout::LayoutArgs),

    /// Open TUI package manager
    Packages(packages::PackagesArgs),

    /// Remove all stopped sandboxes
    Prune(prune::PruneArgs),

    /// Generate devbox.toml
    Init(init::InitArgs),

    /// Manage Nix packages
    Nix(nix_cmd::NixArgs),

    /// Sync overlay changes to host
    Commit(commit::CommitArgs),

    /// Show overlay changes vs host
    Diff(diff::DiffArgs),

    /// Throw away overlay changes
    Discard(discard::DiscardArgs),

    /// Show quick reference for a tool
    #[command(name = "guide")]
    Guide(help::HelpArgs),

    /// Re-provision a sandbox (push latest configs + rebuild)
    Reprovision(reprovision::ReprovisionArgs),

    /// Update devbox to the latest version
    SelfUpdate(self_update::SelfUpdateArgs),

    /// Manage overlay layer (status, diff, commit, stash, ...)
    Layer(layer::LayerArgs),

    /// Open VS Code / Cursor into a sandbox via Remote SSH
    Code(code::CodeArgs),

    /// Switch sandbox to use current directory
    #[command(name = "use")]
    Use(use_cmd::UseArgs),
}

impl Command {
    pub async fn run(self, manager: &SandboxManager) -> Result<()> {
        match self {
            Command::Create(args) => create::run(args, manager).await,
            Command::Shell(args) => shell::run(args, manager).await,
            Command::Exec(args) => exec::run(args, manager).await,
            Command::Stop(args) => stop::run(args, manager).await,
            Command::Destroy(args) => destroy::run(args, manager).await,
            Command::List(args) => list::run(args, manager).await,
            Command::Status(args) => status::run(args, manager).await,
            Command::Snapshot(args) => snapshot::run(args, manager).await,
            Command::Upgrade(args) => upgrade::run(args, manager).await,
            Command::Config(args) => config::run(args, manager).await,
            Command::Doctor(args) => doctor::run(args, manager).await,
            Command::Layout(args) => layout::run(args, manager).await,
            Command::Packages(args) => packages::run(args, manager).await,
            Command::Prune(args) => prune::run(args, manager).await,
            Command::Init(args) => init::run(args, manager).await,
            Command::Nix(args) => nix_cmd::run(args, manager).await,
            Command::Commit(args) => commit::run(args, manager).await,
            Command::Diff(args) => diff::run(args, manager).await,
            Command::Discard(args) => discard::run(args, manager).await,
            Command::Guide(args) => help::run(args, manager).await,
            Command::Reprovision(args) => reprovision::run(args, manager).await,
            Command::SelfUpdate(args) => self_update::run(args, manager).await,
            Command::Layer(args) => layer::run(args, manager).await,
            Command::Code(args) => code::run(args, manager).await,
            Command::Use(args) => use_cmd::run(args, manager).await,
        }
    }
}
