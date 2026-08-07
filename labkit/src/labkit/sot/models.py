"""Source of truth — pydantic models for the fabric (§10.2).

A tiny, opinionated NetBox-in-a-file. Everything else derives from this: the
address plan, the rendered configs, the ZTP server's serial-to-role mapping,
and the assertions the test SDK makes.

The models validate aggressively because the whole value of a source of truth
is that everything downstream can *trust* it. A device with a duplicate serial
or a link to a device that does not exist has to fail here, not three steps
later when a node has been provisioned with someone else's config.
"""

from __future__ import annotations

import ipaddress
import re
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

# A device or site name: lowercase, digits, hyphen. These become hostnames,
# namespace names, and path components, so the character set is deliberately
# narrow.
NAME_RE = re.compile(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?$")

Role = Literal["spine", "leaf", "border", "service", "host"]


class Model(BaseModel):
    """Base model: reject unknown fields.

    A typo in a YAML key is one of the most common ways a source of truth
    silently stops meaning what its author thought. Rejecting extras turns
    that into an error at load time.
    """

    model_config = ConfigDict(extra="forbid", frozen=True)


class Interface(Model):
    """One physical interface on a device."""

    name: str
    #: The `device:interface` this connects to, if anything.
    peer: str | None = None
    #: Assigned by IPAM when absent.
    address: str | None = None

    @field_validator("name")
    @classmethod
    def _valid_name(cls, value: str) -> str:
        """The same rule the Rust topology validator applies.

        These names are rendered unescaped into FRR configuration and become
        arguments to `ip link`, so a name with whitespace, a colon, or a
        newline produces an invalid — or injected — statement a long way from
        where it was written. IFNAMSIZ is 16 including the NUL, and `.`/`..`
        are rejected by the kernel outright.
        """
        if value in {".", ".."} or len(value) > 15:
            raise ValueError(
                f"interface name {value!r} must be 1-15 characters and not '.' or '..'"
            )
        if value and not all(c.isalnum() or c in "-_." for c in value):
            raise ValueError(
                f"interface name {value!r} may contain letters, digits, '-', '_', and '.' only"
            )
        if not value:
            raise ValueError("an interface needs a name")
        return value

    @field_validator("peer")
    @classmethod
    def _valid_peer(cls, value: str | None) -> str | None:
        if value is None:
            return None
        if ":" not in value:
            raise ValueError(f"peer {value!r} must be written device:interface")
        return value

    @field_validator("address")
    @classmethod
    def _valid_address(cls, value: str | None) -> str | None:
        if value is None:
            return None
        # Raises for anything that is not a valid interface address.
        interface = ipaddress.ip_interface(value)
        # IPv4-only, for the same reason as AddressPlan: allocation reparses
        # these as IPv4Interface, so an IPv6 value fails deep inside IPAM
        # instead of here where the message can name the field.
        if interface.version != 4:
            raise ValueError(f"{value} is IPv6; interfaces address IPv4 fabrics only")
        return value


class Device(Model):
    """One device in the fabric."""

    name: str
    role: Role
    #: Hardware serial. This is what a blank node presents to the ZTP server
    #: to ask "who am I?", so it must be unique across the fabric.
    serial: str
    site: str = "default"
    asn: int | None = None
    loopback: str | None = None
    interfaces: list[Interface] = Field(default_factory=list)

    @field_validator("name", "site")
    @classmethod
    def _valid_name(cls, value: str) -> str:
        if not NAME_RE.match(value):
            raise ValueError(
                f"{value!r} must be lowercase letters, digits, or '-'; it becomes a hostname"
            )
        return value

    @field_validator("serial")
    @classmethod
    def _valid_serial(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("a device needs a serial; it is how ZTP identifies it")
        return value

    @field_validator("asn")
    @classmethod
    def _private_asn(cls, value: int | None) -> int | None:
        if value is None:
            return None
        # RFC 6996 private ranges. A lab that hands out a real ASN is a lab
        # that teaches the wrong habit.
        in_16bit = 64512 <= value <= 65534
        in_32bit = 4200000000 <= value <= 4294967294
        if not (in_16bit or in_32bit):
            raise ValueError(
                f"ASN {value} is outside the RFC 6996 private ranges "
                "(64512-65534, 4200000000-4294967294)"
            )
        return value

    @field_validator("loopback")
    @classmethod
    def _valid_loopback(cls, value: str | None) -> str | None:
        if value is None:
            return None
        interface = ipaddress.ip_interface(value)
        if interface.version != 4:
            raise ValueError(f"loopback {value} is IPv6; the fabric is IPv4-only")
        if interface.network.prefixlen != 32:
            raise ValueError(f"loopback {value} should be a /32 host route")
        return value

    @model_validator(mode="after")
    def _no_self_peering(self) -> Device:
        """An interface cannot peer with itself.

        `r1:eth1` naming `r1:eth1` passes the reciprocity check — `far`
        resolves to the very same interface — and then collapses the link to a
        single endpoint, so rendering dies in `peer_of` with a bare
        StopIteration nowhere near the cause. Caught here, where the device
        name is known and the message can name the interface.
        """
        for iface in self.interfaces:
            if iface.peer == f"{self.name}:{iface.name}":
                raise ValueError(
                    f"{self.name}:{iface.name} peers with itself; "
                    "a link needs two distinct endpoints"
                )
        return self

    def interface(self, name: str) -> Interface | None:
        """Look up one interface."""
        return next((i for i in self.interfaces if i.name == name), None)

    @property
    def routes(self) -> bool:
        """Whether this device runs a routing protocol."""
        return self.role in {"spine", "leaf", "border"}


class AddressPlan(Model):
    """The prefixes IPAM allocates from."""

    #: Every point-to-point link comes from here.
    p2p_base: str = "10.0.0.0/16"
    #: Every loopback comes from here.
    loopback_base: str = "10.255.0.0/24"
    #: First ASN handed out to a device that does not declare one.
    asn_base: int = 65000

    @field_validator("asn_base")
    @classmethod
    def _private_asn_base(cls, value: int) -> int:
        """RFC 6996 private-use ranges only.

        A lab must not advertise itself with someone else's AS number. 65535
        and 4294967295 are reserved rather than private, so both ranges stop
        one short — and a base at the very top would allocate straight into
        them for the second router.
        """
        if not (64512 <= value <= 65534 or 4_200_000_000 <= value <= 4_294_967_294):
            raise ValueError(
                f"asn_base {value} is not a private ASN; "
                "use 64512-65534 or 4200000000-4294967294 (RFC 6996)"
            )
        return value

    @field_validator("p2p_base", "loopback_base")
    @classmethod
    def _valid_prefix(cls, value: str) -> str:
        network = ipaddress.ip_network(value, strict=True)
        # The allocator is IPv4-only — it reparses these as IPv4Network. An
        # IPv6 prefix accepted here failed much later, inside allocation, with
        # a message about the wrong thing. A declared validation boundary that
        # lets invalid data past is not a boundary.
        if network.version != 4:
            raise ValueError(
                f"{value} is IPv6; the source of truth addresses IPv4 fabrics only"
            )
        return value

    @model_validator(mode="after")
    def _no_overlap(self) -> AddressPlan:
        p2p = ipaddress.ip_network(self.p2p_base)
        loopback = ipaddress.ip_network(self.loopback_base)
        if p2p.overlaps(loopback):
            raise ValueError(
                f"p2p_base {p2p} overlaps loopback_base {loopback}; "
                "two allocators sharing a range will eventually collide"
            )
        return self


class Fabric(Model):
    """The whole source of truth."""

    name: str
    devices: list[Device]
    address_plan: AddressPlan = Field(default_factory=AddressPlan)

    @field_validator("name")
    @classmethod
    def _valid_name(cls, value: str) -> str:
        if not NAME_RE.match(value):
            raise ValueError(f"{value!r} must be lowercase letters, digits, or '-'")
        return value

    @model_validator(mode="after")
    def _coherent(self) -> Fabric:
        if not self.devices:
            raise ValueError("a fabric with no devices has nothing to provision")

        by_name: dict[str, Device] = {}
        serials: dict[str, str] = {}

        for device in self.devices:
            if device.name in by_name:
                raise ValueError(f"two devices are both called {device.name!r}")
            by_name[device.name] = device

            if device.serial in serials:
                raise ValueError(
                    f"devices {serials[device.serial]!r} and {device.name!r} share "
                    f"serial {device.serial!r}; ZTP would provision one as the other"
                )
            serials[device.serial] = device.name

        # Every peer must exist, and peering must be mutual: a one-sided link
        # is a typo that would otherwise produce a fabric that half-converges.
        for device in self.devices:
            seen_ifaces: set[str] = set()
            for iface in device.interfaces:
                if iface.name in seen_ifaces:
                    raise ValueError(f"{device.name} has two interfaces called {iface.name!r}")
                seen_ifaces.add(iface.name)

                if iface.peer is None:
                    continue
                peer_device, peer_iface = iface.peer.split(":", 1)

                if peer_device not in by_name:
                    raise ValueError(
                        f"{device.name}:{iface.name} peers with unknown device {peer_device!r}"
                    )
                far = by_name[peer_device].interface(peer_iface)
                if far is None:
                    raise ValueError(
                        f"{device.name}:{iface.name} peers with "
                        f"{iface.peer!r}, which does not exist"
                    )
                if far.peer != f"{device.name}:{iface.name}":
                    raise ValueError(
                        f"{device.name}:{iface.name} points at {iface.peer!r}, "
                        f"but that interface points at {far.peer!r}; "
                        "links must be mutual"
                    )
        return self

    def device(self, name: str) -> Device | None:
        """Look up a device by name."""
        return next((d for d in self.devices if d.name == name), None)

    def by_serial(self, serial: str) -> Device | None:
        """Look up a device by serial — the ZTP 'who am I?' query (§10.1)."""
        return next((d for d in self.devices if d.serial == serial), None)

    def by_role(self, role: Role) -> list[Device]:
        """Every device with a role."""
        return [d for d in self.devices if d.role == role]

    @property
    def routers(self) -> list[Device]:
        """Every device that runs a routing protocol."""
        return [d for d in self.devices if d.routes]

    def links(self) -> list[tuple[str, str]]:
        """Every link, as `device:iface` pairs, deduplicated and sorted.

        Each link appears once, oriented so the lexicographically smaller end
        comes first — otherwise IPAM would allocate a subnet per *direction*.
        """
        seen: set[tuple[str, str]] = set()
        for device in self.devices:
            for iface in device.interfaces:
                if iface.peer is None:
                    continue
                near = f"{device.name}:{iface.name}"
                pair = (near, iface.peer) if near < iface.peer else (iface.peer, near)
                seen.add(pair)
        return sorted(seen)
