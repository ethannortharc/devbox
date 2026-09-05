# Devbox v5 — Run Evidence, Credential Broker, MCP Sandboxing, Checkpoints, Export

> Design document for the v5 evolution of devbox: narrow the product to one job — running AI coding agents in a box whose every side effect is visible, scoped, and reviewable — and split the network lab into its own product.

**Author:** Ethan
**Date:** 2026-09-05
**Status:** Approved for implementation. This document is the source of truth for the v5 build. `DECISIONS.md` carries the ADRs; `PROGRESS.md` the build log.
**Supersedes:** the product framing of `2026-08-06-devbox-v4-design.md`. The v4 sandbox core, web console, observability plane, and policy engine are kept. Components D and E of v4 (box lab, ZTP) are removed; their code is archived outside this repository.

---

## 1. What changes from v4

v4 shipped two products in one binary: a glass-box sandbox and a network lab. The September 2026 review (see the v5 decision record in `PROGRESS.md`) found that the sandbox baseline — "the agent cannot touch your files" — is now built into Claude Code, Codex CLI, and Docker Sandboxes, while nobody ships the two things v4 is actually good at: a kernel-level account of what one agent run did, and a way to keep credentials out of the box while the agent still uses them. The lab, meanwhile, has a different user entirely.

v5 therefore does five things and one removal:

| Track | One line | New surface |
|---|---|---|
| **A. Run evidence** | One command runs the agent; when it exits you get a report: files changed, processes, domains, TLS names, bytes, policy violations, credential use — scoped to that run, collected below the agent | `devbox run -- <cmd>`, `devbox runs`, `devbox report`, Runs tab |
| **B. Credential broker** | Raw API keys and tokens never enter the box; a host-side broker injects them per request, within a declared scope, and logs each use | `devbox secret`, `devbox broker`, `credential` events |
| **D. MCP sandboxing** | Any stdio MCP server runs inside a box under a posture and shows up in the audit; the host sees a plain stdio MCP server | `devbox mcp add/run/ls/rm` |
| **E. Overlay checkpoints** | The overlay upper layer can be checkpointed, diffed, and restored; runs are bracketed by checkpoints so file changes are per-run | `devbox layer checkpoint/checkpoints/diff/restore` |
| **G. Export** | Every event and report exports as OCSF and OTLP so enterprise control planes can ingest devbox without an adapter | `devbox export --format ocsf\|otlp-json\|jsonl` |
| **Removal** | `src/lab`, `ztpd`, `labkit`, the Labs console views, and their docs leave devbox. The code is archived with its history in a local `devbox-lab` repository; a future lab, if any, will be a separate, container-based tool | — |

Track C from the review (VM-level parallel workspaces) is deliberately deferred until dogfooding shows it is wanted.

## 2. Goals and non-goals

### Goals

- **G1 — One run, one report.** `devbox run -- claude` (or any command) produces a self-contained report that answers "what did this run do" across files, processes, network, and credentials, with an honest statement of capture coverage.
- **G2 — Credentials stay on the host.** No API key, OAuth token, or git credential is written into the guest or its environment. The agent uses a per-box broker token; the broker enforces scope and records use.
- **G3 — Everything is a run.** Interactive shells, `exec`, MCP servers, and `devbox run` all attribute their events to a run so reports and diffs have stable boundaries.
- **G4 — Coverage is stated, never implied.** Every report, `doctor`, and the console say which capture sources were active (eBPF vs proc+packet) and how many events were dropped.
- **G5 — Ingestible by others.** Events and reports map to OCSF and OpenTelemetry without loss of the fields that matter.
- **G6 — One product.** After the split, nothing in the binary, console, or README is about network labs.

### Non-goals

- **N1 — No TLS interception.** The broker is a per-service reverse proxy; devbox never installs a CA in the guest.
- **N2 — No per-process egress policy in v5.** Posture stays box-granular; MCP servers needing a different posture get their own box.
- **N3 — Not a hosted service.** Local-first, single host, no accounts.
- **N4 — No live OTLP push required.** File export is the contract; push is optional.

## 3. Architecture deltas

```
host                                                      guest (Linux VM)
────                                                      ────────────────
devbox CLI ── run ──▶ obs::run (runs table, attribution) ◀── devbox-obsd events (cgroup_id, pid, ppid)
           ── layer ─▶ sandbox::checkpoint ──── exec ─────▶ /var/devbox/checkpoints/<id>/
           ── mcp ───▶ mcp::shim (stdio ⇄ exec) ─────────▶ MCP server process (a run)
           ── export ▶ export::{ocsf, otlp}  (reads store)
devbox __broker ◀──── host-reach (per runtime) ──────────── agent / git / gh with DEVBOX_BROKER_*
   │ secrets: OS keychain (macOS) / 0600 file (Linux)
   └─▶ credential events ──▶ obs store
web console: Runs tab, capture-source badge, report view
```

New Rust modules and who owns them during the parallel build (§10):

| Module | Track | Notes |
|---|---|---|
| `src/obs/run.rs` | A | runs table, attribution, run lifecycle |
| `src/report/` | A | `RunReport` model + markdown/json/html renderers |
| `src/cli/run.rs`, `src/cli/runs.rs`, `src/cli/report.rs` | A | |
| `src/sandbox/checkpoint.rs` | E | create/list/diff/restore/retention |
| `src/broker/` | B | server, providers, scopes, secrets, host-reach |
| `src/cli/secret.rs`, `src/cli/broker.rs` | B | |
| `src/mcp/` | D | registry, stdio shim, `mcp run` |
| `src/cli/mcp.rs` | D | |
| `src/export/` | G | ocsf.rs, otlp.rs, jsonl passthrough |
| `src/cli/export.rs` | G | |

Shared files touched by more than one track are listed in §10 with the integration order.

## 4. Component A — Run evidence

### 4.1 The run

A **run** is a bounded execution inside a box with a stable identity. Every run records:

| Field | Source |
|---|---|
| `run_id` | host-generated, 26-char, time-sortable (ULID-style) |
| `box_id`, `kind` (`run` \| `exec` \| `shell` \| `mcp`) | CLI |
| `argv`, `cwd`, `label` | CLI |
| `started_at`, `ended_at`, `exit_code`, `status` (`running` \| `finished` \| `aborted`) | CLI |
| `posture_before`, `posture_during` | policy |
| `cgroup_id`, `root_pid` | guest wrapper (§4.2) |
| `checkpoint_start`, `checkpoint_end` | E |
| `capture_sources`, `agent_version`, `dropped_events` | collector health at end |

Stored in a `runs` table in the box's SQLite store (WAL; the CLI and the collector daemon are both writers, with a busy timeout). `events` gains a nullable `run_id` column and an `attribution` column (`cgroup` \| `pidtree` \| `window`).

### 4.2 Scoping inside the guest

`devbox run` does not exec the command directly. It execs a wrapper that:

1. Puts the process in its own cgroup: `systemd-run --scope --unit devbox-run-<id> --quiet -- <cmd>` when systemd is present; otherwise creates `/sys/fs/cgroup/devbox/run-<id>` and writes its own pid into `cgroup.procs` before `exec`.
2. Publishes the cgroup id (the inode of the cgroup directory) and root pid to `/run/devbox/runs/<id>.json`, which the host reads back once.
3. Execs the command with the run's environment (`DEVBOX_RUN_ID`, and B's broker variables).

The collector attributes an event to a run in this order: exact `cgroup_id` match against active runs (eBPF and proc sources both carry it); else `ppid` chain to the run's root pid (proc source); else, for events with no process (`pid == u32::MAX`, packet-derived), the single active run whose window contains the timestamp, marked `attribution = window`. A report states how many events of each attribution kind it contains.

Interactive commands (Claude Code, a shell) get a pty: `run` passes `interactive = true` to the runtime when stdin is a tty, exactly as `shell` does today.

### 4.3 Posture for the duration

`--posture <p>` applies the posture at start through the existing enforcement path and restores the previous one at end (ADR-0047: only reverse a switch that happened). Without the flag the box's current posture is used and recorded.

### 4.4 The report

`report::RunReport` is assembled from: the run row; E's checkpoint diff (`files`); `behavior::summarize` over `events WHERE run_id = ?` (`processes`, `domains`, `dns`, `tls`, `connections`, `bytes`); `policy` events (`violations`); B's `credential` events (`credential_use`); and collector health (`coverage`: sources, dropped, unattributed count).

Three renderers, all hand-rendered, no external assets:

- **Markdown** — the terminal summary and what `devbox report` prints by default.
- **JSON** — the full model; the contract for tooling and for G.
- **HTML** — one self-contained file: inline CSS, no JavaScript required, the JSON embedded in a `<script type="application/json">` block so the file is also machine-readable. Sections: header (box, command, duration, exit code, coverage badge), files (with per-file size delta), network (domains → connections table, TLS names, bytes), processes (tree), credential use, violations, unattributed events.

Reports live at `~/.devbox/runs/<box>/<run_id>/report.{md,json,html}`. `devbox run` prints the summary and the HTML path; `--open` opens it; `--no-report` skips rendering (the run is still recorded).

### 4.5 Diffing runs

`devbox behavior diff --run <a> --run <b>` reuses `behavior::diff` on the two run-scoped summaries and adds the file-change diff between the two runs' end checkpoints.

### 4.6 CLI and console

```
devbox run [NAME] [--posture P] [--label L] [--no-report] [--open] -- CMD...
devbox runs [NAME] [--limit N]
devbox report <RUN_ID> [--format md|json|html] [--open]
devbox behavior diff --run A --run B
```

Console: a **Runs** tab on the box page (list, status, duration, coverage badge, violations count) and `/boxes/{name}/runs/{id}` rendering the HTML report. `exec` and `shell` also create runs (`kind = exec|shell`) so the tab is complete; they do not render reports unless asked.

## 5. Component E — Overlay checkpoints

### 5.1 Model

A checkpoint is a copy of the overlay upper layer at a moment, plus a manifest. It is **not** a new overlay layer: stacking lowers would need a remount that disturbs open files, and the upper is small by construction. Guest layout:

```
/var/devbox/overlay/upper/                 live upper (unchanged)
/var/devbox/checkpoints/<id>/upper/        cp -a --reflink=auto of the upper
/var/devbox/checkpoints/<id>/manifest.json id, label, created_at, run_id, files, bytes
```

Whiteouts (char 0/0 devices) and opaque directories (`trusted.overlay.opaque`) are copied as-is so a checkpoint is a faithful upper.

### 5.2 API (Rust, `sandbox::checkpoint`)

```rust
pub async fn create(rt: &dyn Runtime, box_name: &str, label: Option<&str>) -> Result<Checkpoint>;
pub async fn list(rt: &dyn Runtime, box_name: &str) -> Result<Vec<Checkpoint>>;
pub async fn diff(rt: &dyn Runtime, box_name: &str, from: &CheckpointId, to: Target) -> Result<Vec<OverlayChange>>; // Target = Checkpoint(id) | Live
pub async fn restore(rt: &dyn Runtime, box_name: &str, id: &CheckpointId) -> Result<()>;
pub async fn delete(rt: &dyn Runtime, box_name: &str, id: &CheckpointId) -> Result<()>;
pub async fn prune(rt: &dyn Runtime, box_name: &str, keep: usize) -> Result<Vec<CheckpointId>>;
```

`diff` walks both trees the way `overlay::diff` walks the upper today and yields the same `OverlayChange` type, so `devbox diff`, the Files tab, and the run report share one representation. `restore` uses the `discard` machinery (clear the upper) and then copies the checkpoint back; it refuses while a run is active on the box. Retention: keep the last 20 unless pinned by a run that still exists.

### 5.3 CLI

```
devbox layer checkpoint [NAME] [--label L]
devbox layer checkpoints [NAME]
devbox layer diff [NAME] --from <id> [--to <id>]
devbox layer restore <id> [NAME]        # the id is required, so it comes first (clap), as in `snapshot restore`
```

## 6. Component B — Credential broker

### 6.1 Shape

A host process, `devbox __broker`, supervised like the collector (started by lifecycle commands, identity sidecar, one per user). It is a **per-service reverse proxy**: the guest talks plain HTTP to the broker at a devbox-provided address; the broker opens TLS to the real upstream and injects the credential. No CA, no MITM, no CONNECT tunnelling.

### 6.2 Secrets

```
devbox secret set <provider> [--from-env VAR | --from-keychain SERVICE ACCOUNT | --stdin]
devbox secret ls | rm <provider>
```

Storage: macOS Keychain (`security add-generic-password -s devbox -a <provider>`); Linux `~/.devbox/secrets/<provider>` mode 0600 (`secret-tool` when available). Never in `state.json`, `devbox.toml`, or logs. The `provision.rs` code that copies `~/.claude/.credentials.json`, `~/.codex/auth.json`, and synthesises an `aichat` config with a key inside the guest is **removed**; it contradicts the README's own promise.

### 6.3 Providers and guest wiring

| Provider | Upstream | Guest sees | Injection |
|---|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `ANTHROPIC_BASE_URL=http://<broker>/anthropic`, `ANTHROPIC_AUTH_TOKEN=<box token>` | `x-api-key` or `Authorization: Bearer` depending on secret kind |
| `openai` | `https://api.openai.com` | `OPENAI_BASE_URL=http://<broker>/openai/v1` | `Authorization: Bearer` |
| `github` | `https://api.github.com`, `https://github.com` (smart HTTP) | git: `url."http://<broker>/github/".insteadOf "https://github.com/"` in the guest gitconfig; gh: verified in the build (GHES-style `GH_HOST` pointing at the broker, or a credential helper) | `Authorization: Bearer` |
| `http:<name>` | any `--url` | `DEVBOX_SECRET_<NAME>_URL` | configured header |

The box token is per box, rotated at box start, passed only in the environment of sessions devbox itself starts (`run`, `exec`, `shell`, `mcp run`). Holding the token grants brokered, scoped, logged access — nothing more.

### 6.4 Scopes

Per provider: allowed methods, path prefixes, and for `github` a repository allowlist that **defaults to the project's own origin repositories**, detected from the project directory's git remotes. `devbox secret scope github --repo owner/name [--allow read|push|admin]` widens it. A denied request gets a 403 with the reason and a `credential` event with `verdict = denied`.

### 6.5 Audit

Every brokered request produces an event of the new kind `credential`: provider, method, host, path (query stripped), upstream status, request/response bytes, verdict, and run attribution (the active run of that box at that time, `attribution = window`). It appears in `watch`, in run reports, and in G's export.

### 6.6 Reaching the host

The guest must reach a listener on the host. This is runtime-specific and **verified, not assumed** (memory: "the substrate is not my machine"):

| Runtime | First choice | Fallback |
|---|---|---|
| Lima | `host.lima.internal:<port>` | reverse tunnel devbox opens over Lima's SSH (`-R`) |
| Incus | host bridge address | reverse tunnel over `incus exec` socat |
| Docker | `host.docker.internal:<port>` | — |

`Runtime` gains `async fn host_reach(&self, name) -> Result<HostReach>` returning the verified address; `doctor` prints it. The broker binds loopback plus whatever the runtime needs, and every request must carry a valid box token, so binding wider than loopback does not widen access.

### 6.7 Policy interplay

Under `allowlist` and `mirror-only`, the broker address is implicitly allowed (the same reasoning as DNS in ADR-0020: the posture would otherwise be unusable). Under `isolated` it is blocked; an isolated run has no credentials, which is the point.

## 7. Component D — MCP sandboxing

### 7.1 Model

```
devbox mcp add <name> [--box BOX] [--posture P] -- <command...>
devbox mcp run <name>              # host-side stdio MCP server; what Claude Code / Codex launch
devbox mcp ls | rm <name> | report <name>
```

`add` records `[mcp.<name>] command = [...], box = "...", posture = "..."` in `devbox.toml`. `run` is a stdio shim: it starts the command inside the box through the runtime's exec path with stdin/stdout piped byte-for-byte (JSON-RPC over stdio) and stderr to `~/.devbox/mcp/<name>.log`. The in-box process is started as a **run** (`kind = mcp`), so its events are attributed and `mcp report <name>` is the last run's report.

The host tells the agent to use it with the ordinary command: `claude mcp add <name> -- devbox mcp run <name>`; `add` prints that line.

### 7.2 Posture

Posture is box-granular (N2). `--posture` on `add` is honoured by switching the box's posture for the duration of the run as §4.3 does, which is right for a dedicated MCP box and noisy for a shared one; `add` warns when the target box is a project box. The recommended layout is one small box for MCP servers (`devbox create --name mcp-tools --bare`).

### 7.3 `devbox mcp self`

An MCP server exposing devbox itself to the agent: `list_runs`, `run_report`, `behavior_summary`, `watch`. Built on D's JSON-RPC plumbing after A's API lands (integration wave).

## 8. Component G — Export

```
devbox export [NAME] (--run ID | --from T --to T) --format ocsf|otlp-json|jsonl [--out PATH]
devbox export ... --otlp-endpoint URL      # optional push of the same OTLP payload
```

Hand-rendered JSON (ADR-0018 spirit; no SDK).

**OCSF 1.3 mapping**

| devbox event | OCSF class (`class_uid`) | activity |
|---|---|---|
| `exec` / `exit` | Process Activity (1007) | Launch / Terminate |
| `file` | File System Activity (1001) | Create / Read / Update / Delete from `op` |
| `connect` / `accept` | Network Activity (4001) | Open, with `connection_info` and `traffic` bytes |
| `tls` | Network Activity (4001) + `tls` object (`sni`, `alpn`) | Open |
| `dns` | DNS Activity (4003) | Query / Response |
| `api` | HTTP Activity (4002) | from method |
| `policy` | Detection Finding (2004) | Create; `verdict` in `finding_info` |
| `credential` | API Activity (6003) | from method; `actor.session.uid = run_id` |

`metadata.product = {name: "devbox", version}`, `device.hostname = box`, `actor.process` from pid/comm, `metadata.correlation_uid = run_id`.

**OTLP/JSON** — `ExportLogsServiceRequest`; resource attributes `service.name = devbox`, `service.version`, `devbox.box`, `devbox.run.id`; record attributes on the semantic conventions: `process.pid`, `process.parent_pid`, `process.executable.path`, `process.command_line`, `network.peer.address`, `network.peer.port`, `network.transport`, `dns.question.name`, `tls.client.server_name`, `file.path`, `http.request.method`, `url.full`; `event.name = devbox.<type>`. Run reports export as one log record with the JSON report as the body plus the same resource attributes.

## 9. Security model (updated)

| Layer | v4 | v5 |
|---|---|---|
| Files | overlay, explicit commit | + per-run checkpoints; restore to any checkpoint |
| Behaviour | box-wide audit | + per-run attribution, coverage stated per report |
| Egress | box posture | + posture per run; broker is the only credentialled path |
| Credentials | copied into the guest | **never in the guest**; scoped, logged broker |
| Tool servers | run on the host with full rights | run in a box as a run |
| Integrity of the record | — | reports carry store generation, event count, dropped count, attribution counts |

## 10. Build plan, file ownership, integration order

### Wave 0 — foundations (in flight)

| Task | Branch | Owns |
|---|---|---|
| W0-1 merge `origin/main` (70 commits) | `v5/merge-main` | the 8 conflict files |
| W0-2a remove labs/ZTP | `v5/lab-removal` | everything in `lab-manifest.md` |
| W0-2b archive the lab code with history (`~/Projects/design/devbox-lab`, no remote, no further work) | new repo | read-only on devbox |
| W0-3 commit CO-RE objects, local eBPF agent, capture source in doctor/console | `v5/ebpf-local` | `build.rs`, `agent/bpf`, `.gitignore`, handshake, doctor capture line, capture bar |
| W0-4 CLI: positional `NAME` everywhere, `--name` kept as hidden alias | after the above | every `src/cli/*.rs` |

Integration order: W0-1 → W0-2a → W0-3 → W0-4, each followed by the full gate on the integrated branch `v5`.

### Wave 1 — the five tracks, in parallel from `v5`

| Track | Owns (new) | Touches (shared, small, listed) |
|---|---|---|
| A | `src/obs/run.rs`, `src/report/`, `src/cli/{run,runs,report}.rs`, `src/web/templates/{_runs_tab,run_report}.html` | `src/obs/store.rs` (runs table, `run_id` columns), `src/cli/mod.rs` (register), `src/web/routes.rs` (Runs tab), `src/obs/mod.rs` |
| E | `src/sandbox/checkpoint.rs` | `src/cli/layer.rs`, `src/sandbox/mod.rs` (`pub mod`) |
| B | `src/broker/`, `src/cli/{secret,broker}.rs` | `src/obs/event.rs` (`Credential` kind), `src/runtime/mod.rs` (+`host_reach`) and the three runtimes, `src/sandbox/provision.rs` (remove credential copying; write gitconfig), `src/sandbox/mod.rs` (broker lifecycle hook), `src/cli/mod.rs` |
| D | `src/mcp/`, `src/cli/mcp.rs` | `src/sandbox/config.rs` (`[mcp.*]`), `src/cli/mod.rs` |
| G | `src/export/`, `src/cli/export.rs` | `src/cli/mod.rs` |

Rules: a track never edits another track's new files. `src/cli/mod.rs` is touched by all five — each adds exactly one subcommand arm and one `mod` line; the supervisor resolves the trivial conflicts. A and E do not call each other during wave 1: A consumes the `checkpoint` API by name as specified in §5.2, and the supervisor wires it at integration. The same holds for A ↔ B (broker env into the run wrapper), D ↔ A (`mcp run` as a run), G ↔ A (`run_id` in exports).

### Wave 2 — integration

Wire A↔E↔B↔D↔G, `devbox mcp self`, `exec`/`shell` as runs, README rewrite around the run report, quickstart, ADRs 0056–0063, version 0.2.0, release with the eBPF agent, screenshot replaced by a run report.

### Acceptance (the whole of v5)

1. On a fresh macOS host: `cargo build --release`, `devbox`, `devbox secret set anthropic --from-env ANTHROPIC_API_KEY`, `devbox run -- claude -p "add a test"` → an HTML report with files, processes with pids, TLS names, credential use, and `coverage: ebpf+packet`; `grep -r sk-ant- ` inside the guest finds nothing.
2. `devbox mcp add fetch -- uvx mcp-server-fetch`, `claude mcp add fetch -- devbox mcp run fetch`, use it once → `devbox mcp report fetch` shows the domains it reached.
3. `devbox layer checkpoint`, edit, `devbox layer diff --from <id>`, `devbox layer restore <id>` → the edit is gone; `devbox diff` agrees.
4. `devbox export --run <id> --format ocsf` validates against the OCSF schema for every class used; `--format otlp-json` is accepted by an OpenTelemetry Collector `otlp` receiver.
5. `devbox --help` has no `lab`; nothing in the binary, console, or docs refers to labs or ZTP.

## 11. Open questions for Ethan

1. Whether to rename devbox itself; `devbox` is also jetify's Nix tool.
2. Whether `exec` and `shell` should render a report by default or only record the run (default here: record only).

Decided 2026-09-05: the lab is removed rather than split into a maintained product; a future lab will be container-based and separate.

© 2026 Ethan H.B. Zhou
