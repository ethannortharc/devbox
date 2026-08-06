//! `devbox lab` — bring up, inspect, and tear down multi-node topologies (§9).
//!
//! The substrate is a single Linux box (a Lima VM on macOS, the host on Linux),
//! and every lab node is a network namespace inside it. That is why `lab up`
//! takes a `--substrate` box name: it is the one heavyweight thing a lab needs,
//! and it is reused across labs.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::lab::fault::{Direction, Impairment};
use crate::lab::{Lab, fault, scenarios, wiring};
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

    /// Impair a link: delay, jitter, loss, rate, or a full partition
    Fault(FaultArgs),

    /// Clear every impairment on a link
    Heal(HealArgs),
}

#[derive(Args, Debug)]
pub struct FaultArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Link, as `nodeA-nodeB` or `node:iface`
    pub link: String,

    /// One-way delay, in milliseconds
    #[arg(long)]
    pub delay: Option<u32>,

    /// Delay variation, in milliseconds (needs --delay)
    #[arg(long)]
    pub jitter: Option<u32>,

    /// Packet loss, as a percentage
    #[arg(long)]
    pub loss: Option<f64>,

    /// Packet reordering, as a percentage (needs --delay)
    #[arg(long)]
    pub reorder: Option<f64>,

    /// Packet duplication, as a percentage
    #[arg(long)]
    pub duplicate: Option<f64>,

    /// Egress rate limit, in kbit/s
    #[arg(long)]
    pub rate: Option<u32>,

    /// Black-hole the link entirely (100% loss, both directions)
    #[arg(long)]
    pub partition: bool,

    /// Which end to impair: a, b, or both
    #[arg(long, default_value = "both")]
    pub direction: String,

    /// Box to use as the Linux substrate
    #[arg(long)]
    pub substrate: Option<String>,

    /// Print the commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct HealArgs {
    /// Scenario name, or a path to a lab.toml
    pub target: String,

    /// Link, as `nodeA-nodeB` or `node:iface`
    pub link: String,

    /// Box to use as the Linux substrate
    #[arg(long)]
    pub substrate: Option<String>,

    /// Print the commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
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
        LabCommand::Fault(a) => inject(a, manager).await,
        LabCommand::Heal(a) => heal(a, manager).await,
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

    let (runtime, substrate) = resolve_substrate(
        manager,
        args.substrate.as_deref(),
        Some(&lab.topology.lab.substrate),
    )
    .await?;
    println!("Substrate: '{substrate}' ({})\n", runtime.name());

    // Preflight, before a single namespace exists. Discovering that FRR is
    // missing *after* wiring left the user with a half-built lab and a
    // suggested re-run that then failed at `ip netns add`, because the
    // namespaces were already there. Nothing is worse to hand someone than a
    // fix that cannot be applied.
    if !lab.topology.routers().is_empty() {
        let probe = runtime
            .exec_cmd(&substrate, &["sh", "-c", "command -v zebra"], false)
            .await;
        if !probe.is_ok_and(|r| r.exit_code == 0) {
            bail!(
                "this topology has {} router(s), but substrate '{substrate}' has no FRR.\n  \
                 Enable the `network` set (`devbox sets enable network --name {substrate}`) \
                 and rebuild, then re-run `devbox lab up`.",
                lab.topology.routers().len()
            );
        }
    }

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

    // Router configs, then the daemons that read them.
    //
    // The comment that used to sit here said the node's own service manager
    // starts the routing daemon — but a namespace is not a machine and has no
    // service manager, and nothing in substrate provisioning installs one. So
    // `lab up` wrote every frr.conf, printed success, and left BGP unstarted:
    // adjacent nodes could ping, non-adjacent loopbacks never converged, and
    // the only symptom was a scenario that quietly failed its assertions.
    let mut started = 0usize;
    for (node, conf) in lab.router_configs() {
        push_file(
            runtime.as_ref(),
            &substrate,
            &format!("/etc/devbox/lab/{}/{node}/frr.conf", lab.name()),
            &conf,
        )
        .await?;

        for argv in crate::lab::frr::start_commands(lab.name(), &node) {
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            let result = runtime.exec_cmd(&substrate, &args, false).await?;
            if result.exit_code != 0 {
                bail!(
                    "could not start the routing daemons in namespace '{node}': {}\n\n  \
                     A routed lab needs FRR on the substrate box — enable the \
                     `network` set (`devbox sets enable network`) and rebuild, \
                     then re-run `devbox lab up`.",
                    result.stderr.trim()
                );
            }
        }
        started += 1;
    }
    println!("Router configs written and FRR started for {started} node(s).");

    // Service orchestration — dnsmasq, chrony, and `devbox-ztpd` — is not
    // wired yet. Saying so is the whole point: a topology that asks for those
    // and gets a silent success is a lab the user will debug for an hour
    // before discovering nothing was ever started.
    report_unstarted_services(&lab);

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

    let (runtime, substrate) = resolve_substrate(
        manager,
        args.substrate.as_deref(),
        Some(&lab.topology.lab.substrate),
    )
    .await?;

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

/// Name the parts of a topology this bring-up did not start.
///
/// The wiring, addressing, and routing configs are real. Everything a
/// `service` or `ztp-blank` node needs — dnsmasq with options 66/67, chrony,
/// `devbox-ztpd`, and the bootstrap run itself — is not orchestrated yet, and
/// reporting success without saying so would send someone hunting a fabric
/// that was never asked to provision.
fn report_unstarted_services(lab: &Lab) {
    use crate::lab::topology::Role;

    let mut pending: Vec<String> = Vec::new();
    if lab.topology.services.dns {
        pending.push("dnsmasq (DNS)".into());
    }
    if lab.topology.services.dhcp {
        pending.push("dnsmasq DHCP with options 66/67".into());
    }
    if lab.topology.services.ntp {
        pending.push("chrony (NTP)".into());
    }

    let blank = lab
        .topology
        .nodes
        .iter()
        .filter(|n| n.role == Role::ZtpBlank)
        .count();
    if blank > 0 {
        pending.push(format!(
            "devbox-ztpd, and the bootstrap on {blank} blank node(s)"
        ));
    }
    let services = lab
        .topology
        .nodes
        .iter()
        .filter(|n| n.role == Role::Service)
        .count();
    if services > 0 {
        pending.push(format!("the workload on {services} service node(s)"));
    }

    if pending.is_empty() {
        return;
    }

    println!("\nNot started by this bring-up (service orchestration is not wired yet):");
    for item in &pending {
        println!("  - {item}");
    }
    println!(
        "  The wiring, addressing, and routing configs above are real; \n  \
         these have to be started inside the substrate by hand for now."
    );
}

/// Parse the `--direction` flag.
fn parse_direction(text: &str) -> Result<Direction> {
    match text {
        "a" => Ok(Direction::A),
        "b" => Ok(Direction::B),
        "both" => Ok(Direction::Both),
        other => bail!("--direction must be a, b, or both, got '{other}'"),
    }
}

async fn inject(args: FaultArgs, manager: &SandboxManager) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let link = fault::find_link(&lab.topology, &args.link)?;
    let direction = parse_direction(&args.direction)?;

    let (commands, description) = if args.partition {
        (
            fault::partition(lab.name(), &link)?,
            "partitioned (100% loss, both directions)".to_string(),
        )
    } else {
        let impairment = Impairment {
            delay_ms: args.delay,
            jitter_ms: args.jitter,
            loss_pct: args.loss,
            duplicate_pct: args.duplicate,
            reorder_pct: args.reorder,
            rate_kbit: args.rate,
        };
        let description = impairment.describe();
        (
            fault::apply(lab.name(), &link, direction, &impairment)?,
            description,
        )
    };

    println!("{} ↔ {}: {description}", link.0, link.1);

    if args.dry_run {
        print_commands(&commands);
        return Ok(());
    }

    let (runtime, substrate) = resolve_substrate(
        manager,
        args.substrate.as_deref(),
        Some(&lab.topology.lab.substrate),
    )
    .await?;
    run_all(runtime.as_ref(), &substrate, &commands).await?;
    println!(
        "Applied. Clear it with `devbox lab heal {} {}`.",
        args.target, args.link
    );
    Ok(())
}

async fn heal(args: HealArgs, manager: &SandboxManager) -> Result<()> {
    let lab = Lab::resolve(&args.target)?;
    let link = fault::find_link(&lab.topology, &args.link)?;
    let commands = fault::heal(lab.name(), &link, Direction::Both);

    if args.dry_run {
        print_commands(&commands);
        return Ok(());
    }

    let (runtime, substrate) = resolve_substrate(
        manager,
        args.substrate.as_deref(),
        Some(&lab.topology.lab.substrate),
    )
    .await?;

    // Healing a link that was never impaired is a no-op, not an error.
    let mut cleared = 0;
    for cmd in &commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        if let Ok(result) = runtime.exec_cmd(&substrate, &argv, false).await
            && result.exit_code == 0
        {
            cleared += 1;
        }
    }
    println!(
        "{} ↔ {}: healed ({cleared} end(s) cleared).",
        link.0, link.1
    );
    Ok(())
}

/// Run a command sequence inside the substrate, stopping at the first failure.
async fn run_all(runtime: &dyn Runtime, substrate: &str, commands: &[Vec<String>]) -> Result<()> {
    for cmd in commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let result = runtime
            .exec_cmd(substrate, &argv, false)
            .await
            .with_context(|| format!("failed to run: {}", wiring::render(cmd)))?;
        if result.exit_code != 0 {
            bail!(
                "`{}` failed:\n{}",
                wiring::render(cmd),
                result.stderr.trim()
            );
        }
    }
    Ok(())
}

/// Find the box acting as the Linux substrate.
async fn resolve_substrate(
    manager: &SandboxManager,
    explicit: Option<&str>,
    configured: Option<&str>,
) -> Result<(Box<dyn Runtime>, String)> {
    // `--substrate` wins, then the topology's own `lab.substrate`, then the
    // single-box guess. Skipping the middle step meant a topology that named
    // its substrate was ignored, and a user with two boxes got "which one?"
    // for a question their file had already answered.
    // A topology's `substrate` may name a *runtime kind* rather than a box —
    // `lima`, `incus`, `host` are the documented values. Treating those as box
    // names made such a topology fail unless a box happened to be called
    // "lima", which nobody's is. A kind selects among the boxes; only an
    // explicit `--substrate` is a name.
    const RUNTIME_KINDS: &[&str] = &["lima", "incus", "multipass", "docker", "host"];
    let configured = configured.filter(|s| !s.is_empty() && *s != "auto");
    let (from_topology, want_kind) = match configured {
        Some(value) if RUNTIME_KINDS.contains(&value) => (None, Some(value)),
        other => (other, None),
    };

    let name = match explicit.or(from_topology) {
        Some(name) => name.to_string(),
        None => {
            let mut boxes = manager.list_sandboxes()?;
            if let Some(kind) = want_kind {
                boxes.retain(|b| b.runtime == kind);
                if boxes.is_empty() {
                    bail!(
                        "the topology asks for a '{kind}' substrate, but no box uses that \
                         runtime. Create one, or pass `--substrate <name>`."
                    );
                }
            }
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

    // A lab needs network namespaces and veth pairs, which need CAP_NET_ADMIN.
    // devbox creates Docker boxes without it, so such a substrate is accepted
    // and then fails on the first `ip netns add` — after the user has waited
    // for everything before it.
    if state.runtime == "docker" {
        bail!(
            "box '{name}' runs under Docker without the capabilities a lab needs \
             (network namespaces and veth pairs require CAP_NET_ADMIN).\n  \
             Use a VM substrate — `devbox create --runtime lima` — and pass it \
             with `--substrate <name>`."
        );
    }

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
