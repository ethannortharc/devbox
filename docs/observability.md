# Observability — the glass box

The v3 promise was "the AI can't touch your files". v4 added "…and everything
it *does* touch, you can see and govern." v5 adds "…and here is which parts of
that we actually saw."

## What is captured

| Domain | Event kinds | Signals |
|---|---|---|
| Process | `exec`, `exit` | path, argv, cwd, uid, the process tree |
| Network | `connect`, `accept`, `close` | 5-tuple; `close` carries settled bytes each way, duration, and direction |
| DNS | `dns` | query name, type, answers |
| TLS | `tls` | SNI and ALPN, from the ClientHello — no decryption |
| File | `file` | open/create/write/unlink, under a declared scope |
| API (opt-in) | `api` | plaintext HTTP, including LLM endpoints and token counts |
| Policy | `policy` | what the posture refused, and why |
| Credentials | `credential` | produced on the **host** by the broker, not by the guest agent |

Every event carries `(pid, tid, cgroup_id, box_id, ts_mono_ns, ts_wall)`, which
is what makes correlation a join rather than a guess. Since v5 it also carries
`run_id` and `attribution` when devbox could say which run it belonged to.

### Bytes are settled at close, and nowhere else

`connect`, `accept` and `tls` all fire while a connection is being established,
when nothing has crossed it yet. Reporting a byte count there would be
reporting a zero that looks like a measurement. A `kprobe/tcp_close` reads the
kernel's `bytes_sent` / `bytes_received` on the way out and emits a `close`
event carrying them, the connection's duration, and its direction. Every
consumer — the behaviour summary, the correlation chains, the console flow
table, the run report — counts bytes from `close` and only from `close`.

Consequences worth knowing:

- A connection still open when you look reads `0B`. That is the truth about a
  connection nobody has closed, not a gap.
- UDP is not settled. UDP events come from the packet tap, not a probe, so UDP
  bytes remain 0.
- A `close` whose open was never observed (the agent started mid-connection) is
  marked `orphan`: the bytes are real, but the process on the record is the one
  that *closed* the socket, which may not be the one that opened it. The report
  says so rather than adding those bytes to that process's total.

### File events have a scope

The `openat` probe fires for every path the box opens, which on a NixOS guest
means `/nix/store`, `/etc`, journald, and thousands of things nobody asked
about. The agent therefore filters file events by path prefix in user space
before they leave the box.

The default scope is `/workspace` plus the box user's home. It is set by the
host (`-file-scope`), rides in the agent's handshake, and is printed by
`devbox doctor` and in the console's capture bar, so a report can say what it
was watching:

```
file scope: /workspace, /home
```

Measured on a real box: over one minute, 1,446 file events became 90. Over an
eight-hour session, 1.8% of file events were in scope.

Two limits, both deliberate:

- **An empty scope captures everything**, and warns once at startup. A relative
  prefix is rejected at startup instead of being silently ignored — silently
  ignoring it would empty the list and let the flood back in.
- **A relative path is out of scope.** The probe records `openat`'s pathname but
  not its `dirfd`, so user space cannot resolve a relative path to a subtree.
  Guessing is worse than dropping. In a ten-minute sample, 16.5% of file events
  had relative paths and were dropped.

## How it fits together

```
in the box                          on the host
──────────                          ───────────
devbox-obsd  ──length-prefixed──▶   collector ──▶ SQLite (per box)
  eBPF programs   framed stream         ▲              │
  packet tap      Unix socket on        │              ├─▶ console (SSE)
  proc fallback   native Linux Docker;  handshake      ├─▶ devbox watch
                  authenticated exec    + version      ├─▶ behaviour diff
                  stdio for VM-backed   + source       ├─▶ run reports
                  runtimes              + file scope   └─▶ devbox export

                                    broker ──────────────▶ credential events
                                    (host-side, never in the box)
```

The host collector is a per-user background daemon, independent of the web
console. Box lifecycle commands ensure it is running; it supervises every
registered running box and owns a per-box collector claim so two concurrent CLI
or browser processes cannot write the same SQLite store. Closing the browser or
`devbox web` therefore does not stop capture. The guest agent keeps a bounded
pending queue during a transport outage; anything it or the host must discard
increments an explicit dropped-event counter.

### Which agent a box is running

A version string is not enough to answer this. Two builds of devbox from
different branches can carry the same version *and* the same commit tag while
shipping different agents — that is not hypothetical, it is how a degraded box
went unnoticed for a day. So the test is a **content hash**: devbox compares the
sha256 of the agent installed in the box against the sha256 of the agent this
binary embeds, and replaces it when they differ — at box start, at box entry,
and when the collector attaches. `devbox doctor` prints the verdict:

```
agent: devbox-obsd 0.2.0 (<commit>)
agent binary: matches host embed
```

or `stale (sha … vs …)`, `missing — no agent is installed`, or
`unverifiable — the guest has no sha256 tool`.

The same rule governs the collector daemon itself: its identity sidecar records
version, commit *and* the sha256 of the host binary, and a differing build takes
over rather than deferring to an equal version number.

`devbox doctor` also reports the daemon's last published counters, then probes
running guests for BTF, nftables, vsock/Unix-socket transport, the installed
agent, the capture source and the file scope. The daemon log is
`~/.devbox/logs/collector.log`.

## Run attribution

A run is a command devbox started; attribution decides which events belong to
it. Three rules are tried in order, and the report says how many events each
one claimed.

1. **cgroup.** The run's wrapper puts itself in an exclusive cgroup and
   publishes that cgroup's id; an event whose `cgroup_id` equals it belongs to
   the run. This is the kernel's own answer, and it is the only one that
   survives the process tree changing shape underneath you. If the wrapper
   could not get an exclusive cgroup, it records `0` rather than a shared one —
   a shared cgroup id is worse than none, because it would sweep the whole box
   into the report and call it evidence.
2. **pidtree.** An event whose pid or ppid is a known descendant of the run's
   root pid belongs to the run. The descendant set is learned as events arrive
   and released when the run ends.
3. **window.** Only for events with no usable pid (the `4294967295` sentinel the
   packet tap uses), and only when exactly **one** run's time window contains
   the timestamp. Two overlapping runs mean the event is left unattributed
   rather than assigned to a coin flip.

A real pid that belongs to no run's tree is **not** attributed by window —
otherwise one run would claim the entire box.

There is a start-of-run race the rules cannot close on their own: the cgroup
does not exist until the wrapper enters it, and the host does not learn its id
until the wrapper publishes it, by which time the command has already exec'd
and connected. Once the host learns the id it back-fills, claiming only rows
whose `cgroup_id` is exactly equal. In the run that first exposed this, the
process tree went from 2 entries to 6 and the event count from 45 to 256.

`exec` and `shell` are recorded as runs but have no wrapper — `exec` needs its
output captured, `shell`'s attach path takes no interactive flag — so they have
neither a cgroup id nor a root pid, and only the window rule can reach them.

## Reading it

```bash
devbox watch                          # recent activity
devbox watch --type dns,connect,close
devbox watch --type credential
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

The bar also names the file scope, so a narrow capture reads as narrow rather
than as quiet. The collector publishes this per box to
`~/.devbox/boxes/<name>/capture.json` (ADR-0050).

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

## Run reports

```bash
devbox runs                   # every recorded run on this box
devbox report <RUN_ID>        # md by default; --format json|html; --open
```

One model, three renderings (`src/report/{model,markdown,json,html}.rs`), so
the terminal summary, the file on disk and the console page cannot disagree.
The HTML rendering has no external references and embeds the model as
`<script type="application/json">`, so a single file is readable by a person
and by a machine. Reports are written to `~/.devbox/runs/<box>/<run>/` mode
0600 — they name every host the run reached and every file it touched.

The Coverage section is the part to read first. It carries the capture sources
that were actually live, the agent version, the attributed event count broken
down by rule, the events dropped during the run, and the count of events in the
same window that belonged to something else.

`dropped during the run` is a difference between two daemon-wide snapshots, so
concurrent activity in another box can inflate it. That is the current limit of
the metrics granularity, and it is stated rather than smoothed over.

## Behaviour diff

The behavioural analogue of `devbox diff`:

```bash
devbox behavior summary
devbox behavior diff --from 2026-09-05T14:00:00Z --at 2026-09-05T15:00:00Z
```

`--at` is the boundary between the two windows being compared: everything from
`--from` up to it is the baseline, everything after it is what is being judged.
It defaults to now, which makes the second window empty — pass one explicitly
whenever you want a real comparison. The start of a run is a good boundary.

`summary` scans newest-first and stops at the scan limit, so the default window
covers the most recent activity rather than the oldest; when it stops early it
says so and points at `--since`.

A summary is a comparable value, not a formatted string: domains contacted,
processes run, files written, traffic, policy posture, violations, API calls.
`diff` distinguishes **new behaviour** — a domain this run touched and the last
one did not — from merely doing less, because only the first is worth an alarm.

Exports: Markdown, JSON, and JSON Lines, from the CLI or
`/api/boxes/{name}/behavior?format=…`.

## Credential events

The broker runs on the host and never puts a credential in the box. Every
brokered request produces a `credential` event: provider, method, upstream host,
path with the query string stripped, upstream status, request and response
bytes, and a verdict (`allowed`, `denied`, `error`).

```bash
devbox watch --type credential
```

```
2026-09-05T15:12:25.563Z  credential pid=4294967295 credential allowed w1b-test 127.0.0.1:18090/anything 404
```

The pid is the unattributed sentinel because the request was made by the host on
the box's behalf, not by a process in the box — so these events reach a run
through the window rule. The audit row is written when the response body
finishes streaming, which is what makes its byte count real; it is written even
if the client disconnects halfway.

The upstream host is deliberately *not* folded into the box's contacted-domains
summary. It is somewhere the host went for the box, not somewhere the box went.

## Export

```bash
devbox export --run <RUN_ID> --format ocsf
devbox export --from T --to T --format otlp-json --out events.json
devbox export --format jsonl
```

| Format | What comes out |
|---|---|
| `ocsf` | OCSF 1.3, one JSON object per line. Process Activity 1007, File System Activity 1001, Network Activity 4001, DNS Activity 4003, HTTP Activity 4002, Detection Finding 2004, API Activity 6003. |
| `otlp-json` | One OTLP/JSON `ExportLogsServiceRequest`: 64-bit fields as decimal strings, enums as integers, semconv attribute names where one exists and `devbox.*` where none does. |
| `jsonl` | The canonical devbox event, unchanged. |

`--run` resolves the run to a row-id range before scanning, so exporting one run
out of a large store costs the run, not the store.

A brokered credential use is API Activity 6003: `api.operation` and
`api.service.name` name the call and the provider, `http_request.url` the
upstream host and path (the broker strips the query string before it records
anything), `status_id` folds the verdict onto Success/Failure with the reason in
`status_detail`, and `actor.session.uid` carries the run — as does
`metadata.correlation_uid`. 6003 is the one class in the mapping with no
`device` attribute, so the common envelope omits it there. The credential itself
never appears: the broker holds it and the guest never sees it.

An event kind with no honest OCSF class is counted as **unmapped** and skipped,
not filed under a neighbouring class, and the summary names the kinds it
dropped. One kind is in that position today: `syscall`, which OCSF has no class
for. `--format jsonl` always carries everything.

The invariant `matched == written + unmapped` is checked, and an export that
does not balance fails rather than printing a plausible-looking partial
record.

Where a required OCSF field has no observed value, the export says so instead of
inventing one: `tls.version` is `"Unknown"` because the agent reads only the
ClientHello, and `http_response.code` is `0` when no status was captured.

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
devbox run --posture isolated -- ./build.sh   # for one run, then back
```

`policy set` and `policy allow` apply the ruleset to a running box straight
away; on a stopped box the posture is saved and applied at start. `reprovision`
re-applies it too, since rebuilding the box rebuilds its network stack — and so
does the agent replacement path, because a `nixos-rebuild switch` tears down
devbox's nftables table on the way through. Nothing enforces `open` — it clears
devbox's table rather than leaving an empty one behind that looks like it is
doing something.

Enforcement is nftables inside the box, with the allow set kept in sync by the
agent — `devbox-obsd -policy /etc/devbox/policy.json`, which the NixOS module
passes whenever the control plane has written one. That file carries the
domains as well as the compiled ruleset, because a ruleset alone cannot be
enforced: it is default-deny with an allow set only DNS can populate. Enabling
a posture without a running agent gives you the deny half and none of the
allow half. Enforcement uses the DNS the agent is already capturing — so the
firewall learns the address from the same resolution the application is about
to use. DNS itself stays open in every posture except `isolated`: blocking it
would make an allowlist *unenforceable*, not stricter.

Under `allowlist` and `mirror-only` the credential broker's address is
implicitly allowed, for the same reason DNS is: a posture that cannot reach the
broker is a posture in which no agent can work. Under `isolated` it is blocked
along with everything else.

## Degraded modes

- **No BTF in the kernel** → `devbox-obsd -no-ebpf` polls `/proc` for processes
  and sockets. The independent packet tap still captures DNS and TLS; file-open
  fidelity and byte settlement are what is lost. Effective capture is published
  inside the box at `/run/devbox/obsd-status.json` and logged when the collector
  accepts an agent, and the Activity tab's status bar names the backends that
  actually attached — so a degraded capture says so rather than looking like a
  quiet box.
- **No CO-RE object for the guest architecture** → the build embeds the portable
  proc+packet agent and `build.rs` prints a `cargo:warning` naming the fallback.
  The objects are committed per architecture (`agent/bpf/devbox_<arch>_bpfel.{go,o}`),
  so a source build on a machine whose guests are a covered architecture embeds
  the eBPF agent; release artifacts always do.
- **Docker** → containers share the host kernel, so devbox never enables eBPF
  there. Capture is proc + packet, and byte counts stay 0.
- **No agent in the box** → boxes created before v4 never had `devbox-obsd`
  installed, and boxes created before v5 have an older one. Both are replaced by
  content hash the next time the box is started, entered, or attached to; the
  Activity status bar and `devbox doctor` say which state a box is in, and the
  underlying failure is in `~/.devbox/logs/collector.log`.

© 2026 Ethan H.B. Zhou
