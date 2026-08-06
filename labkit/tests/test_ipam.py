"""IPAM properties: deterministic, total, and never overlapping."""

from __future__ import annotations

import ipaddress

import pytest

from labkit.sot import Device, Fabric, Interface, OverlapError, allocate, verify
from labkit.sot.models import AddressPlan


def test_every_link_and_router_gets_addressed(fabric: Fabric) -> None:
    allocation = allocate(fabric)

    assert len(allocation.links) == len(fabric.links())
    assert len(allocation.loopbacks) == len(fabric.routers)
    assert len(allocation.asns) == len(fabric.routers)


def test_links_get_p31_subnets(fabric: Fabric) -> None:
    # RFC 3021: two usable addresses instead of two wasted ones.
    for link in allocate(fabric).links:
        assert ipaddress.IPv4Network(link.subnet).prefixlen == 31
        assert len(link.ends) == 2


def test_allocation_is_deterministic(fabric: Fabric) -> None:
    # This is what makes a golden-file config test possible at all, and what
    # makes a re-provision after a wipe produce the same fabric.
    first = allocate(fabric)
    second = allocate(fabric)

    assert first.links == second.links
    assert first.loopbacks == second.loopbacks
    assert first.asns == second.asns


def test_no_address_is_ever_assigned_twice(fabric: Fabric) -> None:
    addresses = allocate(fabric).all_addresses()
    assert len(addresses) == len(set(addresses))


def test_verify_catches_a_hand_built_collision(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    first, second = allocation.links[0], allocation.links[1]
    # Force the second link onto the first's subnet.
    allocation.links[1] = type(second)(subnet=first.subnet, ends=second.ends)

    with pytest.raises(OverlapError, match="overlap"):
        verify(allocation)


def test_loopbacks_and_links_come_from_separate_pools(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    plan = fabric.address_plan

    p2p = ipaddress.IPv4Network(plan.p2p_base)
    loopbacks = ipaddress.IPv4Network(plan.loopback_base)

    for link in allocation.links:
        assert ipaddress.IPv4Network(link.subnet).subnet_of(p2p)
    for address in allocation.loopbacks.values():
        assert ipaddress.IPv4Interface(address).ip in loopbacks


def test_explicit_values_win(fabric: Fabric) -> None:
    explicit = Fabric(
        name="explicit",
        devices=[
            Device(
                name="a",
                role="leaf",
                serial="S1",
                asn=64512,
                loopback="10.255.9.9/32",
                interfaces=[Interface(name="eth1", peer="b:eth1", address="192.0.2.0/31")],
            ),
            Device(
                name="b",
                role="spine",
                serial="S2",
                interfaces=[Interface(name="eth1", peer="a:eth1")],
            ),
        ],
    )
    allocation = allocate(explicit)

    assert allocation.asns["a"] == 64512
    assert allocation.loopbacks["a"] == "10.255.9.9/32"
    assert allocation.links[0].subnet == "192.0.2.0/31"
    # And the derived ones still get values.
    assert "b" in allocation.asns
    assert "b" in allocation.loopbacks


def test_peers_resolve_in_both_directions(fabric: Fabric) -> None:
    allocation = allocate(fabric)

    near = "leaf1:eth1"
    peer = allocation.peer_of(near)
    assert peer is not None
    far, far_address = peer
    assert far == "spine1:eth1"

    back = allocation.peer_of(far)
    assert back is not None
    assert back[0] == near

    assert allocation.address_of(far) == far_address
    assert allocation.address_of("leaf1:eth99") is None


def test_a_pool_with_no_room_fails_loudly() -> None:
    tight = Fabric(
        name="tight",
        devices=[
            Device(
                name="a",
                role="leaf",
                serial="S1",
                interfaces=[
                    Interface(name="eth1", peer="b:eth1"),
                    Interface(name="eth2", peer="b:eth2"),
                ],
            ),
            Device(
                name="b",
                role="spine",
                serial="S2",
                interfaces=[
                    Interface(name="eth1", peer="a:eth1"),
                    Interface(name="eth2", peer="a:eth2"),
                ],
            ),
        ],
        address_plan=AddressPlan(p2p_base="10.0.0.0/31", loopback_base="10.255.0.0/24"),
    )

    # One /31 of space, two links wanted.
    with pytest.raises(OverlapError, match="no room"):
        allocate(tight)


def test_asns_are_handed_out_in_order_skipping_explicit_ones(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    assigned = sorted(allocation.asns.values())

    assert assigned == list(range(65000, 65000 + len(fabric.routers)))
    assert len(set(assigned)) == len(assigned)


def test_an_explicit_address_stays_at_the_end_that_declared_it() -> None:
    # "Explicit values win" has to mean the address lands where it was
    # declared, not merely that its subnet was used.
    fabric = Fabric(
        name="explicit",
        devices=[
            Device(
                name="a",
                role="leaf",
                serial="S1",
                interfaces=[Interface(name="eth1", peer="b:eth1", address="10.9.0.2/31")],
            ),
            Device(
                name="b",
                role="spine",
                serial="S2",
                interfaces=[Interface(name="eth1", peer="a:eth1")],
            ),
        ],
    )
    allocation = allocate(fabric)

    assert allocation.address_of("a:eth1") == "10.9.0.2/31"
    assert allocation.address_of("b:eth1") == "10.9.0.3/31"


def test_both_ends_declaring_different_subnets_is_rejected() -> None:
    fabric = Fabric(
        name="conflict",
        devices=[
            Device(
                name="a",
                role="leaf",
                serial="S1",
                interfaces=[Interface(name="eth1", peer="b:eth1", address="10.9.0.0/31")],
            ),
            Device(
                name="b",
                role="spine",
                serial="S2",
                interfaces=[Interface(name="eth1", peer="a:eth1", address="10.9.4.0/31")],
            ),
        ],
    )
    with pytest.raises(OverlapError, match="same subnet"):
        allocate(fabric)


def test_a_derived_asn_never_lands_on_an_explicit_one() -> None:
    # Two intended eBGP peers sharing an AS do not peer at all, so the fabric
    # comes up and never converges.
    fabric = Fabric(
        name="asn",
        devices=[
            Device(
                name="a",
                role="leaf",
                serial="S1",
                asn=65000,
                interfaces=[Interface(name="eth1", peer="b:eth1")],
            ),
            Device(
                name="b",
                role="spine",
                serial="S2",
                interfaces=[Interface(name="eth1", peer="a:eth1")],
            ),
        ],
    )
    allocation = allocate(fabric)

    assert allocation.asns["a"] == 65000
    assert allocation.asns["b"] != 65000
    assert len(set(allocation.asns.values())) == len(allocation.asns)


def test_verify_catches_duplicate_asns(fabric: Fabric) -> None:
    allocation = allocate(fabric)
    first = next(iter(allocation.asns))
    second = next(d for d in allocation.asns if d != first)
    allocation.asns[second] = allocation.asns[first]

    with pytest.raises(OverlapError, match="share AS"):
        verify(allocation)
