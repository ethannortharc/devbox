"""The source of truth must refuse anything downstream cannot trust."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from labkit.sot import AddressPlan, Device, Fabric, Interface


def test_a_coherent_fabric_validates(fabric: Fabric) -> None:
    assert len(fabric.devices) == 4
    assert len(fabric.routers) == 4
    assert fabric.device("leaf1") is not None
    assert fabric.device("nobody") is None


def test_devices_are_found_by_serial(fabric: Fabric) -> None:
    # This is the ZTP "who am I?" query (§10.1); everything downstream of a
    # blank node booting depends on it.
    device = fabric.by_serial("SN-LEAF-001")
    assert device is not None
    assert device.name == "leaf1"
    assert fabric.by_serial("SN-NOBODY") is None


def test_duplicate_serials_are_rejected() -> None:
    # Two devices sharing a serial means ZTP would provision one as the other,
    # which is the single worst thing this model can allow.
    with pytest.raises(ValidationError, match="share serial"):
        Fabric(
            name="broken",
            devices=[
                Device(name="a", role="leaf", serial="SAME"),
                Device(name="b", role="leaf", serial="SAME"),
            ],
        )


def test_duplicate_device_names_are_rejected() -> None:
    with pytest.raises(ValidationError, match="both called"):
        Fabric(
            name="broken",
            devices=[
                Device(name="a", role="leaf", serial="S1"),
                Device(name="a", role="leaf", serial="S2"),
            ],
        )


def test_a_link_to_a_device_that_does_not_exist_is_rejected() -> None:
    with pytest.raises(ValidationError, match="unknown device"):
        Fabric(
            name="broken",
            devices=[
                Device(
                    name="a",
                    role="leaf",
                    serial="S1",
                    interfaces=[Interface(name="eth1", peer="ghost:eth1")],
                )
            ],
        )


def test_links_must_be_mutual() -> None:
    # A one-sided link is a typo that would otherwise produce a fabric that
    # half-converges and takes an hour to explain.
    with pytest.raises(ValidationError, match="must be mutual"):
        Fabric(
            name="broken",
            devices=[
                Device(
                    name="a",
                    role="leaf",
                    serial="S1",
                    interfaces=[Interface(name="eth1", peer="b:eth1")],
                ),
                Device(
                    name="b",
                    role="spine",
                    serial="S2",
                    interfaces=[Interface(name="eth1", peer="c:eth1")],
                ),
                Device(
                    name="c",
                    role="spine",
                    serial="S3",
                    interfaces=[Interface(name="eth1", peer="b:eth1")],
                ),
            ],
        )


def test_a_typo_in_a_key_is_an_error_not_a_silent_default() -> None:
    # `extra="forbid"`: one of the most common ways a source of truth stops
    # meaning what its author thought.
    with pytest.raises(ValidationError):
        Device(name="a", role="leaf", serial="S1", rolle="spine")  # type: ignore[call-arg]


def test_device_names_must_be_hostname_safe() -> None:
    for bad in ["Leaf1", "leaf_1", "-leaf", "leaf-", "leaf 1", ""]:
        with pytest.raises(ValidationError):
            Device(name=bad, role="leaf", serial="S1")


def test_asns_must_be_private() -> None:
    # A lab that hands out a real ASN teaches the wrong habit.
    Device(name="a", role="leaf", serial="S1", asn=65000)
    Device(name="b", role="leaf", serial="S2", asn=4200000000)

    for public in [1, 64511, 65535, 4199999999]:
        with pytest.raises(ValidationError, match="RFC 6996"):
            Device(name="a", role="leaf", serial="S1", asn=public)


def test_a_loopback_must_be_a_host_route() -> None:
    Device(name="a", role="leaf", serial="S1", loopback="10.255.0.1/32")
    with pytest.raises(ValidationError, match="/32"):
        Device(name="a", role="leaf", serial="S1", loopback="10.255.0.0/24")


def test_an_empty_fabric_is_rejected() -> None:
    with pytest.raises(ValidationError, match="nothing to provision"):
        Fabric(name="empty", devices=[])


def test_address_pools_must_not_overlap() -> None:
    # Two allocators sharing a range will eventually collide, and the symptom
    # is a fabric that comes up and behaves inexplicably.
    with pytest.raises(ValidationError, match="overlaps"):
        AddressPlan(p2p_base="10.0.0.0/8", loopback_base="10.255.0.0/24")


def test_links_are_listed_once_and_oriented_consistently(fabric: Fabric) -> None:
    links = fabric.links()

    # Four leaf interfaces, each to a spine: four links, not eight.
    assert len(links) == 4
    assert len(set(links)) == len(links)
    # Each pair is oriented so the smaller endpoint comes first, or IPAM would
    # allocate a subnet per direction.
    assert all(near < far for near, far in links)
    assert links == sorted(links), "ordering must be stable for determinism"


def test_roles_decide_who_routes() -> None:
    assert Device(name="a", role="leaf", serial="S").routes
    assert Device(name="a", role="spine", serial="S").routes
    assert Device(name="a", role="border", serial="S").routes
    assert not Device(name="a", role="host", serial="S").routes
    assert not Device(name="a", role="service", serial="S").routes


def test_an_interface_cannot_peer_with_itself() -> None:
    """Self-peering collapses the link and dies far from the cause.

    `far` resolves to the same interface, so the reciprocity check passes and
    the failure surfaces later as a bare StopIteration inside rendering.
    """
    with pytest.raises(ValidationError, match="peers with itself"):
        Device(
            name="r1",
            role="leaf",
            serial="AAA",
            interfaces=[Interface(name="eth1", peer="r1:eth1")],
        )


def test_interface_names_match_the_kernel_and_the_rust_validator() -> None:
    """Names reach `ip link` and FRR config unescaped, so the limits are real."""
    for bad in ["eth 1", "eth:1", "eth\n1", ".", "..", "e" * 16, "eth$1"]:
        with pytest.raises(ValidationError):
            Interface(name=bad, peer="b:eth1")

    for good in ["eth1", "eth-1", "eth_1", "eth.1", "e" * 15]:
        Interface(name=good, peer="b:eth1")


def test_network_and_broadcast_addresses_are_rejected() -> None:
    """A /31 uses both addresses; anything wider cannot."""
    with pytest.raises(ValidationError, match="network address"):
        Interface(name="eth1", peer="b:eth1", address="10.0.0.0/24")
    with pytest.raises(ValidationError, match="broadcast address"):
        Interface(name="eth1", peer="b:eth1", address="10.0.0.255/24")

    # RFC 3021: both addresses of a /31 are usable, which is what every
    # point-to-point link in these labs relies on.
    Interface(name="eth1", peer="b:eth1", address="10.0.0.0/31")
    Interface(name="eth1", peer="b:eth1", address="10.0.0.1/31")


def test_an_interface_name_is_measured_in_bytes_not_code_points() -> None:
    """The kernel's IFNAMSIZ is a byte budget, and this used to count characters.

    ``len()`` counts code points and ``str.isalnum()`` is true for CJK and
    every other Unicode letter, so fifteen Chinese characters were forty-five
    UTF-8 bytes that passed here and failed later at ``ip link`` — in another
    process, long after the source of truth had declared the fabric valid.

    The docstring on the validator claimed it applied "the same rule the Rust
    topology validator applies". It did not: ``str::len`` in Rust is a byte
    count and the test there is ``is_ascii_alphanumeric``. Two validators
    described as one rule were two rules.
    """
    fifteen_cjk = "一二三四五六七八九十一二三四五"
    assert len(fifteen_cjk) == 15
    assert len(fifteen_cjk.encode("utf-8")) == 45
    with pytest.raises(ValidationError, match="bytes"):
        Interface(name=fifteen_cjk, peer="b:eth1")

    # Non-ASCII is refused outright, whatever its length: these names are
    # interpolated into FRR configuration and `ip link` arguments.
    with pytest.raises(ValidationError):
        Interface(name="ethé", peer="b:eth1")

    # Fifteen ASCII bytes is still the limit, and still allowed.
    assert Interface(name="e" * 15, peer="b:eth1").name == "e" * 15
    with pytest.raises(ValidationError, match="bytes"):
        Interface(name="e" * 16, peer="b:eth1")
