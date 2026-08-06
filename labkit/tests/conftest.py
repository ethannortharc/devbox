"""Shared fixtures: the fabric the rest of the tests reason about."""

from __future__ import annotations

from pathlib import Path

import pytest

from labkit.sot import Device, Fabric, Interface


def _leaf(index: int) -> Device:
    """A leaf, dual-homed to both spines."""
    return Device(
        name=f"leaf{index}",
        role="leaf",
        serial=f"SN-LEAF-{index:03d}",
        interfaces=[
            Interface(name="eth1", peer=f"spine1:eth{index}"),
            Interface(name="eth2", peer=f"spine2:eth{index}"),
        ],
    )


def _spine(index: int, leaves: int) -> Device:
    return Device(
        name=f"spine{index}",
        role="spine",
        serial=f"SN-SPINE-{index:03d}",
        interfaces=[
            Interface(name=f"eth{n}", peer=f"leaf{n}:eth{index}") for n in range(1, leaves + 1)
        ],
    )


@pytest.fixture
def fabric() -> Fabric:
    """A 2x2 fat-tree: two leaves, two spines, every leaf dual-homed.

    Dual-homing is the point — it is what gives every device more than one
    adjacency, so convergence and ECMP assertions have something to measure.
    """
    return Fabric(
        name="ztp-fabric",
        devices=[_spine(1, 2), _spine(2, 2), _leaf(1), _leaf(2)],
    )


@pytest.fixture
def golden_dir(request: pytest.FixtureRequest) -> Path:
    """Directory holding the golden config files."""
    return Path(request.path).parent / "golden"
