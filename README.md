# Devbox

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

**The sandbox that shows you what your coding agent did.**

Devbox runs Claude, Codex, or any other agent inside an isolated Linux VM where
your project is mounted read-only, and records the run: every file it wrote,
every host it reached, every process it spawned, every credential it used.

Then it hands you the receipt.

```console
$ devbox run --label "fetch a page and call an API" -- sh -c 'curl -s -o /dev/null https://example.com; curl -s "$DEVBOX_BROKER_URL/shot/v1/thing" -H "x-devbox-broker-token: $DEVBOX_BROKER_TOKEN"; echo hi > /workspace/shot.txt'
run 01M1SD2Z4F842DVZSBZXA2B688 · 682ms · exit 0 · finished
  files    1 changed (1 added, 0 modified, 0 deleted) · scope: run
  network  2 peers · 2 DNS · ↑2.1KB ↓6.4KB
  process  16 in the tree
  coverage full (ebpf+packet+netfilter) · 61 events · 0 dropped
  report   ~/.devbox/runs/devtest/01M1SD2Z4F842DVZSBZXA2B688/report.html
```

That is a real run on a real box, verbatim except for `$HOME`. The command is a
stand-in for an agent because the box it ran in has no agent installed; with
`claude` in the box, you would write `devbox run -- claude -p "…"` and read the
same five lines.

The last line of that summary is the point of the whole product. `coverage`
says how much of the run devbox actually saw — which capture backends attached,
how many events were attributed to this run, and how many were dropped. A tool
that reports on a sandbox and cannot report on its own blind spots is worse
than no tool at all.

---

## Quick start

### Install

```bash
curl -fsSL https://raw.githubusercontent.com/ethannortharc/devbox/main/install.sh | sh
```

Or build from source (requires Rust 1.89+ and Go 1.26+; Go builds the embedded
Linux observability agent):

```bash
git clone https://github.com/ethannortharc/devbox.git
cd devbox
cargo install --path .
```

You also need a VM runtime: [Lima](https://lima-vm.io/) on macOS,
[Incus](https://linuxcontainers.org/incus/) on Linux. Run `devbox doctor` to
check.

### Make a box for this project

```bash
cd my-project
devbox
```

Devbox detects the project type, provisions a NixOS VM with
[120+ tools](docs/PACKAGES.md), and opens the local web console on that box.
Your project directory is mounted **read-only**; everything the box writes goes
to an overlay layer that never touches your files until you say so.

### Give the agent a credential without giving it the key

```bash
devbox secret set anthropic --from-env ANTHROPIC_API_KEY
```

The value goes into the host keychain. It is never written into the box, into
its environment, or into any file the box can read. A host-side broker holds it
and injects it per request; the box gets a per-box token instead, and every use
lands in the run's report and in `devbox watch --type credential`.

### Run the agent

```bash
devbox run -- claude
```

`devbox run` takes a checkpoint, wires the broker environment into the
command's own argv, runs it — attached to your terminal when you have one — and
closes the run with a second checkpoint. The five-line summary appears when it
finishes.

### Read the report

```bash
devbox runs                        # every recorded run on this box
devbox report <RUN_ID>             # the Markdown rendering
devbox report <RUN_ID> --open      # the HTML one, in a browser
```

The console shows the same report at `/boxes/<box>/runs/<run>` — one model,
three renderings, so the terminal, the file on disk and the page cannot
disagree.

---

## What you get

![The run report page in the devbox console: run identity and coverage, the file the run changed under scope run, the two peers it reached with byte counts, and the brokered credential it used](docs/screenshot-console.png)

That is the report for the run at the top of this page. The rule across the
middle is an elision: the Processes section sits between Network and
Credentials and is left out here, because on a box with a broker configured it
contains that box's broker token.

Every run produces one report with these sections.

| Section | What it answers |
|---|---|
| **Files** | What this run wrote, from the diff of the checkpoint taken before it against the checkpoint taken after — not the box's lifetime of changes. The report says which scope it used. A second table, **Writes outside the workspace overlay**, names the directories the run wrote to that `devbox commit` would never sync — a package cache, a config file in `$HOME` — by directory, with opens and file counts. |
| **Network** | One row per peer: connections, ports, whether TLS was seen, bytes each way, and how long the connection lasted. Bytes are settled at `close`, so an open connection reads 0 rather than a guess. TLS server names and DNS answers are listed under it. |
| **Processes** | The process tree the run spawned, by pid. Devbox's own wrapper collapses to a single `[devbox wrapper]` row — folded, not dropped, because a tree that quietly lost three processes would not line up with `devbox watch --tree`. |
| **Credential use** | Which brokered credential the run reached for, the upstream it went to, the methods, how many times, and when it was last used. A refusal is counted and shown — `2 (1 denied)` — because a line where every use was denied is the policy working, not the credential being used. |
| **Coverage** | Which capture backends were live, the agent version, how many events were attributed to this run and by which rule, how many were dropped, and how many events in the same window belonged to something else. |

Read them together and `devbox behavior diff` answers "what did this run do
that the last one didn't?" the same way `devbox diff` answers "what files
changed?"

---

## How it works

### Files: an overlay, and checkpoints inside it

```
Host filesystem ──(read-only)──> /mnt/host (lower layer)
                                      │
                                      ▼
                                OverlayFS merge ──> /workspace (what you see)
                                      ▲
                                      │
                        /var/devbox/overlay/upper (the box's changes)
```

Nothing reaches your real files until you run `devbox commit`. `devbox discard`
throws the upper layer away.

A **checkpoint** is a copy of that upper layer, not a new overlay level — the
kernel supports a fixed number of layers and stacking one per run would run
out. `devbox run` takes one before and one after, and the report's Files
section is the diff between them. `devbox layer restore <ID>` puts the upper
layer back to a checkpoint and remounts the overlay, because replacing the
upper without a remount leaves processes reading stale content.

```bash
devbox layer checkpoint --label before-refactor
devbox layer checkpoints
devbox layer diff --from 01m1s3zz          # against the box now
devbox layer restore 01m1s3zz
```

### Behaviour: an eBPF agent, and an honest coverage line

An agent (`devbox-obsd`) runs inside the box and streams events to a per-user
collector on the host, which writes a SQLite store per box.

Where the kernel has BTF, the agent attaches eBPF probes and sees execs,
connects, accepts, closes, and file opens at the syscall boundary. Where it
does not, it polls `/proc` and keeps an independent packet tap for DNS and TLS
— which loses process attribution and file events, and says so.

Two things worth knowing before you trust a timeline:

- **The CO-RE objects are committed per architecture** (`agent/bpf/devbox_<arch>_bpfel.{go,o}`).
  A source build embeds the eBPF agent when the object for the guest
  architecture is present, and prints a `cargo:warning` naming the fallback
  when it is not. Release builds always embed it.
- **`devbox doctor` tells you which one you got**, per running box:

  ```
  devtest (lima):
    kernel: 6.19.0
    btf: ready
    agent: devbox-obsd 0.2.0 (<commit>)
    agent binary: matches host embed
    capture: ebpf+packet+netfilter
    file scope: /workspace, /home
  ```

  `agent binary:` compares the content hash of the agent in the box against the
  one this devbox ships — versions are equal far too often to be a useful test.
  `file scope:` is the set of path prefixes the agent exports file events for;
  it rides in the handshake, so the report can say what it was watching.

### Credentials: a broker, not a copy

`devbox secret set` puts the value in the host keychain (macOS Keychain; on
Linux `~/.devbox/secrets/<provider>`, mode 0600). A host process,
`devbox __broker`, is a **per-service reverse proxy**: the box speaks plain
HTTP to it, and the broker opens TLS to the real upstream and injects the
credential. There is no CA in the box and no TLS interception.

| Provider | Upstream | The box sees | Injected |
|---|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `ANTHROPIC_BASE_URL=http://<broker>/anthropic`, `ANTHROPIC_AUTH_TOKEN=<box token>` | `Authorization: Bearer` or `x-api-key`, per the stored secret |
| `openai` | `https://api.openai.com` | `OPENAI_BASE_URL=http://<broker>/openai/v1`, `OPENAI_API_KEY=<box token>` | `Authorization: Bearer` |
| `github` | `https://api.github.com`, `https://github.com` | git only: `url."http://<broker>/github/".insteadOf "https://github.com/"` in the guest gitconfig | `Authorization: Bearer` |
| `http:<name>` | whatever `--url` says | `DEVBOX_SECRET_<NAME>_URL` | the header `--header` names |

The box token is per box, rotated at box start, and passed only in the
environment of sessions devbox itself starts. Holding it grants brokered,
scoped, logged access and nothing else.

```bash
devbox secret ls                                   # names and backends, never values
devbox secret scope github --repo owner/name       # what this credential may reach
devbox broker reach mybox                          # how this box gets to the broker
```

**`gh` is not brokered.** It forces HTTPS and `GH_HOST` does not accept a
scheme, so pointing it at a plaintext broker fails at the TLS handshake.
Supporting it would mean a TLS listener and a trust anchor inside the box,
which is the thing this design refuses. `git` over smart HTTP works.

### Egress: a posture, for the box or for one run

```bash
devbox policy set mirror-only          # for the box
devbox run --posture isolated -- ./build.sh   # for this run only, then back
```

| Posture | Behaviour |
|---|---|
| `open` | Nothing blocked. With an allowlist set, out-of-policy connections are flagged. |
| `allowlist` | Default-deny. Only listed domains and CIDRs. A bare `github.com` covers its subdomains; `evilgithub.com` is not one. |
| `mirror-only` | Package mirrors and git hosts only. `pip`, `npm`, `cargo`, `nix` work; nothing phones home. |
| `isolated` | No egress. Loopback only. The broker is blocked too — an isolated run has no credentials, which is the point. |

Postures compile to live nftables rules inside the box, with the allow set kept
in sync from the DNS the agent is already capturing.

### MCP servers: inside a box, as a run

An MCP server is code someone else wrote, launched by your agent, on your
machine, with your rights. `devbox mcp` moves it into a box.

```bash
devbox mcp add fetch --box mcp-tools -- uvx mcp-server-fetch
devbox mcp ls
claude mcp add fetch -- devbox mcp run fetch
devbox mcp report fetch                   # what its last session did
```

`devbox mcp run` is a byte-exact stdio shim: it hands the agent's JSON-RPC
through to the server in the box and back, with no parsing and no line
buffering. The server's stderr goes to `~/.devbox/mcp/<name>.log` so it cannot
corrupt the protocol stream. `--posture` holds an egress posture for the
duration and restores the box's own afterwards. Registration edits
`devbox.toml` as text, so your comments survive; `--global` writes
`~/.devbox/mcp.toml` instead, for servers you want from any directory.

**A session is a run.** `mcp run` opens a run of `kind = mcp` labelled with the
server's name, so an MCP server gets the same evidence a `devbox run` does — two
checkpoints, an attributed event stream, a coverage line — and `devbox mcp report
<name>` renders the most recent one. Because an agent's clean shutdown and a
forced stop both exit 143, the run also records **how** it ended (`exit`,
`signal`, `stdin-eof`, or `forced`), which is the more useful fact for a server
that ran for hours.

### And devbox as an MCP server

```bash
devbox mcp self
claude mcp add devbox -- devbox mcp self
```

The other direction: `devbox mcp self` is a stdio JSON-RPC server that hands the
agent four read-only tools — `list_runs`, `run_report`, `behavior_summary` and
`watch` — so it can ask what a box has been doing instead of being told. It is
read-only, starts no box, and reports a tool-level failure as `isError` rather
than as a protocol error, because a client reads a protocol error as "this
server is broken" and stops asking.

### Export: the same events, in someone else's schema

```bash
devbox export --run 01M1SD2Z4F842DVZSBZXA2B688 --format ocsf
devbox export --from 2026-09-05T14:00:00Z --format otlp-json --out events.json
devbox export --format jsonl
```

`ocsf` emits OCSF 1.3 — Process Activity, File System Activity, Network
Activity, DNS Activity, HTTP Activity, Detection Finding and API Activity — one
JSON object per line, every class validated against the OCSF schema server.
A brokered credential use is API Activity 6003, carrying the provider, the
upstream host and path, the verdict, and the run it belongs to on
`actor.session.uid`; the credential itself is never in the record, because the
broker is the only thing that ever held it. `otlp-json` emits one OTLP/JSON
`ExportLogsServiceRequest`, accepted by a stock OpenTelemetry Collector.
`jsonl` is devbox's own event, unchanged.

An event kind with no honest mapping is counted as unmapped and skipped rather
than filed under a nearby class, and the export says which kinds it dropped:

```
Exported 60 of 61 event(s) as ocsf to run.ocsf.jsonl (scanned 61).
warning: 1 event(s) have no ocsf class in this build and were not written:
syscall=1. Use --format jsonl for the complete record.
```

`syscall` is the one kind in that position today: OCSF has no class for it, and
filing it under a neighbouring one would put a claim into an audit record that
nothing observed. `jsonl` carries everything. An export whose counts do not
balance (`matched == written + unmapped`) fails rather than printing a
plausible-looking partial record.

---

## Commands

Every command that acts on an existing box takes the box name as an **optional
first positional**: `devbox status devtest`, or plain `devbox status` for the
box registered to the current directory. Where a command has a positional of
its own, the box name comes second — `devbox snapshot save nightly devtest`,
`devbox layer restore 01kfx9 devtest`.

| Command | What it does |
|---|---|
| `devbox` | Ensure a box for this project, then open its console page |
| `devbox create` | Create a new sandbox |
| `devbox shell` | Attach a terminal |
| `devbox exec -- <cmd>` | Run a one-off command in the box |
| `devbox run -- <cmd>` | Run a command and record what it did |
| `devbox runs` | List a box's recorded runs |
| `devbox report <RUN_ID>` | Print a run's report (`--format md\|json\|html`, `--open`) |
| `devbox stop` / `destroy` / `prune` | Stop one box, remove one, or remove all stopped ones |
| `devbox list` / `status` | Every sandbox, or one in detail |
| `devbox diff` / `commit` / `discard` | Review, accept, or throw away the overlay's changes |
| `devbox layer status` | Overlay layer summary |
| `devbox layer refresh` | Pick up host-side file changes |
| `devbox layer conflicts` | Files modified on both sides |
| `devbox layer stash` / `stash-pop` | Set the overlay aside and bring it back |
| `devbox layer checkpoint` | Save the upper layer as a checkpoint (`--label`) |
| `devbox layer checkpoints` | List checkpoints |
| `devbox layer diff --from <ID>` | Diff a checkpoint against the box now, or `--to` another |
| `devbox layer restore <ID>` | Put the overlay back to a checkpoint |
| `devbox layer checkpoint-rm <ID>` | Delete a checkpoint (`--force` for one a run's report cites) |
| `devbox snapshot save` / `restore` / `list` | Whole-VM snapshots |
| `devbox watch` | Query captured activity (`--type`, `--peer`, `--path`, `--tree`, `--json`) |
| `devbox behavior summary` / `diff` / `pcap` | Summarize, compare, or capture a real flow pcap |
| `devbox policy show` / `set` / `allow` / `test` / `rules` | Read and enforce egress posture |
| `devbox secret set` / `ls` / `rm` / `scope` | The credentials the broker holds |
| `devbox broker status` / `start` / `stop` / `reach` | The host-side broker |
| `devbox mcp add` / `run` / `ls` / `rm` | MCP servers that run inside a box |
| `devbox mcp report <name>` | The report for that server's most recent session |
| `devbox mcp self` | Run devbox's own MCP server: runs and events, as tools |
| `devbox export --format <fmt>` | Export events as OCSF, OTLP/JSON, or JSON Lines |
| `devbox web` | Start the local web console without touching a box |
| `devbox code` | Open VS Code / Cursor into a box via Remote SSH |
| `devbox use <name>` | Point an existing box at the current directory |
| `devbox upgrade --tools <set>` | Add tools to a running box |
| `devbox sets list` / `apply` | Inspect or rebuild the declarative set selection |
| `devbox nix add` / `remove` | Add or remove a nixpkgs package |
| `devbox init` | Generate `devbox.toml` |
| `devbox config set` / `get` / `show` | Global defaults |
| `devbox guide [tool]` | Built-in cheat sheets |
| `devbox doctor` | Diagnose the host, the runtimes, and each running box |
| `devbox reprovision` | Re-push configs and rebuild |
| `devbox self-update` | Update the devbox binary |

The pre-v5 `--name <NAME>` spelling still works everywhere it used to, but no
longer appears in `--help`. `devbox policy allow` and `devbox report` keep a
visible `--name`: the first because its list of entries leaves no room for
another positional, the second because omitting it means "search every box for
this run".

---

## Security model

| Layer | v4 | v5 |
|---|---|---|
| **Files** | overlay, explicit commit | + per-run checkpoints; restore to any checkpoint |
| **Behaviour** | box-wide audit | + per-run attribution, coverage stated in every report |
| **Egress** | posture per box | + posture per run; the broker is the only credentialled path |
| **Credentials** | copied into the guest | **never in the guest**; scoped, logged broker |
| **Tool servers** | run on the host with full rights | run in a box, as a run |
| **Integrity of the record** | — | reports carry event counts, dropped counts, and attribution counts |

Underneath, unchanged from v4: a full VM boundary rather than a container, a
host project directory mounted read-only, `--writable` as an explicit opt-in,
and NixOS generations for whole-system rollback.

```bash
devbox diff                      # review the overlay
devbox commit --path src/        # accept selectively
devbox discard                   # throw it all away
devbox snapshot restore <snapshot>   # roll the whole VM back
```

### Overlay layer lifecycle

| Command | Direction | What it does |
|---|---|---|
| `devbox layer refresh` | Host → VM (read) | Re-read host changes; your edits preserved. Clears stale file handles. |
| `devbox layer conflicts` | — | Files modified on both host and box sides. |
| `devbox diff` | — | What is in the upper layer vs the lower layer. |
| `devbox commit` | VM → Host (write) | Copy upper-layer changes to the host. The **only** operation that writes to the host. |
| `devbox discard` | — | Wipe the upper layer, then remount so the next read is the truth. |
| `devbox layer stash` | — | Save the upper layer aside for later. |

**On `devbox layer refresh`** (re-read host files):

| Your box (upper) | Host (lower) | After refresh |
|---|---|---|
| Didn't touch the file | Host updated it | You see the new host version |
| You edited the file | Host didn't change | Your edit is preserved |
| You edited the file | Host also changed | **Your edit wins** (upper always overrides lower) |
| You deleted the file | Host didn't change | File stays deleted |
| You deleted the file | Host also changed | File stays deleted (your whiteout wins) |
| Didn't touch the file | Host deleted it | File disappears |
| You created a new file | — | Your new file is preserved |
| — | Host added a new file | You see the new file |

**On `devbox commit`** (sync your changes to the host):

| Your box (upper) | Host (lower) | After commit |
|---|---|---|
| You edited a file | Host didn't change | Host gets your version |
| You edited a file | Host also changed | **Host is overwritten** with your version |
| You created a new file | File doesn't exist on host | File is created on host |
| You deleted a file | File exists on host | File is deleted on host |
| Didn't touch the file | — | No change (not in the upper layer) |

> **Key rule:** `refresh` never loses your work (upper always wins in the
> merge). `commit` always overwrites the host with your version. Run
> `devbox layer conflicts` before either to see what overlaps.

---

## Local web console

The console binds loopback only and gives every launch a random
`devbox-….localhost` browser origin. A one-time URL token installs a key in
that origin's storage and requests send it explicitly as `x-devbox-key`; it is
never a cookie or a navigable URL credential. `Host`, origin, and framing
guards protect it.

| View | What it does |
|---|---|
| **Boxes** | Every box, live status, start/stop/destroy inline, plus **New box**. |
| **Overview** | Runtime, image, mount mode, sets, project directory. |
| **Activity** | Live event stream, peers rollup, flow table, DNS log, process tree, file writes, policy refusals. |
| **Runs** | Every recorded run, with its coverage badge; each row opens its report. |
| **Sets** | A checklist of Nix sets. Only what is checked gets built. |
| **Policy** | Egress posture and allowlist, editable live. |
| **Files** | Overlay changes — what the box wrote, before you commit it. |
| **Terminal** | A real shell, over a real pty, with the same broker variables `devbox shell` gets. |
| **Guides** | The cheat sheets, rendered in the browser. |

Open as many tabs as you need: typing the bound loopback address shown by
`devbox web` (`http://127.0.0.1:7878` by default) redirects each tab to the
current private origin. Browser profiles do not share credentials, so open the
launch URL once in each profile before using its bare address.

---

## Configuration

### Project-level (`devbox.toml`)

Generated with `devbox init`, auto-detects your project settings.

```toml
[sandbox]
runtime = "auto"            # auto | lima | incus | docker (explicit limited mode)
image = "nixos"             # nixos | ubuntu
mount_mode = "overlay"      # overlay (safe) | writable (direct)

[sets]
editor = true               # neovim, helix, nano
git = true                  # git, lazygit, gh
container = false           # docker, compose, lazydocker
network = false             # network diagnostics
ai_code = true              # claude-code (npm), codex, aider, aichat, ...
ai_infra = false            # ollama, open-webui

[languages]
go = true                   # auto-detected from go.mod
rust = false
python = false
node = false

[resources]
cpu = 4
memory = "8GiB"

[policy]
egress = "mirror-only"      # open | allowlist | mirror-only | isolated
allow = ["github.com"]
alert_on_violation = true

[mcp.fetch]                 # written by `devbox mcp add`
command = ["uvx", "mcp-server-fetch"]
box = "mcp-tools"
posture = "mirror-only"
```

### Global defaults

```bash
devbox config set runtime lima
devbox config show
```

---

## Base images and runtimes

Both images install the same [120+ tools](docs/PACKAGES.md) from
[nixpkgs](https://search.nixos.org/packages).

| Image | Method | Rollback | Best for |
|---|---|---|---|
| **nixos** (default) | `nixos-rebuild switch` | Full system generations | Reproducible, declarative environments |
| **ubuntu** | Nix package manager | `nix profile rollback` | A familiar base OS |

Devbox auto-detects only runtimes that implement the default NixOS protected
overlay contract. Restricted runtimes must be selected explicitly.

| Runtime | Platform | New-box support |
|---|---|---|
| Incus | Linux | Auto-detected; NixOS overlay or Ubuntu writable |
| Lima | macOS | Auto-detected; NixOS overlay or Ubuntu writable |
| Docker | Any | Explicit only: `--runtime docker --image ubuntu --writable --bare`. No protected OverlayFS, and no eBPF — it shares your kernel. |

A fourth backend, Multipass, is still carried for boxes registered under older
versions; creating new ones is disabled and `devbox run` does not work on it.

---

## Tool catalog

Devbox ships with [**120+ tools**](docs/PACKAGES.md) organized into toggleable
sets, all from [nixpkgs](https://search.nixos.org/packages). See the
[full package reference](docs/PACKAGES.md) for every tool.

### Core sets (always installed)

<details>
<summary><b>system</b> -- 24 packages</summary>

coreutils, gnugrep, gnused, gawk, findutils, diffutils, gzip, gnutar, xz, bzip2, file, which, tree, less, curl, wget, openssh, openssl, cacert, gnupg, gcc, gnumake, pkg-config, man-db

</details>

<details>
<summary><b>shell</b> -- 11 packages</summary>

| Package | Description |
|---------|-------------|
| zsh | Z shell with advanced scripting |
| zsh-autosuggestions | Fish-like autosuggestions for zsh |
| zsh-syntax-highlighting | Syntax highlighting for zsh |
| starship | Cross-shell prompt |
| fzf | Fuzzy finder |
| zoxide | Smart cd (remembers directories) |
| direnv | Per-directory environment variables |
| nix-direnv | Nix integration for direnv |
| yazi | Terminal file manager |
| micro | Simple terminal editor |

</details>

<details>
<summary><b>tools</b> -- 22 packages</summary>

| Package | Description |
|---------|-------------|
| ripgrep | Fast regex search (replaces grep) |
| fd | Fast file finder (replaces find) |
| bat | Syntax-highlighted cat |
| eza | Modern ls with icons |
| delta | Git diff viewer |
| sd | Regex find-and-replace |
| choose | Field selection (replaces cut/awk) |
| jq | JSON processor |
| yq-go | YAML/TOML/XML processor |
| fx | Interactive JSON viewer |
| htop | Interactive process viewer |
| bottom | System monitor (btm) |
| procs | Modern ps |
| dust | Disk usage analyzer |
| duf | Disk usage overview |
| tokei | Code statistics |
| hyperfine | Command benchmarking |
| tealdeer | Simplified man pages (tldr) |
| httpie | HTTP client |
| dog | DNS client |
| glow | Markdown renderer |
| entr | File watcher |

</details>

<details>
<summary><b>editor</b> -- neovim, helix, nano</summary>

Three terminal editors covering different preferences. Neovim for power users, Helix for modal editing with LSP built-in, Nano for quick edits. `vim` and `vi` are aliased to `nvim`.

</details>

### Default sets (on by default)

<details>
<summary><b>git</b> -- 6 packages</summary>

git, lazygit (TUI), gh (GitHub CLI), git-lfs, git-crypt, pre-commit

</details>

<details>
<summary><b>ai-code</b> -- 5 packages (AI coding assistants)</summary>

codex, opencode, aider-chat, aichat, continue — plus the latest claude-code,
installed via npm during provisioning

</details>

### Optional sets (off by default)

<details>
<summary><b>container</b> -- 6 packages</summary>

docker, docker-compose, lazydocker (TUI), dive (image analyzer), buildkit, skopeo

</details>

<details>
<summary><b>network</b> -- 13 packages</summary>

frr, dnsmasq, chrony, busybox, iproute2, conntrack-tools, tailscale, mosh,
nmap, tcpdump, bandwhich, trippy, doggo

</details>

<details>
<summary><b>ai-infra</b> -- 5 packages (local AI inference)</summary>

ollama, open-webui, litellm, mcp-hub, huggingface-hub

</details>

### Language sets (auto-detected or `--tools`)

| Language | Detection | Packages |
|---|---|---|
| **Go** | `go.mod` | go, gopls, golangci-lint, delve, gotools, gore |
| **Rust** | `Cargo.toml` | rustup, rust-analyzer, cargo-watch, cargo-edit, cargo-expand, sccache |
| **Python** | `pyproject.toml`, `requirements.txt` | python 3.12, uv, ruff, pyright, ipython, pytest |
| **Node.js** | `package.json` | node 22, bun, pnpm, typescript, ts-language-server, biome |
| **Java** | `pom.xml`, `build.gradle` | jdk 21, gradle, maven, jdt-language-server |
| **Ruby** | `Gemfile` | ruby 3.3, bundler, solargraph, rubocop |

---

## IDE integration

Use your local VS Code, Cursor, or Windsurf to edit code inside the box — full
IntelliSense, extensions, and debugging, all running in the isolated VM.

```bash
devbox code                       # Open VS Code into the box
devbox code --editor cursor       # Use Cursor instead
devbox code myapp                 # Open a specific box
devbox code --path /workspace/src # Open a specific directory
```

Devbox configures `~/.ssh/config` for the box, refreshes the overlay layer, and
launches the editor with Remote SSH pointed at `/workspace`. Works with any
editor that supports [Remote SSH](https://code.visualstudio.com/docs/remote/ssh).

> **NixOS compatibility:** devbox enables `nix-ld` in the VM so VS Code Server
> and other dynamically linked binaries run without issues.
>
> **Known gap:** `devbox code` does not inject the broker environment. The
> editor's remote server is started by VS Code, not by devbox, so there is no
> guest command line to wrap. Use `devbox shell` or `devbox run` for anything
> that needs a brokered credential.

---

## Remote access via SSH

Devbox VMs run a full SSH server, so they are reachable from any machine on
your network — useful for headless servers and remote development.

```bash
# SSH into a box directly (Lima)
ssh -p $(limactl show-ssh --format=port devbox-myapp) $(whoami)@localhost

# Or use Lima's built-in shortcut
limactl shell devbox-myapp

# Incus VMs
incus exec devbox-myapp -- bash
```

**SSH agent forwarding** is enabled by default on Lima, so your host SSH keys
work inside the box without copying them.

**Port forwarding** for web development:

```bash
ssh -L 3000:localhost:3000 -p $(limactl show-ssh --format=port devbox-myapp) $(whoami)@localhost
```

**Remote team workflow:**

```bash
# On the server
devbox create --name shared-api --tools go,docker

# From your laptop
ssh yourserver -t "devbox shell shared-api"
```

---

## Architecture

```
devbox (single binary)
  |
  |-- CLI + local web control plane
  |     one lifecycle, policy, observability, run and terminal API
  |
  |-- Sandbox Manager
  |     Lifecycle: create -> start -> attach -> stop -> destroy
  |     State at ~/.devbox/sandboxes/, overlay diff/commit/discard, checkpoints
  |
  |-- Runtime Abstraction
  |     Trait-based backends (Lima, Incus, Multipass, Docker)
  |     Auto-detection with priority scoring; uniform exec/start/stop/status
  |
  |-- NixOS Provisioning
  |     All .nix files embedded in the binary (include_str!)
  |     Declarative package management via nixos-rebuild
  |
  |-- Observability + control
  |     embedded devbox-obsd, background collector, per-box SQLite
  |     eBPF/proc capture, run attribution, behavior diff, pcap, nftables policy
  |
  |-- Credential broker
        host keychain, per-service reverse proxy, per-box tokens, scopes, audit
```

### Provisioning flow

1. The VM runtime creates and boots a NixOS (or Ubuntu) image
2. Devbox pushes `.nix` config files into the VM at `/etc/devbox/`
3. The NixOS module is imported into the VM's system configuration
4. `nixos-rebuild switch` installs all declared packages from the binary cache
5. The matching observability agent and the saved egress policy are installed
6. Box state is saved to `~/.devbox/sandboxes/<name>/` and the background
   collector begins supervising it

---

## Documentation

- [Quickstart](docs/quickstart-v5.md) — the v5 tour, one page
- [Observability](docs/observability.md) — what is captured, how to read it
- [Package reference](docs/PACKAGES.md) — every tool in every set
- [E2E test guide](docs/E2E_TEST_GUIDE.md) — manual lifecycle verification
- [Decisions](DECISIONS.md) — the architecture decision log

---

## Development

```bash
cargo build --release          # build
cargo test                     # all Rust units and integrations
go test ./...                  # the Go agent
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Contributing

Contributions are welcome. Please open an issue to discuss significant changes
before submitting a pull request.

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Write tests for your changes
4. Ensure `cargo test` and `cargo clippy` pass
5. Submit a pull request

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for
details.

© 2026 Ethan H.B. Zhou
