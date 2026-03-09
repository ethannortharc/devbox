# Devbox v3 — Tools set (Modern CLI replacements)
# 22 packages: search, view, monitor, benchmark, HTTP
{ pkgs }:
with pkgs;
[
  ripgrep fd bat eza delta sd choose
  jq yq-go fx htop bottom procs dust duf
  tokei hyperfine tealdeer httpie dog glow entr
]
