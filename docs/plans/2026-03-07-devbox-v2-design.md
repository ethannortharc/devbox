# Devbox v2 — The Ultimate Developer VM

> Comprehensive design document for a standalone, Nix-powered, VM-based developer environment CLI.

**Goal:** One command to create a fully-loaded, safe, persistent developer VM on any local machine. Pre-installed with every modern tool a developer needs. Language packs added incrementally. Beautiful terminal UX with pre-built workspace layouts.

**Status:** Design approved. Implementation pending.

**Language:** Rust

**Prior art:** [Original devbox design (2026-03-06)](../reference/2026-03-06-devbox-original-design.md)

---

## Table of Contents

1. [Problem Statement](#1-problem-statement)
2. [Design Philosophy](#2-design-philosophy)
3. [User Experience — Progressive Disclosure](#3-user-experience--progressive-disclosure)
4. [Architecture](#4-architecture)
5. [Runtime Model](#5-runtime-model)
6. [Tool Definition Layer — Nix](#6-tool-definition-layer--nix)
7. [Complete Tool Catalog](#7-complete-tool-catalog)
8. [Zellij Layouts — Pre-built Workspaces](#8-zellij-layouts--pre-built-workspaces)
9. [Configuration — devbox.toml](#9-configuration--devboxtoml)
10. [TUI Package Manager](#10-tui-package-manager)
11. [Security Model](#11-security-model)
12. [Project Structure](#12-project-structure)
13. [Open Questions](#13-open-questions)
14. [Changes from Original Design](#14-changes-from-original-design)

---

## 1. Problem Statement

AI coding tools (Claude Code, Codex, Aider, etc.) need to execute arbitrary commands: install packages, run builds, modify files, run tests. On a developer's host machine, this is risky — a careless `rm -rf`, a bad `npm install`, or a rogue build script can damage the system.

Beyond safety, developers waste hours configuring environments: installing tools, managing versions, setting up shell configs, and debugging "works on my machine" issues.

Developers need:
- A safe, isolated environment for AI coding tools
- A fully-loaded modern dev environment out of the box
- Zero-config for the common case, deep customization when needed
- Persistent state (installed tools survive reboots)
- Cross-platform: macOS and Linux

**Non-goals:** Remote machine management, multi-tenant access, agent orchestration, fleet provisioning. These are solved by higher-level tools (e.g., Holonex `hx dev`).

---

## 2. Design Philosophy

### Progressive Disclosure

Simple by default, powerful when needed. No flags for the common case. Discoverable advanced features.

- **Beginner:** `devbox` — just works
- **Intermediate:** `devbox --tools claude-code,go` — customize
- **Advanced:** `devbox create --runtime incus --cpu 4 --memory 8G` — full control

### Out-of-the-Box Delight

Every tool a modern developer expects is pre-installed. The first `devbox` session should feel like opening a fully configured IDE, not a bare terminal.

### Nix-Powered Composability

All tools managed through Nix sets. Users can toggle sets, add individual packages, import external Nix flakes, or define their own. A unified tool definition layer ensures consistency.

### VM-Based Persistence

Unlike containers, VMs persist state naturally. Tools installed at any time survive reboots. The environment grows with the developer.

---

## 3. User Experience — Progressive Disclosure

### Smart Default: Bare `devbox` Command

The bare `devbox` command (no subcommand) is the primary entry point:
- If no sandbox exists for the current directory: **create one** (auto-detect everything)
- If a sandbox exists: **attach to it** (start if stopped)

This is idempotent — running `devbox` twice is always safe.

### Tier 1: Zero-Config

```bash
$ devbox                    # Smart default: create or attach
$ devbox stop               # Stop current project's sandbox
$ devbox destroy            # Remove it
```

### Tier 2: Explicit Control

```bash
$ devbox create --tools claude-code,go    # Explicit tools
$ devbox create --name my-sandbox         # Custom name
$ devbox shell myapp                       # Attach by name
$ devbox shell --layout ai-pair           # Attach with specific layout
$ devbox exec -- make test                 # Run one-off command
$ devbox list                              # List all sandboxes
$ devbox status                            # Detailed status
$ devbox snapshot save before-refactor     # Checkpoint
$ devbox snapshot restore before-refactor  # Rollback
$ devbox upgrade --tools rust              # Add tools to existing sandbox
```

### Tier 3: Power User

```bash
$ devbox create --runtime incus --cpu 4 --memory 8G
$ devbox create --mount ./data:ro --env-file .env
$ devbox config set default.runtime incus
$ devbox config set default.tools go,nodejs
$ devbox config set default.layout ai-pair
$ devbox prune                              # Remove all stopped sandboxes
$ devbox init                               # Generate devbox.toml
$ devbox doctor                             # Diagnose issues
$ devbox layout list                        # List available layouts
$ devbox layout edit tdd                    # Customize a layout
$ devbox layout create my-layout            # Create new layout
$ devbox nix add github:user/flake#pkg     # Add external Nix package
$ devbox packages                           # Open TUI package manager
```

### Naming Convention

Default name: derived from the project directory name.
- `~/projects/myapp` -> sandbox named `myapp`
- Collision: error with "use `devbox shell` to reattach or `devbox destroy` first"
- Override: `--name custom-name`

### What Happens on First `devbox`

1. Detect best available runtime (Incus on Linux, Lima on macOS)
2. Create a persistent VM with Ubuntu 24.04
3. Apply Nix profile: `system` + `shell` + `tools` + `editor` + `git` + `container` sets
4. Mount current directory to `/workspace` (read-write)
5. Scan project files for language detection (`go.mod` -> install `lang-go` set)
6. Configure shell (Zsh + Starship + autosuggestions + syntax-highlighting)
7. Launch Zellij with default layout
8. Drop into `/workspace` ready to code

---

## 4. Architecture

```
+------------------------------------------------------+
|  CLI Layer (clap)                                     |
|  devbox | create | shell | exec | stop | destroy      |
|  list | snapshot | status | upgrade | config           |
|  layout | packages | doctor | prune | init | nix       |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Sandbox Manager                                      |
|  - Smart default logic (create-or-attach)             |
|  - State persistence (~/.devbox/ + devbox.toml)       |
|  - Nix profile orchestration                          |
|  - Tool installation and upgrade                      |
|  - Layout management                                  |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Nix Set Manager                                      |
|  - Set composition (toggle sets on/off)               |
|  - Individual package management                      |
|  - External flake integration                         |
|  - Profile rollback                                   |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Runtime Trait                                         |
|  create() start() stop() exec_cmd() destroy()         |
|  snapshot_create() snapshot_restore() list()           |
|  is_available() priority() upgrade()                  |
+------------------------------------------------------+
     |                          |
+---------+               +---------+
|  Incus  |               |  Lima   |
| (Linux) |               | (macOS) |
+---------+               +---------+
```

### Runtime Trait (Rust)

```rust
#[async_trait]
pub trait Runtime: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    fn priority(&self) -> u32;  // Higher = preferred. Incus=20, Lima=10

    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult>;
    async fn destroy(&self, name: &str) -> Result<()>;
    async fn status(&self, name: &str) -> Result<SandboxStatus>;
    async fn list(&self) -> Result<Vec<SandboxInfo>>;

    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()>;
    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()>;
    async fn snapshot_list(&self, name: &str) -> Result<Vec<SnapshotInfo>>;

    async fn upgrade(&self, name: &str, tools: &[String]) -> Result<()>;
}

pub struct CreateOpts {
    pub name: String,
    pub mounts: Vec<Mount>,
    pub cpu: u32,           // 0 = unlimited
    pub memory: String,     // "" = unlimited
    pub env: HashMap<String, String>,
    pub env_file: Option<PathBuf>,
    pub sets: Vec<String>,  // Nix sets to activate
    pub tools: Vec<String>, // Additional tools
    pub layout: String,     // Default zellij layout
    pub bare: bool,         // Skip auto-detection
}

pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: String,
    pub read_only: bool,
}
```

### Exec-Based Runtime Calls

All runtime interactions via safe subprocess invocation (no shell interpolation):
- **Incus:** `incus launch`, `incus exec`, `incus config device add`
- **Lima:** `limactl create`, `limactl shell`, Lima YAML config

No native SDK dependencies. Every operation is a visible subprocess call — easy to debug, easy to understand.

### Runtime Auto-Detection

| Host OS | Runtime | Priority | Detection |
|---------|---------|----------|-----------|
| Linux | Incus (VM mode) | 20 | `incus info` succeeds |
| macOS | Lima | 10 | `limactl` exists |

Selection logic:
- `--runtime <name>`: use specified, error if unavailable
- No flag: pick highest priority available
- None available: print install instructions

---

## 5. Runtime Model

### Why VMs, Not Containers

The original design supported Docker containers as the primary runtime. This revision uses VMs exclusively because:

1. **Persistence:** VM state survives reboots. Installed tools, config changes, cached downloads — all persist.
2. **Full system:** VMs run a real init system (systemd). Services like Docker, Tailscale, Ollama run as daemons.
3. **Stronger isolation:** Separate kernel. A rogue process in the VM cannot escape to the host.
4. **Nested virtualization:** Can run Docker, Incus, and other container runtimes inside the VM.
5. **Network stack:** Full network namespace with its own IP. Tailscale can give it a routable address.

### Incus (Linux)

```bash
incus launch ubuntu:24.04 devbox-<name> --vm
incus config device add devbox-<name> workspace disk source=<host> path=/workspace
incus exec devbox-<name> -- sudo -u dev zsh
```

- Native VM support via QEMU/KVM
- Excellent snapshot support: `incus snapshot create devbox-<name> <snap>`
- Resource limits via `incus config set`
- Mounts via disk devices

### Lima (macOS)

```bash
limactl create --name devbox-<name> template://ubuntu-24.04
limactl shell devbox-<name>
```

- HVF-based VM on Apple Silicon
- Mounts via virtiofs (fast, native)
- Lima YAML config for resource limits, port forwarding

---

## 6. Tool Definition Layer — Nix

### Why Nix

| Option | Verdict |
|--------|---------|
| **Nix** | Declarative, 100K+ packages, composable profiles, rollback, reproducible |
| Guix | Scheme-based, smaller community, fewer packages |
| pkgx | Simpler but cannot manage system-level config |
| mise | Great for language runtimes, cannot manage system tools |
| Homebrew | Not reproducible, no composable profiles, slow on Linux |
| Ansible | Good for provisioning, not for interactive package management |

**Decision:** Nix as the unified tool definition layer. devbox provides a friendly CLI/TUI on top.

### How It Works

1. **Nix is installed inside the VM** during first boot
2. **Devbox ships a Nix flake** with all tool "sets" defined
3. **Sets are Nix package lists** that can be toggled on/off
4. **Users can add custom packages** from Nixpkgs or external flakes
5. **`devbox.toml` captures selections** — committable to git for team consistency
6. **Rollback is built-in:** `nix profile rollback` undoes any install

### Nix Flake Structure

```nix
# devbox/nix/flake.nix
{
  description = "Devbox - Developer Environment Tool Sets";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        sets = import ./sets { inherit pkgs; };
      in {
        packages = {
          system      = sets.system;
          shell       = sets.shell;
          tools       = sets.tools;
          editor      = sets.editor;
          git         = sets.git;
          container   = sets.container;
          network     = sets.network;
          ai          = sets.ai;
          lang-go     = sets.lang-go;
          lang-rust   = sets.lang-rust;
          lang-python = sets.lang-python;
          lang-node   = sets.lang-node;
          lang-java   = sets.lang-java;
          lang-ruby   = sets.lang-ruby;
        };

        devShells = {
          minimal = pkgs.mkShell {
            packages = sets.system ++ sets.shell ++ sets.tools;
          };
          default = pkgs.mkShell {
            packages = sets.system ++ sets.shell ++ sets.tools
                    ++ sets.editor ++ sets.git ++ sets.container;
          };
          full = pkgs.mkShell {
            packages = sets.system ++ sets.shell ++ sets.tools
                    ++ sets.editor ++ sets.git ++ sets.container
                    ++ sets.network ++ sets.ai;
          };
        };
      }
    );
}
```

### External Nix Integration

Users can add packages from any Nix source:

```bash
# Add from Nixpkgs
devbox nix add nixpkgs#terraform

# Add from external flake
devbox nix add github:nix-community/neovim-nightly-overlay#neovim

# Add from local flake
devbox nix add ./my-tools#mytool

# Remove
devbox nix remove terraform
```

---

## 7. Complete Tool Catalog

### SET 1: `system` — OS Foundation (Always Installed, Locked)

> The bedrock. These packages ensure the VM is a functional Linux development machine.

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `coreutils` | `ls`, `cp`, `mv`... | GNU core utilities | Standard POSIX tools. Nix ensures consistent version. |
| 2 | `util-linux` | `mount`, `lsblk`... | Linux system utilities | Disk, partition, and process tools. |
| 3 | `gcc` | `gcc`, `g++` | C/C++ compiler | Required by packages with native extensions. |
| 4 | `gnumake` | `make` | Build automation | Universal build tool. |
| 5 | `pkg-config` | `pkg-config` | Library path resolver | Required when compiling against system libraries. |
| 6 | `openssl` | `openssl` | TLS/SSL library + CLI | Needed by curl, git, and all network tools. |
| 7 | `cacert` | — (CA bundle) | CA certificates | HTTPS trust store. |
| 8 | `glibc` | — (libc) | C standard library | Every binary links against it. |
| 9 | `systemd` | `systemctl` | Init system | Manages background services (Docker, Tailscale, Ollama). |
| 10 | `sudo` | `sudo` | Privilege escalation | Developers expect sudo access. |
| 11 | `openssh` | `ssh`, `sshd`, `ssh-keygen` | SSH client + server | Git SSH, remote access, key management. |
| 12 | `iproute2` | `ip`, `ss` | Network configuration | View/configure interfaces. |
| 13 | `iptables` | `iptables` | Firewall | Required by Docker networking and Tailscale. |
| 14 | `procps` | `ps`, `pgrep`, `kill` | Process utilities | Basic process inspection. |
| 15 | `findutils` | `find`, `xargs` | File search (classic) | Fallback when fd is not appropriate. |
| 16 | `gawk` | `awk` | Text processing | Shell scripting essential. |
| 17 | `gnused` | `sed` | Stream editor | Config file manipulation. |
| 18 | `gnutar` | `tar` | Archive tool | Extract tarballs. |
| 19 | `gzip` | `gzip`, `gunzip` | Compression | `.tar.gz` archives. |
| 20 | `unzip` | `unzip` | ZIP extraction | GitHub releases, etc. |
| 21 | `xz` | `xz` | XZ compression | `.tar.xz` archives. |
| 22 | `zstd` | `zstd` | Zstandard compression | Used by Nix itself. |
| 23 | `curl` | `curl` | HTTP client | Universal download/API tool. |
| 24 | `wget` | `wget` | HTTP downloader | Some scripts require wget specifically. |
| 25 | `less` | `less` | Pager | Default for git, man pages. |
| 26 | `man-db` | `man` | Manual pages | `man git`, `man docker`. |
| 27 | `file` | `file` | File type detection | Identifies file types. |
| 28 | `which` | `which` | Binary path lookup | Check tool availability. |
| 29 | `locale` | — | Locale support | UTF-8 locale data. |

### SET 2: `shell` — Terminal and Shell Environment (Always Installed, Locked)

> The developer's home. This is where the "extreme UX" lives.

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `ghostty` | `ghostty` | GPU-accelerated terminal emulator | Zig-based, blazing fast. Ligatures, images, undercurl. Config: `~/.config/ghostty/config`. Used when accessing devbox via GUI. |
| 2 | `zellij` | `zellij` | Terminal multiplexer + Wasm plugins | Modern tmux replacement. Layout system, session persistence, floating panes. Auto-starts on shell entry. |
| 3 | `zsh` | `zsh` | Z Shell (default shell) | Superior completion, globbing, plugin ecosystem. |
| 4 | `zsh-autosuggestions` | — (plugin) | Fish-like input suggestions | Shows grayed-out completion from history. Accept with right-arrow. |
| 5 | `zsh-syntax-highlighting` | — (plugin) | Real-time command coloring | Valid = green, invalid = red. Catches typos before Enter. |
| 6 | `starship` | `starship` | Cross-shell prompt | Shows devbox name, git branch, language versions, exit code. Config: `~/.config/starship.toml`. |
| 7 | `zoxide` | `z` | Smart directory jumper | Learns cd patterns. `z proj` -> `/workspace/project`. |
| 8 | `fzf` | `fzf` | Fuzzy finder | Ctrl+R: history. Ctrl+T: files. `**<TAB>`: paths. |
| 9 | `yazi` | `yazi` | Terminal file manager | Fast, image preview, bulk operations. Built in Rust. |
| 10 | `nerd-fonts` | — (fonts) | Developer icon fonts | Required by eza, yazi, starship for icons. FiraCode Nerd Font. |
| 11 | `tmux` | `tmux` | Classic terminal multiplexer | Fallback for tmux users. Not the default. |

**Shell auto-configuration:**
```zsh
# ~/.zshrc (auto-generated by devbox)
source ${pkgs.zsh-autosuggestions}/share/zsh-autosuggestions/zsh-autosuggestions.zsh
source ${pkgs.zsh-syntax-highlighting}/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh
eval "$(starship init zsh)"
eval "$(zoxide init zsh)"
source <(fzf --zsh)

# Modern tool aliases
alias ls='eza --icons'
alias cat='bat --paging=never'
alias grep='rg'
alias find='fd'
alias diff='delta'
alias top='htop'

# Devbox identity
export DEVBOX_NAME="<sandbox-name>"
export DEVBOX_RUNTIME="<runtime>"
```

### SET 3: `tools` — Modern Developer CLI (Always Installed, Locked)

> Rust-powered replacements for classic Unix tools.

| # | Nix Package | Binary | Purpose | Replaces | Notes |
|---|-------------|--------|---------|----------|-------|
| 1 | `ripgrep` | `rg` | Ultra-fast regex search | grep | AI tools use rg as primary search engine. .gitignore aware. |
| 2 | `fd` | `fd` | Intuitive file finding | find | Simple: `fd "\.go$"`. Respects .gitignore. |
| 3 | `bat` | `bat` | Syntax-highlighted viewer | cat | Line numbers, git changes, syntax highlighting. |
| 4 | `eza` | `eza` | Modern directory listing | ls | Git status, icons, tree view. |
| 5 | `delta` | `delta` | Beautiful diff viewer | diff | Side-by-side, syntax-highlighted. Git default pager. |
| 6 | `jq` | `jq` | JSON processor | — | Parse API responses, config files. |
| 7 | `yq-go` | `yq` | YAML/TOML/XML processor | — | Like jq for YAML. K8s, Compose, CI configs. |
| 8 | `glow` | `glow` | Terminal Markdown renderer | — | Read AI-generated docs beautifully in terminal. |
| 9 | `htop` | `htop` | Interactive process viewer | top | Color, mouse, tree view, CPU/memory graphs. |
| 10 | `httpie` | `http`, `https` | Human-friendly HTTP | curl (testing) | `http POST api.example.com name=test`. |
| 11 | `watchexec` | `watchexec` | File watcher + runner | inotifywait | `watchexec -e go -- make test`. Auto-runs on save. |
| 12 | `direnv` | `direnv` | Per-dir env loader | — | Auto-loads `.envrc`. Pairs with Nix (`use flake`). |
| 13 | `tree` | `tree` | Directory structure | — | AI tools use `tree` output to understand codebases. |
| 14 | `sqlite-interactive` | `sqlite3` | SQLite CLI | — | Quick local databases. Many tools use SQLite. |
| 15 | `strace` | `strace` | System call tracer | — | Debug hangs and mysterious failures. |
| 16 | `hexyl` | `hexyl` | Hex viewer | xxd | Modern hex dump with colors. |
| 17 | `dust` | `dust` | Disk usage analyzer | du | Visual, sorted. Find disk space hogs. |
| 18 | `procs` | `procs` | Modern process viewer | ps | Colored, tree view, port display. |
| 19 | `tokei` | `tokei` | Code statistics | cloc | Fast LOC counter by language. |
| 20 | `bandwhich` | `bandwhich` | Network bandwidth monitor | — | See which processes use network. |
| 21 | `bottom` | `btm` | System resource monitor | — | Beautiful TUI: CPU, memory, network, disk. |

### SET 4: `editor` — Terminal Editors (Default: ON)

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `neovim` | `nvim` | Terminal IDE | Pre-configured with NVChad or AstroNvim. LSP, Treesitter, telescope, AI plugins (Copilot.lua). |
| 2 | `vscode-server` | `code-server` | VS Code in browser | Access at `http://devbox:8080`. Full VS Code. Extensions persist. |
| 3 | `helix` | `hx` | Post-modern editor | Built-in LSP, no plugins needed. Kakoune-inspired. Rust. |
| 4 | `nano` | `nano` | Simple editor | Fallback for quick edits. |

### SET 5: `git` — Git and Code Collaboration (Default: ON)

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `git` | `git` | Version control | Pre-configured: delta as pager, default branch `main`. |
| 2 | `lazygit` | `lazygit` | TUI git client | Visual staging, branching, rebasing, conflict resolution. Best way to review AI commits. |
| 3 | `gh` | `gh` | GitHub CLI | PRs, issues, workflows. Bridge for AI auto-submissions. |
| 4 | `git-lfs` | `git-lfs` | Large file storage | Binary assets, ML models, datasets. |
| 5 | `pre-commit` | `pre-commit` | Git hook manager | Linters, formatters, type checkers on commit. |
| 6 | `git-absorb` | `git-absorb` | Auto-fixup commits | Automatically amends into the right commit. |

### SET 6: `container` — Container and Virtualization (Default: ON)

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `docker` | `docker` | Container runtime | Non-root access. `dev` user in `docker` group. |
| 2 | `docker-compose` | `docker compose` | Multi-container orchestration | Local dev stacks. |
| 3 | `lazydocker` | `lazydocker` | TUI Docker manager | Visual container/image/volume management. |
| 4 | `dive` | `dive` | Image layer analyzer | Find bloat in Docker images. |
| 5 | `incus` | `incus` | Container/VM manager | Nested virtualization inside devbox. Linux only. |
| 6 | `skopeo` | `skopeo` | Image operations | Copy between registries, inspect without pulling. |
| 7 | `buildah` | `buildah` | OCI image builder | Rootless, daemonless builds. |

### SET 7: `network` — Networking and Remote Access (Default: OFF)

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `tailscale` | `tailscale` | Mesh VPN | Access devbox from any device. Zero-config, NAT-traversal. |
| 2 | `mosh` | `mosh` | Mobile shell | Survives network changes. High-latency friendly. |
| 3 | `nmap` | `nmap` | Network scanner | Port scanning, service detection. |
| 4 | `tcpdump` | `tcpdump` | Packet capture | Low-level network debugging. |
| 5 | `dog` | `dog` | DNS lookup | Modern dig replacement. DoH/DoT support. |
| 6 | `wireguard-tools` | `wg` | WireGuard VPN | Manual VPN if Tailscale unavailable. |
| 7 | `socat` | `socat` | Multipurpose relay | Port forwarding, socket proxying. |

### SET 8: `ai` — AI Engines and MCP (Default: OFF)

> Installed separately because AI tools need API keys and are large.

| # | Package | Binary | Purpose | Install Method | Notes |
|---|---------|--------|---------|----------------|-------|
| 1 | `claude-code` | `claude` | Anthropic AI coding CLI | `npm i -g @anthropic-ai/claude-code` | Primary AI assistant. Needs `ANTHROPIC_API_KEY`. |
| 2 | `aider-chat` | `aider` | AI pair programmer | `pip install aider-chat` | Multi-file refactoring. Supports multiple backends. |
| 3 | `ollama` | `ollama` | Local LLM runtime | Nix package | Run DeepSeek-R1, Llama locally. No API key needed. |
| 4 | `open-webui` | — (web) | LLM web UI | Docker or pip | ChatGPT-like interface for Ollama at `http://devbox:3000`. |
| 5 | `mcp-server-filesystem` | — (MCP) | File system MCP | npm | AI tools read/write files via MCP. |
| 6 | `mcp-server-github` | — (MCP) | GitHub MCP | npm | AI tools manage PRs/issues via MCP. Needs `GITHUB_TOKEN`. |
| 7 | `mcp-server-postgres` | — (MCP) | PostgreSQL MCP | npm | AI tools query databases via MCP. |
| 8 | `mcp-server-sqlite` | — (MCP) | SQLite MCP | npm | AI tools interact with SQLite via MCP. |
| 9 | `codex` | `codex` | OpenAI Codex CLI | `npm i -g @openai/codex` | Needs `OPENAI_API_KEY`. |
| 10 | `opencode` | `opencode` | OpenCode TUI | `go install` | Beautiful TUI for AI coding. |

Note: AI tools from npm/pip/go install via their native managers inside a Nix-managed runtime.

### SET 9: `lang-go` — Go Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `go` | `go` | Go compiler + toolchain (latest stable) |
| 2 | `gopls` | `gopls` | Go language server (LSP) |
| 3 | `golangci-lint` | `golangci-lint` | Linter aggregator (50+ linters) |
| 4 | `delve` | `dlv` | Go debugger |
| 5 | `gore` | `gore` | Go REPL |
| 6 | `gotools` | `goimports`... | Official Go tools |

### SET 10: `lang-rust` — Rust Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `rustup` | `rustup`, `rustc`, `cargo` | Toolchain manager |
| 2 | `rust-analyzer` | `rust-analyzer` | Rust LSP |
| 3 | `cargo-watch` | `cargo watch` | Auto-rebuild on save |
| 4 | `cargo-edit` | `cargo add/rm` | Dependency management |
| 5 | `cargo-nextest` | `cargo nextest` | Fast test runner (3x cargo test) |
| 6 | `sccache` | `sccache` | Compilation cache |

### SET 11: `lang-python` — Python Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `python3` | `python3`, `pip` | Python interpreter (3.12+) |
| 2 | `uv` | `uv` | Ultra-fast package manager (10-100x pip) |
| 3 | `ruff` | `ruff` | Linter + formatter (replaces flake8+black+isort) |
| 4 | `pyright` | `pyright` | Type checker + LSP |
| 5 | `poetry` | `poetry` | Project manager |
| 6 | `ipython` | `ipython` | Enhanced REPL |

### SET 12: `lang-node` — Node.js Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `nodejs` | `node`, `npm` | Node.js runtime (LTS) |
| 2 | `bun` | `bun` | Fast runtime + bundler + pkg manager |
| 3 | `pnpm` | `pnpm` | Efficient package manager |
| 4 | `typescript` | `tsc` | TypeScript compiler |
| 5 | `biome` | `biome` | JS/TS linter + formatter (Rust-based) |
| 6 | `nodePackages.tsx` | `tsx` | TypeScript executor |

### SET 13: `lang-java` — Java Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `jdk` | `java`, `javac` | JDK (latest LTS, OpenJDK) |
| 2 | `gradle` | `gradle` | Modern build tool |
| 3 | `maven` | `mvn` | Classic build tool |
| 4 | `jdt-language-server` | `jdtls` | Java LSP |

### SET 14: `lang-ruby` — Ruby Development (On-Demand)

| # | Nix Package | Binary | Purpose |
|---|-------------|--------|---------|
| 1 | `ruby` | `ruby`, `gem`, `irb` | Ruby interpreter |
| 2 | `bundler` | `bundle` | Dependency manager |
| 3 | `solargraph` | `solargraph` | Ruby LSP |
| 4 | `rubocop` | `rubocop` | Linter + formatter |

### Package Count Summary

| Set | Packages | Default State |
|-----|----------|---------------|
| system | 29 | Always ON (locked) |
| shell | 11 | Always ON (locked) |
| tools | 21 | Always ON (locked) |
| editor | 4 | ON |
| git | 6 | ON |
| container | 7 | ON |
| network | 7 | OFF |
| ai | 10 | OFF |
| lang-go | 6 | On-demand / detected |
| lang-rust | 6 | On-demand / detected |
| lang-python | 6 | On-demand / detected |
| lang-node | 6 | On-demand / detected |
| lang-java | 4 | On-demand / detected |
| lang-ruby | 4 | On-demand / detected |
| **Total** | **127** | |

---

## 8. Zellij Layouts — Pre-built Workspaces

Devbox ships 8 pre-built Zellij layouts. Users select on first launch or via `devbox shell --layout <name>`.

### Layout Files

```
~/.config/zellij/
  config.kdl                    # Global config (keybindings, theme)
  themes/
    devbox.kdl                  # Custom dark theme
  layouts/
    default.kdl                 # Home base
    ai-pair.kdl                 # AI pair programming
    fullstack.kdl               # Frontend + backend + DB + logs
    tdd.kdl                     # Test-driven development
    debug.kdl                   # Debugging deep-dive
    monitor.kdl                 # System monitoring dashboard
    git-review.kdl              # Code review workflow
    presentation.kdl            # Clean demo mode
  plugins/
    devbox-status.wasm          # Custom status bar
```

### Layout 1: `default` — Home Base

Auto-starts when you `devbox shell`. Clean, simple, productive.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  nvim .                            |  $ _                      |
|  (editor - 60%)                    |  (terminal)               |
|                                    |                           |
|                                    +---------------------------+
|                                    |                           |
|                                    |  yazi /workspace          |
|                                    |  (file manager)           |
|                                    |                           |
+------------------------------------+---------------------------+
| devbox:myapp | docker | go1.23 | git:main +2 | CPU 12%       |
+------------------------------------------------------------------+
Tabs: [workspace] [shell] [git(lazygit)]
```

```kdl
// layouts/default.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/devbox-status.wasm"
        }
        children
    }

    tab name="workspace" focus=true {
        pane split_direction="vertical" {
            pane name="editor" size="60%" {
                command "nvim"
                args "."
            }
            pane split_direction="horizontal" {
                pane name="terminal" size="60%" focus=true
                pane name="files" size="40%" {
                    command "yazi"
                    args "/workspace"
                }
            }
        }
    }

    tab name="shell" {
        pane name="main"
    }

    tab name="git" {
        pane {
            command "lazygit"
        }
    }
}
```

### Layout 2: `ai-pair` — AI Pair Programming

The killer layout. AI assistant on the left, your code in the middle, output on the right.

```
+-------------------+-------------------+-------------------+
|                   |                   |                   |
|  claude           |  nvim .           |  $ make test     |
|  (AI - 30%)       |  (editor - 40%)   |  (output - 30%)  |
|                   |                   |                   |
|  > "Fix the       |                   |  42 passed       |
|    auth..."       |                   |  1 failed        |
|                   |                   |                   |
+-------------------+-------------------+-------------------+
| [MCP] filesystem.read /workspace/src/auth.go              |
+-----------------------------------------------------------+
| AI-PAIR | claude-code | go1.23 | git:feat/auth | CPU 34%  |
+-----------------------------------------------------------+
Tabs: [ai-pair] [aider] [git(lazygit)]
```

```kdl
// layouts/ai-pair.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/devbox-status.wasm"
        }
        children
    }

    tab name="ai-pair" focus=true {
        pane split_direction="vertical" {
            pane name="ai" size="30%" {
                command "claude"
            }
            pane name="editor" size="40%" {
                command "nvim"
                args "."
            }
            pane name="output" size="30%"
        }
        pane name="mcp-logs" size=6 {
            command "tail"
            args "-f" "/tmp/devbox-mcp.log"
        }
    }

    tab name="aider" {
        pane split_direction="vertical" {
            pane name="aider" size="50%" {
                command "aider"
            }
            pane name="diff" size="50%" {
                command "watch"
                args "-n1" "git diff --stat"
            }
        }
    }

    tab name="git" {
        pane {
            command "lazygit"
        }
    }
}
```

### Layout 3: `fullstack` — Full Stack Development

Frontend, backend, database, and logs — all visible at once.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  $ go run ./cmd/server             |  $ npm run dev            |
|  (backend)                         |  (frontend)               |
|                                    |                           |
+------------------------------------+---------------------------+
|                                    |                           |
|  lazydocker                        |  $ http :8080/api/health  |
|  (containers)                      |  (api test)               |
|                                    |                           |
+------------------------------------+---------------------------+
| request GET /api/users 200 4ms   (logs)                       |
+-----------------------------------------------------------+
| FULLSTACK | docker:3 | go+node | git:main | CPU 23%       |
+-----------------------------------------------------------+
Tabs: [dev] [editor(nvim)] [git(lazygit)]
```

```kdl
// layouts/fullstack.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/devbox-status.wasm"
        }
        children
    }

    tab name="dev" focus=true {
        pane split_direction="horizontal" {
            pane split_direction="vertical" size="60%" {
                pane name="backend" size="50%"
                pane name="frontend" size="50%"
            }
            pane split_direction="vertical" size="25%" {
                pane name="containers" size="50%" {
                    command "lazydocker"
                }
                pane name="api-test" size="50%"
            }
            pane name="logs" size="15%"
        }
    }

    tab name="editor" {
        pane {
            command "nvim"
            args "."
        }
    }

    tab name="git" {
        pane {
            command "lazygit"
        }
    }
}
```

### Layout 4: `tdd` — Test-Driven Development

Code on the left, auto-running tests on the right. Red-green-refactor loop.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  nvim .                            |  watchexec -- make test   |
|  (editor - 50%)                    |  (auto tests - 50%)      |
|                                    |                           |
|                                    |  PASS TestAuth            |
|                                    |  FAIL TestToken           |
|                                    |                           |
+------------------------------------+---------------------------+
| coverage: 78.3% (+2.1%)                                      |
+-----------------------------------------------------------+
| TDD | 41 pass 1 fail | coverage 78% | git:feat/auth       |
+-----------------------------------------------------------+
Tabs: [tdd] [git(lazygit)]
```

```kdl
// layouts/tdd.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="file:~/.config/zellij/plugins/devbox-status.wasm"
        }
        children
    }

    tab name="tdd" focus=true {
        pane split_direction="horizontal" {
            pane split_direction="vertical" size="80%" {
                pane name="editor" size="50%" {
                    command "nvim"
                    args "."
                }
                pane name="tests" size="50%" {
                    command "watchexec"
                    args "-e" "go,rs,py,ts,js" "--" "make" "test"
                }
            }
            pane name="coverage" size="20%"
        }
    }

    tab name="git" {
        pane {
            command "lazygit"
        }
    }
}
```

### Layout 5: `debug` — Debugging Deep-Dive

For when something is seriously broken. All inspection tools in one view.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  nvim (with DAP)                   |  dlv debug ./cmd/server   |
|  (source)                          |  (debugger)               |
|                                    |                           |
+------------------------------------+---------------------------+
|                                    |                           |
|  tail -f app.log                   |  btm (system monitor)     |
|  (logs)                            |                           |
+------------------------------------+---------------------------+
| bandwhich (network per-process)                               |
+-----------------------------------------------------------+
| DEBUG | dlv attached | PID 4201 | 3 breakpoints | CPU 45% |
+-----------------------------------------------------------+
```

### Layout 6: `monitor` — System Monitoring Dashboard

A glanceable operations dashboard. Great on a secondary monitor.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  htop                              |  lazydocker               |
|  (processes)                       |  (containers)             |
|                                    |                           |
+------------------------------------+---------------------------+
|                                    |                           |
|  btm (bottom)                      |  bandwhich                |
|  (resources)                       |  (network)                |
|                                    |                           |
+-----------------------------------------------------------+
| MONITOR | 3 containers | CPU 34% | MEM 4.1G | NET 2.4M   |
+-----------------------------------------------------------+
```

### Layout 7: `git-review` — Code Review

Review PRs, browse diffs, manage branches — all visual.

```
+------------------------------------+---------------------------+
|                                    |                           |
|  lazygit                           |  delta (diff viewer)      |
|  (branches, commits, files)        |  (syntax-highlighted)     |
|                                    |                           |
+------------------------------------+---------------------------+
| gh pr view 123 --comments                                     |
| #123: Add OAuth2 support (3 files, +142 -31)                 |
+-----------------------------------------------------------+
| REVIEW | PR #123 | 3 files | +142 -31 | 2 comments        |
+-----------------------------------------------------------+
```

### Layout 8: `presentation` — Clean Demo Mode

Minimal, large font-friendly. For screenshares, demos, pair sessions.

```
+-----------------------------------------------------------+
|                                                           |
|                                                           |
|                        $ _                                |
|                                                           |
|               (single clean pane)                         |
|               (large text, no clutter)                    |
|                                                           |
|                                                           |
+-----------------------------------------------------------+
| devbox:myapp | go1.23 | git:main                         |
+-----------------------------------------------------------+
```

### Layout Picker TUI

On first `devbox shell`, if no default layout is set:

```
+-- Choose your workspace layout -------------------------+
|                                                         |
|  > default         Clean workspace: editor+term+files   |
|    ai-pair         AI assistant + editor + output       |
|    fullstack       Frontend + backend + DB + logs       |
|    tdd             Editor + auto-running tests          |
|    debug           Source + debugger + logs + system     |
|    monitor         System monitoring dashboard          |
|    git-review      Code review: lazygit + diff + PR     |
|    presentation    Minimal clean mode for demos         |
|    plain           No layout, just a shell              |
|                                                         |
|  [Up/Down] Select  [Enter] Launch  [d] Set as default  |
+---------------------------------------------------------+
```

### Devbox Status Bar Plugin (Wasm)

Custom Zellij Wasm plugin showing:

```
devbox:myapp | docker | go1.23 node20 | git:main +2 *1 | CPU 12% MEM 2.1G | 2h14m
```

Fields: sandbox name/status, runtime, detected languages, git branch/status, resource usage, session uptime.

### Layout CLI Commands

```bash
devbox shell                          # Default layout
devbox shell --layout ai-pair        # Specific layout
devbox shell --layout none           # Plain shell
devbox layout list                    # Show all with descriptions
devbox layout preview ai-pair        # ASCII preview
devbox layout edit tdd               # Open in $EDITOR
devbox layout create my-layout       # Create new
devbox layout set-default ai-pair    # Change default
```

---

## 9. Configuration — devbox.toml

Optional declarative config. Auto-generated by `devbox init` or first `devbox` run. Commit to git for team-wide consistency.

```toml
# devbox.toml

[sandbox]
runtime = "auto"           # "auto" | "incus" | "lima"
image = "ubuntu:24.04"
layout = "default"         # Default zellij layout

[sets]
# Toggle which Nix sets are active
system = true              # Locked: always true
shell = true               # Locked: always true
tools = true               # Locked: always true
editor = true
git = true
container = true
network = false
ai = false

[languages]
# Auto-detected from project files, or explicit
go = true                  # Detected: go.mod found
node = false
rust = false
python = false
java = false
ruby = false

[mounts]
workspace = { host = ".", target = "/workspace", readonly = false }
# data = { host = "./data", target = "/data", readonly = true }

[resources]
cpu = 0                    # 0 = unlimited
memory = ""                # "" = unlimited

[env]
# Inherit from host (true) or set explicit value
# ANTHROPIC_API_KEY = true
# NODE_ENV = "development"

[custom_packages]
# Additional Nix packages beyond sets
# terraform = "nixpkgs#terraform"
# my-tool = "github:user/flake#pkg"
```

### `--tools` Behavior

`--tools` **adds to** auto-detected languages. It does NOT replace auto-detection. Only `--bare` suppresses detection.

```bash
# In a directory with go.mod:
devbox                         # Installs lang-go (auto-detected)
devbox --tools claude-code     # Installs lang-go + ai set (claude-code)
devbox --bare                  # Nothing auto-detected, minimal install
```

---

## 10. TUI Package Manager

`devbox packages` opens an interactive TUI for managing installed tools.

### Set View

```
+-- devbox packages ------------------------------------------+
|                                                              |
|  SETS                  PACKAGES              STATUS          |
|  ---------------------------------------------------------- |
|  # system (locked)     29 packages           active         |
|  # shell  (locked)     11 packages           active         |
|  # tools  (locked)     21 packages           active         |
|  # editor              4 packages            active         |
|  # git                 6 packages            active         |
|  # container           7 packages            active         |
|  . network             7 packages            off            |
|  . ai                  10 packages           off            |
|  . lang-go             6 packages            active         |
|  . lang-rust           6 packages            off            |
|  ---------------------------------------------------------- |
|  Custom: 2 packages (user-added)                            |
|                                                              |
|  [Space] Toggle set  [Enter] Browse packages                |
|  [a] Add custom pkg  [n] Add nix flake  [q] Quit           |
+--------------------------------------------------------------+
```

### Package Detail View

```
+-- devbox packages > tools ----------------------------------+
|                                                              |
|  PACKAGE           BINARY     STATUS    DESCRIPTION          |
|  ---------------------------------------------------------- |
|  # ripgrep         rg         active    Ultra-fast search    |
|  # fd              fd         active    File finding         |
|  # bat             bat        active    Syntax viewer        |
|  # eza             eza        active    Dir listing          |
|  # delta           delta      active    Diff viewer          |
|  ...                                                         |
|  ---------------------------------------------------------- |
|                                                              |
|  [Space] Toggle package  [i] Info  [Esc] Back  [q] Quit    |
+--------------------------------------------------------------+
```

---

## 11. Security Model

### What Is Isolated

| Layer | Isolation | Notes |
|-------|-----------|-------|
| Filesystem | Full | Only mounted dirs visible in VM |
| Processes | Full | VM processes invisible to host |
| Network | Open | AI tools need API access |
| Kernel | Full | Separate kernel (VM, not container) |

### Safety Features

- Warn if mounted directory has uncommitted git changes
- Auto-snapshot before `upgrade` operations
- `--mount dir:ro` for explicit read-only
- `--env` inherits specific host vars, not all of env
- `--env-file` reads `.env` but never copies it into VM
- VM names prefixed with `devbox-` to avoid collisions
- `devbox doctor` diagnoses common issues

### Error Handling

Tool installation failures are **warnings, not errors**. The sandbox is still usable.

```
$ devbox
  OK Detected: Go 1.23 (go.mod), Node 20 (package.json)
  OK Runtime: Incus (VM)
  OK Creating sandbox 'myapp'... done (8.2s)
  OK Installing Go 1.23... done
  !! Installing Node 20... failed (npm registry unreachable)
     -> Sandbox ready. Run 'devbox upgrade' to retry Node later.

  /workspace $
```

---

## 12. Project Structure

```
devbox/
  Cargo.toml
  src/
    main.rs                     # Entry point, clap CLI
    cli/
      mod.rs
      create.rs                 # devbox create
      shell.rs                  # devbox shell (+ layout picker)
      exec.rs                   # devbox exec
      stop.rs                   # devbox stop
      destroy.rs                # devbox destroy
      list.rs                   # devbox list
      status.rs                 # devbox status
      snapshot.rs               # devbox snapshot
      upgrade.rs                # devbox upgrade
      config.rs                 # devbox config
      doctor.rs                 # devbox doctor
      layout.rs                 # devbox layout
      packages.rs               # devbox packages (TUI)
      prune.rs                  # devbox prune
      init.rs                   # devbox init
      nix.rs                    # devbox nix add/remove
    runtime/
      mod.rs                    # Runtime trait + auto-detection
      incus.rs                  # Incus VM implementation
      lima.rs                   # Lima VM implementation
    sandbox/
      mod.rs                    # Sandbox manager (lifecycle)
      state.rs                  # ~/.devbox/ state persistence
      config.rs                 # devbox.toml read/write
    tools/
      mod.rs
      detect.rs                 # Project language detection
      registry.rs               # Tool/set definitions
    nix/
      mod.rs                    # Nix set manager
      sets.rs                   # Set composition logic
      profile.rs                # Nix profile operations
    tui/
      mod.rs                    # TUI framework
      packages.rs               # Package manager TUI
      layout_picker.rs          # Layout selection TUI
  nix/
    flake.nix                   # Master Nix flake
    flake.lock
    sets/
      default.nix               # Set loader
      system.nix                # System set definition
      shell.nix                 # Shell set definition
      tools.nix                 # Tools set definition
      editor.nix                # Editor set definition
      git.nix                   # Git set definition
      container.nix             # Container set definition
      network.nix               # Network set definition
      ai.nix                    # AI set definition
      lang-go.nix               # Go language set
      lang-rust.nix             # Rust language set
      lang-python.nix           # Python language set
      lang-node.nix             # Node.js language set
      lang-java.nix             # Java language set
      lang-ruby.nix             # Ruby language set
  layouts/
    config.kdl                  # Zellij global config
    themes/
      devbox.kdl                # Devbox theme
    default.kdl
    ai-pair.kdl
    fullstack.kdl
    tdd.kdl
    debug.kdl
    monitor.kdl
    git-review.kdl
    presentation.kdl
  plugins/
    devbox-status/              # Zellij Wasm status bar plugin
      Cargo.toml
      src/main.rs
  docs/
    plans/
      2026-03-07-devbox-v2-design.md
    reference/
      2026-03-06-devbox-original-design.md
```

**Language:** Rust (single binary, cross-platform)

**Dependencies:** clap (CLI), ratatui (TUI), tokio (async), serde (config), toml (devbox.toml)

**Runtime calls:** All via safe subprocess invocation (no native SDK dependencies)

---

## 13. Open Questions

| # | Question | Recommendation |
|---|----------|----------------|
| 1 | Should `devbox.toml` auto-generate on first run? | Yes. First `devbox` creates it. Users can `.gitignore` it if unwanted. |
| 2 | Should `devbox doctor` be v1? | Yes. Small to implement, saves huge support burden. |
| 3 | Neovim distro: NVChad or AstroNvim? | Let user choose during `devbox init`. Default: AstroNvim (easier for beginners). |
| 4 | Ghostty inside VM? | Ghostty is a GUI app. Ship the config so it works if user has Ghostty on host. Inside VM, zellij is the multiplexer. |
| 5 | Tool version pinning? | v1: latest. v2: `devbox.toml` supports version constraints per tool. |
| 6 | Nix garbage collection? | Auto-run `nix-collect-garbage` weekly via systemd timer. Configurable. |

---

## 14. Changes from Original Design

| Area | Original (2026-03-06) | Revised (2026-03-07) |
|------|----------------------|----------------------|
| **Language** | Go | Rust |
| **Runtime** | Docker > Incus > Lima (containers first) | Incus VM / Lima VM only (VMs only) |
| **Isolation** | Container (shared kernel) | VM (separate kernel) |
| **Persistence** | Container state (fragile) | VM state (persistent, durable) |
| **Tool management** | apt/manual install | Nix sets (declarative, composable) |
| **Shell** | bash | Zsh + autosuggestions + syntax-highlighting + Starship |
| **Multiplexer** | tmux (mentioned) | Zellij (primary) + pre-built layouts |
| **Base tools** | Minimal (git, curl, jq, build-essential) | 127 tools across 14 composable sets |
| **Default command** | `devbox create` | Smart `devbox` (create-or-attach) |
| **`--tools` behavior** | Replaces auto-detection | Adds to auto-detection |
| **Priority numbers** | Docker=10 "highest first" (contradictory) | Higher = preferred (Incus=20, Lima=10) |
| **Missing commands** | No exec, no status, no upgrade | Added: exec, status, upgrade, doctor, prune, layout, packages, nix |
| **Env vars** | Not addressed | `--env`, `--env-file`, per-project .envrc via direnv |
| **Error handling** | Not specified | Warnings on tool install failure, continue |
| **Config file** | Not in v1 | `devbox.toml` (optional, auto-generated) |
| **Package management** | Manual | TUI package manager + Nix sets |
| **Workspace layouts** | Not considered | 8 pre-built Zellij layouts with TUI picker |
