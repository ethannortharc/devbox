"""Source of truth: the models everything else derives from (§10.2)."""

from __future__ import annotations

from .ipam import Allocation, LinkAddresses, OverlapError, allocate, verify
from .models import AddressPlan, Device, Fabric, Interface

__all__ = [
    "AddressPlan",
    "Allocation",
    "Device",
    "Fabric",
    "Interface",
    "LinkAddresses",
    "OverlapError",
    "allocate",
    "verify",
]
