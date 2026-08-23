use std::sync::Arc;

use anyhow::Result;

use devbox::cli::Cli;
use devbox::sandbox::SandboxManager;
use devbox::web::{WebOptions, serve};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse_smart();
    let manager = SandboxManager::new()?;

    match cli.command {
        Some(cmd) => {
            if cmd.needs_collector() {
                devbox::obs::daemon::ensure_running(&manager);
            }
            cmd.run(&manager).await
        }
        None => {
            devbox::obs::daemon::ensure_running(&manager);
            open_console_for_cwd(&manager, cli.tools.as_deref()).await
        }
    }
}

/// Bare `devbox`: make sure this project has a box, then open the console on
/// it (§5 — the console replaces the Zellij attach as the default experience).
///
/// `devbox shell` is still the way to get a terminal without a browser, so
/// nothing is lost for headless use.
async fn open_console_for_cwd(manager: &SandboxManager, tools: Option<&[String]>) -> Result<()> {
    let name = manager.ensure_box_for_cwd(tools).await?;

    // Enforce before serving. `ensure_box_for_cwd` hands back a box that may
    // have been created moments ago or started outside devbox, and this path
    // does not otherwise touch the start lifecycle — so a project with a
    // restrictive posture would sit unrestricted behind a console reporting
    // it, until the user happened to open a terminal.
    // `ensure_box_for_cwd` creates a box if there is none, but an existing one
    // may be stopped — and applying a posture to a stopped box fails on its
    // first exec. Start it, exactly as every other console entry point does.
    devbox::web::service::ensure_running(manager, &name).await?;

    let manager = Arc::new(SandboxManager {
        state_dir: manager.state_dir.clone(),
    });

    serve(
        manager,
        WebOptions {
            landing: format!("/boxes/{}", devbox::web::encode_segment(&name)),
            ..WebOptions::default()
        },
    )
    .await
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
