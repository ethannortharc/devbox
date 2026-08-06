"""Config generation: intent + source of truth → per-device config (§10.2)."""

from __future__ import annotations

from .render import ConfigDiff, diff, render_all, render_device

__all__ = ["ConfigDiff", "diff", "render_all", "render_device"]
