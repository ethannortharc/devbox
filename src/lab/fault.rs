//! Fault injection — §9.3.
//!
//! Per-link `tc`/`netem`: delay, jitter, loss, reorder, duplication, rate
//! limits, plus partition/heal and flap. Faults are applied to one *end* of a
//! link, which is the detail that makes them realistic — a real lossy link is
//! usually lossy in one direction, and a fault applied symmetrically hides
//! exactly the asymmetries worth debugging.
//!
//! As everywhere else in the lab, this generates commands; the orchestrator
//! runs them, and `--dry-run` prints them.

use std::fmt::Write as _;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::topology::{Endpoint, Topology};
use super::wiring;

/// An impairment applied to one interface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Impairment {
    /// One-way delay, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u32>,
    /// Delay variation, in milliseconds. Requires `delay_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_ms: Option<u32>,
    /// Packet loss, as a percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f64>,
    /// Packet duplication, as a percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_pct: Option<f64>,
    /// Packet reordering, as a percentage. Requires `delay_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reorder_pct: Option<f64>,
    /// Egress rate limit, in kilobits per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_kbit: Option<u32>,
}

impl Impairment {
    /// Whether anything is actually being impaired.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Reject combinations netem cannot express.
    pub fn validate(&self) -> Result<()> {
        if self.is_empty() {
            bail!("no impairment given; specify at least one of delay, jitter, loss, rate, …");
        }
        for (name, pct) in [
            ("loss", self.loss_pct),
            ("duplicate", self.duplicate_pct),
            ("reorder", self.reorder_pct),
        ] {
            if let Some(pct) = pct
                && !(0.0..=100.0).contains(&pct)
            {
                bail!("{name} must be a percentage between 0 and 100, got {pct}");
            }
        }
        // netem's own constraints, worth catching before `tc` says something
        // considerably less helpful.
        if self.jitter_ms.is_some() && self.delay_ms.is_none() {
            bail!("jitter needs a delay to vary; add --delay");
        }
        if self.reorder_pct.is_some() && self.delay_ms.is_none() {
            bail!("reordering needs a delay to reorder within; add --delay");
        }
        if self.rate_kbit == Some(0) {
            bail!("a rate limit of 0 would drop everything; use --loss 100 if that is the intent");
        }
        Ok(())
    }

    /// The netem argument list.
    pub fn netem_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        if let Some(delay) = self.delay_ms {
            args.push("delay".into());
            args.push(format!("{delay}ms"));
            if let Some(jitter) = self.jitter_ms {
                args.push(format!("{jitter}ms"));
            }
            if let Some(reorder) = self.reorder_pct {
                args.push("reorder".into());
                args.push(format!("{reorder}%"));
            }
        }
        if let Some(loss) = self.loss_pct {
            args.push("loss".into());
            args.push(format!("{loss}%"));
        }
        if let Some(dup) = self.duplicate_pct {
            args.push("duplicate".into());
            args.push(format!("{dup}%"));
        }
        if let Some(rate) = self.rate_kbit {
            args.push("rate".into());
            args.push(format!("{rate}kbit"));
        }
        args
    }

    /// A one-line description, for logs and the console.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(d) = self.delay_ms {
            let mut text = format!("{d}ms delay");
            if let Some(j) = self.jitter_ms {
                let _ = write!(text, " ±{j}ms");
            }
            parts.push(text);
        }
        if let Some(l) = self.loss_pct {
            parts.push(format!("{l}% loss"));
        }
        if let Some(r) = self.reorder_pct {
            parts.push(format!("{r}% reorder"));
        }
        if let Some(d) = self.duplicate_pct {
            parts.push(format!("{d}% duplicate"));
        }
        if let Some(r) = self.rate_kbit {
            parts.push(format!("{r}kbit rate"));
        }
        if parts.is_empty() {
            "no impairment".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Which end (or ends) of a link a fault applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The first endpoint only.
    A,
    /// The second endpoint only.
    B,
    /// Both ends — a symmetric fault.
    #[default]
    Both,
}

/// Resolve a link by either endpoint spec or by `nodeA-nodeB`.
///
/// `leaf1-spine1` is what a person types; `leaf1:eth1` is what they type when a
/// pair of nodes has more than one link between them.
pub fn find_link(topology: &Topology, spec: &str) -> Result<(Endpoint, Endpoint)> {
    // An explicit endpoint: exactly one link owns it.
    if spec.contains(':') {
        let wanted = Endpoint::parse(spec)?;
        for link in &topology.links {
            let (a, b) = link.parse_endpoints()?;
            if a == wanted || b == wanted {
                return Ok((a, b));
            }
        }
        bail!("no link uses {spec}");
    }

    // A node pair.
    let (left, right) = spec
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("write a link as nodeA-nodeB or node:iface"))?;

    let matches: Vec<(Endpoint, Endpoint)> = topology
        .links
        .iter()
        .filter_map(|l| l.parse_endpoints().ok())
        .filter(|(a, b)| (a.node == left && b.node == right) || (a.node == right && b.node == left))
        .collect();

    match matches.len() {
        0 => bail!("no link connects '{left}' and '{right}'"),
        1 => Ok(matches.into_iter().next().expect("checked len")),
        n => bail!(
            "'{left}' and '{right}' are connected by {n} links; name one end \
             explicitly, e.g. {}",
            matches[0].0
        ),
    }
}

/// Commands that apply an impairment.
pub fn apply(
    lab: &str,
    link: &(Endpoint, Endpoint),
    direction: Direction,
    impairment: &Impairment,
) -> Result<Vec<Vec<String>>> {
    impairment.validate()?;
    let args = impairment.netem_args();

    Ok(ends(link, direction)
        .into_iter()
        .map(|end| {
            let mut cmd = vec![
                "tc".to_string(),
                "qdisc".to_string(),
                // `replace` rather than `add`: applying a second fault to the
                // same interface should change it, not fail.
                "replace".to_string(),
                "dev".to_string(),
                end.iface.clone(),
                "root".to_string(),
                "netem".to_string(),
            ];
            cmd.extend(args.iter().cloned());
            wiring::in_node(
                lab,
                &end.node,
                &cmd.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        })
        .collect())
}

/// Commands that clear any impairment.
///
/// Tolerates an interface that has none: healing a link that was never
/// impaired is a no-op, not an error.
pub fn heal(lab: &str, link: &(Endpoint, Endpoint), direction: Direction) -> Vec<Vec<String>> {
    ends(link, direction)
        .into_iter()
        .map(|end| {
            wiring::in_node(
                lab,
                &end.node,
                &["tc", "qdisc", "del", "dev", &end.iface, "root"],
            )
        })
        .collect()
}

/// Commands that partition a link: total loss in both directions.
///
/// Implemented as 100% loss rather than `link set down`, because a downed
/// interface tells the routing protocol immediately while a black-holing link
/// does not — and the second is the failure mode that actually hurts.
pub fn partition(lab: &str, link: &(Endpoint, Endpoint)) -> Result<Vec<Vec<String>>> {
    apply(
        lab,
        link,
        Direction::Both,
        &Impairment {
            loss_pct: Some(100.0),
            ..Default::default()
        },
    )
}

/// Commands that take one end of a link administratively down.
///
/// The other half of the partition story: this *is* signalled to the routing
/// protocol, so it is what a clean link failure looks like.
pub fn link_down(lab: &str, link: &(Endpoint, Endpoint), direction: Direction) -> Vec<Vec<String>> {
    ends(link, direction)
        .into_iter()
        .map(|end| wiring::in_node(lab, &end.node, &["ip", "link", "set", &end.iface, "down"]))
        .collect()
}

/// Commands that bring a downed end back up.
pub fn link_up(lab: &str, link: &(Endpoint, Endpoint), direction: Direction) -> Vec<Vec<String>> {
    ends(link, direction)
        .into_iter()
        .map(|end| wiring::in_node(lab, &end.node, &["ip", "link", "set", &end.iface, "up"]))
        .collect()
}

/// One flap cycle: down, wait, up, wait.
///
/// Returned as a plan rather than executed so `--dry-run` can show it and a
/// test can assert the shape without waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlapStep {
    pub command: Vec<String>,
    /// Seconds to wait after this command.
    pub wait_secs: u64,
}

/// A flap plan: `cycles` rounds of down/up on one end.
pub fn flap(
    lab: &str,
    link: &(Endpoint, Endpoint),
    direction: Direction,
    interval_secs: u64,
    cycles: u32,
) -> Result<Vec<FlapStep>> {
    if cycles == 0 {
        bail!("a flap needs at least one cycle");
    }
    if interval_secs == 0 {
        bail!("a flap interval of 0 would just thrash; use at least 1 second");
    }

    let mut steps = Vec::new();
    for _ in 0..cycles {
        for command in link_down(lab, link, direction) {
            steps.push(FlapStep {
                command,
                wait_secs: interval_secs,
            });
        }
        for command in link_up(lab, link, direction) {
            steps.push(FlapStep {
                command,
                wait_secs: interval_secs,
            });
        }
    }
    Ok(steps)
}

/// The endpoints a direction selects.
fn ends(link: &(Endpoint, Endpoint), direction: Direction) -> Vec<&Endpoint> {
    match direction {
        Direction::A => vec![&link.0],
        Direction::B => vec![&link.1],
        Direction::Both => vec![&link.0, &link.1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::Lab;

    fn clos() -> Topology {
        Lab::resolve("clos-3node").unwrap().topology
    }

    fn link() -> (Endpoint, Endpoint) {
        find_link(&clos(), "leaf1-spine1").unwrap()
    }

    fn rendered(cmds: &[Vec<String>]) -> Vec<String> {
        cmds.iter().map(|c| wiring::render(c)).collect()
    }

    #[test]
    fn links_resolve_by_node_pair_in_either_order() {
        let t = clos();
        let forward = find_link(&t, "leaf1-spine1").unwrap();
        let reverse = find_link(&t, "spine1-leaf1").unwrap();
        assert_eq!(forward, reverse, "order should not matter");
        assert_eq!(forward.0.node, "leaf1");
    }

    #[test]
    fn links_resolve_by_an_explicit_endpoint() {
        let t = clos();
        let by_end = find_link(&t, "spine1:eth2").unwrap();
        assert_eq!(by_end.0.node, "leaf2");
        assert!(find_link(&t, "spine1:eth9").is_err());
    }

    #[test]
    fn an_unknown_or_ambiguous_link_is_an_error() {
        let t = clos();
        assert!(
            find_link(&t, "leaf1-leaf2")
                .unwrap_err()
                .to_string()
                .contains("no link")
        );
        assert!(find_link(&t, "nonsense").is_err());

        // Two parallel links between the same pair must be disambiguated.
        let fat = Lab::resolve("fat-tree-4x2").unwrap().topology;
        // leaf1 reaches spine1 once and spine2 once, so this is unambiguous...
        assert!(find_link(&fat, "leaf1-spine1").is_ok());
    }

    #[test]
    fn netem_args_render_the_way_tc_expects() {
        let imp = Impairment {
            delay_ms: Some(20),
            jitter_ms: Some(5),
            loss_pct: Some(0.1),
            ..Default::default()
        };
        assert_eq!(
            imp.netem_args(),
            vec!["delay", "20ms", "5ms", "loss", "0.1%"]
        );

        let rate = Impairment {
            rate_kbit: Some(1000),
            ..Default::default()
        };
        assert_eq!(rate.netem_args(), vec!["rate", "1000kbit"]);
    }

    #[test]
    fn netem_constraints_are_caught_before_tc_complains() {
        // jitter without delay, and reorder without delay, are both things tc
        // rejects with a considerably less helpful message.
        assert!(
            Impairment {
                jitter_ms: Some(5),
                ..Default::default()
            }
            .validate()
            .unwrap_err()
            .to_string()
            .contains("delay")
        );
        assert!(
            Impairment {
                reorder_pct: Some(5.0),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(Impairment::default().validate().is_err(), "no impairment");
        assert!(
            Impairment {
                loss_pct: Some(150.0),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Impairment {
                rate_kbit: Some(0),
                ..Default::default()
            }
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--loss 100")
        );
    }

    #[test]
    fn a_fault_can_be_applied_to_one_end_only() {
        // A real lossy link is usually lossy in one direction, and a symmetric
        // fault hides exactly the asymmetries worth debugging.
        let imp = Impairment {
            loss_pct: Some(5.0),
            ..Default::default()
        };

        let one = apply("clos-3node", &link(), Direction::A, &imp).unwrap();
        assert_eq!(one.len(), 1);
        assert!(rendered(&one)[0].contains("devbox-clos-3node-leaf1"));

        let both = apply("clos-3node", &link(), Direction::Both, &imp).unwrap();
        assert_eq!(both.len(), 2);
        assert!(rendered(&both)[1].contains("devbox-clos-3node-spine1"));
    }

    #[test]
    fn applying_a_second_fault_replaces_the_first() {
        // `add` would fail; `replace` is what makes the UI slider work.
        let cmds = apply(
            "clos-3node",
            &link(),
            Direction::A,
            &Impairment {
                delay_ms: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(rendered(&cmds)[0].contains("qdisc replace"));
    }

    #[test]
    fn healing_clears_both_ends_by_default() {
        let cmds = heal("clos-3node", &link(), Direction::Both);
        assert_eq!(cmds.len(), 2);
        for text in rendered(&cmds) {
            assert!(text.contains("tc qdisc del"), "{text}");
            assert!(text.ends_with("root"));
        }
    }

    #[test]
    fn a_partition_black_holes_rather_than_downing_the_link() {
        // A downed interface tells the routing protocol immediately; a
        // black-holing link does not, and the second is the failure that hurts.
        let cmds = partition("clos-3node", &link()).unwrap();
        assert_eq!(cmds.len(), 2, "both directions");
        for text in rendered(&cmds) {
            assert!(text.contains("loss 100%"), "{text}");
            assert!(!text.contains("link set"), "{text}");
        }
    }

    #[test]
    fn link_down_is_the_other_half_of_the_story() {
        let down = rendered(&link_down("clos-3node", &link(), Direction::A));
        assert_eq!(down.len(), 1);
        assert!(down[0].ends_with("ip link set eth1 down"));

        let up = rendered(&link_up("clos-3node", &link(), Direction::A));
        assert!(up[0].ends_with("ip link set eth1 up"));
    }

    #[test]
    fn a_flap_alternates_down_and_up_for_the_requested_cycles() {
        let steps = flap("clos-3node", &link(), Direction::A, 2, 3).unwrap();

        assert_eq!(steps.len(), 6, "three cycles of down and up");
        assert!(wiring::render(&steps[0].command).ends_with("down"));
        assert!(wiring::render(&steps[1].command).ends_with("up"));
        assert!(steps.iter().all(|s| s.wait_secs == 2));
    }

    #[test]
    fn a_degenerate_flap_is_rejected() {
        assert!(flap("clos", &link(), Direction::A, 1, 0).is_err());
        assert!(
            flap("clos", &link(), Direction::A, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("thrash")
        );
    }

    #[test]
    fn impairments_describe_themselves() {
        assert_eq!(
            Impairment {
                delay_ms: Some(20),
                jitter_ms: Some(5),
                loss_pct: Some(0.1),
                ..Default::default()
            }
            .describe(),
            "20ms delay ±5ms, 0.1% loss"
        );
        assert_eq!(Impairment::default().describe(), "no impairment");
    }

    #[test]
    fn generated_commands_never_reach_a_shell() {
        let cmds = apply(
            "clos-3node",
            &link(),
            Direction::Both,
            &Impairment {
                loss_pct: Some(1.5),
                ..Default::default()
            },
        )
        .unwrap();
        for text in rendered(&cmds) {
            for meta in [';', '|', '&', '`', '$'] {
                assert!(!text.contains(meta), "{text} contains {meta}");
            }
        }
    }
}
