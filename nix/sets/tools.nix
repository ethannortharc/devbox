# Devbox v3 — Tools set (Modern CLI replacements)
# 21 packages: search, view, monitor, benchmark, HTTP
{ pkgs }:
with pkgs;
[
  ripgrep fd bat eza delta sd choose
  jq yq-go fx htop procs dust duf
  tokei hyperfine tealdeer httpie dog glow entr
]
