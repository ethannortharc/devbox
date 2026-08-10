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

    // The retry commands this function may have to print, built while the
    // outer `args` is still in scope — the per-command loop below shadows it
    // with the argv it is running.
    let target = args.target.clone();
    let substrate_flag = args
        .substrate
        .as_deref()
        .map(|s| format!(" --substrate {s}"))
        .unwrap_or_default();

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
                // `sets apply` replaces the selection: naming one set there
                // disables every set and package not listed, so the old advice
                // to add `network` "keeping your other sets" described
                // something the command it gave could not do. `upgrade` adds.
                "this topology has {} router(s), but substrate '{substrate}' has no FRR.\n  \
                 Add it with `devbox upgrade --name {substrate} --tools network`, \
                 then re-run `devbox lab up`.",
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

    // Record this lab's prefixes where the policy engine can find them.
    //
    // `isolated` permits a running lab's subnets and nothing else (ADR-0046),
    // which needs the ruleset generator to know what those subnets are — and
    // it runs on the host, from a policy file, with no idea a lab exists. A
    // file on the box is the handoff: written when a lab comes up, removed
    // when it goes down, read at every policy apply.
    // The prefixes this lab actually allocated, not the pool it drew from.
    // Writing the base exempted every address in a `/16` when the lab used a
    // handful of `/31`s — and omitted explicit links outside the base
    // entirely, so it was both too permissive and incomplete.
    let mut prefixes: Vec<String> = lab
        .plan
        .links
        .iter()
        .map(|link| link.subnet.clone())
        .collect();
    prefixes.extend(lab.plan.loopbacks.values().map(|addr| format!("{addr}/32")));
    prefixes.sort();
    prefixes.dedup();

    push_file(
        runtime.as_ref(),
        &substrate,
        &format!("/etc/devbox/lab/{}/prefixes", lab.name()),
        &format!("{}\n", prefixes.join("\n")),
    )
    .await?;

    // And reload, so the exemption is in force now. `resolve_substrate`
    // applied the posture before this file existed, so an isolated substrate
    // would otherwise run the whole lab with its own subnets blocked until
    // some later operation happened to reapply.
    reapply_policy(manager, &substrate).await?;

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
                // Two things this has to get right, and got wrong.
                //
                // `sets apply` *replaces* a selection, so telling the user to
                // run it with one set — while parenthetically asking them to
                // keep the others — hands them a command that removes their
                // shell, tools, languages and packages. `upgrade` adds. The
                // same wrong advice was corrected in the preflight message in
                // round 27 and left standing here.
                //
                // And by this point the namespaces exist, so `lab up` cannot
                // simply be re-run: it fails on the first `ip netns add`. The
                // teardown has to come first, and saying so is the difference
                // between a recoverable failure and a lab that has to be
                // unpicked by hand.
                // The commands have to be runnable as printed.
                //
                // `lab down` and `lab up` both take the topology as a
                // positional argument, so the version without it failed Clap
                // parsing before doing anything — advice that cannot be
                // followed is worse than none, because it costs the reader the
                // time to find out. An explicitly chosen `--substrate` is
                // carried through for the same reason: dropping it sends the
                // retry at whichever box resolution picks by default.
                bail!(
                    "could not start the routing daemons in namespace '{node}': {}\n\n  \
                     A routed lab needs FRR on the substrate box:\n    \
                     devbox upgrade --name {substrate} --tools network\n\n  \
                     Then tear this partial lab down before retrying — its \
                     namespaces already exist, and `lab up` will not recreate \
                     them:\n    \
                     devbox lab down {target}{substrate_flag}\n    \
                     devbox lab up {target}{substrate_flag}",
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
    // that is already gone is not an error. A command that could not be *run*
    // is a different thing — the substrate is unreachable, and reporting a
    // clean teardown then would be a lie.
    // Counted per *namespace*, not per command. Each node contributes two
    // commands — kill what is running inside it, then delete it — and counting
    // both reported a clean three-node teardown as "6 of 6 namespaces
    // removed". The number was always exactly twice the truth, which is the
    // kind of wrong that looks right.
    let mut removed = 0;
    for cmd in &commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let deletes_namespace = argv.windows(2).any(|w| w == ["netns", "del"]);
        let result = runtime
            .exec_cmd(&substrate, &argv, false)
            .await
            .with_context(|| format!("could not reach substrate '{substrate}' to tear down"))?;
        if result.exit_code == 0 {
            if deletes_namespace {
                removed += 1;
            }
        } else if !result.stderr.contains("No such file or directory") {
            // A namespace that is already gone is the expected case after a
            // partial bring-up. One that is *busy*, or that permissions
            // refuse, is a real failure — and reporting a clean teardown then
            // left a live namespace behind while the policy metadata that
            // exempted it was removed.
            bail!(
                "could not remove namespace during teardown: {}",
                result.stderr.trim()
            );
        }
    }

    // The lab is gone, so its prefixes must stop being an exemption — an
    // `isolated` box would otherwise keep permitting a subnet nothing uses.
    // Checked, not fired and forgotten. If the metadata survives, the
    // isolated ruleset keeps exempting the subnet this lab just gave up — and
    // reapplying below would then re-install that exemption while the command
    // printed a clean teardown.
    let cleaned = runtime
        .exec_cmd(
            &substrate,
            &[
                "sh",
                "-c",
                &crate::policy::enforce::elevated(&format!(
                    "rm -rf /etc/devbox/lab/{}",
                    lab.name()
                )),
            ],
            false,
        )
        .await
        .with_context(|| format!("could not reach substrate '{substrate}' to clean up"))?;
    if cleaned.exit_code != 0 {
        bail!(
            "namespaces are gone, but the lab's policy metadata could not be \
             removed: {}\n  An isolated box would keep permitting this lab's \
             subnets. Remove /etc/devbox/lab/{} on '{substrate}' and re-run \
             `devbox policy set` to rebuild the ruleset.",
            cleaned.stderr.trim(),
            lab.name()
        );
    }

    // Reload, or the torn-down subnet stays permitted until something else
    // reapplies — and a routed network overlapping the old range would be
    // reachable from a box reporting itself isolated.
    reapply_policy(manager, &substrate).await?;

    println!(
        "Lab '{}' torn down ({removed} of {} namespaces removed).",
        lab.name(),
        commands
            .iter()
            .filter(|c| c.windows(2).any(|w| w == ["netns", "del"]))
            .count()
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

    // Healing a link that was never impaired is a no-op, not an error. A heal
    // that *failed* is a different thing: the fault is still in place, and
    // printing "healed" sends the user chasing a symptom they believe they
    // just fixed.
    let mut cleared = 0;
    for cmd in &commands {
        let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let result = runtime
            .exec_cmd(&substrate, &argv, false)
            .await
            .with_context(|| format!("could not reach substrate '{substrate}' to heal"))?;
        if result.exit_code == 0 {
            cleared += 1;
        } else if !result
            .stderr
            .contains("Cannot delete qdisc with handle of zero")
            && !result.stderr.contains("No such file or directory")
            && !result
                .stderr
                .contains("RTNETLINK answers: No such file or directory")
        {
            bail!(
                "could not heal {} ↔ {}: {}\n  The fault is still in place.",
                link.0,
                link.1,
                result.stderr.trim()
            );
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
    // `host` is documented but not a registered runtime — sandbox state only
    // ever records lima/incus/multipass/docker, and there is no host Runtime
    // to execute through. Treating it as a kind sent the user off to create a
    // box that cannot exist, so it is rejected by name instead.
    const RUNTIME_KINDS: &[&str] = &["lima", "incus", "multipass", "docker"];
    if configured == Some("host") && explicit.is_none() {
        bail!(
            "this topology asks for `substrate = \"host\"`, which devbox does not \
             implement — a lab runs inside a Linux box, not on the host.\n  \
             Set `substrate = \"auto\"` (or a box name), or pass `--substrate <name>`."
        );
    }
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

    // Every lab operation goes through this function and then straight to
    // `Runtime`, bypassing attach and exec — so a substrate started outside
    // devbox ran `lab up`, faults, and teardown with no posture at all. This
    // is the chokepoint, so the enforcement belongs here.
    crate::policy::enforce::apply_saved_or_step_aside(manager, &name).await?;

    Ok((runtime, name))
}

/// Re-apply the substrate's saved posture.
///
/// Called whenever the set of lab prefixes changes. The ruleset embeds those
/// prefixes (ADR-0046), so writing the file is only half the job — the table
/// in the kernel is what decides, and it does not reread anything.
async fn reapply_policy(manager: &SandboxManager, substrate: &str) -> Result<()> {
    // Strict, and that has to include contention.
    //
    // The comment below has always said this, and the code stepped aside — the
    // one place in the codebase where stepping aside is wrong. Everywhere else
    // the claim holder is obliged to restore the posture before it returns, so
    // skipping costs nothing. Here the prefixes file has *already* changed, and
    // the holder's restore reads a posture it captured before that change, or
    // has already run. The reload is not deferred by skipping it; it is lost.
    // `lab up` then leaves an isolated lab blocked, and `lab down` leaves a
    // stale subnet exemption behind.
    let claim = crate::web::build::claim_box(&manager.state_dir, substrate).context(
        "the lab's prefixes changed but the substrate is busy, so the firewall          could not be reloaded to match them",
    )?;
    // Strict: the lab's prefixes just changed, so a posture that fails to
    // reload is either exempting a subnet that no longer exists or blocking
    // one that does. Neither is something to print a success message over.
    crate::policy::enforce::restore_after_rebuild(manager, substrate, &claim).await
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
