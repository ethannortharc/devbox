//! `devbox run` — one execution, one report (§4).
//!
//! The difference from `devbox exec` is the whole of component A: `exec` runs
//! a command, `run` gives that command an identity, scopes the guest's capture
//! to it, and writes down what it did. Everything else here follows from
//! needing the answer to be *citable* afterwards.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::obs::collector::store_path;
use crate::obs::run::{
    self, GuestScope, RunKind, RunRecord, RunStatus, bootstrap, cleanup_argv, readback_argv,
};
use crate::obs::{Query, Store};
use crate::policy::{Policy, Posture};
use crate::report::{self, RunReport, SCOPE_BOX};
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;

/// The default working directory inside a box.
const GUEST_CWD: &str = "/workspace";

/// How long the host waits for the wrapper to publish its cgroup.
///
/// Generous, because it is spent inside the guest in one `sh` loop rather than
/// in repeated round trips, and because it runs concurrently with the command
/// itself — a run that beats this deadline has simply had its first second of
/// events attributed by the parent chain instead of by cgroup.
const SCOPE_READBACK_MS: u64 = 5_000;

/// How long to let the collector settle before reading the run's events.
///
/// The collector batches with a 250ms linger, so the last events of a command
/// are still in memory when the command's exit reaches us. Reading immediately
/// produced reports that were reliably missing their own final connections.
const SETTLE: Duration = Duration::from_millis(750);

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Egress posture for the duration of this run only
    #[arg(long)]
    pub posture: Option<String>,

    /// A name for this run, shown in `devbox runs` and the report
    #[arg(long)]
    pub label: Option<String>,

    /// Record the run but do not render a report
    #[arg(long)]
    pub no_report: bool,

    /// Open the HTML report when the run finishes
    #[arg(long)]
    pub open: bool,

    /// Working directory inside the box
    #[arg(long, default_value = GUEST_CWD)]
    pub cwd: String,

    /// Command and arguments to run
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub async fn run(args: RunArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Box '{name}' not found.");
    }

    let run_id = run::new_run_id();
    let started_at = now();
    let state = manager.get_sandbox(&name)?;
    let config = DevboxConfig::load_or_default(&state.project_dir);
    let posture_before = config.policy.egress;
    let requested: Option<Posture> = match &args.posture {
        Some(p) => Some(p.parse()?),
        None => None,
    };
    let posture_during = requested.unwrap_or(posture_before);

    let record = RunRecord {
        run_id: run_id.clone(),
        box_id: name.clone(),
        kind: RunKind::Run.as_str().to_string(),
        argv: args.command.clone(),
        cwd: args.cwd.clone(),
        label: args.label.clone().unwrap_or_default(),
        started_at: started_at.clone(),
        status: RunStatus::Running.as_str().to_string(),
        posture_before: posture_before.to_string(),
        posture_during: posture_during.to_string(),
        ..Default::default()
    };

    let path = store_path(&manager.state_dir, &name);
    // The run row goes in before the command starts, so the collector — which
    // re-reads the live runs on every flush — is already attributing by the
    // time the first event arrives.
    Store::open(&path)
        .context("failed to open the box's event store")?
        .insert_run(&record)
        .context("failed to record the run")?;

    let dropped_before = crate::obs::daemon::stats_snapshot(manager)
        .map(|s| s.dropped + s.persist_failed)
        .unwrap_or(0);

    // Posture, and the guard that puts it back. ADR-0047: reverse only a
    // switch that happened — and if the apply itself failed, we do not know
    // how far it got, so reversing is the recoverable side of the mistake.
    let mut posture_guard = PostureGuard::inactive();
    if let Some(posture) = requested
        && posture != posture_before
    {
        let mut policy = config.policy.clone();
        policy.egress = posture;
        policy.validate()?;
        let runtime = manager.runtime_for_sandbox(&state)?;
        posture_guard = PostureGuard::armed(config.policy.clone());
        crate::policy::enforce::apply(runtime.as_ref(), &name, &policy)
            .await
            .with_context(|| format!("could not apply posture {posture} to box '{name}'"))?;
        println!("posture {posture_before} → {posture} for this run");
    }

    // Learn the guest scope while the command runs.
    //
    // Not before: the cgroup only exists once the wrapper has entered it, and
    // the wrapper is the command. A task rather than a poll loop of execs —
    // one `sh` waiting in the guest costs one round trip instead of a hundred,
    // and does not fill the box's own event stream with the act of watching it.
    let readback = {
        let manager_dir = manager.state_dir.clone();
        let name = name.clone();
        let run_id = run_id.clone();
        let started = started_at.clone();
        let state = state.clone();
        let runtime = manager.runtime_for_sandbox(&state)?;
        tokio::spawn(async move {
            let argv = readback_argv(&run_id, SCOPE_READBACK_MS);
            let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            let result = runtime.exec_cmd(&name, &refs, false).await.ok()?;
            let scope: GuestScope = serde_json::from_str(result.stdout.trim()).ok()?;
            let cgroup = scope.exclusive_cgroup_id();
            let store = Store::open(&store_path(&manager_dir, &name)).ok()?;
            store.set_run_scope(&run_id, cgroup, scope.root_pid).ok()?;
            // And claim what the run's cgroup already produced. The collector
            // could not have attributed those: they were written before this
            // line, which is the first moment the host knew which cgroup to
            // look for. Everything after it the collector handles itself.
            match store.backfill_run(&run_id, cgroup, &started) {
                Ok(n) if n > 0 => tracing::debug!(run = %run_id, claimed = n, "back-filled"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not back-fill a run's early events"),
            }
            Some(scope)
        })
    };

    // stdin decides, not a flag.
    //
    // Lima and Incus degrade gracefully when told to be interactive without a
    // tty; Docker does not — `docker exec -it` exits with "the input device is
    // not a TTY" and the command never runs at all. So the check is here, and
    // it is `stdin`, because that is the stream a pty is for.
    let interactive = std::io::stdin().is_terminal();
    let mut argv = bootstrap(&run_id, &args.cwd);
    argv.extend(args.command.iter().cloned());

    let outcome = manager.exec_in_sandbox(&name, &argv, interactive).await;
    let ended_at = now();

    let scope = readback.await.ok().flatten();
    let restore = posture_guard.take();

    // Put the posture back before anything that can fail, so a report that
    // cannot be rendered does not leave the box on a posture nobody asked for.
    if let Some(previous) = restore {
        let runtime = manager.runtime_for_sandbox(&state)?;
        if let Err(e) = crate::policy::enforce::apply(runtime.as_ref(), &name, &previous).await {
            eprintln!(
                "Warning: box '{name}' is still on posture {} — restoring {} failed: {e:#}",
                posture_during, previous.egress
            );
        }
    }

    let exit_code = match &outcome {
        Ok(code) => Some(*code),
        Err(_) => None,
    };
    let status = if exit_code.is_some() {
        RunStatus::Finished
    } else {
        RunStatus::Aborted
    };

    // The last batch is still in the collector's linger window.
    tokio::time::sleep(SETTLE).await;

    let health = crate::obs::health::load(&manager.state_dir, &name)
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

    let store = Store::open(&path).context("failed to reopen the box's event store")?;
    store
        .finish_run(
            &run_id,
            &ended_at,
            exit_code,
            status,
            &sources,
            &agent_version,
            dropped,
        )
        .context("failed to close the run")?;

    // Best-effort; a box that has already gone away leaves two small files in
    // a tmpfs that its next boot clears.
    if let Ok(runtime) = manager.runtime_for_sandbox(&state) {
        let argv = cleanup_argv(&run_id);
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let _ = runtime.exec_cmd(&name, &refs, false).await;
    }

    if let Some(scope) = &scope
        && !scope.scope_enum().is_exclusive()
    {
        eprintln!(
            "Note: box '{name}' could not give this run its own cgroup, so its \
             events were attributed by process ancestry only."
        );
    }

    if !args.no_report {
        let rendered = render(manager, &store, &run_id, &name, &state).await?;
        if args.open
            && let Some(rendered) = &rendered
        {
            open_report(&rendered.html);
        }
    }

    match outcome {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(e) => Err(e),
    }
}

/// Build, write and print a run's report.
async fn render(
    manager: &SandboxManager,
    store: &Store,
    run_id: &str,
    name: &str,
    state: &crate::sandbox::state::SandboxState,
) -> Result<Option<report::Rendered>> {
    let Some(record) = store.get_run(run_id)? else {
        return Ok(None);
    };
    let events = store.query(&Query {
        run_id: Some(run_id.to_string()),
        limit: Some(Query::MAX_LIMIT),
        ..Default::default()
    })?;
    let attribution = store.attribution_counts(run_id)?;
    let unattributed =
        store.unattributed_in_window(&record.started_at, record.ended_at.as_deref())?;

    // The file section, wave 1 (§4.4, and the brief's own caveat).
    //
    // `overlay::diff` answers "what has changed in this box since it was
    // created", which is a superset of this run. Component E's checkpoint diff
    // is the run-scoped answer, and this closure is the seam it lands in: at
    // integration its body becomes `checkpoint::diff(start, end)` and the
    // scope string becomes `SCOPE_RUN`. Awaited here rather than inside,
    // because the source is a synchronous closure by design — the report
    // module has no business being async.
    let changes = if state.mount_mode == "writable" {
        Vec::new()
    } else {
        let runtime = manager.runtime_for_sandbox(state)?;
        crate::sandbox::overlay::diff(runtime.as_ref(), name)
            .await
            .unwrap_or_default()
    };

    let report = RunReport::build(
        record,
        &events,
        Box::new(move || Ok(changes.clone())),
        SCOPE_BOX,
        attribution,
        unattributed,
    );

    print!("{}", report::markdown::render_summary(&report));
    let rendered = report::write(&manager.state_dir, &report)?;
    println!("  report   {}", rendered.html.display());
    Ok(Some(rendered))
}

fn open_report(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(path).spawn() {
        eprintln!("Could not open {}: {e}", path.display());
    }
}

/// Restores the box's posture if — and only if — this run switched it.
///
/// ADR-0047's rule, applied to postures rather than generations: an apply that
/// was never attempted leaves nothing to undo, and an apply that failed
/// part-way is undone anyway, because an unnecessary restore is recoverable
/// and a skipped one silently leaves the box open.
struct PostureGuard(Option<Policy>);

impl PostureGuard {
    fn inactive() -> Self {
        Self(None)
    }

    fn armed(previous: Policy) -> Self {
        Self(Some(previous))
    }

    fn take(&mut self) -> Option<Policy> {
        self.0.take()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Record a run for a command that is not `devbox run` (§4.6).
///
/// `exec` and `shell` create runs so the console's Runs tab is the whole story
/// of a box rather than the part that happened to go through one command. They
/// get no wrapper: `exec`'s output is captured and `shell`'s attach path does
/// not take an interactive flag, so neither can be re-pointed at a cgroup
/// without changing the runtime contract. The consequence is stated rather
/// than hidden — these runs have no `cgroup_id` and no `root_pid`, so only the
/// window rule can attribute to them, and they render no report.
pub struct SimpleRun {
    run_id: String,
    path: std::path::PathBuf,
}

impl SimpleRun {
    /// Open a run row, or `None` if the store cannot be written — recording a
    /// run is never a reason for the command itself to fail.
    pub fn start(
        manager: &SandboxManager,
        name: &str,
        kind: RunKind,
        argv: &[String],
    ) -> Option<Self> {
        let path = store_path(&manager.state_dir, name);
        let record = RunRecord {
            run_id: run::new_run_id(),
            box_id: name.to_string(),
            kind: kind.as_str().to_string(),
            argv: argv.to_vec(),
            started_at: now(),
            status: RunStatus::Running.as_str().to_string(),
            ..Default::default()
        };
        match Store::open(&path).and_then(|s| s.insert_run(&record)) {
            Ok(()) => Some(Self {
                run_id: record.run_id,
                path,
            }),
            Err(e) => {
                tracing::debug!(error = %e, "could not record a {kind} run");
                None
            }
        }
    }

    /// Close it out. A row left `running` is what `aborted` is for, so this is
    /// called on the error path too.
    pub fn finish(self, manager: &SandboxManager, name: &str, exit_code: Option<i32>) {
        let health = crate::obs::health::load(&manager.state_dir, name)
            .ok()
            .flatten();
        let sources = health
            .as_ref()
            .map(crate::obs::health::capture_composition)
            .unwrap_or_default();
        let agent_version = health.map(|h| h.agent_version).unwrap_or_default();
        let status = if exit_code.is_some() {
            RunStatus::Finished
        } else {
            RunStatus::Aborted
        };
        let result = Store::open(&self.path).and_then(|s| {
            s.finish_run(
                &self.run_id,
                &now(),
                exit_code,
                status,
                &sources,
                &agent_version,
                0,
            )
        });
        if let Err(e) = result {
            tracing::debug!(error = %e, "could not close a run");
        }
    }
}
