# Zero-touch provisioning

Blank nodes boot, discover where to get their config, apply it, verify
themselves, and appear healthy — with no manual step anywhere.

```bash
devbox lab up ztp-fabric --substrate mybox
```

## The flow

```
blank node boots
  → DHCP (options 66/67: boot server + script URL)
  → GET  /bootstrap.sh
  → POST /identify        {"serial": "…"}  → who am I?
  → GET  /config/{name}   the rendered config
  → apply, then self-check
  → POST /status          healthy | failed
```

Nothing on the node is pre-seeded. A `ztp-blank` node in a topology boots with
no configuration at all — pre-configuring it would make the demonstration a lie.

## The pieces

| Piece | Language | Job |
|---|---|---|
| `labkit.sot` | Python | The source of truth: sites, devices, roles, links, address plan. A small, opinionated NetBox-in-a-file. |
| `labkit.sot.ipam` | Python | Derives every address, loopback, and ASN. Deterministic and verified. |
| `labkit.gen` | Python | Jinja render → diff → apply → verify. Golden-file tested and idempotent. |
| `devbox-ztpd` | Go | HTTP server plus the provisioning state machine and Prometheus metrics. |
| dnsmasq | — | DHCP options 66/67 pointing at ztpd. |

The split is deliberate: rendering stays in Python where templating is
ergonomic, and HTTP stays in Go where a long-running service belongs. They meet
at a directory of rendered files plus a `serials.json`.

## The source of truth

```python
from labkit.sot import Device, Fabric, Interface, allocate

fabric = Fabric(
    name="ztp-fabric",
    devices=[
        Device(name="spine1", role="spine", serial="SN-SPINE-001",
               interfaces=[Interface(name="eth1", peer="leaf1:eth1")]),
        Device(name="leaf1", role="leaf", serial="SN-LEAF-001",
               interfaces=[Interface(name="eth1", peer="spine1:eth1")]),
    ],
)
plan = allocate(fabric)
```

It refuses anything downstream cannot trust: duplicate serials (ZTP would
provision one device as another), one-sided links, links to devices that do not
exist, non-private ASNs, and — because `extra="forbid"` — a typo'd key.

## The state machine

```
discovered → identified → rendering → pushing → verifying → healthy
                                                          ↘ failed
```

Two properties matter more than the states:

- **A restart is legal from every state.** A node that fails half-configured
  starts over from `discovered` and converges. That *is* self-healing, and the
  test suite exercises it.
- **Nothing else moves backwards.** A node sliding from `verifying` to
  `pushing` is a server bug, and the machine refuses it rather than logging it.

Re-discovery keeps the node's first-seen time, so the provisioning SLO measures
the whole ordeal rather than the last attempt.

## Idempotency

`/config/{name}` carries `X-Devbox-Config-Hash`. A node whose config already
matches is verified, not rewritten — which is what makes a re-provision cheap
and a reconciliation loop safe. On the Python side, applying a rendered config
twice produces no second diff, and neither comment nor whitespace churn counts
as a change: a diff that always shows changes trains people to stop reading
diffs.

## SLOs

```
ztp_fabric_converged                      1 when every node is healthy
node_provision_seconds{quantile="0.95"}   the tail that matters
ztp_nodes_by_state{state="…"}             emitted at zero, so it can be alerted on
ztp_node_attempts{node="…"}               >1 under chaos means recovery worked
```

The test SDK asserts the same things:

```python
from labkit.sdk import all_nodes_healthy, provision_p95, no_egress_outside

ok, why = all_nodes_healthy(reports);  assert ok, why
assert provision_p95(reports) < 90
ok, why = no_egress_outside(destinations, allowed=["10.0.0.0/8"]);  assert ok, why
```

Each returns a verdict *and* an explanation, so a failing test names the node
rather than printing `False`.

## Chaos

Kill `ztpd` mid-provision and restart it; nodes that were half-configured come
back through `discovered` and converge. Assert on `attempts` to confirm
recovery actually happened rather than the fabric having got lucky.

© 2026 Ethan H.B. Zhou
