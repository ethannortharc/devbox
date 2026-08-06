//! Straggler detection — §9.5.
//!
//! The failure that dominates real AI clusters is not an outage: it is one
//! slow or flapping link turning a collective operation into a queue behind
//! its slowest participant. A ring all-reduce runs at the speed of its worst
//! hop, so a 2% loss on one link of sixteen halves the whole job's throughput
//! while every individual node looks healthy.
//!
//! This module models a collective at the IP layer (per N4 — topology and
//! traffic patterns, not RDMA emulation) and, given the observed per-link
//! throughput, names the culprit. "The obs plane flags the culprit link" is
//! Phase 7's acceptance criterion, and this is the part of it that can be
//! tested without a cluster.

use std::collections::BTreeMap;

use serde::Serialize;

/// What a collective measured on one hop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopSample {
    /// `nodeA-nodeB`, matching how `devbox lab fault` names links.
    pub link: String,
    /// Observed throughput on this hop, in megabits per second.
    pub mbps: f64,
}

/// The verdict on a collective run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Analysis {
    /// The hop that limited the whole operation, if one clearly did.
    pub culprit: Option<String>,
    /// Throughput of the slowest hop.
    pub slowest_mbps: f64,
    /// Median hop throughput, the "healthy" baseline.
    pub median_mbps: f64,
    /// How much slower the culprit is than the median, as a ratio.
    pub severity: f64,
    /// What the collective actually achieved.
    pub effective_mbps: f64,
    /// What it would have achieved with no straggler.
    pub potential_mbps: f64,
}

impl Analysis {
    /// Whether a straggler was found.
    pub fn has_straggler(&self) -> bool {
        self.culprit.is_some()
    }

    /// A sentence a person can act on.
    pub fn explain(&self) -> String {
        match &self.culprit {
            Some(link) => format!(
                "{link} is running at {:.0} Mbps against a median of {:.0} — {:.1}× slower. \
                 The collective is achieving {:.0} Mbps instead of {:.0}.",
                self.slowest_mbps,
                self.median_mbps,
                self.severity,
                self.effective_mbps,
                self.potential_mbps
            ),
            None => format!(
                "No straggler: every hop is within tolerance of the {:.0} Mbps median.",
                self.median_mbps
            ),
        }
    }
}

/// How much slower than the median a hop must be to be called a straggler.
///
/// Chosen so ordinary variance does not raise an alarm but a genuinely
/// impaired link does: a link at half the median is not noise.
pub const STRAGGLER_RATIO: f64 = 1.8;

/// Analyse a collective's per-hop throughput.
///
/// A ring all-reduce completes at the speed of its slowest hop, so the
/// effective throughput *is* the minimum — which is why one bad link is so
/// disproportionately expensive and why the median is the right baseline to
/// compare against.
pub fn analyse(samples: &[HopSample]) -> Analysis {
    if samples.is_empty() {
        return Analysis {
            culprit: None,
            slowest_mbps: 0.0,
            median_mbps: 0.0,
            severity: 1.0,
            effective_mbps: 0.0,
            potential_mbps: 0.0,
        };
    }

    let mut sorted: Vec<f64> = samples.iter().map(|s| s.mbps).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let median = if sorted.len().is_multiple_of(2) {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    };

    let slowest = samples
        .iter()
        .min_by(|a, b| {
            a.mbps
                .partial_cmp(&b.mbps)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("non-empty");

    let severity = if slowest.mbps > 0.0 {
        median / slowest.mbps
    } else {
        f64::INFINITY
    };

    // The collective runs at the slowest hop; without the straggler it would
    // run at the median.
    Analysis {
        culprit: (severity >= STRAGGLER_RATIO).then(|| slowest.link.clone()),
        slowest_mbps: slowest.mbps,
        median_mbps: median,
        severity,
        effective_mbps: slowest.mbps,
        potential_mbps: median,
    }
}

/// Derive per-hop throughput from observed flows.
///
/// The observability plane records bytes and duration per flow; a collective's
/// hops are the flows between adjacent lab nodes. This is the bridge between
/// "the obs plane saw traffic" and "this link is the problem".
pub fn samples_from_flows(flows: &[(String, u64, u64)]) -> Vec<HopSample> {
    let mut by_link: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (link, bytes, ms) in flows {
        let entry = by_link.entry(link.clone()).or_insert((0, 0));
        entry.0 += bytes;
        entry.1 += ms;
    }

    by_link
        .into_iter()
        .map(|(link, (bytes, ms))| HopSample {
            link,
            // bytes → megabits, milliseconds → seconds.
            mbps: if ms == 0 {
                0.0
            } else {
                (bytes as f64 * 8.0 / 1_000_000.0) / (ms as f64 / 1000.0)
            },
        })
        .collect()
}

/// The ring order for a collective across a set of nodes.
///
/// A ring all-reduce sends to its successor and receives from its predecessor,
/// so the hops a collective actually uses are the consecutive pairs — which is
/// what makes it possible to attribute a slowdown to a *link* rather than to a
/// node.
pub fn ring_hops(nodes: &[String]) -> Vec<String> {
    if nodes.len() < 2 {
        return Vec::new();
    }
    (0..nodes.len())
        .map(|i| {
            let next = (i + 1) % nodes.len();
            format!("{}-{}", nodes[i], nodes[next])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(pairs: &[(&str, f64)]) -> Vec<HopSample> {
        pairs
            .iter()
            .map(|(link, mbps)| HopSample {
                link: (*link).into(),
                mbps: *mbps,
            })
            .collect()
    }

    #[test]
    fn a_healthy_collective_has_no_straggler() {
        let a = analyse(&samples(&[
            ("leaf1-spine1", 950.0),
            ("leaf2-spine1", 980.0),
            ("leaf3-spine1", 940.0),
            ("leaf4-spine1", 970.0),
        ]));

        assert!(!a.has_straggler());
        assert!(a.culprit.is_none());
        assert!(a.explain().contains("No straggler"));
    }

    #[test]
    fn one_slow_hop_is_named_and_quantified() {
        // The point of §9.5: one lossy link, everything else fine, and the
        // whole collective runs at the bad link's speed.
        let a = analyse(&samples(&[
            ("leaf1-spine1", 950.0),
            ("leaf2-spine1", 120.0),
            ("leaf3-spine1", 940.0),
            ("leaf4-spine1", 970.0),
        ]));

        assert_eq!(a.culprit.as_deref(), Some("leaf2-spine1"));
        assert!(a.severity > 7.0, "severity was {}", a.severity);
        assert_eq!(
            a.effective_mbps, 120.0,
            "the collective runs at the worst hop"
        );
        assert!(a.potential_mbps > 900.0);

        let text = a.explain();
        assert!(text.contains("leaf2-spine1"), "{text}");
        assert!(text.contains("120"), "{text}");
    }

    #[test]
    fn ordinary_variance_does_not_raise_an_alarm() {
        // A detector that fires on noise is a detector nobody looks at.
        let a = analyse(&samples(&[
            ("a-b", 900.0),
            ("b-c", 1000.0),
            ("c-d", 850.0),
            ("d-a", 1050.0),
        ]));
        assert!(!a.has_straggler(), "severity was {}", a.severity);
    }

    #[test]
    fn a_link_at_half_the_median_is_a_straggler() {
        // The threshold has to sit between "noise" and "obviously broken".
        let a = analyse(&samples(&[
            ("a-b", 1000.0),
            ("b-c", 1000.0),
            ("c-a", 500.0),
        ]));
        assert!(a.has_straggler(), "severity was {}", a.severity);
        assert_eq!(a.culprit.as_deref(), Some("c-a"));
    }

    #[test]
    fn a_dead_hop_is_the_most_severe_case() {
        let a = analyse(&samples(&[("a-b", 1000.0), ("b-c", 1000.0), ("c-a", 0.0)]));
        assert_eq!(a.culprit.as_deref(), Some("c-a"));
        assert!(a.severity.is_infinite());
        assert_eq!(a.effective_mbps, 0.0);
    }

    #[test]
    fn an_empty_run_analyses_to_nothing_rather_than_panicking() {
        let a = analyse(&[]);
        assert!(!a.has_straggler());
        assert_eq!(a.effective_mbps, 0.0);
    }

    #[test]
    fn a_single_hop_cannot_be_a_straggler_against_itself() {
        let a = analyse(&samples(&[("a-b", 100.0)]));
        assert!(!a.has_straggler());
        assert_eq!(a.median_mbps, 100.0);
    }

    #[test]
    fn flows_become_per_hop_throughput() {
        // 12.5 MB in 1 second = 100 Mbps.
        let flows = vec![
            ("leaf1-spine1".to_string(), 12_500_000u64, 1000u64),
            ("leaf2-spine1".to_string(), 125_000_000, 1000),
        ];
        let s = samples_from_flows(&flows);

        assert_eq!(s.len(), 2);
        let leaf1 = s.iter().find(|h| h.link == "leaf1-spine1").unwrap();
        assert!((leaf1.mbps - 100.0).abs() < 0.01, "got {}", leaf1.mbps);
        let leaf2 = s.iter().find(|h| h.link == "leaf2-spine1").unwrap();
        assert!((leaf2.mbps - 1000.0).abs() < 0.01, "got {}", leaf2.mbps);
    }

    #[test]
    fn multiple_flows_on_one_hop_are_aggregated() {
        let flows = vec![
            ("a-b".to_string(), 6_250_000u64, 500u64),
            ("a-b".to_string(), 6_250_000, 500),
        ];
        let s = samples_from_flows(&flows);
        assert_eq!(s.len(), 1, "one hop, however many flows");
        assert!((s[0].mbps - 100.0).abs() < 0.01, "got {}", s[0].mbps);
    }

    #[test]
    fn a_zero_duration_flow_does_not_divide_by_zero() {
        let s = samples_from_flows(&[("a-b".to_string(), 1000, 0)]);
        assert_eq!(s[0].mbps, 0.0);
    }

    #[test]
    fn a_ring_visits_every_node_and_closes() {
        let nodes: Vec<String> = ["n1", "n2", "n3", "n4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let hops = ring_hops(&nodes);

        assert_eq!(hops.len(), 4, "a ring has as many hops as nodes");
        assert_eq!(hops[0], "n1-n2");
        assert_eq!(hops[3], "n4-n1", "the ring closes");
    }

    #[test]
    fn a_ring_needs_at_least_two_nodes() {
        assert!(ring_hops(&[]).is_empty());
        assert!(ring_hops(&["only".to_string()]).is_empty());
    }

    #[test]
    fn the_end_to_end_story_from_observed_flows_to_a_named_link() {
        // What §9.5 actually promises: the obs plane's flow records go in, the
        // culprit link comes out.
        let flows = vec![
            ("leaf1-spine1".to_string(), 125_000_000u64, 1000u64),
            ("leaf2-spine1".to_string(), 125_000_000, 1000),
            ("leaf3-spine1".to_string(), 12_500_000, 1000), // the bad one
            ("leaf4-spine1".to_string(), 125_000_000, 1000),
        ];

        let analysis = analyse(&samples_from_flows(&flows));
        assert_eq!(analysis.culprit.as_deref(), Some("leaf3-spine1"));
        assert!(analysis.explain().contains("leaf3-spine1"));
    }
}
