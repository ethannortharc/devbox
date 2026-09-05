//! Collector lifecycle for the web console.
//!
//! `Collector` is deliberately one box and one socket. This supervisor is the
//! missing control-plane half: it reconciles registered boxes into running
//! listeners, adds boxes created while the console is open, and stops a
//! listener when its box is removed.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::collector::{Collector, Hello, Stats, socket_path, store_path};
use super::health::{self, CaptureHealth, CaptureState};
use super::store::Store;
use crate::sandbox::SandboxManager;

/// How quickly a CLI-created or destroyed box is reflected in collector
/// listeners while the console is already open.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Ceiling on the wait before looking again at a box that was not running.
///
/// A stopped box is an expected state, not a broken collector, so it does not
/// share the failure backoff: at thirty seconds, starting a box that had been
/// down a while left its first half-minute of activity uncaptured — the window
/// in which a box does the most interesting things.
///
/// It is a ceiling and not a fixed interval because the probe is not free:
/// every one shells out to a runtime CLI, and this supervisor runs in a
/// background daemon whether or not a console is open. Eight seconds bounds
/// what a starting box can lose while keeping a permanently stopped box down
/// to a fraction of a percent of a core.
const STOPPED_RETRY_MAX: Duration = Duration::from_secs(8);

/// Longest agent stderr line the collector will hold.
///
/// Generous for a diagnostic and bounded against an agent that never writes a
/// newline. Nothing downstream shows more than one line of it anyway.
const MAX_STDERR_LINE: usize = 4096;

/// Lines from one agent's stderr the collector will write to its log.
///
/// Enough for any real diagnostic, and finite — an append-only log fed by a
/// guest is a way to fill someone's disk.
const MAX_STDERR_LINES: usize = 1_000;

/// How long to wait on one runtime status probe before giving up on it.
///
/// Generous next to a healthy `docker inspect` or `limactl list`, and short
/// next to "forever", which is what a stale runtime socket otherwise costs
/// every box this supervisor is responsible for.
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a retiring collector is given to drain before it is aborted.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconcile registered boxes until the console shuts down.
pub async fn run<F>(manager: Arc<SandboxManager>, stats: Arc<Stats>, shutdown: F)
where
    F: Future<Output = ()> + Send,
{
    let mut supervisor = Supervisor::new(manager, stats);
    tokio::pin!(shutdown);

    loop {
        // Runtime CLIs can hang on stale Docker Desktop/Lima sockets. Shutdown
        // must still win immediately: an upgraded foreground binary is waiting
        // for this process to release the daemon lock before it can replace us.
        let reconcile = supervisor.reconcile();
        tokio::pin!(reconcile);
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            result = &mut reconcile => {
                if let Err(error) = result {
                    // A damaged registry must not kill listeners already serving
                    // other boxes. Keep them and retry; the error is visible.
                    tracing::warn!(%error, "could not reconcile observability collectors");
                }
            }
        }

        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(RECONCILE_INTERVAL) => {}
        }
    }

    supervisor.stop_all().await;
}

struct Supervisor {
    manager: Arc<SandboxManager>,
    stats: Arc<Stats>,
    active: HashMap<String, ActiveCollector>,
    retries: HashMap<String, RetryState>,
}

struct ActiveCollector {
    task: tokio::task::JoinHandle<()>,
    /// Cleared when this collector is retired.
    ///
    /// Socket connections are served on detached tasks, so a connection that
    /// belonged to a destroyed box can outlive the collector that accepted it
    /// and report its own departure afterwards — overwriting the *replacement*
    /// collector's health with "the agent disconnected".
    ///
    /// A mutex and not an atomic: the hook holds it across the publish, so
    /// retirement cannot land between a check that passed and the write that
    /// check authorised.
    live: Arc<std::sync::Mutex<bool>>,
    /// Set once an agent completes a handshake on this collector.
    ///
    /// Both transports need a deadline for "nobody came", and only this
    /// distinguishes a collector still waiting from one that is serving. The
    /// socket transport has no readiness channel — it starts no agent, it
    /// waits for one — so its accept hook sets this instead.
    agent_seen: Arc<std::sync::atomic::AtomicBool>,
    /// Asks the accept loop to finish, so its writer drains before exit.
    ///
    /// `None` for the exec transport, whose task ends when its child does, and
    /// for the test listener.
    stop: Option<tokio::sync::watch::Sender<bool>>,
    /// Present until an exec-carried agent completes an accepted handshake.
    /// Native Unix listeners are ready as soon as bind succeeds and use None.
    ///
    ready: Option<oneshot::Receiver<()>>,
    started_at: Instant,
    state_identity: StateIdentity,
    _claim: CollectorClaim,
}

/// Cross-process ownership of one box's collector/agent pair.
///
/// Multiple consoles are supported, but duplicating the producer is not: two
/// exec agents observe the same activity twice and race writes to one SQLite
/// database. The file stays open for exactly as long as ActiveCollector.
struct CollectorClaim {
    _file: std::fs::File,
}

impl Drop for CollectorClaim {
    fn drop(&mut self) {
        // Be explicit rather than relying only on close-on-drop. The
        // supervisor can retire and reacquire the same name in one async
        // reconciliation tick, and some Unix lock implementations otherwise
        // keep reporting the just-closed claim as busy for that handoff.
        let _ = std::fs::File::unlock(&self._file);
    }
}

/// Filesystem identity of `sandboxes/<name>/state.json`.
///
/// Names are reusable and runtime timestamps have one-second resolution. The
/// inode changes when destroy removes a state directory and create writes a
/// new one, even if both happen between two supervisor ticks. Tracking it
/// prevents the new box from inheriting the old box's still-running collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StateIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanos: i64,
}

/// What `start` has already settled before a transport takes over.
///
/// Grouped rather than passed one by one: the two transports need the same
/// four things, and threading them individually put `start_remote` past the
/// point where a reader can tell which argument is which.
struct Launch {
    state_identity: StateIdentity,
    claim: CollectorClaim,
    /// Consecutive failed attempts, for the health record.
    attempts: u32,
    /// Cleared when this collector is retired.
    live: Arc<std::sync::Mutex<bool>>,
}

struct RetryState {
    /// Attempts that actually failed. Drives the failure backoff and the
    /// attempt count the console shows.
    failures: u32,
    /// Consecutive expected non-starts — a stopped box, a collector another
    /// console owns. Counted separately, or waiting out a weekend of a box
    /// being switched off would put its first genuine failure straight onto
    /// the thirty-second ceiling and report it as the ninetieth attempt.
    rechecks: u32,
    retry_at: Instant,
}

impl Supervisor {
    fn new(manager: Arc<SandboxManager>, stats: Arc<Stats>) -> Self {
        Self {
            manager,
            stats,
            active: HashMap::new(),
            retries: HashMap::new(),
        }
    }

    async fn reconcile(&mut self) -> Result<()> {
        let states = self
            .manager
            .list_sandboxes()
            .context("list boxes for collector reconciliation")?;
        let mut wanted = BTreeMap::new();
        for state in states {
            wanted.insert(
                state.name.clone(),
                state_identity(&self.manager.state_dir, &state.name)?,
            );
        }

        // Retire a removed or same-name-recreated box before classifying
        // finished tasks as failures. Unlinking a live Unix socket can make
        // its accept loop finish; if that race wins and we schedule ordinary
        // backoff first, the replacement box is left unobserved for a second.
        // An identity change is an intentional lifecycle transition, not a
        // failed collector start, and should be replaced in this same tick.
        let retired: Vec<String> = self
            .active
            .iter()
            .filter(|(name, active)| wanted.get(*name) != Some(&active.state_identity))
            .map(|(name, _)| name.clone())
            .collect();
        for name in retired {
            if let Some(active) = self.active.remove(&name) {
                // Release ownership explicitly before attempting the
                // replacement below. With a partially moved struct the
                // remaining file field's drop can otherwise be delayed until
                // the loop scope ends, and the same process then sees its own
                // advisory lock as busy.
                let ActiveCollector {
                    task,
                    live,
                    stop,
                    _claim: claim,
                    ..
                } = active;
                // Before the stop, not after: a connection task can publish
                // between the two.
                // Taking the lock also waits out a hook that is mid-publish
                // rather than racing it.
                if let Ok(mut live) = live.lock() {
                    *live = false;
                }
                // Retire first, release second. Dropping the claim before
                // the old task had drained let a second supervisor take the
                // box, start its own agent — duplicating events — and then
                // have its fresh health record deleted by the `health::clear`
                // below.
                Self::retire(task, stop).await;
                drop(claim);
            }
            self.retries.remove(&name);
            // The box this record described is gone or has been recreated.
            // Leaving it would let a fresh box inherit the old one's failure.
            health::clear(&self.manager.state_dir, &name);
        }

        // An exec process is not healthy merely because it spawned. Reset its
        // backoff only at the collector's accepted-handshake boundary, and
        // bound a child that never says hello (for example a stuck sudo).
        let mut ended = Vec::new();
        for (name, active) in &mut self.active {
            let ready = active
                .ready
                .as_mut()
                .map(tokio::sync::oneshot::Receiver::try_recv);
            match ready {
                Some(Ok(())) => {
                    active.ready = None;
                    active
                        .agent_seen
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.retries.remove(name);
                }
                Some(Err(tokio::sync::oneshot::error::TryRecvError::Closed)) => {
                    active.ready = None;
                }
                Some(Err(tokio::sync::oneshot::error::TryRecvError::Empty)) | None => {}
            }
            // The exec transport waits on its readiness channel; the socket
            // transport has none, because it does not start an agent — it
            // waits for one to dial in. Both need a deadline, or a box whose
            // guest service was never installed sits on "Attaching the
            // agent…" for as long as the console runs.
            let handshake_expired = !active.agent_seen.load(std::sync::atomic::Ordering::SeqCst)
                && active.started_at.elapsed() >= HANDSHAKE_TIMEOUT;
            if active.task.is_finished() || handshake_expired {
                ended.push((name.clone(), handshake_expired));
            }
        }
        for (name, handshake_expired) in ended {
            if let Some(active) = self.active.remove(&name) {
                if handshake_expired {
                    active.task.abort();
                    tracing::warn!(box_id = %name, "observability agent handshake timed out");
                    // The task is being stopped, so it will not publish its own
                    // diagnosis. This one is the supervisor's to record.
                    let attempts = self.retries.get(&name).map_or(0, |r| r.failures) + 1;
                    self.publish_health(
                        CaptureHealth::new(&name, CaptureState::Failed)
                            .with_detail(format!(
                                "no agent completed a handshake within {}s — the box may \
                                 have been provisioned before devbox-obsd existed",
                                HANDSHAKE_TIMEOUT.as_secs()
                            ))
                            .with_attempts(attempts),
                    );
                }
                let _ = active.task.await;
            }
            self.schedule_retry(&name);
        }

        self.retries.retain(|name, _| wanted.contains_key(name));

        // Started concurrently, because starting one means probing a runtime
        // CLI and those wedge. Serially, ten boxes on a stale Docker socket
        // spent ten timeouts — two and a half minutes — before an eleventh,
        // healthy, newly created box got a collector at all, and its first
        // seconds are the ones worth capturing. `start` takes `&self`, so the
        // only ordering this gives up is between unrelated boxes.
        let candidates: Vec<_> = wanted
            .into_iter()
            .filter(|(name, _)| !self.active.contains_key(name))
            .filter(|(name, _)| {
                self.retries
                    .get(name)
                    .is_none_or(|retry| retry.retry_at <= Instant::now())
            })
            .collect();
        // A shared reference, so the futures borrow rather than move `&mut self`.
        let supervisor: &Supervisor = self;
        let started =
            futures::future::join_all(candidates.into_iter().map(|(name, identity)| async move {
                let outcome = supervisor.start(&name, identity).await;
                (name, outcome)
            }))
            .await;

        for (name, outcome) in started {
            match outcome {
                Ok(Some(active)) => {
                    if active.ready.is_none() {
                        self.retries.remove(&name);
                    }
                    self.active.insert(name, active);
                }
                Ok(None) => self.schedule_recheck(&name),
                Err(error) => {
                    // One broken box must not stop capture for every other
                    // box. Its next runtime probe is exponentially delayed.
                    tracing::warn!(box_id = %name, %error, "could not start observability collector");
                    // And it must not be left looking like it is still trying.
                    // `Starting` is published partway through `start`, so an
                    // error after that point — a missing runtime executable,
                    // an unreadable store — left the console saying
                    // "Attaching the agent…" indefinitely with the reason
                    // visible only in the daemon's log.
                    let attempts = self.retries.get(&name).map_or(0, |r| r.failures) + 1;
                    self.publish_health(
                        CaptureHealth::new(&name, CaptureState::Failed)
                            .with_detail(format!("{error:#}"))
                            .with_attempts(attempts),
                    );
                    self.schedule_retry(&name);
                }
            }
        }
        Ok(())
    }

    fn schedule_retry(&mut self, name: &str) {
        let failures = self.retries.get(name).map_or(0, |state| state.failures);
        let delay = retry_delay(failures);
        self.retries.insert(
            name.to_string(),
            RetryState {
                failures: failures.saturating_add(1),
                rechecks: 0,
                retry_at: Instant::now() + delay,
            },
        );
    }

    /// Look again soon, without counting this as a failure.
    ///
    /// For states that are expected and self-resolving: a box that is not
    /// running, and a collector another console already owns. Backs off like a
    /// failure but to a much lower ceiling, so a box that has just gone down is
    /// noticed returning almost at once and one that has been down for hours
    /// still costs one probe every eight seconds.
    fn schedule_recheck(&mut self, name: &str) {
        let previous = self.retries.get(name);
        let failures = previous.map_or(0, |state| state.failures);
        let rechecks = previous.map_or(0, |state| state.rechecks);
        let delay = retry_delay(rechecks).min(STOPPED_RETRY_MAX);
        self.retries.insert(
            name.to_string(),
            RetryState {
                failures,
                rechecks: rechecks.saturating_add(1),
                retry_at: Instant::now() + delay,
            },
        );
    }

    async fn start(
        &self,
        name: &str,
        state_identity: StateIdentity,
    ) -> Result<Option<ActiveCollector>> {
        let state = self.manager.get_sandbox(name)?;
        let Some(claim) = try_claim_collector(&self.manager.state_dir, name)? else {
            return Ok(None);
        };
        #[cfg(test)]
        if state.runtime == "test" {
            return self
                .start_test_listener(name, state_identity, claim)
                .map(Some);
        }
        let runtime = self.manager.runtime_for_sandbox(&state)?;
        // Bounded. This probe shells out to a runtime CLI, and those wedge on
        // a stale Docker or Lima socket — reconciliation is one loop for every
        // box, so one hung probe suspended capture for all of them and for
        // every box created afterwards.
        //
        // A timeout is not an answer, and neither is an error. Only a probe
        // that *succeeded and said stopped* may publish `BoxStopped`: folding
        // the other two into it reported a missing `docker` binary as a box
        // the user had switched off, and hid the real fault in the log.
        let status = match tokio::time::timeout(STATUS_PROBE_TIMEOUT, runtime.status(name)).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => return Err(error).context("probe the box's runtime status"),
            Err(_) => {
                tracing::warn!(
                    box_id = %name,
                    seconds = STATUS_PROBE_TIMEOUT.as_secs(),
                    "runtime status probe timed out"
                );
                return Ok(None);
            }
        };
        match status {
            crate::runtime::SandboxStatus::Running => {}
            // Not a failure and not nothing: the single most common reason an
            // Activity tab is empty, and the console can only say so if the
            // collector writes it down.
            crate::runtime::SandboxStatus::Stopped | crate::runtime::SandboxStatus::NotFound => {
                self.publish_health(CaptureHealth::new(name, CaptureState::BoxStopped));
                return Ok(None);
            }
            // `Unreachable` and `Unknown` are not "switched off". Publishing
            // them as stopped told the user they had shut the box down and
            // hid a guest control channel that had actually failed.
            other => {
                self.publish_health(
                    CaptureHealth::new(name, CaptureState::Starting).with_detail(format!(
                        "the runtime reports this box as {}",
                        crate::web::service::status_label(&other)
                    )),
                );
                return Ok(None);
            }
        }

        let database = store_path(&self.manager.state_dir, name);
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create collector directory {}", parent.display()))?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("protect collector directory {}", parent.display()))?;
        }
        let store = Store::open(&database)?;
        let transport = if crate::obs::uses_host_socket(runtime.name()) {
            "socket"
        } else {
            "exec"
        };
        let state_dir = self.manager.state_dir.clone();
        let health_box = name.to_string();
        // Connections overlap: an agent restarting inside the box dials in
        // before the old stream has finished draining. Each one's hello is
        // kept under its own number, so a departure removes that connection's
        // claim and the record is rebuilt from whoever is still here.
        //
        // A count alone was not enough. It stopped a departure from erasing an
        // arrival, but when the *newer* connection died first the box went on
        // advertising its capture backends on behalf of the older one.
        let agents: Arc<std::sync::Mutex<BTreeMap<u64, Hello>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let live = Arc::new(std::sync::Mutex::new(true));
        let hook_live = live.clone();
        let agent_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_seen = agent_seen.clone();
        let collector = Arc::new(
            Collector::new(socket_path(&self.manager.state_dir, name), store)
                .for_box(name)
                .with_stats(self.stats.clone())
                .with_agent_hook(move |connection, hello| {
                    // Held for the whole hook, publish included. Checking and
                    // then releasing left a window in which retirement landed
                    // between the two, and a straggler from a destroyed box
                    // wrote over its replacement's record.
                    let Ok(live) = hook_live.lock() else { return };
                    if !*live {
                        // This collector has been retired. Whatever this
                        // connection has to say is about a box that is gone.
                        return;
                    }
                    let Ok(mut agents) = agents.lock() else {
                        return;
                    };
                    match hello {
                        Some(hello) => {
                            hook_seen.store(true, std::sync::atomic::Ordering::SeqCst);
                            agents.insert(connection, hello.clone());
                        }
                        None => {
                            agents.remove(&connection);
                        }
                    }
                    // Newest surviving connection wins: it is the one whose
                    // events are arriving now.
                    let record = match agents.values().next_back() {
                        Some(hello) => CaptureHealth {
                            ebpf: hello.ebpf,
                            capture: hello.capture.clone(),
                            source: hello.source.clone(),
                            agent_version: hello.version.clone(),
                            ..CaptureHealth::new(&health_box, CaptureState::Streaming)
                                .with_transport(transport)
                        },
                        // Nobody left. For an exec child the task below records
                        // why; for a host-side socket listener, which outlives
                        // the container that was dialling it, this is the only
                        // notice there is.
                        None => CaptureHealth::new(&health_box, CaptureState::Starting)
                            .with_transport(transport)
                            .with_detail("the agent disconnected"),
                    };
                    if let Err(error) = health::publish(&state_dir, &record) {
                        tracing::warn!(box_id = %record.box_id, %error, "publish capture health");
                    }
                }),
        );
        let attempts = self.retries.get(name).map_or(0, |retry| retry.failures) + 1;
        // Carry the last failure's text into the attempt that is retrying it.
        // Without this a flapping agent blanks its own diagnosis for the few
        // seconds each attempt survives, so the reason is visible only to
        // whoever happens to be looking during the failed half of the cycle.
        let previous = health::load(&self.manager.state_dir, name)
            .ok()
            .flatten()
            .filter(|record| record.state == CaptureState::Failed)
            .map(|record| record.detail)
            .unwrap_or_default();
        let retirement = live.clone();
        self.publish_health(
            CaptureHealth::new(name, CaptureState::Starting)
                .with_transport(transport)
                .with_detail(previous)
                .with_attempts(attempts),
        );
        if !crate::obs::uses_host_socket(runtime.name()) {
            return self
                .start_remote(
                    name,
                    runtime.as_ref(),
                    collector,
                    Launch {
                        state_identity,
                        claim,
                        attempts,
                        live: retirement,
                    },
                )
                .map(Some);
        }

        let listener = collector.bind()?;
        let box_id = name.to_string();
        let (stop, stopping) = tokio::sync::watch::channel(false);
        Ok(Some(ActiveCollector {
            task: tokio::spawn(async move {
                if let Err(error) = collector.run_until(listener, stopping).await {
                    tracing::error!(box_id = %box_id, %error, "observability collector stopped");
                }
            }),
            ready: None,
            agent_seen,
            live,
            stop: Some(stop),
            started_at: Instant::now(),
            state_identity,
            _claim: claim,
        }))
    }

    #[cfg(test)]
    fn start_test_listener(
        &self,
        name: &str,
        state_identity: StateIdentity,
        claim: CollectorClaim,
    ) -> Result<ActiveCollector> {
        let database = store_path(&self.manager.state_dir, name);
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let collector = Arc::new(
            Collector::new(
                socket_path(&self.manager.state_dir, name),
                Store::open(&database)?,
            )
            .for_box(name)
            .with_stats(self.stats.clone()),
        );
        let listener = collector.bind()?;
        Ok(ActiveCollector {
            task: tokio::spawn(async move {
                let _ = collector.run(listener).await;
            }),
            ready: None,
            // The test listener has no agent and no deadline to miss.
            agent_seen: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            live: Arc::new(std::sync::Mutex::new(true)),
            stop: None,
            started_at: Instant::now(),
            state_identity,
            _claim: claim,
        })
    }

    /// Record what the collector currently knows about a box, for the console.
    ///
    /// Never fatal: capture health is a diagnostic, and failing to write one
    /// must not take down capture for the box it describes.
    fn publish_health(&self, health: CaptureHealth) {
        if let Err(error) = health::publish(&self.manager.state_dir, &health) {
            tracing::warn!(box_id = %health.box_id, %error, "publish capture health");
        }
    }

    fn start_remote(
        &self,
        name: &str,
        runtime: &dyn crate::runtime::Runtime,
        collector: Arc<Collector>,
        launch: Launch,
    ) -> Result<ActiveCollector> {
        let Launch {
            state_identity,
            claim,
            attempts,
            live,
        } = launch;
        let agent_args = remote_agent_args(name, crate::obs::uses_ebpf(runtime.name()));
        let agent_refs: Vec<&str> = agent_args.iter().map(String::as_str).collect();
        let argv = runtime.argv(name, &agent_refs, false);
        let (program, args) = argv
            .split_first()
            .context("runtime returned an empty agent command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("start observability agent in box '{name}'"))?;
        let stdin = child.stdin.take().context("agent exec has no stdin")?;
        let stdout = child.stdout.take().context("agent exec has no stdout")?;
        let stderr = child.stderr.take().context("agent exec has no stderr")?;
        let box_id = name.to_string();
        let (ready_tx, ready_rx) = oneshot::channel();
        // Asked to stop, this task kills the agent's process rather than being
        // aborted. Killing it ends the read, which closes the queue, which
        // lets the writer flush its last batch — an abort skipped all three
        // and left those events in memory.
        let (stop, mut stopping) = tokio::sync::watch::channel(false);
        let state_dir = self.manager.state_dir.clone();
        // The agent's own last words, kept for the failure record.
        //
        // "agent closed the connection before saying hello" is what the
        // collector sees; "devbox-obsd: not found" is what happened. Only the
        // second one tells anyone to reprovision the box, and it exists for
        // exactly as long as nobody writes it down.
        let last_stderr = Arc::new(std::sync::Mutex::new(String::new()));

        Ok(ActiveCollector {
            task: tokio::spawn({
                let last_stderr = last_stderr.clone();
                async move {
                    let log_box = box_id.clone();
                    let stderr_sink = last_stderr.clone();
                    let log_task = tokio::spawn(async move {
                        // Read bytes, not lines. `BufReader::lines` grows its
                        // buffer until it finds a newline, so an agent that
                        // completes its handshake and then writes to stderr
                        // forever without one grows the collector daemon's
                        // memory without bound — while its event stream looks
                        // perfectly healthy.
                        //
                        // The pipe must still be drained, or the agent blocks
                        // on its own stderr. So an overlong line is truncated
                        // at a bound and reading continues.
                        // Read in chunks and split them here. A byte at a
                        // time meant one loop iteration per byte, which let a
                        // guest that writes stderr continuously occupy a
                        // collector worker for as long as it liked — even
                        // after logging had been switched off.
                        let mut reader = BufReader::new(stderr);
                        let mut line = Vec::new();
                        let mut chunk = [0u8; 8192];
                        // Lines logged before the agent is told to be quiet.
                        //
                        // Bounding each line stopped one from growing without
                        // end; it did not stop a guest emitting short ones
                        // forever, which costs a core and fills the host's
                        // disk through an append-only log. The pipe is still
                        // drained past this point — the failure record wants
                        // the *last* line — but nothing more is written down.
                        let mut logged = 0usize;
                        'drain: loop {
                            let read = match reader.read(&mut chunk).await {
                                Ok(0) | Err(_) => break 'drain,
                                Ok(n) => n,
                            };
                            for &byte in &chunk[..read] {
                                if byte != b'\n' {
                                    if line.len() < MAX_STDERR_LINE {
                                        line.push(byte);
                                    }
                                    continue;
                                }
                                let text = String::from_utf8_lossy(&line).trim().to_string();
                                line.clear();
                                if text.is_empty() {
                                    continue;
                                }
                                if let Ok(mut slot) = stderr_sink.lock() {
                                    slot.clone_from(&text);
                                }
                                logged += 1;
                                match logged.cmp(&MAX_STDERR_LINES) {
                                    std::cmp::Ordering::Less => {
                                        tracing::info!(box_id = %log_box, message = %text, "observability agent");
                                    }
                                    std::cmp::Ordering::Equal => {
                                        tracing::warn!(
                                            box_id = %log_box,
                                            lines = MAX_STDERR_LINES,
                                            "observability agent is too talkative; \
                                             no longer logging its output"
                                        );
                                    }
                                    std::cmp::Ordering::Greater => {}
                                }
                            }
                        }
                        // Whatever was buffered when the pipe closed. An agent
                        // that prints `devbox-obsd: not found` and exits writes
                        // no trailing newline, and that is precisely the line
                        // the failure record exists to carry.
                        let text = String::from_utf8_lossy(&line).trim().to_string();
                        if !text.is_empty() {
                            if let Ok(mut slot) = stderr_sink.lock() {
                                slot.clone_from(&text);
                            }
                            tracing::info!(box_id = %log_box, message = %text, "observability agent");
                        }
                    });

                    let stream = collector.run_agent_stream_ready(stdout, stdin, ready_tx);
                    tokio::pin!(stream);
                    // Raced against the stop signal exactly once. `wait_for`
                    // returns immediately when the value already matches, so
                    // selecting on it in a loop turned a stopped collector
                    // into a hot spin that re-killed the child forever.
                    let raced = tokio::select! {
                        biased;
                        result = &mut stream => Some(result),
                        _ = stopping.wait_for(|stopping| *stopping) => {
                            // Ends the agent's stream, which closes the queue,
                            // which lets the writer flush.
                            let _ = child.start_kill();
                            None
                        }
                    };
                    // Outside the select, so the wait is not still borrowed:
                    // this is where the flush is waited for.
                    let outcome = match raced {
                        Some(result) => result,
                        None => stream.await,
                    };
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = log_task.await;

                    // Read the guest's last line only after the log task has
                    // finished draining, or a failure that happens faster than
                    // the pipe is reported with the previous attempt's words.
                    let guest = last_stderr
                        .lock()
                        .map(|slot| slot.clone())
                        .unwrap_or_default();
                    if let Err(error) = outcome {
                        tracing::warn!(box_id = %box_id, %error, "remote observability stream stopped");
                        let detail = if guest.is_empty() {
                            format!("{error}")
                        } else {
                            format!("{error} — the agent said: {guest}")
                        };
                        let record = CaptureHealth::new(&box_id, CaptureState::Failed)
                            .with_transport("exec")
                            .with_detail(detail)
                            .with_attempts(attempts);
                        if let Err(error) = health::publish(&state_dir, &record) {
                            tracing::warn!(box_id = %box_id, %error, "publish capture health");
                        }
                    }
                }
            }),
            ready: Some(ready_rx),
            agent_seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live,
            stop: Some(stop),
            started_at: Instant::now(),
            state_identity,
            _claim: claim,
        })
    }

    async fn stop_all(&mut self) {
        for (_, active) in self.active.drain() {
            if let Ok(mut live) = active.live.lock() {
                *live = false;
            }
            Self::retire(active.task, active.stop).await;
        }
    }

    /// End one collector, giving it the chance to flush what it has.
    ///
    /// An abort skips everything after the accept loop, the writer's handle
    /// included — so the last batch, up to a linger interval of events, was
    /// still in memory when the process exited. Asking first lets it drain;
    /// the abort remains the backstop for a task that will not.
    async fn retire(
        task: tokio::task::JoinHandle<()>,
        stop: Option<tokio::sync::watch::Sender<bool>>,
    ) {
        let Some(stop) = stop else {
            task.abort();
            let _ = task.await;
            return;
        };
        let _ = stop.send(true);
        tokio::pin!(task);
        if tokio::time::timeout(DRAIN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

fn retry_delay(failures: u32) -> Duration {
    let seconds = 1_u64.checked_shl(failures.min(5)).unwrap_or(u64::MAX);
    Duration::from_secs(seconds).min(MAX_RETRY_DELAY)
}

fn state_identity(state_dir: &std::path::Path, name: &str) -> Result<StateIdentity> {
    let path = state_dir.join("sandboxes").join(name).join("state.json");
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("read collector state identity at {}", path.display()))?;
    Ok(StateIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

fn try_claim_collector(state_dir: &std::path::Path, name: &str) -> Result<Option<CollectorClaim>> {
    if !crate::sandbox::state::is_safe_name(name) {
        anyhow::bail!("refusing a collector lock for unsafe box name {name:?}");
    }
    let directory = state_dir.join("locks");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create collector lock directory {}", directory.display()))?;
    let path = directory.join(format!("collector-{name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open collector lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(CollectorClaim { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(anyhow::Error::new(error))
            .with_context(|| format!("evaluate collector ownership for box '{name}'")),
    }
}

fn remote_agent_args(name: &str, has_ebpf: bool) -> Vec<String> {
    // Decide privilege in the guest. Docker exec commonly starts as root and
    // minimal images have no sudo; VM runtimes start as an ordinary user. The
    // fixed shell script forwards every following token through "$@", so the
    // framed stdin/stdout remain the agent's and no box-controlled value is
    // interpolated into shell syntax.
    let mut direct = vec![
        "/usr/local/bin/devbox-obsd".to_string(),
        "-box-id".to_string(),
        name.to_string(),
        "-stdio".to_string(),
        "-packet=true".to_string(),
        "-policy".to_string(),
        "/etc/devbox/policy.json".to_string(),
        // The system service remains the only firewall owner. This exec agent
        // reads the policy for capture/posture context but must not race the
        // service's delete+add updates to the DNS-backed allow sets.
        "-restore-policy=false".to_string(),
    ];
    if !has_ebpf {
        direct.push("-no-ebpf".to_string());
    }
    let mut wrapped = vec![
        "sh".to_string(),
        "-c".to_string(),
        "if [ \"$(id -u)\" -eq 0 ]; then exec \"$@\"; else exec sudo -n \"$@\"; fi".to_string(),
        "devbox-obsd".to_string(), // sh -c's $0; "$@" starts after it.
    ];
    wrapped.append(&mut direct);
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::{SCHEMA, SandboxState};

    fn save_box(state_dir: &std::path::Path, name: &str) {
        SandboxState {
            schema: SCHEMA,
            name: name.into(),
            runtime: "test".into(),
            project_dir: state_dir.join("project"),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec![],
            languages: vec![],
            image: "nixos".into(),
            packages: vec![],
            package_sources: Default::default(),
        }
        .save(state_dir)
        .unwrap();
    }

    #[test]
    fn failed_runtime_probes_back_off_to_a_ceiling() {
        let got: Vec<_> = (0..8).map(retry_delay).collect();
        assert_eq!(
            got,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn remote_agent_privilege_is_decided_inside_the_guest_without_touching_stdio() {
        let args = remote_agent_args("alpha", crate::obs::uses_ebpf("docker"));
        assert_eq!(&args[..2], ["sh", "-c"]);
        assert!(args[2].contains("id -u"));
        assert!(args[2].contains("exec \"$@\""));
        assert!(args[2].contains("sudo -n \"$@\""));
        assert_eq!(args[4], "/usr/local/bin/devbox-obsd");
        assert!(args.windows(2).any(|pair| pair == ["-box-id", "alpha"]));
        assert!(args.iter().any(|arg| arg == "-stdio"));
        assert!(
            args.iter().any(|arg| arg == "-no-ebpf"),
            "a Docker exec agent must not trace the shared kernel"
        );
    }

    #[tokio::test]
    async fn registered_boxes_get_listeners_and_removed_boxes_lose_them() {
        let dir = tempfile::tempdir().unwrap();
        save_box(dir.path(), "alpha");
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        let mut supervisor = Supervisor::new(manager, Arc::new(Stats::default()));

        supervisor.reconcile().await.unwrap();
        assert!(supervisor.active.contains_key("alpha"));
        assert!(socket_path(dir.path(), "alpha").exists());
        assert!(store_path(dir.path(), "alpha").exists());
        let box_dir = socket_path(dir.path(), "alpha")
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(
            std::fs::metadata(box_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(socket_path(dir.path(), "alpha"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        SandboxState::remove(dir.path(), "alpha").unwrap();
        supervisor.reconcile().await.unwrap();
        assert!(!supervisor.active.contains_key("alpha"));
        supervisor.stop_all().await;
    }

    #[tokio::test]
    async fn same_name_recreation_replaces_the_old_collector_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        save_box(dir.path(), "alpha");
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        let mut supervisor = Supervisor::new(manager, Arc::new(Stats::default()));

        supervisor.reconcile().await.unwrap();
        let old_identity = supervisor.active["alpha"].state_identity;

        // Destroy and recreate entirely between two reconcile ticks. A map
        // keyed only by name never observed the absence and kept the old DB.
        crate::obs::collector::remove_box_data(dir.path(), "alpha").unwrap();
        SandboxState::remove(dir.path(), "alpha").unwrap();
        save_box(dir.path(), "alpha");
        let new_identity = state_identity(dir.path(), "alpha").unwrap();
        assert_ne!(old_identity, new_identity);

        supervisor.reconcile().await.unwrap();
        assert_eq!(supervisor.active["alpha"].state_identity, new_identity);
        assert!(socket_path(dir.path(), "alpha").exists());
        supervisor.stop_all().await;
    }

    #[tokio::test]
    async fn a_second_console_does_not_duplicate_one_boxes_collector() {
        let dir = tempfile::tempdir().unwrap();
        save_box(dir.path(), "alpha");
        let manager = Arc::new(SandboxManager {
            state_dir: dir.path().to_path_buf(),
        });
        let mut first = Supervisor::new(manager.clone(), Arc::new(Stats::default()));
        let mut second = Supervisor::new(manager, Arc::new(Stats::default()));

        first.reconcile().await.unwrap();
        second.reconcile().await.unwrap();
        assert!(first.active.contains_key("alpha"));
        assert!(
            second.active.is_empty(),
            "the second console duplicated capture"
        );
        assert!(second.retries.contains_key("alpha"));

        first.stop_all().await;
        // Do not make the test sleep through production backoff. Once the
        // owner exits, the next due retry must be able to take over.
        second.retries.clear();
        second.reconcile().await.unwrap();
        assert!(second.active.contains_key("alpha"));
        second.stop_all().await;
    }
}
