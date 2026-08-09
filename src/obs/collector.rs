//! Collector — the host side of the agent protocol (§11.3).
//!
//! Listens on a unix socket, performs the version handshake, then reads
//! length-prefixed frames of events, batches them into the store, and fans
//! them out to the console's live stream.
//!
//! Framing matches `agent/transport`: a 4-byte big-endian length followed by
//! that many bytes of payload. The payload is JSON today and versioned so it
//! can become protobuf without changing the framer (ADR-0015).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use super::event::Event;
use super::store::{Retention, Store};

/// Protocol version, matching `transport.ProtocolVersion` in Go.
pub const PROTOCOL_VERSION: u32 = 1;

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

/// The agent's opening frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    #[serde(default)]
    pub version: String,
    pub box_id: String,
    #[serde(default)]
    pub capture: Vec<String>,
    #[serde(default)]
    pub ebpf: bool,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
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
    version.is_empty() || version == "0.0.0" || version.contains("-dev")
}

/// A running collector.
pub struct Collector {
    socket_path: PathBuf,
    store: Arc<Mutex<Store>>,
    stats: Arc<Stats>,
    /// Every stored event is republished here for the console's live view.
    live: tokio::sync::broadcast::Sender<Event>,
    box_id: Option<String>,
    retention: Retention,
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
        }
    }

    /// Only accept agents claiming this box.
    pub fn for_box(mut self, box_id: impl Into<String>) -> Self {
        self.box_id = Some(box_id.into());
        self
    }

    /// Override the retention policy.
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
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
        }
        // A previous run that was killed leaves the socket file behind; bind
        // would then fail with EADDRINUSE even though nothing is listening.
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .with_context(|| format!("failed to remove {}", self.socket_path.display()))?;
        }
        UnixListener::bind(&self.socket_path)
            .with_context(|| format!("failed to listen on {}", self.socket_path.display()))
    }

    /// Accept agents until the listener fails.
    pub async fn run(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        let (tx, rx) = mpsc::channel::<Event>(QUEUE_DEPTH);
        let writer = tokio::spawn(Arc::clone(&self).write_loop(rx));

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(error = %e, "collector accept failed");
                    break;
                }
            };

            let me = Arc::clone(&self);
            let tx = tx.clone();
            tokio::spawn(async move {
                // Counted inside `serve_agent`, once the hello is accepted.
                // Incrementing here counted malformed clients and agents
                // rejected for the wrong box, so a retry loop inflated the
                // metric without a single agent ever connecting.
                if let Err(e) = me.serve_agent(stream, tx).await {
                    tracing::warn!(error = %e, "agent connection ended");
                }
            });
        }

        drop(tx);
        let _ = writer.await;
        Ok(())
    }

    /// Handshake with one agent, then read its event stream.
    async fn serve_agent(&self, mut stream: UnixStream, tx: mpsc::Sender<Event>) -> Result<()> {
        let Some(frame) = read_frame(&mut stream).await? else {
            bail!("agent closed the connection before saying hello");
        };
        let hello: Hello =
            serde_json::from_slice(&frame).context("agent sent a malformed hello")?;

        let ack = evaluate_hello(&hello, self.box_id.as_deref());
        write_frame(&mut stream, &serde_json::to_vec(&ack)?).await?;
        if !ack.accepted {
            bail!("rejected agent: {}", ack.reason);
        }
        // Only now: an accepted agent is what the metric claims to count.
        self.stats.agents_connected.fetch_add(1, Ordering::Relaxed);

        tracing::info!(
            box_id = %hello.box_id,
            version = %hello.version,
            ebpf = hello.ebpf,
            "agent connected"
        );

        let box_id = hello.box_id.clone();
        while let Some(frame) = read_frame(&mut stream).await? {
            if frame.is_empty() {
                continue; // keepalive
            }
            self.stats.received.fetch_add(1, Ordering::Relaxed);

            let event: Event = match serde_json::from_slice(&frame) {
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
            // The handshake decided which box this connection speaks for.
            // Accepting an event that names a different one would let a
            // compromised agent write into another box's timeline.
            if event.box_id != box_id {
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    expected = %box_id,
                    claimed = %event.box_id,
                    "agent sent an event for another box"
                );
                continue;
            }

            // Never block the socket reader: a full queue means the store
            // cannot keep up, and the honest response is to drop and count.
            if tx.try_send(event).is_err() {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Drain the queue into the store in batches.
    async fn write_loop(self: Arc<Self>, mut rx: mpsc::Receiver<Event>) {
        let mut batch: Vec<Event> = Vec::with_capacity(BATCH_SIZE);
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
                Ok(Some(event)) => {
                    // Publish live before storing: the console should not wait
                    // on a disk write to show what just happened.
                    let _ = self.live.send(event.clone());
                    if batch.is_empty() {
                        deadline = Some(tokio::time::Instant::now() + BATCH_LINGER);
                    }
                    batch.push(event);
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
                    return;
                }
            }
            self.flush(&mut batch).await;
            // The next batch starts its own clock when its first event lands.
            deadline = None;
        }
    }

    async fn flush(&self, batch: &mut Vec<Event>) {
        if batch.is_empty() {
            return;
        }
        let mut store = self.store.lock().await;
        match store.insert_batch(batch) {
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

/// Default socket path for a box's agent.
pub fn socket_path(state_dir: &Path, box_id: &str) -> PathBuf {
    state_dir.join("boxes").join(box_id).join("obsd.sock")
}

/// Default event-database path for a box.
pub fn store_path(state_dir: &Path, box_id: &str) -> PathBuf {
    state_dir.join("boxes").join(box_id).join("events.db")
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
            ebpf: true,
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
    fn paths_are_scoped_per_box() {
        let dir = Path::new("/home/x/.devbox");
        assert_eq!(
            socket_path(dir, "myapp"),
            PathBuf::from("/home/x/.devbox/boxes/myapp/obsd.sock")
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
            ebpf: true,
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
            ebpf: true,
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
            ebpf: false,
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
}
