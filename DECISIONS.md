# Architecture Decision Log — devbox v4

Append-only. Newest entries at the bottom. Each entry: **context → decision →
rationale → how to revisit**. Decisions here are made by the autonomous build
loop when `docs/plans/2026-08-06-devbox-v4-design.md` does not settle a choice;
they are deliberately biased toward the *most reversible* option that keeps the
build green.

---

## ADR-0001 — Branch `v4` from `fix/devbox-code-remote-arg`, not `main`

**Date:** 2026-08-06

**Context.** The build brief says to create `v4` from the default branch
(`main`). At the time of branching, `main` was 4 commits behind the checked-out
`fix/devbox-code-remote-arg`, which is a strict descendant of `main` and carries
four shipped bug fixes (VS Code `--remote` argument format, Lima SSH `Host`
alias rewrite, nix-ld for VS Code Server, overlay refresh before opening the
editor).

**Decision.** Create `v4` from the current `HEAD` (`d5907d2`), which contains all
of `main` plus those four fixes.

**Rationale.** `HEAD` is a fast-forward of `main`, so nothing is lost or
duplicated: `main..v4` is a superset. Branching from `main` instead would have
silently reverted four correctness fixes inside the v4 line and made a future
merge noisier. The intent of the instruction — "start from a clean, current
base" — is better served by the newer commit.

**Revisit.** If `fix/devbox-code-remote-arg` is ever rejected rather than merged,
rebase `v4` onto `main` and drop those four commits.

---

## ADR-0002 — Re-include `PROGRESS.md` in `.gitignore`

**Date:** 2026-08-06

**Context.** `.gitignore` already ignored `progress.md` (a leftover from the
`planning-with-files` plugin). The repository is on macOS with
`core.ignorecase=true`, which makes gitignore matching case-insensitive — so
`PROGRESS.md`, the file the brief requires the loop to keep current, was being
silently ignored.

**Decision.** Keep the lowercase ignores and add an explicit `!/PROGRESS.md`
negation, plus ignores for the new Go/Python trees and `.DS_Store`.

**Rationale.** Least invasive: other tooling that writes lowercase `progress.md`
scratch files keeps working, and the tracked build log stops disappearing.
Removing the lowercase entry outright would have been the alternative but would
change behavior for the plugin.

**Revisit.** If the `planning-with-files` scratch files are no longer used, drop
lines 9–11 of `.gitignore` and the negation together.

---

## ADR-0003 — Web stack: `axum` + `askama` + `rust-embed`, htmx/SSE, no build step

**Date:** 2026-08-06

**Context.** §6.1/§13 of the design fix the stack but not the exact crate
versions or how assets get into the binary.

**Decision.** `axum` 0.8, `askama` 0.16 with its `askama::Template` derive,
`rust-embed` 8 for vendored assets, and `tokio-stream` for SSE. Vendored assets (htmx, xterm.js, CSS) live in
`src/web/assets/` and are compiled in with `rust-embed`; there is no npm/node
step anywhere in the release path.

**Rationale.** These are the current stable majors, all pure-Rust, and keep
`cargo build` the single build command for the web tier. Compile-time templates
(askama) mean a broken template is a build error, not a 500 at runtime.

**Revisit.** If askama's API churns again, `minijinja` is a drop-in-ish runtime
alternative; the handler signatures would not change.

---

## ADR-0004 — Console auth: per-launch token in a cookie, loopback-only bind

**Date:** 2026-08-06

**Context.** §6.1 specifies "a per-launch random token in the opened URL (`?t=…`)
plus loopback binding", but a token that lives only in the query string is lost
on the first internal navigation and leaks into `Referer` headers.

**Decision.** The launch URL still carries `?t=<token>`; the first request
exchanges it for a `HttpOnly`, `SameSite=Strict` session cookie and redirects to
the clean path. Every subsequent request authenticates from the cookie. The
listener still binds `127.0.0.1` only. The token is 32 bytes of OS randomness,
URL-safe base64 encoded, regenerated per launch, and compared in constant time.

**Rationale.** Preserves the design's UX (one clickable URL, no accounts) while
surviving navigation and keeping the token out of browser history and `Referer`.
Loopback binding remains the primary boundary; the token defends against other
local users and against a browser page on some other origin poking at
`127.0.0.1:7878`.

**Revisit.** If the console is ever exposed beyond loopback (explicitly a
non-goal, N1), this must become real auth — not a bigger token.

---

## ADR-0005 — `devbox` with no arguments keeps v3 behavior for now

**Date:** 2026-08-06

**Context.** §5 says `devbox` (no args) should start the console. §3 also says
"`devbox` with no args still does the right thing (create-or-attach for the
current project)". These pull in opposite directions, and flipping the default
in Phase 0 would change the behavior of every existing user's muscle memory
before the console can actually manage boxes.

**Decision.** Phase 0 adds `devbox web` as an explicit command and leaves the
no-arg default as create-or-attach. The default flips to "open the console" in
Phase 1, once the console can do the full box lifecycle.

**Rationale.** Most reversible ordering: the console has to be useful before it
becomes the default entry point, and a one-line change flips it later.

**Revisit.** Phase 1, acceptance criterion "full box lifecycle from the browser".

**Resolved (Phase 1).** Bare `devbox` now *ensures a box exists for the current
directory and then opens the console on that box's page* — the union of the two
readings rather than a choice between them. `devbox shell` remains the
browser-free way to get a terminal, so headless use is unaffected.

---

## ADR-0006 — One Go module at the repository root, not one per service

**Date:** 2026-08-06

**Context.** §14 lists `agent/` and `ztpd/` as sibling trees. That says nothing
about module boundaries, and two modules would each need their own `go.mod`,
`go.sum`, lint invocation, and CI cache key.

**Decision.** A single module, `github.com/ethannortharc/devbox`, rooted at the
repository. `agent/cmd/obsd` and `ztpd/cmd/ztpd` are packages within it, and
shared code lives in `internal/`.

**Rationale.** The two binaries are versioned together (§7.3 pins the agent to
the host binary's build hash), ship together, and share the event schema and
build-identity code. One module makes `go test ./...` cover both, keeps
`internal/` genuinely internal, and avoids a `replace` directive or a published
intermediate package just to share a struct. The directory layout the design
asks for is unchanged.

**Revisit.** If `ztpd` ever ships independently of devbox — a real
possibility if the ZTP server becomes useful on its own — split it into its own
module then; the import paths already anticipate it.

---

## ADR-0007 — Build the Rust crate as a library plus a thin binary

**Date:** 2026-08-06

**Context.** v3 was a binary-only crate. Integration tests in `tests/` could
therefore only shell out to the compiled binary, which is fine for CLI parsing
but useless for the v4 subsystems: the web console's HTTP surface, the
observability collector, the policy engine, and the lab orchestrator all need
to be driven directly.

**Decision.** Add `src/lib.rs` exporting the module tree; `src/main.rs` becomes
a thin `devbox::cli` driver. Integration tests import the library.

**Rationale.** `tests/web_console.rs` now drives the real axum router through
`tower::ServiceExt::oneshot` — exercising auth, routing, rendering, and
serialization without binding a socket or spawning a process. The same lever is
what Phase 3 needs to feed recorded agent frames into the collector.

**Cost.** Everything reachable from `lib.rs` is `pub`, so `dead_code` no longer
flags unused items. Accepted: the test surface is worth more than that warning,
and clippy still covers the rest.

**Revisit.** Not expected to.

---

## ADR-0008 — Log at `warn` by default, `DEVBOX_LOG` to raise it

**Date:** 2026-08-06

**Context.** §17 asks for structured logging via `tracing`. Devbox is a
foreground CLI whose stdout is user-facing output (and sometimes piped into
`jq`), so a chatty default would be actively harmful.

**Decision.** Initialize `tracing_subscriber` with an `EnvFilter` defaulting to
`warn`, writing to **stderr**. `DEVBOX_LOG` overrides it, falling back to
`RUST_LOG` for people with the reflex.

**Rationale.** Silent by default, debuggable on demand, and stdout stays clean
for `--output json`. A dedicated variable avoids inheriting a `RUST_LOG=debug`
some other tool set in the shell.

**Revisit.** If the console grows a log view, route the same subscriber into a
broadcast layer so the UI can show what the CLI would print.

---

## ADR-0009 — Retire the TUI by replacement, and take Zellij with it

**Date:** 2026-08-06

**Context.** §5 requires the TUI to go, "by replacement, not deletion-first".
The question was how far the removal reaches: `src/tui/` also held the Zellij
layout catalogue that `devbox shell`, `devbox layout`, and provisioning all
depended on.

**Decision.** Remove `src/tui/` (ratatui package manager + layout catalogue),
`devbox layout`, `devbox packages`, and `layouts/*.kdl`. `devbox shell` now
opens a plain login shell. `zellij` leaves the default `shell` Nix set and the
`ratatui`/`crossterm`/`dialoguer`/`indicatif` dependencies leave `Cargo.toml`.
The `layout` field disappears from `devbox.toml`, `state.json`, `CreateOpts`,
and the global config. The Zellij cheat sheet stays, marked optional.

**Rationale.** The replacements shipped first and are tested: the console does
box management and the browser terminal, and the Help view renders the same
cheat sheets. Keeping a half-wired Zellij path would mean two ways to attach,
one of them untested. Old `state.json` files still load — serde ignores the now
unknown `layout` key — so no box is broken by the upgrade.

**Cost.** Anyone who scripted `devbox layout …` or `devbox shell --layout …`
must stop. Both were interactive conveniences, not automation surfaces.

**Revisit.** If a multiplexer turns out to be load-bearing for someone, the
honest fix is `devbox nix add zellij` inside the box, not resurrecting a
devbox-managed layout catalogue.

---

## ADR-0010 — Reject non-loopback `Host` headers

**Date:** 2026-08-06

**Context.** Binding to `127.0.0.1` stops remote traffic but not DNS
rebinding: a page on `evil.example` can resolve that name to `127.0.0.1` and
issue requests the browser treats as same-origin with the attacker.

**Decision.** The auth middleware rejects any request whose `Host` header is
not a loopback name (`localhost`, `127.0.0.0/8`, `::1`), with
`421 Misdirected Request`. The check runs before the public-path exemption, so
it covers assets too.

**Rationale.** Cheap, total, and independent of the cookie: a rebound request
carries the attacker's hostname whatever it resolves to. The `SameSite=Strict`
cookie already prevents the *session* from being replayed cross-site; this
closes the same-origin-by-rebinding variant.

**Revisit.** If the console ever needs to answer to a real hostname (it should
not — N1), this becomes an allowlist rather than a predicate.

---

## ADR-0011 — `DEVBOX_DOCKER_IMAGE` overrides the Docker base image

**Date:** 2026-08-06

**Context.** `DockerRuntime` hard-coded `devbox-nixos:latest`, an image that
has to be built locally and exists on no registry. That made a real end-to-end
test against the Docker runtime impossible, which is exactly what Phase 1's
acceptance criteria call for.

**Decision.** `DockerRuntime::image_name()` reads `DEVBOX_DOCKER_IMAGE`,
falling back to `devbox-nixos:latest`. `tests/e2e_docker.rs` builds a
few-megabyte busybox image with `CMD ["sleep","infinity"]` and points the
variable at it.

**Rationale.** One environment variable buys a genuine e2e test — create,
start, stop, destroy, overlay diff, and a real pty over a real WebSocket, all
through the HTTP API against a real container. It is also useful outside tests
for anyone maintaining their own base image.

**Revisit.** If per-box image selection becomes a product feature, promote it
from an environment variable to a `devbox.toml` key.

---

## ADR-0012 — Only `system` is a locked set

**Date:** 2026-08-06

**Context.** v3 hard-coded four sets as "Locked: always true" — `system`,
`shell`, `tools`, `editor`. §6.3 makes the set list a live checklist, and G5
says nothing heavy is built until asked for. A checklist with four entries you
cannot uncheck contradicts both.

**Decision.** `LOCKED_SETS = ["system"]`. Everything else, including `shell`,
`tools`, and `editor`, is toggleable. `system` stays locked because it carries
coreutils, the CA bundle, and the compiler toolchain — a box without them is
not a minimal box, it is a broken one.

**Rationale.** A genuinely minimal box (say, `system` + `lang-rust`) is now
expressible, which is the point of on-demand building. The default selection is
unchanged, so nobody's existing box shrinks by surprise.

**Revisit.** If people routinely end up with a box that has no editor and are
surprised, the fix is a better default in the create form, not re-locking.

---

## ADR-0013 — Composition writes `configuration.nix`; set modules are pushed wholesale

**Date:** 2026-08-06

**Context.** §6.3 says the composed configuration should import "only the
selected set modules". Two things could be selective: which module *files* land
in the box, and which the configuration *imports*.

**Decision.** All 15 set modules are always written to `/etc/devbox/sets/`;
`/etc/devbox/devbox.nix` — regenerated from the selection on every rebuild —
imports only the chosen ones.

**Rationale.** The modules are a few kilobytes of text that Nix never evaluates
unless imported, so writing them all costs nothing and makes toggling a set on
later a pure configuration change with no extra round trip into the box. What
actually matters — which closures get evaluated and built — is controlled
exactly as the design requires. `compose_configuration_nix` is pure and has 8
tests, including one asserting unselected sets appear nowhere in the output.

**Revisit.** If the set catalogue grows to where pushing it is slow, push
lazily and diff against what is already in the box.

---

## ADR-0014 — Rebuilds are fire-and-forget with an SSE log

**Date:** 2026-08-06

**Context.** `nixos-rebuild switch` can run for minutes. Holding the HTTP
request open for it would hit a proxy, browser, or client timeout somewhere in
between, and would give the user nothing to look at meanwhile.

**Decision.** `POST /api/boxes/{name}/sets` validates, returns `202 Accepted`
with a log panel, and spawns the rebuild. Output streams to per-box SSE events
(`build-{name}`, `build-status-{name}`) that the panel appends to. The box's
persisted set list is updated **only after** the rebuild exits zero.

**Rationale.** The user sees progress immediately, the request cannot time out,
and a failed rebuild leaves the recorded state matching the box's real state
rather than what was hoped for. Per-box event names mean two concurrent
rebuilds never interleave in one panel.

**Cost.** A page opened *after* a rebuild starts misses the earlier lines.
Acceptable: the terminal status event still arrives, and the log is not an
audit record. If it needs to become one, the collector store from Phase 3 is
the right place for it.

**Revisit.** Phase 3 introduces a real event store; build logs could move
there and become replayable.

---

## ADR-0015 — Length-prefixed JSON on the wire, not protobuf (yet)

**Date:** 2026-08-06

**Context.** §11.3 specifies "length-prefixed protobuf (or JSON in dev)". Real
protobuf means `protoc` (or `buf`) in the build path for two languages, plus
generated code checked in or regenerated in CI.

**Decision.** Length-prefixed **JSON**, with the framer deliberately ignorant of
the payload: a 4-byte big-endian length, then that many opaque bytes. The
handshake carries a `protocol` version so a payload-format change is a
detectable, refusable event rather than a silent misread.

**Rationale.** The framing — which is the part that is hard to change later —
is already protobuf-ready. What JSON buys today is a *cross-language
conformance test*: `agent/event/testdata/events.jsonl` is decoded by both the
Go and the Rust test suites, so a field rename on either side fails a test
instead of producing blanks in the console. That is worth more right now than
the bytes protobuf would save.

Two related choices fall out of this. Unused sub-objects are **omitted**, not
serialized as `null`: at 10k events/s, five null fields per event cost more
than they explain, and every reader treats absent and null identically.
`ts_wall` is millisecond-precision UTC because that string sorts
lexicographically in chronological order, which makes a plain `TEXT` column a
usable index.

**Revisit.** When event volume actually hurts. The framer does not change; only
`Encode`/`Decode` and the protocol version do.

---

## ADR-0016 — Parse DNS and TLS by hand instead of vendoring gopacket

**Date:** 2026-08-06

**Context.** §13 names `gopacket` for DNS and TLS ClientHello parsing. The agent
is embedded in the devbox binary and pushed into every box.

**Decision.** Hand-rolled parsers in `agent/decode/wire.go`. The Go module has
no third-party dependencies at all.

**Rationale.** Only two things are needed — a DNS question/answer set and a
ClientHello's SNI and ALPN — and both are small, frozen, well-specified
formats. A few hundred lines with exhaustive tests (compression pointers,
pointer loops, truncation, reserved label types, non-handshake records) beats
several megabytes of dependency in a binary that ships inside every box. It
also keeps `go.sum` empty, which is one less supply-chain surface on something
that runs privileged inside a sandbox.

**Cost.** If devbox ever needs real packet decoding — VLAN, tunnelling,
fragment reassembly — this is the wrong foundation and `gopacket` is right.

**Revisit.** The moment a third protocol needs parsing.

---

## ADR-0017 — A `fixture` capture source alongside eBPF and proc

**Date:** 2026-08-06

**Context.** Phase 3's acceptance criteria require an integration test that
generates known activity and asserts the correlated chain. eBPF cannot load on
macOS, and even on Linux a real capture is not reproducible enough to assert
exact chains against.

**Decision.** `capture.Source` has three implementations: `ebpf` (Linux with
BTF), `proc` (any Linux, degraded), and `fixture` (any host, replays a recorded
JSONL file). `devbox-obsd -fixture <path>` selects the last.

**Rationale.** `tests/obs_pipeline.rs` now builds the **real agent binary**,
runs it as a **real process**, and streams a recorded capture through the real
handshake, the real framing, the real SQLite store, and the real correlation
pass — deterministically, on any host, in about a second. The only synthetic
part is where the events came from. The fixture is also the same file both
languages' schema tests read, so it earns its keep three times over.

Asking for eBPF from a binary built without it **fails loudly** rather than
falling back to `proc`. A quiet timeline reads as "the box did nothing", which
is the worst possible failure mode for an observability tool.

**Revisit.** Not expected to; a replay source is useful for demos regardless.
