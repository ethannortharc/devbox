//! Correlation — §7.2, "the killer feature".
//!
//! Raw events are cheap; the value is the join. A `pip install requests` run
//! produces an exec, two DNS lookups, two connections, and four hundred file
//! writes — as a flat list that is noise. Keyed on pid and ordered by the
//! monotonic clock, it is one story:
//!
//! ```text
//! exec  pip install requests
//!   ├── dns     pypi.org → 151.101.0.223
//!   ├── connect pypi.org:443   ↑4.1KB ↓812KB
//!   └── file    write /workspace/.venv/.../requests/__init__.py
//! ```
//!
//! Pure functions over a slice of events, so the whole thing is testable
//! without a kernel, a box, or a database.

use std::collections::BTreeMap;

use serde::Serialize;

use super::event::{Event, EventType};

/// One process and everything it did, as the console presents it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Chain {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    /// The command line, if an exec was captured for this pid.
    pub command: Option<String>,
    /// Wall-clock timestamp of this process's first event.
    pub started: String,
    /// Domains and addresses it reached, in first-contact order.
    pub peers: Vec<String>,
    /// Files it touched, in first-touch order.
    pub files: Vec<String>,
    /// Every event belonging to this pid, in monotonic order.
    pub events: Vec<Event>,
}

impl Chain {
    /// Events of a given kind within this chain.
    pub fn of_kind(&self, kind: EventType) -> impl Iterator<Item = &Event> {
        self.events.iter().filter(move |e| e.kind == kind)
    }

    /// Total bytes sent and received across this process's connections.
    pub fn bytes(&self) -> (u64, u64) {
        self.events
            .iter()
            .filter_map(|e| e.net.as_ref())
            .fold((0, 0), |(tx, rx), n| (tx + n.bytes_tx, rx + n.bytes_rx))
    }
}

/// Group events into per-process chains.
///
/// Ordering inside a chain is by `ts_mono_ns` — the kernel's monotonic clock,
/// which unlike the wall clock cannot step backwards under NTP. Chains
/// themselves are ordered by when each process was first seen, so the output
/// reads chronologically.
pub fn chains(events: &[Event]) -> Vec<Chain> {
    let mut by_pid: BTreeMap<u32, Vec<&Event>> = BTreeMap::new();
    for event in events {
        by_pid.entry(event.pid).or_default().push(event);
    }

    let mut chains: Vec<Chain> = by_pid
        .into_iter()
        .map(|(pid, mut group)| {
            group.sort_by_key(|e| e.ts_mono_ns);

            let first = group[0];
            let command = group
                .iter()
                .find(|e| e.kind == EventType::Exec)
                .and_then(|e| e.exec.as_ref())
                .map(|x| {
                    if x.argv.is_empty() {
                        x.path.clone()
                    } else {
                        x.argv.join(" ")
                    }
                });

            Chain {
                pid,
                ppid: first.ppid,
                // A process's `comm` changes at exec; the most specific name
                // is the one that came with the exec, if there was one.
                comm: group
                    .iter()
                    .find(|e| e.kind == EventType::Exec)
                    .map(|e| e.comm.clone())
                    .unwrap_or_else(|| first.comm.clone()),
                command,
                started: first.ts_wall.clone(),
                peers: dedup_in_order(group.iter().filter_map(|e| e.peer())),
                files: dedup_in_order(
                    group
                        .iter()
                        .filter(|e| e.kind == EventType::File)
                        .filter_map(|e| e.path().map(str::to_string)),
                ),
                events: group.into_iter().cloned().collect(),
            }
        })
        .collect();

    chains.sort_by(|a, b| {
        a.events[0]
            .ts_mono_ns
            .cmp(&b.events[0].ts_mono_ns)
            .then(a.pid.cmp(&b.pid))
    });
    chains
}

/// Keep first occurrences, drop repeats, preserve order.
fn dedup_in_order(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if item.is_empty() {
            continue;
        }
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// A resolved name mapped to the addresses it answered with.
///
/// This is what lets a connection to `151.101.0.223` be labelled `pypi.org`
/// after the fact, even when the connect event itself carried only an address
/// (§7.3, "IPs reverse-mapped to the DNS name that produced them").
pub fn dns_map(events: &[Event]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for event in events {
        if event.kind != EventType::Dns {
            continue;
        }
        let Some(net) = &event.net else { continue };
        if net.qname.is_empty() {
            continue;
        }
        for answer in &net.answers {
            // First answer wins: a later lookup of a different name that
            // happens to share a CDN address must not relabel earlier flows.
            map.entry(answer.clone())
                .or_insert_with(|| net.qname.clone());
        }
    }
    map
}

/// Fill in `net.domain` on connections whose address a DNS answer explains.
///
/// Returns how many events gained a name.
pub fn apply_dns_map(events: &mut [Event], map: &BTreeMap<String, String>) -> usize {
    let mut labelled = 0;
    for event in events.iter_mut() {
        let Some(net) = event.net.as_mut() else {
            continue;
        };
        if !net.domain.is_empty() || net.daddr.is_empty() {
            continue;
        }
        if let Some(name) = map.get(&net.daddr) {
            net.domain = name.clone();
            labelled += 1;
        }
    }
    labelled
}

/// Build a process tree from a set of chains: `(chain index, depth)`.
///
/// Depth is capped so a pid/ppid cycle — which a wrapped pid namespace can
/// genuinely produce — cannot make this recurse forever.
pub fn tree(chains: &[Chain]) -> Vec<(usize, usize)> {
    const MAX_DEPTH: usize = 32;

    let index: BTreeMap<u32, usize> = chains.iter().enumerate().map(|(i, c)| (c.pid, i)).collect();

    let depth_of = |mut pid: u32| -> usize {
        let mut depth = 0;
        let mut seen = std::collections::HashSet::new();
        while depth < MAX_DEPTH && seen.insert(pid) {
            let Some(&i) = index.get(&pid) else { break };
            let parent = chains[i].ppid;
            if parent == 0 || parent == pid || !index.contains_key(&parent) {
                break;
            }
            pid = parent;
            depth += 1;
        }
        depth
    };

    chains
        .iter()
        .enumerate()
        .map(|(i, c)| (i, depth_of(c.pid)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Exec, File, Net};

    fn base(pid: u32, ppid: u32, mono: u64, kind: EventType) -> Event {
        Event {
            ts_wall: format!("2026-08-06T22:00:{:02}.000Z", mono / 1_000_000_000),
            ts_mono_ns: mono,
            box_id: "myapp".into(),
            cgroup_id: 1,
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
        }
    }

    fn exec(pid: u32, ppid: u32, mono: u64, comm: &str, argv: &[&str]) -> Event {
        let mut e = base(pid, ppid, mono, EventType::Exec);
        e.comm = comm.to_string();
        e.exec = Some(Exec {
            path: format!("/bin/{comm}"),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: "/workspace".into(),
        });
        e
    }

    fn dns(pid: u32, mono: u64, name: &str, answers: &[&str]) -> Event {
        let mut e = base(pid, 640, mono, EventType::Dns);
        e.net = Some(Net {
            proto: "udp".into(),
            qname: name.into(),
            qtype: "A".into(),
            answers: answers.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        });
        e
    }

    fn connect(pid: u32, mono: u64, addr: &str, tx: u64, rx: u64) -> Event {
        let mut e = base(pid, 640, mono, EventType::Connect);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: addr.into(),
            dport: 443,
            bytes_tx: tx,
            bytes_rx: rx,
            ..Default::default()
        });
        e
    }

    fn file(pid: u32, mono: u64, path: &str) -> Event {
        let mut e = base(pid, 640, mono, EventType::File);
        e.file = Some(File {
            path: path.into(),
            op: "write".into(),
            flags: 0,
        });
        e
    }

    /// The §7.2 story, end to end.
    fn pip_install() -> Vec<Event> {
        vec![
            exec(812, 640, 1_000, "pip", &["pip", "install", "requests"]),
            dns(812, 2_000, "pypi.org", &["151.101.0.223"]),
            connect(812, 3_000, "151.101.0.223", 4102, 831_720),
            dns(812, 4_000, "files.pythonhosted.org", &["151.101.1.63"]),
            connect(812, 5_000, "151.101.1.63", 900, 61_000),
            file(812, 6_000, "/workspace/.venv/lib/requests/__init__.py"),
            file(812, 7_000, "/workspace/.venv/lib/requests/api.py"),
        ]
    }

    #[test]
    fn groups_a_run_into_one_chain() {
        let chains = chains(&pip_install());
        assert_eq!(chains.len(), 1);

        let c = &chains[0];
        assert_eq!(c.pid, 812);
        assert_eq!(c.comm, "pip");
        assert_eq!(c.command.as_deref(), Some("pip install requests"));
        assert_eq!(c.events.len(), 7);
        assert_eq!(c.peers.len(), 4, "two names and two addresses");
        assert_eq!(c.files.len(), 2);

        let (tx, rx) = c.bytes();
        assert_eq!(tx, 5002);
        assert_eq!(rx, 892_720);
    }

    #[test]
    fn events_within_a_chain_are_monotonic_order_not_arrival_order() {
        let mut shuffled = pip_install();
        shuffled.reverse();

        let chains = chains(&shuffled);
        let monos: Vec<u64> = chains[0].events.iter().map(|e| e.ts_mono_ns).collect();
        let mut sorted = monos.clone();
        sorted.sort_unstable();
        assert_eq!(monos, sorted);
    }

    #[test]
    fn chains_are_ordered_by_first_activity() {
        let events = vec![
            exec(900, 640, 5_000, "git", &["git", "status"]),
            exec(812, 640, 1_000, "pip", &["pip", "install"]),
        ];
        let chains = chains(&events);
        assert_eq!(chains[0].pid, 812, "the earlier process comes first");
        assert_eq!(chains[1].pid, 900);
    }

    #[test]
    fn comm_comes_from_the_exec_when_there_is_one() {
        // A shell that execs into pip: the useful name is `pip`, not `sh`.
        let mut events = vec![base(812, 640, 500, EventType::Syscall)];
        events.push(exec(812, 640, 1_000, "pip", &["pip"]));
        assert_eq!(chains(&events)[0].comm, "pip");

        // With no exec, the first event's comm is all there is.
        let chains = chains(&[base(812, 640, 500, EventType::Syscall)]);
        assert_eq!(chains[0].comm, "sh");
    }

    #[test]
    fn peers_and_files_deduplicate_but_keep_order() {
        let events = vec![
            connect(812, 1_000, "10.0.0.1", 0, 0),
            connect(812, 2_000, "10.0.0.2", 0, 0),
            connect(812, 3_000, "10.0.0.1", 0, 0),
            file(812, 4_000, "/a"),
            file(812, 5_000, "/a"),
            file(812, 6_000, "/b"),
        ];
        let c = &chains(&events)[0];
        assert_eq!(c.peers, vec!["10.0.0.1", "10.0.0.2"]);
        assert_eq!(c.files, vec!["/a", "/b"]);
    }

    #[test]
    fn dns_map_labels_connections_by_the_name_that_resolved_them() {
        let mut events = pip_install();
        let map = dns_map(&events);

        assert_eq!(
            map.get("151.101.0.223").map(String::as_str),
            Some("pypi.org")
        );
        assert_eq!(
            map.get("151.101.1.63").map(String::as_str),
            Some("files.pythonhosted.org")
        );

        let labelled = apply_dns_map(&mut events, &map);
        assert_eq!(labelled, 2, "both connections gained a name");

        let c = &chains(&events)[0];
        assert!(c.peers.contains(&"pypi.org".to_string()));
        assert!(
            !c.peers.contains(&"151.101.0.223".to_string()),
            "once named, the raw address should not also appear"
        );
    }

    #[test]
    fn dns_map_does_not_relabel_a_shared_cdn_address() {
        // Two names resolving to the same address: the first wins, and the
        // second must not silently rewrite flows attributed to the first.
        let events = vec![
            dns(812, 1_000, "a.example", &["10.0.0.9"]),
            dns(812, 2_000, "b.example", &["10.0.0.9"]),
        ];
        let map = dns_map(&events);
        assert_eq!(map.get("10.0.0.9").map(String::as_str), Some("a.example"));
    }

    #[test]
    fn apply_dns_map_never_overwrites_a_name_the_agent_already_supplied() {
        let mut events = vec![connect(812, 1_000, "10.0.0.9", 0, 0)];
        events[0].net.as_mut().unwrap().domain = "authoritative.example".into();

        let mut map = BTreeMap::new();
        map.insert("10.0.0.9".to_string(), "guessed.example".to_string());

        assert_eq!(apply_dns_map(&mut events, &map), 0);
        assert_eq!(
            events[0].net.as_ref().unwrap().domain,
            "authoritative.example"
        );
    }

    #[test]
    fn tree_reports_depth_from_the_parent_chain() {
        let events = vec![
            exec(640, 1, 1_000, "zsh", &["zsh"]),
            exec(812, 640, 2_000, "pip", &["pip"]),
            exec(900, 812, 3_000, "gcc", &["gcc"]),
        ];
        let chains = chains(&events);
        let depths: Vec<usize> = tree(&chains).into_iter().map(|(_, d)| d).collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn tree_survives_a_pid_cycle() {
        // A wrapped pid namespace can genuinely produce this; recursing on it
        // would hang the console.
        let mut a = exec(10, 20, 1_000, "a", &["a"]);
        let mut b = exec(20, 10, 2_000, "b", &["b"]);
        a.ppid = 20;
        b.ppid = 10;

        let chains = chains(&[a, b]);
        let depths: Vec<usize> = tree(&chains).into_iter().map(|(_, d)| d).collect();
        assert_eq!(depths.len(), 2);
        assert!(depths.iter().all(|d| *d < 32), "depth must be bounded");
    }

    #[test]
    fn empty_input_yields_no_chains() {
        assert!(chains(&[]).is_empty());
        assert!(dns_map(&[]).is_empty());
        assert!(tree(&[]).is_empty());
    }
}
