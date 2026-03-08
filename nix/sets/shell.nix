# Devbox v3 — Shell set (Terminal & Shell)
# 10 packages: terminal multiplexer, shell, prompt, navigation
{ pkgs }:
with pkgs;
[
  zellij zsh zsh-autosuggestions zsh-syntax-highlighting
  starship fzf zoxide direnv nix-direnv yazi
]
