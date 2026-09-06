# Quickstart — devbox v5

v4 made a box you could watch. v5 makes a **run** you can hand to someone: one
command, one report, and a line at the bottom saying how much of it devbox
actually saw.

## Install and make a box

```bash
cargo build --release            # or: curl -fsSL …/install.sh | sh
cd my-project
devbox                           # ensure a box for this directory, open the console
```

`devbox` with no arguments creates a box for the current project if it does not
have one, then opens the console on that box's page. The URL it prints carries
a per-launch token; the console binds `127.0.0.1` only and refuses any request
whose `Host` header is not a loopback name.

Console without touching a box:

```bash
devbox web --port 8080 --no-open
```

## Record a run

```bash
devbox run -- claude
```

Everything about that command is recorded and rendered:

```
run 01M1SEWB66PS95BP62WTHJJF11 · 666ms · exit 0 · finished
  files    1 changed (1 added, 0 modified, 0 deleted) · scope: run
  network  2 peers · 2 DNS · ↑2.1KB ↓6.4KB
  process  9 in the tree
  coverage full (ebpf+packet+netfilter) · 53 events · 0 dropped
  report   ~/.devbox/runs/devtest/01M1SEWB66PS95BP62WTHJJF11/report.html
```

Useful flags:

| Flag | Effect |
|---|---|
| `--label "…"` | A name for the run, shown in `devbox runs` and at the top of the report |
| `--posture isolated` | Hold an egress posture for this run only, then restore the box's own |
| `--cwd /workspace/sub` | Working directory inside the box (default `/workspace`) |
| `--no-report` | Record the run, skip rendering |
| `--open` | Open the HTML report when it finishes |

Then:

```bash
devbox runs                          # every run on this box
devbox report <RUN_ID>               # Markdown
devbox report <RUN_ID> --format json # the model itself
devbox report <RUN_ID> --open        # HTML, in a browser
```

The report lands at `~/.devbox/runs/<box>/<run>/report.{md,json,html}`, mode
0600 — it names every host the command reached and every file it touched. The
console renders the same model at `/boxes/<box>/runs/<run>`.

`exec` and `shell` are recorded as runs too (`kind = exec`, `kind = shell`),
but do not render a report. They have no wrapper, so only the time-window rule
can attribute events to them — see [observability.md](observability.md#run-attribution).

## What the report says, and what it admits

| Section | Reads |
|---|---|
| Files | The diff of the checkpoint taken before the run against the one taken after. `scope: run` means exactly that; `scope: box` would mean the whole box's overlay. |
| Writes outside the workspace overlay | Directories the run wrote to that `devbox commit` would never sync — `~/.cache/uv`, `/etc` — by directory, with opens and file counts. The overlay only protects `/workspace`; this is the part of the run that escaped it. |
| Network | One row per peer — connections, ports, TLS seen, bytes each way, duration. Plus TLS server names and DNS answers. |
| Processes | The process tree, by pid. Devbox's own wrapper is folded into one `[devbox wrapper]` row, so the root of the tree is your command. Credentials that reached a command line read `***` — see below. |
| Credentials | Each brokered credential the run reached for: provider, upstream, methods, uses, last use. `2 (1 denied)` means the scope refused one of them. |
| Coverage | Capture backends, agent version, events attributed and by which rule, events dropped, and events in the window that belonged to something else. Plus, if capture was disturbed mid-run, one of two lines: **capture was interrupted** (a warning — the agent changed after this run had already recorded something; it names the window it cannot vouch for, and `report.json` carries it as `capture_gap`) or *capture re-attached* (no warning — same agent, nothing lost). |

The outside-overlay table is the one worth reading twice. `devbox discard`
undoes `/workspace`; it does not undo a package the run installed into `$HOME`,
and until v5 the report did not say that had happened. Its `opens` column counts
capture events (the probe sees `open`/`create`, not each `write(2)`), so a 64 KB
`dd` into one file counts as one.

**A report can be handed to someone.** Devbox has to pass the broker's
variables on the command line, because that is the only form every runtime's
exec accepts — so the box's own token would otherwise be sitting in the process
tree of every report. It is redacted by variable name (`*_TOKEN`, `*_SECRET`,
`*_KEY`, `*_CREDENTIALS`, anything with `PASSWORD`, and the `Authorization` /
`Cookie` family of headers) in the agent before the event is sent, again in the
collector on arrival, and again in the store on the way out so that events
recorded before any of it existed are covered too. The value becomes `***`.
It is not a containment boundary — the box can read `/proc/*/cmdline` — it is
what makes the receipt shareable. See
[observability.md](observability.md#secrets-that-reach-an-argv).

Two numbers surprise people the first time:

- **`unattributed in the window`** counts events from *other* activity in the
  same box during the run. On a shared box it is large; on a box doing one
  thing it is small. It is not a defect, it is the cost of sharing a box, shown.
- **Byte counts are settled at connection close.** A connection still open when
  the run ends reads `0B` — the kernel counters are read at `tcp_close` and
  nowhere else, because that is the only place they are true.

## Checkpoints

A checkpoint is a copy of the overlay's upper layer. `devbox run` brackets each
run with two, which is where the report's Files section comes from.

```bash
devbox layer checkpoint --label before-refactor
devbox layer checkpoints
devbox layer diff --from 01m1s3zz              # checkpoint vs the box now
devbox layer diff --from 01m1s3zz --to 01m1s40 # checkpoint vs checkpoint
devbox layer restore 01m1s3zz                  # put the overlay back
devbox layer checkpoint-rm 01m1s3zz            # delete one
devbox layer prune --dry-run                   # say what would go
devbox layer prune --runs-older-than 7d        # let old runs' checkpoints go too
```

An id can be shortened to any unique prefix. The newest 20 are kept; the ones a
run's report cites are never pruned, and `checkpoint-rm` refuses them without
`--force`.

`layer prune` is how the pinned ones are eventually released. On its own it
only touches unclaimed checkpoints (`--keep` sets how many survive).
`--runs-older-than` also drops the pair a run took, but only once that run has
ended, ended longer ago than the age you gave, **and** had its report written —
a checkpoint is that report's evidence, and evidence is not something to drop
because a timer expired.

`restore` refuses while a run is still going — rewriting the upper layer out
from under a run would invalidate the evidence the report is about to cite.

## Credentials that never enter the box

```bash
devbox secret set anthropic --from-env ANTHROPIC_API_KEY
devbox secret ls                                # names and backends, never values
devbox secret scope github --repo owner/name    # what this credential may reach
devbox broker status
devbox broker reach mybox                       # how this box gets to the broker
```

The value goes to the host keychain. A host-side broker proxies each request to
the real upstream over TLS and injects the credential there; the box only ever
holds a per-box token that is rotated at box start. Every brokered request is a
`credential` event: provider, method, host, path (query stripped), upstream
status, bytes, verdict.

```bash
devbox watch --type credential
```

Two things this does *not* do:

- **No TLS interception.** There is no CA in the box. That is why `gh` cannot be
  brokered — it forces HTTPS and `GH_HOST` rejects a scheme. `git` over smart
  HTTP works, through `insteadOf` in the guest gitconfig.
- **No credentials under `isolated`.** The posture blocks the broker along with
  everything else. An isolated run has no credentials, deliberately.

## MCP servers, in a box

```bash
devbox mcp add fetch --box mcp-tools -- uvx mcp-server-fetch
devbox mcp add gitsrv --box mcp-tools --posture mirror-only -- mcp-server-git
devbox mcp ls
devbox mcp report fetch     # the report for its most recent session
devbox mcp rm fetch
```

Then point the agent at it:

```bash
claude mcp add fetch -- devbox mcp run fetch
codex mcp add fetch -- devbox mcp run fetch
```

`devbox mcp run` is a byte-exact stdio shim — no parsing, no line buffering, no
re-framing. The server's stderr goes to `~/.devbox/mcp/<name>.log`, so a chatty
server cannot corrupt the JSON-RPC stream. Registration edits `devbox.toml` as
text so comments and ordering survive; `--global` writes `~/.devbox/mcp.toml`
instead, which is what you want for a server you use from any directory.

Each `mcp run` is a run: `kind = mcp`, labelled with the server's name,
bracketed by two checkpoints, with its own report — `devbox mcp report <name>`
renders the latest, and `devbox runs` lists them alongside everything else. The
run also records how it ended, because an agent's shutdown handshake and a
forced stop both exit 143:

| `ended_by` | Meaning |
|---|---|
| `exit` | The server exited on its own |
| `stdin-eof` | The agent closed the pipe — the normal MCP shutdown |
| `signal` | `devbox mcp run` was signalled |
| `forced` | The transport had to be killed to get out |

If the box has no `uvx` or `npx`, `mcp add` says so at registration time and
names the set that provides it:

```
Warning: box 'devtest' has no 'uvx' on its PATH.
  It comes with the 'python' set (uv, uvx and python3). Add it with:
    devbox upgrade devtest --tools python
```

## devbox as an MCP server

The other direction — let the agent ask what a box has been doing:

```bash
devbox mcp self
claude mcp add devbox -- devbox mcp self
```

Four read-only tools, each with a JSON Schema: `list_runs`, `run_report`,
`behavior_summary`, `watch`. It starts no box and changes nothing. A bad request
gets a JSON-RPC error and the session continues; a tool that could not answer
(no such box, a run id that is not a run id) comes back as a normal result with
`isError: true`, because a client reads a protocol error as "this server is
broken" and stops asking.

Only stdio, and only tools: `resources/*`, `prompts/*`, `sampling/*` and the
rest get `-32601`, which is the answer that tells a client to stop asking. A
JSON-RPC batch array is not supported either — it has no top-level `method`, so
it comes back as `-32600`.

## Export

```bash
devbox export --run 01M1SEWB66PS95BP62WTHJJF11 --format ocsf
devbox export --from 2026-09-05T14:00:00Z --format otlp-json --out events.json
devbox export --format jsonl
```

`--run` resolves the run to a row-id range first, so exporting one run out of a
440k-event store is a scan of that run, not of the store. `ocsf` is OCSF 1.3,
one JSON object per line; `otlp-json` is a single OTLP/JSON
`ExportLogsServiceRequest`; `jsonl` is devbox's own event, unchanged.

`ocsf` skips what it cannot map honestly, and names it on the way out —
`syscall` is the one kind with no class, so a run that made syscall events
exports that many fewer and says so. A run that used a credential exports it as
API Activity 6003, stamped with the run id. Use `jsonl` when you need the
complete record.

## The CLI, by task

```bash
# lifecycle
devbox shell                     # a terminal, no browser needed
devbox list / status / stop / destroy

# what is in the box
devbox sets list
devbox sets apply --set system --set git --set lang-rust

# what the box did
devbox watch --type dns,tls --tree
devbox behavior summary
devbox behavior diff --from 2026-09-05T14:00:00Z --at 2026-09-05T15:00:00Z
devbox behavior pcap --proto tcp --daddr 93.184.216.34 --dport 443 --seconds 5

# what the box may reach
devbox policy show
devbox policy set mirror-only    # applied to a running box immediately
devbox policy test pypi.org      # exits non-zero if denied

# is any of this working
devbox doctor
```

Every command that acts on a box takes the box name as an optional first
positional. Leave it out and devbox uses the box registered for the current
directory.

## What changed from v4

- **`devbox run`** — a run is a first-class object with an id, a report, two
  checkpoints, and a coverage line. `devbox runs` and `devbox report` read it
  back; the console has a **Runs** tab.
- **Checkpoints** — `devbox layer checkpoint / checkpoints / restore /
  checkpoint-rm`, and `devbox layer diff --from`.
- **Credentials left the box** — `devbox secret` and `devbox broker`. The v4
  provisioning code that copied `~/.claude/.credentials.json`,
  `~/.codex/auth.json`, and a plaintext `~/.devbox-ai-env` into the guest is
  gone, and re-provisioning an old box deletes those files from it.
- **`devbox mcp`** — MCP servers run inside a box instead of on your host, each
  session recorded as a run with its own report; and `devbox mcp self` exposes
  devbox's own runs and events to the agent as tools.
- **`devbox export`** — OCSF 1.3 and OTLP/JSON, for anything downstream.
- **File events have a scope** — the agent exports file events only under
  declared prefixes (`/workspace` and the box user's home by default). The scope
  rides in the handshake and `devbox doctor` prints it.
- **Byte counts are real** — a `tcp_close` probe settles them; `connect` no
  longer reports a zero that looked like a measurement.
- **Network labs and the ZTP fabric were removed.** They will return as a
  separate, container-based tool.

Existing v4 boxes load unchanged. A v4 event store is migrated in place, column
by column, checked against `PRAGMA table_info` rather than a recorded version
number, so an interrupted migration is retried instead of being skipped.

© 2026 Ethan H.B. Zhou
