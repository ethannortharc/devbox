# Devbox v3 — Rust language set
{ pkgs }:
with pkgs;
[
  rustup rust-analyzer cargo-watch cargo-edit
  cargo-expand sccache
]
