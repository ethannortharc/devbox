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
    ///
    /// From `close` events alone, matching `behavior::summarize`: a connection
    /// is settled once, when it ends, and every other network event about it
    /// was emitted before a byte had moved. Saturating, because these counters
    /// arrive from the guest and a plain `+` would panic a debug build on a
    /// pair of them that overflowed.
    pub fn bytes(&self) -> (u64, u64) {
        self.events
            .iter()
            .filter(|e| e.kind == EventType::Close)
            .filter_map(|e| e.net.as_ref())
            .fold((0, 0), |(tx, rx), n| {
                (tx.saturating_add(n.bytes_tx), rx.saturating_add(n.bytes_rx))
            })
    }
}

/// How far apart two events with the same pid must be to be different processes.
///
/// Linux recycles pids, so "same pid" is not "same process" over any real span
/// of time. Splitting on an `exec` boundary catches the common case exactly;
/// this gap catches the rest — a pid seen again after a long silence, with no
/// exec in between, is a different process on any box that has been up a while.
const PID_REUSE_GAP_NS: u64 = 60 * 1_000_000_000;

/// Group events into per-process chains.
///
/// Ordering inside a chain is by `ts_mono_ns` — the kernel's monotonic clock,
/// which unlike the wall clock cannot step backwards under NTP. Chains
/// themselves are ordered by when each process was first seen, so the output
/// reads chronologically.
///
/// A pid is not an identity: the kernel reuses it, and on a long-running or
/// rebooted box the same number covers several unrelated processes. Grouping
/// on pid alone merged their commands, parents, peers, and files into one
/// chain that never existed. So a chain is broken whenever the pid is clearly
/// a different process — a new `exec` after the chain already had one, an
/// `exit` (nothing that pid does afterwards belongs to it), or a long gap.
pub fn chains(events: &[Event]) -> Vec<Chain> {
    // Wall clock first, monotonic second — the same reason `behavior::summarize`
    // does: `ts_mono_ns` restarts at each guest boot while the store persists
    // across boots, so ordering by it alone interleaves two boots' events and
    // the gap heuristic below then splits chains in the wrong places.
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by(|a, b| {
        a.pid
            .cmp(&b.pid)
            .then_with(|| a.ts_wall.cmp(&b.ts_wall))
            .then_with(|| a.ts_mono_ns.cmp(&b.ts_mono_ns))
    });

    // Keyed on (pid, incarnation) so the same pid can hold several chains.
    let mut by_pid: BTreeMap<(u32, u32), Vec<&Event>> = BTreeMap::new();
    let mut incarnation: BTreeMap<u32, u32> = BTreeMap::new();
    let mut last_ts: BTreeMap<u32, u64> = BTreeMap::new();
    let mut has_exec: BTreeMap<u32, bool> = BTreeMap::new();

    for event in ordered {
        let pid = event.pid;
        let generation = incarnation.entry(pid).or_insert(0);

        let gap = last_ts.get(&pid).is_some_and(|prev| {
            // A clock that went *backwards* is not a small gap.
            //
            // `ts_mono_ns` is monotonic within a boot and restarts with the
            // guest, so a window spanning a reboot sees the new boot's
            // timestamps as smaller than the old boot's. `saturating_sub`
            // turned that into zero — the one value that most certainly means
            // "the same process, moments later" — so two unrelated processes
            // sharing a pid across the reboot were merged into one chain, and
            // their events then sorted into each other.
            //
            // A rollback is the strongest evidence available that this is a
            // different incarnation, so it says so directly.
            event.ts_mono_ns < *prev || event.ts_mono_ns - *prev > PID_REUSE_GAP_NS
        });
        let re_exec = event.kind == EventType::Exec && *has_exec.get(&pid).unwrap_or(&false);

        if gap || re_exec {
            *generation += 1;
            has_exec.insert(pid, false);
        }

        if event.kind == EventType::Exec {
            has_exec.insert(pid, true);
        }
        if event.kind == EventType::Exit {
            // Whatever this pid does next is a different process.
            *incarnation.entry(pid).or_insert(0) += 1;
            has_exec.insert(pid, false);
        }
        last_ts.insert(pid, event.ts_mono_ns);

        let key = (pid, *incarnation.get(&pid).unwrap_or(&0));
        // Exit closes the chain it belongs to, not the one after it.
        let key = if event.kind == EventType::Exit {
            (pid, key.1.saturating_sub(1))
        } else {
            key
        };
        by_pid.entry(key).or_default().push(event);
    }

    let mut chains: Vec<Chain> = by_pid
        .into_iter()
        .map(|((pid, _generation), mut group)| {
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

    // Wall clock first, as the grouping above already does. Ordering the
    // output by uptime alone would put a newer boot's chains before older
    // ones, undoing the cross-boot correctness the grouping was careful about.
    chains.sort_by(|a, b| {
        a.events[0]
            .ts_wall
            .cmp(&b.events[0].ts_wall)
            .then_with(|| a.events[0].ts_mono_ns.cmp(&b.events[0].ts_mono_ns))
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
pub fn dns_map(events: &[Event]) -> BTreeMap<String, Vec<(String, String)>> {
    let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut entries = 0usize;
    for event in events {
        if event.kind != EventType::Dns {
            continue;
        }
        let Some(net) = &event.net else { continue };
        if net.qname.is_empty() {
            continue;
        }
        // Bounded, because both halves of every entry are guest-supplied and
        // this map is rebuilt on every render of the Activity tab. One lookup
        // is allowed to answer with thousands of addresses, and each answer
        // clones the name — so a single event well under the frame limit
        // expanded into hundreds of megabytes of map.
        if entries >= MAX_DNS_MAP_ENTRIES {
            tracing::debug!(
                limit = MAX_DNS_MAP_ENTRIES,
                "dns correlation map is full; later answers are not indexed"
            );
            break;
        }
        for answer in net.answers.iter().take(MAX_DNS_ANSWERS_PER_LOOKUP) {
            entries += 1;
            // Every answer, with when it was given. A single first-wins name
            // per address misattributed every connection once two domains
            // shared a CDN address — including connections that *preceded*
            // the lookup it was credited to. The label a flow deserves is the
            // most recent answer before it, so the timestamps have to survive
            // into the map.
            map.entry(answer.clone())
                .or_default()
                .push((event.ts_wall.clone(), net.qname.clone()));
        }
    }
    map
}

/// Answers indexed from one lookup.
///
/// A resolver returning more than this is not describing a destination anyone
/// is about to connect to.
const MAX_DNS_ANSWERS_PER_LOOKUP: usize = 64;

/// Entries the correlation map will hold for one window.
///
/// The map exists to give a connection a readable name. Past this many
/// address-to-name pairs it has stopped being that and started being a way to
/// spend the console's memory.
const MAX_DNS_MAP_ENTRIES: usize = 50_000;

/// Fill in `net.domain` on connections whose address a DNS answer explains.
///
/// Returns how many events gained a name.
pub fn apply_dns_map(events: &mut [Event], map: &BTreeMap<String, Vec<(String, String)>>) -> usize {
    let mut labelled = 0;
    for event in events.iter_mut() {
        let when = event.ts_wall.clone();
        let Some(net) = event.net.as_mut() else {
            continue;
        };
        if !net.domain.is_empty() || net.daddr.is_empty() {
            continue;
        }
        let Some(answers) = map.get(&net.daddr) else {
            continue;
        };
        // The most recent answer at or before this connection. Falling back to
        // the earliest answer covers a connect whose lookup the capture missed
        // — better a plausible name than none — but a connection that follows
        // a *different* lookup now gets that lookup's name.
        let name = answers
            .iter()
            .filter(|(ts, _)| *ts <= when)
            .max_by(|a, b| a.0.cmp(&b.0))
            .or_else(|| answers.iter().min_by(|a, b| a.0.cmp(&b.0)));
        if let Some((_, name)) = name {
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

    // Every chain for a pid, in start order — not one. `chains()` splits a
    // reused pid into several incarnations on purpose, and a map keyed on pid
    // alone kept whichever came last, so an earlier child would be filed under
    // the *newer* process's parent and appear in the wrong tree.
    let mut by_pid: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, chain) in chains.iter().enumerate() {
        by_pid.entry(chain.pid).or_default().push(i);
    }

    // The incarnation of `pid` that was alive when `at` happened: the last one
    // that had already started. A parent always starts before its child.
    let contemporaneous = |pid: u32, at: &Chain| -> Option<usize> {
        let candidates = by_pid.get(&pid)?;
        candidates
            .iter()
            .rev()
            .find(|&&i| chains[i].events[0].ts_wall <= at.events[0].ts_wall)
            .or_else(|| candidates.first())
            .copied()
    };

    let depth_of = |start: usize| -> usize {
        let mut depth = 0;
        let mut seen = std::collections::HashSet::new();
        let mut current = start;
        while depth < MAX_DEPTH && seen.insert(current) {
            let Some(i) = contemporaneous(chains[current].ppid, &chains[current]) else {
                break;
            };
            if i == current {
                break; // its own parent, which nothing real produces
            }
            current = i;
            depth += 1;
        }
        depth
    };

    (0..chains.len()).map(|i| (i, depth_of(i))).collect()
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
            credential: None,
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

    fn connect(pid: u32, mono: u64, addr: &str) -> Event {
        let mut e = base(pid, 640, mono, EventType::Connect);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: addr.into(),
            dport: 443,
            ..Default::default()
        });
        e
    }

    /// The connection ending, which is where its traffic is reported.
    fn close(pid: u32, mono: u64, addr: &str, tx: u64, rx: u64) -> Event {
        let mut e = base(pid, 640, mono, EventType::Close);
        e.net = Some(Net {
            proto: "tcp".into(),
            daddr: addr.into(),
            dport: 443,
            bytes_tx: tx,
            bytes_rx: rx,
            dir: "out".into(),
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
            connect(812, 3_000, "151.101.0.223"),
            dns(812, 4_000, "files.pythonhosted.org", &["151.101.1.63"]),
            connect(812, 5_000, "151.101.1.63"),
            file(812, 6_000, "/workspace/.venv/lib/requests/__init__.py"),
            file(812, 7_000, "/workspace/.venv/lib/requests/api.py"),
            close(812, 8_000, "151.101.0.223", 4102, 831_720),
            close(812, 9_000, "151.101.1.63", 900, 61_000),
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
        assert_eq!(c.events.len(), 9);
        assert_eq!(
            c.peers.len(),
            4,
            "two names and two addresses; a close names the peer its connect already did"
        );
        assert_eq!(c.files.len(), 2);

        let (tx, rx) = c.bytes();
        assert_eq!(tx, 5002);
        assert_eq!(rx, 892_720);
    }

    #[test]
    fn a_chains_bytes_come_from_its_closes_only() {
        // `watch --tree` credits a process with what its connections moved.
        // The credit has to come from one event per connection, or the day a
        // connect probe starts filling counters every tree doubles.
        let mut events = pip_install();
        let opening = events
            .iter_mut()
            .find(|e| e.kind == EventType::Connect)
            .expect("the run dials something");
        let net = opening.net.as_mut().unwrap();
        net.bytes_tx = 999_999;
        net.bytes_rx = 999_999;

        let c = &chains(&events)[0];
        assert_eq!(c.bytes(), (5002, 892_720));
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
    fn a_monotonic_rollback_starts_a_new_incarnation() {
        // `ts_mono_ns` restarts with the guest, so a window spanning a reboot
        // sees the new boot's timestamps as *smaller* than the old boot's.
        // `saturating_sub` turned that into a zero gap — the value that most
        // certainly means "the same process, moments later" — so two unrelated
        // processes that happened to share a pid across the reboot were merged
        // into one chain and their events sorted into each other.
        //
        // No exec or exit in the window to give the split away: that is the
        // case the gap heuristic exists for, and the case it got backwards.
        let events = vec![
            base(812, 640, 900_000_000_000, EventType::Syscall),
            base(812, 640, 1_000_000_000, EventType::Syscall),
        ];
        assert_eq!(
            chains(&events).len(),
            2,
            "a clock that went backwards is a different boot, not a 0ns gap"
        );

        // And the ordinary forward step still keeps one chain.
        let same = vec![
            base(812, 640, 1_000_000_000, EventType::Syscall),
            base(812, 640, 1_500_000_000, EventType::Syscall),
        ];
        assert_eq!(chains(&same).len(), 1);
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
            connect(812, 1_000, "10.0.0.1"),
            connect(812, 2_000, "10.0.0.2"),
            connect(812, 3_000, "10.0.0.1"),
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

        let named = |addr: &str| -> Vec<String> {
            map.get(addr)
                .map(|answers| answers.iter().map(|(_, n)| n.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(named("151.101.0.223"), vec!["pypi.org"]);
        assert_eq!(named("151.101.1.63"), vec!["files.pythonhosted.org"]);

        let labelled = apply_dns_map(&mut events, &map);
        assert_eq!(
            labelled, 4,
            "both connections and both settlements gained a name"
        );

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
        let answers = map.get("10.0.0.9").expect("the address was answered");
        assert!(answers.iter().any(|(_, name)| name == "a.example"));
    }

    #[test]
    fn a_shared_cdn_address_follows_the_most_recent_lookup() {
        // Two domains behind one address is the normal case for a CDN, and a
        // first-wins map credited every connection to whichever resolved
        // first — including connections made before that lookup happened.
        // Whole seconds apart: the fixture derives `ts_wall` from the
        // monotonic value at second granularity, and correlation orders by
        // wall clock (it is what survives a guest reboot).
        const SEC: u64 = 1_000_000_000;
        let mut events = vec![
            dns(812, SEC, "first.example", &["10.0.0.9"]),
            connect(812, 2 * SEC, "10.0.0.9"),
            dns(812, 3 * SEC, "second.example", &["10.0.0.9"]),
            connect(812, 4 * SEC, "10.0.0.9"),
        ];

        let map = dns_map(&events);
        assert_eq!(apply_dns_map(&mut events, &map), 2);
        assert_eq!(events[1].net.as_ref().unwrap().domain, "first.example");
        assert_eq!(
            events[3].net.as_ref().unwrap().domain,
            "second.example",
            "a connection after the second lookup belongs to the second name"
        );
    }

    #[test]
    fn apply_dns_map_never_overwrites_a_name_the_agent_already_supplied() {
        let mut events = vec![connect(812, 1_000, "10.0.0.9")];
        events[0].net.as_mut().unwrap().domain = "authoritative.example".into();

        let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        map.insert(
            "10.0.0.9".to_string(),
            vec![("2026-08-07T00:00:00.000Z".into(), "guessed.example".into())],
        );

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
