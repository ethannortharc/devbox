//! `devbox lab` — bring up, inspect, and tear down multi-node topologies (§9).
//!
//! The substrate is a single Linux box (a Lima VM on macOS, the host on Linux),
//! and every lab node is a network namespace inside it. That is why `lab up`
//! takes a `--substrate` box name: it is the one heavyweight thing a lab needs,
//! and it is reused across labs.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::lab::{Lab, scenarios, wiring};
use crate::runtime::{Runtime, SandboxStatus};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct LabArgs {
    #[command(subcommand)]
    pub command: LabCommand,
}

#[derive(Subcommand, Debug)]
pub enum LabCommand {
    /// List the built-in scenarios
    List,

    /// Bring a lab up
    Up(UpArgs),

    /// Tear a lab down
    Down(DownArgs),

    /// Show a lab's address plan and wiring
    Status(StatusArgs),

    /// Print a router's generated FRR configuration
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Box to use as the Linux substrate
    #[arg(long)]
    pub substrate: Option<String>,

    /// Print the commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct DownArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Box used as the Linux substrate
    #[arg(long)]
    pub substrate: Option<String>,

    /// Print the commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Emit JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Router to show; omit to show every router
    pub node: Option<String>,
}

pub async fn run(args: LabArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        LabCommand::List => list(),
        LabCommand::Up(a) => up(a, manager).await,
        LabCommand::Down(a) => down(a, manager).await,
        LabCommand::Status(a) => status(a),
        LabCommand::Config(a) => config(a),
    }
}

fn list() -> Result<()> {
    println!("Built-in lab scenarios:\n");
    for s in scenarios::SCENARIOS {
        println!("  {:<20} {}", s.name, s.summary);
    }
    println!("\nBring one up with:\n  devbox lab up clos-3node --substrate <box>");
    Ok(())
}

fn status(args: StatusArgs) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let summary = lab.summary();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    println!("Lab '{}' ({} substrate)\n", summary.name, summary.substrate);

    println!("Nodes:");
    for node in &summary.nodes {
        let asn = node.asn.map(|a| format!("  AS{a}")).unwrap_or_default();
        let loopback = node
            .loopback
            .as_deref()
            .map(|l| format!("  lo {l}"))
            .unwrap_or_default();
        println!("  {:<14} {:<12}{asn}{loopback}", node.name, node.role);
        for iface in &node.interfaces {
            let peer = iface.peer.as_deref().unwrap_or("—");
            println!("    {:<6} {:<18} ↔ {peer}", iface.name, iface.addr);
        }
    }

    println!("\nLinks:");
    for link in &summary.links {
        println!("  {:<18} {} ↔ {}", link.subnet, link.a, link.b);
    }

    println!(
        "\n{} node(s), {} link(s), {} router(s).",
        summary.nodes.len(),
        summary.links.len(),
        lab.topology.routers().len()
    );
    Ok(())
}

fn config(args: ConfigArgs) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let configs = lab.router_configs();

    match args.node {
        Some(node) => {
            let (_, conf) = configs
                .iter()
                .find(|(name, _)| *name == node)
                .with_context(|| format!("'{node}' is not a router in this lab"))?;
            print!("{conf}");
        }
        None => {
            for (name, conf) in &configs {
                println!("# ── {name} ──────────────────────────────");
                print!("{conf}");
                println!();
            }
        }
    }
    Ok(())
}

async fn up(args: UpArgs, manager: &SandboxManager) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let commands = lab.up_commands()?;

    println!(
        "Lab '{}': {} node(s), {} link(s)",
        lab.name(),
        lab.topology.nodes.len(),
        lab.topology.links.len()
    );

    if args.dry_run {
        print_commands(&commands);
        print_configs(&lab);
        return Ok(());
    }

    let (runtime, substrate) = resolve_substrate(manager, args.substrate.as_deref()).await?;
    println!("Substrate: '{substrate}' ({})\n", runtime.name());

    for cmd in &commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let result = runtime
            .exec_cmd(&substrate, &argv, false)
            .await
            .with_context(|| format!("failed to run: {}", wiring::render(cmd)))?;

        if result.exit_code != 0 {
            bail!(
                "wiring failed at `{}`:\n{}\n\nTear down with `devbox lab down {}`.",
                wiring::render(cmd),
                result.stderr.trim(),
                args.target
            );
        }
    }
    println!("Wiring is up ({} commands).", commands.len());

    // Router configs go in next; the routing daemon is started by the node's
    // own service manager, which the substrate provisioning installs.
    for (node, conf) in lab.router_configs() {
        push_file(
            runtime.as_ref(),
            &substrate,
            &format!("/etc/devbox/lab/{}/{node}/frr.conf", lab.name()),
            &conf,
        )
        .await?;
    }
    println!(
        "Router configs written for {} node(s).",
        lab.topology.routers().len()
    );
    println!("\nCheck it with `devbox lab status {}`.", args.target);
    Ok(())
}

async fn down(args: DownArgs, manager: &SandboxManager) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let commands = lab.down_commands();

    if args.dry_run {
        print_commands(&commands);
        return Ok(());
    }

    let (runtime, substrate) = resolve_substrate(manager, args.substrate.as_deref()).await?;

    // A teardown after a partial bring-up is the common case, so a namespace
    // that is already gone is not an error.
    let mut removed = 0;
    for cmd in &commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        if let Ok(result) = runtime.exec_cmd(&substrate, &argv, false).await
            && result.exit_code == 0
        {
            removed += 1;
        }
    }

    println!(
        "Lab '{}' torn down ({removed} of {} namespaces removed).",
        lab.name(),
        commands.len()
    );
    Ok(())
}

/// Find the box acting as the Linux substrate.
async fn resolve_substrate(
    manager: &SandboxManager,
    explicit: Option<&str>,
) -> Result<(Box<dyn Runtime>, String)> {
    let name = match explicit {
        Some(name) => name.to_string(),
        None => {
            let boxes = manager.list_sandboxes()?;
            match boxes.as_slice() {
                [] => bail!(
                    "no box to use as a substrate. A lab needs one Linux box to hold its \
                     namespaces — create one with `devbox create`, then pass \
                     `--substrate <name>`."
                ),
                [only] => only.name.clone(),
                many => bail!(
                    "several boxes could be the substrate ({}); pass `--substrate <name>`.",
                    many.iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };

    let state = manager.get_sandbox(&name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    if runtime.status(&name).await? != SandboxStatus::Running {
        bail!("substrate box '{name}' is not running; start it with `devbox shell {name}`");
    }
    Ok((runtime, name))
}

/// Write a file inside the substrate.
async fn push_file(runtime: &dyn Runtime, box_name: &str, path: &str, content: &str) -> Result<()> {
    use base64::Engine;
    // base64 rather than a heredoc: an FRR config contains `!`, quotes, and
    // whatever a node name happens to be, and none of that should have to
    // survive a shell.
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let dir = std::path::Path::new(path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".to_string());
    let script = format!("mkdir -p '{dir}' && echo '{encoded}' | base64 -d > '{path}'");

    let result = runtime
        .exec_cmd(box_name, &["sudo", "bash", "-c", &script], false)
        .await?;
    if result.exit_code != 0 {
        bail!("failed to write {path}: {}", result.stderr.trim());
    }
    Ok(())
}

fn print_commands(commands: &[Vec<String>]) {
    println!("\n# wiring ({} commands)", commands.len());
    for cmd in commands {
        println!("{}", wiring::render(cmd));
    }
}

fn print_configs(lab: &Lab) {
    for (node, conf) in lab.router_configs() {
        println!("\n# /etc/devbox/lab/{}/{node}/frr.conf", lab.name());
        print!("{conf}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_has_a_dry_run() {
        // `--dry-run` is the path that works everywhere, so it must never fail
        // for a built-in scenario.
        for scenario in scenarios::SCENARIOS {
            let lab = Lab::resolve(scenario.name).unwrap();
            let cmds = lab.up_commands().unwrap();
            assert!(!cmds.is_empty(), "{} produced no commands", scenario.name);
            assert!(!lab.down_commands().is_empty());
        }
    }

    #[test]
    fn rendered_commands_are_shell_free() {
        // These are argv vectors executed directly, never through a shell —
        // so a node name can never become a command.
        let lab = Lab::resolve("clos-3node").unwrap();
        for cmd in lab.up_commands().unwrap() {
            let text = wiring::render(&cmd);
            for meta in [';', '|', '&', '`', '$', '>', '<'] {
                assert!(!text.contains(meta), "{text} contains {meta}");
            }
        }
    }
}
