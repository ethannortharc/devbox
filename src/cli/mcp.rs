//! `devbox mcp` — run an MCP server in a box (§7).
//!
//! An MCP server is the one process in an agent's workflow that the agent
//! starts itself, on the host, with the agent's own privileges. `mcp add`
//! records it, `mcp run` is what the agent launches instead, and the agent
//! cannot tell the difference: it gets a plain stdio MCP server on the other
//! end of a pipe. The box gets one more observed process under a posture.
//!
//! `--box` here is a flag rather than the usual `[NAME]` positional, and that
//! is deliberate. Everywhere else the box is *the box this command acts on*.
//! On `mcp add` it is a property of the registration being written — which box
//! this server will run in, later, when something else launches it — so it
//! reads as an option of the entry, and `mcp add <name>` keeps its own name in
//! the first slot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;

use crate::mcp::registry::{self, McpEntry};
use crate::mcp::shim::{self, ShimOptions};
use crate::obs::run::{RunKind, RunStatus, bootstrap};
use crate::policy::{Policy, Posture};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Register an MCP server to run inside a box
    Add(AddArgs),

    /// Run a registered server; this is what the agent launches
    Run(RunArgs),

    /// List registered servers
    Ls(LsArgs),

    /// Forget a registered server
    Rm(RmArgs),

    /// Show the report for a server's most recent run
    Report(ReportArgs),

    /// Run devbox's own MCP server, exposing runs and events to the agent
    #[command(name = "self")]
    SelfServer,
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Name of the server, as the agent will know it
    pub name: String,

    /// Box to run it in; defaults to the box registered for this project
    #[arg(long = "box", value_name = "BOX")]
    pub box_name: Option<String>,

    /// Egress posture to hold for the duration of the run
    #[arg(long)]
    pub posture: Option<String>,

    /// Register in ~/.devbox/mcp.toml, visible from every directory
    #[arg(long)]
    pub global: bool,

    /// The server's command and arguments
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Name of a registered server
    pub name: String,
}

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Emit JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Name of a registered server
    pub name: String,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// Name of a registered server
    pub name: String,

    /// Which rendering to print
    #[arg(long, value_enum, default_value = "md")]
    pub format: crate::cli::report::Format,

    /// Open the HTML report in a browser
    #[arg(long)]
    pub open: bool,
}

impl McpArgs {
    /// Only `run` touches a box, so only `run` starts the collector.
    ///
    /// The whole point of boxing an MCP server is that its syscalls, processes
    /// and connections land in the audit; a run whose events nothing collects
    /// would be sandboxing without the evidence.
    pub fn needs_collector(&self) -> bool {
        matches!(self.command, McpCommand::Run(_))
    }
}

pub async fn run(args: McpArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        McpCommand::Add(a) => add(a, manager).await,
        McpCommand::Run(a) => run_server(a, manager).await,
        McpCommand::Ls(a) => ls(a, manager),
        McpCommand::Rm(a) => rm(a, manager),
        McpCommand::Report(a) => report(a, manager).await,
        McpCommand::SelfServer => crate::mcp::rpc::serve(manager).await,
    }
}

/// The directory whose `devbox.toml` is the project registry.
///
/// The current directory — but only *a* registry, not the registry. An agent
/// launches `devbox mcp run <name>` from a working directory of its own
/// choosing, and Claude Code does not promise that is the project root, so a
/// project-scoped registration would resolve to "no such server" through no
/// fault of the user's. `~/.devbox/mcp.toml` is the fallback every directory
/// can see; see [`registry::Source`].
fn project_dir() -> Result<PathBuf> {
    std::env::current_dir().context("cannot determine the current directory")
}

/// What a freshly created `~/.devbox/mcp.toml` starts as.
///
/// A comment, because the file is otherwise indistinguishable from a project
/// config and someone will find it a year from now wondering what wrote it.
const GLOBAL_HEADER: &str = "\
# devbox — MCP servers registered for every directory.
#
# A project's own devbox.toml wins over this file for the same name.
# Written by `devbox mcp add --global`; edit by hand if you prefer.
";

// ── add ─────────────────────────────────────────────────

async fn add(args: AddArgs, manager: &SandboxManager) -> Result<()> {
    registry::validate_name(&args.name)?;
    shim::validate_command(&args.command)?;
    let posture = args
        .posture
        .as_deref()
        .map(str::parse::<Posture>)
        .transpose()?;

    let dir = project_dir()?;
    let path = if args.global {
        registry::global_path(&manager.state_dir)
    } else {
        let path = registry::config_path(&dir);
        if !path.exists() {
            // Not written for them. A file containing only `[mcp.…]` parses,
            // but every section it omits then reads as the *serde* default —
            // and the serde default for `[mounts]` is empty, not the workspace
            // mount `DevboxConfig::default()` carries. `devbox create` in that
            // directory would go on to build a box with nothing mounted, from
            // a config the user never asked for and would not think to check.
            // Generating a whole project config is `devbox init`'s job, and it
            // detects languages while it does it.
            bail!(
                "no devbox.toml in {}, and `mcp add` will not write a project config \
                 for you — run `devbox init` first, or register it for every \
                 directory with `devbox mcp add {} --global -- <command…>`",
                dir.display(),
                args.name
            );
        }
        path
    };
    let existed = path.exists();
    if !existed {
        // The global registry is ours, and creating it costs the user nothing:
        // unlike `devbox.toml` it means only what is in it.
        std::fs::create_dir_all(&manager.state_dir)
            .with_context(|| format!("failed to create {}", manager.state_dir.display()))?;
        crate::sandbox::state::write_atomically(&path, GLOBAL_HEADER.as_bytes(), "MCP registry")
            .with_context(|| format!("failed to create {}", path.display()))?;
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let entry = McpEntry {
        command: args.command.clone(),
        box_name: args.box_name.clone(),
        posture,
        env: Default::default(),
    };
    let replaced = registry::load_file(&path)?.contains_key(&args.name);
    let updated = registry::add_entry(&text, &args.name, &entry)?;
    crate::sandbox::state::write_atomically(&path, updated.as_bytes(), "MCP registry")
        .with_context(|| format!("failed to write {}", path.display()))?;

    if !existed {
        println!("{} {}", "Created".green().bold(), path.display());
    }
    // A global registration that a project entry already shadows would look
    // like it did nothing the next time the user ran it from that project.
    if args.global
        && let Ok(project) = registry::load(&dir)
        && project.contains_key(&args.name)
    {
        eprintln!(
            "\n{} {} also registers '{}', and a project entry wins there.",
            "Note:".yellow().bold(),
            registry::config_path(&dir).display(),
            args.name
        );
    }
    println!(
        "{} MCP server '{}' → box '{}'",
        if replaced { "Updated" } else { "Registered" }
            .green()
            .bold(),
        args.name,
        entry
            .box_name
            .clone()
            .unwrap_or_else(|| "(this project)".to_string()),
    );
    println!("  command: {}", args.command.join(" "));
    println!("  in: {}", path.display());
    if let Some(posture) = posture {
        println!("  posture: {posture}");
        warn_about_posture_on_a_project_box(manager, &entry, posture);
    }

    warn_if_the_box_cannot_run_it(manager, &entry).await;

    println!("\nTell the agent to use it:");
    println!(
        "  claude mcp add {} -- devbox mcp run {}",
        args.name, args.name
    );
    println!(
        "  codex mcp add {} -- devbox mcp run {}",
        args.name, args.name
    );
    println!(
        "\nOr in Codex's ~/.codex/config.toml:\n  [mcp_servers.{}]\n  command = \"devbox\"\n  args = [\"mcp\", \"run\", \"{}\"]",
        args.name, args.name
    );
    Ok(())
}

/// The Nix set that would put `program` on a box's PATH.
///
/// The two that matter are `uvx` and `npx`: between them they launch most of
/// the MCP servers anyone publishes, and neither is in a `--bare` box. Naming
/// the set is the difference between "command not found" three days later,
/// inside an agent's log, and a one-line fix now.
fn set_that_provides(program: &str) -> Option<(&'static str, &'static str)> {
    let program = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let set = match program {
        "uvx" | "uv" | "python" | "python3" | "pip" | "pip3" | "pipx" | "ruff" | "pyright"
        | "ipython" | "pytest" => ("python", "uv, uvx and python3"),
        "npx" | "node" | "npm" | "pnpm" | "bun" | "bunx" | "tsc" | "typescript" => {
            ("node", "nodejs, npx, bun and pnpm")
        }
        "go" | "gofmt" => ("go", "the Go toolchain"),
        "cargo" | "rustc" | "rustup" => ("rust", "the Rust toolchain"),
        "java" | "javac" | "mvn" | "gradle" => ("java", "a JDK"),
        "ruby" | "gem" | "bundle" | "bundler" => ("ruby", "Ruby and bundler"),
        "docker" | "podman" | "docker-compose" => ("container", "docker and compose"),
        "git" => ("git", "git"),
        _ => return None,
    };
    Some(set)
}

/// Say now, not at the agent's first tool call, that the box has no `uvx`.
///
/// Only against a box that is already running: registering a server is not a
/// reason to boot a VM, and an unstartable or absent box is not an error here
/// — `mcp add` records an intention, and the box can be built afterwards.
async fn warn_if_the_box_cannot_run_it(manager: &SandboxManager, entry: &McpEntry) {
    let Some(program) = entry.command.first() else {
        return;
    };
    let Some(box_name) = entry.box_name.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|dir| manager.name_from_dir(&dir))
    }) else {
        return;
    };
    let Ok(state) = manager.get_sandbox(&box_name) else {
        return;
    };
    let Ok(runtime) = manager.runtime_for_sandbox(&state) else {
        return;
    };
    if !matches!(
        runtime.status(&box_name).await,
        Ok(crate::runtime::SandboxStatus::Running)
    ) {
        return;
    }

    // `sh -lc` with the program as `$1`, so nothing about it is re-parsed —
    // and a login shell, because a NixOS box keeps its tools on a PATH that
    // only the profile sets.
    let probe = [
        "sh",
        "-lc",
        "command -v \"$1\" >/dev/null 2>&1",
        "devbox-mcp-probe",
        program,
    ];
    let found = tokio::time::timeout(
        Duration::from_secs(30),
        runtime.exec_cmd(&box_name, &probe, false),
    )
    .await;
    // A probe that could not run says nothing about the box; only a clean
    // "not found" is worth a warning.
    let Ok(Ok(result)) = found else { return };
    if result.exit_code == 0 {
        return;
    }

    eprintln!(
        "\n{} box '{box_name}' has no '{program}' on its PATH.",
        "Warning:".yellow().bold()
    );
    match set_that_provides(program) {
        Some((set, contents)) => {
            eprintln!("  It comes with the '{set}' set ({contents}). Add it with:");
            eprintln!("    devbox upgrade {box_name} --tools {set}");
        }
        None => {
            eprintln!("  Install it in the box, or add the set that carries it:");
            eprintln!("    devbox sets list {box_name}");
        }
    }
}

/// §7.2: posture is box-granular, so a posture on a shared box moves the whole
/// box — including the editor, the shell, and whatever else is open in it.
///
/// The warning is about the *target* box being a project box, not about the
/// posture being unusual. A dedicated `mcp-tools` box is the recommended
/// layout precisely because nothing else is disturbed when it switches.
fn warn_about_posture_on_a_project_box(
    manager: &SandboxManager,
    entry: &McpEntry,
    posture: Posture,
) {
    let target = entry.box_name.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|dir| manager.name_from_dir(&dir))
    });
    let Some(target) = target else { return };
    let is_project_box = manager
        .get_sandbox(&target)
        .map(|state| state.project_dir.join("devbox.toml").exists())
        .unwrap_or(true);
    if !is_project_box {
        return;
    }
    eprintln!(
        "\n{} '{target}' looks like a project box, and a posture is box-granular:",
        "Warning:".yellow().bold()
    );
    eprintln!("  every shell, editor and build in it moves to '{posture}' while this");
    eprintln!("  server runs, and back when it exits. For a posture of its own, give");
    eprintln!("  the MCP servers their own box:");
    eprintln!("    devbox create mcp-tools --bare");
    eprintln!("    devbox mcp add <name> --box mcp-tools --posture {posture} -- <cmd…>");
}

// ── ls / rm ─────────────────────────────────────────────

fn ls(args: LsArgs, manager: &SandboxManager) -> Result<()> {
    let dir = project_dir()?;
    let rows = registry::visible(&dir, &manager.state_dir)?;
    // A name in both files appears twice, and only the project one is what
    // `mcp run` would launch.
    let shadowed: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter(|(name, _, source)| {
            matches!(source, registry::Source::Global(_))
                && rows.iter().any(|(other, _, source)| {
                    other == name && matches!(source, registry::Source::Project(_))
                })
        })
        .map(|(name, _, _)| name.as_str())
        .collect();

    if args.json {
        let rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, entry, source)| {
                serde_json::json!({
                    "name": name,
                    "box": entry.box_name,
                    "posture": entry.posture.map(|p| p.as_str()),
                    "command": entry.command,
                    "source": source.label(),
                    "file": source.path(),
                    "shadowed": matches!(source, registry::Source::Global(_))
                        && shadowed.contains(name.as_str()),
                    "log": log_path(manager, name),
                    "last_log": last_log_time(manager, name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "No MCP servers registered in {} or {}",
            registry::config_path(&dir).display(),
            registry::global_path(&manager.state_dir).display(),
        );
        println!("  devbox mcp add <name> -- <command…>");
        println!("  devbox mcp add <name> --global -- <command…>");
        return Ok(());
    }

    println!(
        "{:<16} {:<9} {:<14} {:<12} {:<20} {}",
        "NAME".bold(),
        "SOURCE".bold(),
        "BOX".bold(),
        "POSTURE".bold(),
        "LAST LOG".bold(),
        "COMMAND".bold()
    );
    for (name, entry, source) in &rows {
        let label =
            if matches!(source, registry::Source::Global(_)) && shadowed.contains(name.as_str()) {
                "global*"
            } else {
                source.label()
            };
        println!(
            "{:<16} {:<9} {:<14} {:<12} {:<20} {}",
            name,
            label,
            entry.box_name.as_deref().unwrap_or("(project)"),
            entry.posture.map(|p| p.as_str()).unwrap_or("(box's own)"),
            last_log_time(manager, name).unwrap_or_else(|| "-".to_string()),
            one_line(&entry.command),
        );
    }
    if !shadowed.is_empty() {
        println!(
            "\n* shadowed here by the project entry of the same name: {}",
            shadowed.iter().copied().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// A command as one row of a table.
///
/// `command` is an argv vector and an argument may contain anything, newlines
/// included — a hand-written `[mcp.x]` with a small shell script in it is a
/// reasonable thing for a user to write. Printed raw, one such entry turns the
/// listing into something that is no longer a table.
fn one_line(command: &[String]) -> String {
    command
        .join(" ")
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn rm(args: RmArgs, manager: &SandboxManager) -> Result<()> {
    let dir = project_dir()?;
    // The one `mcp run` would have launched, so removing it is removing the
    // thing the user can see working.
    let Some((_, source)) = registry::find(&dir, &manager.state_dir, &args.name)? else {
        bail!(
            "no MCP server named '{}' in {} or {}; `devbox mcp ls` shows what is registered",
            args.name,
            registry::config_path(&dir).display(),
            registry::global_path(&manager.state_dir).display(),
        );
    };
    let path = source.path().to_path_buf();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let Some(updated) = registry::remove_entry(&text, &args.name)? else {
        bail!(
            "'{}' was in {} a moment ago and is not now; nothing was changed",
            args.name,
            path.display()
        );
    };
    crate::sandbox::state::write_atomically(&path, updated.as_bytes(), "MCP registry")
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!(
        "{} MCP server '{}' from {}",
        "Removed".green().bold(),
        args.name,
        path.display()
    );

    // Removing the project entry can *uncover* a global one, and a `mcp run`
    // that goes on working after a `mcp rm` needs explaining.
    if matches!(source, registry::Source::Project(_))
        && let Ok(Some((_, uncovered))) = registry::find(&dir, &manager.state_dir, &args.name)
    {
        println!(
            "  {} still registers '{}', so `devbox mcp run {}` keeps working.",
            uncovered.path().display(),
            args.name,
            args.name
        );
    }
    // The log stays. It is the record of what that server did, and `mcp rm` is
    // a change to the registry, not a request to destroy evidence.
    println!("  its log is kept at {}", log_path_display(&args.name));
    Ok(())
}

// ── report ──────────────────────────────────────────────

/// `devbox mcp report <name>` — the last run this server had (§7.1).
///
/// A registration is a *name*, not a run, so this is two lookups: which box
/// the server runs in, then the most recent `kind = mcp` run there carrying
/// that name as its label. Delegating the rendering to `devbox report` is what
/// keeps the two commands from growing different ideas of what a report is —
/// including the stored-JSON-first fallback that makes a report readable after
/// its events have aged out of the store.
async fn report(args: ReportArgs, manager: &SandboxManager) -> Result<()> {
    let dir = project_dir()?;
    let (entry, _) = registry::find(&dir, &manager.state_dir, &args.name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no MCP server named '{}' in {} or {}",
            args.name,
            registry::config_path(&dir).display(),
            registry::global_path(&manager.state_dir).display(),
        )
    })?;
    let box_name = match &entry.box_name {
        Some(name) => name.clone(),
        None => manager.resolve_name(None)?,
    };

    let path = crate::obs::collector::store_path(&manager.state_dir, &box_name);
    let run_id = if path.exists() {
        crate::obs::Store::open(&path)
            .with_context(|| format!("failed to open the event store for box '{box_name}'"))?
            .list_runs(RUN_SEARCH_DEPTH)?
            .into_iter()
            .find(|run| run.kind_enum() == RunKind::Mcp && run.label == args.name)
            .map(|run| run.run_id)
    } else {
        None
    };
    let Some(run_id) = run_id else {
        bail!(
            "MCP server '{}' has no recorded run on box '{box_name}' yet. \
             An agent has to start it once — `devbox mcp run {}` is what \
             `claude mcp add` wires up — before there is anything to report.",
            args.name,
            args.name
        );
    };

    crate::cli::report::run(
        crate::cli::report::ReportArgs {
            run_id,
            format: args.format,
            open: args.open,
            name: Some(box_name),
        },
        manager,
    )
    .await
}

/// How far back `mcp report` looks for a server's last run.
///
/// A busy box records a run per `exec` and per shell, so the newest `mcp` run
/// carrying a given label can be some way down the list. Bounded because the
/// answer is "the most recent one", and a server that has not run in two
/// hundred runs is one the message about having never run describes just as
/// well.
const RUN_SEARCH_DEPTH: usize = 200;

// ── run ─────────────────────────────────────────────────

/// The shim. Everything it prints goes to stderr: stdout is the agent's
/// JSON-RPC channel and one stray `println!` corrupts the session.
async fn run_server(args: RunArgs, manager: &SandboxManager) -> Result<()> {
    let dir = project_dir()?;
    // The project registry first, then the global one. The agent chose this
    // working directory, not the user, so "not found" here must mean the name
    // is in neither file rather than that the agent started somewhere else.
    let (entry, source) =
        registry::find(&dir, &manager.state_dir, &args.name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no MCP server named '{}' in {} or {}; register it with \
                 `devbox mcp add {} -- <command…>`, or with `--global` to make it \
                 visible from every directory",
                args.name,
                registry::config_path(&dir).display(),
                registry::global_path(&manager.state_dir).display(),
                args.name
            )
        })?;
    shim::validate_command(&entry.command)?;
    tracing::debug!(server = %args.name, source = %source.path().display(), "resolved MCP server");

    let box_name = match &entry.box_name {
        Some(name) => name.clone(),
        None => manager.resolve_name(None)?,
    };
    if !manager.sandbox_exists(&box_name) {
        bail!(
            "MCP server '{}' is registered for box '{box_name}', which does not exist; \
             create it with `devbox create {box_name}`",
            args.name
        );
    }

    // Lazy start under the box claim, exactly as the console's Terminal tab
    // does — and under the *same* claim as the posture override below, so a
    // concurrent `devbox use` or rebuild cannot land between them.
    let claim = wait_for_the_box_claim(manager, &box_name).await?;
    crate::web::service::ensure_running_holding_claim(manager, &box_name, &claim).await?;

    let state = manager.get_sandbox(&box_name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    // ADR-0047 in miniature: only reverse a switch that happened. The saved
    // posture is read here and restored at the end *only* if the override was
    // both different and successfully applied — restoring one we never
    // installed would reinstall a stale posture over whatever changed while
    // the server ran.
    let saved: Policy = crate::sandbox::config::DevboxConfig::load_for_edit(&state.project_dir)
        .with_context(|| {
            format!("cannot read the egress posture of box '{box_name}' from its devbox.toml")
        })?
        .policy;
    let mut switched = false;
    if let Some(posture) = entry.posture
        && posture != saved.egress
    {
        let overridden = Policy {
            egress: posture,
            ..saved.clone()
        };
        crate::policy::enforce::apply(runtime.as_ref(), &box_name, &overridden)
            .await
            .with_context(|| {
                format!(
                    "MCP server '{}' asks for posture '{posture}' on box '{box_name}', \
                     and it could not be applied; refusing to run the server under the \
                     box's current '{}' instead",
                    args.name, saved.egress
                )
            })?;
        switched = true;
        eprintln!(
            "devbox mcp: box '{box_name}' posture {} → {posture} for this run",
            saved.egress
        );
    }
    // Released before the long-lived process: the claim protects the
    // preparation boundary, not the session, and an MCP session lasts hours.
    drop(claim);

    // ── the run (§4.6, §7.1) ──
    //
    // An MCP server is a run like any other, and `kind = mcp` is the one that
    // makes `mcp report` possible. The row goes in before the server starts so
    // the collector is already attributing when its first event arrives.
    //
    // Fatal if it cannot be recorded, deliberately. The product is a box whose
    // side effects are visible; a sandboxed MCP server that nothing is
    // recording is the one thing this command exists to prevent, and starting
    // it anyway would be sandboxing with the evidence quietly switched off.
    let run_id = crate::obs::run::new_run_id();
    let started_at = crate::cli::run::now();
    crate::cli::run::insert_wrapped_run(
        manager,
        &box_name,
        &run_id,
        RunKind::Mcp,
        &entry.command,
        crate::cli::run::GUEST_CWD,
        &args.name,
        &started_at,
        saved.egress,
        entry.posture.unwrap_or(saved.egress),
    )
    .with_context(|| {
        format!(
            "MCP server '{}' was not started because its run could not be recorded",
            args.name
        )
    })?;
    let dropped_before = crate::obs::daemon::stats_snapshot(manager)
        .map(|s| s.dropped + s.persist_failed)
        .unwrap_or(0);
    let checkpoint_start =
        crate::cli::run::take_checkpoint(manager, &state, &box_name, &run_id, "run-start").await;
    let readback = crate::cli::run::spawn_scope_readback(
        manager,
        manager.runtime_for_sandbox(&state)?,
        &box_name,
        &run_id,
        &started_at,
    );

    // The environment the server sees: the broker's, this run's id, then the
    // registration's own — last, so a `[mcp.x] env` entry wins over a name the
    // broker happened to use.
    let mut env = crate::cli::run::run_env(manager, runtime.as_ref(), &box_name, &run_id).await;
    for (key, value) in &entry.env {
        env.retain(|(existing, _)| existing != key);
        env.push((key.clone(), value.clone()));
    }

    // Three layers, and the order is the whole design:
    //
    //   sh -c <pgid wrapper>            ← mine: records the process group
    //     sh -c <A's run bootstrap>     ← A's:  enters the run's cgroup
    //       env -- K=V … <server>       ← the server itself
    //
    // A's wrapper *inside* mine, so the run's cgroup covers the server and
    // everything it spawns, while the process group I recorded still covers
    // A's wrapper too — which is what lets the reaper take the whole tree down
    // when the transport has to be killed.
    let pgid_file = shim::pgid_file_path(&args.name);
    let mut inner = bootstrap(&run_id, crate::cli::run::GUEST_CWD);
    inner.extend(crate::broker::with_env(&env, &entry.command));
    let guest = shim::wrap_guest_command(&pgid_file, std::iter::empty(), &inner);
    let guest_refs: Vec<&str> = guest.iter().map(String::as_str).collect();
    let argv = runtime.argv(&box_name, &guest_refs, false);

    let log = log_path(manager, &args.name);
    let mut options = ShimOptions::new(log.clone());
    options.mirror_stderr = shim::stderr_is_a_terminal();

    let outcome = shim::pump(
        &argv,
        tokio::io::stdin(),
        tokio::io::stdout(),
        &options,
        shim::termination_signal(),
    )
    .await;
    let ended_at = crate::cli::run::now();

    // Cleanup runs whatever happened above, including a shim that failed to
    // start: `wrap_guest_command` may already have written the file.
    reap(runtime.as_ref(), &box_name, &pgid_file, &outcome).await;
    let scope = readback.await.ok().flatten();
    if switched {
        restore_posture(manager, &box_name).await;
    }
    close_the_run(
        manager,
        &state,
        &box_name,
        &run_id,
        &ended_at,
        &outcome,
        checkpoint_start,
        dropped_before,
        scope.is_none(),
    )
    .await;

    // Always exit explicitly, including on success.
    //
    // `tokio::io::stdin()` reads on a blocking-pool thread, and a read parked
    // on the agent's pipe cannot be cancelled. Returning normally would drop
    // the runtime, and dropping a runtime waits for its blocking tasks — so a
    // session that is over in every meaningful sense would hang on an agent
    // that has not closed its end. A shim whose stdin belongs to somebody else
    // does not get to unwind.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    match outcome {
        Ok(outcome) => {
            if outcome.exit_code != 0 {
                report_failure(&args.name, &log, outcome.exit_code);
            }
            std::process::exit(outcome.exit_code)
        }
        Err(error) => {
            eprintln!("devbox mcp: {error:#}");
            std::process::exit(1)
        }
    }
}

/// How long `mcp run` waits for the box claim before giving up.
///
/// Long enough to sit behind a box that is starting (a cold Lima VM is tens of
/// seconds), short enough that a genuinely stuck rebuild is reported rather
/// than waited on forever.
const CLAIM_WAIT: Duration = Duration::from_secs(120);

/// Take the per-box claim, waiting for it rather than refusing on contention.
///
/// Everywhere else in devbox this claim refuses immediately, and that is right:
/// a second `devbox sets apply` on one box is a mistake, not a queue. Here it
/// is neither. An agent starts *all* of its configured MCP servers at once, and
/// several of them on one box is the recommended layout (§7.2) — so contention
/// is the normal case, the holder is another shim doing a lazy start that takes
/// milliseconds, and refusing would fail every server but the first with a
/// message about rebuilds that are not happening.
///
/// Waiting here cannot deadlock: this holds nothing while it waits, and the
/// box-then-project order every other path takes is unchanged once it has the
/// claim.
async fn wait_for_the_box_claim(
    manager: &SandboxManager,
    box_name: &str,
) -> Result<crate::web::build::BoxClaim> {
    let deadline = std::time::Instant::now() + CLAIM_WAIT;
    let mut announced = false;
    loop {
        // `?`, not a shrug: `Ok(None)` is contention, and an `Err` means the
        // claim could not be evaluated at all — an unwritable state directory
        // is not something to sit in a loop over.
        if let Some(claim) = crate::web::build::try_claim_box(&manager.state_dir, box_name)? {
            return Ok(claim);
        }
        if std::time::Instant::now() >= deadline {
            // `concat!` with positional arguments, not a `\`-continued literal:
            // rustfmt joins those back onto one line and keeps the
            // continuation's indentation *inside* the string, so the message
            // reaches the user with a run of twenty spaces in the middle of a
            // sentence. It did, for one release of this very message.
            bail!(
                concat!(
                    "box '{}' has been busy for {}s, so this MCP server was not ",
                    "started. Another devbox process is holding it — a rebuild, ",
                    "a `devbox use`, or a start that is not finishing.",
                ),
                box_name,
                CLAIM_WAIT.as_secs()
            );
        }
        if !announced {
            eprintln!("devbox mcp: waiting for box '{box_name}' to be free…");
            announced = true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Close the run out and write its report.
///
/// Everything here is best effort and nothing is printed to stdout: the run is
/// evidence about a session that has already ended, and the session's stdout
/// belongs to the agent. A report that cannot be written is a warning on
/// stderr, not a reason to change the exit code the agent sees.
#[allow(clippy::too_many_arguments)]
async fn close_the_run(
    manager: &SandboxManager,
    state: &crate::sandbox::state::SandboxState,
    box_name: &str,
    run_id: &str,
    ended_at: &str,
    outcome: &Result<shim::ShimOutcome>,
    checkpoint_start: Option<crate::sandbox::checkpoint::Checkpoint>,
    dropped_before: u64,
    no_scope: bool,
) {
    // The last batch is still in the collector's linger window.
    tokio::time::sleep(crate::cli::run::SETTLE).await;

    let (exit_code, ended_by) = match outcome {
        Ok(outcome) => (
            Some(outcome.exit_code),
            Some(outcome.stopped_by.ended_by(outcome.forced)),
        ),
        // The transport never started or died in a way we could not read. The
        // run is `aborted`, and we do not know who ended it.
        Err(_) => (None, None),
    };
    let status = if exit_code.is_some() {
        RunStatus::Finished
    } else {
        RunStatus::Aborted
    };

    let health = crate::obs::health::load(&manager.state_dir, box_name)
        .ok()
        .flatten();
    let sources = health
        .as_ref()
        .map(crate::obs::health::capture_composition)
        .unwrap_or_default();
    let agent_version = health.map(|h| h.agent_version).unwrap_or_default();
    let dropped = crate::obs::daemon::stats_snapshot(manager)
        .map(|s| (s.dropped + s.persist_failed).saturating_sub(dropped_before))
        .unwrap_or(0);

    let checkpoint_end =
        crate::cli::run::take_checkpoint(manager, state, box_name, run_id, "run-end").await;

    let store = match crate::obs::Store::open(&crate::obs::collector::store_path(
        &manager.state_dir,
        box_name,
    )) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("devbox mcp: could not close the run: {error:#}");
            return;
        }
    };
    if checkpoint_start.is_some() || checkpoint_end.is_some() {
        let start = checkpoint_start.as_ref().map(|c| c.id.as_str());
        let end = checkpoint_end.as_ref().map(|c| c.id.as_str());
        if let Err(e) = store.set_run_checkpoints(run_id, start, end) {
            tracing::warn!(error = %e, "could not record a run's checkpoints");
        }
    }
    if let Err(error) = store.finish_run(
        run_id,
        ended_at,
        exit_code,
        status,
        &sources,
        &agent_version,
        dropped,
        ended_by,
    ) {
        eprintln!("devbox mcp: could not close the run: {error:#}");
    }

    if let Ok(runtime) = manager.runtime_for_sandbox(state) {
        let argv = crate::obs::run::cleanup_argv(run_id);
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let _ = runtime.exec_cmd(box_name, &refs, false).await;
    }
    if no_scope {
        eprintln!(
            "devbox mcp: box '{box_name}' did not report a cgroup for this run, so its \
             events were attributed by process ancestry only."
        );
    }

    match crate::cli::run::build_report(manager, &store, run_id, box_name, state).await {
        Ok(Some(report)) => match crate::report::write(&manager.state_dir, &report) {
            Ok(rendered) => eprintln!("devbox mcp: report {}", rendered.html.display()),
            Err(error) => eprintln!("devbox mcp: could not write the run report: {error:#}"),
        },
        Ok(None) => {}
        Err(error) => eprintln!("devbox mcp: could not build the run report: {error:#}"),
    }
}

/// Kill whatever the guest still has running, and remove the marker file.
///
/// Always, not only after a forced shutdown: a clean exit still leaves the
/// marker in the box's `/tmp`, and the reaper's first act is to delete it. It
/// is best effort and bounded — the session is over either way, and a box that
/// has gone away must not turn a finished session into a hang.
async fn reap(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
    pgid_file: &str,
    outcome: &Result<shim::ShimOutcome>,
) {
    let script = shim::reaper_script(pgid_file);
    let refs: Vec<&str> = script.iter().map(String::as_str).collect();
    let forced = matches!(outcome, Ok(o) if o.forced) || outcome.is_err();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        runtime.exec_cmd(box_name, &refs, false),
    )
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) if forced => eprintln!(
            "devbox mcp: could not clean up the server's process group in '{box_name}': {error:#}"
        ),
        Err(_) if forced => eprintln!(
            "devbox mcp: timed out cleaning up the server's process group in '{box_name}'"
        ),
        _ => {}
    }
}

async fn restore_posture(manager: &SandboxManager, box_name: &str) {
    let claim = match crate::web::build::claim_box(&manager.state_dir, box_name) {
        Ok(claim) => claim,
        Err(error) => {
            eprintln!(
                "devbox mcp: box '{box_name}' is busy, so its posture was left as this \
                 server set it: {error:#}. Run `devbox policy set <posture> {box_name}`"
            );
            return;
        }
    };
    if let Err(error) = crate::policy::enforce::apply_saved(manager, box_name, &claim).await {
        eprintln!(
            "devbox mcp: box '{box_name}' is still on the posture this server set — \
             its own could not be restored: {error:#}. Run \
             `devbox policy set <posture> {box_name}`"
        );
    }
}

fn report_failure(name: &str, log: &Path, code: i32) {
    eprintln!("devbox mcp: server '{name}' exited {code}");
    match shim::log_tail(log, 5) {
        Ok(lines) if !lines.is_empty() => {
            eprintln!("  last lines of {}:", log.display());
            for line in lines {
                eprintln!("    {line}");
            }
        }
        _ => eprintln!("  see {}", log.display()),
    }
}

// ── logs ────────────────────────────────────────────────

fn log_path(manager: &SandboxManager, name: &str) -> PathBuf {
    manager.state_dir.join("mcp").join(format!("{name}.log"))
}

fn log_path_display(name: &str) -> String {
    format!("~/.devbox/mcp/{name}.log")
}

/// When the server last said anything, from the log's mtime.
fn last_log_time(manager: &SandboxManager, name: &str) -> Option<String> {
    let modified = std::fs::metadata(log_path(manager, name))
        .ok()?
        .modified()
        .ok()?;
    Some(
        chrono::DateTime::<chrono::Utc>::from(modified)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::{CommandFactory, Parser};

    use super::*;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} did not parse: {e}"))
    }

    #[test]
    fn add_takes_the_server_name_first_and_the_command_after_a_dash_dash() {
        let cli = parse(&[
            "devbox",
            "mcp",
            "add",
            "fetch",
            "--box",
            "mcp-tools",
            "--posture",
            "mirror-only",
            "--",
            "uvx",
            "mcp-server-fetch",
            "--flag",
        ]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!("not an mcp command")
        };
        let McpCommand::Add(add) = args.command else {
            panic!("not an add")
        };
        assert_eq!(add.name, "fetch");
        assert_eq!(add.box_name.as_deref(), Some("mcp-tools"));
        assert_eq!(add.posture.as_deref(), Some("mirror-only"));
        assert_eq!(add.command, ["uvx", "mcp-server-fetch", "--flag"]);
    }

    /// The server's own flags must reach the server, not clap.
    #[test]
    fn the_servers_flags_are_not_devboxs() {
        let cli = parse(&[
            "devbox",
            "mcp",
            "add",
            "srv",
            "--",
            "server",
            "--posture",
            "nonsense",
            "--box",
            "x",
        ]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Add(add) = args.command else {
            panic!()
        };
        assert_eq!(add.posture, None);
        assert_eq!(add.box_name, None);
        assert_eq!(
            add.command,
            ["server", "--posture", "nonsense", "--box", "x"]
        );
    }

    /// `--box` selects the box the *entry* names, so it stays a flag while the
    /// rest of the CLI went positional in W0-4.
    #[test]
    fn box_is_a_flag_here_and_the_first_positional_is_the_server_name() {
        let cli = parse(&["devbox", "mcp", "add", "devtest", "--", "cat"]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Add(add) = args.command else {
            panic!()
        };
        assert_eq!(
            add.name, "devtest",
            "the positional is the server, not the box"
        );
        assert_eq!(add.box_name, None);
    }

    #[test]
    fn global_is_a_flag_on_add_and_off_by_default() {
        let cli = parse(&[
            "devbox", "mcp", "add", "fetch", "--global", "--", "uvx", "srv",
        ]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Add(add) = args.command else {
            panic!()
        };
        assert!(add.global);

        let cli = parse(&["devbox", "mcp", "add", "fetch", "--", "uvx", "srv"]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Add(add) = args.command else {
            panic!()
        };
        assert!(
            !add.global,
            "a registration is project-scoped unless asked otherwise"
        );
    }

    /// `--global` after `--` belongs to the server, like every other flag.
    #[test]
    fn the_servers_own_global_flag_is_not_devboxs() {
        let cli = parse(&["devbox", "mcp", "add", "srv", "--", "server", "--global"]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Add(add) = args.command else {
            panic!()
        };
        assert!(!add.global);
        assert_eq!(add.command, ["server", "--global"]);
    }

    #[test]
    fn run_and_rm_require_a_name() {
        for verb in ["run", "rm"] {
            let error = Cli::try_parse_from(["devbox", "mcp", verb])
                .expect_err(&format!("mcp {verb} with no name should be an error"));
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }
    }

    #[test]
    fn add_requires_a_command_after_the_dash_dash() {
        let error = Cli::try_parse_from(["devbox", "mcp", "add", "fetch"])
            .expect_err("a registration with no command is not one");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    /// The whole surface §7.1 specifies, and nothing that is not implemented.
    #[test]
    fn the_help_offers_the_whole_component() {
        let mcp = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "mcp")
            .expect("mcp is registered")
            .clone();
        let names: Vec<&str> = mcp.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(
            names,
            ["add", "run", "ls", "rm", "report", "self"],
            "the mcp surface changed"
        );
    }

    /// `mcp self` is a server, not a box command: it takes no box and starts no
    /// collector, because it only *reads* what other runs recorded.
    #[test]
    fn self_takes_no_arguments_and_starts_no_collector() {
        let cli = parse(&["devbox", "mcp", "self"]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        assert!(matches!(args.command, McpCommand::SelfServer));
        assert!(!args.needs_collector());
    }

    #[test]
    fn report_takes_the_server_name_and_a_format() {
        let cli = parse(&["devbox", "mcp", "report", "fetch", "--format", "json"]);
        let Some(Command::Mcp(args)) = cli.command else {
            panic!()
        };
        let McpCommand::Report(report) = args.command else {
            panic!("not a report")
        };
        assert_eq!(report.name, "fetch");
        assert!(matches!(report.format, crate::cli::report::Format::Json));
        assert!(!report.open);
    }

    /// The two that decide whether a published MCP server can run at all.
    #[test]
    fn uvx_and_npx_name_the_set_that_carries_them() {
        assert_eq!(set_that_provides("uvx").map(|s| s.0), Some("python"));
        assert_eq!(set_that_provides("npx").map(|s| s.0), Some("node"));
        // Registered as an absolute path, which is how a user pins one.
        assert_eq!(
            set_that_provides("/home/ethan/.local/bin/uvx").map(|s| s.0),
            Some("python")
        );
        assert_eq!(set_that_provides("mcp-server-fetch"), None);
    }

    /// An agent starts every configured MCP server at once, and several on
    /// one box is the layout §7.2 recommends. A claim that refused on
    /// contention would fail all but the first with a message about a rebuild
    /// that is not happening.
    #[tokio::test]
    async fn a_second_shim_waits_for_the_box_claim_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        let held = crate::web::build::claim_box(&manager.state_dir, "shared").unwrap();

        let state_dir = manager.state_dir.clone();
        let waiter = tokio::spawn(async move {
            let manager = SandboxManager { state_dir };
            wait_for_the_box_claim(&manager, "shared").await.map(|_| ())
        });

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !waiter.is_finished(),
            "the second shim gave up instead of waiting for the box"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("the claim was released and the waiter never took it")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn a_multi_line_command_still_prints_as_one_row() {
        let command = vec!["sh".to_string(), "-c".to_string(), "a\nb\tc\\d".to_string()];
        let rendered = one_line(&command);
        assert!(!rendered.contains('\n'), "{rendered}");
        assert_eq!(rendered, "sh -c a\\nb\\tc\\\\d");
    }

    #[test]
    fn only_run_starts_the_collector() {
        let collector = |argv: &[&str]| {
            let Some(Command::Mcp(args)) = parse(argv).command else {
                panic!()
            };
            args.needs_collector()
        };
        assert!(collector(&["devbox", "mcp", "run", "fetch"]));
        assert!(!collector(&["devbox", "mcp", "ls"]));
        assert!(!collector(&["devbox", "mcp", "rm", "fetch"]));
        assert!(!collector(&["devbox", "mcp", "add", "f", "--", "cat"]));
    }
}
