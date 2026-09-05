//! Run evidence, end to end on the host side (§4).
//!
//! A fixed set of events goes into a store, a run claims them, and the three
//! renderings are held to what they must contain. Golden in the sense that
//! matters: not a byte-for-byte snapshot that a wording change breaks, but the
//! facts a report is *for* — the file that changed, the host that was reached,
//! the process that reached it, and how much of that the capture layer saw.
//!
//! Also here because it needs a real SQLite file rather than an in-memory one:
//! the v4 → v5 migration, which is the one path where getting it wrong loses
//! somebody's history rather than a test.

use devbox::obs::event::{Event, EventType, Exec, File, Net, Policy};
use devbox::obs::run::{
    ActiveRun, Attribution, Attributor, EndedBy, RunKind, RunRecord, RunStatus, is_run_id,
    new_run_id,
};
use devbox::obs::store::{Query, Store};
use devbox::report::model::{RunReport, SCOPE_BOX};
use devbox::report::{html, json, markdown};
use devbox::sandbox::overlay::{ChangeStatus, OverlayChange};

const BOX: &str = "devtest";
const RUN: &str = "01K4SZ0000000000000000ABCD";
const CGROUP: u64 = 17557;
const ROOT_PID: u32 = 900;

fn base(kind: EventType, pid: u32, ppid: u32, cgroup: u64, ts: &str) -> Event {
    Event {
        ts_wall: ts.to_string(),
        ts_mono_ns: ts.len() as u64,
        box_id: BOX.to_string(),
        cgroup_id: cgroup,
        pid,
        tid: pid,
        ppid,
        comm: "sh".into(),
        uid: 1000,
        kind,
        net: None,
        exec: None,
        file: None,
        api: None,
        policy: None,
        credential: None,
    }
}

/// The fixture: a shell that curls a host, writes a file, and gets one
/// connection refused by policy on the way.
fn fixture() -> Vec<Event> {
    let mut wrapper = base(
        EventType::Exec,
        ROOT_PID,
        1,
        CGROUP,
        "2026-09-05T10:00:00.000Z",
    );
    wrapper.comm = "sh".into();
    wrapper.exec = Some(Exec {
        path: "/bin/sh".into(),
        argv: vec!["sh".into(), "-c".into(), "curl …".into()],
        cwd: "/workspace".into(),
    });

    let mut curl = base(
        EventType::Exec,
        901,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.100Z",
    );
    curl.comm = "curl".into();
    curl.exec = Some(Exec {
        path: "/run/current-system/sw/bin/curl".into(),
        argv: vec!["curl".into(), "-s".into(), "https://example.com".into()],
        cwd: "/workspace".into(),
    });

    let mut dns = base(
        EventType::Dns,
        901,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.200Z",
    );
    dns.comm = "curl".into();
    dns.net = Some(Net {
        proto: "udp".into(),
        qname: "example.com".into(),
        qtype: "A".into(),
        answers: vec!["93.184.216.34".into()],
        response: true,
        ..Default::default()
    });

    let mut connect = base(
        EventType::Connect,
        901,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.300Z",
    );
    connect.comm = "curl".into();
    connect.net = Some(Net {
        proto: "tcp".into(),
        daddr: "93.184.216.34".into(),
        dport: 443,
        ..Default::default()
    });

    let mut tls = base(
        EventType::Tls,
        901,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.310Z",
    );
    tls.comm = "curl".into();
    tls.net = Some(Net {
        proto: "tcp".into(),
        daddr: "93.184.216.34".into(),
        dport: 443,
        sni: "example.com".into(),
        alpn: "h2".into(),
        ..Default::default()
    });

    // The same connection, settled. Traffic lives here and nowhere else: a
    // `connect` fires before any payload has crossed the socket.
    let mut close = base(
        EventType::Close,
        901,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.350Z",
    );
    close.comm = "curl".into();
    close.net = Some(Net {
        proto: "tcp".into(),
        daddr: "93.184.216.34".into(),
        dport: 443,
        bytes_tx: 812,
        bytes_rx: 4096,
        dur_ms: 690,
        dir: "out".into(),
        ..Default::default()
    });

    let mut write = base(
        EventType::File,
        ROOT_PID,
        1,
        CGROUP,
        "2026-09-05T10:00:00.400Z",
    );
    write.file = Some(File {
        path: "/workspace/run-a.txt".into(),
        op: "write".into(),
        flags: 577,
    });

    // Packet-derived: no process at all. Only the window rule can place it.
    let mut refused = base(
        EventType::Policy,
        u32::MAX,
        0,
        0,
        "2026-09-05T10:00:00.500Z",
    );
    refused.comm = "netfilter".into();
    refused.net = Some(Net {
        proto: "tcp".into(),
        daddr: "10.1.2.3".into(),
        dport: 22,
        ..Default::default()
    });
    refused.policy = Some(Policy {
        verdict: "block".into(),
        mode: "allowlist".into(),
        target: "10.1.2.3:22".into(),
        reason: "not in allowlist".into(),
    });

    // A process on the box that has nothing to do with this run.
    let mut stranger = base(EventType::Exec, 4242, 1, 5632, "2026-09-05T10:00:00.250Z");
    stranger.comm = "sshd".into();
    stranger.exec = Some(Exec {
        path: "/usr/sbin/sshd".into(),
        argv: vec!["sshd".into()],
        ..Default::default()
    });

    vec![
        wrapper, curl, dns, connect, tls, close, write, refused, stranger,
    ]
}

fn record() -> RunRecord {
    RunRecord {
        run_id: RUN.to_string(),
        box_id: BOX.to_string(),
        kind: RunKind::Run.as_str().to_string(),
        argv: vec![
            "sh".into(),
            "-c".into(),
            "curl -s https://example.com; echo hi > /workspace/run-a.txt".into(),
        ],
        cwd: "/workspace".into(),
        label: "smoke".into(),
        started_at: "2026-09-05T10:00:00.000Z".into(),
        ended_at: Some("2026-09-05T10:00:01.500Z".into()),
        exit_code: Some(0),
        status: RunStatus::Finished.as_str().to_string(),
        posture_before: "open".into(),
        posture_during: "allowlist".into(),
        cgroup_id: CGROUP,
        root_pid: ROOT_PID,
        capture_sources: "ebpf+packet+netfilter".into(),
        agent_version: "0.1.6".into(),
        dropped_events: 0,
        ..Default::default()
    }
}

/// A store with the fixture written through the same attribution the collector
/// applies, so the test exercises the rule rather than restating its answer.
fn loaded() -> Store {
    let mut store = Store::open_in_memory().expect("in-memory store");
    let run = record();
    store.insert_run(&run).expect("insert run");

    let mut attributor = Attributor::new();
    attributor.set_active(vec![ActiveRun {
        run_id: RUN.to_string(),
        cgroup_id: CGROUP,
        root_pid: ROOT_PID,
        started_at: run.started_at.clone(),
        ended_at: None,
    }]);

    let events = fixture();
    let tags: Vec<_> = events.iter().map(|e| attributor.attribute(e)).collect();
    store
        .insert_batch_tagged(&events, &tags)
        .expect("insert events");
    store
        .finish_run(
            RUN,
            run.ended_at.as_deref().unwrap(),
            Some(0),
            RunStatus::Finished,
            &run.capture_sources,
            &run.agent_version,
            0,
            Some(EndedBy::Exit),
        )
        .expect("finish run");
    store
}

fn built(store: &Store) -> RunReport {
    let run = store.get_run(RUN).unwrap().expect("the run");
    let events = store
        .query(&Query {
            run_id: Some(RUN.to_string()),
            limit: Some(Query::MAX_LIMIT),
            ..Default::default()
        })
        .expect("run-scoped query");
    let attribution = store.attribution_counts(RUN).unwrap();
    let unattributed = store
        .unattributed_in_window(&run.started_at, run.ended_at.as_deref())
        .unwrap();
    let changes = vec![
        OverlayChange {
            status: ChangeStatus::Added,
            path: "/workspace/run-a.txt".into(),
            is_dir: false,
        },
        OverlayChange {
            status: ChangeStatus::Added,
            path: "/workspace/sub".into(),
            is_dir: true,
        },
    ];
    RunReport::build(
        run,
        &events,
        Box::new(move || Ok(changes.clone())),
        SCOPE_BOX,
        attribution,
        unattributed,
    )
}

#[test]
fn a_run_claims_its_own_events_and_leaves_the_rest_alone() {
    let store = loaded();
    let mine = store
        .query(&Query {
            run_id: Some(RUN.to_string()),
            limit: Some(Query::MAX_LIMIT),
            ..Default::default()
        })
        .unwrap();
    // Eight of the nine: the stranger's exec belongs to no run, and so does
    // nothing else on the box.
    assert_eq!(
        mine.len(),
        8,
        "{:#?}",
        mine.iter().map(|e| (&e.comm, e.pid)).collect::<Vec<_>>()
    );
    assert!(
        !mine.iter().any(|e| e.comm == "sshd"),
        "an unrelated process was swept into the run"
    );

    let counts: std::collections::BTreeMap<_, _> =
        store.attribution_counts(RUN).unwrap().into_iter().collect();
    // The wrapper's own exec has the run's cgroup, so it is a cgroup match;
    // so is everything the wrapper spawned inside it.
    assert_eq!(counts.get(&Attribution::Cgroup), Some(&7));
    // The pid-less refusal can only be a window decision.
    assert_eq!(counts.get(&Attribution::Window), Some(&1));

    // And the stranger is visible as such, rather than quietly missing.
    assert_eq!(
        store
            .unattributed_in_window("2026-09-05T10:00:00.000Z", Some("2026-09-05T10:00:01.500Z"))
            .unwrap(),
        1
    );
}

#[test]
fn the_markdown_report_states_every_fact_it_was_built_from() {
    let report = markdown::render(&built(&loaded()));

    for expected in [
        // header
        "# Run 01K4SZ0000000000000000ABCD",
        "**smoke**",
        "- box: `devtest`",
        "- exit: 0",
        "- status: finished",
        "- posture: allowlist (restored to open after)",
        // coverage — the badge and the arithmetic behind it
        "`full` — ebpf+packet+netfilter",
        "| agent | 0.1.6 |",
        "| … by cgroup | 7 |",
        "| … by window | 1 |",
        "| unattributed in the window | 1 |",
        // files, with the caveat that wave 1 owes the reader
        "_scope: box (not run-scoped until checkpoints land)_",
        "| + | `/workspace/run-a.txt` |",
        "(and 1 directories)",
        // network
        "| `example.com` | 1 | 443 | yes | 812B | 4.0KB | 690ms |",
        "TLS server names: `example.com`",
        "DNS: `example.com`",
        // processes, as a tree
        "curl [901] curl -s https://example.com",
        // policy
        "| `10.1.2.3:22` | block | not in allowlist |",
        // The broker is wired now, so an empty section is a fact about the
        // run rather than a fact about the build.
        "No credential use recorded.",
    ] {
        assert!(
            report.contains(expected),
            "the report does not say {expected:?}\n---\n{report}"
        );
    }
}

#[test]
fn traffic_is_counted_from_close_and_only_from_close() {
    // The regression this guards is double counting. `connect` fires from a
    // probe that runs before any payload has crossed the socket, so its
    // counters are zero today — but the day a source starts filling both ends,
    // a report that summed them would quietly double every number in it.
    let store = loaded();
    let report = built(&store);
    let row = &report.network.domains[0];
    assert_eq!(row.peer, "example.com");
    assert_eq!((row.bytes_tx, row.bytes_rx), (812, 4096));
    assert_eq!(row.dur_ms, 690);
    assert_eq!(row.connections, 1, "one connect");
    assert_eq!(row.closes, 1, "one close");
    assert_eq!(row.conns_human(), "1", "matched pairs stay quiet");
    // The section total agrees with the row, and with `behavior::summarize`.
    assert_eq!(
        (report.network.bytes_tx, report.network.bytes_rx),
        (812, 4096)
    );

    // Now the same connection with counters on *both* events, which is what a
    // future source might send. The close still decides.
    let mut doubled = fixture();
    for event in &mut doubled {
        if event.kind == EventType::Connect
            && let Some(net) = &mut event.net
        {
            net.bytes_tx = 812;
            net.bytes_rx = 4096;
        }
    }
    let report = RunReport::build(
        record(),
        &doubled,
        Box::new(|| Ok(Vec::new())),
        SCOPE_BOX,
        Vec::new(),
        0,
    );
    assert_eq!(
        (
            report.network.domains[0].bytes_tx,
            report.network.domains[0].bytes_rx
        ),
        (812, 4096),
        "the connect's counters must not be added to the close's"
    );
}

#[test]
fn a_connection_the_window_did_not_see_start_is_still_reported() {
    // A run that inherits an open socket sees the close and never the connect.
    // Dropping it would lose the only record of that traffic; counting it as
    // an ordinary connection would claim the run opened something it did not.
    let only_close: Vec<_> = fixture()
        .into_iter()
        .filter(|e| e.kind != EventType::Connect && e.kind != EventType::Accept)
        .collect();
    let report = RunReport::build(
        record(),
        &only_close,
        Box::new(|| Ok(Vec::new())),
        SCOPE_BOX,
        Vec::new(),
        0,
    );
    let row = report
        .network
        .domains
        .iter()
        .find(|d| d.peer == "example.com")
        .expect("the peer survives with no connect");
    assert_eq!(row.connections, 0);
    assert_eq!(row.closes, 1);
    assert_eq!(row.bytes_rx, 4096);
    assert_eq!(row.conns_human(), "0 (+1 closed)", "and it says so");
}

#[test]
fn the_renderers_show_what_the_overlay_and_the_broker_saw() {
    use devbox::obs::event::{Credential, File as FileDetail};

    let mut events = Vec::new();
    // A run that installed something: nothing in the workspace, a lot in a
    // cache the overlay does not carry. Before this section such a run read
    // "No file changes", which is true of the workspace and false of the box.
    for i in 0..12 {
        let mut write = base(
            EventType::File,
            ROOT_PID,
            1,
            CGROUP,
            "2026-09-05T10:00:00.400Z",
        );
        write.file = Some(FileDetail {
            path: format!("/home/dev/.cache/uv/wheel-{i}.whl"),
            op: "write".into(),
            flags: 0,
        });
        events.push(write);
    }
    // The wrapper's own exec, which is where `$HOME` comes from and which the
    // process tree must fold away.
    let mut wrapper = base(
        EventType::Exec,
        ROOT_PID,
        1,
        CGROUP,
        "2026-09-05T10:00:00.000Z",
    );
    wrapper.exec = Some(Exec {
        path: "/bin/sh".into(),
        argv: vec![
            "/bin/sh".into(),
            format!("/tmp/.devbox-run-{RUN}.sh"),
            "--devbox-scoped".into(),
            "systemd-user".into(),
            RUN.into(),
            format!("/run/devbox/runs/{RUN}.json"),
            "/workspace".into(),
            "/home/dev".into(),
            "dev".into(),
            "uv".into(),
            "sync".into(),
        ],
        cwd: "/workspace".into(),
    });
    events.push(wrapper);

    let mut user = base(
        EventType::Exec,
        950,
        ROOT_PID,
        CGROUP,
        "2026-09-05T10:00:00.100Z",
    );
    user.comm = "uv".into();
    user.exec = Some(Exec {
        path: "/bin/uv".into(),
        argv: vec!["uv".into(), "sync".into()],
        cwd: "/workspace".into(),
    });
    events.push(user);

    // Two broker requests, one refused. These have no pid at all.
    for (verdict, ts) in [
        ("allowed", "2026-09-05T10:00:00.600Z"),
        ("denied", "2026-09-05T10:00:00.700Z"),
    ] {
        let mut credential = base(EventType::Credential, u32::MAX, 0, 0, ts);
        credential.comm = "broker".into();
        credential.credential = Some(Credential {
            provider: "anthropic".into(),
            method: "POST".into(),
            host: "api.anthropic.com".into(),
            path: "/v1/messages".into(),
            status: 200,
            verdict: verdict.into(),
            ..Default::default()
        });
        events.push(credential);
    }

    let report = RunReport::build(
        record(),
        &events,
        Box::new(|| Ok(Vec::new())),
        SCOPE_BOX,
        Vec::new(),
        0,
    );
    let text = markdown::render(&report);

    for expected in [
        // The overlay saw nothing; the box saw twelve wheels.
        "No changes to the workspace overlay.",
        "### Writes outside the workspace overlay",
        "| `~/.cache/uv` | 12 | 12 |",
        "not part of what `devbox commit` would sync",
        // The broker's two requests, grouped, with the refusal called out.
        "| `anthropic` | `api.anthropic.com` | POST | 2 (1 denied) | 2026-09-05T10:00:00.700Z |",
        // And the wrapper is one line, with the user's command as the root.
        "devbox [900] [devbox wrapper]",
        "uv [950] uv sync",
    ] {
        assert!(
            text.contains(expected),
            "the report does not say {expected:?}\n---\n{text}"
        );
    }
    // The wrapper's argv must not survive anywhere in the rendered tree.
    let tree = text
        .split("## Processes")
        .nth(1)
        .and_then(|s| s.split("## Credentials").next())
        .expect("a process section");
    assert!(
        !tree.contains("--devbox-scoped"),
        "the wrapper's argv is still in the tree:\n{tree}"
    );

    // The HTML says the same things.
    let page = html::render(&report).unwrap();
    assert!(page.contains("~/.cache/uv"));
    assert!(page.contains("api.anthropic.com"));
    assert!(page.contains("[devbox wrapper]"));
    assert!(!page.contains("not wired in this build"));
}

#[test]
fn the_terminal_summary_fits_on_a_screen_and_still_says_the_verdict() {
    let summary = markdown::render_summary(&built(&loaded()));
    assert!(
        summary.lines().count() <= 8,
        "the summary printed after every run must stay short:\n{summary}"
    );
    assert!(summary.contains("exit 0"));
    assert!(summary.contains("scope: box (not run-scoped until checkpoints land)"));
    assert!(summary.contains("coverage full"));
}

#[test]
fn the_json_report_round_trips_through_its_own_renderer() {
    let report = built(&loaded());
    let encoded = json::render(&report).unwrap();
    let decoded = json::parse(&encoded).unwrap();
    // The JSON is the contract, so re-rendering from it must be identical —
    // that is what makes `devbox report` work after the events have aged out.
    assert_eq!(markdown::render(&report), markdown::render(&decoded));

    let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(value["run"]["run_id"], RUN);
    assert_eq!(value["coverage"]["attribution"]["cgroup"], 7);
    assert_eq!(value["network"]["domains"][0]["peer"], "example.com");
    assert_eq!(value["files"]["scope"], SCOPE_BOX);
}

#[test]
fn the_html_report_is_one_file_with_the_model_inside_it() {
    let report = built(&loaded());
    let page = html::render(&report).unwrap();

    for forbidden in ["/assets/", "src=\"http", "href=\"http", "@import"] {
        assert!(
            !page.contains(forbidden),
            "a report has to open from a file:// URL, so it cannot reference {forbidden}"
        );
    }
    assert!(page.contains("example.com"));
    assert!(page.contains("/workspace/run-a.txt"));
    assert!(page.contains("coverage: full"));

    let embedded = page
        .split(r#"<script type="application/json" id="report">"#)
        .nth(1)
        .and_then(|rest| rest.split_once("</script>"))
        .map(|(body, _)| body.replace(r"<\/", "</"))
        .expect("the embedded model");
    let value: serde_json::Value = serde_json::from_str(&embedded).expect("valid JSON");
    assert_eq!(value["run"]["run_id"], RUN);
}

#[test]
fn a_run_with_nothing_captured_reports_that_rather_than_reporting_nothing() {
    // The failure this guards: a box whose agent never connected renders an
    // empty report, which reads exactly like a command that did nothing.
    let store = Store::open_in_memory().unwrap();
    let mut run = record();
    run.capture_sources = String::new();
    run.agent_version = String::new();
    store.insert_run(&run).unwrap();
    store
        .finish_run(
            RUN,
            "2026-09-05T10:00:01.500Z",
            Some(0),
            RunStatus::Finished,
            "",
            "",
            0,
            Some(EndedBy::Exit),
        )
        .unwrap();

    let report = RunReport::build(
        store.get_run(RUN).unwrap().unwrap(),
        &[],
        Box::new(|| Ok(Vec::new())),
        SCOPE_BOX,
        Vec::new(),
        0,
    );
    assert_eq!(report.coverage.badge(), "unknown");
    let text = markdown::render(&report);
    assert!(text.contains("no capture source recorded"), "{text}");
    assert!(html::render(&report).unwrap().contains("coverage: unknown"));
}

#[test]
fn a_v4_store_gains_the_run_columns_without_losing_a_row() {
    // The real shape of the risk: a database somebody has been collecting into
    // for weeks, opened by a binary that expects two columns it does not have.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");

    {
        // v4's schema, verbatim from the shipped `Store::init` — no `run_id`,
        // no `attribution`, no `runs`, and no `schema` key in `meta`.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_wall     TEXT    NOT NULL,
                ts_mono_ns  INTEGER NOT NULL,
                box_id      TEXT    NOT NULL,
                cgroup_id   INTEGER NOT NULL,
                pid         INTEGER NOT NULL,
                tid         INTEGER NOT NULL,
                ppid        INTEGER NOT NULL,
                comm        TEXT    NOT NULL,
                uid         INTEGER NOT NULL,
                type        TEXT    NOT NULL,
                peer        TEXT,
                dport       INTEGER,
                path        TEXT,
                raw         TEXT    NOT NULL
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('generation', '4242');
            "#,
        )
        .unwrap();
        let old = base(EventType::Exec, 700, 1, 99, "2026-08-01T00:00:00.000Z");
        let mut old = old;
        old.exec = Some(Exec {
            path: "/bin/true".into(),
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO events
               (ts_wall, ts_mono_ns, box_id, cgroup_id, pid, tid, ppid, comm, uid, type, raw)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                old.ts_wall,
                old.ts_mono_ns,
                old.box_id,
                old.cgroup_id,
                old.pid,
                old.tid,
                old.ppid,
                old.comm,
                old.uid,
                "exec",
                serde_json::to_string(&old).unwrap(),
            ],
        )
        .unwrap();
    }

    let mut store = Store::open(&path).expect("a v4 store must still open");
    assert_eq!(store.schema_version().unwrap(), 2);
    // The identity survives: a migration that reset the generation would tell
    // every reader its cursor belonged to a different database.
    assert_eq!(store.generation().unwrap(), 4242);

    let old = store.query(&Query::default()).unwrap();
    assert_eq!(old.len(), 1, "the pre-existing row must still read back");
    assert_eq!(old[0].pid, 700);

    // Its `run_id` is NULL, which is the truth — it belongs to no run.
    assert!(
        store
            .query(&Query {
                run_id: Some(RUN.to_string()),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );

    // And the new half works on the migrated database, through the same read
    // the collector makes: `active_runs` is what decides attribution, so a run
    // still marked `running` is the state that matters here.
    let mut live = record();
    live.status = RunStatus::Running.as_str().to_string();
    live.ended_at = None;
    store.insert_run(&live).unwrap();
    let mut attributor = Attributor::new();
    let active = store.active_runs().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].cgroup_id, CGROUP);
    attributor.set_active(active);
    let events = fixture();
    let tags: Vec<_> = events.iter().map(|e| attributor.attribute(e)).collect();
    store.insert_batch_tagged(&events, &tags).unwrap();
    assert_eq!(
        store
            .query(&Query {
                run_id: Some(RUN.to_string()),
                limit: Some(Query::MAX_LIMIT),
                ..Default::default()
            })
            .unwrap()
            .len(),
        8
    );

    // Re-opening is not a second migration.
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 2);
    assert_eq!(reopened.count().unwrap(), 10);
}

#[test]
fn the_start_of_a_run_is_recovered_once_the_host_learns_its_cgroup() {
    // The race this exists for: the cgroup only exists once the wrapper has
    // entered it, and the host only learns its id when the wrapper publishes
    // — by which time the command has already exec'd and connected. Before the
    // back-fill, a run that fetched a URL showed the connection and not the
    // process that made it, because `exec` all happens in that first gap.
    let mut store = Store::open_in_memory().unwrap();
    let mut live = record();
    live.status = RunStatus::Running.as_str().to_string();
    live.ended_at = None;
    // The host does not know the scope yet.
    live.cgroup_id = 0;
    live.root_pid = 0;
    store.insert_run(&live).unwrap();

    let mut attributor = Attributor::new();
    attributor.set_active(store.active_runs().unwrap());
    let events = fixture();
    let tags: Vec<_> = events.iter().map(|e| attributor.attribute(e)).collect();
    store.insert_batch_tagged(&events, &tags).unwrap();
    assert!(
        tags.iter().filter(|t| t.is_some()).count() < events.len(),
        "the gap this test is about did not happen"
    );

    // The wrapper publishes; the host records it and claims what it can.
    store.set_run_scope(RUN, CGROUP, ROOT_PID).unwrap();
    let claimed = store
        .backfill_run(RUN, CGROUP, "2026-09-05T10:00:00.000Z")
        .unwrap();
    assert_eq!(claimed, 7, "every event in the run's own cgroup");

    let mine = store
        .query(&Query {
            run_id: Some(RUN.to_string()),
            limit: Some(Query::MAX_LIMIT),
            ..Default::default()
        })
        .unwrap();
    assert!(
        mine.iter()
            .any(|e| e.comm == "curl" && e.kind == EventType::Exec),
        "the process that made the connection is still missing"
    );
    // And the stranger is still a stranger: the back-fill matches on the
    // kernel's cgroup id, so there is nothing for it to widen into.
    assert!(!mine.iter().any(|e| e.comm == "sshd"));

    // Idempotent — a second pass claims nothing, because the rows it would
    // have taken are no longer NULL.
    assert_eq!(
        store
            .backfill_run(RUN, CGROUP, "2026-09-05T10:00:00.000Z")
            .unwrap(),
        0
    );
    // And a run with no exclusive cgroup never claims anything at all, rather
    // than claiming everything whose cgroup id happens to be zero.
    assert_eq!(
        store
            .backfill_run(RUN, 0, "2026-09-05T10:00:00.000Z")
            .unwrap(),
        0
    );
}

#[test]
fn run_ids_sort_in_the_order_they_were_minted() {
    let ids: Vec<String> = (0..2000).map(|_| new_run_id()).collect();
    assert!(ids.iter().all(|id| is_run_id(id)));
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
}
