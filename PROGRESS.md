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
| 7 | Fault injection & scenario library | in progress |
| 8 | ZTP fabric + SoT + config-gen | not started |
| 9 | Polish, docs, examples | not started |

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
