//! Behaviour diff — §7.6.
//!
//! `devbox diff` answers "what files changed?". This answers "what did it
//! *do*?": the domains contacted, the processes spawned, the files written,
//! and any policy violations, for a run — and the difference between two runs.
//!
//! "This run contacted a domain the last one didn't" is the question worth
//! being able to ask, and it is why the summary is a comparable value rather
//! than a formatted string.

use std::collections::BTreeSet;

use serde::Serialize;

use super::correlate;
use super::event::{Event, EventType};

/// What a box did over a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub box_id: String,
    /// First and last event timestamps in the window.
    pub started: String,
    pub ended: String,
    pub events: usize,

    /// Every domain or address contacted, sorted.
    pub domains: BTreeSet<String>,
    /// Distinct process names executed, sorted.
    pub processes: BTreeSet<String>,
    /// Files written or created, sorted.
    pub files_written: BTreeSet<String>,
    /// Distinct DNS names looked up, sorted.
    pub dns_queries: BTreeSet<String>,

    pub bytes_tx: u64,
    pub bytes_rx: u64,

    /// Policy posture seen in the window, if any policy event occurred.
    pub egress_mode: Option<String>,
    /// Blocked or flagged connections.
    pub violations: Vec<Violation>,

    /// API endpoints reached and the tokens seen, when API capture is on.
    pub api_calls: Vec<ApiCall>,
}

/// A connection policy did not allow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Violation {
    pub target: String,
    pub verdict: String,
    pub reason: String,
    pub ts: String,
}

/// An application-level call, aggregated per host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ApiCall {
    pub host: String,
    pub calls: usize,
    pub tokens: u64,
}

/// Build a summary from a window of events.
///
/// The events are labelled with the DNS map first, so a connection that only
/// carried an address is credited to the name that resolved it (§7.3).
pub fn summarize(box_id: &str, events: &[Event]) -> Summary {
    let mut events = events.to_vec();
    let map = correlate::dns_map(&events);
    correlate::apply_dns_map(&mut events, &map);
    // Wall clock first, monotonic second. `ts_mono_ns` restarts at each guest
    // boot while the store persists across boots, so ordering by it alone put
    // a fresh boot's events *before* everything older — the summary's start
    // and end times came out reversed for any window spanning a reboot. The
    // monotonic value still breaks ties, which is what gives sub-millisecond
    // events within one boot a stable order.
    events.sort_by(|a, b| {
        a.ts_wall
            .cmp(&b.ts_wall)
            .then_with(|| a.ts_mono_ns.cmp(&b.ts_mono_ns))
    });

    let mut summary = Summary {
        box_id: box_id.to_string(),
        events: events.len(),
        started: events
            .first()
            .map(|e| e.ts_wall.clone())
            .unwrap_or_default(),
        ended: events.last().map(|e| e.ts_wall.clone()).unwrap_or_default(),
        ..Default::default()
    };

    let mut api: std::collections::BTreeMap<String, (usize, u64)> = Default::default();

    for event in &events {
        match event.kind {
            EventType::Exec => {
                if let Some(exec) = &event.exec {
                    // The basename is what a person recognizes; the full
                    // /nix/store path is noise in a summary.
                    let name = exec
                        .path
                        .rsplit('/')
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&exec.path);
                    summary.processes.insert(name.to_string());
                }
            }
            EventType::Dns => {
                if let Some(net) = &event.net
                    && !net.qname.is_empty()
                {
                    summary.dns_queries.insert(net.qname.clone());
                }
            }
            EventType::Connect | EventType::Accept | EventType::Tls => {
                if let Some(peer) = event.peer() {
                    summary.domains.insert(peer);
                }
                if let Some(net) = &event.net {
                    // Saturating: an agent supplies these and nothing
                    // validates them, so two events claiming most of a `u64`
                    // between them would panic a checked build and wrap a
                    // release one — on every render of the Activity tab.
                    summary.bytes_tx = summary.bytes_tx.saturating_add(net.bytes_tx);
                    summary.bytes_rx = summary.bytes_rx.saturating_add(net.bytes_rx);
                }
            }
            EventType::File => {
                if let Some(file) = &event.file
                    && matches!(file.op.as_str(), "write" | "create")
                {
                    summary.files_written.insert(file.path.clone());
                }
            }
            EventType::Policy => {
                if let Some(policy) = &event.policy {
                    summary.egress_mode = Some(policy.mode.clone());
                    if policy.verdict != "allow" {
                        summary.violations.push(Violation {
                            target: policy.target.clone(),
                            verdict: policy.verdict.clone(),
                            reason: policy.reason.clone(),
                            ts: event.ts_wall.clone(),
                        });
                    }
                }
            }
            EventType::Api => {
                if let Some(call) = &event.api
                    && !call.host.is_empty()
                {
                    let entry = api.entry(call.host.clone()).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 = entry.1.saturating_add(call.tokens);
                }
            }
            EventType::Exit | EventType::Syscall => {}
        }
    }

    summary.api_calls = api
        .into_iter()
        .map(|(host, (calls, tokens))| ApiCall {
            host,
            calls,
            tokens,
        })
        .collect();
    summary.violations.sort();

    summary
}

/// What changed between two runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Diff {
    pub new_domains: Vec<String>,
    pub gone_domains: Vec<String>,
    pub new_processes: Vec<String>,
    pub gone_processes: Vec<String>,
    pub new_files: Vec<String>,
    /// Byte deltas, which can be negative.
    pub bytes_tx_delta: i64,
    pub bytes_rx_delta: i64,
    pub new_violations: usize,
}

impl Diff {
    /// Whether anything changed at all.
    pub fn is_empty(&self) -> bool {
        self.new_domains.is_empty()
            && self.gone_domains.is_empty()
            && self.new_processes.is_empty()
            && self.gone_processes.is_empty()
            && self.new_files.is_empty()
            && self.new_violations == 0
            // Two runs can touch identical domains and processes while moving
            // very different amounts of data. Leaving these out made the diff
            // report "no behavioural change" and then never render the traffic
            // delta it had already computed.
            && self.bytes_tx_delta == 0
            && self.bytes_rx_delta == 0
    }

    /// Whether anything appeared that was not there before.
    ///
    /// This is the question that matters for review: a run that stops doing
    /// something is rarely alarming; a run that starts contacting a new domain
    /// is exactly what a behaviour diff is for.
    pub fn has_new_behavior(&self) -> bool {
        !self.new_domains.is_empty()
            || !self.new_processes.is_empty()
            // A run that starts writing somewhere it never wrote before is
            // exactly as reportable as one that contacts a new domain, and
            // `is_empty` already counted it — leaving it out here meant the
            // two disagreed about whether anything happened.
            || !self.new_files.is_empty()
            || self.new_violations > 0
    }
}

/// Compare two summaries.
/// `after - before` as a signed number, clamped rather than wrapped.
fn signed_delta(before: u64, after: u64) -> i64 {
    if after >= before {
        i64::try_from(after - before).unwrap_or(i64::MAX)
    } else {
        i64::try_from(before - after).map_or(i64::MIN, |d| -d)
    }
}

pub fn diff(before: &Summary, after: &Summary) -> Diff {
    Diff {
        new_domains: difference(&after.domains, &before.domains),
        gone_domains: difference(&before.domains, &after.domains),
        new_processes: difference(&after.processes, &before.processes),
        gone_processes: difference(&before.processes, &after.processes),
        new_files: difference(&after.files_written, &before.files_written),
        // Signed subtraction on values an agent supplied, so neither the
        // cast nor the difference may be taken on trust: `u64::MAX as i64` is
        // -1, which reported a vast transfer as a byte less than nothing, and
        // two large totals overflowed the subtraction outright.
        bytes_tx_delta: signed_delta(before.bytes_tx, after.bytes_tx),
        bytes_rx_delta: signed_delta(before.bytes_rx, after.bytes_rx),
        // Compared by identity, not by count. One violation against A
        // followed by one against B is the same length, and subtracting gave
        // zero — so a run that started hitting a different blocked target
        // reported nothing new, which is precisely the alarm this exists for.
        new_violations: {
            // Both sides deduplicated. `violation_key` says a target blocked
            // twice for the same reason is one recurring problem — but only
            // `before` was a set, so one unseen target hit a hundred times
            // reported a hundred new violations.
            let seen: BTreeSet<_> = before.violations.iter().map(violation_key).collect();
            let now: BTreeSet<_> = after.violations.iter().map(violation_key).collect();
            now.difference(&seen).count()
        },
    }
}

/// What makes two violations the same violation.
///
/// The timestamp is deliberately excluded: the same target blocked twice for
/// the same reason is one recurring problem, not two.
fn violation_key(v: &Violation) -> (&str, &str, &str) {
    (v.target.as_str(), v.verdict.as_str(), v.reason.as_str())
}

fn difference(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    a.difference(b).cloned().collect()
}

/// Render a summary the way §7.6 shows it.
pub fn render_markdown(summary: &Summary) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(out, "# Behavior summary — box \"{}\"", summary.box_id);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "**Window:** {} → {}  ·  **{} events**",
        if summary.started.is_empty() {
            "—"
        } else {
            &summary.started
        },
        if summary.ended.is_empty() {
            "—"
        } else {
            &summary.ended
        },
        summary.events
    );
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "- **Domains contacted ({}):** {}",
        summary.domains.len(),
        list(&summary.domains)
    );
    let _ = writeln!(
        out,
        "- **Processes ({}):** {}",
        summary.processes.len(),
        list(&summary.processes)
    );
    let _ = writeln!(
        out,
        "- **DNS lookups ({}):** {}",
        summary.dns_queries.len(),
        list(&summary.dns_queries)
    );
    let _ = writeln!(
        out,
        "- **Files written ({}):** {}",
        summary.files_written.len(),
        list(&summary.files_written)
    );
    let _ = writeln!(
        out,
        "- **Traffic:** ↑{} ↓{}",
        crate::cli::watch::human_bytes(summary.bytes_tx),
        crate::cli::watch::human_bytes(summary.bytes_rx)
    );

    // Not "open". The posture is only known from a policy event, and a window
    // holding none says nothing about it — an isolated box that simply refused
    // nothing in the last hour was being written into an audit summary as
    // wide open, which is the opposite of the truth.
    let _ = writeln!(
        out,
        "- **Egress policy:** {} — {} violation(s)",
        summary
            .egress_mode
            .as_deref()
            .unwrap_or("not recorded in this window"),
        summary.violations.len()
    );

    if !summary.violations.is_empty() {
        let _ = writeln!(out, "\n## Violations\n");
        for v in &summary.violations {
            let _ = writeln!(
                out,
                "- `{}` **{}** — {} ({})",
                v.target, v.verdict, v.reason, v.ts
            );
        }
    }

    if !summary.api_calls.is_empty() {
        let _ = writeln!(out, "\n## API calls\n");
        for call in &summary.api_calls {
            let _ = writeln!(
                out,
                "- `{}` — {} call(s), ~{} tokens",
                call.host, call.calls, call.tokens
            );
        }
    }

    out
}

/// Render a diff for a human.
pub fn render_diff_markdown(diff: &Diff) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("# Behavior diff\n\n");

    if diff.is_empty() {
        out.push_str("No behavioural change between the two runs.\n");
        return out;
    }

    let section = |out: &mut String, title: &str, items: &[String]| {
        if items.is_empty() {
            return;
        }
        let _ = writeln!(out, "**{title} ({})**", items.len());
        for item in items {
            let _ = writeln!(out, "- `{item}`");
        }
        out.push('\n');
    };

    section(&mut out, "New domains", &diff.new_domains);
    section(&mut out, "Domains no longer contacted", &diff.gone_domains);
    section(&mut out, "New processes", &diff.new_processes);
    section(&mut out, "Processes no longer run", &diff.gone_processes);
    section(&mut out, "New files written", &diff.new_files);

    if diff.new_violations > 0 {
        let _ = writeln!(out, "**{} new policy violation(s)**\n", diff.new_violations);
    }
    let _ = writeln!(
        out,
        "Traffic delta: ↑{:+} ↓{:+} bytes",
        diff.bytes_tx_delta, diff.bytes_rx_delta
    );

    out
}

/// Comma-separated, truncated for readability.
fn list(items: &BTreeSet<String>) -> String {
    const SHOWN: usize = 12;
    if items.is_empty() {
        return "—".to_string();
    }
    let shown: Vec<&String> = items.iter().take(SHOWN).collect();
    let rest = items.len().saturating_sub(SHOWN);
    let mut text = shown
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if rest > 0 {
        text.push_str(&format!(", … and {rest} more"));
    }
    text
}

/// Export events as JSON Lines.
pub fn render_jsonl(events: &[Event]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for event in events {
        out.push_str(&serde_json::to_string(event)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_window_with_no_policy_event_does_not_claim_a_posture() {
        // An audit summary that reports a posture it never observed is worse
        // than one that admits it did not see it — and `open` is the most
        // reassuring of the four, which is exactly the wrong default.
        let summary = Summary::default();
        let rendered = render_markdown(&summary);
        assert!(!rendered.contains("Egress policy:** open"), "{rendered}");
        assert!(rendered.contains("not recorded"));
    }

    use super::*;
    use crate::obs::event::{Api, Exec, File, Net, Policy};

    fn base(pid: u32, mono: u64, kind: EventType) -> Event {
        Event {
            ts_wall: format!("2026-08-06T22:00:{:02}.000Z", mono / 1_000_000_000),
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

    fn run() -> Vec<Event> {
        let mut exec = base(812, 1_000_000_000, EventType::Exec);
        exec.exec = Some(Exec {
            path: "/nix/store/abc-python3.12/bin/python3.12".into(),
            argv: vec!["python3.12".into(), "-m".into(), "pip".into()],
            cwd: "/workspace".into(),
        });

        let mut dns = base(812, 2_000_000_000, EventType::Dns);
        dns.net = Some(Net {
            qname: "pypi.org".into(),
            qtype: "A".into(),
            answers: vec!["151.101.0.223".into()],
            ..Default::default()
        });

        let mut connect = base(812, 3_000_000_000, EventType::Connect);
        connect.net = Some(Net {
            proto: "tcp".into(),
            daddr: "151.101.0.223".into(),
            dport: 443,
            bytes_tx: 4102,
            bytes_rx: 831_720,
            ..Default::default()
        });

        let mut write = base(812, 4_000_000_000, EventType::File);
        write.file = Some(File {
            path: "/workspace/.venv/requests.py".into(),
            op: "write".into(),
            flags: 0,
        });

        let mut read = base(812, 5_000_000_000, EventType::File);
        read.file = Some(File {
            path: "/etc/passwd".into(),
            op: "open".into(),
            flags: 0,
        });

        vec![exec, dns, connect, write, read]
    }

    #[test]
    fn summarizes_a_run() {
        let s = summarize("myapp", &run());

        assert_eq!(s.box_id, "myapp");
        assert_eq!(s.events, 5);
        assert_eq!(s.started, "2026-08-06T22:00:01.000Z");
        assert_eq!(s.ended, "2026-08-06T22:00:05.000Z");

        // The connection carried only an address; the DNS answer names it.
        assert!(s.domains.contains("pypi.org"), "domains: {:?}", s.domains);
        assert!(!s.domains.contains("151.101.0.223"));

        // The basename, not the /nix/store path.
        assert!(s.processes.contains("python3.12"), "{:?}", s.processes);

        assert_eq!(s.dns_queries.len(), 1);
        assert_eq!(s.bytes_tx, 4102);
        assert_eq!(s.bytes_rx, 831_720);
    }

    #[test]
    fn only_writes_count_as_files_written() {
        let s = summarize("myapp", &run());
        assert!(s.files_written.contains("/workspace/.venv/requests.py"));
        assert!(
            !s.files_written.contains("/etc/passwd"),
            "a read is not a write"
        );
    }

    #[test]
    fn collects_policy_violations_and_the_posture() {
        let mut events = run();
        let mut blocked = base(941, 6_000_000_000, EventType::Policy);
        blocked.policy = Some(Policy {
            verdict: "block".into(),
            mode: "allowlist".into(),
            target: "telemetry.example.com".into(),
            reason: "not in allowlist".into(),
        });
        let mut allowed = base(941, 7_000_000_000, EventType::Policy);
        allowed.policy = Some(Policy {
            verdict: "allow".into(),
            mode: "allowlist".into(),
            target: "github.com".into(),
            reason: String::new(),
        });
        events.push(blocked);
        events.push(allowed);

        let s = summarize("myapp", &events);
        assert_eq!(s.egress_mode.as_deref(), Some("allowlist"));
        assert!(render_markdown(&s).contains("allowlist"));
        assert_eq!(
            s.violations.len(),
            1,
            "only non-allow verdicts are violations"
        );
        assert_eq!(s.violations[0].target, "telemetry.example.com");
    }

    #[test]
    fn aggregates_api_calls_per_host() {
        let mut events = run();
        for tokens in [1000, 2000] {
            let mut call = base(941, 8_000_000_000, EventType::Api);
            call.api = Some(Api {
                method: "POST".into(),
                host: "api.anthropic.com".into(),
                path: "/v1/messages".into(),
                status: 200,
                tokens,
                endpoint: "messages".into(),
            });
            events.push(call);
        }

        let s = summarize("myapp", &events);
        assert_eq!(s.api_calls.len(), 1);
        assert_eq!(s.api_calls[0].calls, 2);
        assert_eq!(s.api_calls[0].tokens, 3000);
    }

    #[test]
    fn an_empty_window_summarizes_to_nothing() {
        let s = summarize("myapp", &[]);
        assert_eq!(s.events, 0);
        assert!(s.domains.is_empty());
        assert!(s.started.is_empty());
    }

    #[test]
    fn diff_reports_what_appeared_and_what_left() {
        let before = summarize("myapp", &run());

        let mut later = run();
        let mut extra_dns = base(900, 9_000_000_000, EventType::Dns);
        extra_dns.net = Some(Net {
            qname: "telemetry.example.com".into(),
            answers: vec!["10.0.0.9".into()],
            ..Default::default()
        });
        let mut extra_connect = base(900, 10_000_000_000, EventType::Connect);
        extra_connect.net = Some(Net {
            daddr: "10.0.0.9".into(),
            dport: 443,
            bytes_tx: 100,
            bytes_rx: 200,
            ..Default::default()
        });
        later.push(extra_dns);
        later.push(extra_connect);

        let after = summarize("myapp", &later);
        let d = diff(&before, &after);

        assert_eq!(d.new_domains, vec!["telemetry.example.com"]);
        assert!(d.gone_domains.is_empty());
        assert_eq!(d.bytes_tx_delta, 100);
        assert_eq!(d.bytes_rx_delta, 200);
        assert!(d.has_new_behavior(), "a new domain is new behaviour");
        assert!(!d.is_empty());
    }

    #[test]
    fn diff_of_identical_runs_is_empty() {
        let s = summarize("myapp", &run());
        let d = diff(&s, &s);
        assert!(d.is_empty());
        assert!(!d.has_new_behavior());
        assert_eq!(d.bytes_tx_delta, 0);
    }

    #[test]
    fn a_run_that_merely_stops_doing_things_is_not_new_behavior() {
        let before = summarize("myapp", &run());
        let after = summarize("myapp", &[]);
        let d = diff(&before, &after);

        assert!(!d.gone_domains.is_empty());
        assert!(
            !d.has_new_behavior(),
            "doing less is not what a behaviour diff warns about"
        );
        assert!(!d.is_empty(), "but it is still a change");
    }

    #[test]
    fn markdown_summary_covers_every_section() {
        let mut events = run();
        let mut blocked = base(941, 6_000_000_000, EventType::Policy);
        blocked.policy = Some(Policy {
            verdict: "block".into(),
            mode: "allowlist".into(),
            target: "telemetry.example.com".into(),
            reason: "not in allowlist".into(),
        });
        events.push(blocked);

        let md = render_markdown(&summarize("myapp", &events));
        assert!(md.contains("Behavior summary"));
        assert!(md.contains("myapp"));
        assert!(md.contains("pypi.org"));
        assert!(md.contains("python3.12"));
        assert!(md.contains("812.2KB"), "traffic should be human-readable");
        assert!(md.contains("allowlist"));
        assert!(md.contains("Violations"));
        assert!(md.contains("telemetry.example.com"));
    }

    #[test]
    fn markdown_diff_says_so_when_nothing_changed() {
        let s = summarize("myapp", &run());
        let md = render_diff_markdown(&diff(&s, &s));
        assert!(md.contains("No behavioural change"));
    }

    #[test]
    fn long_lists_are_truncated_with_a_count() {
        let many: BTreeSet<String> = (0..30).map(|i| format!("host{i:02}.example")).collect();
        let text = list(&many);
        assert!(text.contains("and 18 more"), "got: {text}");
        assert!(!text.contains("host29"));

        assert_eq!(list(&BTreeSet::new()), "—");
    }

    #[test]
    fn jsonl_export_is_one_event_per_line() {
        let events = run();
        let text = render_jsonl(&events).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), events.len());

        // Every line must independently decode.
        for line in lines {
            let _: Event = serde_json::from_str(line).unwrap();
        }
    }
}
