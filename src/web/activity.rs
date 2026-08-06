//! Activity views — §7.5.
//!
//! Four renderings of one store: the live stream, the flow table, the process
//! tree, and the DNS log. All of them are derived here as plain data so the
//! templates stay dumb and the derivations stay testable.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::obs::behavior::{self, Summary};
use crate::obs::correlate::{self, Chain};
use crate::obs::event::{Event, EventType};
use crate::obs::store::{Query, Store};
use crate::sandbox::SandboxManager;

/// How many events the Activity tab loads on first paint.
///
/// Enough to see what just happened, small enough that the page renders
/// instantly; the live stream carries everything after that.
pub const INITIAL_EVENTS: usize = 200;

/// Open a box's event store, if it has one yet.
///
/// Returns `None` rather than an error when no agent has ever connected —
/// "nothing recorded yet" is a state to render, not a failure.
pub fn open_store(manager: &Arc<SandboxManager>, name: &str) -> Result<Option<Store>> {
    let path = crate::obs::collector::store_path(&manager.state_dir, name);
    if !path.exists() {
        return Ok(None);
    }
    Store::open(&path).map(Some)
}

/// One row of the live event stream.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamRow {
    pub ts: String,
    pub kind: String,
    /// Colour-coding group: process, network, file, syscall, api, policy.
    pub domain: &'static str,
    pub pid: u32,
    pub comm: String,
    pub summary: String,
}

impl From<&Event> for StreamRow {
    fn from(e: &Event) -> Self {
        Self {
            // Time-of-day only: the date is the same for everything on screen.
            ts: e.ts_wall.get(11..23).unwrap_or(&e.ts_wall).to_string(),
            kind: e.kind.to_string(),
            domain: e.kind.domain(),
            pid: e.pid,
            comm: e.comm.clone(),
            summary: e.summary(),
        }
    }
}

/// One row of the flow table (§7.5).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Flow {
    pub peer: String,
    pub addr: String,
    pub port: u16,
    pub proto: String,
    pub sni: String,
    pub alpn: String,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub dur_ms: u64,
    pub pid: u32,
    pub comm: String,
    pub ts: String,
    pub direction: &'static str,
}

/// Collapse connect/accept/tls events into one row per flow.
///
/// A connection and its TLS handshake are two events about one thing; showing
/// them as separate rows is exactly the wall-of-events the flow table exists
/// to replace. They are joined on (pid, address, port).
pub fn flows(events: &[Event]) -> Vec<Flow> {
    use std::collections::BTreeMap;

    let mut by_key: BTreeMap<(u32, String, u16, String, u16), Flow> = BTreeMap::new();

    for event in events {
        let Some(net) = &event.net else { continue };
        let direction = match event.kind {
            EventType::Connect => "out",
            EventType::Accept => "in",
            // TLS enriches a flow it does not create.
            EventType::Tls => "out",
            _ => continue,
        };

        // Keyed on the full 4-tuple plus protocol. Dropping the source port
        // merged every sequential connection to the same destination into one
        // row, so its timestamp, duration, and byte counts described several
        // unrelated connections at once. TLS events are matched back to their
        // connect by the same tuple, which is why enrichment still lands.
        let key = (
            event.pid,
            net.proto.clone(),
            net.sport,
            net.daddr.clone(),
            net.dport,
        );
        let flow = by_key.entry(key).or_insert_with(|| Flow {
            peer: event.peer().unwrap_or_else(|| net.daddr.clone()),
            addr: net.daddr.clone(),
            port: net.dport,
            proto: net.proto.clone(),
            sni: String::new(),
            alpn: String::new(),
            bytes_tx: 0,
            bytes_rx: 0,
            dur_ms: 0,
            pid: event.pid,
            comm: event.comm.clone(),
            ts: event.ts_wall.clone(),
            direction,
        });

        if !net.sni.is_empty() {
            flow.sni = net.sni.clone();
            // An SNI is a better name than an address, and better than a
            // reverse-mapped guess.
            flow.peer = net.sni.clone();
        }
        if !net.alpn.is_empty() {
            flow.alpn = net.alpn.clone();
        }
        if !net.domain.is_empty() && flow.peer == flow.addr {
            flow.peer = net.domain.clone();
        }
        flow.bytes_tx += net.bytes_tx;
        flow.bytes_rx += net.bytes_rx;
        flow.dur_ms = flow.dur_ms.max(net.dur_ms);
        if event.kind != EventType::Tls {
            flow.direction = direction;
        }
    }

    let mut out: Vec<Flow> = by_key.into_values().collect();
    // Busiest first: the flow you want to look at is almost always the one
    // moving the most data.
    out.sort_by(|a, b| (b.bytes_rx + b.bytes_tx).cmp(&(a.bytes_rx + a.bytes_tx)));
    out
}

/// One DNS lookup, for the DNS log.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Lookup {
    pub ts: String,
    pub name: String,
    pub qtype: String,
    pub answers: Vec<String>,
    pub pid: u32,
    pub comm: String,
}

/// Every DNS lookup, newest first.
pub fn lookups(events: &[Event]) -> Vec<Lookup> {
    let mut out: Vec<Lookup> = events
        .iter()
        .filter(|e| e.kind == EventType::Dns)
        .filter_map(|e| {
            let net = e.net.as_ref()?;
            (!net.qname.is_empty()).then(|| Lookup {
                ts: e.ts_wall.get(11..23).unwrap_or(&e.ts_wall).to_string(),
                name: net.qname.clone(),
                qtype: net.qtype.clone(),
                answers: net.answers.clone(),
                pid: e.pid,
                comm: e.comm.clone(),
            })
        })
        .collect();
    out.reverse();
    out
}

/// A process-tree row: a chain plus its indent depth.
#[derive(Debug, Clone, Serialize)]
pub struct TreeRow {
    pub depth: usize,
    pub chain: Chain,
}

/// The process tree, ready to render.
pub fn process_tree(events: &[Event]) -> Vec<TreeRow> {
    let chains = correlate::chains(events);
    correlate::tree(&chains)
        .into_iter()
        .map(|(i, depth)| TreeRow {
            depth,
            chain: chains[i].clone(),
        })
        .collect()
}

/// Everything the Activity tab needs, loaded in one pass.
pub struct Activity {
    pub events: Vec<Event>,
    pub stream: Vec<StreamRow>,
    pub flows: Vec<Flow>,
    pub lookups: Vec<Lookup>,
    pub tree: Vec<TreeRow>,
    pub summary: Summary,
    /// False when no agent has ever connected for this box.
    pub has_store: bool,
}

/// Load and derive every activity view for a box.
pub fn load(manager: &Arc<SandboxManager>, name: &str, limit: usize) -> Result<Activity> {
    let Some(store) = open_store(manager, name)? else {
        return Ok(Activity {
            events: vec![],
            stream: vec![],
            flows: vec![],
            lookups: vec![],
            tree: vec![],
            summary: Summary::default(),
            has_store: false,
        });
    };

    // Newest-first so the limit keeps recent events, then reversed so the
    // views read forwards.
    let mut events = store.query(&Query {
        limit: Some(limit),
        newest_first: true,
        ..Default::default()
    })?;
    events.reverse();

    let map = correlate::dns_map(&events);
    correlate::apply_dns_map(&mut events, &map);

    Ok(Activity {
        stream: events.iter().rev().map(StreamRow::from).collect(),
        flows: flows(&events),
        lookups: lookups(&events),
        tree: process_tree(&events),
        summary: behavior::summarize(name, &events),
        events,
        has_store: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Exec, Net};

    fn base(pid: u32, mono: u64, kind: EventType) -> Event {
        Event {
            ts_wall: format!("2026-08-06T22:14:{:02}.412Z", mono / 1_000_000_000),
            ts_mono_ns: mono,
            box_id: "myapp".into(),
            cgroup_id: 1,
            pid,
            tid: pid,
            ppid: 640,
            comm: "pip".into(),
            uid: 1000,
            kind,
            net: None,
            exec: None,
            file: None,
            api: None,
            policy: None,
        }
    }

    fn connect(pid: u32, mono: u64, addr: &str, port: u16, tx: u64, rx: u64) -> Event {
        let mut e = base(pid, mono, EventType::Connect);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: addr.into(),
            dport: port,
            bytes_tx: tx,
            bytes_rx: rx,
            dur_ms: 690,
            ..Default::default()
        });
        e
    }

    #[test]
    fn stream_rows_show_time_of_day_and_a_colour_group() {
        let mut e = base(812, 1_000_000_000, EventType::Exec);
        e.exec = Some(Exec {
            path: "/bin/pip".into(),
            argv: vec!["pip".into(), "install".into()],
            ..Default::default()
        });

        let row = StreamRow::from(&e);
        assert_eq!(row.ts, "22:14:01.412", "the date is redundant on screen");
        assert_eq!(row.domain, "process");
        assert_eq!(row.pid, 812);
        assert!(row.summary.contains("pip install"));
    }

    #[test]
    fn a_connection_and_its_tls_handshake_are_one_flow() {
        let mut tls = base(812, 2_000_000_000, EventType::Tls);
        tls.net = Some(Net {
            proto: "tcp".into(),
            daddr: "151.101.0.223".into(),
            dport: 443,
            sni: "pypi.org".into(),
            alpn: "h2".into(),
            ..Default::default()
        });

        let rows = flows(&[
            connect(812, 1_000_000_000, "151.101.0.223", 443, 4102, 831_720),
            tls,
        ]);

        assert_eq!(rows.len(), 1, "two events about one connection is one row");
        let f = &rows[0];
        assert_eq!(f.peer, "pypi.org", "the SNI names the flow");
        assert_eq!(f.addr, "151.101.0.223");
        assert_eq!(f.alpn, "h2");
        assert_eq!(f.bytes_tx, 4102);
        assert_eq!(f.bytes_rx, 831_720);
        assert_eq!(f.direction, "out");
    }

    #[test]
    fn flows_are_sorted_by_traffic() {
        let rows = flows(&[
            connect(812, 1_000_000_000, "10.0.0.1", 80, 1, 1),
            connect(812, 2_000_000_000, "10.0.0.2", 80, 1000, 9000),
            connect(812, 3_000_000_000, "10.0.0.3", 80, 50, 50),
        ]);
        assert_eq!(rows[0].addr, "10.0.0.2");
        assert_eq!(rows[2].addr, "10.0.0.1");
    }

    #[test]
    fn different_processes_to_the_same_peer_are_different_flows() {
        let rows = flows(&[
            connect(812, 1_000_000_000, "10.0.0.1", 443, 1, 1),
            connect(900, 2_000_000_000, "10.0.0.1", 443, 1, 1),
        ]);
        assert_eq!(rows.len(), 2, "a flow belongs to a process");
    }

    #[test]
    fn accepts_are_marked_inbound() {
        let mut e = base(900, 1_000_000_000, EventType::Accept);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: "10.0.0.9".into(),
            dport: 51999,
            ..Default::default()
        });
        assert_eq!(flows(&[e])[0].direction, "in");
    }

    #[test]
    fn dns_log_is_newest_first_and_carries_answers() {
        let mut a = base(812, 1_000_000_000, EventType::Dns);
        a.net = Some(Net {
            qname: "pypi.org".into(),
            qtype: "A".into(),
            answers: vec!["151.101.0.223".into()],
            ..Default::default()
        });
        let mut b = base(812, 2_000_000_000, EventType::Dns);
        b.net = Some(Net {
            qname: "files.pythonhosted.org".into(),
            qtype: "A".into(),
            answers: vec![],
            ..Default::default()
        });

        let log = lookups(&[a, b]);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].name, "files.pythonhosted.org", "newest first");
        assert_eq!(log[1].answers, vec!["151.101.0.223"]);
    }

    #[test]
    fn non_dns_events_do_not_appear_in_the_dns_log() {
        assert!(lookups(&[connect(1, 1, "10.0.0.1", 80, 0, 0)]).is_empty());
    }

    #[test]
    fn process_tree_nests_children_under_parents() {
        let mut parent = base(640, 1_000_000_000, EventType::Exec);
        parent.ppid = 1;
        parent.comm = "zsh".into();
        parent.exec = Some(Exec {
            path: "/bin/zsh".into(),
            ..Default::default()
        });

        let mut child = base(812, 2_000_000_000, EventType::Exec);
        child.ppid = 640;
        child.exec = Some(Exec {
            path: "/bin/pip".into(),
            ..Default::default()
        });

        let rows = process_tree(&[parent, child]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].chain.pid, 812);
    }

    #[test]
    fn every_view_is_empty_for_an_empty_window() {
        assert!(flows(&[]).is_empty());
        assert!(lookups(&[]).is_empty());
        assert!(process_tree(&[]).is_empty());
    }
}
