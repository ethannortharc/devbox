# Devbox v3 — Shell set (Terminal & Shell)
# 11 packages: terminal multiplexer, shell, prompt, navigation, notes editor
{ pkgs }:
with pkgs;
[
  zellij zsh zsh-autosuggestions zsh-syntax-highlighting
  starship fzf zoxide direnv nix-direnv yazi micro
]
