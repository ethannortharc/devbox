//! Prometheus exporter — §7.7.
//!
//! Text format, rendered on demand from the collector's counters and the
//! event store. No metrics client library: the exposition format is a dozen
//! lines of text, and the alternative is a dependency plus a global registry
//! plus a lifecycle to manage.
//!
//! The dropped-event counter is the one that matters most. §7.3 says events
//! are never silently dropped — this is where "never silently" is made true.

use std::fmt::Write as _;

use crate::obs::collector::StatsSnapshot;
use crate::obs::event::EventType;

/// One metric family.
struct Family {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
}

const FAMILIES: &[Family] = &[
    Family {
        name: "devbox_events_received_total",
        help: "Events received from agents, before validation.",
        kind: "counter",
    },
    Family {
        name: "devbox_events_stored_total",
        help: "Events written to the per-box store.",
        kind: "counter",
    },
    Family {
        name: "devbox_events_dropped_total",
        help: "Events dropped because the collector queue was full.",
        kind: "counter",
    },
    Family {
        name: "devbox_events_rejected_total",
        help: "Events refused because they did not decode or validate.",
        kind: "counter",
    },
    Family {
        name: "devbox_agents_connected_total",
        help: "Agent connections accepted since start.",
        kind: "counter",
    },
    Family {
        name: "devbox_events_by_type",
        help: "Events currently stored, by event type.",
        kind: "gauge",
    },
    Family {
        name: "devbox_boxes",
        help: "Registered boxes, by status.",
        kind: "gauge",
    },
    Family {
        name: "devbox_build_info",
        help: "Build identity of the running devbox binary.",
        kind: "gauge",
    },
];

/// Everything the exporter renders.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub collector: StatsSnapshot,
    /// Stored event counts by type.
    pub events_by_type: Vec<(EventType, u64)>,
    /// Registered boxes by status label (`running`, `stopped`, …).
    pub boxes_by_status: Vec<(String, u64)>,
    pub version: String,
}

/// Escape a Prometheus label value.
///
/// Backslash, quote, and newline are the three characters that can break the
/// exposition format; a box name is user-chosen, so this is not theoretical.
pub fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Render the Prometheus text exposition format.
pub fn render(snapshot: &Snapshot) -> String {
    let mut out = String::with_capacity(1024);

    let header = |out: &mut String, name: &str| {
        if let Some(f) = FAMILIES.iter().find(|f| f.name == name) {
            let _ = writeln!(out, "# HELP {} {}", f.name, f.help);
            let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind);
        }
    };

    let c = &snapshot.collector;
    for (name, value) in [
        ("devbox_events_received_total", c.received),
        ("devbox_events_stored_total", c.stored),
        ("devbox_events_dropped_total", c.dropped),
        ("devbox_events_rejected_total", c.rejected),
        ("devbox_agents_connected_total", c.agents_connected),
    ] {
        header(&mut out, name);
        let _ = writeln!(out, "{name} {value}");
    }

    header(&mut out, "devbox_events_by_type");
    // Every type, every scrape, zero when absent — not only when the whole map
    // is empty. The old branch emitted the full set at zero on a quiet box and
    // then, the moment one type appeared, dropped every other series. An alert
    // on "no exec events" would have gone blind exactly when the box got busy,
    // which is the opposite of when you want it working.
    for kind in EventType::ALL {
        let n = snapshot
            .events_by_type
            .iter()
            .find(|(k, _)| k == kind)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let _ = writeln!(out, "devbox_events_by_type{{type=\"{kind}\"}} {n}");
    }

    header(&mut out, "devbox_boxes");
    // Every status, every scrape — the same reason the event types are all
    // emitted. A status series that only appears once a box is in that state
    // cannot be alerted on, and "no box is running" is exactly the condition
    // worth alerting on.
    for status in ["running", "stopped", "not-found", "unknown"] {
        let n = snapshot
            .boxes_by_status
            .iter()
            .find(|(s, _)| s == status)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let _ = writeln!(out, "devbox_boxes{{status=\"{status}\"}} {n}");
    }
    // Anything the runtime reports that is not in that list still gets a
    // series, so an unexpected state is visible rather than swallowed.
    for (status, n) in &snapshot.boxes_by_status {
        if !matches!(
            status.as_str(),
            "running" | "stopped" | "not-found" | "unknown"
        ) {
            let _ = writeln!(
                out,
                "devbox_boxes{{status=\"{}\"}} {n}",
                escape_label(status)
            );
        }
    }

    header(&mut out, "devbox_build_info");
    let _ = writeln!(
        out,
        "devbox_build_info{{version=\"{}\"}} 1",
        escape_label(&snapshot.version)
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            collector: StatsSnapshot {
                received: 1200,
                stored: 1195,
                dropped: 5,
                rejected: 0,
                agents_connected: 2,
            },
            events_by_type: vec![(EventType::Exec, 40), (EventType::Connect, 155)],
            boxes_by_status: vec![("running".into(), 1), ("stopped".into(), 2)],
            version: "0.1.3".into(),
        }
    }

    #[test]
    fn renders_the_exposition_format() {
        let text = render(&snapshot());

        assert!(text.contains("# HELP devbox_events_received_total"));
        assert!(text.contains("# TYPE devbox_events_received_total counter"));
        assert!(text.contains("devbox_events_received_total 1200"));
        assert!(text.contains("devbox_events_stored_total 1195"));
        assert!(text.contains("devbox_agents_connected_total 2"));
    }

    #[test]
    fn the_dropped_counter_is_always_present() {
        // §7.3: never silently drop. A series that only appears when non-zero
        // cannot be alerted on, so it must be emitted even at zero.
        let mut s = snapshot();
        s.collector.dropped = 0;
        let text = render(&s);
        assert!(text.contains("devbox_events_dropped_total 0"));

        let text = render(&snapshot());
        assert!(text.contains("devbox_events_dropped_total 5"));
    }

    #[test]
    fn every_family_is_declared_before_use() {
        let text = render(&snapshot());
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let name = line
                .split(['{', ' '])
                .next()
                .expect("a metric line names a family");
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "{name} is emitted without a TYPE declaration"
            );
        }
    }

    #[test]
    fn event_types_are_emitted_at_zero_when_nothing_is_stored() {
        let mut s = snapshot();
        s.events_by_type.clear();
        let text = render(&s);

        for kind in EventType::ALL {
            assert!(
                text.contains(&format!("devbox_events_by_type{{type=\"{kind}\"}} 0")),
                "{kind} is missing from an empty snapshot"
            );
        }
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("plain"), "plain");
        assert_eq!(escape_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label(r"a\b"), r"a\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");

        // A box name is user-chosen, so this is not theoretical.
        let mut s = snapshot();
        s.boxes_by_status = vec![("weird\"name".into(), 1)];
        let text = render(&s);
        assert!(text.contains(r#"status="weird\"name""#), "got: {text}");
    }

    #[test]
    fn output_is_line_oriented_and_ends_cleanly() {
        let text = render(&snapshot());
        assert!(text.ends_with('\n'));
        assert!(!text.contains("\n\n"), "no blank lines in the exposition");
    }
}
