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
    pub rejected: u64,
    pub agents_connected: u64,
}

/// Read one length-prefixed frame.
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        // A clean EOF on a frame boundary means the agent hung up normally.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("failed to read a frame header"),
    }

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

    HelloAck {
        protocol: PROTOCOL_VERSION,
        accepted: true,
        reason: String::new(),
    }
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
                me.stats.agents_connected.fetch_add(1, Ordering::Relaxed);
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

        loop {
            let got = tokio::time::timeout(BATCH_LINGER, rx.recv()).await;
            match got {
                Ok(Some(event)) => {
                    // Publish live before storing: the console should not wait
                    // on a disk write to show what just happened.
                    let _ = self.live.send(event.clone());
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
            Err(e) => tracing::error!(error = %e, "failed to store an event batch"),
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
            version: "0.1.3".into(),
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

        let snap = stats.snapshot();
        assert_eq!(snap.received, 10);
        assert_eq!(snap.dropped, 2);
        assert_eq!(snap.stored, 0);
    }
}
