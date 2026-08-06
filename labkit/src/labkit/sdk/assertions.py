"""Assertions the test SDK makes about a provisioned fabric (§10.3, §10.4).

The observability plane is the oracle: these read the same event store the
console shows, so a test asserting "no egress outside 10.0.0.0/8" is asking
exactly the question an operator would ask, of exactly the same data.

Every function here is pure — it takes observations and returns a verdict — so
the SLO logic is testable without a fabric. Collecting the observations is the
fixture's job.
"""

from __future__ import annotations

import ipaddress
from dataclasses import dataclass
from typing import Literal

#: Provisioning states, in order (§10.2).
ProvisionState = Literal[
    "discovered", "identified", "rendering", "pushing", "verifying", "healthy", "failed"
]

STATES: tuple[ProvisionState, ...] = (
    "discovered",
    "identified",
    "rendering",
    "pushing",
    "verifying",
    "healthy",
    "failed",
)

#: The terminal states.
TERMINAL: frozenset[str] = frozenset({"healthy", "failed"})


@dataclass(frozen=True)
class ProvisionReport:
    """What the ZTP server observed for one node."""

    device: str
    state: ProvisionState
    #: Seconds from first discovery to the terminal state.
    duration_secs: float
    #: How many times the node restarted provisioning.
    attempts: int = 1
    reason: str = ""

    @property
    def healthy(self) -> bool:
        return self.state == "healthy"

    @property
    def terminal(self) -> bool:
        return self.state in TERMINAL


def all_nodes_healthy(reports: list[ProvisionReport]) -> tuple[bool, str]:
    """Whether every node reached `healthy`.

    Returns the verdict and an explanation, so a failing assertion says *which*
    node and *why* rather than just `False`.
    """
    if not reports:
        return False, "no nodes reported at all — did the ZTP server ever see one?"

    unhealthy = [r for r in reports if not r.healthy]
    if not unhealthy:
        return True, f"all {len(reports)} node(s) healthy"

    detail = ", ".join(
        f"{r.device}={r.state}" + (f" ({r.reason})" if r.reason else "") for r in unhealthy
    )
    return False, f"{len(unhealthy)} of {len(reports)} not healthy: {detail}"


def provision_p95(reports: list[ProvisionReport]) -> float:
    """The 95th-percentile provisioning time, in seconds.

    p95 rather than a mean because the SLO that matters is "almost every node
    provisions quickly", and a mean hides a long tail of one node taking five
    minutes.
    """
    durations = sorted(r.duration_secs for r in reports if r.terminal)
    if not durations:
        return 0.0

    # Nearest-rank: the smallest value at or above the 95th percentile.
    rank = max(1, int(-(-len(durations) * 95 // 100)))
    return durations[rank - 1]


def bgp_fully_converged(
    adjacencies: dict[str, list[str]], expected: dict[str, list[str]]
) -> tuple[bool, str]:
    """Whether every expected BGP adjacency is established.

    `adjacencies` is what each device reports as established; `expected` is
    what the topology says it should have.
    """
    if not expected:
        return False, "no adjacencies were expected — is the topology empty?"

    missing: list[str] = []
    for device, peers in expected.items():
        established = set(adjacencies.get(device, []))
        for peer in peers:
            if peer not in established:
                missing.append(f"{device}→{peer}")

    if missing:
        detail = ", ".join(sorted(missing))
        return False, f"{len(missing)} adjacency(ies) not established: {detail}"

    total = sum(len(p) for p in expected.values())
    return True, f"all {total} adjacency(ies) established"


def no_egress_outside(destinations: list[str], allowed: list[str]) -> tuple[bool, str]:
    """Whether every observed destination falls inside an allowed prefix.

    The security assertion from §10.4: a fabric under test should not be
    talking to the internet, and this reads the same connection events the
    console shows.
    """
    if not allowed:
        return False, "no allowed prefixes given; that would permit nothing"

    networks = [ipaddress.ip_network(prefix, strict=False) for prefix in allowed]
    violations: list[str] = []

    for destination in destinations:
        try:
            address = ipaddress.ip_address(destination)
        except ValueError:
            # A destination that is a name, not an address, cannot be checked
            # against a CIDR — and silently passing it would make the
            # assertion meaningless.
            violations.append(f"{destination} (not an address)")
            continue

        if not any(address in network for network in networks):
            violations.append(destination)

    if violations:
        return False, f"egress outside {allowed}: {', '.join(sorted(set(violations)))}"
    return True, f"all {len(destinations)} destination(s) inside {allowed}"


def state_index(state: ProvisionState | str) -> int:
    """Position of a state in the provisioning sequence.

    Used to assert a node never moves backwards through the state machine
    except by restarting, which is what an idempotent retry looks like.
    """
    try:
        return STATES.index(state)
    except ValueError as exc:
        raise ValueError(f"unknown provisioning state {state!r}") from exc


def is_forward_progress(previous: str, current: str) -> bool:
    """Whether a state transition is forward progress or a legitimate retry.

    A node that fails and restarts from `discovered` is the *desired*
    behaviour under chaos (§10.3) — it is how idempotent recovery looks — so
    that is not a regression. Sliding from `verifying` back to `pushing`
    without a restart is.
    """
    if current == "discovered":
        return True  # a restart
    return state_index(current) > state_index(previous)
