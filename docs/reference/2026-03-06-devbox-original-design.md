# Devbox — Safe AI Coding Sandbox Tool

> Design document for a standalone, cross-runtime sandbox CLI for AI coding tools.

**Goal:** One command to create a safe, isolated sandbox for AI coding on any local machine. No remote, no fleet management, no auth — just a local tool that detects the best available runtime and drops you into a working environment.

**Status:** Design only. Implementation deferred.

---

## 1. Problem Statement

AI coding tools (Claude Code, Codex, Gemini CLI, Aider, etc.) need to execute arbitrary commands: install packages, run builds, modify files, run tests. On a developer's host machine, this is risky — a careless `rm -rf`, a bad `npm install`, or a rogue build script can damage the system.

Developers need a way to run AI coding tools in an isolated environment that:
- Protects the host from destructive commands
- Mounts the project directory so AI tools can read/write code
- Has the right language toolchains pre-installed
- Works across macOS and Linux
- Requires zero configuration for the common case

**Non-goals:** Remote machine management, multi-tenant access, agent orchestration, fleet provisioning. These are solved by higher-level tools (e.g., Holonex `hx dev`).

---

## 2. User Experience

### Core Workflow

```bash
# Simplest usage: sandbox with auto-detected tools in current project dir
devbox create

# Specify AI tool + language runtime
devbox create --tools claude-code,go

# Specify runtime explicitly
devbox create --runtime incus

# Mount specific directories
devbox create --mount ./src --mount ./docs:ro

# Resource limits
devbox create --cpu 4 --memory 8GB

# Custom name
devbox create --name my-sandbox

# Bare sandbox (no auto-detection, no AI tools)
devbox create --bare

# Management
devbox list                              # list all sandboxes
devbox shell [name]                      # attach to sandbox (starts if stopped)
devbox stop [name]                       # stop but preserve state
devbox destroy [name]                    # permanently delete
devbox snapshot create [name] [snap]     # checkpoint
devbox snapshot restore [name] [snap]    # rollback
devbox snapshot list [name]              # list snapshots
```

### What Happens on `devbox create`

1. Detect best available runtime (Docker > Incus > Lima)
2. Create a persistent container/VM
3. Mount current directory to `/workspace` (read-write)
4. Scan project files for language detection (`go.mod` -> Go, `package.json` -> Node, etc.)
5. Install detected language runtimes + any `--tools` specified
6. Drop into an interactive bash shell at `/workspace`

### Naming Convention

Default name: derived from the project directory name.
- `~/projects/myapp` -> sandbox named `myapp`
- Collision: append suffix (`myapp-2`)

---

## 3. Architecture

```
+--------------------------------------------------+
|  CLI Layer (cobra)                                |
|  create | list | shell | stop | destroy | snapshot|
+--------------------------------------------------+
           |
+--------------------------------------------------+
|  Sandbox Manager                                  |
|  Lifecycle orchestration, state tracking          |
+--------------------------------------------------+
           |
+--------------------------------------------------+
|  Runtime Interface                                |
|  Create | Start | Stop | Exec | Destroy |         |
|  Snapshot | Restore | IsAvailable                 |
+--------------------------------------------------+
     |              |              |
+---------+   +---------+   +---------+
| Docker  |   |  Incus  |   |  Lima   |
+---------+   +---------+   +---------+
```

### 3.1 Runtime Interface

```go
type Runtime interface {
    Name() string
    IsAvailable() bool       // checks if binary + daemon are present
    Priority() int           // for auto-detection ordering

    Create(opts CreateOpts) (Sandbox, error)
    Start(name string) error
    Stop(name string) error
    Exec(name string, cmd []string, interactive bool) error
    Destroy(name string) error

    Snapshot(name, snapName string) error
    Restore(name, snapName string) error
    ListSnapshots(name string) ([]SnapshotInfo, error)

    List() ([]SandboxInfo, error)
}

type CreateOpts struct {
    Name       string
    Mounts     []Mount        // host:container:mode
    CPU        int            // cores (0 = no limit)
    Memory     string         // e.g., "8GB" (empty = no limit)
    Tools      []string       // tools to install post-creation
    BaseImage  string         // override base image
}

type Mount struct {
    HostPath      string
    ContainerPath string
    ReadOnly      bool
}
```

### 3.2 Runtime Auto-Detection

Priority order (highest first):
1. **Docker** (priority 10) — check `docker info` succeeds
2. **Incus** (priority 20, Linux only) — check `incus info` succeeds
3. **Lima** (priority 30, macOS only) — check `limactl` exists

Selection logic:
- `--runtime <name>`: use specified runtime, error if unavailable
- No flag: iterate by priority, pick first available
- No runtime available: print actionable error with install instructions

### 3.3 Project Detector

Scans the mounted directory (default: current working directory) for known project files:

| File | Detected Language | Tools Installed |
|------|------------------|-----------------|
| `go.mod` | Go | `go` (latest stable) |
| `package.json` | Node.js | `node`, `npm` |
| `pyproject.toml`, `requirements.txt` | Python | `python3`, `pip` |
| `Cargo.toml` | Rust | `rustc`, `cargo` |
| `Gemfile` | Ruby | `ruby`, `bundler` |
| `pom.xml`, `build.gradle` | Java | `java`, `gradle`/`maven` |
| `Makefile` only | Unknown | (no auto-install) |

Multiple detection: if both `go.mod` and `package.json` exist, install both Go and Node.

Skipped when `--bare` is specified or `--tools` explicitly provided.

### 3.4 Tool Registry

Tools are categorized into two types:

**AI Coding Tools:**

| Tool ID | Name | Install Method |
|---------|------|----------------|
| `claude-code` | Claude Code | `npm install -g @anthropic-ai/claude-code` |
| `codex` | OpenAI Codex CLI | `npm install -g @openai/codex` |
| `gemini-cli` | Google Gemini CLI | `npm install -g @anthropic-ai/claude-code && ...` (TBD) |
| `opencode` | OpenCode | `go install github.com/opencode-ai/opencode@latest` |
| `aider` | Aider | `pip install aider-chat` |

**Language Runtimes:**

| Tool ID | Install Method |
|---------|----------------|
| `go` | Official tarball or distro package |
| `nodejs` | NodeSource or nvm |
| `python` | System package + pip |
| `rust` | rustup |
| `ruby` | rbenv or system package |
| `java` | SDKMAN or system package |
| `docker` | Docker-in-Docker or socket mount |

All tools use the same `--tools` flag: `--tools claude-code,go,nodejs`.

No AI tool is installed by default. Only auto-detected language runtimes are installed unless `--bare` is specified.

### 3.5 Sandbox Manager

Manages sandbox lifecycle and persists state to `~/.devbox/`.

```
~/.devbox/
  config.yaml              # user defaults
  sandboxes/
    myapp/
      sandbox.yaml         # runtime, mounts, tools, created_at
      snapshots/           # snapshot metadata (actual data in runtime)
```

**sandbox.yaml example:**

```yaml
name: myapp
runtime: docker
created: 2026-03-06T10:00:00Z
status: running
mounts:
  - host: /home/user/projects/myapp
    container: /workspace
    readonly: false
resources:
  cpu: 0
  memory: ""
tools:
  - go
  - claude-code
base_image: ubuntu:24.04
```

**config.yaml example:**

```yaml
default_runtime: ""         # empty = auto-detect
default_tools: []           # tools to always install
default_cpu: 0
default_memory: ""
base_image: ubuntu:24.04
```

---

## 4. Runtime-Specific Implementation Notes

### 4.1 Docker

- `docker run -d --name devbox-<name> -v <host>:<container> -it ubuntu:24.04 sleep infinity`
- `docker exec -it devbox-<name> bash` for shell
- Snapshots via `docker commit` + `docker tag`
- Resource limits via `--cpus` and `--memory`
- Simplest implementation, widest compatibility
- Limitation: container isolation only (shared kernel)

### 4.2 Incus

- `incus launch ubuntu:24.04 devbox-<name>`
- `incus exec devbox-<name> -- bash` for shell
- Native snapshot support: `incus snapshot create devbox-<name> <snap>`
- Resource limits via `incus config set`
- Mounts via `incus config device add ... disk source=<host> path=<container>`
- Stronger isolation: can use VMs (`incus launch ubuntu:24.04 devbox-<name> --vm`)

### 4.3 Lima

- `limactl create --name devbox-<name> template://ubuntu-24.04`
- `limactl shell devbox-<name>` for shell
- Mounts via Lima's built-in mount (9p/virtiofs)
- VM-level isolation on macOS
- Slower startup than Docker, but no Docker Desktop dependency

---

## 5. Base Image Contents

Every sandbox starts with:

- **OS:** Ubuntu 24.04 LTS
- **Core tools:** git, curl, wget, jq, build-essential, ca-certificates, openssh-client
- **Shell:** bash with basic prompt showing sandbox name
- **User:** non-root user `dev` with sudo access (mirrors typical dev setup)
- **Locale:** UTF-8

For Docker, this could be a pre-built image (`ghcr.io/holonexai/devbox-base:24.04`) to speed up creation. For Incus/Lima, a setup script runs on first boot.

---

## 6. Security Model

**What's isolated:**
- Filesystem: only explicitly mounted directories are visible
- Processes: sandbox processes cannot see or signal host processes
- Network: full network access by default (AI tools need API access)
- Users: sandbox runs as non-root `dev` user (sudo available inside)

**What's NOT isolated:**
- Mounted directories are read-write by default (the AI can modify/delete project files)
- Network is unrestricted (AI tools can make arbitrary HTTP requests)
- With Docker, kernel is shared (not a hard security boundary)

**Safety features:**
- Warn if mounted directory has uncommitted git changes
- `devbox snapshot` for manual checkpoints before risky operations
- `--mount ./src:ro` for explicit read-only when needed
- Container names prefixed with `devbox-` to avoid collisions

**Explicit non-features (by design):**
- No authentication/authorization
- No network restrictions
- No file access audit logging
- No resource quotas beyond optional `--cpu`/`--memory`

---

## 7. CLI Command Reference

### `devbox create`

Create and enter a new sandbox.

```
Flags:
  --name       string    Sandbox name (default: directory name)
  --runtime    string    Runtime: docker, incus, lima (default: auto-detect)
  --mount      strings   Mount directories (host[:container][:ro])
  --tools      strings   Tools to install (comma-separated)
  --cpu        int       CPU cores limit (0 = unlimited)
  --memory     string    Memory limit (e.g., "8GB", empty = unlimited)
  --bare                 Skip auto-detection, minimal base only
  --no-enter             Create but don't enter shell
```

Default mount: current directory -> `/workspace` (read-write).

### `devbox list`

List all sandboxes with status, runtime, and creation time.

```
NAME     RUNTIME  STATUS   CREATED              MOUNTS
myapp    docker   running  2026-03-06 10:00:00  ~/projects/myapp
backend  incus    stopped  2026-03-05 14:30:00  ~/work/backend
```

### `devbox shell [name]`

Attach to a sandbox. Starts it if stopped. If `name` omitted, uses sandbox matching current directory.

### `devbox stop [name]`

Stop a sandbox, preserving all state.

### `devbox destroy [name]`

Permanently delete a sandbox and all its state. Prompts for confirmation.

### `devbox snapshot create [sandbox] [snapshot-name]`

Create a named snapshot of the sandbox's current state.

### `devbox snapshot restore [sandbox] [snapshot-name]`

Restore a sandbox to a previous snapshot. Stops the sandbox first if running.

### `devbox snapshot list [sandbox]`

List all snapshots for a sandbox.

---

## 8. Project Structure

```
devbox/
  cmd/
    devbox/
      main.go              # entry point
  internal/
    cli/                   # cobra commands
      create.go
      list.go
      shell.go
      stop.go
      destroy.go
      snapshot.go
    sandbox/               # sandbox manager
      manager.go           # lifecycle orchestration
      state.go             # sandbox.yaml read/write
      config.go            # ~/.devbox/config.yaml
    runtime/               # runtime abstraction
      runtime.go           # interface definition
      detect.go            # auto-detection
      docker.go            # Docker implementation
      incus.go             # Incus implementation
      lima.go              # Lima implementation
    tools/                 # tool registry
      registry.go          # tool definitions
      detect.go            # project language detection
      install.go           # tool installation scripts
  go.mod
  go.sum
  README.md
  LICENSE
```

**Language:** Go (single binary, cross-platform).

**Dependencies:** Minimal — cobra for CLI, yaml for config. All runtime interactions via exec (no Docker SDK, no Incus client library — keeps it simple and dependency-free).

---

## 9. Future Considerations (Not in v1)

These are explicitly out of scope but worth noting:

- **Docker-in-Docker** — running Docker inside the sandbox for projects that need it
- **GPU passthrough** — for ML/AI workloads
- **Shared tool cache** — mount a shared `GOMODCACHE` or `npm cache` across sandboxes
- **Pre-built images per language** — `devbox create --image go-dev` for instant Go environments
- **VSCode/Cursor devcontainer integration** — generate `.devcontainer.json` from devbox config
- **Holonex integration** — `hx dev` could shell out to `devbox` for local sandbox creation

---

## 10. Open Questions

1. **Image strategy:** Pre-build a devbox base image and publish to a registry, or always build from `ubuntu:24.04` + setup script? Pre-built is faster but adds a registry dependency.

2. **Tool version pinning:** Should `--tools go` install latest, or should users be able to specify `--tools go@1.23`? v1 could just use latest and add version pinning later.

3. **Multiple sandboxes per project:** Should `devbox create` in a directory that already has a sandbox error, or create a second one with a suffix? Recommend: error with "use `devbox shell` to reattach or `devbox destroy` first."

4. **Shell customization:** Should devbox inject a custom `.bashrc` with the sandbox name in the prompt, or leave the shell vanilla?
