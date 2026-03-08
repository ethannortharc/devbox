# Devbox v3 — System set (OS Foundation)
# 24 packages: coreutils, networking, crypto, build tools
{ pkgs }:
with pkgs;
[
  coreutils gnugrep gnused gawk findutils diffutils
  gzip gnutar xz bzip2 file which tree less
  curl wget openssh openssl cacert gnupg
  gcc gnumake pkg-config man-db
]
