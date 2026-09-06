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
    self, CaptureVerdict, EndedBy, GuestScope, RunKind, RunRecord, RunStatus, StartGate, bootstrap,
    cleanup_argv, readback_argv,
};
use crate::obs::{Query, Store};
use crate::policy::{Policy, Posture};
use crate::report::{self, RunReport, SCOPE_BOX, SCOPE_RUN};
use crate::sandbox::SandboxManager;
use crate::sandbox::checkpoint::{self, Checkpoint, CheckpointId, Target};
use crate::sandbox::config::DevboxConfig;
use crate::sandbox::overlay::OverlayChange;

/// The default working directory inside a box.
pub(crate) const GUEST_CWD: &str = "/workspace";

/// How long the host waits for the wrapper to publish its cgroup.
///
/// Generous, because it is spent inside the guest in one `sh` loop rather than
/// in repeated round trips, and because it runs concurrently with the command
/// itself — a run that beats this deadline has simply had its first second of
/// events attributed by the parent chain instead of by cgroup.
/// Shorter than the guest's own wait (`obs::run::GATE_SECONDS`), and it has to
/// be: this budget is spent *reading* the wrapper's record, and opening the
/// gate afterwards costs another round trip. Equal deadlines meant a readback
/// that only just made it had already lost the wrapper.
const SCOPE_READBACK_MS: u64 = 5_000;

/// How long the gate waits for capture to come back before letting go.
///
/// Longer than a handover takes (about two seconds, measured) and shorter than
/// the guest's own gate deadline, so a wait that succeeds still has time to
/// open the gate.
const CAPTURE_WAIT: Duration = Duration::from_millis(4_000);
const CAPTURE_POLL: Duration = Duration::from_millis(100);

/// How long to let the collector settle before reading the run's events.
///
/// The collector batches with a 250ms linger, so the last events of a command
/// are still in memory when the command's exit reaches us. Reading immediately
/// produced reports that were reliably missing their own final connections.
pub(crate) const SETTLE: Duration = Duration::from_millis(750);

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

    let path = store_path(&manager.state_dir, &name);
    insert_wrapped_run(
        manager,
        &name,
        &run_id,
        RunKind::Run,
        &args.command,
        &args.cwd,
        &args.label.clone().unwrap_or_default(),
        &started_at,
        posture_before,
        posture_during,
    )?;

    let dropped_before = crate::obs::daemon::stats_snapshot(manager)
        .map(|s| s.dropped + s.persist_failed)
        .unwrap_or(0);
    // Which agent process was serving this box when the run began. Compared
    // at the end: a `since` that moved while this stayed put is the collector
    // re-publishing its view, not a new agent, and nothing was lost.
    let agent_pid_before = crate::obs::health::load(&manager.state_dir, &name)
        .ok()
        .flatten()
        .map(|h| h.agent_pid)
        .unwrap_or(0);

    // A checkpoint before the command, so the report's Files section can be
    // about *this run* rather than about everything the box has accumulated.
    // Best effort in both directions: a box in writable mode has no upper to
    // copy, and a copy that fails is a reason to fall back to the box-wide
    // diff — never a reason to refuse to run the command someone asked for.
    let checkpoint_start = take_checkpoint(manager, &state, &name, &run_id, "run-start").await;

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
    let readback = spawn_scope_readback(
        manager,
        manager.runtime_for_sandbox(&state)?,
        &name,
        &run_id,
        &started_at,
    );

    // stdin decides, not a flag.
    //
    // Lima and Incus degrade gracefully when told to be interactive without a
    // tty; Docker does not — `docker exec -it` exits with "the input device is
    // not a TTY" and the command never runs at all. So the check is here, and
    // it is `stdin`, because that is the stream a pty is for.
    let interactive = std::io::stdin().is_terminal();

    // The environment wraps the *command*, not the bootstrap.
    //
    // `env -- K=V …` in front of the whole wrapper would be lost on the
    // `sudo -n systemd-run` path, because sudo resets the environment. Inside
    // the wrapper's argv it is carried verbatim through every hop — the
    // wrapper only ever passes `"$@"` along — so it survives sudo, the
    // transient scope, and the re-exec into stage 2.
    let env = match manager.runtime_for_sandbox(&state) {
        Ok(runtime) => run_env(manager, runtime.as_ref(), &name, &run_id).await,
        Err(_) => Vec::new(),
    };
    let mut argv = bootstrap(&run_id, &args.cwd);
    argv.extend(crate::broker::with_env(&env, &args.command));

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
    // `devbox run` owns its command's whole life, so the only two answers it
    // can give are "it exited" and "we lost it". A run that ends in a way the
    // exit code cannot express is what `mcp run` needs the column for.
    let ended_by = exit_code.map(|_| EndedBy::Exit);

    // The last batch is still in the collector's linger window.
    tokio::time::sleep(SETTLE).await;

    let health = crate::obs::health::load(&manager.state_dir, &name)
        .ok()
        .flatten();
    let sources = health
        .as_ref()
        .map(crate::obs::health::capture_composition)
        .unwrap_or_default();
    // When the current capture stream was established, if that was after this
    // run began. What it *means* is decided further down — a stream that moved
    // is not on its own a stream that lost anything.
    let capture_since = health
        .as_ref()
        .map(|h| h.since.clone())
        .filter(|since| since.as_str() > started_at.as_str())
        .unwrap_or_default();
    let agent_pid_after = health.as_ref().map(|h| h.agent_pid).unwrap_or(0);
    let agent_version = health.map(|h| h.agent_version).unwrap_or_default();
    let dropped = crate::obs::daemon::stats_snapshot(manager)
        .map(|s| (s.dropped + s.persist_failed).saturating_sub(dropped_before))
        .unwrap_or(0);

    let checkpoint_end = take_checkpoint(manager, &state, &name, &run_id, "run-end").await;

    let store = Store::open(&path).context("failed to reopen the box's event store")?;

    // One more back-fill, now that everything has settled.
    //
    // The collector re-reads the live runs once per flush batch, so events
    // that arrived between the gate opening and that re-read were attributed
    // against a list that did not yet carry this run's cgroup. Claiming them
    // here rather than waiting for the collector keeps the whole path free of
    // sleeps that guess at somebody else's timing — and the match is still the
    // exact one the kernel made, so nothing is being widened to fill a hole.
    if let Some(scope) = &scope {
        let cgroup = scope.exclusive_cgroup_id();
        match store.backfill_run(&run_id, cgroup, &started_at) {
            Ok(n) if n > 0 => tracing::debug!(run = %run_id, claimed = n, "back-filled at the end"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not back-fill a run's late events"),
        }
    }

    // Did anything actually go missing, or did the stream merely move?
    // The rule, and why it is that rule, live in `run::capture_verdict`.
    let first_attributed = store.first_attributed_at(&run_id).ok().flatten();
    let verdict = run::capture_verdict(
        &capture_since,
        first_attributed.as_deref(),
        agent_pid_before,
        agent_pid_after,
    );
    if let Err(e) = store.set_run_capture(&run_id, verdict, &capture_since) {
        tracing::warn!(error = %e, "could not record what happened to this run's capture");
    }
    if verdict == CaptureVerdict::Interrupted {
        // Two calls rather than one `\`-continued literal: rustfmt joins a
        // continued string back onto one line and the continuation's
        // indentation survives into the message.
        eprintln!("Warning: capture restarted at {capture_since}, during this run.");
        eprintln!("         Its report is missing whatever the previous agent had not delivered.");
    }

    if checkpoint_start.is_some() || checkpoint_end.is_some() {
        let start = checkpoint_start.as_ref().map(|c| c.id.as_str());
        let end = checkpoint_end.as_ref().map(|c| c.id.as_str());
        if let Err(e) = store.set_run_checkpoints(&run_id, start, end) {
            tracing::warn!(error = %e, "could not record a run's checkpoints");
        }
    }
    store
        .finish_run(
            &run_id,
            &ended_at,
            exit_code,
            status,
            &sources,
            &agent_version,
            dropped,
            ended_by,
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

    // The run is closed, so an agent update this box was owed can happen now.
    // Doing it here rather than leaving it for the next lifecycle command is
    // what keeps a box that is only ever used through `devbox run` from
    // holding an old agent indefinitely.
    apply_deferred_agent_update(manager, &name, &state).await;

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

/// Do the agent update this run was holding up, if there was one.
///
/// Best effort throughout, and quiet when there is nothing to do: this runs at
/// the end of every `devbox run`, and a run that finished must not start
/// reporting errors about a background concern of devbox's own.
async fn apply_deferred_agent_update(
    manager: &SandboxManager,
    name: &str,
    state: &crate::sandbox::state::SandboxState,
) {
    use crate::sandbox::agent_sync;

    if agent_sync::pending(&manager.state_dir, name).is_none() {
        return;
    }
    // Another run may have started between this one ending and now.
    if crate::obs::run_in_flight(&manager.state_dir, name) {
        return;
    }
    let Ok(Some(claim)) = crate::web::build::try_claim_box(&manager.state_dir, name) else {
        return;
    };
    let Ok(runtime) = manager.runtime_for_sandbox(state) else {
        return;
    };
    match agent_sync::ensure_current(
        manager,
        runtime.as_ref(),
        name,
        &state.image,
        agent_sync::Scope::Full,
        &claim,
    )
    .await
    {
        Ok(refresh) if refresh.changed() => {
            eprintln!("Note: box '{name}' had an agent update waiting for this run; it is done.")
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(
            box_id = %name,
            %error,
            "the agent update this run deferred could not be applied"
        ),
    }
}

/// Build, write and print a run's report.
pub(crate) async fn build_report(
    manager: &SandboxManager,
    store: &Store,
    run_id: &str,
    name: &str,
    state: &crate::sandbox::state::SandboxState,
) -> Result<Option<RunReport>> {
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

    // The Files section (§4.4).
    //
    // Two checkpoints bracket the run, so the diff between them is what *this
    // command* changed. Without both — a writable box has no upper to copy, a
    // checkpoint can fail, an older run predates this code — the answer falls
    // back to `overlay::diff`, which is everything the box has accumulated
    // since it was created. That is a superset, not an approximation, so the
    // scope string changes with it and all three renderings print which one
    // the reader is holding.
    //
    // Resolved here rather than inside the closure because it needs the guest:
    // `report` is deliberately synchronous and knows nothing about runtimes.
    let (changes, scope) = file_changes(manager, state, name, &record).await;

    Ok(Some(RunReport::build(
        record,
        &events,
        Box::new(move || Ok(changes.clone())),
        scope,
        attribution,
        unattributed,
    )))
}

/// Build, write and *print* a run's report — the `devbox run` ending.
///
/// Split from [`build_report`] because `devbox mcp run` needs the same report
/// and cannot have the printing: its stdout is the agent's JSON-RPC channel,
/// and one `println!` into it ends the session with a parse error.
pub(crate) async fn render(
    manager: &SandboxManager,
    store: &Store,
    run_id: &str,
    name: &str,
    state: &crate::sandbox::state::SandboxState,
) -> Result<Option<report::Rendered>> {
    let Some(report) = build_report(manager, store, run_id, name, state).await? else {
        return Ok(None);
    };
    print!("{}", report::markdown::render_summary(&report));
    let rendered = report::write(&manager.state_dir, &report)?;
    println!("  report   {}", rendered.html.display());
    Ok(Some(rendered))
}

/// The environment a run's command sees: the broker's, plus its own identity.
///
/// One builder for all three entry points (`run`, `exec`, `shell`), because
/// there is no other place a guest command's environment is assembled — every
/// runtime's `exec_cmd` takes an argv and no environment, and two of the three
/// silently drop `CreateOpts.env`. `broker::with_env` puts it on the command
/// line with `env --`, which is the only form that works uniformly.
///
/// `DEVBOX_RUN_ID` is here rather than only in the guest wrapper because
/// `exec` and `shell` have no wrapper: this is their sole route to it.
pub async fn run_env(
    manager: &SandboxManager,
    runtime: &dyn crate::runtime::Runtime,
    name: &str,
    run_id: &str,
) -> Vec<(String, String)> {
    let mut env = manager.broker_env(runtime, name).await;
    env.push(("DEVBOX_RUN_ID".to_string(), run_id.to_string()));
    env
}

/// Checkpoint the box for a run, or explain why there is none.
///
/// Never fatal. The point of a run is to execute the command; the checkpoint
/// makes the report sharper, and a box that cannot give one still produces a
/// report — with `scope: box` on its Files section, which says so.
pub(crate) async fn take_checkpoint(
    manager: &SandboxManager,
    state: &crate::sandbox::state::SandboxState,
    name: &str,
    run_id: &str,
    label: &str,
) -> Option<Checkpoint> {
    if state.mount_mode == "writable" {
        return None;
    }
    let runtime = match manager.runtime_for_sandbox(state) {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::debug!(error = %e, "no runtime for a run checkpoint");
            return None;
        }
    };
    match checkpoint::create_for_run(runtime.as_ref(), name, run_id, Some(label)).await {
        Ok(checkpoint) => Some(checkpoint),
        Err(e) => {
            eprintln!("Warning: could not take the {label} checkpoint: {e:#}");
            eprintln!("         The report's file section will cover the whole box instead.");
            None
        }
    }
}

/// The run's file changes, and the scope they actually describe.
async fn file_changes(
    manager: &SandboxManager,
    state: &crate::sandbox::state::SandboxState,
    name: &str,
    record: &crate::obs::run::RunRecord,
) -> (Vec<OverlayChange>, &'static str) {
    if state.mount_mode == "writable" {
        return (Vec::new(), SCOPE_BOX);
    }
    let Ok(runtime) = manager.runtime_for_sandbox(state) else {
        return (Vec::new(), SCOPE_BOX);
    };

    if let (Some(start), Some(end)) = (&record.checkpoint_start, &record.checkpoint_end)
        && let (Ok(start), Ok(end)) = (CheckpointId::parse(start), CheckpointId::parse(end))
    {
        match checkpoint::diff(runtime.as_ref(), name, &start, Target::Checkpoint(end)).await {
            Ok(changes) => return (changes, SCOPE_RUN),
            Err(e) => {
                // Falling back rather than failing, but loudly: a Files section
                // that silently widened from the run to the box would be the
                // report's most confident lie.
                eprintln!("Warning: could not diff this run's checkpoints: {e:#}");
                eprintln!("         Falling back to the box-wide overlay diff.");
            }
        }
    }

    let changes = crate::sandbox::overlay::diff(runtime.as_ref(), name)
        .await
        .unwrap_or_default();
    (changes, SCOPE_BOX)
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

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Open the run row a *wrapped* run starts from.
///
/// `devbox run` and `devbox mcp run` are the two commands that give their
/// guest command the wrapper, and therefore the two whose rows carry a scope,
/// a posture pair and a report. One constructor, because a field one of them
/// forgets to set is a field the report renders as empty and nobody notices.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_wrapped_run(
    manager: &SandboxManager,
    name: &str,
    run_id: &str,
    kind: RunKind,
    argv: &[String],
    cwd: &str,
    label: &str,
    started_at: &str,
    posture_before: Posture,
    posture_during: Posture,
) -> Result<()> {
    // What the agent is watching *now*, recorded now. The scope can change
    // between the run and the report — a re-provision, a different transport —
    // and a Files section naming today's scope while describing last week's
    // run would be worse than one naming none.
    let file_scope = crate::obs::health::load(&manager.state_dir, name)
        .ok()
        .flatten()
        .map(|health| health.file_scope.join(", "))
        .unwrap_or_default();
    let record = RunRecord {
        run_id: run_id.to_string(),
        box_id: name.to_string(),
        kind: kind.as_str().to_string(),
        argv: argv.to_vec(),
        cwd: cwd.to_string(),
        label: label.to_string(),
        started_at: started_at.to_string(),
        status: RunStatus::Running.as_str().to_string(),
        posture_before: posture_before.to_string(),
        posture_during: posture_during.to_string(),
        file_scope,
        ..Default::default()
    };
    // The run row goes in before the command starts, so the collector — which
    // re-reads the live runs on every flush — is already attributing by the
    // time the first event arrives.
    Store::open(&store_path(&manager.state_dir, name))
        .context("failed to open the box's event store")?
        .insert_run(&record)
        .context("failed to record the run")
}

/// Learn the guest scope, register it, and let the command go.
///
/// The wrapper publishes the cgroup it entered and then blocks on a pipe. This
/// reads the record, writes it to the run row, and opens the pipe — in that
/// order, which is the whole point: until it happens the host does not know
/// which cgroup to attribute to, and a command that finished in the meantime
/// produced a report whose process tree started partway down the wrapper's own
/// children. `devbox run -- true` hit that several times in four attempts.
///
/// Everything here is best effort in the same direction. A gate that cannot be
/// opened is a slower start and a `start_gate: timeout` in the record; it is
/// never a reason for the command not to run, which is why the guest carries
/// its own deadline as well.
///
/// A task rather than a poll loop of execs — one `sh` waiting in the guest
/// costs one round trip instead of a hundred, and does not fill the box's own
/// event stream with the act of watching it.
pub(crate) fn spawn_scope_readback(
    manager: &SandboxManager,
    runtime: Box<dyn crate::runtime::Runtime>,
    name: &str,
    run_id: &str,
    started_at: &str,
) -> tokio::task::JoinHandle<Option<GuestScope>> {
    let manager_dir = manager.state_dir.clone();
    let name = name.to_string();
    let run_id = run_id.to_string();
    let started = started_at.to_string();
    tokio::spawn(async move {
        let store = Store::open(&store_path(&manager_dir, &name)).ok();
        // Every exit from here writes an outcome, including the ones that
        // learn nothing: a blank `start_gate` has to keep meaning "this run
        // predates the gate" rather than doubling as "we gave up".
        let argv = readback_argv(&run_id, SCOPE_READBACK_MS);
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let read = runtime.exec_cmd(&name, &refs, false).await;
        let scope = read
            .ok()
            .and_then(|result| serde_json::from_str::<GuestScope>(result.stdout.trim()).ok());
        let Some(scope) = scope else {
            if let Some(store) = &store {
                let _ = store.set_run_start_gate(&run_id, StartGate::Timeout);
            }
            return None;
        };
        let cgroup = scope.exclusive_cgroup_id();

        if let Some(store) = &store {
            let _ = store.set_run_scope(&run_id, cgroup, scope.root_pid);
            // Claim whatever the run's cgroup already produced. With the gate
            // working there is usually nothing here — the wrapper's own execs
            // happen before it publishes — but the wrapper that timed out is
            // exactly the one that needs this.
            match store.backfill_run(&run_id, cgroup, &started) {
                Ok(n) if n > 0 => tracing::debug!(run = %run_id, claimed = n, "back-filled"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not back-fill a run's early events"),
            }
        }

        // Before the gate, wait for capture to actually be live.
        //
        // A collector handover ends the agent that delivers events and starts
        // a new one, and for about two seconds in between nothing is captured.
        // A run released into that window comes back empty — not "quiet", but
        // *empty*, with a cgroup id that matches nothing. Measured at two in
        // twenty on a box another build was running commands against.
        //
        // The gate is already holding the command, so this costs latency and
        // nothing else. Bounded: capture that never comes back is a run that
        // still has to happen, and it is flagged rather than delayed forever.
        let deadline = tokio::time::Instant::now() + CAPTURE_WAIT;
        while tokio::time::Instant::now() < deadline {
            let live = crate::obs::health::load(&manager_dir, &name)
                .ok()
                .flatten()
                .is_some_and(|h| h.state == crate::obs::health::CaptureState::Streaming);
            if live {
                break;
            }
            tokio::time::sleep(CAPTURE_POLL).await;
        }

        // Now the gate. The path came from inside the box and becomes a shell
        // word, so it is checked against the two the bootstrap can choose from
        // rather than trusted.
        let gate = if run::is_gate_path(&scope.gate, &run_id) {
            let argv = run::gate_release_argv(&scope.gate);
            let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            match runtime.exec_cmd(&name, &refs, false).await {
                Ok(result) if result.exit_code == 0 => StartGate::Ok,
                _ => StartGate::Timeout,
            }
        } else {
            // A wrapper from before the gate existed, or a path this host did
            // not hand out. Either way the command is running unregistered.
            StartGate::Timeout
        };
        if let Some(store) = &store {
            let _ = store.set_run_start_gate(&run_id, gate);
        }
        Some(scope)
    })
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

    /// This run's id, so a caller can put it in the command's environment.
    pub fn run_id(&self) -> &str {
        &self.run_id
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
                exit_code.map(|_| EndedBy::Exit),
            )
        });
        if let Err(e) = result {
            tracing::debug!(error = %e, "could not close a run");
        }
    }
}
