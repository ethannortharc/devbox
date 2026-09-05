pub mod behavior;
pub mod box_arg;
pub mod broker;
pub mod code;
pub mod commit;
pub mod config;
pub mod create;
pub mod destroy;
pub mod diff;
pub mod discard;
pub mod doctor;
pub mod exec;
pub mod export;
pub mod help;
pub mod init;
pub mod layer;
pub mod list;
pub mod mcp;
pub mod nix_cmd;
pub mod policy;
pub mod prune;
pub mod report;
pub mod reprovision;
pub mod run;
pub mod runs;
pub mod secret;
pub mod self_update;
pub mod sets;
pub mod shell;
pub mod snapshot;
pub mod status;
pub mod stop;
pub mod upgrade;
pub mod use_cmd;
pub mod watch;
pub mod web;

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
    /// Internal entry point for the per-user collector process.
    #[command(name = "__collector", hide = true)]
    Collector,

    /// Internal entry point for the per-user credential broker process.
    #[command(name = "__broker", hide = true)]
    Broker,

    /// Create a new sandbox
    Create(create::CreateArgs),

    /// Attach to a sandbox (start if stopped)
    Shell(shell::ShellArgs),

    /// Run a one-off command in the sandbox
    Exec(exec::ExecArgs),

    /// Run a command and record what it did (§4)
    Run(run::RunArgs),

    /// List a box's recorded runs
    Runs(runs::RunsArgs),

    /// Print a run's report
    Report(report::ReportArgs),

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

    /// Show or change which Nix sets a box has
    Sets(sets::SetsArgs),

    /// Show what a box has been doing
    Watch(watch::WatchArgs),

    /// Summarize or diff a box's behaviour
    Behavior(behavior::BehaviorArgs),

    /// Read or change a box's egress policy
    Policy(policy::PolicyArgs),

    /// Run MCP servers inside a box
    Mcp(mcp::McpArgs),

    /// Start the local web console
    Web(web::WebArgs),

    /// Export a box's events as OCSF, OTLP/JSON, or JSON Lines
    Export(export::ExportArgs),

    /// Manage the credentials the broker holds for your boxes
    Secret(secret::SecretArgs),

    /// Inspect the host-side credential broker
    #[command(name = "broker")]
    BrokerCmd(broker::BrokerArgs),
}

impl Command {
    /// Whether this command can create, start, or actively observe a box.
    ///
    /// Read-only metadata commands deliberately do not start a background
    /// process. Lifecycle and observation commands do, so capture survives
    /// after the foreground command or web console exits.
    pub fn needs_collector(&self) -> bool {
        // `mcp` decides for itself: `mcp run` starts a box and its events have
        // to be collected, while `mcp add`, `ls` and `rm` only edit a file.
        // Registering the whole subcommand here would launch a background
        // process for `devbox mcp ls`.
        if let Self::Mcp(args) = self {
            return args.needs_collector();
        }
        matches!(
            self,
            Self::Create(_)
                | Self::Shell(_)
                | Self::Exec(_)
                | Self::Run(_)
                | Self::Reprovision(_)
                | Self::Code(_)
                | Self::Use(_)
                | Self::Sets(_)
                | Self::Watch(_)
                | Self::Behavior(_)
                | Self::Policy(_)
                | Self::Web(_)
        )
    }

    pub async fn run(self, manager: &SandboxManager) -> Result<()> {
        match self {
            Command::Collector => {
                let manager = std::sync::Arc::new(SandboxManager {
                    state_dir: manager.state_dir.clone(),
                });
                crate::obs::daemon::run(manager).await
            }
            Command::Broker => {
                let manager = std::sync::Arc::new(SandboxManager {
                    state_dir: manager.state_dir.clone(),
                });
                crate::broker::daemon::run(manager).await
            }
            Command::Create(args) => create::run(args, manager).await,
            Command::Shell(args) => shell::run(args, manager).await,
            Command::Exec(args) => exec::run(args, manager).await,
            Command::Run(args) => run::run(args, manager).await,
            Command::Runs(args) => runs::run(args, manager).await,
            Command::Report(args) => report::run(args, manager).await,
            Command::Stop(args) => stop::run(args, manager).await,
            Command::Destroy(args) => destroy::run(args, manager).await,
            Command::List(args) => list::run(args, manager).await,
            Command::Status(args) => status::run(args, manager).await,
            Command::Snapshot(args) => snapshot::run(args, manager).await,
            Command::Upgrade(args) => upgrade::run(args, manager).await,
            Command::Config(args) => config::run(args, manager).await,
            Command::Doctor(args) => doctor::run(args, manager).await,
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
            Command::Sets(args) => sets::run(args, manager).await,
            Command::Watch(args) => watch::run(args, manager).await,
            Command::Behavior(args) => behavior::run(args, manager).await,
            Command::Policy(args) => policy::run(args, manager).await,
            Command::Mcp(args) => mcp::run(args, manager).await,
            Command::Web(args) => web::run(args, manager).await,
            Command::Export(args) => export::run(args, manager).await,
            Command::Secret(args) => secret::run(args, manager).await,
            Command::BrokerCmd(args) => broker::run(args, manager).await,
        }
    }
}
