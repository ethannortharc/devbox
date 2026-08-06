ALL PHASES DONE

# devbox v4 — build log

Append-only, newest entry at the bottom. One entry per committed milestone:
timestamp, phase, what landed, gate result, next step.

**Source of truth:** `docs/plans/2026-08-06-devbox-v4-design.md` (§16 is the
ordered work list, §17 the quality bar).
**Decisions:** `DECISIONS.md`.
**Branch:** `v4`, cut from `d5907d2` (see ADR-0001).

## Phase status

| Phase | Title | Status |
|---|---|---|
| 0 | Foundations | **DONE** |
| 1 | Web console: box management (retire TUI) | **DONE** |
| 2 | On-demand build / selective sets | **DONE** |
| 3 | Observability capture (Go agent + eBPF) | **DONE** |
| 4 | Observability presentation + behavior diff | **DONE** |
| 5 | Egress & activity control | **DONE** |
| 6 | Box lab: substrate & topology | **DONE** |
| 7 | Fault injection & scenario library | **DONE** |
| 8 | ZTP fabric + SoT + config-gen | **DONE** |
| 9 | Polish, docs, examples | **DONE** |

## Environment notes

Recorded once so later sessions do not re-discover them:

- Host: macOS (darwin 25.5.0), arm64. Rust 1.94, Go 1.26.2, uv 0.10.11.
- Runtimes present: **Lima**, **Docker**. Absent: Incus, Multipass, Nix.
- `clang` present (Apple), `llvm-strip` absent — `bpf2go` codegen will need
  LLVM binutils; eBPF *loading* is impossible on macOS regardless, so kernel
  paths are exercised in the privileged Linux CI job (`ebpf` in `ci.yml`) and
  the Go decode/transport layers are tested against fixtures locally.
- One pre-existing v3 sandbox is registered (`test2`, lima, stopped) and is
  what the console's dashboard renders against during development.

---

## 2026-08-06T06:40Z — Phase 0 DONE

**Landed**

- Branch `v4` cut from `d5907d2` (ADR-0001); `PROGRESS.md` + `DECISIONS.md`
  added; `.gitignore` fixed so `PROGRESS.md` is actually tracked (ADR-0002).
- Rust crate restructured as **library + thin binary** (ADR-0007) so the v4
  subsystems can be integration-tested directly.
- **Web console skeleton** (`src/web/`): axum server on loopback with port
  auto-increment, askama templates, htmx + SSE, all assets vendored and
  embedded with `rust-embed` — no CDN, no node step.
  - `devbox web [--port N] [--no-open]` starts it and opens the browser.
  - Dashboard lists boxes read from existing **v3** state, with live status.
  - SSE heartbeat drives the header "live" pill; a background watcher
    re-renders the box grid only when something actually changed, and idles
    entirely while no console is connected.
  - Auth: per-launch 256-bit token in the opened URL, exchanged once for an
    `HttpOnly; SameSite=Strict` cookie (ADR-0004).
  - JSON API: `GET /api/boxes`, `GET /api/boxes/{name}`, `GET /api/stream`.
- **Multi-language CI** (`.github/workflows/ci.yml`): Rust (fmt/clippy/test/
  release build on Linux + macOS), Go (gofmt/vet/test/golangci-lint), Python
  (ruff/ruff-format/mypy --strict/pytest), plus a privileged Linux `ebpf` lane
  that is informational until Phase 3.
- **Go tree**: single module at the repo root (ADR-0006) with
  `agent/cmd/obsd`, `ztpd/cmd/ztpd`, and `internal/buildinfo` (the agent↔host
  version handshake from §7.3), all under test.
- **Python tree**: `labkit/` packaged with uv + hatchling, ruff/mypy strict
  clean, smoke tests asserting the version cannot drift from `pyproject.toml`.
- Fixed four pre-existing `collapsible_if` clippy failures in `src/cli/code.rs`
  that a newer clippy surfaced (let-chains under edition 2024).

**Gate** — all green:

```
cargo fmt --check                            ok
cargo clippy --all-targets -- -D warnings    ok
cargo test                                   85 unit + 15 cli + 11 console = 111 passed
go vet ./... && go test ./...                ok (3 packages)
gofmt -l agent ztpd internal                 clean
ruff check . && ruff format --check .        ok
mypy .                                       ok (strict)
pytest                                       3 passed
```

**Verified by hand** — `devbox web --no-open --port 7999` served the dashboard
against the real `test2` sandbox; screenshot confirmed layout, live heartbeat,
and status pill.

**Next step** — Phase 1: wire Dashboard/Box-detail to the sandbox manager for
the full lifecycle (start/stop/create/destroy), add the browser terminal
(xterm.js ↔ WebSocket ↔ PTY), migrate the cheat sheets into a Help view, then
remove `src/tui/` and drop Zellij from the default `shell` set.

---

## 2026-08-06T07:00Z — Phase 1 DONE

**Landed**

- **Box detail page** (`/boxes/{name}`) with Overview / Files / Terminal tabs;
  an unknown tab falls back to Overview rather than 404ing.
- **Full lifecycle from the browser**: `POST /api/boxes/{name}/{start,stop,
  destroy}`. Each action re-renders the box card so button state can never
  disagree with the status beside it. Destroy keeps the CLI's
  uncommitted-overlay guard and sends the detail page back to the dashboard via
  `HX-Redirect`.
- **Browser terminal**: xterm.js ↔ WebSocket ↔ a real pty
  (`portable-pty`). Keystrokes and output are binary frames so no byte is
  mangled by UTF-8 validation; resize is a JSON control frame driving
  `TIOCSWINSZ`. New `Runtime::interactive_argv` gives the web layer the
  host-side argv without moving pty ownership into the runtime abstraction.
- **Lazy start** (§6.3): opening the Terminal tab, or the terminal socket,
  starts a stopped box.
- **Files tab**: overlay diff rendered live, degrading to "nothing to show"
  (never a 500) when the box is stopped, the runtime is missing, or the overlay
  was never provisioned.
- **Help view**: all 13 cheat sheets rendered from the same embedded markdown
  `devbox guide` serves, via `pulldown-cmark`. Nothing was lost in the move.
- **TUI + Zellij retired by replacement** (ADR-0009): `src/tui/`,
  `devbox layout`, `devbox packages`, `layouts/*.kdl`, the `layout` field
  everywhere, and the `ratatui`/`crossterm`/`dialoguer`/`indicatif`
  dependencies are gone. `zellij` left the default `shell` Nix set; its cheat
  sheet stays, marked optional. Old `state.json` files still load.
- Bare `devbox` now ensures a box for the cwd and opens the console on it
  (ADR-0005 resolved); `devbox shell` remains the browser-free path.
- **DNS-rebinding guard** (ADR-0010): non-loopback `Host` headers get 421.
- `DEVBOX_DOCKER_IMAGE` overrides the Docker base image (ADR-0011).

**Gate** — all green:

```
cargo fmt --check                            ok
cargo clippy --all-targets -- -D warnings    ok
cargo test                                   112 unit + 15 cli + 21 console
                                             + 1 docker e2e = 149 passed
go vet ./... && go test ./...                ok (3 packages)
gofmt -l agent ztpd internal                 clean
```

**e2e evidence** — `tests/e2e_docker.rs` builds a busybox-based image, creates a
real container through `Runtime`, serves the console on an ephemeral port, and
drives it end to end: list → stop → start (asserting the *runtime's* state each
time, not just the HTML) → detail → files → **a real pty over a real
WebSocket** (asserts a marker echoed back from `sh`) → destroy → gone from
state. It skips rather than fails when Docker is absent.

**Next step** — Phase 2: turn the create/edit form into a set/language/package
checklist, compose `configuration.nix` from the selection, run the rebuild, and
stream progress over SSE. Unit-test the nix-composition logic first
(`src/nix/sets.rs` already has the set catalogue).

---

## 2026-08-06T07:25Z — Phase 2 DONE

**Landed**

- `src/nix/compose.rs` — the composition logic, pure and 15-tests deep.
  `Selection` (sets + ad-hoc packages) composes a `configuration.nix` that
  imports **only** the chosen set modules, in catalogue order, byte-identically
  for the same selection. `validate()` rejects unknown sets and any attribute
  path that is not dotted `[A-Za-z0-9_-]` — that string is interpolated into a
  Nix expression evaluated as root inside the box.
- Only `system` is locked now (ADR-0012), so a minimal box is expressible.
- **Sets tab** in the console: grouped checklist with per-set package counts,
  locked entries checked-and-disabled, free-text extra packages.
- **Streamed rebuilds** (ADR-0014): POST returns `202` with a log panel and
  spawns the work; output goes to per-box SSE events and appends with
  `hx-swap="beforeend scroll:bottom"`. Persisted state is written only after
  the rebuild exits zero.
- **CLI parity** (§6.4): `devbox sets list`, `devbox sets apply --set … --package …
  [--dry-run]`, the latter printing the package-closure diff first.
- `Runtime::interactive_argv` generalized to `argv(name, cmd, interactive)`.

**Gate** — all green:

```
cargo fmt --check                            ok
cargo clippy --all-targets -- -D warnings    ok
cargo test                                   146 unit + 15 cli + 26 console
                                             + 1 docker e2e = 188 passed
go vet ./... && go test ./...                ok
```

**e2e evidence** — the Docker test subscribes to `/api/stream` before posting a
selection (as the page does), then asserts both build progress and the terminal
build status arrive over SSE from a real container.

**Next step** — Phase 3: the Go observability agent. Start with the canonical
event schema from §11.1 in Go (`agent/event`), with JSON round-trip and decoder
tests against recorded fixtures; then the Rust collector + SQLite store in
`src/obs/`; then wire the transport. eBPF *loading* cannot be tested on macOS —
that lives in the privileged Linux CI lane.

---

## 2026-08-06T08:05Z — Phase 3 DONE

**Landed**

- **One event schema, two languages.** `agent/event` (Go) and `src/obs/event.rs`
  (Rust) both decode `agent/event/testdata/events.jsonl`, which covers all ten
  §11.1 types. A field rename on either side now fails a test rather than
  silently rendering blanks.
- **Agent** (`agent/`): versioned length-prefixed transport (ADR-0015);
  fixed-layout ring-buffer decoders with byte-size assertions against the C
  structs; hand-rolled DNS and TLS ClientHello parsers (ADR-0016) covering
  compression pointers, pointer loops, and truncation; three capture sources by
  fidelity — eBPF, proc-polling, fixture replay (ADR-0017). The Go module has
  **no third-party dependencies**.
- **eBPF** (`agent/bpf`): CO-RE programs for exec, connect (v4/v6), accept, and
  openat, filtering on cgroup id *in the kernel*. Behind a build tag so a plain
  `go build` works on macOS; loading is the privileged Linux CI lane's job.
- **Collector** (`src/obs/`): unix-socket listener with the handshake, a bounded
  queue that drops **and counts**, batched SQLite writes, lifted+indexed filter
  columns with the raw event alongside, and correlation (per-process chains,
  the DNS reverse map, a depth-bounded process tree).
- NixOS module supervising the agent with `CAP_BPF` only, `ProtectSystem=strict`,
  and a CPU quota.
- CLI: `devbox watch [--type --pid --peer --path --since --tree --json]`.

**e2e evidence** — `tests/obs_pipeline.rs` builds the real agent, runs it as a
real process against the real collector, and asserts the exec→dns→connect→file
chain arrives, is queryable by every filter, and correlates — including that
the DNS answer explains the address that was connected to. A second test
asserts a box-id mismatch is refused and stores nothing.

**Known gap** — eBPF *loading* is untested locally (macOS host). The decode and
transport layers are fully covered by fixtures; kernel attach lives in the
`ebpf` CI job.

---

## 2026-08-06T08:20Z — Phase 4 DONE

**Landed**

- **Activity tab**: live stream (server-rendered first paint, htmx-refreshed,
  colour-coded by event domain), flow table (a connection and its TLS handshake
  collapse into one row, sorted by traffic), DNS log, and process tree.
- **Behaviour diff** (`src/obs/behavior.rs`): a run summary that is a
  *comparable value*, not a formatted string — domains, processes, files
  written, DNS lookups, traffic, policy posture, violations, API calls. `diff`
  distinguishes "did something new" from "did less", because only the first is
  worth warning about.
- Exports: Markdown, JSON, and JSONL, from both the API and the CLI.
- CLI: `devbox behavior summary [--since --json --jsonl]` and
  `devbox behavior diff --from <ts> [--at <ts>]`.
- **`/metrics`** (`src/metrics.rs`): Prometheus text format, hand-rolled — the
  exposition format is a dozen lines and the alternative is a dependency plus a
  global registry. Every family is declared before use, label values are
  escaped, and event types are emitted **at zero** so a series can be alerted
  on before it first fires. Scrapeable without a token (loopback + Host check
  still apply); it exposes counts and statuses, never box contents.
- Grafana dashboard in `docs/grafana/`, with the dropped-event panel red at the
  first drop — §7.3 promises drops are never *silent*, and this is where that
  promise is made visible.

**Gate** — all green:

```
cargo fmt --check / clippy --all-targets -D warnings    ok
cargo test          215 unit + 15 cli + 29 console + 2 obs + 1 docker = 262
go vet / go test / gofmt                                 ok
```

**Deferred, deliberately** — pcap export per flow (§7.5) needs the tap capture
that lands with the eBPF path; writing a pcap file with no packets to put in it
would be fabricating data. Tracked for Phase 5's tap work.

**Next step** — Phase 5: the policy engine. `src/policy/` with the four
postures, DNS-driven allowlist resolution, and an nftables driver the agent
applies; violations become `policy` events, which the behaviour summary already
knows how to read.

---

## 2026-08-06T08:50Z — Phase 5 DONE

**Landed**

- **Policy engine** (`src/policy/`), pure and 43 tests deep. Four postures with
  the semantics §8 specifies, and the details that matter:
  - A bare `github.com` covers `codeload.github.com` but **not**
    `evilgithub.com` — label-boundary matching, shared by the Rust and Go
    implementations.
  - `open` + an allowlist **flags without blocking**: the useful first step
    before enforcing.
  - `isolated` blocks even explicitly allowlisted hosts. A posture that quietly
    makes exceptions is a lie.
  - Loopback is never egress under **any** posture, so no posture can break the
    box's own services.
- **`mirror-only` curated list** (`src/policy/mirrors.rs`): ten ecosystems,
  download hosts rather than web UIs, telemetry endpoints deliberately excluded.
  The acceptance criterion — pip/npm/cargo/nix work, arbitrary hosts do not —
  is a test.
- **nftables generation** (`src/policy/nftables.rs`): idempotent (destroy then
  rebuild), devbox's own table so a flush never touches the box's own rules,
  conntrack before the set lookup, DNS permitted in every posture except
  `isolated` (blocking it would make the allowlist *unenforceable*, not
  stricter), and `log prefix "devbox-blocked "` — which is what turns a dropped
  packet into a `policy` event.
- **Agent enforcement** (`agent/policy/`): applies the ruleset, then keeps the
  named sets in sync from the DNS it is already capturing. The firewall learns
  the address from the same resolution the application is about to use, so a
  CDN rotation needs no re-resolution timer. Malformed answers are dropped
  before they can reach a command running as root — tested with the injection
  strings.
- **Cross-language list test**: the Go mirror list is parsed out of
  `mirrors.rs` and compared, so the two cannot drift.
- **Policy tab** in the console: radio postures with their own explanations, a
  paste-anything allowlist textarea, live save to `devbox.toml`, and validation
  that rejects without partially applying.
- **CLI**: `devbox policy show|set|allow|test|rules`. `test` exits non-zero on a
  denial so it is usable in a script.

**Gate** — all green: 315 Rust tests, Go vet/test/gofmt, Python ruff/pytest.

**Next step** — Phase 6: the lab. `src/lab/` with the topology schema, IPAM
address assignment, and veth/bridge wiring inside one Linux substrate.

---

## 2026-08-06T09:20Z — Phase 6 DONE

**Landed** — the lab as a chain of pure transformations with exactly one impure
step at the end: `lab.toml → Topology → Plan → commands + configs → run`.

- **Topology** (`src/lab/topology.rs`): the §9.1 schema, and validation that is
  strict on purpose. A reused interface, a self-link, an unconnected node, or a
  node name that is not interface-safe all fail *before* anything is created —
  a topology that half-comes-up wastes an hour debugging a network that was
  never going to work.
- **IPAM** (`src/lab/ipam.rs`): /31 links (RFC 3021), router loopbacks, ASNs
  from the RFC 6996 private range. Deterministic, so `lab down` then `lab up` is
  the same lab. The base prefix splits in half — links below, loopbacks above —
  so the two allocators cannot collide whatever the base happens to be, and
  every allocation is verified for overlap rather than trusted.
- **Wiring** (`src/lab/wiring.rs`): namespaces, veth pairs, addresses, link-up,
  forwarding — generated as argv vectors, in the order that matters. Tests
  assert the ordering constraints that actually break setups: namespace before
  anything moves into it, address before link-up, `lo` up in every namespace.
- **FRR** (`src/lab/frr.rs`): eBGP-unnumbered with ECMP and 3/9 timers, so a
  lab converges while you watch instead of after 30 seconds; `no bgp
  ebgp-requires-policy`, without which modern FRR shows "BGP up" and advertises
  nothing. Byte-stable output, which is what `render → diff → apply → verify`
  needs.
- **Scenario library** (§9.4): all six, embedded so `devbox lab up clos-3node`
  works from a clean install. A test loads, validates, allocates, wires, and
  renders configs for every one of them.
- **CLI**: `devbox lab list|up|down|status|config`, with `--dry-run` that works
  on any host.

**e2e evidence** — `tests/e2e_lab.rs` builds a privileged Alpine substrate, runs
the generated wiring commands **verbatim**, then asserts: every namespace
exists, every planned address is on the right interface and up, every router
loopback landed, all four directly-connected pairs ping, a *non*-adjacent pair
does **not** (proving the namespaces are really isolated rather than one flat
segment), and teardown removes everything.

**Scope stated plainly** — that test covers the wiring. Reachability *across*
the fabric needs FRR running BGP in the substrate image, which belongs to the
privileged Linux CI job; the generated config itself is covered by unit tests.

**Next step** — Phase 7: per-link netem, partition/heal/flap, and the
collective-traffic generator that makes a straggler visible.

---

## 2026-08-06T09:45Z — Phase 7 DONE

**Landed**

- **Fault injection** (`src/lab/fault.rs`): per-link netem — delay, jitter,
  loss, reorder, duplication, rate — plus partition, link down/up, and flap.
  Two decisions worth naming:
  - Faults apply to **one end** by default-able direction. A real lossy link is
    usually lossy in one direction, and a symmetric fault hides exactly the
    asymmetries worth debugging.
  - `partition` is 100% loss, not `link set down`. A downed interface tells the
    routing protocol immediately; a black-holing link does not, and the second
    is the failure that actually hurts.
  - `tc qdisc replace`, not `add`, so a second fault changes the first instead
    of failing — which is what makes a UI slider possible.
  - netem's own constraints (jitter needs delay, reorder needs delay) are
    caught with a useful message before `tc` produces a much less useful one.
- **Straggler detection** (`src/lab/straggler.rs`): §9.5's actual point. A ring
  all-reduce runs at the speed of its worst hop, so one impaired link of
  sixteen halves a whole job while every node looks healthy. Given the obs
  plane's flow records, this names the culprit link, quantifies the severity
  against the median, and says what the collective achieved versus what it
  could have. A threshold of 1.8× keeps ordinary variance from raising an alarm
  nobody would look at twice.
- **CLI**: `devbox lab fault <lab> <link> [--delay --jitter --loss --reorder
  --duplicate --rate --partition --direction]` and `devbox lab heal`.
- The scenario library (§9.4) landed with Phase 6; `fat-tree-4x2` is dual-homed
  specifically so a single lossy link shows up as a straggler rather than a
  uniform slowdown.

**e2e evidence** — the lab e2e now also partitions a real link, asserts traffic
stops, heals it, asserts traffic resumes, then applies a **one-way** 100% loss
and asserts it stays one-way.

**Next step** — Phase 8: the ZTP flagship. Python `labkit` (pydantic SoT/IPAM,
Jinja config-gen, pytest SDK) and Go `ztpd` (HTTP + provisioning state machine
+ Prometheus).

### Follow-up, same session — two defects found and fixed after the Phase 7 commit

The commit-time security review and a re-run of `clippy --all-targets` caught
three things the Phase 7 commit landed with. All are fixed in the amended
commit:

1. **Command injection through the lab name** (ADR-0023). `devbox lab up`
   writes generated FRR configs into the substrate through a shell, and the
   path contains the lab name — which `lab.toml` supplied with only an
   is-it-empty check. The lab name is now validated exactly like a node name,
   at the boundary rather than escaped at each use site.
2. **veth name collisions** (ADR-0024). Names were `{node}-{iface}` truncated
   to Linux's 15-character limit; two node names sharing a prefix produced the
   same name, and both ends of every pair live in the root namespace at once.
   Now `dvb{index}{a|b}` — unique by construction, with a test that walks 500
   links.
3. Two clippy findings (`needless_lifetimes`, `manual_is_multiple_of`).

Worth recording plainly: the Phase 7 commit was pushed before clippy finished,
so it briefly violated the "never land red" rule. The amended commit is green.

---

## 2026-08-06T10:20Z — Phase 8 DONE

**Landed** — the flagship, in the two languages the design assigns it to.

**Python `labkit`** (58 tests, ruff + mypy --strict clean):
- **Source of truth** (`labkit.sot.models`): pydantic models with
  `extra="forbid"`, so a typo'd YAML key is an error at load time rather than a
  silently ignored field. Validation refuses everything downstream cannot
  trust: duplicate serials (ZTP would provision one device as another),
  one-sided links, links to devices that do not exist, non-private ASNs, and
  loopbacks that are not /32.
- **IPAM** (`labkit.sot.ipam`): deterministic, total, /31 links, separate pools
  for links and loopbacks, invariants *verified* after every allocation rather
  than assumed.
- **Config generation** (`labkit.gen`): Jinja with `StrictUndefined`, so a
  typo'd variable is a loud error instead of a config shipping with a blank
  where a router-id belonged. **Golden-file tested** (`UPDATE_GOLDEN=1` to
  rewrite) and **idempotent**: applying twice produces no second change, and
  comment or whitespace churn is not counted as one — a diff that always shows
  changes trains people to stop reading diffs.
- **Test SDK** (`labkit.sdk`): the §10.4 assertions as pure functions —
  `all_nodes_healthy`, `provision_p95`, `bgp_fully_converged`,
  `no_egress_outside`. Each returns a verdict *and* an explanation, so a
  failure names the node. Several tests exist specifically to pin behaviour
  that could otherwise pass vacuously: zero nodes is not "all healthy", an
  empty allowlist is not "everything permitted", and an unparseable
  destination is a violation rather than a skip.

**Go `ztpd`**:
- **State machine**: `discovered → identified → rendering → pushing →
  verifying → healthy|failed`, with the two properties §10.3 actually needs. A
  restart to `discovered` is legal **from every state** — that is what
  idempotent recovery looks like under chaos — while any other backwards move
  is refused rather than logged. Re-discovery keeps `FirstSeen`, so the
  provisioning SLO measures the whole ordeal rather than the last attempt.
- **HTTP API**: `/bootstrap.sh` (plain `sh`, because a blank node has nothing
  else), `/identify`, `/config/{name}` with a content hash so a re-push of
  identical config can be skipped, `POST /status`, `GET /status`, `/metrics`.
- **Metrics**: `ztp_fabric_converged`, `node_provision_seconds{quantile=0.95}`,
  and per-state gauges emitted **at zero** so they can be alerted on before
  they first fire.
- An unknown serial is recorded as `failed` with a reason, not silently
  dropped: a device on the network the source of truth does not know about is
  exactly what an operator wants to see.

**Chaos coverage** — a test walks a node to `pushing`, fails it, and provisions
it again cleanly, asserting `attempts == 2` and that the fabric converges.

**Gate** — 421 Rust, 10 Go packages, 58 Python; every lane green.

**Next step** — Phase 9: docs, README, and the draft PR.

---

## 2026-08-06T10:45Z — Phase 9 DONE — all phases complete

**Landed** — four guides, each answering the question someone actually arrives
with rather than enumerating features:

- `docs/quickstart-v4.md` — what changed from v3, the console at a glance, the
  CLI organised by task.
- `docs/observability.md` — what is captured, how agent and collector fit
  together, and what the degraded modes really cover.
- `docs/lab.md` — the substrate model, the scenario library, and the two
  fault-injection choices worth understanding.
- `docs/ztp.md` — the flow, the language split and why, the two state-machine
  properties that make chaos recovery work.

README gains a v4 pointer and the one paragraph that explains the whole idea:
`devbox diff` is to files what `devbox behavior diff` is to what a run did.

---

# Final summary

**Phases 0–9: all DONE.** Every acceptance criterion in §16 is met or has its
gap stated explicitly below.

## Gate, as of the final commit

```
cargo fmt --check                            ok
cargo clippy --all-targets -- -D warnings    ok
cargo test                                   421 passed
go vet ./... && go test ./...                ok (10 packages)
gofmt -l agent ztpd internal                 clean
ruff check . && ruff format --check .        ok
mypy .                                       ok (--strict)
pytest                                       58 passed
```

Four of those Rust tests are end-to-end against real infrastructure:

| Test | What it actually proves |
|---|---|
| `e2e_docker` | A real container, driven through the HTTP API: list → stop → start → detail → files → **a real pty over a real WebSocket** → destroy, asserting the *runtime's* state at each step, plus build progress arriving over SSE. |
| `e2e_lab` | A real privileged Linux substrate running the generated wiring commands verbatim: namespaces, addresses, link state, loopbacks, a reachability matrix, a **non**-adjacent pair correctly failing, partition → heal, a one-way fault staying one-way, and teardown. |
| `obs_pipeline` (×2) | The **real Go agent binary**, built and run as a real process, streaming through the real handshake, framing, SQLite store, and correlation — plus a mismatched agent being refused and storing nothing. |

## What is deliberately not done, and why

- **pcap export per flow** (§7.5). Needs the tap capture that lands with the
  eBPF path. Writing a pcap file with no packets in it would be fabricating
  data, so it is not written.
- **eBPF kernel load** is untested locally — the host is macOS. The decode and
  transport layers are fully covered by fixtures; loading and attaching belong
  to the privileged Linux CI lane (`ebpf` in `ci.yml`), which is present and
  marked informational until it has a kernel to run against.
- **Cross-fabric reachability** in `e2e_lab` stops at the wiring. Reaching a
  loopback *across* the fabric needs FRR running BGP in the substrate image;
  the generated config is covered by unit tests, and the CI lane is where the
  daemon belongs.
- **`5b` interactive first-connection prompt** and **`5c` credential proxy**
  are marked stretch in §16 and were not attempted.
- **Web Lab view with a live traffic overlay** (§9, Phase 6) is not built. The
  lab's data model, address plan, and status are all exposed
  (`devbox lab status --json`), so the view is a rendering job on top of an
  API that exists — but it is not there, and the CLI is the only lab interface
  today.

## Where to start next

1. The **Lab view** in the console. `Lab::summary()` already serialises
   everything a topology graph needs, and the obs plane already has per-link
   byte counts — this is the highest-value remaining gap.
2. **Wire the collector into `devbox web`.** `src/obs/collector.rs` is
   complete and tested, but the console reads stores from disk rather than
   running a collector itself; `AppState::collector_stats` is a placeholder
   until it does, which is why `/metrics` reports zeroes for agent counters.
3. **The privileged eBPF CI lane** needs a runner with BTF to stop being
   informational.

## Decisions worth reading before changing anything

`DECISIONS.md` holds 24 ADRs. The four most load-bearing:

- **ADR-0015** — length-prefixed JSON, and why the framing matters more than
  the encoding.
- **ADR-0020** — why DNS stays open in every enforcing posture (blocking it
  makes an allowlist unenforceable, not stricter).
- **ADR-0021** — why the firewall learns addresses from observed DNS rather
  than from a re-resolution timer.
- **ADR-0024** — why veth names are indexed rather than derived from node
  names.

---

## 2026-08-06T11:30Z — Codex review round 1: 23 findings, all addressed

Ran `codex review --base main` with `gpt-5.6-sol` at `xhigh`. It found 8 P1 and
15 P2 issues. Several were things the test suite could not have caught, because
they were about how the pieces meet rather than how each behaves:

**Paths that reported success without working**

| Finding | Fix |
|---|---|
| `sets apply` wrote a file nothing imports | Write `devbox-state.toml` from the same selection (ADR-0025) |
| Streamed rebuild missing the `NIX_PATH` workaround | `rebuild_argv()`, shared by both callers |
| `sets apply` ran `nixos-rebuild` on Ubuntu boxes | Explicit guard naming `devbox nix add` instead |
| `lab up` failed on its first command (no privileges) | Every generated command is `sudo`-prefixed, with a test (ADR-0027) |
| `lab up ztp-fabric` started nothing and exited zero | Reports every service it did not start (ADR-0028) |
| Unchecking `shell`/`tools`/`editor` was a no-op | `active_sets()` respects them, per ADR-0012 |
| Extra packages vanished on the next rebuild | Persisted in `SandboxState.packages` |

**Data loss and correctness**

- `devbox policy set` on a project with a broken `devbox.toml` loaded defaults
  and saved them over the file — erasing mounts, resources, and env (ADR-0026).
- `behavior diff` without `--at` compared overlapping windows.
- `behavior summary` silently truncated at 50k events; now warns.
- `policy test` exited non-zero for a `flag` verdict, which is permitted
  traffic.
- Derived ASNs could collide with explicit ones, in **both** the Rust lab and
  the Python labkit — two eBGP peers sharing an AS never peer, so the fabric
  comes up and never converges. Both now claim explicit values first, and both
  `verify` functions check.
- An explicit link subnet like `192.0.2.1/31` was accepted as a network,
  putting the two ends in different subnets.
- Python IPAM moved an explicitly declared address to whichever end came first;
  it now stays where it was declared, and conflicting declarations are refused.
- Two concurrent Sets submissions each overwrote the other's config and then
  persisted their own selection; one rebuild per box at a time now.

**Security**

- The collector accepted events naming a **different** box than the one that
  handshook, letting a compromised agent write into another box's timeline.

**Go**

- The DNS→nftables enforcer marked an answer "seen" before nft accepted it, so
  a transient failure blocked an allowlisted domain permanently.
- `ztpd -metrics` was parsed, documented, and never listened on.
- The ZTP registry was in-memory only, so the documented chaos case lost
  attempt counts and first-seen times — making the SLO it advertises fiction.
  Now persisted atomically, with a test.
- The bootstrap script rewrote and restarted FRR on every rediscovery instead
  of comparing first, which is what made "idempotent" untrue in practice.

**All 23 fixed.** The last one — `proc` capture advertising `connect` coverage
it did not deliver — is now implemented: the poll loop reads `/proc/net/tcp`
and `tcp6`, emits one event per newly established socket, deduplicates on the
5-tuple so a long-lived connection is not re-reported every sweep, and
tolerates a kernel with no IPv6. The honest limitation is stated where it
lives: `/proc/net/tcp` has no pid column, so these events carry the connection
without process attribution. eBPF gets both, which is why it is the default.

**Gate after the fixes** — 373 Rust unit + 52 integration/e2e, 10 Go packages,
62 Python; fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-06T14:10Z — Codex review round 2: 23 findings, all addressed

A second pass at `xhigh` found 23 more (3 P1, 20 P2). The three P1s were all
the same failure mode, and it is the one worth naming: **a feature that reports
success without doing the work.**

- `nix/devbox-module.nix` force-installed `shell`, `tools`, and `editor`, and
  never read `custom_packages`. Unchecking a set in the console wrote the right
  file, rebuilt successfully, and installed the old selection. Only `system` is
  locked now (ADR-0029), which is what §6.4 always said.
- `write_set_modules` regenerated `devbox-state.toml` from `DevboxConfig::
  default()` — a fix I made in round 1 — which dropped the `[user]` and
  `[sandbox]` sections the in-box module reads. Every Sets apply silently reset
  the guest username to `dev` and the mount mode to `overlay`. It now reads the
  box's existing state back and preserves both.
- Egress postures were never enforced. `devbox policy set isolated` saved the
  posture and printed "apply it with `devbox reprovision`"; no provisioning
  path generated or loaded a ruleset. The box reported `isolated` and had
  unrestricted egress. `policy::enforce` closes it (ADR-0030).

The P2s clustered into four groups. **Ordering**: `ts_mono_ns` resets at guest
boot but the store persists across boots, so any summary spanning a reboot came
out backwards; and pids are recycled, so process chains merged unrelated
processes (ADR-0031). **Durability**: ZTP saved on a 2s ticker, losing exactly
the writes the chaos test kills the process to exercise (ADR-0032).
**Allocation**: both IPAMs let automatic allocation hand out a prefix an
explicit link already held, and both verified overlap by comparing subnet
strings — which a `/29` containing a `/31` passes. **eBPF**: six real capture
bugs, including reading the socket at `tcp_v*_connect` *entry*, before the
kernel has filled in the destination, so every outbound flow decoded as zeroes.

The eBPF ones deserve a note on why they survived two rounds of testing: the C
is `//go:build ignore` and only compiles in the privileged Linux lane, so the
Go and Rust test suites — which run against the fixture capture source — cannot
see them. That is the cost of the decision in ADR-0017, and it is the right
trade, but it means the eBPF source needs review rather than tests.

**Gate after the fixes** — 378 Rust unit + 52 integration/e2e, 10 Go packages,
64 Python; fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-06T15:40Z — Codex review round 3: 14 findings, all addressed

Round 3 is mostly the bill for round 2, and it is the right bill to get. Making
enforcement real turned a dormant subsystem into a live one, and live
subsystems have failure modes that dormant ones do not.

The sharpest finding: **`allowlist` and `mirror-only` blocked exactly what they
promise to permit.** An allowlist names domains; nftables matches addresses.
The generated ruleset is default-deny with `allow_v4`/`allow_v6` seeded only
from literal CIDRs, and the only code that could add resolved answers — Go
`Enforcer.OnDNS` — existed, was tested, and was never instantiated by anything.
Before round 2 that was invisible, because no ruleset was ever loaded. After
round 2 it would have broken every allowlisted box. `devbox-obsd -policy` now
runs the enforcer (ADR-0033).

Two findings were the same shape as ones I had already fixed, one layer out:
the posture was applied where it was *set* but not in the *start* lifecycle, so
it lasted until the first reboot (ADR-0034); and the web Sets path still did
not `ensure_running` after I fixed the CLI path last round. Both are worth
noting as a pattern — fixing the path in front of me rather than the lifecycle
the path belongs to.

The rest: per-mutation ZTP saves made concurrency real and every save wrote the
same `.tmp` path; `ai-code.nix`/`ai-infra.nix` were being regenerated as flat
lists, discarding the `tryEval` guards that exist because those tools are
sometimes absent (ADR-0035); dotted package paths were read as nested TOML
tables; behaviour diffs compared violation *counts*, so one violation against A
and one against B read as no change; and `edge-a-edge-b` split on the first
hyphen. Also, `ruff format --check` failed on the test I added last round —
the CI lane would have gone red on the first push.

**Gate after the fixes** — 381 Rust unit + 52 integration/e2e, 10 Go packages,
64 Python; fmt/clippy/vet/gofmt/ruff/mypy all clean.
