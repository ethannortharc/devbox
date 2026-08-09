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

**Superseded by [ADR-0048](#adr-0048-the-console-key-is-not-a-cookie-and-pages-are-shells).**
The cookie below is the vulnerability that ADR was written to remove: cookies
are scoped by host and not by port, so the browser handed this one to every
other service on `127.0.0.1`. Kept as written, because the reasoning that
follows is exactly the reasoning that was wrong.

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

---

## ADR-0018 — Hand-render Prometheus text; no client library

**Date:** 2026-08-06

**Context.** §7.7 requires a `/metrics` endpoint in Prometheus text format.
The obvious move is `prometheus` or `metrics-exporter-prometheus`.

**Decision.** Render the exposition format directly in `src/metrics.rs`.

**Rationale.** The format is a dozen lines of text. A client library brings a
global registry, a metric lifecycle, and a registration order to get wrong — in
exchange for formatting we can do in fifty lines with tests that assert the
things that actually break: every family declared before use, label values
escaped (a box name is user-chosen), and **every event type emitted at zero**
so a series can be alerted on before it first fires. Counters live where they
are produced (`collector::Stats`), which is where they belong anyway.

**Revisit.** If devbox ever needs histograms — `node_provision_seconds` p95 in
Phase 8 is a real candidate — bucket rendering is where hand-rolling stops
paying.

---

## ADR-0019 — `/metrics` needs no console token

**Date:** 2026-08-06

**Context.** Prometheus scrapes with no cookie and no way to obtain a
per-launch token. Requiring one would make the endpoint unusable for its only
purpose.

**Decision.** `/metrics` joins the public paths, alongside assets and
`/healthz`.

**Rationale.** It exposes counts and statuses — how many events, how many
boxes, in what state — and never box contents, file paths, domains, or command
lines. The loopback bind and the `Host` check (ADR-0010) still apply, so
reaching it already means being a local user on this machine, who could read
`~/.devbox` directly.

**Revisit.** If a metric ever carries a box-derived label beyond `status`
— a domain, a path, a command — that label leaks and this decision has to
change with it.

---

## ADR-0020 — DNS must survive every posture except `isolated`

**Date:** 2026-08-06

**Context.** An obvious reading of "default-deny egress" blocks port 53 along
with everything else.

**Decision.** `allowlist` and `mirror-only` explicitly permit UDP and TCP 53.
Only `isolated` blocks it.

**Rationale.** The allowlist names *domains*, and the firewall enforces on
*addresses*. The bridge between them is the agent watching DNS answers and
adding them to the nftables set. Block resolution and that bridge collapses:
nothing ever gets added, so the allowlist permits nothing and the posture is
not "strict", it is "broken". `isolated` is the exception because it has no
allowlist to resolve.

**Cost.** DNS is an exfiltration channel, and this leaves it open in the
enforcing postures. Mitigated by the fact that every query is *captured* —
`devbox watch --type dns` shows exactly what was asked for — but a determined
tunnel would work. Closing it properly means a resolver inside the box that
only answers for allowlisted names, which is real work for a later phase.

**Revisit.** When the lab gains its own dnsmasq (§9), it is the natural place
to put an allowlist-only resolver.

---

## ADR-0021 — Enforce from observed DNS, not from periodic re-resolution

**Date:** 2026-08-06

**Context.** An allowlist of domains has to become a set of addresses. The
usual approach is to resolve each name on a timer and refresh the set.

**Decision.** The agent adds addresses to the nftables set as it *observes*
DNS answers for allowlisted names, using the same capture pipeline that feeds
the timeline.

**Rationale.** Periodic re-resolution races the application: a CDN answers a
different address to the box than it answered to the refresher, and the
connection is blocked despite the name being allowed. Watching the actual
answer means the firewall learns the exact address the application is about to
use, in the right order, with no timer to tune. It also costs nothing extra —
the DNS events are already being captured.

**Cost.** A process that resolves by some other path (DoH, a hard-coded
address, `/etc/hosts`) is not seen, so its connection is blocked even to an
allowlisted name. That is arguably the correct outcome — an application
bypassing the resolver is exactly what a glass box should not silently permit
— but it will surprise someone, so the reason lands in a `policy` event.

**Revisit.** If DoH becomes common inside boxes, the SSL uprobe (§7.1) sees
those requests and can feed the same path.

---

## ADR-0022 — The `mirror-only` list is duplicated in Go, and a test keeps it honest

**Date:** 2026-08-06

**Context.** `mirror-only` needs the curated host list in two places: the Rust
policy engine, which decides, and the Go agent, which resolves those names into
the firewall. Sharing it would mean generating one from the other, or a data
file both read at runtime.

**Decision.** Duplicate the list, and add a Go test that parses
`src/policy/mirrors.rs` and asserts the two agree exactly.

**Rationale.** Codegen for a 38-entry list of strings is more machinery than
the problem deserves, and a runtime data file would have to be pushed into the
box and version-matched — a third thing that can drift. A test that fails the
build when the lists disagree gets the same guarantee for twenty lines. The
Rust side stays the source of truth: it carries the reasoning (which hosts,
grouped by ecosystem, and why telemetry endpoints are excluded).

**Revisit.** If the list grows past a few hundred entries, or gains structure
beyond "host string", generate the Go side from the Rust one.

---

## ADR-0023 — The lab name is validated like a node name

**Date:** 2026-08-06

**Context.** A commit-time security review flagged `src/cli/lab.rs`'s
`push_file`, which builds a shell script to write a generated FRR config into
the substrate. The path contains the lab name, and the lab name came straight
from `lab.toml` with only an is-it-empty check.

**Decision.** `Topology::validate` now holds the lab name to the same rule as a
node name: lowercase letters, digits, and `-`. A `lab.toml` naming itself
`x'; rm -rf /; '` is rejected before anything runs.

**Rationale.** The lab name is not merely a label — it becomes a network
namespace name and a path component inside the substrate. Both of those want
the same character set a node name wants, and the validator was already
enforcing it one level down. This is the fix at the boundary rather than
escaping at each use site, which is the kind of thing that gets forgotten at
the fourth use site.

**Note on the rest of that path.** The wiring and fault commands are argv
vectors executed directly — never through a shell — and a test asserts no shell
metacharacter appears in any of them. `push_file` is the one place a shell is
involved, because writing a file into a box through `exec` needs one; the
content is base64-encoded for exactly that reason, and now the path is
constrained too.

**Revisit.** If `push_file` grows more callers, give it a non-shell
implementation (a runtime `copy_into` method) and delete the question.

---

## ADR-0024 — veth names are indexed, not derived from node names

**Date:** 2026-08-06

**Context.** The first wiring implementation named each veth end
`{node}-{iface}`, truncated to Linux's 15-character interface limit.

**Decision.** Name them `dvb{link_index}{a|b}`.

**Rationale.** Both ends of every pair exist in the **root** namespace at the
moment they are created, so their names must be unique there. Truncation makes
that false: two node names sharing a 15-character prefix produce the same veth
name, and `ip link add` either fails or — worse — the second pair attaches
where the first one was. The names are transient anyway; each end is renamed to
its topology interface name the instant it is inside its namespace, so nothing
is lost by making them opaque and everything is gained by making them unique by
construction. A test walks 500 links and asserts no collision.

**Revisit.** Not expected to; if a lab ever exceeds ~10⁹ links the format
string is the least of the problems.

---

## ADR-0025 — The selection drives `devbox-state.toml`, not just `devbox.nix`

**Date:** 2026-08-06

**Context.** A code review found that `sets apply` wrote the composed selection
to `/etc/devbox/devbox.nix` — a file nothing in the box imports. The NixOS
module a provisioned box actually loads reads `/etc/devbox/devbox-state.toml`.
So the command reported success, persisted the new selection, and left the
installed closure exactly as it was.

**Decision.** `write_set_modules` writes **both**: `devbox.nix` as the readable
record of what was composed, and `devbox-state.toml` — derived from the same
`Selection` — as the file the module reads.

**Rationale.** The alternative (change the module to import `devbox.nix`) would
break every box provisioned before this change, because the module is baked in
at provision time. Writing both keeps existing boxes working and makes the
command truthful today. Also fixed alongside: `active_sets()` no longer forces
`shell`/`tools`/`editor` on, which had made unchecking them a no-op that
contradicted ADR-0012.

**Revisit.** When the module itself can be updated in place, `devbox.nix`
becomes the single source and the TOML can go.

---

## ADR-0026 — Config writes never fall back to defaults

**Date:** 2026-08-06

**Context.** `DevboxConfig::load_or_default` silently substitutes a default
config for one that fails to parse. Read paths do not care. Write paths do:
`devbox policy set` on a project whose `devbox.toml` has a syntax error would
load defaults, apply the policy, and save — erasing mounts, resources,
environment, and set selections.

**Decision.** Add `load_for_edit`, which errors on a file that exists but does
not parse, and use it on every path that writes the config back.

**Rationale.** The failure is silent, total, and hits exactly the file a user
hand-edited. Two functions with names that say which is which beats one
function everyone has to remember not to misuse.

---

## ADR-0027 — Lab commands are privileged; the substrate provides `sudo`

**Date:** 2026-08-06

**Context.** `ip netns add`, veth creation, `sysctl`, and `tc` all need
`CAP_NET_ADMIN`. Runtime `exec` runs as the ordinary VM user on Lima and
Multipass, so `devbox lab up` failed on its very first command.

**Decision.** Every generated wiring and fault command is prefixed with `sudo`
in `wiring::privileged` / `wiring::in_node`, and a test asserts no generated
command is missing it. The e2e substrate image installs `sudo` rather than
running as root, so the test exercises the same path a real substrate does.

**Rationale.** Prefixing at the generator means no future command can forget.
Testing against a root container would have made the whole class of bug
invisible.

---

## ADR-0028 — `lab up` reports what it did not start

**Date:** 2026-08-06

**Context.** `devbox lab up ztp-fabric` wired namespaces and wrote router
configs, then exited zero — having started no dnsmasq, no chrony, no `ztpd`,
and no bootstrap. The documented flagship scenario looked like it worked.

**Decision.** `lab up` ends by listing every service the topology asks for that
this bring-up did not start, and says the wiring and configs above are real.

**Rationale.** Service orchestration is genuinely not built. The choice was
between removing the scenario, silently succeeding, or saying so — and only the
last leaves the topology, address plan, and generated configs useful while
being honest about the gap. A silent success would cost someone an hour of
debugging a fabric that was never asked to provision.

**Revisit.** When `lab up` grows service orchestration, this function's list
shrinks to nothing and can be deleted.

## ADR-0029: `system` is the only locked set — the module must honour the rest

**Status:** accepted (2026-08-06)

Round 2 of review found that `nix/devbox-module.nix` unconditionally installed
`shell`, `tools`, and `editor`, and never read `custom_packages`. The console
would report the new selection, the rebuild would report success, and the box
would install the old set. A toggle that is displayed but not obeyed is worse
than no toggle, because it is believed.

The module now gates all three on the state file, defaulting to `true` so boxes
provisioned before this change are unaffected, and appends validated extra
packages via `pkgs.${name} or null`. `LOCKED_SETS` stays `["system"]`, which is
what §6.4 actually says.

## ADR-0030: enforcement is a code path, not a printed suggestion

**Status:** accepted (2026-08-06)

`devbox policy set isolated` wrote the posture to `devbox.toml` and printed
"apply it with `devbox reprovision`". No provisioning path ever generated or
loaded a ruleset, so the box kept unrestricted egress while reporting
`isolated`. Round 2 caught it; the honest description is that §8 was half
implemented and the printed line covered the gap.

`policy::enforce` now pushes the generated ruleset into the box and loads it,
`policy set` applies it to a running box immediately, and `reprovision`
re-applies the saved posture after rebuilding the network stack. `open` clears
devbox's table rather than installing an empty one.

## ADR-0031: pids and monotonic clocks are not identities

**Status:** accepted (2026-08-06)

Two ordering bugs with the same root: treating a value as unique when the
kernel reuses it.

`ts_mono_ns` restarts at each guest boot while the event store persists across
boots, so sorting a summary by it put a fresh boot's events before everything
older. Behavior summaries now sort by wall clock with the monotonic value as
the tie-breaker — the tie-break is what keeps sub-millisecond ordering stable
within one boot.

Linux recycles pids, so `chains()` grouping on pid alone merged unrelated
processes' commands, parents, peers, and files into one chain. Chains now break
on a second `exec`, on `exit`, and after a 60s gap.

The Go agent had the mirror image: `-no-ebpf` mode anchored `Boot` to agent
startup, so every restart reset the clock. It now derives boot time from
`/proc/uptime`.

## ADR-0032: durability before acknowledgement in ZTP

**Status:** accepted (2026-08-06)

§10.3 kills `ztpd` mid-provision and asserts the fabric self-heals with
`attempts > 1`. A 2s save ticker lost whatever landed in the last tick — which
is exactly the window the chaos test aims at, so the test could pass while the
guarantee it measures did not hold. `Registry.Persisting` now saves after every
mutation, before the caller is told it happened. The cost is one small
synchronous write per state transition; at fabric scale that is nothing.

## ADR-0033: the agent owns the allow set, because DNS is where names become addresses

**Status:** accepted (2026-08-06)

Turning on enforcement in ADR-0030 exposed the half that was missing. An
allowlist names *domains*; nftables matches *addresses*. The generated ruleset
is default-deny with `allow_v4`/`allow_v6` seeded only from literal CIDRs, and
the only code that could add resolved answers — Go `Enforcer.OnDNS` — was never
instantiated. So `allowlist` and `mirror-only` did not merely under-enforce
once enforcement was live: they blocked exactly the traffic they promise to
permit.

`devbox-obsd -policy /etc/devbox/policy.json` now builds the enforcer, loads
the ruleset, and adds every answer for an allowlisted name as it captures it.
The control plane writes that file next to the ruleset. The agent is the right
owner: it is already watching DNS, so the firewall learns an address from the
same resolution the application is about to use — no polling, no TTL guessing.

`nftables` moved into the `system` set at the same time. Enforcement that
depends on the user having ticked an optional checkbox is not enforcement.

## ADR-0034: policy is applied in the start lifecycle, not at the point of decision

**Status:** accepted (2026-08-06)

A firewall does not survive a box restart. Applying a posture only when it is
*set* meant enforcement lasted until the first reboot and then vanished, while
`devbox.toml` and the console kept reporting it — the same "displayed but not
enforced" failure ADR-0030 was written to end, one layer down.

`service::start_box` now applies the saved posture on every start, and
`policy allow` reapplies on a running box. Failure is reported, not fatal:
refusing to start a box because its firewall could not be installed would
strand the user with no way in to fix it.

## ADR-0035: generated modules must not overwrite guarded ones

**Status:** accepted (2026-08-06)

`write_set_modules` regenerated every set as a flat package list, including
`ai-code.nix` and `ai-infra.nix` — which are checked in precisely because they
wrap each optional tool in `tryEval`, since some are absent or broken on a
given nixpkgs channel. One unavailable optional tool then failed the entire
rebuild, for a set that is on by default. Those two are now embedded with
`include_str!` and pushed verbatim.

Related, same root cause of "a flat list loses structure": a bare dotted TOML
key like `python312Packages.ipython` is a *nested table*, so `attrNames` gave
`python312Packages` and the module handed an entire package set to
`systemPackages`. Keys are quoted on write and resolved with `attrByPath`.

## ADR-0036: refuse a posture that cannot be enforced

**Status:** accepted (2026-08-06)

Round 4 found the hole under ADR-0033: the agent that fills the allow set is
not part of provisioning. Nothing pushes `devbox-obsd` into a box, imports its
module, or enables the service. So wiring `-policy` into the agent made the
*agent* correct while leaving the deployed system unable to run it.

The failure mode is the dangerous direction of wrong. `allowlist` loads a
default-deny ruleset whose allow set only the agent can populate, so an
allowlisted domain is **blocked**. The user asks for less egress and gets none,
while the console reports the posture applied.

`enforce::apply` now probes for a running agent and refuses a domain-based
posture without one, naming what would have been blocked. CIDR allowlists,
`isolated`, and `open` need no agent and still apply. The obsd module says
plainly at the top that provisioning does not yet install it.

This is deliberately not a "make it work anyway" fix. Shipping the agent into
every box is real work — a binary to embed, a module to import, a service to
supervise, a version pin to honour — and doing it badly under review pressure
would be worse than an honest refusal that names the gap.

## ADR-0037: durability and enforcement failures are errors, not log lines

**Status:** accepted (2026-08-06)

Three round-4 findings, one mistake: I reported failures where the caller could
not act on them.

- `Registry.saved()` logged a failed write and let the mutation return success,
  so a node was acknowledged before its state was durable — the exact guarantee
  per-mutation saving exists to provide. It now fails the mutation.
- `Registry.Save` took its lock *after* snapshotting, so an older snapshot
  could still overwrite a newer one. The lock now covers both.
- `policy set`/`policy allow` printed enforcement errors and exited 0. A script
  that gets exit 0 from `devbox policy set isolated` is entitled to believe the
  box is isolated. Saving a file is not enforcing it, and only the exit status
  can distinguish them.

And the same shape in `apply_saved`: it used `load_or_default`, so a malformed
`devbox.toml` became the *default* config — posture `open`. Corruption silently
unfirewalled a box that had been isolated. It loads fallibly now. Corruption is
not consent.

## ADR-0038: a namespace is not a machine

**Status:** accepted (2026-08-06)

`lab up` wrote every `frr.conf` and printed success on the strength of a
comment claiming "the routing daemon is started by the node's own service
manager, which the substrate provisioning installs." Neither half was true: a
network namespace has no init, and nothing in provisioning installed one. So a
routed lab came up with adjacent nodes able to ping and BGP never started —
non-adjacent loopbacks simply never converged, and the only symptom was a
scenario quietly failing its assertions.

`frr::start_commands` now starts `zebra` then `bgpd` under `ip netns exec`,
with a per-namespace socket and pidfile directory. The ordering matters —
`bgpd` talks to `zebra` over zserv, so starting it first gives a daemon with
nowhere to install what it learns — and so does the socket path, since every
namespace runs its own `zebra` and a shared `/var/run` would have them all
talking to whichever started first.

The comment is the lesson: it described an architecture nobody had built, and
it read plausibly enough to survive four review rounds.

## ADR-0039: liveness is not capability

**Status:** accepted (2026-08-06)

ADR-0036's guard checked that `devbox-obsd` was running. But the degraded proc
source (§13) captures processes and sockets and no DNS at all, so an agent
started with `-no-ebpf` passed the check and still could not populate a single
allow-set entry — leaving exactly the default-deny-with-empty-allowlist that
ADR-0036 exists to prevent.

The guard now reads the agent's command line and requires a DNS-capturing
source. The general form: when a guard exists to establish that something *can
be done*, checking that the thing which would do it is *present* is a different
question, and the gap between them is where this class of bug lives.

## ADR-0040: privilege is decided in the guest

**Status:** accepted (2026-08-06)

Every command `policy::enforce` runs was prefixed with `sudo`. That is wrong in
both directions: a container exec is already root and may have no `sudo`
installed at all, while a VM's user needs it. Once clearing ran on *every*
start of an `open` box — which round 6 required, so a box that was `isolated`
and is now `open` does not keep its old rules — the container case became a
box that refused to start.

Scripts are now wrapped in `if [ "$(id -u)" -eq 0 ]; then sh -c …; else sudo sh
-c …; fi`. The guest is the only place that knows. `sh` rather than `bash` for
the same reason: nothing here uses a bashism, and not every image ships bash.

Clearing also exits 0 when `nft` is absent, because a box with no firewall
provably has no devbox rules to remove — refusing to start it would be
punishing the user for a cleanup that had nothing to clean.

## ADR-0041: report saving and enforcing separately

**Status:** accepted (2026-08-06)

The Policy tab saved the file, said "run a reprovision to apply it", and
reprovision never applied it either. Making it apply raised the question of
what to do when the save succeeds and the apply does not — and the first
answer, a 500, was wrong: the policy really is saved, and a user told only
"error" does not know whether to re-enter it.

The tab now reports both outcomes in one message, naming the posture and the
entry count in either case, and styles the failure as an error. The CLI does
the same thing with its exit status (ADR-0037). Two facts, two reports.

## ADR-0042: DNS-derived allow entries expire; stated ones do not

**Status:** accepted (2026-08-06)

`OnDNS` recorded every resolved address permanently, in both nftables and its
own `seen` map. An allowlisted domain that rotates addresses therefore left the
old ones reachable for the life of the box — and a CDN address later reassigned
to someone else stayed permitted, silently widening a default-deny posture the
longer it ran.

The allow sets now carry `flags interval,timeout` with a one-hour default, and
`seen` records *when* an address was added so an expired entry can be added
again. A domain in steady use never lapses, because every fresh resolution
refreshes it.

The distinction matters: an address the user stated is a decision, an address
the agent inferred from a DNS answer is an observation, and observations should
not outlive their evidence.

**Correction (round 8):** this ADR originally claimed CIDRs written into
`elements` carry no timeout and never expire. That was false. A set-level
`timeout` is the *default* for elements that do not state one, including
initializer elements — so the stated CIDRs expired after an hour with nothing
to repopulate them, and a CIDR-only allowlist would work and then silently
stop. The two kinds now live in separate sets (`static_v4`/`static_v6` without
a timeout, `allow_v4`/`allow_v6` with one), and both are consulted. Asserting
the intended behaviour in prose is not the same as implementing it.

## ADR-0043: convergence counts what was expected, not what showed up

**Status:** accepted (2026-08-06)

`Summarize()` can only see nodes that have identified themselves, so a node
that never boots is invisible to it. Nineteen healthy out of an expected twenty
reported `converged: true` — the one answer a fabric-convergence signal must
never get wrong, because it is what a test and an operator both key off.

`/status` and `/metrics` now count the catalog's serials, report `missing`, and
require it to be empty. The same reasoning applies one level down: the
bootstrap called a node healthy when `show bgp summary` exited zero, which it
does whenever bgpd is answering even with every neighbour Idle. It now waits
for sessions to leave Idle/Active/Connect, bounded, and reports `failed` if
they do not.

## ADR-0044: who is asking decides what an enforcement failure means

**Status:** accepted (2026-08-06). Supersedes the conflict between ADR-0034 and
ADR-0037.

I wrote both sides of this and shipped the contradiction. ADR-0034 said an
enforcement failure must not be fatal, "because refusing to start a box because
its firewall could not be installed would strand the user with no way in to fix
it." ADR-0037 then made it fatal, because a caller that cannot see the failure
cannot act on it. Round 9 found the result: a box whose posture could not be
applied became permanently inaccessible — terminal, attach, and exec all
repeated the same error — which is exactly the stranding ADR-0034 named.

Both were half right, because the two callers are asking different questions:

- **`policy set` / `policy allow` — "make this true."** The failure *is* the
  answer, and it must reach the exit status, or a script cannot tell a saved
  posture from an enforced one. These call `apply` directly and propagate.
- **start / attach / exec / console — "let me in."** The user is not asking
  about policy. Locking them out of a running box because its firewall failed
  strands them with no way to fix the thing that failed — and the box is no
  more exposed than it was a moment earlier, running without the posture while
  nobody was blocked.

So `apply_saved` returns `Ok` after reporting loudly: `tracing::error!` plus a
stderr warning that names the posture *not* in force. It never returns `Ok`
quietly — the failure must be impossible to mistake for enforcement, which was
ADR-0037's real point, separate from who gets to be blocked by it.

The lesson is not about firewalls. Two ADRs can each be locally right and
jointly produce a broken system, and nothing in a per-change review catches
that. It took a reviewer holding the whole thing at once.

## ADR-0045: one policy, every hook

**Status:** accepted (2026-08-06)

The ruleset filtered `hook output` only. With the `container` set enabled —
nested Docker, which is a headline feature — every packet a container sends is
*forwarded*, not output. So `docker run … curl` walked past `isolated` and
every allowlist: the command a developer is most likely to run inside a
sandboxed box was the one the sandbox did not cover.

`emit_policy_rules` is now shared by `output` and `forward`, with a test
asserting the two chains carry identical rules. Factoring it out is the point —
a rule added to one chain and forgotten in the other is a hole shaped exactly
like this bug, and a shared emitter makes that impossible rather than merely
unlikely.

## ADR-0046: a blanket exemption in a default-deny firewall is a hole

**Status:** accepted (2026-08-07). Reverses the round-4 deferral.

Two exemptions were written as blanket rules because the generator had no way
to know the specifics, and I deferred them in round 4 as "hardening beyond what
§8 specifies, deserving a design decision rather than a reflex fix." The
reviewer raised them again in rounds 5, 9, and 11. It was right and I was
wrong: these are not hardening, they are bypasses.

- **DNS.** `udp dport 53 accept` with no destination meant a process reached
  any endpoint on the internet by speaking to port 53 — the allowlist was
  advisory for anything willing to use one port number. Scoped now to the
  resolvers in the box's own `/etc/resolv.conf`, parsed and validated as
  addresses because the result is loaded as root. No resolver found means no
  exemption: name resolution then visibly fails, which is safer than a policy
  that quietly does not enforce, and the command says so on stderr.
- **`isolated`.** All of RFC 1918 and ULA were permitted so lab traffic would
  work. On a box with a route to a home or corporate network — which is most
  boxes — that reached the LAN router while the posture promised lab-internal
  traffic only. Scoped now to the prefixes of a lab actually running on the
  box; with no lab, `isolated` means loopback.

What I got wrong was the framing, not the caution. I treated "changes what a
posture means" as a reason to defer, when the posture already did not mean what
it said. Deferring a security fix because its semantics deserve thought is only
correct if the current semantics are defensible, and I never checked whether
they were.

## ADR-0047: only reverse a switch that happened

**Status:** accepted (2026-08-07)

Round 9 added `nixos-rebuild switch --rollback` on any non-zero exit. But
`--rollback` activates the generation *before* the current one, and an
evaluation or build failure never moves the profile — so a failed Sets apply
would undo the user's last *successful* configuration. The fix for "a failed
activation is not rolled back" created "a failed evaluation rolls back
something unrelated."

`/run/current-system` is read before and after. Same path means nothing was
activated and the box is genuinely untouched; different (or unreadable) means
the switch may have happened and is reversed. Unreadable counts as changed
because an unnecessary rollback is recoverable and a skipped one is not.

---

## ADR-0048: the console key is not a cookie, and pages are shells

**Date:** 2026-08-09

**Supersedes ADR-0004**, whose session cookie was the vulnerability.

**Context.** A cookie is scoped by host, and a host has no port. Every request
the browser made to any other service on `127.0.0.1` therefore carried the
console's cookie — so a project's own dev server on `:3000` could read the
console token out of its inbound `Cookie` header and drive the console with it:
start, stop, destroy, and a terminal into any box.

Round 39 named it and round 39's fix did not land, because the first sketch was
wrong. "Store the token in `sessionStorage` and send it as a header" covers API
calls and not page navigation — and navigation is the reason the cookie existed.

No header check can close this either. The replayer is not a browser: it omits
`Origin` and `Sec-Fetch-*`, which the guards must tolerate because a genuine
navigation omits them too, and it can forge them just as cheaply. Headers are
not secrets. The prior justification — "a browser new enough to be steered into
this attack is new enough to send the header" — was answering a threat model
with a browser in it.

**Decision.** Two per-launch secrets, and no ambient credential at all.

- `token` rides the printed URL (`?t=…`) and buys exactly one thing: a bootstrap
  page that installs the key. It is never authority for anything else.
- `key` lives in `sessionStorage`, which is scoped to an origin — *port
  included* — so another loopback service cannot read it, and to a single tab.
  It is presented as `X-Devbox-Key`, or as `?k=` on the two channels that
  cannot set a header (`EventSource`, `WebSocket`), and never on a navigable
  page.
- A navigation cannot present anything, so it is answered with a fixed,
  data-free shell that fetches the real page itself. The shell is served to
  anyone and discloses less than `/metrics` already does.

The two secrets must stay independent. A cookie could only be kept by adding a
*third*, because its value was the token and `?t=` mints credentials — so
stealing the cookie would have re-bootstrapped a fresh key. Three secrets to
protect an information-free shell is not a trade worth making.

**Rationale.** The credential no longer travels anywhere the attacker can stand.
It also retires CSRF as a class here: forgery rides credentials the browser
attaches by itself, and there no longer is one. The `Origin` and fetch-metadata
guards stay as defence in depth — a WebSocket upgrade is exempt from CORS — but
nothing load-bearing rests on them.

**Why per tab, added in round 40.** Same origin is not the same program. The
console binds a predictable port, so a page served earlier from that port by
something since stopped shares this origin exactly. `localStorage` is shared by
every tab on an origin and announces writes through the `storage` event, so such
a page — still open — was handed the key the instant the console installed it,
and could replay it same-origin against the terminal and lifecycle routes. That
is the cookie's failure again in miniature: a credential readable by something
that is not the console.

`sessionStorage` is per tab, so no other tab can read it and no cross-tab event
fires. **The residual, recorded rather than hidden:** within one tab, history or
bfcache could restore that earlier page into a tab whose storage now holds the
key. Nothing available to a page on a fixed loopback origin closes that. An
ephemeral port would remove the predictable precondition instead, and was
weighed and declined here — it costs the stable URL §6.1 specifies, and narrows
rather than eliminates.

**Cost, recorded honestly.** The console needs JavaScript and `sessionStorage`,
and both failure modes say so on the page. Per-launch keys mean a relaunched
console leaves every open tab holding a dead key; that is answered with a 401,
which clears the key and prints what to do. A freshly typed URL or bookmark
during a live launch has no key and gets the same notice — tabs opened *from*
the console inherit it, and session restore keeps it, so this is narrower than
it sounds, and a bookmark never outlived a relaunch in any case.

**Revisit.** Unchanged from ADR-0004: if the console is ever exposed beyond
loopback (N1), this must become real auth, not a longer key.
