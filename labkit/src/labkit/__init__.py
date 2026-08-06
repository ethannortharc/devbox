"""devbox lab toolkit.

Three responsibilities, each a separate subpackage as they land:

* ``labkit.sot`` — the source of truth: pydantic models for sites, devices,
  roles, links, and the address plan, plus IPAM allocation (Phase 8).
* ``labkit.gen`` — Jinja2 config generation with a render → diff → apply →
  verify cycle, golden-file tested and idempotent (Phase 8).
* ``labkit.sdk`` — pytest fixtures that bring a lab up, drive a scenario, and
  assert on observed behaviour, reading the same event store the console shows
  (Phase 8).

The package version tracks the devbox release it ships with.
"""

from __future__ import annotations

__all__ = ["__version__"]

__version__ = "0.1.0"
