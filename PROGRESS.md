ALL PHASES DONE
IMPLEMENTATION COMPLETE — ALL V4 PHASES GREEN; PR-READY

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
| 9 | Polish, docs, examples | **DONE** — external PR creation awaits explicit authorization |

## v5 wave status

v5 is built in waves of parallel task branches, each merged into `v5-main` after
the full gate. The v5 build log starts at "devbox v5 — build log" below.

| Wave | Task | Branch | Status |
|---|---|---|---|
| W0 | W0-1 merge `origin/main` (70 commits) | `v5/merge-main` | **DONE** |
| W0 | W0-2a remove labs, ZTP and labkit | `v5/lab-removal` | **DONE** |
| W0 | W0-2b archive the lab code with its history | separate repo | **DONE** |
| W0 | W0-3 commit CO-RE objects; embed the eBPF agent in a source build | `v5/ebpf-local` | **DONE** |
| W0 | W0-4 positional box name across the CLI | `v5/cli-names` | **DONE** |
| W0-5 | W0-5a SNI read across TCP segments | `v5/tls-sni` | **DONE** |
| W0-5 | W0-5b byte counts settled at `tcp_close` | `v5/bytes` | **DONE** |
| W0-5 | W0-5c replace a box's agent by content hash | `v5/agent-update` | **DONE** |
| W1 | W1-A run evidence: runs, attribution, reports | `v5/run` | **DONE** |
| W1 | W1-B credential broker: keychain, proxy, scopes, audit | `v5/broker` | **DONE** |
| W1 | W1-D MCP servers inside a box | `v5/mcp` | **DONE** |
| W1 | W1-E overlay checkpoints | `v5/checkpoint` | **DONE** |
| W1 | W1-G export as OCSF 1.3 and OTLP/JSON | `v5/export` | **DONE** |
| W2 | W2-0 file events get a scope | `v5/file-scope` | **DONE** |
| W2 | W2-1 wire A to E, B, G; newest-first summary; `export --run` | `v5/run` | **DONE** |
| W2 | W2-2 `mcp run` as a run, `mcp report`, `mcp self` | `v5/mcp` | **DONE** |
| W2 | W2-3 sweep: discard remount, `checkpoint-rm`, guest home, NixOS file scope | `v5/sweep` | **DONE** |
| W2 | W2-4 README, docs, screenshot, ADR-0059…0067, version 0.2.0 | `v5/docs` | **DONE** — 0.2.0's code surface is final at `db750b3` |
| W2 | W2-5 run report polish: fold the wrapper, writes outside the overlay, wire Credentials | `v5/report-polish` | **DONE** |
| W2 | W2-6 redact credentials out of every argv devbox records or renders | `v5/redact` | **DONE** |
| W2 | W2-7 a brokered credential use is OCSF API Activity 6003 | `v5/redact` | **DONE** |

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
  fidelity — eBPF, proc-polling, fixture replay (ADR-0017). It uses
  `cilium/ebpf` for the generated CO-RE loader and `x/sys` for packet capture.
- **eBPF** (`agent/bpf`): CO-RE programs for exec, connect (v4/v6), accept, and
  openat. The current one-box-per-VM loader traces its whole guest kernel; the
  cgroup selection map exists but is not populated selectively. Behind a build
  tag so a plain `go build` works on macOS; loading is the privileged Linux CI
  lane's job.
- **Collector** (`src/obs/`): unix-socket listener with the handshake, a bounded
  queue that drops **and counts**, batched SQLite writes, lifted+indexed filter
  columns with the raw event alongside, and correlation (per-process chains,
  the DNS reverse map, a depth-bounded process tree).
- NixOS module supervising the agent with bounded BPF/perf, network, and kmsg
  capabilities, `ProtectSystem=strict`, and a CPU quota.
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

# Historical 2026-08-06 status summary

This was the completion assessment at that checkpoint. Later end-to-end review
found the product-surface gaps listed below; they have since been completed.
The phase table at the top is the current authority.

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
- **eBPF kernel load** is untestable on the local macOS host. The mandatory
  privileged Linux CI lane builds the CO-RE object and exercises real attach;
  local Linux-tagged compilation runs in a container.
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

1. Add the missing **create-box flow** and **Lab view** in the console.
   `Lab::summary()` already serialises
   everything a topology graph needs, and the obs plane already has per-link
   byte counts — this is the highest-value remaining gap.
2. Implement **pcap export per flow** from the live packet tap.
3. Add an FRR-enabled integration image so the e2e lab gate covers routed
   cross-fabric reachability, not only namespace wiring.

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

## 2026-08-06T17:20Z — Codex review round 4: 27 findings, and a note on convergence

Round 4 returned **more** findings than round 3, not fewer: 27 (17 P1, 10 P2)
against round 3's 14. Four rounds in, the counts are 23, 23, 14, 27. That is
worth stating plainly rather than burying, because it changes what "review
until clean" means.

The reviewer is not repeating itself — almost every round-4 finding is in
territory earlier rounds did not reach (nftables rule semantics, collector
framing, ZTP concurrency depth, lab topology validation, ASN range checking).
Each round the previous round's fixes become new surface to audit. At `xhigh`
against a patch this size, a genuinely empty round is not obviously reachable.

The most important finding was the hole under my own round-3 fix. Wiring the
DNS enforcer into `devbox-obsd` made the *agent* correct, but nothing
provisions the agent: no path pushes the binary, imports the module, or enables
the service. So a domain allowlist loaded a default-deny ruleset whose allow
set nothing could fill — **the user asks for less egress and gets none**, while
the console reports the posture applied. That is the dangerous direction of
wrong.

The fix is a refusal, not a workaround (ADR-0036). `enforce::apply` probes for
a running agent and declines a domain-based posture without one, naming what
would have been blocked. CIDR allowlists, `isolated`, and `open` need no agent
and still apply. Shipping the agent into every box is real work — a binary to
embed, a module to import, a service to supervise, a version pin to honour —
and doing it badly under review pressure would be worse than an honest refusal
that names the gap. The obsd module now says so at the top.

Three more were the same mistake in three places: reporting a failure where the
caller could not act on it (ADR-0037). ZTP acknowledged nodes whose state had
not been written; `policy set` printed enforcement errors and exited 0; and
restoring a posture used `load_or_default`, so a malformed `devbox.toml` became
posture `open` — corruption silently unfirewalling a box.

**Addressed this round:** every P1 except the five nftables-semantics and
counter items listed below, and every P2 except three.

**Knowingly deferred, with reasons:**

- *Ship the agent in provisioning* — the largest remaining gap, and the reason
  domain allowlists are refused rather than broken. Needs its own change.
- *Restrict DNS to trusted resolvers*, *limit local exemptions to real lab
  destinations*, *revalidate established flows when tightening*, *emit policy
  events for real drops* — all real hardening, all beyond what §8 specifies.
  They change enforcement semantics and deserve a design decision, not a
  reflex fix at the end of a review round.
- *Ring-buffer and dropped-batch counters* — worth having; neither changes
  behaviour, and the eBPF half cannot be tested here (ADR-0017).
- *Fixed batching deadline*, *DNS allow-set expiry*, *capture selection in the
  module*, *materializing hosts in Python IPAM* — small, real, not urgent.

**Gate** — 382 Rust unit + 52 integration/e2e, 10 Go packages, 64 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-06T19:05Z — Codex review round 5: 9 findings, all addressed

Nine (6 P1, 3 P2), down from twenty-seven. The series now reads **23, 23, 14,
27, 9**, which corrects what I wrote after round 4: the spike was the reviewer
reaching new territory once, not evidence that the loop cannot converge. Round
5 stayed inside areas earlier rounds had already touched, which is what
convergence looks like.

Two findings mattered more than their line count.

**`lab up` reported success while BGP never started.** Every `frr.conf` was
written under a comment claiming "the routing daemon is started by the node's
own service manager, which the substrate provisioning installs." Neither half
was true — a network namespace has no init, and nothing installed one. A routed
lab therefore came up with adjacent nodes able to ping and non-adjacent
loopbacks never converging; the only symptom was a scenario quietly failing its
assertions (ADR-0038). The comment is the lesson: it described an architecture
nobody had built, and read plausibly enough to survive four review rounds.

**The agent guard I added in round 4 checked the wrong thing.** It verified
`devbox-obsd` was running, but the degraded proc source captures no DNS, so an
agent started with `-no-ebpf` passed while being unable to fill a single
allow-set entry — recreating the exact default-deny-with-empty-allowlist the
guard exists to prevent (ADR-0039).

Four more were mine from round 4, all the same shape — a lifecycle case I did
not cover. A newly created box is already `Running`, so gating enforcement on
the `Stopped` arm left it unrestricted until its first restart. `reprovision`
read a malformed `devbox.toml` as posture `open` immediately after rebuilding
away the firewall. A *failed* status probe was treated as "stopped", exiting 0
with the posture saved and the running firewall untouched. And the duplicate
`package` option I added made the obsd module fail to evaluate at all.

The remaining one was not mine but is the sharpest ZTP finding yet: the
bootstrap script discarded status reports with `|| true`. Losing the final
`healthy` left the registry and `ztp_fabric_converged` stale permanently,
because nothing schedules reconciliation — in precisely the chaos case §10.3
exists to exercise. It now retries across a restart window.

**Gate** — 383 Rust unit + 52 integration/e2e, 10 Go packages, 64 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-06T21:15Z — Codex review round 6: 14 findings, all addressed

Series so far: **23, 23, 14, 27, 9, 14.**

Three of my round-5 fixes had gaps, and the review found each. The pattern is
worth naming, because it has now repeated four rounds running: **fixing a thing
exposes the layer under it, and I keep stopping at the first layer.**

- `frr::start_commands` used the bare node name; wiring creates
  `devbox-{lab}-{node}`. My "FRR now starts" fix would have failed with
  "namespace not found" on every routed lab — a silent non-start replaced by a
  loud one.
- Even fixed, nothing installs `frr`, so it would only have moved to
  command-not-found. Same for `conntrack`: the flush I added to make tightening
  take effect was silently skipped everywhere, because no package set provided
  the binary. Both are provisioned now.
- The ZTP retry I added made duplicate reports possible, and the state machine
  rejected `from == to` as a regression — so after a lost response the
  bootstrap retried into 409 forever. I converted a dropped report into a
  guaranteed failure.

The policy lifecycle produced the same shape it has since round 2: **the web
path one round behind the CLI.** The Policy tab now applies as well as saves,
`start_box` enforces for an already-running box, and `open` clears what a
previous posture installed rather than returning early and leaving those rules
in force.

One fix caused a regression that testing caught rather than review: clearing on
every start ran `sudo`, which a container exec neither has nor needs, so boxes
refused to start. Privilege is decided in the guest now (ADR-0040) — the only
place that knows.

A UX decision worth recording: when the save succeeds and the apply fails, the
Policy tab returns 200 with both facts rather than a 500 (ADR-0041). The policy
really is saved, and a user told only "error" would not know whether to
re-enter their allowlist.

**Gate** — 384 Rust unit + 52 integration/e2e, 10 Go packages, 64 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-06T23:40Z — Codex review round 7: 11 findings, all addressed

Series: **23, 23, 14, 27, 9, 14, 11.**

Two findings were the shape that has dominated this whole review: something
reporting success while the thing it claims is not true.

**DNS-derived allow entries never expired.** Every address an allowlisted
domain ever resolved to stayed permitted for the life of the box, so a CDN
address later reassigned to someone else remained reachable — a default-deny
posture quietly widening the longer it ran (ADR-0042). Stated CIDRs still never
expire, and that asymmetry is deliberate: an address the user wrote down is a
decision, an address the agent inferred from a DNS answer is an observation,
and observations should not outlive their evidence.

**ZTP declared convergence over the nodes it had seen.** A node that never
boots is invisible to `Summarize()`, so nineteen healthy out of an expected
twenty read as `converged: true` — the one answer a convergence signal must
never get wrong, since a test and an operator both key off it. One level down,
a node counted as healthy whenever `show bgp summary` exited zero, which it
does whenever bgpd is answering even with every neighbour Idle (ADR-0043).

Four more were mine from earlier rounds, all follow-ups to fixes that stopped
one layer short: `correlate` ordered chains by uptime after being careful to
*group* by wall clock; it resolved parents by pid alone after deliberately
splitting reused pids into incarnations; `reprovision` re-added `ai-code` and
persisted it, so disabling the set could not be made to stick; and
`EDITOR=nvim` was exported on boxes where the editor set was unchecked.

I hit the pattern live while fixing the `-once` flag. The obvious wiring —
`Loop: !once` — made fixture replay loop forever by default and hung the test
suite: I changed the default for every existing caller in order to give effect
to a flag nobody had wired. Backed out; `-once` now means what its help text
already said. Worth recording that I caught it because tests hung, not because
I reasoned it through.

**Added unprompted:** a Go test that parses `ALLOW_TTL_SECS` out of
`nftables.rs` and fails if the agent's window and the kernel's timeout drift
apart. Verified by breaking it deliberately. If the agent's window were the
longer of the two, a domain in continuous use would be blocked for the gap —
failing closed on traffic already allowed, an hour into a run. Same technique
as the mirror-list guard (ADR-0022), same reason: one home for the constant.

**Gate** — 384 Rust unit + 52 integration/e2e across all 8 suites, 10 Go
packages, 65 Python; fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-07T02:30Z — Codex review round 9: 13 findings, all addressed

Series: **23, 23, 14, 27, 9, 14, 11, 13, 13.**

The finding worth reading twice: **I wrote two ADRs that contradicted each
other, and shipped the contradiction.** ADR-0034 said an enforcement failure
must not be fatal — refusing to start a box whose firewall failed "would strand
the user with no way in to fix it." ADR-0037 then made it fatal, because a
caller that cannot see a failure cannot act on it. Round 9 found the result: a
box whose posture could not be applied became permanently inaccessible, with
terminal, attach, and exec all repeating the same error. Exactly the stranding
the first ADR named, caused by the second.

ADR-0044 resolves it by asking who is asking. `policy set` means "make this
true" — the failure is the answer and must reach the exit status. Start/attach/
exec means "let me in" — and the box is no more exposed than it was a moment
earlier, running without the posture while nobody was blocked. Two ADRs can
each be locally right and jointly produce a broken system; nothing in a
per-change review catches that.

**Container egress bypassed policy entirely.** The ruleset filtered `hook
output`, but a nested Docker container's packets are *forwarded*. `docker run …
curl` walked past `isolated` and every allowlist — the command a developer is
most likely to run inside a sandboxed box was the one the sandbox did not cover
(ADR-0045).

And a third instance of cross-file drift: `conntrack` was in the checked-in
`system.nix` but only the *network* set in the generated catalog, so any Sets
apply regenerated the box without it and silently disarmed the conntrack flush.

**On that last one — I fixed the class, not the instance.** Two earlier
instances of this drift were each patched individually before anyone
generalized. The guard now covers **every** set: for all fifteen, the catalog
and the checked-in module must agree, and a catalogued set with no module in
the list fails too, so the check cannot quietly stop covering something. A
second guard does the same for the Ubuntu package mapping, which is a third
source of the same truth. Verified by deleting `git-crypt` from `git.nix` and
confirming the failure names the package and the file.

**Gate** — 395 Rust unit + 52 integration/e2e, 10 Go packages, 66 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-07T05:00Z — Rounds 10–13, and what the series is telling us

Findings per round, thirteen rounds in:

```
23  23  14  27   9  14  11  13  13   8   7  12  14
 1   2   3   4   5   6   7   8   9  10  11  12  13
```

Every round's findings were fixed before the next ran, and the gate was green
at every commit. The series is not converging. Over the last eight rounds it
oscillates around eleven with no downward trend.

The reason is visible in the findings themselves: **most of each round's
findings are in the code written to fix the previous round.** A sample from
round 13 — the forward chain that closed the container-egress bypass (round 9)
dropped every inbound connection to a published port; the lab-prefix file
(round 12) recorded the address pool instead of the allocated prefixes, and
nothing reapplied the ruleset after writing it; the `-once` flag (round 7)
needed a follow-up in round 8, and its follow-up needed one in round 10.

That is a fixed-point problem, not a backlog. Each fix is a new change of
comparable size to the ones being reviewed, written under the same conditions
that produced the original defects, and it gets reviewed for the first time in
the following round. A process with a per-change defect rate meaningfully above
zero does not terminate just because it keeps running.

Three things are genuinely improving, and they are worth separating from the
count:

- **Severity is drifting down in aggregate.** Rounds 1–4 turned up unenforced
  postures, an inaccessible box, and a firewall that did nothing. Rounds 10–13
  turn up an inbound SYN drop, a metrics series that vanishes, a wildcard that
  matches one label too many. Still real, less likely to be catastrophic.
- **Classes are getting closed rather than instances patched.** Cross-file
  drift now has three guards covering every set, the Ubuntu mapping, and the
  allow-set TTL. Each was added after the second or third instance of the
  same shape — later than it should have been, but they hold now.
- **The same asymmetry keeps recurring and is now nameable:** a fix lands on
  the path in front of me and not on its sibling. CLI before web, start before
  attach, rebuild-failure before write-failure. When a fix touches a lifecycle,
  the question to ask is which *other* entry points reach the same state.

**Recommendation, stated plainly:** further rounds will keep finding real
defects at roughly this rate, and each round costs a full review plus the work
to fix it. The value is still positive — these are genuine bugs — but "run
until the reviewer has no opinions" is not a condition this process reaches.
A better stopping rule is a severity floor: stop when a round produces no P1s
and no security findings, and take the remaining P2s as a backlog.

**Adopted (2026-08-07).** The stopping rule is now the severity floor: rounds
continue until one produces no P1 findings and nothing security-relevant.
Surviving P2s are recorded as a backlog rather than treated as a blocker.

**Gate** — 402 Rust unit + 52 integration/e2e, 10 Go packages, 67 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-07T07:30Z — Rounds 14–15 under the severity floor

The stopping rule changed after round 13: continue until a round produces no
P1 findings **and** nothing security-relevant, where security-relevant means
the policy/firewall path, authentication or the session cookie, path handling
that reaches a filesystem operation, shell construction from user or config
data, privilege decisions, or network exposure — regardless of the tag the
reviewer put on it.

That second clause is not belt-and-braces. This reviewer routinely files
security defects below P1: the shell-injection hole in Ubuntu provisioning was
a P2, so was the wildcard that matched its own apex, and so were all five of
round 15's blockers. A stop rule keyed on the P1 count alone would have shipped
every one of them.

```
round   findings   P1   security-relevant P2
  14        7       0            3
  15       10       0            5
```

Both rounds cleared the P1 bar and neither stopped. What is striking is *what*
the security-relevant findings were: in both rounds, most were defects in the
previous round's fixes.

- Round 14 fixed the nftables clear to stop swallowing failures. Round 15 found
  that the new probe treated *any* `nft list` error as "table absent".
- Round 14 moved `/metrics` off the node-facing ZTP listener. Round 15 found
  `GET /status` still there, serving the same inventory by another path — and
  that `:9090` binds every interface anyway, so the split had achieved nothing.
- Round 14 added a listener handshake before publishing build output. Round 15
  showed it could not work: the page-level SSE subscription already makes the
  receiver count nonzero, so it waited 50ms and called it synchronisation.

So the severity floor has lowered the *class* of defect without yet reaching
zero: the P1 stream has genuinely stopped, and the security-relevant stream has
not. The honest reading is that the fixed-point dynamic persists at every
severity band, and the floor simply picks which band is worth continuing to pay
for. That was the point of choosing it, and it is holding up — but it is not
converging faster than the raw count did, and nobody should expect round 16 to
be the last on the strength of two clean-P1 rounds.

One pattern is now unmistakable and worth stating as a rule rather than an
observation: **when a fix removes something from one exposed surface, the next
question is which sibling surface still has it.** `/metrics` and `/status`.
CLI and web. Start and attach. Rebuild-failure and write-failure. Every
instance of this in fifteen rounds has been the same shape, and it has been
caught by the reviewer rather than by me every single time.

**Gate** — 402 Rust unit + 52 integration/e2e, 10 Go packages, 67 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-07T09:00Z — Round 16, and a process failure worth naming

Round 16 returned two P1s, so the P1 stream is not drained after two clean
rounds. Both were mine.

The first is the sibling-surface rule failing on the round it was written down.
I removed `GET /status` from the ZTP provisioning listener, never registered it
on the operator listener, and documented it as available there — so the
endpoint was reachable on neither while my own docs said otherwise. The rule
says "when a fix removes something from one surface, ask which sibling still
has it." I asked half of it: I verified the removal, not that the thing still
existed somewhere. The operator listener now serves the operator handler
wholesale, so the two route sets are complementary by construction rather than
by two lists happening to agree, and there is a test for *presence* beside the
one for absence.

The second was a real bypass: container egress was policed by a snapshot of
Docker subnets taken when the policy was applied, so a network created
afterwards — `docker compose up` — met no rule at all. Matching by interface
(`docker0`, `br-*`) covers the networks that do not exist yet, which was the
entire population that mattered.

**The process failure.** Round 15's commit message claimed the nft probe
distinguished a missing table from a query failure. It did not: the edit script
aborted partway, wrote nothing, and I reported the fix as done without
checking. That is the second time — round 13's FRR namespace fix had the same
history — and both times the mechanism was identical: a multi-edit script whose
first replacement fails leaves *nothing* applied, while the surrounding work
proceeds and the summary describes the intent rather than the result.

So the whole recent backlog was audited: twenty-two distinct claims from rounds
11–16, each checked against a string that must be present in the code if the
fix really landed. All twenty-two are there. The two phantoms were the two the
reviewer had already caught, and there are no others.

The lesson is not "be more careful with scripts". It is that **a claim in a
commit message is not evidence, and the cost of checking is a grep.** Where a
fix is a discrete, greppable change, verifying it after the fact takes seconds
and catches exactly this class of error — which no test would, because the
missing code was never covered by one.

**Gate** — 402 Rust unit + 52 integration/e2e, 10 Go packages, 67 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-07T12:00Z — Rounds 16–20 under the severity floor

P1 counts since the floor was adopted: **0, 0, 2, 2, 7, 6, 3** (rounds 14–20).

The spike at seven tracked round 17, which was the heaviest change of the whole
exercise — a forward-chain inversion, a listener split, and a package-source
refactor in one commit. Rounds 19 and 20 were deliberately narrow and the count
came down. The floor is measuring change rate more than residual defect count,
which is worth knowing when reading it.

Three findings in this stretch were worse than ordinary bugs.

**I reopened a hole I had personally closed.** Round 12 added
`is_valid_attr_path` to stop shell injection through `[custom_packages]`. Round
17 taught that path about flake references and validated only the fragment
after `#`, so `github:user/repo; touch /tmp/pwn; #pkg` passed the guard whose
entire purpose was to stop it. Adding a feature to a validated path without
extending the validation is a distinct mistake from writing an unvalidated
path, and it is harder to notice, because the guard is right there.

**The DNS enforcer never worked.** `exec.Command` does not shell-split, so the
whole nft rule as one string was a single argv entry nft could not parse. Every
insertion failed for the life of every box, and nothing noticed because a
blocked domain looks exactly like a network problem. It survived nineteen
rounds because no test could reach it.

**A guard I added guaranteed the outcome it prevented.** The agent exits
without `/etc/devbox/policy.json`; `enforce::apply` refused to write that file
until a qualifying agent was running. Moving from `open` to an allowlist was
therefore impossible — every attempt bailed and the box stayed open.

The common thread is the one worth carrying forward: **the code that stayed
broken longest is the code no test could reach** — nft calls needing a kernel,
the bootstrap script running only on a blank device, eBPF needing BTF. Each has
now produced a serious defect that survived many rounds. The response in each
case was to pull a pure function out of the untestable path and test that:
`elementArgs` for nft, `clear_command`/`write_command` for the firewall shell,
`sh -n` over the generated bootstrap script.

Two guards now exist for classes rather than instances — `policy_lifecycle.rs`
for posture restoration, the set-drift tests for catalog/module agreement — and
the lifecycle one caught a rename in round 20, within the same session it was
written. That is the first time one of these has paid for itself immediately.

**Gate** — 403 Rust unit + 53 integration/e2e, 10 Go packages, 68 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean.

## 2026-08-08T04:30Z — Rounds 21–23, and where the P1s are coming from now

```
round   findings   P1
  21        9      not recorded
  22        8       5
  23        4       3
```

The count is falling. That is the least interesting thing about this stretch.

What changed is the *source* of the P1s. Through round 13 they were defects in
code written before the review started — the original work, found. In rounds 22
and 23 they are almost entirely defects introduced by the previous round's fix.
Round 22's commit says all five P1s were round 21's fixes, and that checks out
finding by finding: the DHCP and neighbour-discovery exemptions, the
`ff02::/16` rule, the DMI fallback, the state-before-restore ordering, and the
status-event ownership were every one of them round 21's work. Round 23's
leading P1 was round 22's eBPF probe.

So the loop is no longer measuring the quality of the original code. It is
measuring the quality of my edits, at a rate of roughly one P1 per two fixes.
That reframes the fixed-point argument from round 13: the process does not
terminate not because the codebase is deep, but because the fixing is itself a
defect source of comparable rate, and there is no round in which that stops
being true.

**The worst single finding in these three rounds is a test that passed while
the bug it was written for was live.** Round 21 put the DHCP and ND exemptions
in `emit_policy_rules`, which is shared with the chain that judges *forwarded*
traffic — so a nested container could send DHCP-shaped packets straight past an
isolated posture. The test asserted the rules were absent from `chain forward`.
They were. They always had been; `chain forward` never carried them. The leak
was into `forward_egress`, and the test never looked there.

That is worth stating as a rule, because it is a different failure from an
absent test: **a test that asserts on the wrong object is worse than no test,
because it converts an open question into a false answer.** An untested path is
a known unknown and stays on the list. A wrongly-tested path is retired. The
mechanism to watch for is specific and recognisable — when a fix lands in a
shared emitter, the test has to name *every* chain that consumes it, not the
one that was in my head when I wrote the fix. The rules now live in an
output-only emitter and the test checks both chains.

**Second: narrowing a predicate to kill false positives can kill the true ones
too.** Round 22 tried to stop the eBPF probe reporting destinations that were
never reached, by requiring `TCP_ESTABLISHED` when `tcp_v*_connect` returns.
But a successful connect returns in `TCP_SYN_SENT` — the handshake finishes
later. The check would have dropped nearly every ordinary outbound connection
from the timeline. Trying to stop the probe reporting unreached destinations, I
would have stopped it reporting anything.

The question I did not ask: *what fraction of the legitimate cases does the
narrower condition also exclude?* Here it was substantially all of them. That
question is cheap and I have never once asked it unprompted. Completion is now
observed at `tcp_finish_connect`, with identity captured at connect time and
looked up on completion through an LRU map, because the softirq context has no
useful pid or cgroup of its own.

Both of these are in paths no test reaches — a shared nftables emitter whose
output only matters in a kernel, and an eBPF program needing BTF to load. That
is the round 16–20 lesson repeating rather than a new one: the untestable paths
keep producing the serious defects, and the reviewer keeps being the only thing
that reads them.

**On the stopping rule, one honest caveat.** Round 23 was the smallest round of
the exercise and returned the fewest findings, and rounds 19–20 did the same
thing earlier. A round that changes less has fewer things to be wrong about. So
the severity floor is partly under my own control, and I could reach it by
making the next round trivial rather than by running out of defects. That would
satisfy the rule and mean nothing. The floor is a stopping condition for a
round of *real* work, and the moment it starts shaping how much work a round
contains, it has stopped measuring anything.

**A gap worth naming:** round 21's P1 count is simply not recoverable. The
review output was read, acted on, and discarded, and the commit message records
the finding count but not the split. The reviews are the primary evidence in
this whole exercise and they are the one artefact not being kept.

**Gate** — 406 Rust unit + 53 integration/e2e, 10 Go packages, 69 Python;
fmt/clippy/vet/gofmt/ruff/mypy all clean. Verified by running each, not by
reading the previous commit message.

## 2026-08-09T08:40Z — Round 39's last finding, and two the browser found

Rounds 24–39 are not written up here, and that gap is itself the first thing to
record. The commit messages carry the detail; this file stopped keeping pace at
round 23 and nobody noticed, which is the same failure as round 21's missing P1
split — the reviews are the primary evidence and they are the artefact least
looked after. Round 39's output is the first that was kept as a file rather than
read and discarded.

**The finding.** Round 39 returned seven; six landed. The seventh was a P1 on
the console's session cookie, and it is the most instructive defect of the whole
exercise, because the code was *correct against the threat it was written for*.

Cookies are scoped by host. A host has no port. So the browser attached the
console's cookie to every request it made to any other service on `127.0.0.1` —
including a project's own dev server, which could read the token straight out of
its inbound `Cookie` header and then drive the console: start, stop, destroy, a
terminal into any box. The header guards did not help and could not. They must
tolerate a missing `Origin` and a missing `Sec-Fetch-Site`, because a genuine
top-level navigation sends neither, and the replayer is not a browser — it omits
them for free and can forge them just as cheaply.

The comment in `auth.rs` defending that tolerance said: *a browser modern enough
to be steered into this attack is modern enough to send the header.* True, and
irrelevant. **There was no browser in the attack.** The guard was sound against
the adversary it imagined and silent about the one it had.

**Why round 39 did not fix it.** The write-up of the fix was itself wrong —
`sessionStorage` plus a header covers API calls and not navigation, and
navigation is why the cookie existed. Stopping there was right. What is worth
recording is that the *second* sketch was also incomplete, and only fell apart
when written out: keeping the cookie as a second credential requires a **third**
secret, because the cookie's value was the token and `?t=` mints credentials, so
a stolen cookie could simply re-bootstrap a fresh key. Three secrets to protect
a shell that carries no data and discloses less than `/metrics`. Once that was
on the page, the cookie stopped being half of the fix and became the thing to
delete.

**What landed.** Two per-launch secrets and no ambient credential at all
(ADR-0048). `token` buys one bootstrap page; `key` lives in `localStorage`,
which is scoped to a full origin *including the port*, and is presented
explicitly — as a header, or as `?k=` on the two channels that cannot set one.
A navigation can present nothing, so it gets a fixed data-free shell that
fetches the real page itself. CSRF stops being a class of problem here, because
forgery rides credentials the browser attaches by itself and there no longer is
one.

**Two defects found in a browser that no test could have caught.** Both were
silent, and that is the point.

The first: with the page delivered by `document.write`, `defer` does not mean
what it means during a navigation. `htmx.min.js` won the race against `sse.js`,
initialised `<body>` before the SSE extension was registered, and htmx marks a
node initialised — so the stream could never be connected afterwards either. The
page rendered, every button worked, the console logged nothing, and the
heartbeat simply never arrived. Blocking scripts, ordered by the parser, in both
paths; the terminal's scripts moved to the end of `<body>` where their element
exists, because `defer` was what used to let them wait.

The second: a key from a previous launch fell into the same branch as *no key at
all*, so the shell's own fetch was answered with a second shell, wrote it over
itself, and left a blank page — no notice, no error, the dead key still stored,
every reload repeating it. Offering nothing is a navigation. Offering the wrong
thing is the shell reporting back. Conflating them cost the entire recovery path.

Both are now tested, and the second is a server-side test that would have caught
it. The first still is not: nothing in the suite parses a document.

**The lesson this stretch adds.** The standing one is that untestable paths
produce the serious defects. This round sharpens it: *a guard can be sound and
still be aimed at the wrong adversary, and it will not look wrong while it is*.
The cookie comment was not sloppy — it was a correct argument about browsers,
sitting in front of an attacker that was not one. Re-reading it teaches nothing.
The only thing that surfaced it was asking who else receives this credential,
and the only thing that surfaced the two follow-on defects was opening a browser
and looking at the page.

**Gate** — 446 Rust unit + 48 console + 22 integration/e2e, 10 Go packages, 71
Python; fmt/clippy/vet/gofmt/ruff/mypy all clean. Verified by running each. The
console was additionally driven end to end in a real browser: bootstrap on a
deep link, token stripped and `tab=terminal` preserved, no cookie set at any
point, dashboard and detail rendered, SSE heartbeat live, xterm booted, and both
credential-failure notices shown with the dead key cleared.

## 2026-08-09T17:45Z — Codex review round 40: 7 findings, all addressed

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
```

The first round whose findings split cleanly into "the change under review" and
"everything else", and the split is the interesting part: **all three of mine
were caused by the previous round's fix, and two of those were regressions of
behaviour the codebase had already got right once.**

**A fix that re-opened a CSRF, three lines below the comment explaining it.**
The terminal tab starts its box with a POST rather than a GET precisely because
a side-effecting GET can be provoked by a hostile page; that reasoning is
written directly above the POST. Putting the shell branch ahead of the origin
checks handed the attack back in a new shape. A foreign navigation carries no
key, so it reached the shell before any check ran — and `shell.js` then fetched
the page itself, same-origin and keyed, which is indistinguishable from a real
request. **The shell laundered the foreign navigation into a local one.** The
box started for a page the user never chose to visit.

Worth naming as a mechanism rather than an instance: *introducing a new
early-return reorders every check that used to run after it*. The shell was
added as a fallback, which does not feel like touching the auth sequence. It is.

**The credential change broke every download link and no test noticed.** The
behaviour exports were plain anchors to `/api/`. A navigation cannot present the
key and `/api/` is not shell-eligible, so all three returned 401 from the moment
the cookie went. Nothing caught it because the console fixture has no event
store, so the block those links live in never rendered — the assertion had to
move to the template fixture that *does* have activity data. **A test that
cannot reach the markup is not weaker than one that can; it is silent.** That is
the round-21 lesson (a test asserting on the wrong object) with the object
missing entirely rather than merely wrong.

**And the storage choice was wrong for a reason I had argued away.** The key
lived in `localStorage`, which every tab on an origin shares and whose writes
are announced by the `storage` event. The console binds a predictable port, so a
page served from that port earlier by something since stopped is same-origin
with it and was handed the key on installation. Narrower preconditions than the
cookie, identical category: a credential readable by something that is not the
console. It is `sessionStorage` now.

The part to keep: **the trade-off I presented for that choice was itself wrong.**
`localStorage` was justified on surviving bookmarks and browser restarts — but
the key is per-launch, so it never survived either. The advantage being paid for
did not exist. Checking whether the stated benefit is real is a different
question from weighing it, and only the second was asked.

**The four pre-existing findings were all the same shape: a guard that proves
the wrong proposition.** The FRR marker proves a restart once succeeded, not
that the daemon runs now — so a crashed daemon skipped its own restart forever.
`save` proves a name is storable, but runs after `runtime.create`, so an
unstorable name built a box nothing could remove — the third check to need
moving above that call. Serde proves a field is empty, not that it was absent,
so "no custom packages" read as "written before packages existed" and the Sets
tab reported packages nobody had installed. And the agent's enforcement, moved
ahead of the collector dial so observability could not gate it, exited on a
failed dial into a two-second restart loop that destroyed and rebuilt the
nftables table every cycle — **re-blocking the entire allowlist for the length
of the outage, because of a fix intended to keep enforcement up.**

Two of those four are earlier fixes producing the defect they were meant to
prevent, which puts the rate from rounds 22–23 — roughly one new P1 per two
fixes — where it has stayed for eighteen rounds.

**Gate** — 521 Rust, 10 Go packages, 71 Python; fmt/clippy/vet/gofmt/ruff/mypy
clean. Driven in a real browser twice, once per credential change, which is the
only reason two of round 39's silent defects were found at all.

## 2026-08-09T19:30Z — Codex review round 41: 4 findings, all addressed

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
round 41  1   3      2              2
```

Half the findings of the round before, and the same story underneath: **the P1
was caused by round 40's fix, and one P2 was round 40's fix being only half of
one.** Eighteen rounds on, the loop is still measuring the quality of the edits
rather than the quality of the original code.

**A marker that made a survivable bug permanent.** Round 40 added `schema` to
`state.json` so an *absent* field could be told from an empty one. `devbox
upgrade` reads packages off `state.packages`, which a v3 box does not have, so
the rebuild had always dropped them — survivable, because the absence stayed
legible and the next render read them back out of `devbox.toml`. The marker
closed that door: upgrade would have recorded "this file is current and has no
packages" about a box whose packages it had just discarded.

The rule the marker actually asserts is *every field was written by code that
writes them all*. Adding it obliged every writer to be checked against that
claim, and only the writer that motivated it was. **A field whose meaning is
"trust this file" converts every incomplete path into a permanent one.**

**And a fix that solved the half it could see.** Round 40 moved the ruleset load
ahead of the collector dial, which stopped the restart loop destroying the
nftables table every two seconds. But those sets are created *empty* and only
captured DNS fills them, and DNS arrives through a loop that did not start until
the collector answered — so an allowlist posture still blocked every allowlisted
domain for the whole outage. The finding and the fix had the same words in them
and different scopes: *the table survives* is not *the allowlist works*.

Restructuring it broke the cross-language pipeline test twice, and both were
real. Events captured before the first connection were counted and discarded —
trading the ordinary case, where the collector is listening at startup, for the
rare one. And a refused handshake stopped being fatal, because it had been moved
into the same goroutine as the retrying dial: **an outage and a rejection are
different events, and putting them on one path turned a loud misconfiguration
into an infinite retry.** That is the same shape as round 40's shell branch
laundering a foreign navigation — a path added for one case quietly swallowing
another.

Worth noting what caught them: the only test in the suite that runs the Go agent
against the Rust collector. Neither defect is visible from either side alone.

**The two older findings were both a value standing in for a different value.**
ztpd recorded the hash of whatever its catalog held when a node reported
healthy — "what would I serve now" in place of "what did that node install",
which come apart precisely when a node retries its report across a restart. And
the console took an OS file lock on a Tokio worker and held it across an await,
so two overlapping policy saves could wedge a single-worker runtime permanently:
the holder can only finish on a worker that is now blocked waiting for it.

**Gate** — 524 Rust, 10 Go packages, 71 Python; six linters clean.

## 2026-08-09T21:40Z — Codex review round 42: 6 findings, every one of them mine

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
round 41  1   3      2              2
round 42  3   3      6              0
```

The first round in the series to return **nothing but defects introduced by the
previous two**. The loop has stopped measuring the codebase and is now measuring
the edits, exactly as rounds 22–23 predicted, at the same rate — roughly one new
P1 per two fixes, holding for nineteen rounds.

**The guard excused the file it lived in.** Round 41 ended with two audits that
cover a class rather than an instance, and the P1 here is a blocking
`lock_project_config` inside `apply_selection` — in `build.rs`, which the lock
audit skipped wholesale because it "defines the locks and the escape hatch". It
defines them and also calls one.

The mistake is worth stating exactly, because it will recur in some other form:
**the exemption was scoped to a file when what needed excusing was a few lines.**
A file-shaped hole does not stay the size of the thing it was cut for — the file
goes on accumulating code, and all of it inherits the excuse. In this case none
was needed at all: the definition is not `async`, and `lock_blocking` takes a
closure rather than naming the lock, so neither trips the check. Removing the
exemption entirely was the fix, and it was verified by reverting the very site
it had been hiding.

**The same collapse, twice in two rounds.** Round 41's fixes were widened to
`stop_box` and `destroy_sandbox` on the reasoning that a lock is a lock — but
`lock_rebuild` is a `try_lock`, which refuses instantly, and only
`lock_project_config` waits. Those two changes were made for a defect they never
had, and had to be undone. Round 42 then found the identical error one layer
down: a handshake was made fatal because a refusal is fatal, and a collector
that restarts *after accepting the connection* also fails the handshake — with
an EOF, which is an outage, arriving one step later than the dial. Treating it
as a refusal ended the agent, and the restart cleared every allow set.

Both times the reasoning was **"these arrive through the same function, so they
are the same event"**. Both times the fix was to make the difference a type
rather than an inference: `try_lock` versus `lock` in the one case, exported
`ErrRejected`/`ErrProtocol` sentinels in the other. That is the generalisable
part — when two outcomes need different handling, the distinction belongs in the
signature, not in the caller's memory.

**Three findings of one shape: a guard that protects a caller instead of an
operation.** `devbox stop` bypassed the console's claim entirely, because the
claim was in the console's wrapper rather than in `stop_sandbox`. `devbox use`
took its claim *after* `update_mounts` had already stopped and restarted Lima,
and never took it at all in `--writable`. And the node reported the hash of
`frr.conf` when the marker is what the daemon actually loaded. In each case the
protection was attached to one path through the thing rather than to the thing.

**On the audits, honestly.** They did find two real instances that the review
had not named, which is why they were written. They also produced this round's
P1 through a bad exemption, and one of them was briefly vacuous — defeated by a
comment that mentioned the helper by name, because the lookback window included
prose. Verifying a guard by reverting the fix is now the habit; twice that
verification itself lied, once because `cargo fmt` had reflowed the code so the
revert patch matched nothing, and once because the revert left the crate
uncompilable and the test never ran. **Both look identical to a pass when you
are reading for a failure that does not appear.**

**Gate** — 528 Rust, 10 Go packages, 71 Python; six linters clean.

## 2026-08-09T22:35Z — Codex review round 43: 4 findings, and three self-inflicted wounds while fixing them

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
round 41  1   3      2              2
round 42  3   3      6              0
round 43  2   2      3              1
```

**The pre-existing P1 is the most ordinary bug in the whole series, and it
survived forty-two rounds.** `devbox upgrade` clears the selection and rebuilds
it by feeding each recorded set name through `apply_tools` — which is an *alias*
table. It knows `claude`, `mosh`, `aider`; it has no case for `shell`, `tools`,
`editor`, `git` or `container`. Those five were cleared and never restored, so a
routine upgrade rebuilt the box without them. `--tools git` was accepted and did
nothing for the same reason.

Nothing exotic, nothing concurrent, no kernel involved. It lasted because
`active_sets` and the code that reads its output back are a *pair* with nothing
holding them together, and a pair with no test between them fails silently in
one direction only — the direction nobody looks. The repair is the round trip:
turn everything on, emit, read back, compare. A set added later is covered
without anyone remembering it exists.

**Three things went wrong while making these fixes, and all three read as
success.**

The write deadline was added by rewriting every `transport.WriteFrame(conn, …)`
call — including the one inside the wrapper being introduced, so `writeFrame`
called itself. It compiled. `go vet` was clean. The agent's own tests passed,
because none of them reach a live collector; only the cross-language pipeline
test writes a frame, and it had not been re-run.

A comment naming a file in backticks terminated the Go raw string the ZTP script
lives in. `server.go` says in its own doc comment that the script must contain
no backticks *including in comments*, and the test that parses it says it has
been broken that way three times. This is the fourth, written while adding a
comment about being careful. The `sh -n` guard cannot catch it: the crate no
longer compiles, so the test never runs.

And the round-40 test asserting the FRR skip branch checks liveness sliced the
condition at the first newline. Splitting that condition over two lines made it
read half and report the check as missing.

**The common shape is worth naming, because it is now the dominant failure mode
of this loop:** a broken build, an unreachable test, and a test reading the
wrong half of a line all produce *the absence of a failure*, which is exactly
what success looks like from the outside. Round 21 said a test asserting on the
wrong object is worse than no test. These are the same claim generalised — **any
verification that cannot run is indistinguishable from one that passed**, and
the only defence found so far is to make the thing fail on purpose and watch it.
That habit — revert the fix, confirm the guard screams — has now caught a
vacuous guard, a no-op patch, and an uncompilable revert, in one session.

**Gate** — 530 Rust, 10 Go packages, 71 Python; six linters clean.

## 2026-08-09T23:30Z — Codex review round 44: 3 findings, and the shape of a half-finished lock

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
round 41  1   3      2              2
round 42  3   3      6              0
round 43  2   2      3              1
round 44  1   2      1              2
```

Five rounds, twenty-four findings, and the count is finally falling while the
mix moves back toward the codebase. Round 42 was all mine; this one is mostly
not.

**Round 39's fix, seen from the other side.** That round put policy *writes*
under the project claim. The readers were left outside it, so a rebuild or a
start loads posture A, an editor saves and applies B under the lock, and stale A
lands on top afterwards. The file says B and nftables enforces A — and when A is
the more open of the two, a box whose recorded posture reads `isolated` is
serving traffic.

The generalisable form: **a lock that one participant ignores orders nothing.**
Half-serialising a pair looks exactly like serialisation from inside the half
that is covered, and the review that added the write-side lock had no reason to
look at the read side. Locks are a property of a *set* of accesses; nothing in
the code says what that set is, which is why the omission is invisible.

Worth recording that adding the claim needed a call-graph check first, because
`lock_project_config` waits rather than refusing. A self-nested take would hang
forever, and a permanent hang is a worse outcome than the race it would have
prevented. That asymmetry — refusing locks fail loudly, waiting locks fail
silently and terminally — is why the two are now separated in the audit.

**The most destructive finding is a value with two meanings.** `None` in the
generated-file snapshot means "this file was absent", and rollback acts on that
with `rm -f`. A `cat` that failed for any *other* reason recorded the same
value. So a guest transport hiccup during the snapshot turned the rollback that
exists to restore those files into the thing that deleted them.

This is the third time in five rounds that a single value stood for two facts:
empty-versus-absent in `state.packages`, refusal-versus-outage in the handshake,
and now absent-versus-unreadable here. Each time the fix was the same — make the
distinction representable — and each time the bug had survived because *the
common case makes them agree*. A file that is absent and a file that cannot be
read look identical until the day the difference is destructive.

**One finding was caused by round 42's fix**, which is the expected rate. Making
`devbox use` hold its claim across `update_mounts` opened a window where the
Start button could relaunch the guest from the pre-switch YAML. The repair worth
copying is not the lock, it is the naming: the *claiming* form took the plain
name, and the single caller that already holds the claim now asks for
`start_box_holding_claim` explicitly. A new call site gets the safe one without
having to know the hazard exists.

**Gate** — 532 Rust, 10 Go packages, 71 Python; six linters clean.

## 2026-08-10T00:40Z — Round 45: 7 findings, and the point where point-fixing stopped

```
        P1  P2   mine   pre-existing
round 40  3   4      3              4
round 41  1   3      2              2
round 42  3   3      6              0
round 43  2   2      3              1
round 44  1   2      1              2
round 45  6   1      4              3
```

The count went back up, and where it went up matters more than that it did: six
of the seven are in the locking and policy-restore paths, which have now
produced a finding in five consecutive rounds. That is not a run of bad luck. It
is what a subsystem does when its invariants live in prose.

**The finding that settled it.** Round 44 reported that `None` in the
generated-file snapshot meant both "absent" and "unreadable", with rollback
deleting on the former. I fixed the per-file half and left the set-module
directory ten lines below doing the same thing — having quoted that type's own
comment, *"No archive has two meanings and they need opposite handling"*, as
evidence the type already separated them. **Reading the comment that describes
the class is not the same as checking the class.**

Three paths could also leave a box unfirewalled: `nixos-rebuild switch` removes
the nftables table, a *failed* switch rolls back and switches again, and both
`upgrade` and `reprovision` returned on `?` before their restore. And
`reprovision` replayed a posture captured before a rebuild that takes minutes,
overwriting anything edited in the gap.

## The design pass

Rather than a sixth round of point fixes, the subsystem was counted.
**Eighteen policy applications; nine ran with no claim on the box they were
changing.** Nothing in the code said which of them meant to, which is why five
rounds of fixing one call site each had not converged and would not have.

Three things were wrong underneath, and all three were expressible:

**The two locks were one type.** `lock_rebuild` refuses immediately and
`lock_project_config` waits, and both returned `RebuildLock`. Nothing in a
signature distinguished them, so nothing could — which is how two paths came to
be "fixed" for a deadlock only the waiting one could have had, and how a
function that returned "the lock" looked complete while dropping the other. They
are `BoxClaim` and `ProjectClaim` now.

**The claim was a habit.** `restore_after_rebuild` and `apply_saved` take
`&BoxClaim`; a caller without one does not compile. `start_box_holding_claim`
stated that requirement in its *name*, which is a comment with a colon in it.

**Contention had no policy.** Paths that merely *use* a box — `exec`, `attach`,
`code`, the lab — now step aside when a rebuild owns it, because the holder is
already obliged to restore the posture before returning. Nothing that worked
starts failing.

**The pass found a bug in its first ten minutes, and it was the previous
round's.** Round 45 added the box claim to the CLI policy editor as a local
inside `load()` — dropped the instant that function returned, so the editor held
nothing while it saved and applied. Written, committed, and inert. The type
split is what surfaced it: the signature now has to say which claim survives.

**What to take from this.** The loop had been asking "is this call site
correct?" for five rounds and getting a true answer each time. The question that
mattered was "what does this subsystem guarantee, and what enforces it?" — and
nothing in a per-diff review asks that. A reviewer sees the change; the missing
invariant is in the code it did not touch. **Counting the population rather than
inspecting the instance is what turned five rounds of symptoms into one cause.**

Verified adversarially, as everything here now is: applying policy without a
claim does not compile, a `ProjectClaim` cannot be passed where a `BoxClaim` is
wanted, and the re-run census reads thirteen applications, zero unaccounted for.

**Gate** — 533 Rust, 10 Go packages, 71 Python; six linters clean.

## 2026-08-11T16:31Z — Final v4 implementation and verification

All planned v4 phases and acceptance paths are implemented. The final pass
closed the remaining production gaps: the long-lived host collector survives
ordinary CLI use and replaces stale-release owners atomically; Web can create
real boxes and strictly restores their saved egress posture before reporting
success; Labs drive real routed namespaces, FRR convergence, DHCP/ZTP, faults,
healing, and teardown; flow capture returns real AF_PACKET pcap data through
both CLI and Web; observability, examples, doctor output, Grafana material, and
the documented console flow are present.

**Gate** — 576 Rust tests green, including real Docker, routed Lab, ZTP/Web/pcap,
collector process, policy lifecycle, and HTTP console integration; Rust fmt,
strict clippy, check, and release build green. Go fmt/test/vet green (plus
golangci-lint when installed). Python ruff lint/format, strict mypy, and 71
pytest tests green.

**Adversarial review** — four exhaustive Claude Opus 5 xhigh passes reviewed
the full tracked and untracked tree. The latest pass found only that the
machine-readable `ALL PHASES DONE` sentinel had been reworded; the exact first
line is restored and pinned by an integration test. Earlier necessary findings
were repaired and re-gated rather than waived.

**Next** — no planned implementation work remains. Reopen the loop only for a
new necessary correctness, security, lifecycle, or acceptance finding;
otherwise this tree is ready for human PR handoff. No external PR was opened
without explicit authorization.

## 2026-08-22T07:20Z — Activity: diagnosis, a real live stream, three layers

Reopened for one necessary finding: **the console could not tell a reader why
the Activity tab was empty.** A box registered before v4 has no
`devbox-obsd`, so its exec agent closed before the handshake; the collector
logged the same warning every 30 seconds for six days and the tab said "No
observability data yet" — the identical sentence it showed for a stopped box, a
stopped collector daemon, and a box no collector had reached.

**Landed**

- **`obs::health`** — per-box capture health published to
  `boxes/<name>/capture.json`, atomically replaced like `collector.json`
  (ADR-0050). `Collector::with_agent_hook` reports the accepted hello, so the
  host records which backends actually attached; the supervisor keeps the
  agent's own last line of stderr and puts it in the failure record, and
  carries the previous failure's text into a retry so a flapping agent does not
  blank its own diagnosis.
- **`activity::capture_view`** — a pure function resolving daemon lock, box
  status and health record into one status bar with a level, a headline, and a
  remedy (`devbox reprovision` for a missing agent, `devbox doctor` for a
  missing sudo or a dead daemon). All four situations unit-tested without a
  runtime, a daemon or a box.
- **A real live stream** (ADR-0049). `Query::after_id`, `Store::query_with_ids`
  and `Store::max_id` make an id-anchored tail; `web::tail` watches the store
  and publishes a *signal*; the page fetches only what it is missing and
  prepends it, returning its own anchor out-of-band. Replaces
  `every 2s` + `innerHTML`, which re-rendered and replaced the whole stream
  twice a second and lost scroll and selection each time. A quiet box now
  generates no traffic at all. `pause` holds the stream.
- **Three layers.** Situation (density strip coloured by dominant domain,
  window ends, counters including refused connections), seven views over one
  load (stream, peers, flows, DNS, processes, files, policy), and a filtered
  stream. Filters run *after* DNS correlation — the trap `cli::watch` already
  documents — so `pypi` matches a connection that only recorded an address.
  The strip, the switcher's counts and the six analysis views are one region
  re-read together, because a live strip above a table frozen at page load
  reports two different windows a few centimetres apart.
- **Policy promoted.** Refused connections were reachable only inside a
  collapsed `<details>` holding a `<pre>`. They are now a view, a counter, and
  a marked row in the peers rollup — which includes peers that were refused
  *before* connecting and therefore produced no flow at all.
- Byte counts are rendered as sizes; the window heading and both ends of the
  timeline axis now come from one source, so they cannot disagree.

**Reviewed adversarially, twice.** Two Codex `gpt-5.6-sol` xhigh passes over
the scoped diff raised 13 and then 14 findings; every one was accepted and
fixed, and the second pass judged eight of the first round's fixes incomplete
rather than wrong — which is the finding that mattered. The ones worth
recording:

- A tail anchored on the newest row that *decoded* wedged permanently the
  moment an older schema left an undecodable row in front of it. The scan
  position is a property of the scan, not of what survived it.
- A row id alone cannot name a position. A box destroyed and recreated under
  one name gets a new store starting again at id one; the cursor now carries
  the store's inode and a mismatch replaces the stream instead of appending to
  it.
- `after=` arrives in a query string, so `-9223372036854775808` reached
  `newest - anchor`: a panic in a checked build, a wrapped comparison in a
  release one.
- The status bar asked for a `SELECT COUNT(*)` — a full scan near the retention
  limit — on every page load and twice a second on the live path.
- `unreachable` is not `stopped`. A loaded Lima VM reads `unreachable` while
  its agent streams, and the bar was reporting "nothing to capture" over a live
  stream.
- Overlapping agent connections let a departing one's notice overwrite an
  arriving one's, reporting a healthy box as disconnected.

**Gate** — 568 lib + 65 console + 20 integration + e2e (Docker, routed lab,
ZTP, pcap) green; `cargo fmt`, `clippy -D warnings`, `go vet` clean.

**Verified in the running console**, not only in tests: three events inserted
into a live store appeared at the top of an open Activity tab within half a
second, prepended, with every existing row untouched.

**ZTP is in the console** (ADR-0051). `/labs/ztp-fabric` now shows the state
machine as it runs: a convergence verdict with healthy/expected, failures,
never-seen serials and provisioning p95; a node table carrying serial, state,
attempts, config hash and failure reason, sorted so what needs attention reads
first; and a topology whose blank nodes change colour as they provision — which
is what makes "zero touch" something you watch rather than something you are
told about afterwards.

The operator listener did not move. It binds loopback inside the service
namespace because a ZTP server is multi-homed and its inventory routes are
unauthenticated; the console asks the substrate to fetch the status over the
same exec channel every other lab operation uses.

## 2026-08-23T04:10Z — Eleven adversarial review rounds, and ZTP in the console

**Review** — eleven Codex `gpt-5.6-sol` xhigh passes over the scoped work
raised 13, 14, 12, 12, 3, 2, 7, 8, 9, 14 and 14 findings. All 108 were accepted
and fixed. The rounds that mattered most:

- **A live stream that could wedge forever.** A tail anchored on the newest row
  that *decoded* stopped the moment an older schema left an undecodable row in
  front of it. The scan position is a property of the scan, not of what
  survived it.
- **Path traversal on three routes.** The activity fragments take a box name
  straight from a URL and never look a sandbox up; `SandboxState::load` joined
  whatever it was handed, while `save` and `remove` had always refused. Guarded
  at the read path, so every caller is covered.
- **A byte cap that counted characters.** SQLite's `LENGTH` on a TEXT value
  counts characters, so a limit meant as 64 KiB admitted four times that for
  any event carrying multibyte content.
- **A retention sweep that emptied the store.** `page_count` measures the pages
  the *file* holds, and freeing pages does not shrink it — so a size limit
  compared against it could never be satisfied by deleting, and the trim loop
  ran until the table was empty. Measured as pages in use, sized to the
  overage, and guarded against any measure that does not respond to deletion.
- **An audit summary that invented a posture.** A window holding no policy
  event rendered as `open` egress — the most reassuring of the four, chosen as
  the default for "not observed", in the tool whose selling point is an audit
  trail.
- **`pause` that did not pause.** htmx parses a trigger's `[...]` filter
  immediately after the event name; written after `throttle:` it was never read
  as a filter at all.

Five findings in the last rounds were pre-existing hardening the new code was
the first to exercise — retention sizing, pcap buffering, CLI name validation,
agent version pinning, atomic-write temporary names. They were fixed rather
than deferred, and are recorded here as such.

**Gate** — 590 lib + 66 console + 20 integration + e2e (Docker, routed lab,
ZTP, pcap) green; `cargo fmt`, `clippy -D warnings`, `go vet` clean; no stray
collector daemons.

**Also fixed: a test that leaked a daemon per run.** `CollectorCleanup` read
the identity file at drop time, after the temporary home it lived in had
already been deleted — so it never had a pid to signal. The strays accumulated,
slowed the machine, and made the next run's timings look like a regression,
which cost an hour of chasing one. Cleanup must not depend on the thing it
cleans up outliving it.


## The set that was correct until you touched it

Bringing the ZTP flagship up on a freshly built box failed at the preflight:
`devbox doctor` reported `lab: missing: zebra bgpd`, on a box whose `network`
set was enabled and whose `nixos-rebuild` had reported success.

Two bugs, stacked.

**FRR hides its daemons in `libexec`.** The package puts `zebra` and `bgpd` in
`libexec/frr/`, and a NixOS system profile links only `bin`, `sbin`, `lib`,
`etc` and `share`. Installing `frr` gave the substrate `vtysh` and nothing for
it to talk to. `lab up` invokes the daemons by name and its preflight probes
`command -v zebra`, so the entire reason the `network` set carries FRR was
unreachable on the default image. The set now builds a two-line derivation that
symlinks them onto PATH.

**And that fix could not reach a box.** Adding it to `nix/sets/network.nix` and
re-applying changed nothing, because Sets apply does not push that file — it
*regenerates* it from `NIX_SETS`, the package-name index, as a flat list. The
derivation had nowhere to go in a list of names.

The deeper shape is worth stating plainly: three code paths wrote the same
guest files and two of them reconstructed those files from a lossier
representation. For thirteen of fifteen sets the reconstruction was exact, so
nothing looked wrong. The two exceptions were the two sets that need to be more
than a list — the AI sets' `tryEval` guards, and `network`'s derivation. A box
was therefore correct when created and quietly degraded by its first Sets
apply, with a success message on both. `devbox upgrade` was worse: its path had
no exemption at all, so it would strip the AI sets' guards too.

ADR-0052 collapses it: the checked-in module is the artifact, all three paths
push it, and `NIX_SETS` goes back to being only an index. Both drift directions
are now pinned by a test, and the module→catalog direction skips parenthesised
sub-expressions so a set may carry a derivation without its tokens being
mistaken for packages.

**Worth remembering.** The failure was invisible in exactly the cases that
would have caught it early — a fresh box works, a rebuild reports success — and
visible only as a third command's preflight complaining about something two
steps removed. When two representations of one thing agree in every case you
have tested, that is not evidence they agree; it is evidence you have only
tested where they overlap.

## Three faults between a healthy node and a working fabric

The ZTP flagship did not work. Not "worked with a rough edge" — `lab up`
blocked forever, and once unblocked it failed twice more, each time at a later
stage and each time for an unrelated reason. All three were invisible to the
test suite and to every per-node check.

**`lab up` hung on the first daemon.** `zebra -d` detaches but keeps the
descriptors it inherited, so FRR can still report an early failure. Over ssh
that means the session never closes, and `lab up` waited on a daemon that was
up and healthy the entire time. It read as a slow lab, not a hung one.
(ADR-0053.)

**Every blank node failed its DNS self-check.** Two independent faults, either
sufficient. The lab's dnsmasq used `address=/name/v4`, which answers A and
forwards everything else — with no upstream to forward to, so AAAA came back
REFUSED and every stock resolver, which asks for both at once, called a
resolvable name missing. And the check itself used `getent hosts`, which on a
NixOS substrate is answered by `nsncd` in the root network namespace, from the
host's `resolv.conf` — a resolver on the wrong side of the boundary the lab
exists to draw. `tcpdump` inside the node saw no DNS packet at all. (ADR-0054.)

**Then three healthy nodes could not reach each other.** FRR 10 moved interface
configuration into `mgmtd`, and `lab up` started only zebra and bgpd. Zebra
logged "No such command" for every line of every interface stanza and carried
on, so each node came up with its BGP configuration and none of its addresses.
Routed labs never noticed because wiring assigns addresses out of band; a ZTP
node has no such step, by design. (ADR-0055.)

**And the fix for the third broke a fourth thing.** Requiring `mgmtd` was
right for the box devbox builds and wrong as a rule: it arrived in FRR 9, and
Debian bookworm — which the routed e2e test runs on — ships 8.4, where zebra
still owns interface configuration. The lab that had been working failed on its
first daemon. The start is now guarded by `command -v` on the substrate, and
`doctor` asks zebra its version before deciding whether a missing `mgmtd` is a
fault.

That failure was itself nearly missed: the run that surfaced it also had a
flaky `e2e_docker` failure, and with test binaries running in parallel the
first panic is the one that gets reported. The docker test passed alone and on
re-run; the lab test failed both times. Two failures in one run are not one
failure — the second has to be looked at after the first is explained.

**Gate** — 593 lib + 68 console + 20 integration + e2e (Docker, routed lab,
ZTP, pcap) green; `cargo fmt`, `clippy -D warnings`, `go build`, `go vet`,
`go test` clean.

**Worth remembering.** Every one of these announced success at the moment it
failed. `nixos-rebuild` reported success on a rebuild that had dropped a
package; zebra reported startup on a config it had discarded most of; `ztpd`
reported three healthy nodes on a fabric where none could reach another. The
common shape is a component that treats "I did the part I understood" as done.
What caught all three was the same thing: running the feature end to end on a
real box and looking at the state it actually produced, rather than at what
each step said about itself.

## Round 47 — reviewing the fixes themselves

The six ZTP-run fixes went through the same review loop as everything else
(gpt-5.6-sol, xhigh). Seven findings, none must-fix, all addressed:

- The lossy module generator survived as a *fallback* in `write_set_modules` —
  one missing table entry away from reopening ADR-0052's hole. Both writers now
  iterate `NIX_SET_FILES` with no fallback, the generator is deleted, and a
  bijection test pins the table against the catalog.
- The drift test blanked every parenthesised expression, so `(pkgs.htop)`
  could be installed with the catalog never hearing of it; and the AI sets were
  checked in one direction only. Removed expressions now come back to the
  caller and must be the one `runCommand` helper shape; the AI modules'
  quoted tokens and `pkgs.` paths are compared against the catalog both ways.
- The index test accepted a commented-out import. It parses bindings now,
  both directions.
- Teardown killed only `*.supervisor.pid`; on the partial-teardown rerun the
  namespace can already be gone, and `ip netns pids` then enumerates nothing —
  daemons outlived the directory recording their pids. Every `*.pid` dies.
- `restart_frr` sent SIGTERM and immediately started replacements. Bounded
  wait, escalate to KILL, remove stale pidfiles and the zserv socket first.
- `busybox` is a configurable multicall binary; the applet is now confirmed
  with `--list` before use, `getent` is verified to exist, and a substrate with
  no resolver tool at all passes the check rather than failing a healthy node
  over a diagnostic it cannot run.

Also this morning: a wedged macOS XprotectService stalled *every* first-exec of
a fresh binary at `_dyld_start` — `cargo run`, test binaries, a 33KB
hello-world, and the build script alike. Diagnosis that it was environmental:
`codesign --verify` in 0.04s, sample showing pre-main, a control binary
hanging identically. Worth remembering the shape: a toolchain that suddenly
"hangs everywhere" may not be the toolchain.

The same queue then failed the stdio obs-pipeline tests three runs straight —
and the third run finally told the truth: the agent died of `broken pipe`
against a collector whose ten-second Hello window had expired while the
agent's exec sat in the scan queue. The collector's behaviour is correct — a
transport that never says hello should be abandoned — and the agent execs
inside the guest in production, where no host scan queue exists. The fix
belongs to the test: `build_agent` now runs the fresh binary once with
`-version` before anything waits on it, paying the per-file scan where nothing
is timing.

## Round 48 — reviewing the fixes to the fixes

Eight findings on round 47's own changes, none must-fix, all addressed. The
theme this round was tests that promised more than they checked: the AI-set
comparison still passed on a package named only in a comment; the recognised
`runCommand` shape could smuggle `${pkgs.htop}` inside its script string; the
index parser accepted `system.nix.disabled`, an import without
`{ inherit pkgs; }`, and an alias binding; the bijection test proved "at least
one" where "exactly one, and byte-equal to the file on disk" was the claim;
and the stdio warm-up turned a broken agent into a green skip. Each now checks
what its name says. An older, weaker duplicate of the module-drift test was
deleted outright rather than updated — two tests for one invariant is how the
weaker one ends up being the one somebody reads.

On the runtime side: lab teardown validates a pidfile's number and the
process's identity before root sends a signal, and ztpd's `restart_frr`
snapshots the pids it TERMs, polls those pids rather than re-reading files a
dying daemon may unlink, escalates to KILL once, and fails the restart if
anything survives — starting a replacement against a live predecessor's
sockets is the race the wait exists to close.

## Round 50 — NO FINDINGS

The reviewer's closing pass on round 49's fixes returned clean. Four commits
carry the arc: the feature work and its six field-found faults (`aed6cae`),
then three review rounds on the fixes themselves (`52f27ec`, `6bef4ff`,
`690a6b0`) — 18 findings addressed across rounds 47–49, exactly one of them
must-fix, and that one was in a fix. The pattern held to the end: reviewing
the repair is where the sharpest findings live.

---

# devbox v5 — build log

**Source of truth:** `docs/plans/2026-09-05-devbox-v5-design.md`.
**Integration branch:** `v5-main` (the `v5/*` namespace is taken by task
branches, so the plain name is not available). Task branches are `v5/<task>`,
each built in its own worktree by an executor working from a written brief;
the supervisor merges in a fixed order and runs the full gate after every
merge.

## 2026-09-05T07:30Z — Wave 0: merge main, remove the lab, embed the eBPF agent

**Landed** (three task branches merged into `v5-main`, in this order)

- `v5/merge-main` — `origin/main` (70 commits, v0.1.6) merged into v4 with
  the eight conflicts resolved semantically (ADR-0058). Verified by
  provisioning a fresh NixOS box on Lima from the merged binary.
- `v5/ebpf-local` — per-architecture CO-RE objects committed, `build.rs`
  embeds the eBPF agent when the object exists, the handshake carries the
  capture source, `doctor` and the console print it (ADR-0057). Verified on
  `devtest`: connections carry real pids, file events appear.
- `v5/lab-removal` — the lab, ZTP server, labkit, console views, docs and CI
  lanes removed; the prefix contract de-labbed (ADR-0056).

**Gate on `v5-main`** — `cargo fmt`, `clippy -D warnings`, 463 lib + 98
integration tests, `go vet`/`go test` across 7 packages, `gofmt` clean,
release build 0.1.6 with no `lab` command.

**Incidents.** The host root volume filled twice: once from four concurrent
worktree builds on top of a 40 GiB `target/` in the main checkout, once from
a release build. The second time the Lima virtual disk failed a write and
`devtest`'s ext4 journal aborted, leaving the guest root read-only until a
VM restart. Two rules followed: executors check free space before any build
and stop below 20 GiB; release builds only when a brief asks for them.

**Worth remembering.** A full host disk does not stop at the host. The guest
sees a write error on its block device and takes the only safe action ext4
knows, which is to stop writing — so the symptom shows up as a broken box,
one layer away from the cause.

**Next.** W0-4 (positional box name across the CLI), W0-5a (SNI across TCP
segments), W0-5b (byte counters via a close probe), W0-5c (agent update by
content hash, unit arguments generated by the host) run in parallel; then
wave 1 (run evidence, checkpoints, broker, MCP, export) from `v5-main`.

## 2026-09-05T08:00Z — W0-4 landed; a stale collector explained a degraded box

**Landed** — `v5/cli-names`: 34 subcommands take the box as a positional
`[NAME]` through one shared `BoxArg`; `--name` stays as a hidden alias and
the two conflict. `policy allow` keeps a visible `--name` because its entries
are a required variadic and there is no position left. Gate: 572 tests,
clippy clean, release binary answers both spellings.

**Found while recreating `devtest`.** The rebuilt box reported `capture:
proc (degraded)` from a binary that embeds the eBPF agent. The stdio agent
had been started with `-no-ebpf` by the per-user collector daemon — a
process left over from W0-1's smoke test, built before the eBPF merge, still
running from a worktree directory that no longer existed. The new binary has
the same version string, and `daemon.rs` does not take over from a collector
of the same version, so the stale one kept deciding the agent's arguments for
every box. Killing it and rerunning one lifecycle command gave
`ebpf+packet+netfilter` immediately. The takeover rule moves to binary
identity (commit + hash) under W0-5c, alongside the guest-agent update rule
it mirrors.

**Worth remembering.** "Same version" is a claim about intent, not about
bytes. Every place that compares versions to decide whether to replace a
running component — the guest agent, the host collector — was wrong in the
same way, and each was invisible until two builds with one version string
shared a machine.

## 2026-09-05T15:30Z — W0-5 lands; the first three wave-1 tracks land; file events need a scope

**Landed on `v5-main`**, in merge order: `v5/tls-sni` (SNI read from a
ClientHello split across TCP segments, with bounded reassembly along the
sequence number), `v5/bytes` (a `tcp_close` probe settles byte counts into a
new `close` event; connect no longer pretends to know), `v5/checkpoint`
(overlay checkpoints: create/list/diff/restore/prune; the walk now recognises
whiteouts by rdev 0/0 and opaque directories, neither of which v4's diff
did), `v5/agent-update` (a box's agent is replaced when its content hash
differs from the embedded one, unit arguments are generated by the host, and
the collector daemon is taken over by binary identity rather than version),
`v5/export` (OCSF 1.3 and OTLP/JSON, every class validated against the
schema server, a real hour of events accepted by an OpenTelemetry Collector).

**Verified on `devtest` with the integrated binary.** One `exec` from the new
build replaced the stale agent by hash; `doctor` reports `agent binary:
matches host embed` and `capture: ebpf+packet+netfilter`; a `curl` shows
`tls example.com`, `close example.com:443 ↑1.9KB ↓6.3KB 50ms` with a pid, and
`file create /workspace/wave0-final.txt`. The three signals the product's
claim rests on — process attribution, TLS names, bytes — now hold on a source
build.

**Found.** With eBPF on, the `openat` probe reports every open by every
process: 6,750 events in 80 seconds, 1,030 "files written" in one summary,
a 258 MB store after eight hours, and `behavior summary` hitting its scan cap
so that its default window covered the oldest seven minutes. Two work items:
file events get a scope (`/workspace` and the user's home, filtered in the
agent, the scope carried in the handshake and shown by `doctor`) — dispatched
as W2-0; and the summary should scan newest-first, to be done with track A.

**Gate on `v5-main`** — 653 tests, clippy clean, Go clean, release build.

## 2026-09-05T17:40Z — Runs are wired end to end; file events have a scope

**Landed on `v5-main`.** `v5/broker` (credentials stay on the host: OS
keychain, per-service reverse proxy, per-box rotating tokens, scopes,
`credential` events; the v4 code that copied `.credentials.json`,
`auth.json`, and a plaintext `~/.devbox-ai-env` into the guest is gone),
`v5/file-scope` (the `openat` probe is filtered in the agent to
`/workspace` and the user's home; the scope rides in the handshake and
`doctor` prints it; 60 s of file events fell from 1,446 to 90), and `v5/run`
with its integration wave: a run is bracketed by two checkpoints, its file
changes come from the checkpoint diff rather than the box, bytes come from
`close`, the broker environment rides inside the wrapper's argv,
`behavior summary` scans newest-first, `export --run` resolves the run to a
row-id range first (7 s → 12 ms), and a v4 store that could not be opened by
the binary meant to migrate it now can.

**Verified on `devtest`**, integrated binary, one command:

```
$ devbox run devtest --label int-check -- sh -c 'curl -s -o /dev/null https://example.com; echo hi > /workspace/int-check.txt; sleep 1'
run 01M1S4M245XA0DBDZKR6CV8YGF · 1.5s · exit 0 · finished
  files    1 changed (1 added, 0 modified, 0 deleted) · scope: run
  network  1 peers · 1 DNS · ↑1.9KB ↓6.3KB
  process  23 in the tree
  coverage full (ebpf+packet+netfilter) · 42 events · 0 dropped
  report   ~/.devbox/runs/devtest/01M1S4M245XA0DBDZKR6CV8YGF/report.html
```

That line is the v5 thesis in one screen: what the run wrote, who it talked
to and how much, what it spawned, and how much of that the collector actually
saw.

**Gate** — 767 tests, clippy clean, Go clean, release build.

**Process notes.** Merging two branches that each added a field to `Event`
compiled the library and not the tests, and the gate's pipeline swallowed
the compile error; the gate runs under `pipefail` now. Two executors
replacing the same box's agent in parallel stacked bind mounts on each
other; agent-replacing tasks get one box each from here on.

**Next.** W2-2 (`mcp run` as a run, `mcp report`, `mcp self`), W2-3 (the
sweep of small gaps each track left), then docs, README, screenshot,
ADRs 0059+, version 0.2.0 and a release.

## 2026-09-05T19:30Z — 0.2.0 release candidate: the code surface is frozen

**Landed since the last entry.** `v5/sweep` (nine small gaps: discard now
remounts, `layer checkpoint-rm`, the box-name guardrail walks the real command
tree, the guest home comes from the login shell rather than `/etc/passwd`,
the console terminal gets the broker variables, the NixOS unit gets a file
scope), `v5/mcp` with its integration wave (`mcp run` is a run, `mcp report`,
`devbox mcp self`, `uvx mcp-server-fetch` end to end from a dedicated box),
`v5/report-polish` (wrappers fold to one row, writes outside the overlay get
their own table, `destroy` removes the reports with the store, the
Credentials section is fed by the broker's events — which until then never
reached the run they were made for), `v5/docs` (README rewritten around the
run report, a v5 quickstart, ADR-0059..0067, version 0.2.0, CI on `main`,
`v4`, and `v5-main`), and two blockers the documentation pass found:
`v5/redact` (a `*_TOKEN=`, `*_SECRET=`, `*_KEY=` or `Authorization:` value in
an `exec` argv is `***` before the agent sends it, again when the collector
stores it, and again when an old row is read; the runtime's login shell
folds into the wrapper row) and `v5/export-credential` (a brokered call is
OCSF API Activity 6003, validated allowed and denied against the schema
server; `--run` now fills `metadata.correlation_uid`).

**The screenshot in the README is one page of a real run**: files (scope:
run), network with the broker hop, processes with the token shown as `***`,
credentials, policy. The three strings that must not appear — the user
name, the host path, the token — were checked against the DOM and appear
zero times.

**Gate on `v5-main`** — the final one before tagging: every test binary
green, clippy clean, Go clean, release build `devbox 0.2.0`.

**What remains is not code.** The release workflow has never run (this host
has no `act`); `v0.2.0` will be its first run. `install.sh`, the README, and
`Cargo.toml` name github.com while the origin is a private Gitea, so the
release needs `v5-main` on GitHub and a tag pushed there — both outward
actions for Ethan. Follow-ups that did not block 0.2.0 are listed in the
wave-2 checklist and will become the first issues of the next cycle:
`devbox code` broker injection, `detect_vm_username` asking the guest,
`RunRecord.file_scope`, a migration for pre-redaction rows, the broker's
per-process monotonic clock, the run-start attribution race that can start
a process tree mid-wrapper, and the amd64 CO-RE object once CI produces it.

## 2026-09-05T23:40Z — 0.2.0 is out; the first green CI on GitHub

**Released.** `v0.2.0` at `8d67295`:
https://github.com/ethannortharc/devbox/releases/tag/v0.2.0 — `devbox-darwin-arm64`
and `devbox-linux-amd64`, the names `install.sh` expects; the macOS binary
downloaded and run on this host answers `devbox 0.2.0`. `v5-main` is on
GitHub as well as the Gitea origin.

**What the first runs of the workflows taught.** The release workflow died
three times in its first step: on the 24.04 runners `bpftool` is a virtual
package, and the runners' Azure kernels have no `linux-tools` package to
provide it, so the binary now comes from libbpf's static releases. CI's
stable rustc is 1.98, two lints ahead of the 1.94 here; golangci-lint's
`latest` resolved to the v1 line and rejected the v2 config, and once v2
actually ran on Linux it read the two `*_linux.go` files the macOS linter
never compiles and found two unchecked closes. bpf2go names the amd64 pair
`devbox_x86_bpfel`, not `devbox_amd64_bpfel`, which is why build.rs and the
eBPF lane had never found it; the pair CI generated is now committed, so an
x86-64 source build embeds the eBPF agent too.

**Two defects the Linux runner surfaced.** W2-8: the mcp wrapper trusted
`process_group(0)` and, when it did not take effect, recorded the caller's
process group, so the reaper's SIGTERM took the whole `cargo test` down —
the two tests covering that path had `if !/proc/self/stat.exists() { return }`
at the top and had never run on macOS. The wrapper now proves `pgid == $$`
or makes its own group with `setsid` or records nothing; every reaper refuses
its own and its parent's group; the host side calls `killpg` with the same
refusals. W2-9: 147 collector daemons on this host were not a takeover leak
but `tests/mcp.rs` running the real binary with a throwaway `HOME`, three
per test run; tests now set `DEVBOX_NO_COLLECTOR_DAEMON`, takeover stops a
daemon by its process group, orphans inside a state directory are reclaimed
on the next lifecycle command, and `doctor` counts them.

**Gate** — CI run 34000677054 on `4993ad5`: Rust (ubuntu), Rust (macOS),
MSRV, Go, eBPF all green; locally 900 tests across 15 binaries, clippy
1.98.1 clean, Go lint clean under `GOOS=linux --build-tags ebpf`.

**Open, for Ethan.** Publish the drafted release notes on the GitHub
release; fast-forward `main` to `v5-main`; whether to rename the project.
Everything else is in the checklist that becomes the next cycle's issues.

## 2026-09-06T02:30Z — Next cycle, first batch: five closures and one new blocker

**Landed on `v5-main`** (`85789ca`, CI run 34012651203 green, 957 tests):

- `v5/takeover` — one judgement of daemon ownership in `src/daemon_identity.rs`,
  shared by the collector and the broker; the broker is now taken over by
  binary hash too. A daemon that holds its lock with an unreadable sidecar
  is replaced after three consecutive commands agree it is a devbox daemon,
  and never if it is not one.
- `v5/run-hardening` — the host registers a run before the command starts
  (the wrapper blocks on a FIFO until released; 20/20 runs now root at the
  user's command with cgroup-attributed events), `runs.file_scope` and
  `runs.start_gate` (schema v3), broker events carry no monotonic reading
  and sort by wall time, and `devbox store redact` rewrites the argvs
  recorded before redaction existed (a command, because it edits an audit
  log).
- `v5/guest-user` — the guest's user name is what the box says (`id -un`),
  not a plausible passwd row; an MCP session that cannot be recorded still
  runs; `mcp self` answers a batch with an explanation and names the
  protocol revision it conforms to.
- `v5/code-broker` — `devbox code` carries the broker variables as ssh
  environment: one `SetEnv` line per Host (ssh honours only the first) and
  `AcceptEnv` in the guest's sshd; the file is 0600 and a half-written
  devbox block is refused rather than repaired by deleting what follows.
- `v5/home-align` — why every Lima box had two homes: devbox created
  `/home/<user>` while Lima's cloud-init had made `/home/<user>.guest`, and
  sshd caches passwd per connection, so sessions on Lima's long-lived master
  kept the old home while a direct connection could not authenticate at all.
  The passwd home now follows the login shell; old boxes repair themselves
  on the next lifecycle command.

**Found, and now the first priority.** On Lima, a box that has been stopped
does not start again: `limactl start` waits forever for port 22 and the
serial log stays empty. Reproduced twice on clean throwaway boxes; the
morning's `devtest` restart failure was very likely the same thing. Whether
this is Lima, devbox's stop sequence, or a regression between v3 and v5 is
W3-6's question, and no 0.2.1 goes out before it is answered.
