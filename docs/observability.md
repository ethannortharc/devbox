# Observability — the glass box

The v3 promise was "the AI can't touch your files". v4 adds "…and everything it
*does* touch, you can see and govern."

## What is captured

| Domain | Signals |
|---|---|
| Process | `execve` (path, argv, cwd, uid), exit, the process tree |
| Network | TCP/UDP connect and accept, 5-tuple, bytes, duration |
| DNS | query name, type, answers |
| TLS | SNI and ALPN, from the ClientHello — no decryption |
| File | open/create/write/unlink under the workspace |
| API (opt-in) | plaintext HTTP, including LLM endpoints and token counts |

Every event carries `(pid, tid, cgroup_id, box_id, ts_mono_ns, ts_wall)`, which
is what makes correlation a join rather than a guess.

## How it fits together

```
in the box                          on the host
──────────                          ───────────
devbox-obsd  ──length-prefixed──▶   collector ──▶ SQLite (per box)
  eBPF programs   framed stream         ▲              │
  packet tap      Unix socket on        │              ├─▶ console (SSE)
  proc fallback   native Linux Docker;  handshake      ├─▶ devbox watch
                  authenticated exec    + version      └─▶ behaviour diff
                  stdio for VM-backed
                  runtimes
```

The agent is version-pinned to the host binary; a mismatched agent is refused
rather than decoded with the wrong layout.

The host collector is a per-user background daemon, independent of the web
console. Box lifecycle commands ensure it is running; it supervises every
registered running box and owns a per-box collector claim so two concurrent CLI
or browser processes cannot write the same SQLite store. Closing the browser or
`devbox web` therefore does not stop capture. The guest agent keeps a bounded
pending queue during a transport outage; anything it or the host must discard
increments an explicit dropped-event counter.

The daemon publishes pid and devbox version through an atomically replaced
identity sidecar. After an upgrade, the next lifecycle command verifies that
the recorded process is a `devbox __collector`, terminates the outdated daemon,
waits for its ownership lock to be released, and starts the matching binary.
Replacement is bounded; an unreadable identity, unresponsive runtime, or
failed signal is warned about but never blocks the user's lifecycle command.
The next command retries once the old collector or runtime recovers.

`devbox doctor` reports the daemon identity and its last published counters,
then probes running guests for BTF, nftables, vsock/Unix-socket transport, and
the installed agent version. The daemon log is
`~/.devbox/logs/collector.log`.

## Reading it

```bash
devbox watch                          # recent activity
devbox watch --type dns --type connect
devbox watch --peer pypi.org
devbox watch --tree                   # grouped by process
devbox watch --json                   # JSON Lines
```

The console's **Activity** tab reads the same store in three layers.

**Is capture working.** A status bar above everything else, because an empty
timeline has four unrelated causes and they need different responses:

| Bar | What it means | What to do |
|---|---|---|
| `Capturing · eBPF` | Kernel probes attached; everything is seen at the syscall boundary | — |
| `Capturing · proc + packet` | No probes on this runtime; short-lived processes can be missed | see [degraded modes](#degraded-modes) |
| `Box is not running` | Nothing to attach to | start the box |
| `The agent could not start` | The agent's own last line of stderr, plus the command that fixes it | usually `devbox reprovision` |
| `Collector is not running` | No host collector owns the lock; nothing is being recorded for any box | `devbox doctor` |

The collector publishes this per box to `~/.devbox/boxes/<name>/capture.json`
(ADR-0050). A box created before v4 has no agent installed, which used to
surface only as six days of identical warnings in `collector.log`.

**What the window looks like.** An event-density strip coloured by the dominant
kind of activity per column, over the loaded window, with the counters that
matter above it — events, processes, peers, bytes each way, and connections
policy refused.

**The events.** Seven views over one load, so switching between them costs no
request: the live stream, a **Peers** rollup (one row per destination, refused
peers included and sorted first), the flow table, the DNS log, the process
tree, file writes, and **Policy** — every connection policy refused, promoted
out of the behaviour summary it used to be buried in.

The stream filters by domain and by free text over process, peer, summary and
path. Filtering runs *after* DNS correlation, so `pypi` matches the connection
that only ever recorded `151.101.0.223`; filtering in SQL would discard the
lookup that names it (the trap `devbox watch --peer` documents).

New events arrive by themselves: the console watches the store and signals the
page, which fetches only what it is missing and prepends it (ADR-0049). A quiet
box generates no traffic at all. **pause** holds the stream while you read.

## Behaviour diff

The behavioural analogue of `devbox diff`:

```bash
devbox behavior summary
devbox behavior diff --from 2026-08-06T20:00:00Z --at 2026-08-06T22:00:00Z
```

`--at` is the boundary between the two runs being compared: everything from
`--from` up to it is the baseline, everything after it is what is being judged.
It is required, because there is no honest default — a boundary of "now" leaves
the second window empty and reports that all behaviour disappeared. Take one
from `devbox behavior list`; the start of a run is a good boundary.

A summary is a comparable value, not a formatted string: domains contacted,
processes run, files written, traffic, policy posture, violations, API calls.
`diff` distinguishes **new behaviour** — a domain this run touched and the last
one did not — from merely doing less, because only the first is worth an alarm.

Exports: Markdown, JSON, and JSON Lines, from the CLI or
`/api/boxes/{name}/behavior?format=…`.

## Live pcap

Stored flow rows deliberately contain metadata, not packet payloads. A pcap
request therefore launches a new, bounded AF_PACKET capture inside the running
box and selects the requested TCP or UDP five-tuple:

```bash
devbox behavior pcap mybox \
  --proto tcp --saddr 10.0.0.2 --daddr 93.184.216.34 --dport 443 \
  --seconds 5 --packets 64 --output handshake.pcap
```

The Activity flow table exposes the same operation as a **pcap** download. The
host validates the classic-pcap header, version, Ethernet link type, snaplen,
and every packet record before returning it. An idle window is an honest
header-only capture; a populated download contains packets observed at request
time, never reconstructed events.

## Metrics

`/metrics` on the console, in Prometheus text format, with no token required
(it carries counts and statuses, never box contents). Import
[`grafana/devbox-observability.json`](grafana/devbox-observability.json).

Watch `devbox_events_dropped_total` first. §7.3 promises events are never
*silently* dropped; anything above zero means the guest queue or collector fell
behind and the timeline has a hole in it. The background collector keeps this
window open without requiring a console session.

## Policy

Observation becomes governance. Four postures:

| Posture | Behaviour |
|---|---|
| `open` | Nothing blocked. With an allowlist set, out-of-policy connections are **flagged** — the useful first step. |
| `allowlist` | Default-deny. Only listed domains and CIDRs. A bare `github.com` covers its subdomains; `evilgithub.com` is not a subdomain. |
| `mirror-only` | Package mirrors and git hosts only. `pip`, `npm`, `cargo`, `nix` work; nothing phones home. |
| `isolated` | No egress. Loopback only, plus any private prefixes a service hosted in the box declares under `/etc/devbox/prefixes/` — this overrides hosts you explicitly allowlisted. |

```bash
devbox policy set mirror-only
devbox policy allow api.anthropic.com
devbox policy test telemetry.example.com   # non-zero exit when denied
devbox policy rules                        # the nftables ruleset it generates
```

`policy set` and `policy allow` apply the ruleset to a running box straight
away; on a stopped box the posture is saved and applied at start. `reprovision`
re-applies it too, since rebuilding the box rebuilds its network stack. Nothing
enforces `open` — it clears devbox's table rather than leaving an empty one
behind that looks like it is doing something.

Enforcement is nftables inside the box, with the allow set kept in sync by the
agent — `devbox-obsd -policy /etc/devbox/policy.json`, which the NixOS module
passes whenever the control plane has written one. That file carries the
domains as well as the compiled ruleset, because a ruleset alone cannot be
enforced: it is default-deny with an allow set only DNS can populate. Enabling
a posture without a running agent gives you the deny half and none of the
allow half. Enforcement uses the DNS the agent is already capturing — so the firewall learns the address
from the same resolution the application is about to use. DNS itself stays open
in every posture except `isolated`: blocking it would make an allowlist
*unenforceable*, not stricter.

## Degraded modes

- **No BTF in the kernel** → `devbox-obsd -no-ebpf` polls `/proc` for processes
  and sockets. The independent packet tap still captures DNS and TLS; file-open
  fidelity is what is lost. Effective capture is published inside the box at
  `/run/devbox/obsd-status.json` and logged when the collector accepts an
  agent, and the Activity tab's status bar names the backends that actually
  attached — so a degraded capture says so rather than looking like a quiet box.
- **Source build** → the embedded Linux agent is the portable proc+packet
  build on every host. Official release artifacts embed the generated CO-RE
  agent; on macOS that agent runs inside the Lima guest where eBPF is available.
  `devbox doctor` makes the effective prerequisites visible instead of asking
  you to infer capture fidelity from an empty timeline.
- **No agent in the box** → boxes created before v4 never had `devbox-obsd`
  installed. The Activity status bar says so and names `devbox reprovision`;
  the underlying failure is in `~/.devbox/logs/collector.log`.

© 2026 Ethan H.B. Zhou
