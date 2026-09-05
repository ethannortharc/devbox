//! `devbox broker` — inspect and control the host-side credential broker (§6).

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct BrokerArgs {
    #[command(subcommand)]
    pub command: BrokerCommand,
}

#[derive(Subcommand, Debug)]
pub enum BrokerCommand {
    /// Show the broker process, its endpoint, and the configured providers
    Status,

    /// Start the broker if it is not already running
    Start,

    /// Stop the broker
    Stop,

    /// Show how a box reaches the broker, probing from inside the box
    Reach(ReachArgs),
}

#[derive(Args, Debug)]
pub struct ReachArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,
}

pub async fn run(args: BrokerArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        BrokerCommand::Status => status(manager),
        BrokerCommand::Start => start(manager),
        BrokerCommand::Stop => stop(manager),
        BrokerCommand::Reach(args) => reach(args, manager).await,
    }
}

/// One line describing the broker, shared with `devbox doctor`.
pub fn status_line(manager: &SandboxManager) -> String {
    match crate::broker::daemon::status(manager) {
        Ok(Some(identity)) => match crate::broker::endpoint(&manager.state_dir) {
            Some(endpoint) => format!("running ({identity}) on http://127.0.0.1:{}", endpoint.port),
            None => format!("running ({identity}), endpoint not yet published"),
        },
        Ok(None) => {
            if crate::broker::configured_providers(&manager.state_dir).is_empty() {
                "not running (no secrets configured)".to_string()
            } else {
                "not running".to_string()
            }
        }
        Err(error) => format!("unknown: {error}"),
    }
}

fn status(manager: &SandboxManager) -> Result<()> {
    println!("broker:    {}", status_line(manager));
    println!("secrets:   {}", crate::cli::secret::backend_label(manager));
    let providers = crate::broker::configured_providers(&manager.state_dir);
    println!(
        "providers: {}",
        if providers.is_empty() {
            "(none)".to_string()
        } else {
            providers.join(", ")
        }
    );
    let tokens = crate::broker::tokens::TokenStore::new(&manager.state_dir);
    println!("tokens:    {} box(es) hold a broker token", tokens.len());
    Ok(())
}

fn start(manager: &SandboxManager) -> Result<()> {
    if crate::broker::configured_providers(&manager.state_dir).is_empty() {
        println!("No secrets are configured, so there is nothing to broker.");
        println!("  devbox secret set anthropic --from-env ANTHROPIC_API_KEY");
        return Ok(());
    }
    crate::broker::daemon::ensure_running(manager);
    // The child publishes its endpoint after it binds; wait briefly so
    // `start` reports the state the user is about to act on rather than the
    // one that existed a millisecond ago.
    for _ in 0..40 {
        if crate::broker::endpoint(&manager.state_dir).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("broker: {}", status_line(manager));
    Ok(())
}

fn stop(manager: &SandboxManager) -> Result<()> {
    let Some(endpoint) = crate::broker::endpoint(&manager.state_dir) else {
        println!("The broker is not running.");
        return Ok(());
    };
    // Same guard the collector uses before signalling: the pid comes from a
    // file, so it is checked against the process's own command line first.
    let process = std::process::Command::new("ps")
        .args(["-ww", "-p", &endpoint.pid.to_string(), "-o", "command="])
        .stdin(std::process::Stdio::null())
        .output()
        .context("inspect the broker process")?;
    let command_line = String::from_utf8_lossy(&process.stdout);
    if !process.status.success() || !command_line.split_whitespace().any(|a| a == "__broker") {
        println!(
            "The endpoint record names pid {}, but that process is not a devbox broker; \
             refusing to signal it.",
            endpoint.pid
        );
        return Ok(());
    }
    std::process::Command::new("kill")
        .args(["-TERM", &endpoint.pid.to_string()])
        .stdin(std::process::Stdio::null())
        .status()
        .context("signal the broker")?;
    println!("Stopped the broker (pid {}).", endpoint.pid);
    Ok(())
}

async fn reach(args: ReachArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    let state = crate::sandbox::state::SandboxState::load(&manager.state_dir, &name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;
    let port = crate::broker::endpoint(&manager.state_dir)
        .map(|e| e.port)
        .unwrap_or(crate::broker::DEFAULT_PORT);

    match runtime.host_reach(&name, port).await {
        Ok(reach) => {
            println!("{name}: {} ({})", reach.base_url(), reach.how);
        }
        Err(error) => {
            println!("{name}: unreachable — {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::{Cli, Command};

    #[test]
    fn reach_takes_the_box_as_a_positional() {
        let cli = Cli::try_parse_from(["devbox", "broker", "reach", "devtest"]).unwrap();
        let Some(Command::BrokerCmd(args)) = cli.command else {
            panic!("not a broker command");
        };
        let super::BrokerCommand::Reach(reach) = args.command else {
            panic!("not reach");
        };
        assert_eq!(reach.boxarg.name(), Some("devtest"));
    }

    #[test]
    fn broker_has_the_four_verbs_and_no_verb_that_reveals_a_secret() {
        use clap::CommandFactory as _;
        let root = Cli::command();
        let broker = root
            .get_subcommands()
            .find(|c| c.get_name() == "broker")
            .expect("devbox broker exists");
        let verbs: Vec<&str> = broker.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(verbs, vec!["status", "start", "stop", "reach"]);
    }
}
