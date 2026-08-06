"""Smoke tests for the labkit package skeleton.

These exist so the Python CI lane is real from Phase 0 onward: an import
failure or a version drift between labkit and the devbox release fails the
build rather than being discovered in Phase 8.
"""

from __future__ import annotations

import tomllib
from pathlib import Path

import labkit


def project_root() -> Path:
    """Return the labkit project directory (the one holding pyproject.toml)."""
    return Path(__file__).resolve().parent.parent


def test_package_imports_and_exposes_a_version() -> None:
    assert labkit.__version__
    assert labkit.__version__.count(".") == 2, "version must be major.minor.patch"


def test_version_matches_pyproject() -> None:
    """A wheel built from pyproject must not disagree with the runtime value."""
    with (project_root() / "pyproject.toml").open("rb") as fh:
        pyproject = tomllib.load(fh)

    assert pyproject["project"]["version"] == labkit.__version__


def test_requires_a_modern_python() -> None:
    """tomllib, `X | Y` unions, and pydantic 2 all assume >= 3.11."""
    with (project_root() / "pyproject.toml").open("rb") as fh:
        pyproject = tomllib.load(fh)

    assert pyproject["project"]["requires-python"] == ">=3.11"
