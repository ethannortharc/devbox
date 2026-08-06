# Devbox — System set (OS Foundation)
# coreutils, networking, crypto, build tools, and the firewall.
#
# nftables and conntrack-tools are here rather than in the optional `network`
# set because every enforcing egress posture loads a ruleset with the first and
# drops now-disallowed sessions with the second. Enforcement that depends on
# the user having ticked an optional checkbox is not enforcement — the first
# `devbox policy set isolated` on a fresh box would simply fail.
{ pkgs }:
with pkgs;
[
  coreutils gnugrep gnused gawk findutils diffutils
  gzip gnutar xz bzip2 file which tree less
  curl wget openssh openssl cacert gnupg
  nftables conntrack-tools
  gcc gnumake pkg-config man-db
]
