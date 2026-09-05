//! The canonical observability event — §11.1.
//!
//! This is the Rust half of a cross-language contract. `agent/event/event.go`
//! is the other half, and both decode the same fixture
//! (`agent/event/testdata/events.jsonl`) in their own test suites, so a field
//! rename on either side fails a test instead of silently rendering blanks in
//! the console.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// What kind of activity an event records, and which sub-object it populates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    Exec,
    Exit,
    Connect,
    Accept,
    /// Connection teardown. `Connect` and `Accept` record that a connection was
    /// made; this records what crossed it and how long it lasted.
    Close,
    Dns,
    Tls,
    File,
    Syscall,
    Api,
    Policy,
    /// A brokered credential use (§6.5). Produced on the host by
    /// `devbox __broker`, never by the guest agent — the whole point is that
    /// the guest never holds the credential that made the request possible.
    Credential,
}

impl EventType {
    /// Every type, in schema order.
    pub const ALL: &'static [EventType] = &[
        EventType::Exec,
        EventType::Exit,
        EventType::Connect,
        EventType::Accept,
        EventType::Close,
        EventType::Dns,
        EventType::Tls,
        EventType::File,
        EventType::Syscall,
        EventType::Api,
        EventType::Policy,
        EventType::Credential,
    ];

    /// The wire name, which is also the stored value and the filter token.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Exec => "exec",
            EventType::Exit => "exit",
            EventType::Connect => "connect",
            EventType::Accept => "accept",
            EventType::Close => "close",
            EventType::Dns => "dns",
            EventType::Tls => "tls",
            EventType::File => "file",
            EventType::Syscall => "syscall",
            EventType::Api => "api",
            EventType::Policy => "policy",
            EventType::Credential => "credential",
        }
    }

    /// Broad grouping used for colour-coding in the console.
    pub fn domain(&self) -> &'static str {
        match self {
            EventType::Exec | EventType::Exit => "process",
            EventType::Connect
            | EventType::Accept
            | EventType::Close
            | EventType::Dns
            | EventType::Tls => "network",
            EventType::File => "file",
            EventType::Syscall => "syscall",
            EventType::Api => "api",
            EventType::Policy => "policy",
            EventType::Credential => "credential",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        EventType::ALL
            .iter()
            .copied()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| anyhow::anyhow!("unknown event type '{s}'"))
    }
}

/// One captured activity, in the canonical envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// RFC 3339 with millisecond precision, UTC. Sorts lexicographically in
    /// chronological order, which makes it usable as a plain indexed column.
    pub ts_wall: String,
    /// Kernel monotonic nanoseconds. The ordering key — a wall clock can step
    /// backwards under NTP, and correlation needs a total order that cannot.
    pub ts_mono_ns: u64,

    pub box_id: String,
    #[serde(default)]
    pub cgroup_id: u64,

    pub pid: u32,
    #[serde(default)]
    pub tid: u32,
    #[serde(default)]
    pub ppid: u32,
    #[serde(default)]
    pub comm: String,
    #[serde(default)]
    pub uid: u32,

    #[serde(rename = "type")]
    pub kind: EventType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<Net>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<Exec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<File>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<Api>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<Credential>,
}

/// Connection, DNS, and TLS detail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    #[serde(default)]
    pub proto: String,
    #[serde(default)]
    pub saddr: String,
    #[serde(default)]
    pub sport: u16,
    #[serde(default)]
    pub daddr: String,
    #[serde(default)]
    pub dport: u16,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub domain: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sni: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alpn: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub qname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub qtype: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<String>,
    /// True only for a DNS answer correlated with a query observed from this
    /// box. The policy enforcer never trusts query-shaped packets.
    #[serde(default, skip_serializing_if = "is_false")]
    pub response: bool,

    /// Settled on a `close` event and nowhere else. Every probe that fires
    /// while a connection is being *made* runs before any payload has crossed
    /// it, so a `connect` carrying a byte count would be carrying a guess.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub bytes_tx: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub bytes_rx: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dur_ms: u64,

    /// `out` for a dialled connection, `in` for an accepted one. Carried on a
    /// `close`, whose type no longer says which; empty when it is unknown,
    /// which is exactly when `orphan` is set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    /// A `close` whose connect or accept was never captured — a connection
    /// older than the agent. The bytes are real; the process identity is
    /// whoever closed the socket, not whoever opened it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub orphan: bool,
}

/// Process-execution detail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
}

/// File-access detail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub flags: u32,
}

/// Application-level detail (opt-in, §7.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Api {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
}

/// An egress-policy decision (§8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// `allow`, `block`, or `flag`.
    pub verdict: String,
    /// The posture in force: `open`, `allowlist`, `mirror-only`, `isolated`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// One brokered credential use (§6.5).
///
/// Deliberately without a field for the credential itself, or for any header:
/// this row is written to the same store the console and the export read, and
/// an audit record that can leak the secret it audits is worse than no record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// The broker provider name: `anthropic`, `github`, `http:<name>`, ...
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
    /// The upstream host the broker spoke to, or the one it would have.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// The upstream path, query string stripped.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Upstream HTTP status, or 0 when the request never reached upstream.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub req_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub resp_bytes: u64,
    /// `allowed`, `denied`, or `error`.
    pub verdict: String,
    /// Why a request was denied, or how it failed. Never a header value.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

impl Event {
    /// Reject anything the collector should not store.
    ///
    /// A row that cannot be interpreted is worse than a dropped-event counter:
    /// it silently corrupts every later query and every behaviour diff.
    pub fn validate(&self) -> Result<()> {
        if self.ts_wall.is_empty() {
            bail!("ts_wall is required");
        }
        if self.box_id.is_empty() {
            bail!("box_id is required");
        }
        if self.pid == 0 {
            bail!("pid is required");
        }

        let missing = match self.kind {
            EventType::Exec => self.exec.is_none(),
            EventType::Connect
            | EventType::Accept
            | EventType::Close
            | EventType::Dns
            | EventType::Tls => self.net.is_none(),
            EventType::File => self.file.is_none(),
            EventType::Api => self.api.is_none(),
            EventType::Policy => self.policy.is_none(),
            EventType::Credential => self.credential.is_none(),
            EventType::Exit | EventType::Syscall => false,
        };
        if missing {
            bail!("{} event is missing its sub-object", self.kind);
        }
        Ok(())
    }

    /// The peer this event names, preferring the human-readable form.
    ///
    /// A DNS query names the thing being looked up; a connection names the
    /// domain that resolved to the address, falling back to the address. This
    /// is what makes the flow table show `pypi.org` rather than `151.101.0.223`.
    pub fn peer(&self) -> Option<String> {
        if self.kind == EventType::Credential {
            return self
                .credential
                .as_ref()
                .map(|c| c.host.clone())
                .filter(|host| !host.is_empty());
        }
        let net = self.net.as_ref()?;
        for candidate in [&net.qname, &net.domain, &net.sni, &net.daddr] {
            if !candidate.is_empty() {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// The filesystem path this event names, if any.
    pub fn path(&self) -> Option<&str> {
        match self.kind {
            EventType::File => self.file.as_ref().map(|f| f.path.as_str()),
            EventType::Exec => self.exec.as_ref().map(|e| e.path.as_str()),
            EventType::Credential => self
                .credential
                .as_ref()
                .map(|c| c.path.as_str())
                .filter(|path| !path.is_empty()),
            _ => None,
        }
    }

    /// A one-line rendering, used by the CLI and the export.
    pub fn summary(&self) -> String {
        match self.kind {
            EventType::Exec => {
                let e = self.exec.as_ref();
                let argv = e
                    .map(|e| {
                        if e.argv.is_empty() {
                            e.path.clone()
                        } else {
                            e.argv.join(" ")
                        }
                    })
                    .unwrap_or_default();
                format!("exec {argv}")
            }
            EventType::Exit => "exit".to_string(),
            EventType::Dns => {
                let n = self.net.as_ref();
                let name = n.map(|n| n.qname.as_str()).unwrap_or("");
                let answers = n.map(|n| n.answers.join(", ")).unwrap_or_default();
                if answers.is_empty() {
                    format!("dns {name}")
                } else {
                    format!("dns {name} → {answers}")
                }
            }
            EventType::Connect | EventType::Accept => {
                let n = self.net.as_ref();
                let peer = self.peer().unwrap_or_default();
                let port = n.map(|n| n.dport).unwrap_or(0);
                format!("{} {peer}:{port}", self.kind)
            }
            EventType::Close => {
                // The byte counts are the whole reason this event exists, so
                // they belong on the one line the CLI prints for it.
                let n = self.net.as_ref();
                let peer = self.peer().unwrap_or_default();
                let port = n.map(|n| n.dport).unwrap_or(0);
                let tx = crate::cli::watch::human_bytes(n.map_or(0, |n| n.bytes_tx));
                let rx = crate::cli::watch::human_bytes(n.map_or(0, |n| n.bytes_rx));
                let dur = n.map_or(0, |n| n.dur_ms);
                let mut text = format!("close {peer}:{port} \u{2191}{tx} \u{2193}{rx}");
                if dur > 0 {
                    text.push_str(&format!(" {dur}ms"));
                }
                if n.is_some_and(|n| n.orphan) {
                    // Said out loud rather than left to be inferred from a
                    // blank column: these bytes are real but unattributed, and
                    // a reader adding them to a process's total would be wrong.
                    text.push_str(" (orphan)");
                }
                text
            }
            EventType::Tls => {
                let sni = self.net.as_ref().map(|n| n.sni.as_str()).unwrap_or("");
                format!("tls {sni}")
            }
            EventType::File => {
                let f = self.file.as_ref();
                format!(
                    "file {} {}",
                    f.map(|f| f.op.as_str()).unwrap_or(""),
                    f.map(|f| f.path.as_str()).unwrap_or("")
                )
            }
            EventType::Api => {
                let a = self.api.as_ref();
                format!(
                    "api {} {}{}",
                    a.map(|a| a.method.as_str()).unwrap_or(""),
                    a.map(|a| a.host.as_str()).unwrap_or(""),
                    a.map(|a| a.path.as_str()).unwrap_or("")
                )
            }
            EventType::Policy => {
                let p = self.policy.as_ref();
                format!(
                    "policy {} {}",
                    p.map(|p| p.verdict.as_str()).unwrap_or(""),
                    p.map(|p| p.target.as_str()).unwrap_or("")
                )
            }
            EventType::Credential => {
                let c = self.credential.as_ref();
                format!(
                    "credential {} {} {}{} {}",
                    c.map(|c| c.verdict.as_str()).unwrap_or(""),
                    c.map(|c| c.provider.as_str()).unwrap_or(""),
                    c.map(|c| c.host.as_str()).unwrap_or(""),
                    c.map(|c| c.path.as_str()).unwrap_or(""),
                    c.map(|c| c.status).unwrap_or(0),
                )
            }
            EventType::Syscall => "syscall".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/event/testdata/events.jsonl")
    }

    #[test]
    fn every_type_round_trips_through_its_wire_name() {
        for t in EventType::ALL {
            let parsed: EventType = t.as_str().parse().unwrap();
            assert_eq!(parsed, *t);
        }
        assert!("nonsense".parse::<EventType>().is_err());
    }

    #[test]
    fn types_group_into_domains() {
        assert_eq!(EventType::Exec.domain(), "process");
        assert_eq!(EventType::Dns.domain(), "network");
        assert_eq!(EventType::Tls.domain(), "network");
        assert_eq!(EventType::File.domain(), "file");
        assert_eq!(EventType::Policy.domain(), "policy");
    }

    /// The cross-language contract: this is the same file the Go test suite
    /// reads. If the two schemas drift apart, one of the two tests fails.
    #[test]
    fn decodes_the_shared_go_fixture() {
        let text = std::fs::read_to_string(fixture_path()).expect("fixture is present");

        let mut seen = std::collections::BTreeSet::new();
        for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            let event: Event = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("fixture line {} does not decode: {e}", i + 1));
            event
                .validate()
                .unwrap_or_else(|e| panic!("fixture line {} is invalid: {e}", i + 1));
            seen.insert(event.kind);
        }

        assert_eq!(
            seen.len(),
            EventType::ALL.len(),
            "the fixture must exercise every event type; missing: {:?}",
            EventType::ALL
                .iter()
                .filter(|t| !seen.contains(t))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn re_encoding_the_fixture_preserves_every_event() {
        let text = std::fs::read_to_string(fixture_path()).unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let event: Event = serde_json::from_str(line).unwrap();
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back, "round trip changed the event");
        }
    }

    #[test]
    fn validate_requires_the_envelope_and_the_sub_object() {
        let mut e = Event {
            ts_wall: "2026-08-06T22:14:07.412Z".into(),
            ts_mono_ns: 1,
            box_id: "b".into(),
            cgroup_id: 0,
            pid: 1,
            tid: 1,
            ppid: 0,
            comm: "sh".into(),
            uid: 0,
            kind: EventType::Exec,
            net: None,
            exec: Some(Exec {
                path: "/bin/sh".into(),
                ..Default::default()
            }),
            file: None,
            api: None,
            policy: None,
            credential: None,
        };
        assert!(e.validate().is_ok());

        e.exec = None;
        assert!(e.validate().is_err(), "exec without its sub-object");

        // exit carries only the envelope.
        e.kind = EventType::Exit;
        assert!(e.validate().is_ok());

        e.pid = 0;
        assert!(e.validate().is_err());
    }

    #[test]
    fn a_close_line_leads_with_what_crossed_the_connection() {
        let mut e = Event {
            ts_wall: "t".into(),
            ts_mono_ns: 0,
            box_id: "b".into(),
            cgroup_id: 0,
            pid: 812,
            tid: 812,
            ppid: 640,
            comm: "curl".into(),
            uid: 1000,
            kind: EventType::Close,
            net: Some(Net {
                proto: "tcp".into(),
                daddr: "151.101.0.223".into(),
                dport: 443,
                bytes_tx: 4102,
                bytes_rx: 831_720,
                dur_ms: 690,
                dir: "out".into(),
                ..Default::default()
            }),
            exec: None,
            file: None,
            api: None,
            policy: None,
        };
        // The byte counts are the reason this event exists; a `close` line
        // without them would be indistinguishable from the `connect` it
        // settles.
        let line = e.summary();
        assert!(line.starts_with("close 151.101.0.223:443"), "{line}");
        assert!(line.contains("4.0KB"), "{line}");
        assert!(line.contains("812.2KB"), "{line}");
        assert!(line.contains("690ms"), "{line}");
        assert!(!line.contains("orphan"), "{line}");

        // Unattributed traffic says so, because a reader who adds it to a
        // process's total would be wrong.
        e.net.as_mut().unwrap().orphan = true;
        assert!(e.summary().ends_with("(orphan)"), "{}", e.summary());

        // And a close is a network event, so the console's existing chip
        // covers it rather than needing a new one.
        assert_eq!(EventType::Close.domain(), "network");

        // It carries a 5-tuple, so it needs the sub-object that holds one.
        e.net = None;
        assert!(e.validate().is_err(), "a close without its net sub-object");
    }

    #[test]
    fn peer_prefers_the_readable_name() {
        let mut e = Event {
            ts_wall: "t".into(),
            ts_mono_ns: 0,
            box_id: "b".into(),
            cgroup_id: 0,
            pid: 1,
            tid: 1,
            ppid: 0,
            comm: "c".into(),
            uid: 0,
            kind: EventType::Connect,
            net: Some(Net {
                daddr: "151.101.0.223".into(),
                dport: 443,
                ..Default::default()
            }),
            exec: None,
            file: None,
            api: None,
            policy: None,
            credential: None,
        };
        assert_eq!(e.peer().as_deref(), Some("151.101.0.223"));

        e.net.as_mut().unwrap().sni = "pypi.org".into();
        assert_eq!(
            e.peer().as_deref(),
            Some("pypi.org"),
            "sni beats the address"
        );

        e.net.as_mut().unwrap().domain = "files.pythonhosted.org".into();
        assert_eq!(
            e.peer().as_deref(),
            Some("files.pythonhosted.org"),
            "the resolved domain beats the sni"
        );
    }

    #[test]
    fn summaries_read_like_a_log_line() {
        let text = std::fs::read_to_string(fixture_path()).unwrap();
        let events: Vec<Event> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let summaries: Vec<String> = events.iter().map(|e| e.summary()).collect();
        assert!(summaries.iter().any(|s| s.contains("pip install requests")));
        assert!(summaries.iter().any(|s| s.contains("dns pypi.org →")));
        assert!(
            summaries
                .iter()
                .any(|s| s.starts_with("connect pypi.org:443"))
        );
        assert!(summaries.iter().any(|s| s.contains("policy block")));
        assert!(summaries.iter().all(|s| !s.is_empty()));
    }
}
