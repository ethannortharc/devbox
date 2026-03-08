# Devbox v3 — Python language set
{ pkgs }:
with pkgs;
[
  python312 uv ruff pyright
  python312Packages.ipython python312Packages.pytest
]
