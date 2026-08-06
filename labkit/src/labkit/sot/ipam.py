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
    # Every subnet and loopback something already claims, gathered before a
    # single automatic value is handed out. Without this an explicit link that
    # named the pool's first candidate got that candidate handed to the next
    # automatic link as well, and allocation failed its own verification.
    claimed_networks = _explicit_networks(fabric)
    claimed_loopbacks = {
        ipaddress.IPv4Interface(device.loopback).ip
        for device in fabric.devices
        if device.loopback
    }

    p2p_pool = _subnets(plan.p2p_base, P2P_PREFIX, skip=claimed_networks)
    loopback_pool = _hosts(plan.loopback_base, skip=claimed_loopbacks)

    allocation = Allocation()

    for near, far in fabric.links():
        near_explicit = _explicit_address(fabric, near)
        far_explicit = _explicit_address(fabric, far)

        if near_explicit is not None and far_explicit is not None:
            near_iface = ipaddress.IPv4Interface(near_explicit)
            far_iface = ipaddress.IPv4Interface(far_explicit)
            if near_iface.network != far_iface.network:
                raise OverlapError(
                    f"{near} declares {near_explicit} and {far} declares {far_explicit}; "
                    "the two ends of a link must be in the same subnet"
                )
            if near_iface.ip == far_iface.ip:
                raise OverlapError(f"{near} and {far} both declare {near_explicit}")

        explicit = near_explicit or far_explicit
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

        # A declared address is honoured *at the end that declared it*, not
        # merely used to pick a subnet — otherwise the API's promise that
        # explicit values win is a half-truth that silently moves addresses.
        if near_explicit is not None:
            near_addr = near_explicit
            far_addr = far_explicit or _other_host(hosts, near_explicit, network)
        elif far_explicit is not None:
            far_addr = far_explicit
            near_addr = _other_host(hosts, far_explicit, network)
        else:
            near_addr = f"{hosts[0]}/{network.prefixlen}"
            far_addr = f"{hosts[1]}/{network.prefixlen}"

        allocation.links.append(
            LinkAddresses(subnet=str(network), ends={near: near_addr, far: far_addr})
        )

    # Explicit ASNs are claimed up front, so a derived one never lands on top
    # of one the fabric already asked for: two intended eBGP peers sharing an
    # AS do not peer at all.
    claimed = {d.asn for d in fabric.devices if d.routes and d.asn is not None}
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

        if device.asn is not None:
            allocation.asns[device.name] = device.asn
        else:
            while next_asn in claimed:
                next_asn += 1
            allocation.asns[device.name] = next_asn
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

    # A loopback inside a link's subnet installs both a connected route and a
    # host route for the same address. The fabric comes up and forwards
    # ambiguously — the worst kind of lab bug, because it looks like it works.
    for device, loopback in allocation.loopbacks.items():
        address = ipaddress.IPv4Interface(loopback).ip
        for subnet in subnets:
            if address in subnet:
                raise OverlapError(
                    f"loopback {loopback} for {device!r} falls inside link subnet "
                    f"{subnet}: a connected route and a host route for the same "
                    "address make forwarding ambiguous"
                )

    # Every allocated ASN must be private (RFC 6996). The base is validated at
    # the model boundary, but derivation walks upward from it, so the last
    # device in a large fabric can still land outside the range.
    for device, asn in allocation.asns.items():
        if not (64512 <= asn <= 65534 or 4_200_000_000 <= asn <= 4_294_967_294):
            raise OverlapError(
                f"{device!r} would use AS {asn}, which is not a private ASN; "
                "leave room in 64512-65534 or 4200000000-4294967294 (RFC 6996)"
            )

    # Two routers sharing an AS do not form an eBGP session, so the fabric
    # comes up and never converges.
    by_asn: dict[int, str] = {}
    for device, asn in allocation.asns.items():
        if asn in by_asn:
            raise OverlapError(f"devices {by_asn[asn]!r} and {device!r} share AS {asn}")
        by_asn[asn] = device


def _subnets(
    base: str, prefix: int, skip: set[IPv4Network] | None = None
) -> Iterator[IPv4Network]:
    """Yield successive subnets of `prefix` length from `base`.

    Subnets overlapping anything in `skip` are passed over: those are already
    claimed by an explicit declaration, and handing one out again produces a
    plan that fails its own verification.
    """
    network = ipaddress.IPv4Network(base)
    if prefix < network.prefixlen:
        raise ValueError(f"cannot carve /{prefix} subnets out of {base}")
    taken = skip or set()
    for candidate in network.subnets(new_prefix=prefix):
        if any(candidate.overlaps(claimed) for claimed in taken):
            continue
        yield candidate


def _hosts(
    base: str, skip: set[ipaddress.IPv4Address] | None = None
) -> Iterator[ipaddress.IPv4Address]:
    """Yield successive host addresses from `base`, minus the claimed ones."""
    taken = skip or set()
    for host in ipaddress.IPv4Network(base).hosts():
        if host not in taken:
            yield host


def _explicit_networks(fabric: Fabric) -> set[IPv4Network]:
    """Every subnet an interface address in the source of truth already claims."""
    networks: set[IPv4Network] = set()
    for device in fabric.devices:
        for iface in device.interfaces:
            if iface.address:
                networks.add(ipaddress.IPv4Interface(iface.address).network)
    return networks


def _other_host(
    hosts: list[ipaddress.IPv4Address], taken: str, network: ipaddress.IPv4Network
) -> str:
    """The host address in `network` that is not `taken`."""
    taken_ip = ipaddress.IPv4Interface(taken).ip
    for host in hosts:
        if host != taken_ip:
            return f"{host}/{network.prefixlen}"
    raise OverlapError(f"{network} has no second address beside {taken}")


def _explicit_address(fabric: Fabric, endpoint: str) -> str | None:
    """An address declared in the source of truth for `device:iface`."""
    device_name, iface_name = endpoint.split(":", 1)
    device = fabric.device(device_name)
    if device is None:
        return None
    iface = device.interface(iface_name)
    return None if iface is None else iface.address
