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

**Decision.** Two per-launch secrets, no ambient credential, and a new browser
origin for every launch.

- `token` rides the printed URL (`?t=…`) and buys exactly one thing: a bootstrap
  page that installs the key. It is never authority for anything else.
- The printed URL uses a random `devbox-….localhost` hostname that resolves to
  loopback, while the listener remains fixed at `127.0.0.1:<port>`. `key` lives
  in `localStorage`, scoped to that one-time origin — *port included*. Tabs for
  the current launch can share it; another loopback service, the stable
  `127.0.0.1` origin, and earlier launches cannot read it. It is presented as
  `X-Devbox-Key`, or as `?k=` on the two channels that cannot set a header
  (`EventSource`, `WebSocket`), and never on a navigable page.
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

**Why a random origin, revised in round 41.** Same origin is not the same
program. `localStorage` was unsafe on predictable `127.0.0.1:7878`: a page
served earlier from that port by something since stopped could still be open
and would receive the key. Moving only to `sessionStorage` closed that leak but
made an independently opened tab unusable, which is not a product-ready console.

The stable loopback URL is now an entry point rather than the credential's
origin. Bare page navigations redirect to this launch's unguessable
`devbox-<128-bit hex>.localhost` name. An old fixed-origin page cannot know or
observe its storage, while every real console tab can share it. API clients and
health checks may still use the bound loopback address directly.

**Cost, recorded honestly.** The console needs JavaScript and `localStorage`,
and both failure modes say so on the page. A restart creates a new hostname, so
old tabs retain only an expired key on an origin the new service does not
accept. The user opens the newly printed launch URL once; after that, new tabs
can use either the header shortcut or the stable loopback URL.

**Revisit.** Unchanged from ADR-0004: if the console is ever exposed beyond
loopback (N1), this must become real auth, not a longer key.

---

## ADR-0049: the console tails the store, and pushes a signal rather than rows

**Date:** 2026-08-22

**Context.** The Activity tab refreshed itself with `hx-get … every 2s` and
`hx-swap="innerHTML"`: it re-read two hundred events, re-rendered them, and
replaced the whole stream twice a second whether or not anything had happened.
Scroll position and text selection died on every swap, a burst inside the
window was never seen, and a quiet box paid the same price as a busy one.

`Collector::subscribe()` — an in-process fan-out of every stored event — had
existed since Phase 3 with no consumer, and `sse.rs` still promised "(from
Phase 3) the observability event feed". It cannot be the mechanism. ADR-0031
moved the collector into its own long-lived process precisely so capture would
not stop when the browser did, and a `tokio::broadcast` does not cross a
process boundary.

**Decision.** The event store is the shared medium, and the cursor is a row id
paired with the identity of the store it indexes.

- `Query` gains `after_id` and `before_id`; `Store::max_id` and
  `Store::tail_scan` expose the tail. A tail is `WHERE id > ? AND id <= ?` on
  the primary key: O(new rows), and nothing on an idle box.
- The row id, not a timestamp: `ts_wall` ties for events captured in the same
  millisecond, and a tie makes a resume either repeat a row or drop one.
- **Paired with the store's inode**, because a row id alone does not identify a
  position. A box destroyed and recreated under the same name gets a different
  store whose ids start again at one, and a page holding id 10 either waits for
  the new box to reach 10 — skipping its first ten events — or, once it has,
  interleaves two boxes' events in one timeline with nothing marking the seam.
  A cursor whose store no longer matches is reset, and that response carries
  `HX-Reswap: innerHTML` so the page *replaces* its stream rather than
  appending to it.
- `tail_scan` returns the highest id **examined**, not the highest decoded. A
  row written by an older schema still parses as a row; anchoring on what
  survived decoding hands the caller the same undecodable rows forever.
- One `max_id` per request bounds the backlog count, the page, and the cursor
  the page leaves with, so a batch committed mid-request cannot make any two of
  them disagree.
- `web::tail` polls `max_id` per box twice a second while a console is
  connected, and publishes **a signal**, not rows.
- A page that hears the signal issues one `hx-get` for what *it* is missing and
  prepends the result, returning its new cursor out-of-band. `hx-sync` keeps a
  filter change and a tail from racing on the same element.
- A backlog larger than one page delivers the **newest** page and says how many
  it jumped, rather than draining oldest-first: a live view that lags ten
  minutes behind a burst is not a live view, and a silent gap is worse than a
  stated one.

**Why a signal and not the rows.** Pushing rendered rows is one round trip
cheaper and wrong twice over. The anchor would be the server's, so a tab opened
five minutes later sees duplicates or a gap depending on which side is ahead;
and one rendered payload cannot serve readers whose filters differ. Both
problems disappear when the cursor and the filter belong to the page.

**Cost, recorded honestly.** A burst costs one HTTP round trip per page, and
half a second of latency in the worst case. Server-side polling remains
polling — it is simply polling something local and indexed instead of something
remote and rendered. `every 20s` survives as a reconnect net, not as the
mechanism.

**What stays a snapshot, and what does not.** The stream is appended to; the
density strip, the view switcher's counts and the six analysis views are one
region re-read together on the same signal. Splitting them was tried and is
wrong: a live strip above a table frozen at page load reports two different
windows a few centimetres apart. One swap cannot both append and replace, which
is why they are two regions and not one.

**Revisit.** If the collector ever gains a control socket the console can dial,
the signal becomes a push and this loop goes away. Nothing above the signal has
to change for that.

---

## ADR-0050: an observability system that cannot report its own health has none

**Date:** 2026-08-22

**Context.** A box registered before v4 has no `devbox-obsd`. Its exec agent
therefore closed immediately, the collector logged `agent closed the connection
before saying hello` every thirty seconds for six days, and the Activity tab
said: *No observability data yet*. That sentence was also what the tab said for
a box that was merely stopped, for a box no collector had reached yet, and for
a host with no collector daemon running at all.

Every counter needed to tell those apart existed. `Stats` distinguishes
`dropped` from `persist_failed` on purpose; `devbox doctor` already probes the
guest for `agent=missing|broken|<version>`. None of it reached the console, so
the product's answer to "why is the glass box empty?" was a log file the user
had no reason to know about.

**Decision.** The collector publishes per-box capture health to
`boxes/<name>/capture.json`, replaced atomically, the same discipline
`metrics/collector.json` already used. It records the state
(`box_stopped`/`starting`/`streaming`/`failed`), the transport, the backends the
agent reported in its hello, the agent's version, the attempt count, and — for
a failure — **the agent's own last line of stderr**.

`activity::capture_view` resolves that record, the daemon's lock, and the box's
status into one status bar with a level, a headline, and where possible a
remedy. It is a pure function, so all four situations are unit-testable without
a runtime, a daemon, or a box.

**Why the guest's stderr.** `agent closed the connection before saying hello` is
the symptom, and it is identical for a missing binary, a `sudo` that wants a
password, and a version-pinned rejection. `sh: /usr/local/bin/devbox-obsd: not
found` is the cause, and it is the only one of the two that implies
`devbox reprovision`. It existed on a pipe the supervisor was already reading
and discarding one line at a time.

**Why the daemon check outranks the record.** A stopped daemon stops updating
every box's record, so an old `streaming` would read as healthy forever. The
one reading that is definitely stale must not be the one shown.

**Precedence, and why it is ordered this way.** The daemon's lock outranks the
record; a probed `stopped` or `missing` box outranks a `streaming` record, since
the host-side socket listener outlives the container that was dialling it. But
`unreachable` and `unknown` do *not*: they mean the probe could not decide, and
a powered-on Lima VM under load reads `unreachable` while its agent streams
perfectly well. Contradicting a live stream is a worse answer than saying
nothing.

**Cost.** One small file per box, two cheap reads per Activity page load, and a
`starting` record that carries the previous failure's text so a flapping agent
does not blank its own diagnosis between retries. Deliberately *not* a stored
row count: that was one `SELECT COUNT(*)` per status bar, a full scan on a store
near its two-million-row retention limit, twice a second on the live path. The
window's own event count sits directly below the bar and is the number a reader
was looking for anyway.

---

## ADR-0051: the console reads ZTP through the substrate, not around it

**Date:** 2026-08-23

**Context.** `devbox lab up ztp-fabric` provisions blank nodes for real — DHCP
options 66/67, a live `udhcpc` on each node, a rendered config fetched and
applied, a self-check, and a convergence gate that waits for every catalogued
serial. All of it was invisible in the console. `grep -rn "ztp" src/web/`
found nothing: `/labs/ztp-fabric` drew the *planned* topology and the traffic
counters in the local event stores, and the state machine — the thing the
scenario exists to demonstrate — lived only in `ztpd`'s output, the CLI's, and
Grafana.

The obvious fix is to let the console dial `ztpd`. ADR-era `docs/ztp.md`
already forbids it, and for a reason worth repeating: the operator listener
binds `127.0.0.1:9090` *inside the service namespace* because a bare `:9090`
binds every interface, the provisioning network included, and a ZTP server is
multi-homed by definition. Its routes return the whole inventory — serials,
names, roles, states, config hashes — unauthenticated, on the assumption that
only a management network can reach them. Moving that assumption to satisfy a
web page would undo a boundary that was got wrong once already.

**Decision.** The console asks the substrate to fetch it, over the same exec
channel every other lab operation uses:

```
ip netns exec <service-ns> wget -qO- http://127.0.0.1:9090/status
```

`lab::ztp_status` owns the read and the view model; `web::labs::ztp` resolves
the substrate and the namespace from the scenario's own `ZtpPlan`, so the
console never learns the address and the listener never moves.

**Four phases, not seven states.** `discovered → identified → rendering →
pushing → verifying → healthy/failed` is the machine's vocabulary. The reader's
question is whether a node is done, moving, or stuck, so the view collapses
them into `healthy`, `failed`, `waiting` (discovered only) and `working`
(everything else) — and an *unrecognised* state is `working`, so a ztpd from
another release cannot paint a healthy fabric red.

**Serials the registry has never seen are rendered anyway.** `Summarize` can
only describe nodes that have identified, so a node that never boots is
invisible to it — the same trap `ztpd` documents having fallen into, where
nineteen healthy out of an expected twenty reported `converged: true`. The
view merges `missing` into the node list as `waiting`, saying plainly that the
serial has never contacted the server.

**Cost.** One exec round trip every two seconds while a lab page is open, and
only for scenarios that declare a ZTP plan. Polled rather than driven by the
collector's signal, because what is being watched is a service inside the
substrate rather than a local store.

**Revisit.** If `ztpd` ever gains an authenticated operator route, the console
can dial it directly and this indirection goes away. Nothing above the read has
to change for that.


## ADR-0052 — `nix/sets/*.nix` is what the box installs

**Status.** Accepted.

**Context.** Three code paths wrote the same guest files, `/etc/devbox/sets/`:
provisioning pushed the checked-in `nix/sets/*.nix` verbatim, while
`write_set_modules` (Sets apply, the console's build) and `apply_config`
(`devbox upgrade`) *regenerated* every module from `NIX_SETS`, the package-name
index, as a flat list of attribute names.

For thirteen of the fifteen sets the two agreed exactly, so the divergence was
invisible. It was not invisible for the two where a set is more than a list of
names. The AI sets wrap each optional tool in `tryEval`, because some of them
are absent or broken on a given nixpkgs channel; `write_set_modules` knew this
and exempted them, `apply_config` did not, so `devbox upgrade` could replace the
guards and fail the rebuild of a box whose selection nobody had touched. And
`network` builds a small derivation to symlink FRR's `zebra` and `bgpd` out of
`libexec` — where a NixOS system profile does not look — onto PATH. That
derivation cannot survive a round trip through a list of names.

The result was a box that was correct when created and quietly wrong after its
first Sets apply: `nixos-rebuild` reported success, and `devbox doctor` reported
`lab: missing: zebra bgpd`. Every lab scenario was unreachable on the one image
devbox builds by default.

**Decision.** The checked-in module is the artifact. `NIX_SET_FILES` moves to
`nix::sets` and all three paths push it, index included. A set is a Nix
expression and some of them need to be.

`NIX_SETS` keeps its own job — it is the package *index*: what the console
lists, what `resolved_packages` diffs, what the non-NixOS `nix profile install`
path consumes. It is no longer a second, lossy encoding of the same thing.

**Consequences.** `generate_sets_default_nix` is gone; adding a set now means
editing `nix/sets/default.nix` as well as adding its module, which
`set_index_imports_every_set` fails on if forgotten.
`every_checked_in_module_matches_the_catalog` compares both directions and
skips parenthesised sub-expressions, so a module may carry a derivation without
the test mistaking its tokens for packages.

**Revisit.** If a set ever needs to vary by host architecture or by box, the
module gains a parameter rather than the generator coming back.

## ADR-0053 — a lab daemon must not hold the substrate's stdio

**Status.** Accepted.

**Context.** `lab up` blocked forever on the first `zebra`, with the fabric
unstarted and no output. The daemon was up, healthy, and correctly detached —
`ppid` 1, its own session — and `/proc/<pid>/fd` showed why: file descriptors 0
and 1 were still the pipes it inherited.

FRR's `-d` forks and detaches but deliberately keeps stdout and stderr, so a
daemon that fails a moment after startup can still say so. Under a service
manager that is right. Here the parent is `limactl shell`, and the ssh
transport underneath holds the session open until every descriptor on the far
end is closed. A daemon that runs forever holds one forever.

It read as a slow lab rather than a hung one, which is the expensive part: the
thing being waited on was working the whole time, and none of the obvious
checks — is the process alive, did it error, is the namespace there — points at
the descriptor.

**Decision.** Start each daemon through `sh -c 'exec … </dev/null >/dev/null
2>&1'`, inside the namespace. The redirect belongs on the daemon and not on the
`ip netns exec` around it: that wrapper passes its own descriptors down, so
redirecting the outer command closes the pipe before the process that must not
hold it exists.

**Consequences.** Early daemon output is discarded. That is the trade: it was
never read — the exec's own stdout is what `lab up` inspects, and the daemons
already log to their configured files. Arguments are quoted only when they need
it, so the command in a failure message stays runnable by hand.

**Revisit.** If a lab ever needs a daemon's startup diagnostics, redirect to a
file under the node's run directory rather than to `/dev/null` — the same shape,
one path different.

## ADR-0054 — the lab's DNS answers, and a lab node must not ask glibc

**Status.** Accepted.

**Context.** Every blank node in `ztp-fabric` reached `verifying` and then
failed on `DNS self-check failed`, three attempts each. BGP had converged, the
config was applied, and the name resolved perfectly when asked by hand. Two
independent faults, either of which alone would have produced the same message.

**The lab's DNS refused what it could not answer.** dnsmasq was configured with
`address=/ztp.devbox/10.0.0.0`, which answers A and forwards every other type
— and the server runs `no-resolv` with no upstream, so an AAAA query came back
REFUSED. Every stock resolver asks for A and AAAA together and treats a refusal
on either leg as failure, so a name that had just resolved was reported
missing. `local=/devbox/` alone only downgrades the refusal to NXDOMAIN, which
is still false: the name exists. `host-record` plus `local` gives A for A and
NODATA for AAAA — the true shape of an IPv4-only zone, and the one every
resolver handles.

**And the check asked a resolver that could not see the fabric.** The self-check
used `getent hosts`. A NixOS substrate runs `nsncd`, and glibc hands every name
lookup to it over a unix socket. `nsncd` lives in the root network namespace and
answers from the host's `resolv.conf`, so a node inside a lab namespace was
asking a resolver on the other side of the boundary the lab exists to draw. The
tell was that `tcpdump` inside the node's namespace saw no DNS packet at all —
the query never reached the network. The check now runs `nslookup -type=A`,
which reads the namespace's `resolv.conf` and sends its own query.

**Consequences.** The check tries three resolvers in order: `nslookup` on PATH,
then `busybox nslookup`, then `getent`. That is not defensive padding — a
substrate is whatever the user brought, and the first version of this fix
assumed `nslookup` and broke the ZTP e2e fixture, a Debian slim image with
`busybox` but no `dnsutils`. Both of the first two reach the same applet on the
image devbox builds.

`getent` stays as the last resort even though it is what this ADR exists to
replace, because the two conditions do not overlap: the image that runs `nsncd`
is the image that carries busybox, and a substrate with no DNS client at all is
one where glibc's resolver is not being intercepted. Preferring the reliable
method and keeping the fallback is more honest than failing a node's
provisioning over a missing diagnostic tool.

`-type=A` is explicit rather than incidental: the lab addresses IPv4 only, and
a bare lookup would reintroduce the dual-query failure from the other
direction.

**Revisit.** If a lab ever addresses IPv6, the DNS records and this check change
together — a `host-record` with both families, and a self-check that asks for
both. They are one decision and should move as one.

## ADR-0055 — every FRR daemon, and then `vtysh` to distribute the config

**Status.** Accepted.

**Context.** `ztpd` reported three healthy nodes and the fabric failed its
reachability matrix: `leaf1 -> leaf2` never came up. BGP was Established on
every session, each leaf was advertising a prefix, and each leaf's own
`show bgp` had its loopback in the table — unmarked, not best, not installed.

`ip addr` explained it. A ZTP-provisioned leaf had `127.0.0.1` on `lo` and
nothing else, while its config plainly said `interface lo / ip address
10.0.128.1/32`. Asking zebra for its running configuration returned no
interface stanzas at all, and `zebra -C` on the same file said why, twice per
line: *No such command on config line 8: interface lo*.

FRR 10 moved interface configuration out of zebra and into `mgmtd`, the
northbound datastore daemon. `lab up` started `zebra` and `bgpd`. Neither owns
`interface`, so every address in every generated config was parsed, rejected,
logged, and skipped — and the daemon carried on and reported success.

Routed labs never noticed, because `wiring` assigns those addresses out of band
with `ip addr add` before FRR starts. A ZTP node has no such step by design:
the config it is handed is the only thing that configures it. So the fault was
invisible everywhere except the one scenario the feature exists for, and there
it presented as the most expensive shape available — every per-node check
passing, and only the fabric-wide assertion failing.

**Decision.** Start `mgmtd` first, then `zebra`, then `bgpd`; then apply the
file with `vtysh -N <ns> -f <conf>`.

The `vtysh` pass is not redundancy. A daemon's own `-f` keeps the commands it
owns and silently drops the rest, so no single daemon reading the file ever
applies all of it — that is what an *integrated* `frr.conf` means, and `vtysh`
is the client that holds a session to every daemon and hands each line to
whichever owns it. The per-daemon `-f` stays as well, so a daemon restarted by
hand comes back with its own share.

`mgmtd` takes no `-f`, which reads as an oversight and is not one: it is the
same statement from the other side.

**Consequences.** `mgmtd` joins the `frr-daemons` derivation, and `doctor`
reports it missing — but only where its absence is a fault. It arrived in FRR 9
and took interface configuration in 10; before that zebra owned it, and Debian
bookworm still ships 8.4. Requiring it everywhere turned a working substrate
into a lab that failed on its first daemon, which the routed e2e test caught on
the run after this was written. So the start is guarded by `command -v` on the
substrate, where the answer lives, and the preflight asks zebra its version
before deciding whether to care.

The guard is an `if`, not a `&&`: `command -v x && exec x` exits 1 when the
test fails, and `lab up` reads a nonzero exit as the daemon having failed to
start — the absent-and-fine case would have looked exactly like the broken one.

`every_daemon_starts_before_the_config_is_distributed` pins the order, the
absence of `-f` on mgmtd, and the guard.

**Revisit.** If FRR moves more configuration into the northbound, this needs no
change — `vtysh` already distributes whatever the file contains. What would
need revisiting is the per-daemon `-f`, which becomes dead weight once nothing
is left that only one daemon owns.

## ADR-0056 — the lab leaves devbox

**Status.** Accepted (2026-09-05).

**Context.** v4 shipped two products in one binary: a glass-box sandbox for
AI coding agents and a multi-node network lab with a ZTP fabric. The
September 2026 review found they share no user: the lab existed to exercise a
network-automation skill set (v4 design §19), and it took one of three
top-level console tabs, the README screenshot, and about a fifth of the code.
The first plan was to split it into a maintained product that depends on
devbox as a library. Ethan then decided the same day that any future lab
would be rebuilt separately on containers, so there is nothing to maintain.

**Decision.** Remove `src/lab`, `ztpd`, `labkit`, the Labs console views and
templates, the lab e2e tests, the lab CI lanes, and the lab documentation.
The code is archived with its full history in a local repository
(`devbox-lab`, no remote, no further work). Three things stay because they
are not lab-specific: the `network` Nix set (tools), the private-prefix
contract that `isolated` honours — renamed from `/etc/devbox/lab/*/prefixes`
to `/etc/devbox/prefixes/*` and from `is_lab_internal` to
`is_private_range`, behaviour unchanged — and `install_embedded_binary`,
which becomes `pub` as the stable entry for installing any guest binary.

**Consequences.** 87 files, about 16,000 lines gone; the unit test count
drops by 137. `GET /labs` is a 404 and a regression test pins it.
`docs/screenshot-console.png` still shows a ZTP topology and must be replaced
by a run report (wave 2). §9–§10 of the v4 design are history, not
description.

## ADR-0057 — CO-RE objects are committed per architecture, and a source build embeds the eBPF agent

**Status.** Accepted (2026-09-05).

**Context.** `build.rs` cross-compiled the Go agent without the `ebpf` tag,
because the bpf2go objects were generated only in release CI, and no v4
release was ever cut. Every source build therefore embedded the portable
proc+packet agent, and on a Lima box that meant connections with pid
4294967295, no TLS or file events, and zero byte counts — while `doctor` said
only `agent: devbox-obsd 0.1.3`. The product's central claim was silently
false on the only build path anyone had used.

**Decision.** Commit `agent/bpf/devbox_<arch>_bpfel.{go,o}` per guest
architecture, as the cilium/ebpf ecosystem does. `build.rs` builds the agent
with `-tags ebpf` when the object for the target architecture exists and
prints a `cargo:warning` naming the fallback when it does not. The arm64
object was generated inside a NixOS guest with BTF; the amd64 object comes
from the CI `ebpf` job as an artifact. CI compares the committed `.go`
bindings byte-for-byte and fails on drift; it does not compare `.o` bytes,
which vary with the clang and BTF on the runner. The agent's handshake now
carries `source`, and `doctor` and the console print which capture sources
are live.

**Consequences.** Anyone who changes `devbox.bpf.c` or the record layout in
`agent/decode/record.go` must regenerate the objects; CI cannot catch a
stale `.o` whose bindings did not change, so this rule is documented rather
than enforced. Two defects that eBPF does not fix surfaced in the same
verification and got their own work items: the SNI parser drops a
ClientHello that spans two TCP segments (W0-5a), and no probe fills the byte
counters (W0-5b). An existing box does not receive a new agent until
provisioning runs again (W0-5c).

## ADR-0058 — merging main: a login shell that checks for root, and a cache key that knows what v4 provisions

**Status.** Accepted (2026-09-05).

**Context.** `v4` branched from a March commit; `origin/main` gained 70
commits after it: Incus cached-image networking, VM user and home detection,
a `run_as_root` abstraction, image caching, and `devbox code` fixes. Eight
files conflicted, the largest being `provision.rs`. Two of main's changes
cut across v4's design: main dropped the literal `sudo` because Incus with
NixOS 25.11 cannot pass PAM, and main's image cache snapshots a provisioned
VM — before v4 taught provisioning to push set modules wholesale, install
the agent, and apply policy.

**Decision.** Keep main's `run_as_root` and add `policy::enforce::
elevated_login`, which decides in the guest whether it is already root and
otherwise elevates through a login shell; `rebuild_argv` uses it. A rebuild
that exits 255 waits for the guest to answer `exec` again and reruns once
before judging. The image cache key includes the package list and the obsd
module hash; post-cache setup redoes v4's steps (state file with packages,
agent install, tool configuration); bare boxes neither consult nor publish
the cache, and only a successful provision publishes. `attach` runs as the
detected guest user. Version becomes 0.1.6.

**Consequences.** The Incus paths that main fixed could not be exercised on
this host and are marked unverified until CI or an Incus machine runs them.
The browser terminal still starts as root on Incus (`interactive_argv` has no
`--user`), a pre-existing gap recorded as its own work item.

## ADR-0059 — a run owns its events by cgroup, then by process tree, then by time

**Status.** Accepted (2026-09-05).

**Context.** A run report is only worth reading if "these events belong to this
run" is a claim someone can check. The v5 design named three rules but left
their precedence and their failure modes open, and a first implementation
showed why that matters: `bpf_get_current_cgroup_id()` returns the cgroup
directory's inode — verified on a real box, `stat -c %i` and the agent both
reported `17557` for the same scope, and the caller's shell stayed at `5632` —
so cgroup identity is exact. But the cgroup does not exist until the run's
wrapper enters it, and the host does not learn its id until the wrapper
publishes it. In the first live run, `curl`'s `connect` was in the report and
its `exec` was not: 45 events, one `exec`, a process tree of two.

**Decision.** Attribute in a fixed order: **cgroup** (`pid != u32::MAX` and a
non-zero `cgroup_id` equal to an active run's), then **pidtree** (pid or ppid in
the run's learned descendant set), then **window** — and window *only* for the
`u32::MAX` sentinel the packet tap uses, and only when exactly one run's window
contains the timestamp. A real pid belonging to no run's tree returns `None`
rather than falling through to window; two overlapping windows refuse to
choose. A wrapper that could not obtain an exclusive cgroup publishes `0`, never
a shared id. To close the startup race, `Store::backfill_run` runs once when the
host learns the cgroup id and claims only rows where `cgroup_id` is exactly
equal — the kernel's equality, not a widened one.

**Consequences.** The same run went from 2 processes and 45 events to 6 and 256.
Every report states its attribution counts per rule, so the reader can see how
much rested on the weakest one. `exec` and `shell` are recorded as runs but have
no wrapper — `exec` must capture its output, `shell`'s attach path takes no
interactive flag — so only window can reach them, and an open `devbox shell`
overlapping a `devbox run` makes window refuse for both. That cost shows up as
`unattributed in the window` rather than as a wrong answer.

**Revisit.** If `Runtime` ever grows an interactive `exec_as_user`, `exec` and
`shell` can take the wrapper and drop out of the window contest entirely. If
window competition proves noisy before then, give `ActiveRun` a kind and let
step 3 skip `shell` runs.

## ADR-0060 — a checkpoint is a copy of the upper layer, and restoring one remounts

**Status.** Accepted (2026-09-05).

**Context.** A run report's Files section has to mean "what this run changed",
not "what this box has ever changed". The obvious implementation — a new overlay
layer per run — runs into a hard kernel limit on stacked layers and would make a
long-lived box unmountable. The alternative is copying the upper directory, and
the question was whether that is affordable and faithful. Measured on a real
box: the live upper was 12 KB across 3 entries; `cp -a --reflink=auto` moved
102 MB / 2000 files in 0.164 s (~620 MB/s), and 17 consecutive checkpoints took
3 seconds. `cp -a` preserved mtime to the nanosecond, character-device whiteout
nodes, and `trusted.overlay.opaque`. `--reflink=auto` is the correct spelling:
`/var/devbox` is ext4, which has no reflink, and `--reflink=always` fails
outright there while `auto` falls back silently and still wins on btrfs or XFS.

**Decision.** `sandbox::checkpoint` copies `/var/devbox/overlay/upper` to
`/var/devbox/checkpoints/<id>/upper` with a JSON manifest beside it. Diffing two
checkpoints compares size **and** mtime, not size alone — a same-length rewrite
is a real change and mtime is the only field that sees it. Restoring clears the
upper the way `discard` does and then calls `overlay::refresh()`.

**Consequences.** Copying without the remount leaves stale reads: on a live box,
after clearing an entry straight out of the upper, `ls` updated immediately
while `cat` went on returning the old content until the overlay was remounted.
Kernel 6.19 refuses `mount -o remount` on an overlay
(`fsconfig() failed: overlay: No changes allowed in reconfigure`), so refresh
falls back to umount + mount; a failure there warns rather than failing the
restore, since the upper is already correct and `devbox diff` reads it directly.
The same missing remount was then found in `devbox layer discard` and fixed
inside `overlay::discard`, so both callers get it. Checkpoints keep the newest
20; ones a run's report cites are never pruned and `checkpoint-rm` refuses them
without `--force`. `layer restore` is `<ID> [NAME]`, not the design's
`[NAME] <ID>` — clap cannot put a required positional after an optional one, and
`snapshot restore` already sets the precedent.

**Revisit.** If a checkpoint ever needs to survive the box, this becomes a host
copy and the cost model changes; measure again before assuming 0.16 s.

## ADR-0061 — the broker is a per-service reverse proxy, so `gh` is out

**Status.** Accepted (2026-09-05).

**Context.** Getting a credential to an agent inside a box without putting the
credential inside the box means terminating the request on the host. Two shapes
were available: a forward proxy with TLS interception (a CA in the box, every
client trusting it), or a per-service reverse proxy the client is pointed at.
The first reaches every client and costs a trust anchor in a sandbox whose whole
point is that you do not trust what runs in it.

**Decision.** A per-service reverse proxy. The box speaks plain HTTP to the
broker; the broker opens TLS to the real upstream and injects the credential
there. No CA, no MITM, no CONNECT tunnelling. Clients are pointed at it through
their own configuration: `ANTHROPIC_BASE_URL` (which Claude Code honours
*including the path prefix* — verified against the real CLI with a dummy token,
correcting a documentation answer that said otherwise), `OPENAI_BASE_URL`, and
for git a `url.<broker>.insteadOf` entry in the guest gitconfig.

**Consequences.** `gh` cannot be brokered. It forces HTTPS
(`tls: first record does not look like a TLS handshake`) and `GH_HOST` rejects a
scheme, treating `http://127.0.0.1:18082` as a hostname. Rather than half-break
it, `session_env` sets no `GH_HOST` at all and offers only an informational
`DEVBOX_BROKER_GITHUB_URL`; the `/api/v3/*` and `/api/graphql` routes are kept so
a future TLS listener would not need a rewrite. Claude Code's unauthenticated
`HEAD <base>/api/hello` probe is answered locally, before auth, so it does not
become a 401 and an audit row. Response bodies stream
(`reqwest::Body::wrap_stream` in, `Body::from_stream` out) because a buffered
proxy breaks SSE; `content-type` survives header stripping for the same reason.

**Revisit.** Supporting `gh` means a TLS listener and a trust anchor in the box.
That reverses this decision's premise and should be its own ADR, not a patch.

## ADR-0062 — the secret lives in the OS keychain; the box gets a token that rotates

**Status.** Accepted (2026-09-05).

**Context.** v4 copied `~/.claude/.credentials.json`, `~/.codex/auth.json`, and a
plaintext `~/.devbox-ai-env` sourced by `.zshrc` into every box it provisioned —
while the README promised credentials stayed on the host. The promise was the
thing that was false.

**Decision.** Values go to the OS keychain (macOS
`security add-generic-password -s devbox -a <provider>`; Linux
`~/.devbox/secrets/<provider>` at 0600, `secret-tool` where available), never to
`state.json`, `devbox.toml`, or a log. The box receives a **per-box token**, 32
random bytes, rotated at box start, delivered only in the argv of sessions
devbox itself starts (`env -- K=V … cmd`, which is uniform across all three
runtimes because `DevboxConfig.env` is honoured only by Docker). Holding the
token grants brokered, scoped, logged access and nothing else. Every brokered
request writes a `credential` event when the response body finishes streaming,
so its byte count is real.

**Consequences.** The four copy sites are deleted, the four settings files that
remain get a content check that skips anything shaped like a credential, and
provisioning purges the v4 leftovers from existing boxes. `[credential]` sections
are stripped from the pushed gitconfig, since a host helper is either meaningless
or a path to a host secret. Lima's user-mode network NATs the guest to the host's
loopback, so the broker binds `127.0.0.1` only and the token is the *sole*
identity — the broker cannot tell boxes apart by source address. The token is
briefly visible in the host's `ps` output as part of `limactl shell`'s command
line; the alternative is writing it to guest disk, which is worse. Incus and
Docker reachability is implemented per the design but unverified on this host.

**Revisit.** If per-box source addresses ever become distinguishable, the token
can become a second factor rather than the only one.

## ADR-0063 — export writes its own JSON, and a profile must be declared

**Status.** Accepted (2026-09-05).

**Context.** OCSF and OTLP both have generated SDKs. Taking either would add a
large dependency and a version treadmill to a binary whose entire event model is
eleven small structs. The risk of hand-writing is getting the schema wrong
quietly, so the question was whether the output could be *checked* instead.

**Decision.** Hand-write both encoders and validate against the authorities:
every OCSF class against `POST https://schema.ocsf.io/1.3.0/api/v2/validate`,
and the OTLP payload by POSTing it to a real OpenTelemetry Collector. Required
fields come from `?profiles=` (the core set), not from the default view that
folds every profile in. Where OCSF requires a field devbox has not observed, say
so rather than invent: `tls.version = "Unknown"` because the agent reads only
the ClientHello, `http_response.code = 0` for "no status", `file.type_id = 0` for
a path the probe merely saw opened.

**Consequences.** The validator caught a real error the tests could not:
`actor` and `device` on Network, DNS, HTTP and Detection Finding come from
OCSF's `host` **profile**, and a record must declare
`metadata.profiles: ["host"]` or they are rejected as unknown attributes. API
Activity 6003 has no `device` at all, so `base()` skips it there. Ten golden
records and eight `type_uid`s drawn from a real 442k-event store validate with
zero errors and zero warnings; 147,998 records (109 MB) were accepted by
otelcol-contrib 0.160.0 in one POST. An event kind with no honest class —
`syscall` today — is counted as unmapped and skipped, and the invariant
`matched == written + unmapped` fails the export rather than printing a partial
one. Not covered by real data: HTTP Activity 4002, Detection Finding 2004 and
the unmapped branch, which have golden coverage only.

**Revisit.** If a third format arrives, or if OCSF 2.x reshapes the objects,
reconsider a generated SDK — but keep the validator step either way, since it
is what found the profile bug.

## ADR-0064 — an MCP server's posture belongs to its box, and the registry can be global

**Status.** Accepted (2026-09-05).

**Context.** An MCP server is third-party code the agent launches on the host
with the user's full rights. Moving it into a box raises two scoping questions:
what egress it gets, and where its registration lives.

**Decision.** Posture is **per box**, applied for the duration of the run and
only when it differs from what the box already has; on exit it is restored via
`enforce::apply_saved`, following ADR-0047's rule that only a switch that
actually happened is rolled back. A failure to apply refuses to start the server
rather than silently falling back to the box's posture. Registration is written
into the project's `devbox.toml` as **text surgery** — appending or excising
exactly one `[mcp.<name>]` block, then re-parsing and asserting nothing else
changed — because `DevboxConfig::save` regenerates the document from parsed
values and destroys the comments `devbox init` itself wrote. A `--global` flag
writes `~/.devbox/mcp.toml` instead, and lookup falls back to it, so a server
registered once is reachable from any directory.

**Consequences.** Verified on a real box: during the run the guest carried a
default-drop `mirror-only` ruleset; after it, no devbox table and the box back at
`open`. `devbox mcp rm` leaves `devbox.toml` byte-identical to the original,
comments and trailing comments included. A registration written in inline form
(`mcp.inline = { … }`) is readable but `rm` refuses it rather than rewriting the
whole file. Because the posture is box-wide for the duration, two MCP servers
that want different postures need different boxes; `mcp add` warns when a
posture is attached to a box that is a project box rather than a dedicated one.

**Revisit.** Per-server egress inside one box would need per-cgroup nftables
rules. That is a real design, not a tweak, and should wait for a user who needs
it.

## ADR-0065 — bytes are settled at `tcp_close`, and a new event kind does not bump the protocol

**Status.** Accepted (2026-09-05).

**Context.** `fill_net()` wrote zeros into every connection's byte counters, and
the console, the behaviour summary and the run report all faithfully displayed
`↑0B ↓0B`. That was not laziness: every existing probe
(`tcp_v4_connect`, `tcp_v6_connect`, `tcp_finish_connect`, `inet_csk_accept`)
fires while the connection is being *established*, when nothing has crossed it.

**Decision.** Add `kprobe/tcp_close`, read `tcp_sock->bytes_sent` and
`bytes_received` (both guarded by `bpf_core_field_exists`), and emit a new
`close` event carrying them, the duration, and the direction. Pair the close
with its open **inside the kernel**, keyed by the socket pointer in an LRU map —
not by five-tuple in user space, because `tcp_close` runs in whichever process
closed the fd, which after a fork or an `SCM_RIGHTS` pass is not the process that
dialled. Every consumer counts bytes from `close` and only from `close`.
`ProtocolVersion` is **not** bumped: an old agent simply never sends `close`, and
an old collector counts it as rejected without dropping the connection, which is
the same treatment the `Hello.Source` field already received.

**Decisive evidence.** Traffic on a real box went from `↑0B ↓0B` to
`↑1.9KB ↓6.3KB`. Live testing also found a genuine bug: an accepted connection
whose handshake completed before the fd was closed had already been unhashed
(`inet_num = 0`), so its close reported source port 0 and split the flow into two
rows — fixed by remembering the published port in the pairing map. The record
layout did not grow: the existing `__u8 _pad` became `__u8 flags`, and
`NetRecordSize` is still 112.

**Consequences.** UDP is not settled — its events come from the packet tap, not a
probe — so "traffic covers all traffic" is still false and is documented as
such. A connection still open reads 0, which is the honest reading of the model.
A close whose open was never seen is marked `orphan`: the bytes are real but the
process on the record closed the socket rather than opening it, so the record
says so instead of adding them to that process's total. Bumping the protocol
would have cut off every agent not upgraded in the same instant, for an addition
no old reader needs to understand.

**Revisit.** If a future change alters an existing record's layout rather than
adding a kind, bump the version — that is the case the field exists for.

## ADR-0066 — file events are filtered in the agent, and the scope rides in the handshake

**Status.** Accepted (2026-09-05).

**Context.** With eBPF attached, the `openat` probe reports every path the box
opens. On a real box that was 1,446 file events in sixty seconds and 651,564 in
an eight-hour session — `/nix/store`, `/etc`, journald — against a design that
says "under the workspace". The store grew 43.6 MiB/hour and `behavior summary`
hit its 50,000-row scan limit inside the first eight minutes of an eight-hour
window.

**Decision.** Filter in the **agent**, at the Source boundary, by absolute path
prefix, before the event reaches the pending queue — so a dropped-by-scope event
is not a dropped event. The default scope is `/workspace` plus the box user's
home. The host passes it (`-file-scope`) on every path that starts an agent —
the collector's stdio agent, the Ubuntu systemd unit, and the NixOS module — so
two agents on one box cannot disagree about what a file event is. The chosen
scope is carried in the agent's `Hello`, stored in the box's capture health, and
printed by `devbox doctor` and the console's capture bar.

**Consequences.** Measured: 1,446 file events in that same minute became 90, and
`exec` counts were identical (50 = 50), so the two agents saw the same syscalls
and differed only in the filter. An empty scope captures everything and warns
once; a relative prefix is rejected at startup rather than ignored, because
ignoring it would empty the list and let the flood back in. **Relative paths are
dropped**: `handle_openat` records `openat`'s pathname but not its `dirfd`, so
user space cannot resolve one — 16.5% of file events in a sample. Fixing that
means adding a field to the BPF record and regenerating the objects. Filtering
in the agent rather than in the summary means the events are gone, not hidden;
that is the point, and it is why the scope is stated everywhere the data is.

**Revisit.** Add `dfd` to the file record and resolve relative paths in user
space; then the scope becomes complete rather than best-effort.

## ADR-0067 — an agent is replaced by content hash, not by version number

**Status.** Accepted (2026-09-05).

**Context.** The embedded agent was reinstalled only at provisioning time, and
the only staleness test anywhere was a version string. Both halves failed
together on a real box: the agent inside reported `devbox-obsd 0.1.6 (portable)`
and the new host binary was also `0.1.6`, so `evaluate_hello` waved it through
and the box kept capturing `proc+packet+netfilter (degraded: no process
attribution, no file events)` indefinitely. The same version equality let an
older collector daemon of the same version keep ownership for a whole login
session — and the daemon is what spawns each box's stdio agent, so it went on
passing `-no-ebpf` to an agent that no longer needed it.

**Decision.** Compare **sha256 of the bytes**. `agent_sync::host_digest()` hashes
the embedded agent once per process; the guest's digest is computed fresh on
every probe (`sha256sum` → `shasum -a 256` → `openssl dgst -sha256 -r`, with the
result validated as 64 hex characters on the host rather than trusted). Mismatch
means replace, at box start/entry (full, including the systemd unit) and at
collector attach (binary only, under a non-blocking per-box claim and a 180 s
timeout). The collector daemon's identity sidecar records version, commit **and**
the sha256 of the host binary, and a differing build takes over.

**Decisive evidence.** Two builds made minutes apart carried the identical commit
tag `11cc51fb0ab1-dirty` and differed only in sha256. `doctor` said the same
sentence about both on its `agent:` line; only `agent binary:` could separate
them.

**Consequences.** `doctor` gains `agent binary: matches host embed / stale (sha …
vs …) / missing / unverifiable`. Rewriting a NixOS unit costs one
`nixos-rebuild switch` (~6 s on a warm store), which happens only when the unit
is genuinely stale and is idempotent afterwards — and it must restore the egress
posture, because a rebuild tears down devbox's nftables table; the source-level
guard in `tests/policy_lifecycle.rs` caught that omission in the first
implementation. No per-box digest cache was added: the host digest belongs in a
`OnceLock`, the guest digest must be live to be true, and one merged probe answers
both the digest and the unit question in a single exec. Docker is reasoned
through but not exercised; Incus is unverified.

**Revisit.** If the probe's cost ever shows up in `exec` latency, cache the
*unit* answer only — never the digest.

## ADR-0068 — the guest's boot mounts are named by label, and a box is repaired before it is stopped

**Status.** Accepted (2026-09-06).

**Context.** Every NixOS box devbox has created on Lima since v3 (`dfba8cc`,
2026-03-07) has been unable to boot a second time. `nixos-generate-config`
records whatever is mounted at provisioning time into
`hardware-configuration.nix` **by UUID**, and Lima's `cidata` volume is an
iso9660 image whose UUID *is* its creation timestamp — regenerated on every
`limactl start`. Measured on one box across a restart:
`2026-09-05-00-33-2246` → `2026-09-05-22-01-3862`. Lima itself is immune
because it names the volume by label and rewrites `/etc/fstab` at every boot;
`nixos-rebuild switch` removes both protections at once by turning `/etc/fstab`
into a read-only store symlink. The result, read out of the stranded box's own
journal: `Timed out waiting for device /dev/disk/by-uuid/…` → `Dependency
failed for /mnt/lima-cidata` → `local-fs.target` fails → emergency mode, which
has no sshd. A control box built from the same image without devbox stopped and
started cleanly.

**Decision.** Generated `configuration.nix` overrides the mount with
`lib.mkForce { device = "/dev/disk/by-label/cidata"; options = [ "ro" "nofail"
"x-systemd.device-timeout=5s" ]; }`. A box that is still running repairs itself:
the guest probe reads the `/mnt/lima-cidata` line out of `/etc/fstab`, a device
under `/dev/disk/by-uuid/` means the box is one stop away from being lost, and
the repair rides the same `nixos-rebuild` as any other drift (about nine
seconds). Because a stopped box cannot be reached at all, `devbox stop` performs
that repair first and **refuses to stop** if it fails — it lets the stop through
for a box that is not running, for one whose probe cannot answer, and for
`--force`, each of which would otherwise make an already-unreachable box
impossible to stop.

**Consequences.** A box that is already stopped and stranded cannot be
recovered by any means we would ask a user to perform: no sshd, and the one
line to change lives in the guest's ext4. `devbox destroy` and recreate;
uncommitted overlay changes are lost. Refusing a stop is a real cost — the user
asked for something and did not get it — but it is the only step in this bug
that cannot be undone. One attempt to generalise `nofail` went badly
wrong and is worth recording: applying it to `/workspace`
broke every subsequent repair, because overlayfs rejects a remount that changes
options (`overlay: No changes allowed in reconfigure`) and
`switch-to-configuration` reloads a mount unit whose options moved, so
`nixos-rebuild switch` exited 4 and the generation rolled back — and it
self-locked, since `/etc` had already been switched. That was reverted before
release; no published version carries it. The original motivation stands
unaddressed: an overlay that cannot mount still takes the box to emergency mode.

**Revisit.** If `/workspace` is ever to gain `nofail`, it has to be set at
provisioning time, before the mount exists, never as a change to a box that
already has it.

## ADR-0069 — the guest's passwd home follows its login shell

**Status.** Accepted (2026-09-06).

**Context.** On Lima the guest account has two homes. Lima's cloud-init creates
the user with `homedir: /home/<user>.guest` — the suffix avoids a collision with
the host home it mounts — and puts `authorized_keys` there. Devbox then created
`/home/<user>` itself and, through `users.users.<name> = { isNormalUser = true;
… }` with no `home` set, let nixpkgs default the passwd entry to it. sshd
resolves keys from passwd, so it looked in the empty directory: any ssh
connection that did not ride Lima's already-authenticated master was refused.
That is exactly the connection `devbox code` needs in order to pass environment,
since ssh does not carry `SetEnv` over a shared connection. Measured: the
identical probe command returned `exit=0` on a repaired box and
`exit=255 Permission denied` on an unrepaired one. Separately,
`detect_vm_username` scanned `/etc/passwd` for the first account with a uid in
1000..65534 — and Lima gives the guest user the *host's* uid, 501 on macOS, so
the scan matched nobody on every Lima box and fell back to the host's `$USER`.
It was right only by the coincidence of the two names matching.

**Decision.** Ask the box who its user is rather than scanning for a plausible
one, and take the home from what the login shell reports rather than from
passwd. The NixOS module declares the home explicitly, so the passwd entry
follows the account Lima actually made. Boxes that already have it wrong repair
themselves on the next lifecycle command, sharing one `nixos-rebuild` with the
other drift repairs.

**Consequences.** Verified end to end on a box provisioned from scratch: `id
-un`, `$HOME` and `getent passwd` agree, `/home/<user>` is no longer created at
all, gitconfig and the AI tool settings land where the shell will read them, and
a direct ssh authenticates. Every write path that stamps the state file had to
carry the home with it — the Sets rebuild path kept only `[user].name`, so one
unrelated `devbox sets apply` would have undone the repair on the next rebuild.
That class of bug has now been hit three times (`name`, `mount_mode`, `home`,
and again with `runtime`); the state file is written through one
read-modify-write helper for that reason.

**Revisit.** If a runtime ever reports a user devbox cannot enter, the probe
should say so rather than fall back to the host's `$USER` — the fallback is the
part that made this silent for so long.

## ADR-0070 — the read loop is the only thing that ends a collector connection

**Status.** Accepted (2026-09-06).

**Context.** One test in the suite failed about one run in ten with
`received: 597, expected: 600`. Three explanations were possible; two were
eliminated by experiment rather than by argument. A `tokio::io::duplex` probe
delivered all 600,000 buffered bytes after the write half was dropped, ruling
out lost buffering; `received` increments immediately after a successful
`read_frame` and before every `continue`, ruling out an uncounted read. The
cause was `serve_agent` running the read loop and the keepalive writer in one
`tokio::select!`, which **cancels the branch that did not finish**. The first
keepalive tick lands at 5 s and the test ran 5.44–5.54 s, so on a loaded machine
the tick fell after the agent closed and before the read loop had drained: the
write failed, the select cancelled the read, and frames the agent had already
delivered were discarded.

**Decision.** A failed keepalive means the *write* half is gone, which is no
reason to stop reading what the agent already wrote. `keepalive` logs one debug
line and then never resolves, so the read loop is the only branch that can end
the connection. `received` thereby means what it always claimed: frames that
arrived, regardless of queueing or of the write half's health.

**Consequences.** Proved deterministically instead of by waiting for a flake:
a regression test with `start_paused = true` pins keepalive failure at 5 s and
three frames arriving at 6 s. Against the old code `received` is **0**, not 597
— the cancellation discards everything still buffered, and the intermittent 597
was only the fraction that happened to be read first. Against the new code it is
3. The first version of that test used a `Cursor` reader and passed against the
old code too, because `Cursor` hits EOF before the keepalive can fire; a
regression test that does not fail against the unfixed code proves nothing, and
checking that is not an optional step. Five consecutive full runs are green.
`tokio`'s `test-util` feature became a dev-dependency, since `full` does not
carry `start_paused`. The general shape is worth remembering: in a `select!`,
the branch that represents the data stream must be the only one allowed to end
the loop.

**Revisit.** If a future branch legitimately needs to end a connection, it
should do so by closing the reader, not by winning a `select!`.

## ADR-0071 — a handover, and an agent push, wait for the run they would interrupt

**Status.** Accepted (2026-09-06).

**Context.** A run came back with zero events about one time in ten, and only
under load. The first suspect — a new agent binary being pushed mid-run — was
wrong: the agent bytes were identical across a reproduction. The cause was the
collector **handover** itself. A box's events all travel over the stdio agent,
which is a *child process* of the collector daemon; replacing the daemon killed
that child, and the new daemon started another. Measured: agent pid
`127130 → 131453`, the box's `capture.json` `since` jumping from
`05:12:13.731Z` to `05:13:08.049Z`, roughly two seconds with no capture — and
not one line in `collector.log`, because the log level was `warn` and every
handover message was `info`. Reproduction rate: 0 in 50 quiet runs, 2 in 20 with
another build issuing commands. `agent_sync` had the same hole and a longer one,
since its restart waits for systemd and, on NixOS, for a rebuild.

**Decision.** Both defer. A handover is host-wide (it ends every box's stdio
agent at once), so it asks whether *any* box has a run in flight; an agent push
affects one box, so it asks about that box only. "In flight" requires
`status='running'` **and** `ended_at IS NULL`, written by one statement, because
a half-written row read as a live run would defer that box's updates forever. A
deferred agent push writes `~/.devbox/boxes/<name>/agent-update-pending`
recording the digest it wanted and the time of the **first** deferral, and
`devbox run` finishes the job on its way out. Deferral is all-or-nothing: pushing
bytes now and restarting later would leave a box reporting an agent it is not
running.

**Consequences.** The remaining gap is stated rather than closed: interruption
rate went 2/20 → 1/20 under load, and the last case is a handover landing
*between* two runs, which needs the two daemons to overlap rather than the run
to wait. What made this cost two rounds to find was the silence, so a handover
now leaves a note beside the lock that both daemons read and log — identified by
build, never by pid, since the pid in that note belongs to the CLI that made the
decision. The daemons' own default log level moved to `info` for the same
reason; without that the two new lines would have been discarded exactly as
their predecessors were. A run whose capture really was interrupted now says so
in its report, and a run whose capture was merely re-published does not: both
conditions — an event already attributed, and a changed agent pid — must hold
before it is called an interruption.

**Revisit.** Overlapping the two daemons across a handover would close the last
case; it changes the ownership protocol, so it belongs in its own decision.

## ADR-0072 — `/workspace` gets `nofail` at birth, or never

**Status.** Accepted (2026-09-06).

**Context.** An overlay that cannot assemble fails `workspace.mount`, which
fails `local-fs.target`, which drops the box into emergency mode with no sshd —
the same ending as ADR-0068's cidata mount. `nofail` is the fix, and it was
applied unconditionally in 0.2.1's development line. That had to be reverted,
because overlayfs rejects **any** remount that changes options
(`overlay: No changes allowed in reconfigure`) and `switch-to-configuration`
reloads a mount unit whose options moved: `nixos-rebuild switch` exits 4, the
generation rolls back, and — because `/etc` has already been switched — every
later rebuild sees the same difference and fails the same way. A box that
received the grant could no longer accept any devbox repair at all. The
mechanism has now been hit three separate times, the third while deliberately
testing that an old box does *not* get the grant.

**Decision.** The option set is decided once, at provisioning, when
`/workspace` does not yet exist and therefore no unit can be reloaded and no
overlay can refuse. The answer is written to `devbox-state.toml` as
`workspace_nofail` and **never recomputed**: no state file means the box is
being born, so grant; a state file that says `true` keeps it; a state file
without the key means the box was born without it, so never. That last case is
the one a naive "new boxes get it" rule gets wrong, because `reprovision` runs
the same code.

Granting requires **two independent facts and a probe that says it finished**.
A single probe line ends in `probe=ok`; without that marker the answer is no,
because `limactl shell` failing to connect exits non-zero rather than erroring,
and "the box has no state file" and "ssh hiccuped" were previously
indistinguishable — a narrow window with an irreversible outcome. Beyond the
absent state file, the box must also demonstrably have no `/workspace`: no line
for it in `/etc/fstab`, and nothing mounted there.

**Consequences.** The consistency check compares the record against
`/etc/fstab`, not against the kernel's mount options — `nofail` is an
fstab/systemd option and never reaches the kernel. Comparing against
`findmnt -no OPTIONS /workspace` was written first and was wrong in exactly the
way that matters: on a freshly built box the state key says `true`, fstab
carries `nofail`, and the mount options do not, so every new box would have been
judged inconsistent and refused every later rebuild — a check against lock-up
becoming the way to lock up. Unit tests did not catch it because the test and
the code shared the assumption. `/etc/fstab` is also what
`switch-to-configuration` itself compares. A state file that disagrees with
fstab now stops the rebuild with an explanation rather than attempting it. The
key is a record of what the mount already is; editing it by hand is a way to
lock a box.

**Revisit.** Only if overlayfs ever accepts an option-changing remount. Until
then, an existing box's mount options are not a thing devbox may change.

## ADR-0073 — the repair happens before the run is recorded

**Status.** Accepted (2026-09-06).

**Context.** 0.2.1 told users to run any command that enters a box to pick up
its guest-side repairs, and named `devbox exec <box> -- true`. That was false
from the day it was written, and false for the two commands most likely to be
used. Two changes landed the same day in 0.2.0/0.2.1: `exec` and `shell` began
recording themselves as runs (`d8c7b83`), and a pending repair began deferring
while a run is in flight (`9faf26a`, ADR-0071). Together, each of those commands
opened a run and then deferred to it — waiting for itself. Every repair shares
one gate, so this covered all of them: agent version, the NixOS module, the
cidata mount, `AcceptEnv`, the unit, and the passwd home. Only `devbox run`
(which finishes the job after its own run ends) and commands that open no run —
`devbox code`, `reprovision`, the pre-check in `stop` — ever applied one.

**Decision.** Reorder rather than relax. `exec` and `shell` now prepare the box
first — which is where the repair happens — and record the run afterwards. The
alternative, teaching the deferral to ignore the caller's own run, would let a
repair run *during* the user's command, and every repair ends by restarting the
agent: that is precisely the capture gap of ADR-0071, manufactured by devbox
inside the window it was supposed to be recording.

**Consequences.** The ordering is a type, not a convention.
`prepare_running_for_use` returns a `Prepared`, and `SimpleRun::start` requires
one; it never reads it. Writing the old order no longer compiles. Both
regression tests fail against the previous code with the message
`recorded 1 run(s) before preparing the box`. Measured on a real box: with the
old binary the pending marker was byte-identical before and after
`devbox exec … -- true`; with the new one the repair ran and the exec's run row
was stamped nine seconds after the command began, with `ended_at` 33 ms later —
so the user's command really did start after the repair finished. The unit tests
pin the cause (preparation strictly precedes recording); the live runs pin the
effect (the marker is cleared). There is still no unit test that watches a
pending update be cleared, because the preparation path resolves a real runtime
and has no injection point; adding one means a test-only runtime factory on
`SandboxManager`.

**Revisit.** If a command ever needs its run recorded before the box is
prepared, that is a new decision and this type will say so.

## ADR-0074 — a stale home is merged, archived to the host, and its credentials are deleted

**Status.** Accepted (2026-09-06, with Ethan).

**Context.** ADR-0069 left every box built before 0.2.1 with a `/home/<user>`
full of things nothing reads, because the login shell used `/home/<user>.guest`.
On the box used to develop this, that directory held `.claude/.credentials.json`
and `.codex/auth.json` — the two files v4 copied in and v5 stopped copying. A
cleanup that tarred the directory up and left the tarball in the box would have
re-created, in one archive, exactly the leak the broker exists to prevent. The
first implementation did that.

**Decision.** Three separate dispositions, not one.

- **Credentials are deleted and named, never merged and never archived.** The
  list is by file, not by directory: `.config` as a whole is settings, but
  `.config/gh/hosts.yml` is an OAuth token and `.config/gcloud` is a credential
  database. `.claude/.credentials.json`, `.codex/auth.json`, `.netrc`,
  `.npmrc`, `.docker/config.json` and `.aws/credentials` complete it. This holds
  under `--keep` too, and the confirmation prompt says so before anything runs.
- **Settings are merged** into the real home without overwriting anything
  already there, and a merged `.gitconfig` loses its `[credential]` sections
  through the same helper provisioning uses.
- **Everything else is archived to the host**, at
  `<state_dir>/archives/<box>-stale-home-<UTC>.tar.gz`, 0600 inside a 0700
  directory. The order is pack in the guest, copy out, compare both sides by
  byte count and sha256, and only then delete. Any failure leaves the guest
  directory untouched and removes both the temporary and the half-written host
  file.

**Consequences.** The archive outlives the box, which is the point: an archive
inside a box is lost with `devbox destroy`, and this directory is most likely to
be cleaned up by someone who is about to recreate the box. `Runtime` gained
`copy_from`, the first primitive for pulling a file out of a box; the Lima and
Docker command lines were checked against `--help` on this host, and the Incus
and Multipass ones against their documentation only, marked unverified in the
code. Incus joins instance and path with `/` rather than `:` — a colon there is
parsed as a remote name. `--keep` does the merge, the archive and the credential
deletion but not the removal, for anyone who wants to look before the directory
goes.

**Revisit.** The credential list is a list, and lists go stale. If it grows a
third time, the decision to enumerate rather than pattern-match is the one to
re-examine.
