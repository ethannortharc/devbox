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
| 3 | Observability capture (Go agent + eBPF) | in progress |
| 4 | Observability presentation + behavior diff | not started |
| 5 | Egress & activity control | not started |
| 6 | Box lab: substrate & topology | not started |
| 7 | Fault injection & scenario library | not started |
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
