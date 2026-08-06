"""Config generation — render → diff → apply → verify (§10.2).

The same mental model as `devbox diff`/`devbox commit`, one layer down: you
render what the config *should* be, diff it against what is *running*, apply
only if they differ, and verify afterwards.

Two properties make that cycle trustworthy, and both are tested:

* **Determinism** — the same source of truth always renders the same bytes, so
  a golden-file test means something and a diff shows real changes rather than
  dictionary ordering.
* **Idempotency** — applying twice produces no second change. A generator that
  is not idempotent turns every reconciliation loop into a config churn loop.
"""

from __future__ import annotations

import difflib
from dataclasses import dataclass
from pathlib import Path

from jinja2 import Environment, FileSystemLoader, StrictUndefined

from ..sot import Allocation, Device, Fabric

TEMPLATE_DIR = Path(__file__).parent / "templates"


def _environment() -> Environment:
    """The Jinja environment used for every render.

    `StrictUndefined` on purpose: a typo'd variable must be a loud error, not
    a config that silently ships with a blank where a router-id belonged.
    `keep_trailing_newline` so rendered output is byte-comparable with a file
    on disk.
    """
    return Environment(
        loader=FileSystemLoader(TEMPLATE_DIR),
        undefined=StrictUndefined,
        trim_blocks=True,
        lstrip_blocks=True,
        keep_trailing_newline=True,
        # Network config, not markup: escaping would corrupt it.
        autoescape=False,
    )


def render_device(fabric: Fabric, allocation: Allocation, device: Device) -> str:
    """Render one device's FRR configuration."""
    interfaces: list[dict[str, object]] = []
    for iface in sorted(device.interfaces, key=lambda i: i.name):
        endpoint = f"{device.name}:{iface.name}"
        address = allocation.address_of(endpoint)
        peer = allocation.peer_of(endpoint)
        interfaces.append(
            {
                "name": iface.name,
                "address": address,
                "peer": peer[0] if peer else None,
                # Unnumbered peering runs over the interface, so a neighbour
                # exists exactly where a link does.
                "peers": iface.peer is not None,
            }
        )

    networks = [str(i["address"]) for i in interfaces if i["address"]]
    template = _environment().get_template("frr.conf.j2")
    return template.render(
        fabric=fabric.name,
        device=device.name,
        role=device.role,
        asn=allocation.asns.get(device.name),
        loopback=allocation.loopbacks.get(device.name),
        interfaces=interfaces,
        networks=sorted(_subnet_of(n) for n in networks),
    )


def render_all(fabric: Fabric, allocation: Allocation) -> dict[str, str]:
    """Render every router's configuration, keyed by device name."""
    return {device.name: render_device(fabric, allocation, device) for device in fabric.routers}


@dataclass(frozen=True)
class ConfigDiff:
    """The difference between a rendered config and a running one."""

    device: str
    unified: str

    @property
    def changed(self) -> bool:
        """Whether applying this would change anything."""
        return bool(self.unified.strip())

    def __str__(self) -> str:
        return self.unified


def diff(device: str, running: str, rendered: str) -> ConfigDiff:
    """Diff a running config against a rendered one.

    Comment lines are ignored: a generated banner naming the render time would
    otherwise make every config look changed, every time, which trains people
    to stop reading diffs.
    """
    unified = "\n".join(
        difflib.unified_diff(
            _significant(running),
            _significant(rendered),
            fromfile=f"{device} (running)",
            tofile=f"{device} (rendered)",
            lineterm="",
        )
    )
    return ConfigDiff(device=device, unified=unified)


def _significant(config: str) -> list[str]:
    """The lines of a config that actually configure something."""
    return [
        line.rstrip()
        for line in config.splitlines()
        if line.strip() and not line.lstrip().startswith("!")
    ]


def _subnet_of(address: str) -> str:
    """The network an interface address belongs to."""
    import ipaddress

    return str(ipaddress.IPv4Interface(address).network)
