# Devbox v3 — Nix set index
# Each set is a function { pkgs } -> [ list of packages ]
{ pkgs }:
{
  system      = import ./system.nix { inherit pkgs; };
  shell       = import ./shell.nix { inherit pkgs; };
  tools       = import ./tools.nix { inherit pkgs; };
  editor      = import ./editor.nix { inherit pkgs; };
  git         = import ./git.nix { inherit pkgs; };
  container   = import ./container.nix { inherit pkgs; };
  network     = import ./network.nix { inherit pkgs; };
  ai_code     = import ./ai-code.nix { inherit pkgs; };
  ai_infra    = import ./ai-infra.nix { inherit pkgs; };
  lang_go     = import ./lang-go.nix { inherit pkgs; };
  lang_rust   = import ./lang-rust.nix { inherit pkgs; };
  lang_python = import ./lang-python.nix { inherit pkgs; };
  lang_node   = import ./lang-node.nix { inherit pkgs; };
  lang_java   = import ./lang-java.nix { inherit pkgs; };
  lang_ruby   = import ./lang-ruby.nix { inherit pkgs; };
}
