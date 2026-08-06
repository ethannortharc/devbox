# Devbox — System set (OS Foundation)
# coreutils, networking, crypto, build tools, and the firewall.
#
# nftables is here rather than in the optional `network` set because every
# enforcing egress posture loads a ruleset with it. Enforcement that depends on
# the user having ticked an optional checkbox is not enforcement — the first
# `devbox policy set isolated` on a fresh box would simply fail.
{ pkgs }:
with pkgs;
[
  coreutils gnugrep gnused gawk findutils diffutils
  gzip gnutar xz bzip2 file which tree less
  curl wget openssh openssl cacert gnupg
  nftables
  gcc gnumake pkg-config man-db
]
