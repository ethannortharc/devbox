"""The assertions the ZTP tests actually make (§10.3, §10.4)."""

from __future__ import annotations

import itertools

import pytest

from labkit.sdk import (
    ProvisionReport,
    all_nodes_healthy,
    bgp_fully_converged,
    no_egress_outside,
    provision_p95,
)
from labkit.sdk.assertions import is_forward_progress, state_index


def report(device: str, state: str, secs: float, **kwargs: object) -> ProvisionReport:
    return ProvisionReport(device=device, state=state, duration_secs=secs, **kwargs)  # type: ignore[arg-type]


# ── all_nodes_healthy ────────────────────────────────────


def test_a_fully_provisioned_fabric_passes() -> None:
    ok, why = all_nodes_healthy(
        [report("leaf1", "healthy", 40), report("leaf2", "healthy", 44)]
    )
    assert ok
    assert "2 node(s) healthy" in why


def test_a_failed_node_is_named_and_explained() -> None:
    # A failing assertion has to say *which* node and *why*, or the test is
    # just as opaque as the fabric.
    ok, why = all_nodes_healthy(
        [
            report("leaf1", "healthy", 40),
            report("leaf2", "failed", 90, reason="config push timed out"),
        ]
    )
    assert not ok
    assert "leaf2=failed" in why
    assert "config push timed out" in why


def test_a_node_stuck_mid_provision_is_not_healthy() -> None:
    ok, why = all_nodes_healthy([report("leaf1", "pushing", 120)])
    assert not ok
    assert "pushing" in why


def test_no_reports_at_all_fails_rather_than_vacuously_passing() -> None:
    # "Zero of zero nodes are unhealthy" must not read as success — it almost
    # always means the ZTP server never saw anything.
    ok, why = all_nodes_healthy([])
    assert not ok
    assert "no nodes reported" in why


# ── provision_p95 ────────────────────────────────────────


def test_p95_is_the_tail_not_the_average() -> None:
    # Eighteen fast nodes and two slow ones. The mean is 43s; p95 is 300s, and
    # the SLO cares about the second.
    reports = [report(f"n{i}", "healthy", 15.0) for i in range(18)]
    reports += [report("slow1", "healthy", 300.0), report("slow2", "healthy", 300.0)]

    assert provision_p95(reports) == 300.0
    mean = sum(r.duration_secs for r in reports) / len(reports)
    assert mean < 50, "the mean would have hidden this entirely"


def test_a_single_outlier_in_twenty_does_not_move_p95() -> None:
    # Nearest-rank p95 of 20 samples is the 19th, so one slow node is exactly
    # the case p95 is designed to tolerate. Worth pinning: it is the most
    # common misreading of the metric.
    reports = [report(f"n{i}", "healthy", 15.0) for i in range(19)]
    reports.append(report("slow", "healthy", 300.0))

    assert provision_p95(reports) == 15.0


def test_p95_of_a_uniform_fabric_is_that_uniform_value() -> None:
    reports = [report(f"n{i}", "healthy", 42.0) for i in range(10)]
    assert provision_p95(reports) == 42.0


def test_p95_ignores_nodes_that_have_not_finished() -> None:
    reports = [
        report("done", "healthy", 20.0),
        report("still-going", "pushing", 9999.0),
    ]
    assert provision_p95(reports) == 20.0


def test_p95_of_nothing_is_zero_not_a_crash() -> None:
    assert provision_p95([]) == 0.0


# ── bgp_fully_converged ──────────────────────────────────


def test_convergence_passes_when_every_expected_adjacency_is_up() -> None:
    ok, why = bgp_fully_converged(
        adjacencies={"leaf1": ["spine1", "spine2"], "spine1": ["leaf1"]},
        expected={"leaf1": ["spine1", "spine2"], "spine1": ["leaf1"]},
    )
    assert ok
    assert "3 adjacency" in why


def test_a_missing_adjacency_is_named() -> None:
    ok, why = bgp_fully_converged(
        adjacencies={"leaf1": ["spine1"]},
        expected={"leaf1": ["spine1", "spine2"]},
    )
    assert not ok
    assert "leaf1→spine2" in why


def test_expecting_nothing_fails_rather_than_passing_vacuously() -> None:
    ok, why = bgp_fully_converged(adjacencies={}, expected={})
    assert not ok
    assert "no adjacencies were expected" in why


# ── no_egress_outside ────────────────────────────────────


def test_traffic_inside_the_fabric_passes() -> None:
    ok, _ = no_egress_outside(["10.0.12.1", "10.255.0.3", "10.0.0.9"], allowed=["10.0.0.0/8"])
    assert ok


def test_a_single_escape_fails_and_is_named() -> None:
    # The security assertion from §10.4: a fabric under test should not be
    # talking to the internet.
    ok, why = no_egress_outside(["10.0.12.1", "151.101.0.223"], allowed=["10.0.0.0/8"])
    assert not ok
    assert "151.101.0.223" in why


def test_a_destination_that_is_not_an_address_is_a_violation() -> None:
    # Silently skipping unparseable destinations would make the assertion
    # meaningless exactly when it matters.
    ok, why = no_egress_outside(["telemetry.example.com"], allowed=["10.0.0.0/8"])
    assert not ok
    assert "not an address" in why


def test_an_empty_allowlist_fails_rather_than_permitting_everything() -> None:
    ok, why = no_egress_outside(["10.0.0.1"], allowed=[])
    assert not ok
    assert "permit nothing" in why


def test_multiple_prefixes_are_all_honoured() -> None:
    ok, _ = no_egress_outside(
        ["10.0.0.1", "192.168.1.1"], allowed=["10.0.0.0/8", "192.168.0.0/16"]
    )
    assert ok


# ── state machine ────────────────────────────────────────


def test_states_are_ordered() -> None:
    assert state_index("discovered") < state_index("identified")
    assert state_index("verifying") < state_index("healthy")
    with pytest.raises(ValueError, match="unknown provisioning state"):
        state_index("nonsense")


def test_a_restart_counts_as_forward_progress() -> None:
    # Under chaos (§10.3) a node that fails and starts over from `discovered`
    # is the *desired* behaviour — it is what idempotent recovery looks like.
    assert is_forward_progress("failed", "discovered")
    assert is_forward_progress("verifying", "discovered")


def test_sliding_backwards_without_a_restart_is_not_progress() -> None:
    assert not is_forward_progress("verifying", "pushing")
    assert not is_forward_progress("healthy", "verifying")
    assert not is_forward_progress("pushing", "pushing")


def test_normal_advancement_is_progress() -> None:
    states = [
        "discovered",
        "identified",
        "rendering",
        "pushing",
        "verifying",
        "healthy",
    ]
    for previous, current in itertools.pairwise(states):
        assert is_forward_progress(previous, current), f"{previous} → {current}"
