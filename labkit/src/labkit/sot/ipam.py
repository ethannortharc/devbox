"""IPAM — deriving the address plan from the source of truth (§10.2).

Allocation is deterministic and total: the same fabric always produces the same
addresses, and every link and every router ends up with one. That determinism
is what makes golden-file config tests possible and what makes a re-provision
after a wipe produce the same fabric rather than a different one.

The invariants — no overlapping subnets, no duplicate addresses — are checked
after every allocation rather than assumed, because an overlapping plan
produces a fabric that comes up and then behaves inexplicably.
"""

from __future__ import annotations

import ipaddress
from collections.abc import Iterator
from dataclasses import dataclass, field

from .models import Fabric

#: Everything here is IPv4: a lab that mixes families in one address plan is a
#: lab whose failures are about the plan rather than about the network.
IPv4Network = ipaddress.IPv4Network

#: Point-to-point links get a /31 (RFC 3021): two usable addresses instead of
#: two wasted ones, and what real fabrics do.
P2P_PREFIX = 31


@dataclass(frozen=True)
class LinkAddresses:
    """Both ends of one link."""

    subnet: str
    #: `device:iface` → address, for the two ends.
    ends: dict[str, str]

    def address_of(self, endpoint: str) -> str | None:
        """The address at one end."""
        return self.ends.get(endpoint)


@dataclass
class Allocation:
    """The derived address plan for a fabric."""

    links: list[LinkAddresses] = field(default_factory=list)
    loopbacks: dict[str, str] = field(default_factory=dict)
    asns: dict[str, int] = field(default_factory=dict)

    def address_of(self, endpoint: str) -> str | None:
        """The address assigned to `device:iface`."""
        for link in self.links:
            found = link.address_of(endpoint)
            if found is not None:
                return found
        return None

    def peer_of(self, endpoint: str) -> tuple[str, str] | None:
        """The far end of `device:iface`, as `(endpoint, address)`."""
        for link in self.links:
            if endpoint in link.ends:
                far = next(e for e in link.ends if e != endpoint)
                return far, link.ends[far]
        return None

    def all_addresses(self) -> list[str]:
        """Every address the plan assigns, bare (no prefix length)."""
        out = [addr.split("/")[0] for link in self.links for addr in link.ends.values()]
        out.extend(addr.split("/")[0] for addr in self.loopbacks.values())
        return out


class OverlapError(ValueError):
    """Raised when an allocation would assign something twice."""


def allocate(fabric: Fabric) -> Allocation:
    """Derive the full address plan.

    Explicit values in the source of truth win; everything else is derived in
    a stable order (links sorted, devices in declaration order).
    """
    plan = fabric.address_plan
    p2p_pool = _subnets(plan.p2p_base, P2P_PREFIX)
    loopback_pool = _hosts(plan.loopback_base)

    allocation = Allocation()

    for near, far in fabric.links():
        explicit = _explicit_address(fabric, near) or _explicit_address(fabric, far)
        if explicit is not None:
            network = ipaddress.IPv4Interface(explicit).network
        else:
            candidate = next(p2p_pool, None)
            if candidate is None:
                raise OverlapError(f"{plan.p2p_base} has no room left for link {near} ↔ {far}")
            network = candidate

        hosts = list(network.hosts()) or list(network)
        if len(hosts) < 2:
            raise OverlapError(f"subnet {network} cannot address two ends")

        allocation.links.append(
            LinkAddresses(
                subnet=str(network),
                ends={
                    near: f"{hosts[0]}/{network.prefixlen}",
                    far: f"{hosts[1]}/{network.prefixlen}",
                },
            )
        )

    next_asn = plan.asn_base
    for device in fabric.devices:
        if not device.routes:
            continue

        if device.loopback is not None:
            allocation.loopbacks[device.name] = device.loopback
        else:
            address = next(loopback_pool, None)
            if address is None:
                raise OverlapError(f"{plan.loopback_base} has no room left for {device.name}")
            allocation.loopbacks[device.name] = f"{address}/32"

        allocation.asns[device.name] = device.asn if device.asn is not None else next_asn
        if device.asn is None:
            next_asn += 1

    verify(allocation)
    return allocation


def verify(allocation: Allocation) -> None:
    """Assert the invariants that make a plan usable.

    Checked rather than trusted: an overlapping plan produces a fabric that
    comes up and then behaves inexplicably, which is the worst kind of bug to
    debug in a network.
    """
    addresses = allocation.all_addresses()
    duplicates = {a for a in addresses if addresses.count(a) > 1}
    if duplicates:
        raise OverlapError(f"addresses assigned more than once: {sorted(duplicates)}")

    subnets = [ipaddress.IPv4Network(link.subnet) for link in allocation.links]
    for i, left in enumerate(subnets):
        for right in subnets[i + 1 :]:
            if left.overlaps(right):
                raise OverlapError(f"subnets {left} and {right} overlap")


def _subnets(base: str, prefix: int) -> Iterator[IPv4Network]:
    """Yield successive subnets of `prefix` length from `base`."""
    network = ipaddress.IPv4Network(base)
    if prefix < network.prefixlen:
        raise ValueError(f"cannot carve /{prefix} subnets out of {base}")
    return network.subnets(new_prefix=prefix)


def _hosts(base: str) -> Iterator[ipaddress.IPv4Address]:
    """Yield successive host addresses from `base`."""
    return iter(ipaddress.IPv4Network(base).hosts())


def _explicit_address(fabric: Fabric, endpoint: str) -> str | None:
    """An address declared in the source of truth for `device:iface`."""
    device_name, iface_name = endpoint.split(":", 1)
    device = fabric.device(device_name)
    if device is None:
        return None
    iface = device.interface(iface_name)
    return None if iface is None else iface.address
