# Devbox — Network set (Networking tools)
#
# frr and conntrack-tools are here because `devbox lab` shells out to them on
# the substrate box: `lab up` starts zebra/bgpd inside each router namespace,
# and tightening an egress policy flushes conntrack. Without them those paths
# fail with command-not-found after having already done half their work.
{ pkgs }:
with pkgs;
[
  tailscale mosh nmap tcpdump bandwhich trippy doggo
  frr conntrack-tools
]
