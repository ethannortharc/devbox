# Box lab

Describe a network in one file; bring it up in seconds.

A lab is **not** N virtual machines. Nodes are network namespaces inside one
Linux substrate, joined by veth pairs — dozens start on one kernel, the
networking is genuine L2/L3, and a single `devbox-obsd` sees the whole thing.

## Try one

```bash
devbox lab list
devbox lab status clos-3node          # the address plan, before anything runs
devbox lab up clos-3node --dry-run    # exactly what would run
devbox lab up clos-3node --substrate mybox
```

The substrate is one Linux box holding the namespaces — a Lima VM on macOS, the
host on Linux. It is the only heavyweight thing a lab needs, and it is reused
across labs.

## The scenarios

| Scenario | What it is for |
|---|---|
| `clos-3node` | Two leaves, one spine. The fastest check that the plumbing works. |
| `fat-tree-4x2` | Four leaves, two spines, every leaf dual-homed — so a single lossy link reads as a *straggler* rather than a uniform slowdown. |
| `partition-3` | A triangle: cut one link and a path remains, cut two and a node is isolated. |
| `client-proxy-server` | The shape every egress question has. |
| `wan-lossy` | Two sites across a link worth abusing. |
| `ztp-fabric` | Blank nodes provisioning themselves. See [ztp.md](ztp.md). |

## Writing your own

```toml
[lab]
name = "my-fabric"          # lowercase, digits, '-' — it becomes a namespace name
substrate = "auto"
base = "10.0.0.0/16"        # link subnets from the lower half, loopbacks the upper

[[nodes]]
name = "leaf1"
role = "frr-router"         # frr-router | host | ztp-blank | service
sets = ["network"]

[[nodes]]
name = "spine1"
role = "frr-router"

[[links]]
endpoints = ["leaf1:eth1", "spine1:eth1"]
subnet = "10.0.12.0/31"     # optional; IPAM assigns one otherwise

[services]
dns = true
dhcp = false
ntp = true
```

Validation is strict on purpose. A reused interface, a self-link, an
unconnected node, or a name that is not interface-safe all fail *before*
anything is created — a topology that half-comes-up wastes an hour debugging a
network that was never going to work.

## What is derived

- **Addresses** — /31 per link (RFC 3021), a loopback per router, an ASN from
  the RFC 6996 private range. Deterministic: `lab down` then `lab up` is the
  same lab.
- **Routing** — eBGP-unnumbered with ECMP, 3/9 timers so it converges while you
  watch, and `no bgp ebgp-requires-policy` without which modern FRR reports "BGP
  up" and advertises nothing.

```bash
devbox lab config clos-3node leaf1     # the generated frr.conf
```

## Breaking things

```bash
devbox lab fault clos-3node leaf1-spine1 --delay 20 --jitter 5
devbox lab fault clos-3node leaf1-spine1 --loss 5 --direction a
devbox lab fault clos-3node leaf1-spine1 --partition
devbox lab heal  clos-3node leaf1-spine1
```

Two choices worth knowing:

- **Faults are directional.** A real lossy link is usually lossy one way, and a
  symmetric fault hides exactly the asymmetries worth debugging.
- **`--partition` is 100% loss, not an interface down.** A downed interface
  tells the routing protocol immediately; a black-holing link does not, and the
  second is the failure that actually hurts.

## Stragglers

A ring all-reduce runs at the speed of its worst hop, so one impaired link of
sixteen halves a whole job while every node looks healthy. Given the
observability plane's flow records, devbox names the culprit link, quantifies
it against the median, and reports achieved versus potential throughput — which
is the §9.5 demo in one sentence.

© 2026 Ethan H.B. Zhou
