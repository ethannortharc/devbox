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
        McpCommand::Rm(a) => rm(a),
    }
}

/// The project whose `devbox.toml` holds the registry.
///
/// The current directory, which is also where the agent runs — `claude mcp add
/// … -- devbox mcp run fetch` records a command that the agent later launches
/// from its own project root.
fn project_dir() -> Result<PathBuf> {
    std::env::current_dir().context("cannot determine the current directory")
}

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
    let path = registry::config_path(&dir);
    let existed = path.exists();
    if !existed {
        // A project with no `devbox.toml` still gets one, written the way
        // `devbox init` writes it, rather than a stub. A file containing only
        // `[mcp.…]` parses, but every section it omits then reads as the serde
        // default — and the serde default for `[mounts]` is *empty*, not the
        // workspace mount `DevboxConfig::default()` carries. `devbox create`
        // in that directory would silently build a box with nothing mounted.
        crate::sandbox::config::DevboxConfig::default()
            .save(&path)
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
    let replaced = registry::load(&dir)?.contains_key(&args.name);
    let updated = registry::add_entry(&text, &args.name, &entry)?;
    crate::sandbox::state::write_atomically(&path, updated.as_bytes(), "devbox config")
        .with_context(|| format!("failed to write {}", path.display()))?;

    if !existed {
        println!("{} {}", "Created".green().bold(), path.display());
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
    let table = registry::load(&dir)?;

    if args.json {
        let rows: Vec<serde_json::Value> = table
            .iter()
            .map(|(name, entry)| {
                serde_json::json!({
                    "name": name,
                    "box": entry.box_name,
                    "posture": entry.posture.map(|p| p.as_str()),
                    "command": entry.command,
                    "log": log_path(manager, name),
                    "last_log": last_log_time(manager, name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if table.is_empty() {
        println!(
            "No MCP servers registered in {}",
            registry::config_path(&dir).display()
        );
        println!("  devbox mcp add <name> -- <command…>");
        return Ok(());
    }

    println!(
        "{:<16} {:<14} {:<12} {:<20} {}",
        "NAME".bold(),
        "BOX".bold(),
        "POSTURE".bold(),
        "LAST LOG".bold(),
        "COMMAND".bold()
    );
    for (name, entry) in &table {
        println!(
            "{:<16} {:<14} {:<12} {:<20} {}",
            name,
            entry.box_name.as_deref().unwrap_or("(project)"),
            entry.posture.map(|p| p.as_str()).unwrap_or("(box's own)"),
            last_log_time(manager, name).unwrap_or_else(|| "-".to_string()),
            one_line(&entry.command),
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

fn rm(args: RmArgs) -> Result<()> {
    let dir = project_dir()?;
    let path = registry::config_path(&dir);
    if !path.exists() {
        bail!(
            "no devbox.toml in {}; nothing is registered here",
            dir.display()
        );
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let Some(updated) = registry::remove_entry(&text, &args.name)? else {
        bail!(
            "no MCP server named '{}' in {}; `devbox mcp ls` shows what is registered",
            args.name,
            path.display()
        );
    };
    crate::sandbox::state::write_atomically(&path, updated.as_bytes(), "devbox config")
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("{} MCP server '{}'", "Removed".green().bold(), args.name);
    // The log stays. It is the record of what that server did, and `mcp rm` is
    // a change to the registry, not a request to destroy evidence.
    println!("  its log is kept at {}", log_path_display(&args.name));
    Ok(())
}

// ── run ─────────────────────────────────────────────────

/// The shim. Everything it prints goes to stderr: stdout is the agent's
/// JSON-RPC channel and one stray `println!` corrupts the session.
async fn run_server(args: RunArgs, manager: &SandboxManager) -> Result<()> {
    let dir = project_dir()?;
    let table = registry::load(&dir)?;
    let entry = table.get(&args.name).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "no MCP server named '{}' in {}; register it with \
             `devbox mcp add {} -- <command…>`",
            args.name,
            registry::config_path(&dir).display(),
            args.name
        )
    })?;
    shim::validate_command(&entry.command)?;

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
    let claim = crate::web::build::claim_box(&manager.state_dir, &box_name)
        .with_context(|| format!("cannot start box '{box_name}' while it is being rebuilt"))?;
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

    let pgid_file = shim::pgid_file_path(&args.name);
    let guest = shim::wrap_guest_command(&pgid_file, entry.env.iter(), &entry.command);
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

    // Cleanup runs whatever happened above, including a shim that failed to
    // start: `wrap_guest_command` may already have written the file.
    reap(runtime.as_ref(), &box_name, &pgid_file, &outcome).await;
    if switched {
        restore_posture(manager, &box_name).await;
    }

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

    /// `mcp report` is A's, and lands in the integration wave (§7.1). Until it
    /// exists it must not be advertised.
    #[test]
    fn report_is_not_in_the_help_yet() {
        let mcp = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "mcp")
            .expect("mcp is registered")
            .clone();
        let names: Vec<&str> = mcp.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(names, ["add", "run", "ls", "rm"], "the mcp surface changed");
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
