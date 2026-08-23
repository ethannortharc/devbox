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

The built-in scenario creates a service namespace, one preconfigured spine and
blank leaves on explicit /30 bootstrap links. `/31` remains the default for
router-to-router links, but DHCP needs distinct network, router, client and
broadcast addresses. `lab up` starts DNS/NTP, installs and supervises the
embedded `devbox-ztpd`, starts per-link DHCP, runs a real `udhcpc` client on
each blank node, verifies option 67, and only then launches the downloaded
bootstrap script.

Success means more than nodes posting `healthy`: devbox waits for the operator
status API to report every catalogued serial healthy, then runs the full routed
reachability matrix. DNS resolution, NTP reachability, live FRR processes and
BGP establishment are part of the node self-check. Any failure leaves the
service and bootstrap logs named in the error.

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

The production binary embeds both Linux executables (`devbox-obsd` and
`devbox-ztpd`) and release CI builds architecture-matched amd64/arm64 artifacts.
Source builds compile portable Go versions automatically.

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

## Watching it happen

`/labs/ztp-fabric` in the console shows the state machine as it runs:

- a verdict — **converged** or not — with healthy against *expected*, failures,
  serials never seen, and the provisioning p95 the SLO is written against;
- a node table with each serial's state, attempt count, config hash and failure
  reason, ordered so a node that needs attention reads first;
- a topology whose blank nodes change colour as they provision. Three grey
  outlines going green is the demonstration.

Seven states are drawn as four: healthy, failed, waiting, and working. The
question a reader has is whether a node is done, moving, or stuck, and an
unrecognised state counts as working — a `ztpd` from another release must not
paint a healthy fabric red.

A serial the source of truth expects and the registry has never heard from is
shown too, as `not seen`. Rendering only the nodes that identified is exactly
how a fabric with a dead node reads as complete.

The console does not dial `ztpd`. The operator listener stays where it is —
loopback inside the service namespace — and the page asks the substrate to
fetch the status over the same exec channel every other lab operation uses
(ADR-0051).

Import [`grafana/devbox-ztp.json`](grafana/devbox-ztp.json) for convergence,
p95 provisioning time, missing/failed nodes, unknown serials and retry counts.
The operator listener is loopback-only inside the service namespace by default;
run the scraper in that namespace or deliberately bind `-metrics` to a separate
management address. Do not expose the inventory routes on the provisioning
listener.

## Chaos

Kill `ztpd` mid-provision and restart it; nodes that were half-configured come
back through `discovered` and converge. Assert on `attempts` to confirm
recovery actually happened rather than the fabric having got lucky.

© 2026 Ethan H.B. Zhou

## Two listeners, and why

`ztpd` serves two disjoint sets of routes on two addresses.

| Listener | Default | Routes |
|---|---|---|
| provisioning (`-listen`) | `:8080` | `GET /bootstrap.sh`, `POST /identify`, `GET /config/{name}`, `POST /status`, `GET /healthz` |
| operator (`-metrics`) | `127.0.0.1:9090` | `GET /metrics`, `GET /status` |

The provisioning listener sits on the network blank devices boot from, and
everything on it is a route a booting node needs. `GET /status` and `/metrics`
are not: both return the fabric inventory — serials, names, roles, states,
config hashes — and neither is authenticated, because the operator listener is
expected to be on a management network.

Two things follow, and both have been gotten wrong once already:

- **A new operator route goes on the operator listener.** Adding it to the
  shared mux puts it on the provisioning network too. `/metrics` was moved off
  and `/status` was left behind, which achieved nothing until the second was
  moved as well.
- **The operator listener defaults to loopback deliberately.** A bare `:9090`
  binds every interface, including the provisioning one, so a separate *port*
  is not separation on a multi-homed host — and a ZTP server is multi-homed by
  definition. Passing `-metrics 10.0.0.5:9090` to bind a management address is
  the supported way to expose it; passing `-metrics :9090` undoes the split.

`api.ProvisioningHandler()` and `api.Handler()` are the two route sets, and a
test asserts the inventory routes are absent from the first.
