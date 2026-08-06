"""Config generation: deterministic, idempotent, and golden-file tested."""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from jinja2 import UndefinedError

from labkit.gen import diff, render_all, render_device
from labkit.sot import Fabric, allocate


def test_every_router_gets_a_config(fabric: Fabric) -> None:
    configs = render_all(fabric, allocate(fabric))

    assert set(configs) == {d.name for d in fabric.routers}
    for name, config in configs.items():
        assert f"hostname {name}" in config
        assert "router bgp" in config


def test_rendering_is_deterministic(fabric: Fabric) -> None:
    # Without this a golden file means nothing and every diff shows churn.
    allocation = allocate(fabric)
    assert render_all(fabric, allocation) == render_all(fabric, allocation)
    assert render_all(fabric, allocate(fabric)) == render_all(fabric, allocate(fabric))


def test_a_config_carries_its_identity(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    leaf = fabric.device("leaf1")
    assert leaf is not None

    config = render_device(fabric, allocation, leaf)
    asn = allocation.asns["leaf1"]
    loopback = allocation.loopbacks["leaf1"].split("/")[0]

    assert f"router bgp {asn}" in config
    assert f"bgp router-id {loopback}" in config
    assert "maximum-paths 8" in config, "ECMP is the point of a fat-tree"
    assert "no bgp ebgp-requires-policy" in config, (
        "without this, modern FRR reports BGP up and advertises nothing"
    )


def test_peering_is_unnumbered_over_every_link(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    leaf = fabric.device("leaf1")
    assert leaf is not None

    config = render_device(fabric, allocation, leaf)
    assert "neighbor eth1 interface remote-as external" in config
    assert "neighbor eth2 interface remote-as external" in config
    assert "neighbor 10." not in config, "unnumbered means no addressed peers"


def test_a_missing_variable_is_a_loud_error_not_a_blank(fabric: Fabric) -> None:
    # StrictUndefined: a typo'd variable must not silently ship a config with
    # a blank where a router-id belonged.
    from jinja2 import Environment, StrictUndefined

    env = Environment(undefined=StrictUndefined, autoescape=False)
    with pytest.raises(UndefinedError):
        env.from_string("router bgp {{ missing }}").render()


# ── golden files ─────────────────────────────────────────


def _golden_path(golden_dir: Path, device: str) -> Path:
    return golden_dir / f"{device}.conf"


def test_configs_match_their_golden_files(fabric: Fabric, golden_dir: Path) -> None:
    """The generated configs are byte-identical to the checked-in ones.

    Set ``UPDATE_GOLDEN=1`` to rewrite them after an intentional change; the
    diff in review is then the whole point.
    """
    configs = render_all(fabric, allocate(fabric))
    golden_dir.mkdir(exist_ok=True)

    if os.environ.get("UPDATE_GOLDEN"):
        for device, config in configs.items():
            _golden_path(golden_dir, device).write_text(config)
        pytest.skip("golden files rewritten")

    for device, config in sorted(configs.items()):
        path = _golden_path(golden_dir, device)
        assert path.exists(), (
            f"no golden file for {device}; run with UPDATE_GOLDEN=1 to create it"
        )
        assert config == path.read_text(), (
            f"{device}'s config changed; review the diff, then rerun with "
            "UPDATE_GOLDEN=1 if it is intended"
        )


# ── the render → diff → apply → verify cycle ─────────────


def test_a_config_applied_twice_produces_no_second_change(fabric: Fabric) -> None:
    """Idempotency: the property that makes reconciliation safe.

    A generator that is not idempotent turns every reconciliation loop into a
    config churn loop, and every diff into noise.
    """
    configs = render_all(fabric, allocate(fabric))
    leaf = configs["leaf1"]

    # First apply: the device was blank, so everything is a change.
    first = diff("leaf1", running="", rendered=leaf)
    assert first.changed

    # Second apply: the running config *is* the rendered one.
    second = diff("leaf1", running=leaf, rendered=leaf)
    assert not second.changed, f"applying twice changed something:\n{second}"


def test_a_real_change_shows_up_in_the_diff(fabric: Fabric) -> None:
    configs = render_all(fabric, allocate(fabric))
    running = configs["leaf1"].replace("maximum-paths 8", "maximum-paths 1")

    result = diff("leaf1", running=running, rendered=configs["leaf1"])
    assert result.changed
    assert "maximum-paths" in str(result)
    assert "leaf1 (running)" in str(result)


def test_comment_churn_is_not_a_change(fabric: Fabric) -> None:
    # A generated banner naming the render time would otherwise make every
    # config look changed, every time — which trains people to stop reading
    # diffs at all.
    configs = render_all(fabric, allocate(fabric))
    running = "! generated at some other time\n" + configs["leaf1"]

    assert not diff("leaf1", running=running, rendered=configs["leaf1"]).changed


def test_whitespace_alone_is_not_a_change(fabric: Fabric) -> None:
    configs = render_all(fabric, allocate(fabric))
    running = configs["leaf1"].replace("\n", "  \n")

    assert not diff("leaf1", running=running, rendered=configs["leaf1"]).changed
