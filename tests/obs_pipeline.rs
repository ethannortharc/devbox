//! Cross-language integration test: the **real Go agent** streaming into the
//! **real Rust collector**.
//!
//! Everything between the two is exercised for real — the handshake, the
//! length-prefixed framing, the JSON schema, the SQLite store, and the
//! correlation pass. Nothing is stubbed except the capture source, which
//! replays a recorded fixture so the test is deterministic (the fixture is the
//! same file both languages' unit tests read).
//!
//! This is Phase 3's acceptance criterion: "run a known command in a box,
//! assert exec+dns+connect+file events captured and queryable … integration
//! test generating known activity asserts the correlated chain".
//!
//! Skips if the Go toolchain is absent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use devbox::obs::behavior;
use devbox::obs::collector::{Collector, socket_path};
use devbox::obs::correlate;
use devbox::obs::event::EventType;
use devbox::obs::store::{Query, Store};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build `devbox-obsd` for the host, returning its path.
fn build_agent(out_dir: &Path) -> Option<PathBuf> {
    let binary = out_dir.join("devbox-obsd");
    let status = Command::new("go")
        .args(["build", "-o"])
        .arg(&binary)
        .arg("./agent/cmd/obsd")
        .current_dir(repo_root())
        .status()
        .ok()?;
    status.success().then_some(binary)
}

#[tokio::test(flavor = "multi_thread")]
async fn go_agent_streams_into_the_rust_collector() {
    if !go_available() {
        eprintln!("skipping: the Go toolchain is not available");
        return;
    }

    let work = tempfile::tempdir().expect("temp dir");
    let Some(agent) = build_agent(work.path()) else {
        eprintln!("skipping: could not build devbox-obsd");
        return;
    };

    // ── collector ────────────────────────────────────────
    let state_dir = work.path().join("state");
    let sock = socket_path(&state_dir, "myapp");
    let store = Store::open(&state_dir.join("boxes/myapp/events.db")).expect("store opens");

    let collector = Arc::new(Collector::new(sock.clone(), store).for_box("myapp"));
    let listener = collector.bind().expect("collector binds");
    let stats = collector.stats();
    let store_handle = collector.store();
    let mut live = collector.subscribe();

    let serving = tokio::spawn(Arc::clone(&collector).run(listener));

    // ── agent ────────────────────────────────────────────
    let fixture = repo_root().join("agent/event/testdata/events.jsonl");
    let output = tokio::process::Command::new(&agent)
        .args(["-box-id", "myapp", "-socket"])
        .arg(&sock)
        .arg("-fixture")
        .arg(&fixture)
        // The test waits for the process, so it wants the flag whose help
        // text says "exit when the source finishes".
        .arg("-once")
        .output()
        .await
        .expect("agent runs");

    assert!(
        output.status.success(),
        "agent failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("connected to"),
        "agent should announce the handshake: {stdout}"
    );

    // ── everything arrived ───────────────────────────────
    let expected = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    // The collector batches with a linger, so give it a moment to flush.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stored = store_handle.lock().await.count().unwrap();
        if stored as usize >= expected || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let store = store_handle.lock().await;
    assert_eq!(
        store.count().unwrap() as usize,
        expected,
        "every fixture event should have been stored"
    );

    let snap = stats.snapshot();
    assert_eq!(snap.agents_connected, 1);
    assert_eq!(snap.received as usize, expected);
    assert_eq!(snap.dropped, 0, "nothing should have been dropped");
    assert_eq!(snap.rejected, 0, "nothing should have been rejected");

    // ── queryable, per the acceptance criteria ───────────
    for kind in [
        EventType::Exec,
        EventType::Dns,
        EventType::Connect,
        EventType::File,
    ] {
        let got = store
            .query(&Query {
                kinds: vec![kind],
                ..Default::default()
            })
            .unwrap();
        assert!(!got.is_empty(), "no {kind} events were captured");
    }

    let by_pid = store
        .query(&Query {
            pid: Some(812),
            ..Default::default()
        })
        .unwrap();
    assert!(by_pid.len() >= 5, "pid filter returned {}", by_pid.len());

    let by_peer = store
        .query(&Query {
            peer: Some("pypi".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(!by_peer.is_empty(), "peer filter found nothing");

    let by_path = store
        .query(&Query {
            path: Some("site-packages".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(!by_path.is_empty(), "path filter found nothing");

    // ── the correlated chain ─────────────────────────────
    let all = store
        .query(&Query {
            limit: Some(1000),
            ..Default::default()
        })
        .unwrap();

    let dns_map = correlate::dns_map(&all);
    assert_eq!(
        dns_map.get("151.101.0.223").map(String::as_str),
        Some("pypi.org"),
        "the DNS answer should explain the address that was connected to"
    );

    let chains = correlate::chains(&all);
    let pip = chains
        .iter()
        .find(|c| c.pid == 812)
        .expect("the pip process should have a chain");

    assert_eq!(
        pip.command.as_deref(),
        Some("python3.12 -m pip install requests"),
        "the chain should name the command that started it"
    );
    assert!(
        pip.peers.iter().any(|p| p == "pypi.org"),
        "the chain should list what it contacted: {:?}",
        pip.peers
    );
    assert!(
        pip.files.iter().any(|f| f.contains("requests/__init__.py")),
        "the chain should list what it wrote: {:?}",
        pip.files
    );
    // exec → dns → connect → tls → file → exit, all on one pid.
    assert!(
        pip.of_kind(EventType::Exec).count() == 1
            && pip.of_kind(EventType::Dns).count() == 1
            && pip.of_kind(EventType::Connect).count() == 1
            && pip.of_kind(EventType::File).count() == 1,
        "the chain should hold the whole story: {:?}",
        pip.events.iter().map(|e| e.kind).collect::<Vec<_>>()
    );

    let (tx, rx) = pip.bytes();
    assert_eq!(tx, 4102);
    assert_eq!(rx, 831_720);

    // ── the behaviour summary matches the run (§7.6) ─────
    let summary = behavior::summarize("myapp", &all);
    assert!(
        summary.domains.contains("pypi.org"),
        "domains: {:?}",
        summary.domains
    );
    assert!(
        summary.processes.contains("python3.12"),
        "the basename, not the /nix/store path: {:?}",
        summary.processes
    );
    assert!(summary.dns_queries.contains("pypi.org"));
    assert_eq!(summary.egress_mode.as_deref(), Some("allowlist"));
    assert_eq!(summary.violations.len(), 1);
    assert_eq!(summary.violations[0].target, "telemetry.example.com");
    assert_eq!(summary.api_calls.len(), 1);
    assert_eq!(summary.api_calls[0].host, "api.anthropic.com");
    assert_eq!(summary.api_calls[0].tokens, 142_000);

    let md = behavior::render_markdown(&summary);
    assert!(md.contains("Behavior summary"));
    assert!(md.contains("telemetry.example.com"));

    // Diffing a run against itself finds nothing; against an empty window it
    // finds everything that left, but no *new* behaviour.
    let self_diff = behavior::diff(&summary, &summary);
    assert!(self_diff.is_empty());

    let from_nothing = behavior::diff(&behavior::Summary::default(), &summary);
    assert!(from_nothing.has_new_behavior());
    assert!(from_nothing.new_domains.contains(&"pypi.org".to_string()));

    // ── the live stream saw them too ─────────────────────
    let mut live_seen = 0;
    while live.try_recv().is_ok() {
        live_seen += 1;
    }
    assert!(
        live_seen > 0,
        "events should also have been published to the live channel"
    );

    drop(store);
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_collector_rejects_an_agent_claiming_another_box() {
    if !go_available() {
        eprintln!("skipping: the Go toolchain is not available");
        return;
    }

    let work = tempfile::tempdir().expect("temp dir");
    let Some(agent) = build_agent(work.path()) else {
        eprintln!("skipping: could not build devbox-obsd");
        return;
    };

    let state_dir = work.path().join("state");
    let sock = socket_path(&state_dir, "myapp");
    let store = Store::open_in_memory().unwrap();

    let collector = Arc::new(Collector::new(sock.clone(), store).for_box("myapp"));
    let listener = collector.bind().unwrap();
    let store_handle = collector.store();
    let serving = tokio::spawn(Arc::clone(&collector).run(listener));

    let output = tokio::process::Command::new(&agent)
        .args(["-box-id", "someone-elses-box", "-socket"])
        .arg(&sock)
        .arg("-fixture")
        .arg(repo_root().join("agent/event/testdata/events.jsonl"))
        .arg("-once")
        .output()
        .await
        .expect("agent runs");

    assert!(
        !output.status.success(),
        "the agent should have been refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("myapp"),
        "the rejection reason should reach the agent: {stderr}"
    );
    assert_eq!(
        store_handle.lock().await.count().unwrap(),
        0,
        "a rejected agent must not get any events stored"
    );

    serving.abort();
}
