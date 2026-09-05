# Devbox v4 — Single Binary, Web Console, Deep Observability, Box Lab

> Design document for the v4 evolution of devbox: retire the terminal UI, converge on **one binary + one local web console**, add **deep eBPF-based observability of everything happening inside a box**, and add **box lab** — a multi-machine virtual network for building and testing small networks (including a Zero-Touch-Provisioning fabric).

**Author:** Ethan
**Date:** 2026-08-06
**Status:** Design approved for implementation. This document is the source of truth for the v4 build; the milestones in §16 drive the autonomous build loop.
**Supersedes UI/UX of:** v3 (`docs/plans/2026-03-07-devbox-v3-design.md`). The v3 sandbox core (runtime abstraction, NixOS provisioning, OverlayFS diff/commit) is **kept and reused**.

---

## Table of Contents

1. Executive Summary — What Changes From v3
2. Goals & Non-Goals
3. Design Philosophy
4. Architecture Overview (Rust core + Go eBPF agent + Python lab toolkit)
5. The Big Removal — TUI / Zellij → Web Console
6. Component A — Single Binary & Local Web Console
7. Component B — Deep Observability (eBPF Glass-Box)
8. Component C — Egress & Activity Control
9. Component D — Box Lab (Multi-Machine Networking)
10. Component E — ZTP Fabric, Source-of-Truth & Config Generation
11. Data Model, Event Schema & APIs
12. Configuration Files (v4)
13. Language Seams & Tech Stack
14. Project Structure (v4)
15. Security Model (updated)
16. Implementation Phases & Acceptance Criteria
17. Testing, CI & Quality Bar
18. Open Questions & Future Work
19. Appendix — CoreWeave JD Skill Mapping

---

## 1. Executive Summary — What Changes From v3

v3 gave us a working local sandbox: a single Rust binary that provisions a NixOS VM across Lima/Incus/Multipass/Docker, mounts the project read-only through OverlayFS, and exposes `diff`/`commit`/`discard` so an AI agent can write code without ever touching the host. The developer experience was delivered through **Zellij layouts** — a set of tiled terminal panes.

v4 keeps the sandbox core and changes three things:

**1. The interface moves from terminal to browser.** The Zellij/TUI layer is retired. The binary now starts a small local web server and opens a **web console** where all operations happen: manage boxes (list, status, start, stop, create, destroy), install/toggle tool sets, open a shell into a box, and — new in v4 — watch everything the box is doing in real time. One binary, one URL, no tmux/zellij muscle memory required.

**2. Boxes become glass boxes.** v4 adds a **deep observability layer built on eBPF**. Every meaningful activity inside a box is captured and correlated onto a single timeline: process executions (who ran what, with which args), network connections (who connected to whom, how many bytes), DNS queries, TLS SNI, file access, and — optionally — application/API-level calls (e.g. which LLM endpoint an agent hit and how many tokens). This is the v3 `devbox diff` idea applied to *behavior* instead of files: after a run you get a **behavior diff** — the domains touched, processes spawned, files changed, and any policy violations.

**3. One box becomes many.** v4 adds **box lab**: describe a topology in one file and devbox brings up multiple nodes wired into a real virtual network — subnets, routing (FRR), DNS/DHCP (dnsmasq), NTP (chrony) — with per-link fault injection (netem) and a scenario library. The flagship scenario is a **ZTP fabric**: blank nodes boot, pull their config with zero manual steps, and converge, with chaos testing and SLOs on top. The whole lab shares one observability plane, so the live topology graph shows real traffic.

The result is two products in one binary. On a single machine it is a **glass-box sandbox** for running and validating tests where you can see and control every side effect. Wired up, it is a **small network lab** for learning and testing routing, provisioning, and failure behavior.

### v3 → v4 diff at a glance

| Aspect | v3 | v4 |
|---|---|---|
| Primary UI | Zellij layouts (TUI) | Local **web console** (htmx + SSE), single binary |
| In-box terminal | Zellij panes | Browser terminal (xterm.js over WebSocket) |
| Box management | CLI + TUI package manager | CLI + web (list/status/start/stop/create/destroy/install) |
| Build model | Sets baked at provision | **On-demand**: toggle sets in UI, Nix builds only what's checked |
| Observability | None (files only, via `diff`) | **eBPF glass-box**: net + DNS + SNI + exec + file + API, correlated |
| Control | Filesystem overlay only | + **egress/activity policy** (default-deny / allowlist / mirror-only) |
| Topology | Single box | + **box lab**: multi-node, routing, DNS/DHCP/NTP, netem, scenarios |
| Languages | Rust | Rust (core/web) + **Go** (eBPF agent, ZTP server) + **Python** (lab toolkit, config-gen, test SDK) |

---

## 2. Goals & Non-Goals

### Goals

- **G1 — One binary, one console.** `devbox` starts a local web console that can do everything. No external UI dependencies, no separate frontend server, no Zellij.
- **G2 — Total in-box visibility.** Capture as much of a box's activity as practical — network, DNS, TLS SNI, process/exec, file access, and (opt-in) API-level calls — and present it correlated on one timeline, live and after the fact.
- **G3 — Actionable control.** Turn observations into policy: choose a box's egress posture and get alerted (or block) when a box does something outside policy.
- **G4 — Multi-machine lab.** Describe and bring up a small network from a single file, with routing, core network services, fault injection, and a scenario library.
- **G5 — On-demand everything.** Boxes and their toolchains build/start lazily. Toggle a tool set in the UI and Nix builds just that; open a stopped box and it starts.
- **G6 — Test/validation first-class.** Provide a Python SDK so tests can assert on observed behavior ("no egress outside the allowlist during this run", "all lab nodes converged in < 90s").
- **G7 — Learning vehicle.** The lab and ZTP scenarios are designed to teach and demonstrate the exact skills in the target CoreWeave role (see §19): ZTP, config generation, source-of-truth/IPAM, observability, SLIs/SLOs — written in Python + Go.

### Non-Goals

- **N1 — Not a cloud/fleet product.** v4 is local-first and single-host. No multi-tenant auth, no remote fleet management.
- **N2 — Not a production NMS.** The lab is for building/testing/learning, not for running a real datacenter network.
- **N3 — Not a full APM.** Application/API-level capture is opt-in and best-effort (uprobe/MITM), not a replacement for in-app instrumentation.
- **N4 — No GPU/RDMA emulation.** The AI-fabric scenarios model *topology and traffic patterns* (fat-tree, rail locality, stragglers) at the IP layer; they do not emulate InfiniBand/RoCE hardware.

---

## 3. Design Philosophy

- **Glass box, not black box.** The v3 promise was "the AI can't touch your files." The v4 promise adds "…and everything it *does* touch — every connection, process, and call — you can see and govern." Observability is not a feature bolted on; it is the product's spine.
- **One binary is a feature.** Distribution, upgrades, and trust all get easier when there is exactly one artifact. The web console is embedded (assets compiled into the binary); the Go agent is embedded and pushed into the box; the Python toolkit ships as a versioned package pushed into the substrate.
- **Server-rendered, minimal JS.** The console is HTML rendered by the Rust binary, made live with htmx + Server-Sent Events. No SPA build step, no node toolchain in the release path. Real-time views (event stream, topology traffic) stream over SSE. This keeps the "single binary" promise honest and the build robust.
- **Progressive disclosure.** `devbox` with no args still does the right thing (create-or-attach for the current project, open the console). Depth (lab, policy, eBPF detail) is available but never in the way.
- **On-demand by default.** Nothing heavy is built or started until asked for. Toggling a set compiles only that set; opening a box starts it; a lab node boots when the topology needs it.
- **Reuse the hard-won core.** The v3 runtime abstraction, NixOS provisioning, and OverlayFS logic are proven. v4 builds on them; it does not rewrite them.
- **Everything is testable.** If the product's job is to observe behavior, then tests can assert on that behavior. The observability plane is also the test oracle.

---

## 4. Architecture Overview

v4 is a **hybrid three-language system** with clean seams. The user runs exactly one binary; the other two languages ship *inside* it or *inside the box*.

```
                          ┌──────────────────────────────────────────────┐
   Browser  ── http/SSE ──▶  devbox  (single Rust binary, runs on host)   │
   (console)   ◀── ws ─────  ┌───────────────────────────────────────┐   │
                            │  Web Console (axum + askama + htmx/SSE) │   │
                            ├───────────────────────────────────────┤   │
                            │  Control Plane                          │   │
                            │   - Box lifecycle (v3 core, reused)     │   │
                            │   - Lab orchestrator (new)              │   │
                            │   - Event collector + store (SQLite)    │   │
                            │   - Policy engine (egress/activity)     │   │
                            │   - Prometheus /metrics                 │   │
                            ├───────────────────────────────────────┤   │
                            │  Runtime Trait (Incus/Lima/Multipass/  │   │
                            │  Docker)  +  Nix Set Manager  (reused)  │   │
                            └───────────────────────────────────────┘   │
                          └───────────────┬──────────────────────────────┘
                                          │  vsock / unix socket (event + control stream)
                          ┌───────────────▼──────────────────────────────┐
                          │  Guest / Lab substrate (Linux)                │
                          │  ┌─────────────────────────────────────────┐ │
                          │  │  devbox-obsd  (Go, cilium/ebpf)          │ │
                          │  │   eBPF: exec, connect/accept, DNS, file  │ │
                          │  │   optional: TLS uprobe (SSL_read/write)  │ │
                          │  │   nftables/cgroup egress enforcement     │ │
                          │  └─────────────────────────────────────────┘ │
                          │  ┌─────────────────────────────────────────┐ │
                          │  │  Lab nodes (containers) wired veth/bridge│ │
                          │  │   FRR (routing), dnsmasq (DHCP/DNS),     │ │
                          │  │   chrony (NTP)                           │ │
                          │  └─────────────────────────────────────────┘ │
                          │  ┌─────────────────────────────────────────┐ │
                          │  │  devbox-ztpd (Go) + devbox-lab (Python)  │ │
                          │  │   ZTP HTTP server + state machine        │ │
                          │  │   source-of-truth / IPAM / config-gen    │ │
                          │  └─────────────────────────────────────────┘ │
                          └───────────────────────────────────────────────┘
```

### The three languages and why

- **Rust — the control plane and web console (the binary the user runs).** Reuses everything from v3 and gains the web server, event collector/store, policy engine, and lab orchestrator. Rust is where lifecycle correctness and the single-binary promise live.
- **Go — the in-guest observability agent (`devbox-obsd`) and the ZTP server (`devbox-ztpd`).** eBPF in Go via `cilium/ebpf` is the most battle-tested option (it's the Cilium/Falco-adjacent ecosystem), and packet/DNS/SNI parsing via `gopacket` is mature. The ZTP server is a real network service — exactly the kind of Go service the target role builds.
- **Python — the lab toolkit (`devbox-lab`): source-of-truth/IPAM, config generation (Jinja2), and the pytest-based test SDK.** This maps directly to the Ansible/Jinja + NetBox + Prometheus world and is the most ergonomic language for templating and test authoring.

### Substrate model (important design decision)

A **box** (single sandbox) uses the v3 runtime (VM on Lima/Incus, container on Docker fallback). A **lab** does *not* spin up N heavyweight VMs. Instead, lab **nodes are lightweight containers wired containerlab-style inside one Linux substrate** (a single Lima VM on macOS, or the host on Linux). Rationale:

- **Density & speed:** dozens of nodes on one kernel start in seconds, not minutes.
- **Real networking:** veth pairs + Linux bridges + network namespaces give genuine L2/L3 behavior; FRR makes nodes real routers.
- **One observability plane:** a single `devbox-obsd` in the substrate sees every node's traffic and processes — the whole lab is glass.
- **eBPF needs Linux:** the substrate is always Linux (native on Linux, the Lima guest on macOS), so eBPF works uniformly.

---

## 5. The Big Removal — TUI / Zellij → Web Console

### What goes

- `src/tui/` (the ratatui package manager and related screens) is retired.
- Zellij layouts (`layouts/*.kdl`) and all `devbox layout *` commands are removed from the default path. The `zellij` package drops out of the default `shell` set (kept available as an optional package for users who still want a multiplexer inside a shell).
- `devbox guide`/cheat-sheets move into the web console as a Help view (content preserved).

### What replaces it

`devbox` (no args) or `devbox web` starts the console and opens the browser. Everything the TUI did — and much it couldn't — happens there. The plain CLI is **retained in full** for scripting and headless use (see §6.4): the web console is a client of the same control-plane API the CLI uses.

### Migration note

v3 sandboxes remain loadable. On first v4 launch, `devbox` detects v3 state under `~/.devbox/sandboxes/`, migrates `sandbox.yaml`/`devbox.toml` to the v4 schema (adds `observability`, `policy` sections with safe defaults), and records a migration entry. No box is destroyed by the upgrade.

---

## 6. Component A — Single Binary & Local Web Console

### 6.1 Server

- **Framework:** `axum` (Tokio) serving on `127.0.0.1:<port>` (default `7878`, auto-increment if taken). Bound to loopback only.
- **Templating:** `askama` (compile-time templates) rendering HTML fragments; `htmx` drives partial updates; **SSE** streams live data (event feed, build progress, lab traffic, box status). WebSocket only for the interactive terminal.
- **Assets embedded:** htmx, a small CSS system, and xterm.js are vendored and embedded via `rust-embed` — no CDN, no network needed, single binary intact.
- **Auth:** a per-launch random token in the opened URL (`?t=…`) plus loopback binding. No accounts. (The console governs local boxes; the trust boundary is the local user.)

### 6.2 Console views

- **Dashboard** — all boxes and labs as cards: name, runtime, status, CPU/mem, live activity sparkline (events/sec from the observability plane). Start/stop/open/destroy inline.
- **Box detail** — status & resources; **Activity** tab (the glass-box live stream, §7); **Policy** tab (egress posture, §8); **Files** tab (overlay `diff`/`commit`/`discard` from v3, now with a UI); **Terminal** tab (browser shell).
- **Create box** — form: project dir, runtime (auto), resources, and a **checklist of tool sets / languages / individual packages to build** (§6.3). Submitting streams Nix build progress over SSE.
- **Lab** — topology graph with live traffic overlay; node list; scenario picker; fault-injection controls (§9).
- **Help** — the migrated cheat sheets.

### 6.3 On-demand build & start

Two distinct "on-demand" behaviors, both surfaced in the UI:

- **Selective compilation.** The create/edit form is a checklist of Nix sets, language sets, and ad-hoc packages. Only checked items are built. Under the hood this composes a `configuration.nix` importing only the selected set modules and runs `nixos-rebuild switch` (or `nix build` of the selected closure). Toggling a set on a running box triggers an incremental rebuild; progress streams via SSE. This is the v3 set system (`src/nix/sets.rs`) driven by checkboxes instead of TOML edits.
- **Lazy start.** Opening a stopped box's Terminal/Activity view auto-starts it (with a visible "starting…" state). Labs start nodes as the topology requires. A configurable idle-TTL stops boxes that haven't been touched (off by default).

### 6.4 CLI ↔ Web parity

The web console and CLI are two clients of one control-plane API. Every web action has a CLI equivalent (existing v3 commands stay; new ones added: `devbox web`, `devbox lab …`, `devbox watch …`, `devbox policy …`, `devbox behavior diff`). Headless environments use the CLI; the console is the default interactive experience.

### 6.5 In-browser terminal

`xterm.js` ↔ WebSocket ↔ a PTY that runs `runtime.exec_cmd(box, shell, interactive=true)`. Resize events propagate. This replaces the Zellij "shell" tab. Multiple terminals = multiple tabs.

---

## 7. Component B — Deep Observability (eBPF Glass-Box)

The centerpiece. Goal: **capture as much of a box's activity as practical, correlate it, and present it live and after the fact.**

### 7.1 What is captured

| Domain | Signals | eBPF attach point (indicative) |
|---|---|---|
| **Process** | `execve` (path, argv, cwd, uid), fork/exit, process tree | tracepoint `sched_process_exec` / `sched_process_exit` |
| **Network — connections** | TCP/UDP connect & accept, 5-tuple, pid/comm, bytes in/out, duration | kprobe `tcp_connect`, tracepoint `inet_sock_set_state`, `sock:*` |
| **Network — DNS** | query name, qtype, response IPs, resolver | uprobe on `getaddrinfo`, or parse UDP/53 via `gopacket` on the tap |
| **Network — TLS** | SNI (server name), ALPN, cert subject | ClientHello parse on flow (no decryption) |
| **File** | open/create/write/unlink under workspace + sensitive paths | tracepoint `sys_enter_openat`, or fanotify (fallback) |
| **Syscall (selective)** | `ptrace`, `mount`, `setuid`, module load — security-relevant | raw tracepoints, allowlisted |
| **Application/API (opt-in)** | plaintext HTTP requests, incl. LLM endpoint + token counts | uprobe on `SSL_write`/`SSL_read` (OpenSSL) or transparent MITM proxy |

Everything is stamped with `(pid, tid, cgroup_id, box_id, monotonic_ts, wall_ts)` so events can be **joined into one timeline**.

### 7.2 The correlation model (the killer feature)

Raw events are cheap; the value is the join. Example correlated story for `pip install requests`:

```
t0  exec    pid=812  /nix/store/.../python3.12 -m pip install requests   (parent: zsh 640)
t0+ dns     pid=812  A pypi.org            → 151.101.0.223
t0+ connect pid=812  → 151.101.0.223:443  (TLS SNI=pypi.org, ALPN=h2)   bytes↑ 4.1KB ↓ 812KB
t0+ dns     pid=812  A files.pythonhosted.org → …
t0+ connect pid=812  → …:443  (SNI=files.pythonhosted.org)  ↓ 61KB
t0+ file    pid=812  write  /workspace/.venv/lib/python3.12/site-packages/requests/__init__.py
```

The collector builds these chains keyed on pid/cgroup and presents "process → resolved → connected → wrote" as a single expandable row. This is what turns a wall of events into insight.

### 7.3 Agent architecture (`devbox-obsd`, Go)

- **Loader:** `cilium/ebpf` with CO-RE objects generated by `bpf2go`; programs pinned per box/cgroup so multi-box and multi-lab-node capture is isolated.
- **Decode/enrich:** ring-buffer consumers decode events; `gopacket` parses DNS/TLS ClientHello; process metadata resolved from `/proc`. IPs reverse-mapped to the DNS name that produced them (so the UI shows `pypi.org`, not just an IP).
- **Transport:** length-prefixed protobuf (or JSON in dev) over **vsock** (VM runtimes) or **unix socket** (container substrate) to the Rust collector. Backpressure: bounded queue with a dropped-event counter surfaced as a metric (never silently drop without counting).
- **Lifecycle:** embedded in the devbox binary (`include_bytes!`), pushed into the box on provision, run as a systemd service in the NixOS guest (`services.devbox-obsd.enable = true`). Auto-restart; version-pinned to the host binary.
- **Overhead budget:** target < 3% CPU at 10k events/s; perf-event ring buffers, per-CPU maps, and in-kernel filtering (only the traced cgroups) keep it cheap.

### 7.4 Collector & store (Rust)

- Receives the event stream, writes to an **embedded SQLite** store (`~/.devbox/boxes/<id>/events.db`) with an append-only events table + derived indices (by pid, by domain, by time). A bounded in-memory ring holds the last N events for instant live view.
- Exposes: query API (filter by time/pid/domain/type), SSE live feed, and the **behavior-diff** builder.
- **Retention:** per-box cap (size + age), configurable; oldest rolled off.

### 7.5 Presentation (web)

- **Live activity stream** — SSE-fed, filterable (by type, pid, domain, path), pausable. Color-coded by domain. The default box "Activity" tab.
- **Flow table** — one row per connection: peer (name + IP), port, SNI, bytes, duration, owning process. Sortable, one-click **"capture pcap"** for a flow.
- **Process tree** — live `execve` tree with args and the network/files each process touched.
- **DNS log** — every lookup and its answer.
- **Topology traffic** (labs) — the graph edges animate with live byte-rate; click an edge for its flows.

### 7.6 Behavior diff & export

`devbox behavior diff [box] [--since <run/ts>]` (and a UI button) produces a **run summary**, the behavioral analogue of `devbox diff`:

```
Behavior summary — box "myapp", run 2026-08-06T22:14Z (4m12s)
  Domains contacted (7):  pypi.org, files.pythonhosted.org, github.com, api.anthropic.com, …
  New processes (23):     python3.12, pip, git, gcc, cc1, ld, …
  Files written (implied by overlay): 412 under /workspace/.venv, 3 under /workspace/src
  Egress policy:          ALLOWLIST — 0 violations
  Notable:                api.anthropic.com  ↑1.2MB ↓18MB  (LLM API, ~142k tokens seen via SSL uprobe)
```

Exports: JSONL (raw events), a Markdown summary, and pcap per selected flow. These summaries are diffable across runs ("this run contacted a domain the last one didn't").

### 7.7 Prometheus metrics

The collector exposes `/metrics` (Prometheus text format): events/sec by type, active flows, bytes by box, dropped-event counter, policy violations, agent overhead. A ready-made Grafana dashboard JSON ships in `docs/`. (This checks the JD's Prometheus/Grafana box and is genuinely useful for the lab.)

---

## 8. Component C — Egress & Activity Control

Observation becomes governance. Each box has an **egress posture**:

| Mode | Behavior |
|---|---|
| `open` (default for v3-migrated boxes) | Full network; everything observed, nothing blocked. |
| `allowlist` | Default-deny egress; only listed domains/CIDRs permitted. DNS-driven: names resolved by the agent are matched against the allowlist before the connection is allowed. |
| `mirror-only` | Only package mirrors / declared sources reachable (a curated allowlist for pip/npm/cargo/nix + git hosts). Ideal for "build, don't phone home." |
| `isolated` | No egress. Loopback + lab-internal only. |

- **Enforcement:** `nftables` sets managed by `devbox-obsd`, populated from resolved DNS answers for allowlisted names, plus static CIDRs. cgroup/connect4 eBPF hook as a second layer for pid-scoped rules.
- **Violations:** a blocked (or, in `open`+alert mode, merely flagged) connection raises an event, shows in the stream, increments a metric, and can fire a desktop notification.
- **Interactive mode (stretch, Phase 5b):** first connection to an unknown domain is held and a prompt appears in the console ("box *myapp* wants to reach `telemetry.example.com` — allow once / always / deny"). A local, sandbox-scoped Little-Snitch.
- **Credential proxy (stretch):** secrets never enter the guest; an egress proxy on the host injects auth headers for allowlisted API hosts, so a compromised box can't exfiltrate keys it never had.

Policy is per-box in `devbox.toml` and editable live in the UI.

---

## 9. Component D — Box Lab (Multi-Machine Networking)

### 9.1 Topology as code

One file (`lab.toml` or `lab.yaml`) describes nodes, links, subnets, roles, and per-node tool sets:

```toml
[lab]
name = "clos-3node"
substrate = "auto"          # auto | lima | incus | host

[[nodes]]
name = "leaf1"
role = "frr-router"         # frr-router | host | ztp-blank | service
sets = ["network"]

[[nodes]]
name = "leaf2"
role = "frr-router"

[[nodes]]
name = "spine1"
role = "frr-router"

[[links]]
endpoints = ["leaf1:eth1", "spine1:eth1"]
subnet = "10.0.12.0/31"

[[links]]
endpoints = ["leaf2:eth1", "spine1:eth2"]
subnet = "10.0.22.0/31"

[services]
dns = true                  # dnsmasq
dhcp = false
ntp = true                  # chrony
```

### 9.2 Bring-up

- `devbox lab up [file|scenario]` creates the substrate (one Linux VM/host), then for each node creates a network namespace / lightweight container, wires links as **veth pairs into Linux bridges**, assigns addresses from the IPAM (§10), and starts the node's role services (FRR with a generated config, dnsmasq, chrony).
- Nodes resolve each other by name (service discovery via the lab DNS).
- `devbox lab status` shows convergence (interfaces up, routing adjacencies, reachability matrix). The web Lab view renders the graph with live traffic.

### 9.3 Fault injection

Per-link `tc`/`netem` controls, scriptable and in the UI:

- delay / jitter / loss / reorder / duplicate / rate-limit
- **partition** (drop all traffic on a link or between groups) and **heal**
- **flap** (up/down on an interval)

`devbox lab fault leaf1-spine1 --loss 0.1% --delay 20ms`, or a slider in the UI.

### 9.4 Scenario library

Prebuilt topologies + scripts, each `devbox lab up <name>`:

| Scenario | What it demonstrates |
|---|---|
| `clos-3node` | eBGP-unnumbered leaf/spine, ECMP, reachability |
| `fat-tree-4x2` | 2-tier fat-tree; run the collective-traffic generator (§9.5) and induce a **straggler** with one lossy link |
| `partition-3` | 3-node cluster; partition & heal; watch reconvergence |
| `client-proxy-server` | egress/observability demo through a proxy hop |
| `wan-lossy` | high-latency lossy WAN link between two sites |
| `ztp-fabric` | the ZTP flagship (§10) |

### 9.5 AI-fabric flavor (learning + demo)

A small **collective-communication traffic generator** models ring/tree all-reduce across lab nodes at the IP layer. Combined with fault injection it reproduces the failure that dominates real AI clusters: **one slow/flapping link creates a straggler that stalls the whole collective.** The observability plane shows which link is the culprit. This is topology/traffic modeling only (not IB/RoCE emulation, per N4), but it makes concepts like bisection bandwidth, rail locality, and straggler detection tangible — and demoable.

---

## 10. Component E — ZTP Fabric, Source-of-Truth & Config Generation

The flagship scenario and the strongest learning/portfolio artifact. It brings up a fabric from **blank nodes with zero manual configuration** and validates it end to end.

### 10.1 The real-world flow being modeled

```
blank node boots → DHCP (option 66/67: boot server + script URL)
   → fetch bootstrap script over HTTP
   → identify self (serial / MAC) → query Source-of-Truth: "who am I? what's my role?"
   → receive rendered config → apply → self-check (interfaces, routing adjacency, NTP, mgmt reachability)
   → phone home / register healthy → appear in observability
```

### 10.2 Components

- **Source-of-truth / IPAM (Python, `devbox-lab`).** Pydantic models for sites/devices/roles/links/address-plan. Allocates subnets, IPs, loopbacks, ASNs; detects overlaps; is the single input everything else derives from. A tiny, opinionated NetBox-in-a-file.
- **Config generation (Python, Jinja2).** `intent + SoT → per-device config` (FRR/`frr.conf`, interfaces, NTP). Then **render → diff vs running → apply → verify**, mirroring the v3 `diff`/`commit` mental model at the network-config layer. Golden-file tested; idempotent (apply twice = no change).
- **ZTP server (Go, `devbox-ztpd`).** HTTP service + provisioning **state machine** (`discovered → identified → rendering → pushing → verifying → healthy | failed`), serial→role mapping, serves rendered configs, records per-node state, exposes status API + Prometheus metrics. dnsmasq provides DHCP with options 66/67 pointing at it.
- **Secure ZTP (stretch).** Signed bootstrap + cert-pinned boot server (the RFC 8572 idea) so a rogue server can't feed malicious config.

### 10.3 Chaos & SLOs

- Bring up N blank nodes; assert **all reach `healthy` with zero manual steps**.
- **Chaos:** kill `devbox-ztpd` mid-provision; make 10% of nodes fail first boot. Assert the system self-heals (idempotent retry from the half-configured state).
- **SLOs:** `node_provision_seconds` p95 < target; `fabric_converged == 100%`; surfaced on a Grafana dashboard and asserted by the test SDK.

### 10.4 Test SDK (Python)

pytest fixtures that spin a lab, drive a scenario, and **assert on observed behavior**:

```python
def test_ztp_fabric_converges(lab):
    lab.up("ztp-fabric", nodes=6)
    assert lab.all_nodes_healthy(timeout="120s")
    assert lab.bgp_fully_converged()
    assert lab.provision_p95() < 90            # SLO
    assert lab.observability.no_egress_outside(["10.0.0.0/8"])   # security assertion

def test_ztp_survives_server_crash(lab):
    lab.up("ztp-fabric", nodes=6)
    lab.chaos.kill("ztpd", at="50%-provisioned")
    lab.chaos.restart("ztpd", after="10s")
    assert lab.all_nodes_healthy(timeout="180s")   # idempotent recovery
```

The observability plane is the oracle: assertions read the same event store the console shows.

---

## 11. Data Model, Event Schema & APIs

### 11.1 Event schema (canonical)

```jsonc
{
  "ts_wall": "2026-08-06T22:14:07.412Z",
  "ts_mono_ns": 84021399123,
  "box_id": "myapp",
  "cgroup_id": 10231,
  "pid": 812, "tid": 812, "ppid": 640, "comm": "pip", "uid": 1000,
  "type": "connect",          // exec | exit | connect | accept | dns | tls | file | syscall | api | policy
  "net":  { "proto":"tcp","saddr":"10.0.0.5","sport":51234,
            "daddr":"151.101.0.223","dport":443,
            "domain":"pypi.org","sni":"pypi.org","alpn":"h2",
            "bytes_tx":4102,"bytes_rx":831720,"dur_ms":690 },
  "exec": null, "file": null, "api": null,
  "policy": null
}
```

Each `type` populates its own sub-object; the common envelope enables uniform storage, filtering, and correlation.

> v5 added two types to that list: `close`, which carries what crossed a connection and how long it lasted (W0-5b), and `credential`, produced on the host by the broker rather than by the guest agent (W1-B). See `docs/plans/2026-09-05-devbox-v5-design.md`.

### 11.2 Control-plane API (Rust, consumed by CLI + web)

```
GET    /api/boxes                       list + status
POST   /api/boxes                       create (streams build via SSE)
POST   /api/boxes/:id/{start,stop,destroy}
GET    /api/boxes/:id/events            query (filters) 
GET    /api/boxes/:id/stream            SSE live events
GET    /api/boxes/:id/flows             flow table
POST   /api/boxes/:id/flows/:fid/pcap   capture a flow
GET    /api/boxes/:id/behavior          behavior diff / run summary
GET/PUT /api/boxes/:id/policy           egress posture
GET    /api/boxes/:id/diff|commit|discard   overlay ops (v3)
WS     /api/boxes/:id/term              interactive terminal

GET    /api/labs, POST /api/labs/up, /down, /status
POST   /api/labs/:id/fault              inject/heal
GET    /api/labs/:id/topology           graph + live traffic (SSE)
GET    /metrics                         Prometheus
```

### 11.3 Agent↔collector protocol

Length-prefixed protobuf over vsock/unix socket; a control channel (collector→agent) pushes policy updates and capture requests; a data channel (agent→collector) streams events. Versioned; agent and host binary share a build hash.

---

## 12. Configuration Files (v4)

### 12.1 `devbox.toml` (per box) — additions over v3

```toml
[sandbox]
runtime = "auto"
image = "nixos"
mount_mode = "overlay"

[sets]                 # now also toggled from the web checklist
editor = true
git = true
network = false
# …

[observability]
enabled = true
capture = ["exec", "connect", "dns", "tls", "file"]  # "api" opt-in
api_capture = false            # SSL uprobe / MITM — off by default
retention = { max_size = "500MiB", max_age = "14d" }

[policy]
egress = "open"                # open | allowlist | mirror-only | isolated
allow  = ["github.com", "api.anthropic.com"]   # for allowlist mode
alert_on_violation = true
```

### 12.2 `lab.toml` — see §9.1. `~/.devbox/config.yaml` gains web `port`, default `egress`, default `capture`.

---

## 13. Language Seams & Tech Stack

| Layer | Language | Key deps | Ships as |
|---|---|---|---|
| Control plane + web console | **Rust** | axum, tokio, askama, rust-embed, rusqlite, serde, clap (existing) | the `devbox` binary |
| Web assets | — | htmx, xterm.js, minimal CSS (vendored) | embedded in binary |
| Observability agent | **Go** | cilium/ebpf, bpf2go, gopacket, vsock | `devbox-obsd`, embedded → pushed into guest |
| ZTP server | **Go** | net/http, chi (or std), prometheus/client_golang | `devbox-ztpd`, runs on ZTP node |
| Lab toolkit / IPAM / config-gen / test SDK | **Python** | pydantic, jinja2, pytest, prometheus-client | `devbox-lab` pkg, pushed into substrate / used host-side |

**Build integration.** A top-level build orchestrates all three: `cargo` builds the Rust binary; a `build.rs` (or `make`/`just` target) invokes `go build` for the agents (static, `CGO_ENABLED=0` where possible; eBPF objects via `bpf2go`) and packages the Python toolkit, then embeds the artifacts. CI builds and tests each language independently and then the integrated binary.

**eBPF portability.** CO-RE (BTF) targets a modern kernel; the NixOS guest image pins a kernel with BTF. On macOS the agent runs in the Lima Linux guest, so eBPF is available regardless of host OS. A `--no-ebpf` degraded mode (proc-polling + tap-based DNS/flow only) exists for kernels without BTF.

---

## 14. Project Structure (v4)

```
devbox/
├── Cargo.toml
├── build.rs                       # embeds go agents + python pkg + web assets
├── src/                           # RUST — control plane + web
│   ├── main.rs
│   ├── cli/                       # existing commands + web, lab, watch, policy, behavior
│   ├── runtime/                   # REUSED (incus, lima, multipass, docker)
│   ├── nix/                       # REUSED (sets, rebuild) + on-demand build driver
│   ├── sandbox/                   # REUSED (mod, overlay, provision, state, config)
│   ├── web/                       # NEW: axum server, askama templates, htmx handlers, SSE, ws term
│   │   ├── server.rs  routes.rs  sse.rs  term.rs
│   │   └── templates/  assets/    # embedded
│   ├── obs/                       # NEW: collector, sqlite store, correlation, behavior diff
│   ├── policy/                    # NEW: egress posture, nftables driver bridge
│   ├── lab/                       # NEW: orchestrator, topology, wiring, fault, scenarios
│   └── metrics.rs                 # NEW: prometheus exporter
│   # src/tui/  REMOVED
├── agent/                         # GO — devbox-obsd
│   ├── cmd/obsd/  bpf/  (bpf2go)  decode/  transport/  policy/
├── ztpd/                          # GO — devbox-ztpd
│   └── cmd/ztpd/  statemachine/  api/
├── labkit/                        # PYTHON — devbox-lab
│   ├── sot/ (pydantic models, IPAM)  gen/ (jinja templates)  sdk/ (pytest fixtures)  scenarios/
├── nix/                           # REUSED + guest kernel BTF pin + obsd service module
├── layouts/                       # REMOVED (or archived)
├── docs/
│   ├── plans/2026-08-06-devbox-v4-design.md   (this file)
│   ├── grafana/  quickstart-v4.md  observability.md  lab.md  ztp.md
└── tests/                         # rust integration + go + python e2e
```

---

## 15. Security Model (updated)

v4 keeps every v3 protection (read-only host mount, overlay, explicit commit, snapshots, VM boundary, no creds in guest) and adds:

- **Egress governance** — default-deny postures (§8) shrink the blast radius of a compromised box: data exfiltration and supply-chain callbacks are blocked, not merely observed.
- **Tamper-resistant option** — network/DNS/SNI capture can additionally run **host-side on the tap/vNIC**, where an in-guest attacker can't disable or blind it. In-guest agent = richer (pid correlation); host-side = harder to evade. Both can run; the UI marks which signals are tamper-resistant.
- **Credential proxy (stretch)** — secrets stay on the host; the box gets scoped, proxied access.
- **Console trust** — loopback bind + per-launch token; no remote exposure by default.
- **Audit** — the event store *is* an audit log; behavior diffs are exportable evidence.

The v4 one-liner: **"The AI can't touch your files — and everything it does reach, you can see and govern."**

---

## 16. Implementation Phases & Acceptance Criteria

Phases are ordered by dependency and value. Each is independently shippable and **gated**: no phase is "done" until its acceptance criteria pass *and* the full quality gate (§17) is green. This section is the contract the autonomous build loop follows.

### Phase 0 — Foundations
- Create `v4` branch; add `PROGRESS.md` and `DECISIONS.md` (ADR log); set up multi-language CI.
- Stand up the axum server behind `devbox web`: serves a Dashboard shell, an SSE heartbeat, opens the browser.
- **Accept:** `devbox web` opens a page listing existing boxes (read from v3 state); SSE tick visible; `cargo test/clippy/fmt` green; CI runs Rust+Go+Python lanes (even if Go/Python are stubs).

### Phase 1 — Web console: box management (retire TUI)
- Wire Dashboard + Box detail to the existing sandbox manager: list/status/start/stop/create/destroy.
- Browser terminal (xterm.js ↔ ws ↔ PTY).
- Remove `src/tui/`; drop Zellij from defaults; migrate cheat sheets into Help view.
- Lazy start on open.
- **Accept:** full box lifecycle from the browser; interactive shell works; TUI code gone; e2e test drives the API through a real box (Docker runtime in CI).

### Phase 2 — On-demand build / selective sets
- Create/edit form as a set/language/package checklist; compose `configuration.nix` from selections; run rebuild; stream progress via SSE.
- **Accept:** toggling a set in the UI rebuilds a box with exactly the selected sets; reflected in-box; progress streamed; unit tests for the nix-composition logic.

### Phase 3 — Observability capture (Go agent + eBPF)
- `agent/`: bpf2go programs for exec, connect/accept, DNS, file-open; decode + enrich; stream over vsock/unix socket.
- Embed + push + supervise `devbox-obsd` (NixOS service module).
- Rust `obs/` collector + SQLite store + query API.
- **Accept:** run a known command in a box, assert exec+dns+connect+file events captured and queryable; Go unit tests for decoders; integration test generating known activity asserts the correlated chain; documented overhead within budget.

### Phase 4 — Observability presentation + behavior diff
- Live stream, flow table, process tree, DNS log in the console (SSE + htmx); filters.
- Correlation view; `devbox behavior diff` + UI summary; JSONL/Markdown/pcap export.
- `/metrics` + Grafana dashboard JSON.
- **Accept:** console shows live correlated activity; behavior diff matches a scripted run; `/metrics` scrapeable; pcap export opens in Wireshark.

### Phase 5 — Egress & activity control
- Policy engine + nftables driver via agent; postures open/allowlist/mirror-only/isolated; DNS-driven allowlist; violations → events/metrics/notifications; UI policy editor.
- **(5b stretch)** interactive first-connection prompt; **(5c stretch)** credential proxy.
- **Accept:** in `allowlist`, disallowed egress blocked + flagged; `mirror-only` lets `pip/npm/nix` work but blocks arbitrary hosts; tests assert both; policy editable live.

### Phase 6 — Box lab: substrate & topology
- `lab/`: topology schema; substrate bring-up; veth/bridge wiring; IPAM address assignment; FRR/dnsmasq/chrony per role; `devbox lab up/down/status`.
- Web Lab view: topology graph + live traffic overlay (reuses obs plane).
- **Accept:** `devbox lab up clos-3node` → full reachability, BGP adjacencies up, graph shows live link traffic; reachability-matrix test passes.

### Phase 7 — Fault injection & scenario library
- Per-link netem (delay/jitter/loss/rate/partition/flap); scriptable + UI; scenario library incl. `fat-tree-4x2` + collective-traffic generator + straggler demo.
- **Accept:** inject partition → observe loss then reconverge on heal; straggler demo reproducibly shows throughput collapse and the obs plane flags the culprit link; scenarios load from library.

### Phase 8 — ZTP fabric + source-of-truth + config-gen (flagship)
- Python `labkit`: SoT/IPAM (pydantic), config-gen (Jinja, golden-tested, idempotent), pytest SDK.
- Go `ztpd`: HTTP server + provisioning state machine + Prometheus; dnsmasq options 66/67.
- `devbox lab up ztp-fabric`: blank → DHCP → fetch → apply → verify → healthy; chaos tests; SLOs + dashboard.
- **Accept:** N blank nodes reach healthy with zero manual steps; server-crash chaos test recovers idempotently; SLO assertions pass; test SDK examples (§10.4) green.

### Phase 9 — Polish, docs, examples
- Quickstart-v4, observability, lab, ZTP guides; screenshots/gifs; README rewrite; example scenarios; final integration pass; open PR.
- **Accept:** docs coherent; examples run from a clean checkout; PR from `v4` summarizes everything with test evidence.

> Realistic overnight expectation: Phases 0–4 (single-box glass box) are the high-probability completion set in one long autonomous run; 5–8 (control + lab + ZTP) proceed as far as time and gates allow, each landing shippable. The loop always leaves `v4` green.

---

## 17. Testing, CI & Quality Bar

- **Quality gate (every commit):** `cargo test && cargo clippy -- -D warnings && cargo fmt --check` (Rust); `go test ./... && go vet && golangci-lint run` (Go); `pytest && ruff check && mypy` (Python). No commit lands red.
- **Test layers:** Rust unit (nix composition, correlation, policy), Go unit (eBPF decoders with recorded ring-buffer fixtures), Python unit (IPAM allocation properties, config-gen golden files + idempotency), and **e2e** (Docker-runtime box in CI: create → run activity → assert events → policy → destroy; lab bring-up → reachability → fault → reconverge).
- **eBPF in CI:** a privileged Linux job with BTF loads the real programs against a scripted workload; decoder unit tests use captured fixtures so most Go tests need no privileges.
- **Property tests:** IPAM (no overlapping subnets, unique IPs), config-gen idempotency (apply twice = no diff).
- **Docs-as-tests:** quickstart commands run in CI.
- **Error handling & observability of the tool itself:** structured logging (`tracing`), clear actionable errors, the dropped-event counter, and `devbox doctor` extended to check kernel BTF, nftables, and vsock availability.

---

## 18. Open Questions & Future Work

1. **API-level capture default.** SSL uprobe vs transparent MITM — uprobe is cleaner (no cert install) but library-specific (OpenSSL first; Go/BoringSSL, rustls later). Ship uprobe-OpenSSL in Phase 4-stretch; MITM as opt-in.
2. **macOS lab density.** All lab nodes live in one Lima VM. Validate resource ceilings; document node-count guidance per host RAM.
3. **Multi-box correlation.** Should the timeline optionally span boxes (e.g., box A talks to lab node B)? Data model supports it; UI is future work.
4. **Windows.** Out of scope for v4 (WSL2 path is a future consideration).
5. **AI-assisted RCA (future, on-trend).** Expose the event store + configs to an agent (MCP server) that answers "why didn't the fabric converge?" — a natural capstone once the event stream exists.
6. **Persisted artifact/report.** A shareable HTML behavior report for a run (beyond Markdown/JSONL).

---

## 19. Appendix — CoreWeave JD Skill Mapping

This project is intentionally shaped to build and demonstrate the target Staff SWE (network automation) skill set. The mapping:

| JD requirement | Where v4 exercises it |
|---|---|
| Config generation | §10.2 Python/Jinja render→diff→apply→verify |
| Device provisioning / **ZTP** | §10 ZTP fabric, DHCP opt 66/67, Go state machine |
| Workflow automation / orchestration | §9 lab orchestrator, §10.3 chaos flows |
| Observability tooling | §7 eBPF glass-box, §7.7 Prometheus + Grafana |
| Internal CLIs and APIs | §6.4 CLI parity, §11.2 control-plane API |
| Python **and** Go | §13 Go agents/ZTP + Python labkit; Rust core alongside |
| CI/CD, well-tested software | §17 multi-language gate, e2e, property/golden tests |
| SLIs/SLOs, runtime health | §10.3 provisioning SLOs, `/metrics`, `devbox doctor` |
| Source-of-truth / IPAM / DCIM (NetBox) | §10.2 pydantic SoT + IPAM |
| Ansible/Jinja | §10.2 Jinja config-gen (Ansible-compatible inventory export = easy add) |
| Prometheus/Grafana | §7.7 exporter + dashboard JSON |
| Linux/Kubernetes | Linux substrate, namespaces, veth, nftables, eBPF (k8s operator = future) |
| Networking fundamentals (IP/subnet, routing, NTP, DNS) | §9 FRR routing, dnsmasq DNS/DHCP, chrony NTP, IPAM subnetting |
| RFCs / design docs others can use | this document + `DECISIONS.md` ADR log |

The honest interview narrative this enables: *"I hadn't operated production ZTP, so I built a lab that lets me chaos-test a zero-touch fabric end to end — here are the failure modes I hit (DHCP races, half-configured idempotency, boot-server trust) and how the design handles them."* Being able to reason about the trade-offs and failure modes is worth more than a line on a résumé.
