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
  eBPF programs   frames over          ▲              │
  DNS/TLS parse   a unix socket        │              ├─▶ console (SSE)
  proc fallback                    handshake          ├─▶ devbox watch
                                   + version          └─▶ behaviour diff
```

The agent is version-pinned to the host binary; a mismatched agent is refused
rather than decoded with the wrong layout.

## Reading it

```bash
devbox watch                          # recent activity
devbox watch --type dns --type connect
devbox watch --peer pypi.org
devbox watch --tree                   # grouped by process
devbox watch --json                   # JSON Lines
```

The console's **Activity** tab shows the same data four ways: a live stream
colour-coded by domain, a flow table (one row per connection, with the TLS
handshake folded in), the DNS log, and the process tree.

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

## Metrics

`/metrics` on the console, in Prometheus text format, with no token required
(it carries counts and statuses, never box contents). Import
[`grafana/devbox-observability.json`](grafana/devbox-observability.json).

Watch `devbox_events_dropped_total` first. §7.3 promises events are never
*silently* dropped; anything above zero means the collector fell behind and the
timeline has a hole in it.

## Policy

Observation becomes governance. Four postures:

| Posture | Behaviour |
|---|---|
| `open` | Nothing blocked. With an allowlist set, out-of-policy connections are **flagged** — the useful first step. |
| `allowlist` | Default-deny. Only listed domains and CIDRs. A bare `github.com` covers its subdomains; `evilgithub.com` is not a subdomain. |
| `mirror-only` | Package mirrors and git hosts only. `pip`, `npm`, `cargo`, `nix` work; nothing phones home. |
| `isolated` | No egress. Loopback and lab-internal only — including for hosts you explicitly allowlisted. |

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

- **No BTF in the kernel** → `devbox-obsd -no-ebpf` polls `/proc`. It sees
  processes and sockets, not DNS, TLS, or file access. The console marks which
  source is in play, so a quiet timeline is never mistaken for a quiet box.
- **macOS host** → the agent runs inside the Lima guest, where eBPF works
  normally.

© 2026 Ethan H.B. Zhou
