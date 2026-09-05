# Devbox — Network set (Networking tools)
#
# frr and conntrack-tools are here because things running on the box shell out
# to them: standing a routed topology up starts zebra/bgpd inside each router
# namespace, and tightening an egress policy flushes conntrack. Without them
# those paths fail with command-not-found after having already done half their
# work.
{ pkgs }:
with pkgs;
[
  tailscale mosh nmap tcpdump bandwhich trippy doggo
  frr dnsmasq chrony busybox iproute2 conntrack-tools

  # FRR's daemons, on PATH.
  #
  # The package keeps them in `libexec/frr/`, and a NixOS system profile links
  # only bin, sbin, lib, etc and share — so installing `frr` gave the substrate
  # `vtysh` and nothing to talk to. Callers invoke `zebra` and `bgpd` by name
  # and probe `command -v zebra` before they start, so the whole reason this
  # set carries frr was unreachable on the one image devbox builds by default.
  #
  # `mgmtd` is here for the same reason and is easy to miss: FRR 10 moved
  # interface configuration into its northbound datastore, so without it a
  # router loads its BGP configuration and none of its addresses.
  #
  # Symlinks rather than a full wrapper: the daemons find their own libraries
  # through the store path they are linked from, and `-N`/`-z` already give
  # each namespace its own sockets.
  (runCommand "frr-daemons" { } ''
    mkdir -p $out/bin
    for daemon in mgmtd zebra bgpd; do
      ln -s ${frr}/libexec/frr/$daemon $out/bin/$daemon
    done
  '')
]
