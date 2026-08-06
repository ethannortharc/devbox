use anyhow::Result;

use devbox::cli::Cli;
use devbox::sandbox::SandboxManager;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse_smart();
    let manager = SandboxManager::new()?;

    match cli.command {
        Some(cmd) => cmd.run(&manager).await,
        None => {
            // Smart default: create-or-attach
            let tools = cli.tools.as_deref();
            manager.create_or_attach(tools).await
        }
    }
}

/// Structured logging, off unless asked for.
///
/// Devbox is a foreground CLI: log lines would compete with its own output, so
/// the default filter is `warn` and anything richer is opt-in through
/// `DEVBOX_LOG` (or the conventional `RUST_LOG`). Logs go to stderr so they
/// never corrupt piped stdout.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("DEVBOX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
