# Devbox — Shell set (Terminal & Shell)
#
# Must list exactly what `NIX_SETS` says this set contains. Provisioning pushes
# this file; a later Sets apply regenerates the same path from the catalog. So
# a package here and not there is installed when the box is created and removed
# by the first rebuild — which is how `zellij` survived the v4 removal of the
# layout subsystem and then vanished on the next Sets apply. A test compares
# the two.
{ pkgs }:
with pkgs;
[
  zsh zsh-autosuggestions zsh-syntax-highlighting
  starship fzf zoxide direnv nix-direnv yazi micro
]
