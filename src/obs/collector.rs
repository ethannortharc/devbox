//! Collector — the host side of the agent protocol (§11.3).
//!
//! Listens on a unix socket, performs the version handshake, then reads
//! length-prefixed frames of events, batches them into the store, and fans
//! them out to the console's live stream.
//!
//! Framing matches `agent/transport`: a 4-byte big-endian length followed by
//! that many bytes of payload. The payload is JSON today and versioned so it
//! can become protobuf without changing the framer (ADR-0015).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::event::Event;
use super::run::{Attributor, RunTag};
use super::store::{Retention, Store};

/// Protocol version, matching `transport.ProtocolVersion` in Go.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long an accepted connection has to say hello.
///
/// A socket the guest can reach is a socket a compromised guest can open and
/// then hold silent. Without a deadline each one occupies a task and a file
/// descriptor for as long as it likes.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Most connections one collector will serve at once.
///
/// One agent per box is the design; a handful covers a restart overlapping its
/// predecessor. Beyond that the far side is not an agent, and refusing is
/// better than running the daemon out of descriptors.
pub const MAX_CONNECTIONS: usize = 8;

/// Maximum accepted frame, matching `transport.MaxFrameSize`.
///
/// A corrupt or hostile length prefix must not make the collector allocate a
/// gigabyte; the agent enforces the same bound on the way out.
pub const MAX_FRAME_SIZE: usize = 1 << 20;

/// How many events to accumulate before writing.
///
/// One transaction per burst rather than per event is the difference between
/// a few thousand and a few hundred thousand events per second.
pub const BATCH_SIZE: usize = 256;

/// How long to wait for a batch to fill before writing what is there.
pub const BATCH_LINGER: Duration = Duration::from_millis(200);

/// Bounded queue between the socket reader and the writer.
///
/// Bounded on purpose (§7.3): when the box outruns the store, events are
/// dropped and *counted*, never silently discarded and never allowed to grow
/// into unbounded memory.
pub const QUEUE_DEPTH: usize = 8192;

/// Bytes of queued events one collector will hold.
///
/// A depth is not a bound on memory. One event may be a whole frame, so eight
/// thousand of them is eight gigabytes — reachable by a guest that simply
/// sends large events faster than SQLite accepts them, and reached long before
/// the depth limit starts counting drops. Whichever limit binds first wins;
/// both drop and count.
pub const QUEUE_BYTES: usize = 64 * 1024 * 1024;

/// The agent's opening frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    #[serde(default)]
    pub version: String,
    pub box_id: String,
    #[serde(default)]
    pub capture: Vec<String>,
    /// The composed capture backends that survived the agent's preflight —
    /// `ebpf+packet+netfilter`, `proc+packet`. Empty from an agent built
    /// before the field existed, which is why every reader falls back to
    /// [`Hello::ebpf`] rather than treating empty as "no capture".
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub ebpf: bool,
    /// Path prefixes a file event has to be under to be sent at all.
    ///
    /// `file` in [`Hello::capture`] says the probe is attached; this says how
    /// much of the filesystem it reports. Empty from an agent that predates
    /// the field and from one told to report every path, which are the same
    /// thing to every reader: not narrowed.
    #[serde(default)]
    pub file_scope: Vec<String>,
    /// The agent process's pid inside the box.
    ///
    /// Distinguishes two things that look identical from the host: the
    /// collector re-publishing its view of a stream that never went away, and
    /// a stream now served by a different agent process. Only the second can
    /// have lost events. Zero from an agent predating the field, which reads
    /// as "cannot tell" rather than as "did not change".
    #[serde(default)]
    pub pid: u32,
}

/// The collector's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub protocol: u32,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// Counters the metrics exporter reads (§7.7).
#[derive(Debug, Default)]
pub struct Stats {
    pub received: AtomicU64,
    pub stored: AtomicU64,
    /// Events dropped because the queue was full. Never silently zero.
    pub dropped: AtomicU64,
    /// Events lost because the store would not take them.
    ///
    /// Separate from `dropped` because the causes are unrelated and so are the
    /// responses: a full queue means the collector is behind, a failed insert
    /// means the disk is full or the database is damaged. Sharing one counter
    /// would have made the second look like the first.
    pub persist_failed: AtomicU64,
    pub rejected: AtomicU64,
    pub agents_connected: AtomicU64,
}

impl Stats {
    /// A snapshot, for rendering.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            received: self.received.load(Ordering::Relaxed),
            stored: self.stored.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            persist_failed: self.persist_failed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            agents_connected: self.agents_connected.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time copy of [`Stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub received: u64,
    pub stored: u64,
    pub dropped: u64,
    pub persist_failed: u64,
    pub rejected: u64,
    pub agents_connected: u64,
}

/// Read one length-prefixed frame.
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    // Read the header a byte at a time for the first byte, so "nothing at all"
    // can be told apart from "half a header". `read_exact` reports both as
    // UnexpectedEof, and treating a truncated header as a clean hangup hides
    // the difference between an agent that finished and one that was killed
    // mid-frame — which is precisely what a chaos test needs to see.
    let mut header = [0u8; 4];
    match reader.read(&mut header[..1]).await {
        Ok(0) => return Ok(None), // clean EOF on a frame boundary
        Ok(_) => {}
        Err(e) => return Err(e).context("failed to read a frame header"),
    }
    reader
        .read_exact(&mut header[1..])
        .await
        .context("frame header truncated: the agent stopped mid-frame")?;

    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_FRAME_SIZE {
        bail!("frame of {size} bytes exceeds the {MAX_FRAME_SIZE}-byte limit");
    }
    if size == 0 {
        return Ok(Some(Vec::new()));
    }

    let mut payload = vec![0u8; size];
    reader
        .read_exact(&mut payload)
        .await
        .with_context(|| format!("failed to read a {size}-byte frame payload"))?;
    Ok(Some(payload))
}

/// Write one length-prefixed frame.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        bail!("refusing to send a {}-byte frame", payload.len());
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .context("failed to write a frame header")?;
    writer
        .write_all(payload)
        .await
        .context("failed to write a frame payload")?;
    writer.flush().await.context("failed to flush a frame")?;
    Ok(())
}

/// Decide whether to accept an agent.
///
/// The protocol version must match exactly. A "close enough" match is how a
/// decoder silently misreads a struct — and the whole point of the store is
/// that what it holds can be trusted.
pub fn evaluate_hello(hello: &Hello, expected_box: Option<&str>) -> HelloAck {
    if hello.protocol != PROTOCOL_VERSION {
        return HelloAck {
            protocol: PROTOCOL_VERSION,
            accepted: false,
            reason: format!(
                "protocol mismatch: agent speaks {}, collector speaks {PROTOCOL_VERSION}",
                hello.protocol
            ),
        };
    }
    if hello.box_id.is_empty() {
        return HelloAck {
            protocol: PROTOCOL_VERSION,
            accepted: false,
            reason: "hello has no box_id; events would not be attributable".to_string(),
        };
    }
    if let Some(expected) = expected_box
        && hello.box_id != expected
    {
        return HelloAck {
            protocol: PROTOCOL_VERSION,
            accepted: false,
            reason: format!(
                "this socket belongs to box '{expected}', but the agent claims '{}'",
                hello.box_id
            ),
        };
    }

    // The version pin the module header promises. Matching protocol versions
    // says the *framing* agrees; it says nothing about the record layouts
    // inside, which are asserted byte-for-byte against this release's structs.
    // An agent from another release can hand over frames that parse and decode
    // to nonsense, which is worse than refusing it.
    //
    // A development build (`0.0.0`, or any version containing `-dev`) is
    // exempt: that is someone running a locally built agent against a locally
    // built collector on purpose.
    if hello.version.is_empty() {
        return HelloAck {
            protocol: PROTOCOL_VERSION,
            accepted: false,
            reason: "hello names no agent version; event layouts are pinned per release"
                .to_string(),
        };
    }
    let host_version = env!("CARGO_PKG_VERSION");
    if !is_development_build(&hello.version)
        && !is_development_build(host_version)
        && hello.version != host_version
    {
        return HelloAck {
            protocol: PROTOCOL_VERSION,
            accepted: false,
            reason: format!(
                "agent is devbox {} but this collector is {host_version}; \
                 event layouts are pinned per release",
                hello.version
            ),
        };
    }

    HelloAck {
        protocol: PROTOCOL_VERSION,
        accepted: true,
        reason: String::new(),
    }
}

/// Is this a build that should skip the release pin?
fn is_development_build(version: &str) -> bool {
    // An *absent* version is not one. It used to be treated as a development
    // build, which meant an agent that simply omitted the field skipped the
    // release pin entirely — the one check standing between a mismatched
    // event layout and a store full of plausible nonsense.
    version == "0.0.0" || version.contains("-dev")
}

/// Called when an accepted agent arrives (`Some`) and again when its
/// connection ends (`None`), identified by a connection number.
///
/// Both edges matter. Only the exec transport's supervisor notices a departure
/// on its own — its child dies with the box. A host-side socket listener
/// outlives the container that was dialling it, so without the closing call
/// its box would be reported as capturing for as long as the console ran.
///
/// The number is what lets a consumer tell *which* connection left. Counting
/// them was not enough: an agent restarting overlaps its predecessor, and when
/// the newer one then died the older one's box kept advertising the newer
/// one's capture backends.
type AgentHook = Arc<dyn Fn(u64, Option<&Hello>) + Send + Sync>;

/// A running collector.
pub struct Collector {
    socket_path: PathBuf,
    store: Arc<Mutex<Store>>,
    stats: Arc<Stats>,
    /// Every stored event is republished here for the console's live view.
    live: tokio::sync::broadcast::Sender<Event>,
    box_id: Option<String>,
    retention: Retention,
    /// Called once per accepted agent, with what it said about itself.
    on_agent: Option<AgentHook>,
    /// Numbers accepted connections, so the hook can tell them apart.
    connections: AtomicU64,
    /// Bytes of events accepted into the queue and not yet written.
    queued_bytes: std::sync::atomic::AtomicUsize,
    /// Which run each event belongs to (§4.2).
    ///
    /// Lives beside the store rather than inside it because it is *history*,
    /// not rows: the parent-chain rule can only answer "descends from the run's
    /// root pid" for pids it has already watched go past.
    attributor: Mutex<Attributor>,
}

/// An event on its way to the store, with the size it arrived as.
///
/// The size travels with it so the writer can release exactly what the reader
/// reserved — re-measuring it there would drift from what was counted in.
struct Queued {
    event: Event,
    size: usize,
}

impl Collector {
    /// Create a collector bound to `socket_path`, writing into `store`.
    pub fn new(socket_path: PathBuf, store: Store) -> Self {
        let (live, _) = tokio::sync::broadcast::channel(1024);
        Self {
            socket_path,
            store: Arc::new(Mutex::new(store)),
            stats: Arc::new(Stats::default()),
            live,
            box_id: None,
            retention: Retention::default(),
            on_agent: None,
            connections: AtomicU64::new(0),
            queued_bytes: std::sync::atomic::AtomicUsize::new(0),
            attributor: Mutex::new(Attributor::new()),
        }
    }

    /// Only accept agents claiming this box.
    pub fn for_box(mut self, box_id: impl Into<String>) -> Self {
        self.box_id = Some(box_id.into());
        self
    }

    /// Share counters with sibling collectors.
    ///
    /// The console runs one listener per box but exposes one `/metrics`
    /// endpoint. Supplying the same counter set to every collector makes that
    /// endpoint the aggregate rather than the counters of whichever box was
    /// started last.
    pub fn with_stats(mut self, stats: Arc<Stats>) -> Self {
        self.stats = stats;
        self
    }

    /// Override the retention policy.
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// Observe every accepted agent's hello.
    ///
    /// The handshake is the only moment the host learns which capture
    /// backends attached; nothing downstream of it carries that. The hook runs
    /// on the connection's task, so it must not block.
    pub fn with_agent_hook(
        mut self,
        hook: impl Fn(u64, Option<&Hello>) + Send + Sync + 'static,
    ) -> Self {
        self.on_agent = Some(Arc::new(hook));
        self
    }

    /// Subscribe to the live event stream.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.live.subscribe()
    }

    /// Counters for the metrics exporter.
    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    /// The store, for queries.
    pub fn store(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    /// Bind the listener, removing a stale socket file first.
    pub fn bind(&self) -> Result<UnixListener> {
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            // The collector trusts the box id in the handshake, so the local
            // endpoint itself is an authority boundary. A world-searchable
            // box directory plus a writable socket lets another host user
            // inject a forged timeline for any known box.
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to protect {}", parent.display()))?;
        }
        // A previous run that was killed leaves the socket file behind; bind
        // would then fail with EADDRINUSE even though nothing is listening.
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .with_context(|| format!("failed to remove {}", self.socket_path.display()))?;
        }
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("failed to listen on {}", self.socket_path.display()))?;
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", self.socket_path.display()))?;
        Ok(listener)
    }

    /// Accept agents until the listener fails, or until told to stop.
    ///
    /// `stop` matters for shutdown, not for tidiness. Aborting the task that
    /// runs this skips everything after the accept loop: the writer's handle
    /// is dropped rather than awaited, so its final batch — up to a linger
    /// interval of events — is still in memory when the process exits. Asked
    /// to stop, it drains instead.
    pub async fn run_until(
        self: Arc<Self>,
        listener: UnixListener,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel::<Queued>(QUEUE_DEPTH);

        // The writer lives in a set of its own, and the reason is its Drop.
        // Spawned bare, its handle was *dropped* rather than aborted when this
        // task was — detaching it, so a writer still flushing could run
        // alongside its replacement. A `JoinSet` aborts what it holds when it
        // is dropped, which is what an abort of this task now reaches.
        //
        // Its own set, not the connections': every connection holds a sender,
        // so the writer cannot finish until they are gone. Waiting for it in
        // the same set deadlocks — the writer is the last thing to end, by
        // construction.
        //
        // Connections are owned by this loop too, not detached from it.
        //
        // Aborting the task that runs `run` only ever stopped the accept loop;
        // every connection it had spawned kept reading and kept writing into
        // the store. A box whose `state.json` is rewritten — which an ordinary
        // atomic save does — retires this collector and starts its
        // replacement, and the old agent then wrote alongside the new one,
        // duplicating every event. A `JoinSet` aborts what it holds when it is
        // dropped, which is what an abort of this task now reaches.
        let mut connections = tokio::task::JoinSet::new();
        let mut writer = tokio::task::JoinSet::new();
        writer.spawn(Arc::clone(&self).write_loop(rx));

        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop.wait_for(|stopping| *stopping) => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, _) = match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(error = %e, "collector accept failed");
                    break;
                }
            };

            // Finished connections are reaped here rather than accumulating
            // for the life of the console.
            while connections.try_join_next().is_some() {}
            if connections.len() >= MAX_CONNECTIONS {
                // One agent per box is the design. Something opening more than
                // a handful is not one, and the honest response is to close
                // rather than to hold a descriptor for it.
                tracing::warn!(
                    open = connections.len(),
                    "refusing an agent connection: too many already open"
                );
                drop(stream);
                continue;
            }

            let me = Arc::clone(&self);
            let tx = tx.clone();
            connections.spawn(async move {
                // Counted inside `serve_agent`, once the hello is accepted.
                // Incrementing here counted malformed clients and agents
                // rejected for the wrong box, so a retry loop inflated the
                // metric without a single agent ever connecting.
                let (mut reader, mut writer) = stream.into_split();
                if let Err(e) = me
                    .serve_agent(&mut reader, &mut writer, tx, None, false)
                    .await
                {
                    tracing::warn!(error = %e, "agent connection ended");
                }
            });
        }

        // Connections first — each holds a sender, and the writer ends when
        // the last one is dropped. Then ours. Then the writer drains what is
        // still queued and returns on its own.
        connections.shutdown().await;
        drop(tx);
        while writer.join_next().await.is_some() {}
        Ok(())
    }

    /// Serve one agent carried by a runtime's authenticated exec stdio.
    ///
    /// VM kernels cannot connect to a host AF_UNIX inode exposed through 9p or
    /// virtiofs. Lima, Multipass and Incus already provide a bidirectional,
    /// authenticated exec stream, so the same framed protocol rides that
    /// stream without opening a host network listener.
    pub async fn run_agent_stream<R, W>(self: Arc<Self>, mut reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        self.run_agent_stream_inner(&mut reader, &mut writer, None)
            .await
    }

    /// Serve an exec stream and signal once its handshake is accepted.
    ///
    /// The supervisor uses this exact boundary to reset retry backoff. A child
    /// merely spawning is not success: a missing agent, rejected version or
    /// closed stdin can all fail before one event is trustworthy.
    ///
    pub(crate) async fn run_agent_stream_ready<R, W>(
        self: Arc<Self>,
        mut reader: R,
        mut writer: W,
        ready: oneshot::Sender<()>,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        self.run_agent_stream_inner(&mut reader, &mut writer, Some(ready))
            .await
    }

    async fn run_agent_stream_inner<R, W>(
        self: Arc<Self>,
        reader: &mut R,
        writer: &mut W,
        ready: Option<oneshot::Sender<()>>,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let (tx, rx) = mpsc::channel::<Queued>(QUEUE_DEPTH);
        // A set rather than a bare handle, for its Drop. Aborting the task
        // that runs this dropped the handle instead of the writer, detaching
        // it — so a writer still flushing could outlive the collector it
        // belonged to and modify the store alongside its replacement.
        let mut store_writer = tokio::task::JoinSet::new();
        store_writer.spawn(Arc::clone(&self).write_loop(rx));
        let served = self.serve_agent(reader, writer, tx, ready, true).await;
        // `tx` was moved into `serve_agent` and is gone by now, so the queue
        // is closed and the writer drains and returns on its own.
        while store_writer.join_next().await.is_some() {}
        served
    }

    /// Accept agents until the listener fails.
    pub async fn run(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        let (keep, never) = tokio::sync::watch::channel(false);
        // Held for the call, so the receiver never sees its sender drop.
        let result = self.run_until(listener, never).await;
        drop(keep);
        result
    }

    /// Handshake with one agent, then read its event stream.
    async fn serve_agent<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        tx: mpsc::Sender<Queued>,
        ready: Option<oneshot::Sender<()>>,
        send_heartbeats: bool,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let hello_frame = tokio::time::timeout(HELLO_TIMEOUT, read_frame(reader))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "agent did not say hello within {}s",
                    HELLO_TIMEOUT.as_secs()
                )
            })??;
        let Some(frame) = hello_frame else {
            bail!("agent closed the connection before saying hello");
        };
        let hello: Hello =
            serde_json::from_slice(&frame).context("agent sent a malformed hello")?;

        let ack = evaluate_hello(&hello, self.box_id.as_deref());
        write_frame(writer, &serde_json::to_vec(&ack)?).await?;
        if !ack.accepted {
            bail!("rejected agent: {}", ack.reason);
        }
        // Only now: an accepted agent is what the metric claims to count.
        self.stats.agents_connected.fetch_add(1, Ordering::Relaxed);
        if let Some(ready) = ready {
            let _ = ready.send(());
        }
        // Both transports pass through here, so both report. The readiness
        // channel deliberately stays a bare boundary marker: it exists to
        // reset retry backoff for one exec child, and only the exec path has
        // one. Who connected, with which backends, is a different question
        // that the accept loop must answer too.
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        if let Some(hook) = &self.on_agent {
            hook(connection, Some(&hello));
        }

        tracing::info!(
            box_id = %hello.box_id,
            version = %hello.version,
            ebpf = hello.ebpf,
            // Without this, three log lines around a restart cannot answer the
            // one question that decides whether anything was lost: is this the
            // same agent process, or a new one?
            pid = hello.pid,
            "agent connected"
        );

        let agent_pid = hello.pid;
        let connected_at = std::time::Instant::now();
        let box_id = hello.box_id;
        // The reader decides when this connection is over. Nothing else may.
        //
        // This used to be a plain `select!` against `write_heartbeats`, and a
        // `select!` cancels the arm that did not finish — so a keepalive that
        // failed took the read loop down with it, mid-drain, and every frame
        // the agent had already sent but the collector had not yet read was
        // never read and never counted. It looked like the agent had sent
        // fewer events than it did, which is the one thing `received` exists
        // to be able to deny.
        //
        // A failed keepalive says the *write* half is gone. That is not a
        // reason to stop reading what the agent already wrote — those bytes
        // are in the buffer either way, and reading them is how they get
        // counted.
        // Counted out here, not returned, so that a stream which ends in an
        // error still says how much it carried before it broke. A count
        // returned alongside an error is a count thrown away, and `events = 0`
        // on a connection that delivered fifty is worse than no line at all.
        let mut arrived = 0u64;
        let served = if send_heartbeats {
            tokio::select! {
                result = self.read_event_stream(reader, tx, &box_id, &mut arrived) => result,
                () = keepalive(writer) => unreachable!("the keepalive never resolves"),
            }
        } else {
            self.read_event_stream(reader, tx, &box_id, &mut arrived)
                .await
        };
        // The collector's own statement that this stream is over.
        //
        // The supervisor logs a stream that ended in *error*, and the agent
        // forwards its own last words — but an agent ended by a signal closes
        // cleanly, so the ordinary restart produced no line from the collector
        // at all. Read with the `agent connected` line that follows it, this
        // says how long the box went unwatched and whether the agent that came
        // back is the one that left.
        tracing::info!(
            box_id = %box_id,
            pid = agent_pid,
            events = arrived,
            elapsed_ms = connected_at.elapsed().as_millis() as u64,
            ok = served.is_ok(),
            "observability agent stream ended"
        );
        // The closing edge, reported only for an agent that was accepted — a
        // refused one never claimed to be capturing.
        if let Some(hook) = &self.on_agent {
            hook(connection, None);
        }
        served
    }

    /// Read frames until the agent's stream ends, tallying them in `arrived`.
    ///
    /// The tally is the collector's own answer to "how much did this
    /// connection carry", which is what makes the line it ends on worth
    /// reading: a stream that ended after four events and one that ended
    /// after forty thousand are different events in an operator's day. It is
    /// written through a reference rather than returned so that neither an
    /// error nor a cancellation can take it down with them.
    async fn read_event_stream<R>(
        &self,
        reader: &mut R,
        tx: mpsc::Sender<Queued>,
        box_id: &str,
        arrived: &mut u64,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
    {
        while let Some(frame) = read_frame(reader).await? {
            if frame.is_empty() {
                continue; // keepalive
            }
            self.stats.received.fetch_add(1, Ordering::Relaxed);
            *arrived += 1;

            let mut event: Event = match serde_json::from_slice(&frame) {
                Ok(e) => e,
                Err(e) => {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "discarding an undecodable event");
                    continue;
                }
            };
            if event.validate().is_err() {
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // On arrival, before this event is stored *or* published live.
            //
            // The agent redacts at the source, so on a current box this finds
            // nothing. It is here for the box that is one release behind —
            // which is the ordinary state between upgrades — and because the
            // live channel hands events straight to the console without
            // passing through the store's read path.
            if let Some(exec) = event.exec.as_mut()
                && exec.argv.iter().any(|a| super::redact::has_secret(a))
            {
                exec.argv = super::redact::argv(&exec.argv);
            }
            // The handshake decided which box this connection speaks for.
            // Accepting an event that names a different one would let a
            // compromised agent write into another box's timeline.
            if event.box_id != box_id {
                let seen = self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                // Logged once, then counted only.
                //
                // The claimed id is guest-controlled, and this is an
                // append-only log: an agent streaming small mismatched events
                // wrote unbounded attacker-chosen text to the host's disk. The
                // counter is the honest record of how often it happened.
                if seen == 0 {
                    tracing::warn!(
                        expected = %box_id,
                        claimed = %event.box_id.escape_debug().to_string(),
                        "agent sent an event for another box; further ones are counted only"
                    );
                }
                continue;
            }

            // Never block the socket reader: a full queue means the store
            // cannot keep up, and the honest response is to drop and count.
            //
            // Measured in bytes as well as in events, using the frame this
            // arrived in. The count alone bounded how many events could be
            // waiting and not how large they were.
            let size = frame.len();
            let queued = self.queued_bytes.fetch_add(size, Ordering::Relaxed) + size;
            if queued > QUEUE_BYTES {
                self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if tx.try_send(Queued { event, size }).is_err() {
                self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Drain the queue into the store in batches.
    async fn write_loop(self: Arc<Self>, mut rx: mpsc::Receiver<Queued>) {
        let mut batch: Vec<Event> = Vec::with_capacity(BATCH_SIZE);
        // What the reader reserved for everything now in `batch`.
        let mut batch_bytes = 0usize;
        // When the *oldest* queued event must be on disk by.
        //
        // The timeout used to be recreated on every receive, so it measured
        // the gap between arrivals rather than the age of the batch: a steady
        // stream every 100ms never let it expire, and nothing was written
        // until all 256 slots filled. Queries ran about twenty-five seconds
        // behind the box, and a crash took the whole pending batch with it —
        // on a busy box, which is when the timeline matters most.
        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let wait = deadline
                .map(|at| at.saturating_duration_since(tokio::time::Instant::now()))
                .unwrap_or(BATCH_LINGER);
            let got = tokio::time::timeout(wait, rx.recv()).await;
            match got {
                Ok(Some(Queued { event, size })) => {
                    // Publish live before storing: the console should not wait
                    // on a disk write to show what just happened.
                    let _ = self.live.send(event.clone());
                    if batch.is_empty() {
                        deadline = Some(tokio::time::Instant::now() + BATCH_LINGER);
                    }
                    batch.push(event);
                    // Still reserved. Releasing here put the batch outside the
                    // budget — two hundred and fifty-six maximal events could
                    // sit in it while the queue happily accepted another
                    // budget's worth behind them.
                    batch_bytes += size;
                    if batch.len() < BATCH_SIZE {
                        continue;
                    }
                }
                // Linger expired: flush whatever is queued.
                Err(_) => {
                    if batch.is_empty() {
                        continue;
                    }
                }
                // Channel closed: flush and stop.
                Ok(None) => {
                    self.flush(&mut batch).await;
                    self.queued_bytes.fetch_sub(batch_bytes, Ordering::Relaxed);
                    return;
                }
            }
            self.flush(&mut batch).await;
            // Released only once the events are on disk, so the budget
            // describes everything the collector is still holding.
            self.queued_bytes.fetch_sub(batch_bytes, Ordering::Relaxed);
            batch_bytes = 0;
            // The next batch starts its own clock when its first event lands.
            deadline = None;
        }
    }

    async fn flush(&self, batch: &mut Vec<Event>) {
        if batch.is_empty() {
            return;
        }
        let mut store = self.store.lock().await;

        // Attribution is decided here, once per batch, not once per event.
        //
        // The live-run list is re-read from the same database the events are
        // about to be written into — the CLI wrote the run row through a
        // second connection under WAL, so this is the one read that cannot
        // disagree with it. A file under `~/.devbox/runs/<box>/active` would
        // have been a second source of truth needing a watcher, and would
        // still have had to be reconciled with the table the report reads
        // back. The query is a partial-index lookup returning at most a
        // handful of rows, and a batch is up to 256 events or 250ms of them,
        // so this costs at most a few reads a second on a busy box.
        let tags: Vec<Option<RunTag>> = {
            let mut attributor = self.attributor.lock().await;
            match store.active_runs() {
                Ok(active) => {
                    if active.is_empty() && attributor.is_idle() {
                        Vec::new()
                    } else {
                        attributor.set_active(active);
                        batch.iter().map(|e| attributor.attribute(e)).collect()
                    }
                }
                Err(e) => {
                    // A report missing its attribution is recoverable; losing
                    // the events is not. Store them unattributed and say so.
                    tracing::warn!(error = %e, "could not read the live runs; events go unattributed");
                    Vec::new()
                }
            }
        };

        match store.insert_batch_tagged(batch, &tags) {
            Ok(n) => {
                self.stats.stored.fetch_add(n as u64, Ordering::Relaxed);
                if let Err(e) = store.enforce_retention(self.retention) {
                    tracing::warn!(error = %e, "retention sweep failed");
                }
            }
            Err(e) => {
                // The transaction rolled back, so these events are gone the
                // moment the batch is cleared. Counting them is what keeps
                // §7.3's "never silently dropped" true on this path: `dropped`
                // only ever counted a full queue, so a disk that filled up
                // left a hole in the timeline while `/metrics` went on
                // reporting zero losses — and a gap in an event timeline is
                // indistinguishable from a quiet box.
                //
                // Counted rather than retained: the failures that reach here
                // are a full disk or a damaged database, neither of which the
                // next flush fixes, and holding the batch to retry it would
                // grow the queue behind it without bound.
                self.stats
                    .persist_failed
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                tracing::error!(
                    error = %e,
                    events = batch.len(),
                    "failed to store an event batch; the events are lost"
                );
            }
        }
        batch.clear();
    }
}

const EXEC_HEARTBEAT: Duration = Duration::from_secs(5);

/// Send keepalives, and then never finish.
///
/// The "never finish" is the point. This runs in a `select!` beside the read
/// loop, where completing means cancelling the other arm — and the other arm
/// is the only thing that can tell the difference between an agent that sent
/// nothing and an agent whose frames nobody read.
async fn keepalive<W>(writer: &mut W)
where
    W: AsyncWrite + Unpin,
{
    if let Err(error) = write_heartbeats(writer).await {
        // Ordinary at the end of any session: the agent closes, and the next
        // keepalive has nowhere to go. Worth a line, not a failure.
        tracing::debug!(%error, "keepalive stopped; reading on until the agent's stream ends");
    }
    std::future::pending::<()>().await
}

async fn write_heartbeats<W>(writer: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // The first tick from `interval` is immediate. Sending a control frame at
    // that instant races a healthy `-once` agent which has already delivered
    // its final event and is closing, turning an ordinary EOF into EPIPE.
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + EXEC_HEARTBEAT, EXEC_HEARTBEAT);
    loop {
        interval.tick().await;
        write_frame(writer, &[]).await?;
    }
}

/// Directory exposed to the observed box.
///
/// The SQLite store deliberately lives one level above this. Mounting the
/// whole box directory gave the subject write access to its own audit record.
pub fn endpoint_dir(state_dir: &Path, box_id: &str) -> PathBuf {
    state_dir.join("boxes").join(box_id).join("endpoint")
}

/// Default socket path for a box's agent.
pub fn socket_path(state_dir: &Path, box_id: &str) -> PathBuf {
    endpoint_dir(state_dir, box_id).join("obsd.sock")
}

/// Default event-database path for a box.
pub fn store_path(state_dir: &Path, box_id: &str) -> PathBuf {
    state_dir.join("boxes").join(box_id).join("events.db")
}

/// Remove every host-side observability artifact owned by a destroyed box.
///
/// These files deliberately live outside `sandboxes/<name>` so a compromised
/// guest cannot reach its audit record. That separation also means sandbox
/// state removal cannot clean them implicitly; destroy must call this too or
/// a later box with the same name inherits an unrelated timeline.
///
/// Two trees, for one reason. `boxes/<name>` is the event store and the
/// capture health; `runs/<name>` is the rendered reports. Leaving the second
/// behind left `devbox report <id>` answering for a box that no longer exists,
/// and — because a report is found by id across every box — handing the next
/// box created under that name a predecessor's evidence.
pub fn remove_box_data(state_dir: &Path, box_id: &str) -> Result<()> {
    if !crate::sandbox::state::is_safe_name(box_id) {
        bail!("refusing to remove observability data for unsafe box name {box_id:?}");
    }
    for directory in [
        state_dir.join("boxes").join(box_id),
        state_dir.join("runs").join(box_id),
    ] {
        if directory.exists() {
            std::fs::remove_dir_all(&directory).with_context(|| {
                format!(
                    "failed to remove observability data at {}",
                    directory.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_from_another_release_is_refused() {
        // Matching protocol versions say the *framing* agrees. The record
        // layouts inside are asserted against this release's structs, so an
        // agent from another release can hand over frames that parse and
        // decode to nonsense — worse than refusing it.
        let mut hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.0.9".into(),
            box_id: "myapp".into(),
            capture: vec!["exec".into()],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: true,
            pid: 0,
        };
        let ack = evaluate_hello(&hello, None);
        assert!(!ack.accepted);
        assert!(ack.reason.contains("pinned per release"), "{}", ack.reason);

        // A locally built agent is exempt on purpose: that is the `-dev` build
        // the cross-language pipeline test runs, and refusing it would make
        // local development impossible.
        hello.version = "0.0.0-dev".into();
        assert!(evaluate_hello(&hello, None).accepted);

        // And the release the collector itself is.
        hello.version = env!("CARGO_PKG_VERSION").into();
        assert!(evaluate_hello(&hello, None).accepted);
    }

    #[test]
    fn destroyed_box_data_is_removed_without_allowing_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let database = store_path(dir.path(), "old-box");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(&database, b"old timeline").unwrap();

        // The rendered reports too. A report is found by id across every box,
        // so one left behind answers for a box that no longer exists — and
        // hands the next box created under that name a predecessor's evidence.
        let report = dir.path().join("runs").join("old-box").join("01ABC");
        std::fs::create_dir_all(&report).unwrap();
        std::fs::write(report.join("report.json"), b"{}").unwrap();

        // A neighbour's, to prove the removal is scoped to the one box.
        let neighbour = dir.path().join("runs").join("other-box").join("01DEF");
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("report.json"), b"{}").unwrap();

        remove_box_data(dir.path(), "old-box").unwrap();
        assert!(!database.exists());
        assert!(
            !dir.path().join("runs").join("old-box").exists(),
            "the reports outlived the box they describe"
        );
        assert!(neighbour.join("report.json").exists());

        assert!(remove_box_data(dir.path(), "../escape").is_err());
        assert!(dir.path().exists());

        // Idempotent: a destroy that already cleaned up, or a box that never
        // produced a report, is not an error.
        remove_box_data(dir.path(), "old-box").unwrap();
    }

    #[test]
    fn paths_are_scoped_per_box() {
        let dir = Path::new("/home/x/.devbox");
        assert_eq!(
            socket_path(dir, "myapp"),
            PathBuf::from("/home/x/.devbox/boxes/myapp/endpoint/obsd.sock")
        );
        assert_eq!(
            store_path(dir, "myapp"),
            PathBuf::from("/home/x/.devbox/boxes/myapp/events.db")
        );
        assert_ne!(socket_path(dir, "a"), socket_path(dir, "b"));
    }

    #[test]
    fn handshake_accepts_a_matching_agent() {
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            box_id: "myapp".into(),
            capture: vec!["exec".into()],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: true,
            pid: 0,
        };
        assert!(evaluate_hello(&hello, None).accepted);
        assert!(evaluate_hello(&hello, Some("myapp")).accepted);
    }

    #[test]
    fn handshake_rejects_a_protocol_mismatch() {
        let hello = Hello {
            protocol: PROTOCOL_VERSION + 1,
            version: "0.1.3".into(),
            box_id: "myapp".into(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: true,
            pid: 0,
        };
        let ack = evaluate_hello(&hello, None);
        assert!(!ack.accepted);
        assert!(ack.reason.contains("protocol mismatch"), "{}", ack.reason);
    }

    #[test]
    fn handshake_rejects_a_nameless_or_foreign_agent() {
        let nameless = Hello {
            protocol: PROTOCOL_VERSION,
            version: String::new(),
            box_id: String::new(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: false,
            pid: 0,
        };
        assert!(!evaluate_hello(&nameless, None).accepted);

        let foreign = Hello {
            box_id: "other".into(),
            ..nameless.clone()
        };
        let ack = evaluate_hello(&foreign, Some("myapp"));
        assert!(!ack.accepted);
        assert!(ack.reason.contains("myapp"), "{}", ack.reason);
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let payloads: Vec<Vec<u8>> = vec![
            b"hello".to_vec(),
            Vec::new(),
            vec![0u8, 0xff, b'\n', b'\r'], // bytes a line framer would ruin
            vec![7u8; 5000],
        ];

        let mut buf: Vec<u8> = Vec::new();
        for p in &payloads {
            write_frame(&mut buf, p).await.unwrap();
        }

        let mut reader = buf.as_slice();
        for want in &payloads {
            let got = read_frame(&mut reader).await.unwrap().unwrap();
            assert_eq!(&got, want);
        }
        assert!(
            read_frame(&mut reader).await.unwrap().is_none(),
            "a drained stream reports a clean end"
        );
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_refused_without_allocating() {
        let header = u32::MAX.to_be_bytes();
        let mut reader = header.as_slice();
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error_not_a_clean_end() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"hello world").await.unwrap();
        buf.truncate(6);

        let mut reader = buf.as_slice();
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn the_queue_releases_every_byte_it_reserved() {
        // The queue is bounded by bytes as well as by depth — a depth bounds
        // how many events can be waiting, not how large they are, and one
        // frame may be a megabyte. The bound is only as good as the release:
        // a reservation that outlives its event accumulates until the
        // collector refuses everything, permanently and silently.
        let collector = Arc::new(
            Collector::new(
                std::path::PathBuf::from("/unused"),
                Store::open_in_memory().unwrap(),
            )
            .for_box("alpha"),
        );
        let stats = collector.stats();

        let (mut agent, host) = tokio::io::duplex(1 << 20);
        let (host_reader, host_writer) = tokio::io::split(host);
        let served =
            tokio::spawn(Arc::clone(&collector).run_agent_stream(host_reader, host_writer));

        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            box_id: "alpha".into(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: false,
            pid: 0,
        };
        write_frame(&mut agent, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        let _ = read_frame(&mut agent).await;

        // Enough to go well past the byte budget, so the queue has to shed —
        // which is the thing under test. The number itself is not load-bearing.
        const SENT: u64 = 600;

        // Each event carries a large, valid payload.
        let mut event = crate::obs::Event {
            ts_wall: "2026-08-06T22:14:01.000Z".into(),
            ts_mono_ns: 1,
            box_id: "alpha".into(),
            cgroup_id: 1,
            pid: 1,
            tid: 1,
            ppid: 1,
            comm: "x".repeat(200_000),
            uid: 0,
            kind: crate::obs::EventType::Exec,
            net: None,
            exec: Some(crate::obs::event::Exec {
                path: "/bin/true".into(),
                ..Default::default()
            }),
            file: None,
            api: None,
            policy: None,
            credential: None,
        };
        for i in 0..SENT {
            // 200 KB each: six hundred of them is well past the byte budget,
            // and how many survive depends on how fast the writer drains.
            event.ts_mono_ns = i + 1;
            write_frame(&mut agent, &serde_json::to_vec(&event).unwrap())
                .await
                .unwrap();
        }
        drop(agent);
        let _ = served.await;

        let snapshot = stats.snapshot();
        // The invariant, not a race. How many of the six hundred survive the
        // queue depends on how fast the writer drains, which depends on the
        // machine — but `received` counts what *arrived*, and all six hundred
        // did: every one of them was written and acknowledged by the duplex
        // before the agent was dropped.
        assert_eq!(snapshot.received, SENT, "every event arrived");
        assert_eq!(
            collector
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a drained queue still holds a reservation"
        );
        // Whatever the writer could not keep up with was dropped and counted,
        // never silently discarded.
        assert_eq!(
            snapshot.stored + snapshot.dropped + snapshot.persist_failed,
            snapshot.received,
            "every arrival was either stored or counted as lost"
        );
    }

    /// A writer that accepts the handshake and then fails everything after.
    ///
    /// Stands in for the half of a connection that goes away first, which is
    /// what a keepalive discovers and what used to end the read loop with it.
    struct WriterThatDiesAfterTheAck {
        writes: usize,
    }

    impl tokio::io::AsyncWrite for WriterThatDiesAfterTheAck {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            // The ack is two writes: the length prefix and the payload.
            self.writes += 1;
            if self.writes <= 2 {
                std::task::Poll::Ready(Ok(buf.len()))
            } else {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the agent stopped reading",
                )))
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_write_half_does_not_stop_the_collector_reading() {
        // The bug behind four separate flaky-test sightings, none of which
        // wrote down the name. `serve_agent` ran the read loop in a `select!`
        // beside the keepalive writer, and `select!` cancels the arm that did
        // not finish — so a keepalive that failed took the reader down with
        // it, and every frame already sitting in the buffer went unread and
        // therefore uncounted. `received` said the agent had sent fewer events
        // than it had, which is the one claim that counter cannot be allowed
        // to get wrong.
        //
        // Here the write half is dead from the moment the ack is out, and the
        // read half still holds three events. All three must be counted.
        let collector = Arc::new(
            Collector::new(
                std::path::PathBuf::from("/unused"),
                Store::open_in_memory().unwrap(),
            )
            .for_box("alpha"),
        );
        let stats = collector.stats();

        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            box_id: "alpha".into(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: false,
            pid: 0,
        };
        let event = crate::obs::Event {
            ts_wall: "2026-08-06T22:14:01.000Z".into(),
            ts_mono_ns: 1,
            box_id: "alpha".into(),
            cgroup_id: 1,
            pid: 1,
            tid: 1,
            ppid: 1,
            comm: "x".into(),
            uid: 0,
            kind: crate::obs::EventType::Exec,
            net: None,
            exec: Some(crate::obs::event::Exec {
                path: "/bin/true".into(),
                ..Default::default()
            }),
            file: None,
            api: None,
            policy: None,
            credential: None,
        };

        // The agent says hello, goes quiet past the first keepalive, and only
        // then sends its events. Time is paused, so "past the first keepalive"
        // costs nothing: the runtime jumps to each timer in turn, and the
        // ordering — keepalive at five seconds, events at six — is exact
        // rather than a race the machine's load decides.
        let (mut agent, host) = tokio::io::duplex(1 << 20);
        let (mut reader, _unused) = tokio::io::split(host);
        write_frame(&mut agent, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        let payload = serde_json::to_vec(&event).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(EXEC_HEARTBEAT + Duration::from_secs(1)).await;
            for _ in 0..3 {
                write_frame(&mut agent, &payload).await.unwrap();
            }
            drop(agent);
        });

        let mut writer = WriterThatDiesAfterTheAck { writes: 0 };
        let (tx, rx) = mpsc::channel::<Queued>(QUEUE_DEPTH);
        let mut store_writer = tokio::task::JoinSet::new();
        store_writer.spawn(Arc::clone(&collector).write_loop(rx));
        let _ = collector
            .serve_agent(&mut reader, &mut writer, tx, None, true)
            .await;
        while store_writer.join_next().await.is_some() {}

        let snapshot = stats.snapshot();
        assert_eq!(
            snapshot.received, 3,
            "frames the agent had already sent went uncounted"
        );
        assert_eq!(
            snapshot.stored + snapshot.dropped + snapshot.persist_failed,
            snapshot.received
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_says_which_agent_ended_and_how_much_it_carried() {
        // An agent stopped by a signal ends its stream cleanly: no error, so
        // nothing was logged, so a restart looked exactly like nothing having
        // happened. Reading the log after a capture gap, the two questions are
        // "is the agent that came back the one that left" and "how long was it
        // gone" — which needs a pid on both edges and a line on the closing
        // one.
        let log = crate::obs::testlog::Captured::install();
        let collector = Arc::new(
            Collector::new(
                std::path::PathBuf::from("/unused"),
                Store::open_in_memory().unwrap(),
            )
            .for_box("alpha"),
        );

        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            box_id: "alpha".into(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: false,
            pid: 4242,
        };
        let event = crate::obs::Event {
            ts_wall: "2026-09-05T09:00:00.000Z".into(),
            ts_mono_ns: 1,
            box_id: "alpha".into(),
            cgroup_id: 1,
            pid: 1,
            tid: 1,
            ppid: 1,
            comm: "x".into(),
            uid: 0,
            kind: crate::obs::EventType::Exec,
            net: None,
            exec: Some(crate::obs::event::Exec {
                path: "/bin/true".into(),
                ..Default::default()
            }),
            file: None,
            api: None,
            policy: None,
            credential: None,
        };

        let (mut agent, host) = tokio::io::duplex(1 << 20);
        let (mut reader, mut writer) = tokio::io::split(host);
        write_frame(&mut agent, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        let payload = serde_json::to_vec(&event).unwrap();
        for _ in 0..3 {
            write_frame(&mut agent, &payload).await.unwrap();
        }
        // Half-closed, not dropped: an agent that goes away ends the stream
        // the collector is reading, but dropping the whole duplex would also
        // break the half the collector writes its acknowledgement to, and the
        // handshake would fail before there was any stream to end.
        {
            use tokio::io::AsyncWriteExt;
            agent.shutdown().await.unwrap();
        }

        let (tx, rx) = mpsc::channel::<Queued>(QUEUE_DEPTH);
        let mut store_writer = tokio::task::JoinSet::new();
        store_writer.spawn(Arc::clone(&collector).write_loop(rx));
        collector
            .serve_agent(&mut reader, &mut writer, tx, None, false)
            .await
            .unwrap();
        while store_writer.join_next().await.is_some() {}

        let connected = log.line("agent connected");
        assert!(
            connected.contains("pid=4242"),
            "the opening edge does not name the agent process: {connected}"
        );
        let ended = log.line("observability agent stream ended");
        assert!(
            ended.contains("pid=4242") && ended.contains("box_id=alpha"),
            "the closing edge does not name the agent process: {ended}"
        );
        assert!(
            ended.contains("events=3"),
            "the closing edge miscounts what the connection carried: {ended}"
        );
        assert!(
            ended.contains("ok=true"),
            "a clean end is reported as a failure: {ended}"
        );
    }

    #[test]
    fn stats_snapshot_reads_every_counter() {
        let stats = Stats::default();
        stats.received.store(10, Ordering::Relaxed);
        stats.dropped.store(2, Ordering::Relaxed);
        stats.persist_failed.store(3, Ordering::Relaxed);

        let snap = stats.snapshot();
        assert_eq!(snap.received, 10);
        assert_eq!(snap.dropped, 2);
        // Distinct from `dropped`: a full queue and a store that will not take
        // the batch are different failures needing different responses, and
        // one counter for both would have reported a full disk as backpressure.
        assert_eq!(snap.persist_failed, 3);
        assert_eq!(snap.stored, 0);
    }

    #[tokio::test]
    async fn an_accepted_agent_reports_its_capture_backends_to_the_hook() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<Hello>>::new()));
        let sink = seen.clone();
        let collector = Arc::new(
            Collector::new(
                std::path::PathBuf::from("/unused"),
                Store::open_in_memory().unwrap(),
            )
            .for_box("alpha")
            .with_agent_hook(move |_, hello| sink.lock().unwrap().push(hello.cloned())),
        );

        let (mut agent, host) = tokio::io::duplex(4096);
        let (host_reader, host_writer) = tokio::io::split(host);
        let served = tokio::spawn(collector.run_agent_stream(host_reader, host_writer));

        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            box_id: "alpha".into(),
            capture: vec!["ebpf".into(), "packet".into()],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: true,
            pid: 0,
        };
        write_frame(&mut agent, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        // The ack proves the handshake completed, so the hook has run.
        let ack: HelloAck =
            serde_json::from_slice(&read_frame(&mut agent).await.unwrap().unwrap()).unwrap();
        assert!(ack.accepted, "{}", ack.reason);
        drop(agent);
        let _ = served.await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one arrival and one departure");
        let arrived = seen[0].as_ref().expect("the arrival carries the hello");
        assert!(
            arrived.ebpf,
            "the console cannot infer this from anywhere else"
        );
        assert_eq!(arrived.capture, vec!["ebpf", "packet"]);
        // The closing edge: without it a host-side socket listener would keep
        // reporting a stopped box as capturing.
        assert!(seen[1].is_none(), "the departure is reported too");
    }

    #[tokio::test]
    async fn a_rejected_agent_is_never_reported_as_capturing() {
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let sink = seen.clone();
        let collector = Arc::new(
            Collector::new(
                std::path::PathBuf::from("/unused"),
                Store::open_in_memory().unwrap(),
            )
            .for_box("alpha")
            .with_agent_hook(move |_, hello| {
                if hello.is_some() {
                    *sink.lock().unwrap() += 1;
                }
            }),
        );

        let (mut agent, host) = tokio::io::duplex(4096);
        let (host_reader, host_writer) = tokio::io::split(host);
        let served = tokio::spawn(collector.run_agent_stream(host_reader, host_writer));

        let hello = Hello {
            protocol: PROTOCOL_VERSION + 1,
            version: env!("CARGO_PKG_VERSION").to_string(),
            box_id: "alpha".into(),
            capture: vec![],
            source: String::new(),
            file_scope: Vec::new(),
            ebpf: false,
            pid: 0,
        };
        write_frame(&mut agent, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        let _ = read_frame(&mut agent).await;
        drop(agent);
        let _ = served.await;

        assert_eq!(*seen.lock().unwrap(), 0, "a refused agent captures nothing");
    }
}
