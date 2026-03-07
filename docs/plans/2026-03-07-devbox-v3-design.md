# Devbox v3 — The Ultimate Developer VM

> Comprehensive, implementation-ready design document for a standalone, NixOS-powered, VM-based developer environment CLI.

**Goal:** One command to create a fully-loaded, safe, persistent developer VM on any local machine. Pre-installed with every modern tool a developer needs. Language packs added incrementally. Beautiful terminal UX with pre-built workspace layouts. Safety-first: host files are never at risk.

**Status:** Design finalized. Ready for implementation.

**Language:** Rust (single binary, maximum performance, cross-platform)

**Prior art:**
- [Original devbox design (2026-03-06)](../reference/2026-03-06-devbox-original-design.md)
- [Devbox v2 design (2026-03-07)](./2026-03-07-devbox-v2-design.md)

---

## Table of Contents

1. [Problem Statement](#1-problem-statement)
2. [Design Philosophy](#2-design-philosophy)
3. [User Experience — Progressive Disclosure](#3-user-experience--progressive-disclosure)
4. [Architecture](#4-architecture)
5. [Runtime Model](#5-runtime-model)
6. [NixOS Base Image](#6-nixos-base-image)
7. [Tool Definition Layer — Nix Sets](#7-tool-definition-layer--nix-sets)
8. [Complete Tool Catalog](#8-complete-tool-catalog)
9. [Zellij Layouts — Pre-built Workspaces](#9-zellij-layouts--pre-built-workspaces)
10. [Configuration — devbox.toml](#10-configuration--devboxtoml)
11. [TUI Package Manager](#11-tui-package-manager)
12. [Security Model](#12-security-model)
13. [Built-in Quick Reference System](#13-built-in-quick-reference-system)
14. [Project Structure](#14-project-structure)
15. [Implementation Scope](#15-implementation-scope)
16. [Open Questions](#16-open-questions)
17. [Changes from Previous Designs](#17-changes-from-previous-designs)

---

## 1. Problem Statement

AI coding tools (Claude Code, Codex, Aider, etc.) need to execute arbitrary commands: install packages, run builds, modify files, run tests. On a developer's host machine, this is risky — a careless `rm -rf`, a bad `npm install`, or a rogue build script can damage the system.

Beyond safety, developers waste hours configuring environments: installing tools, managing versions, setting up shell configs, and debugging "works on my machine" issues.

Developers need:
- A safe, isolated environment where AI tools **cannot damage the host** — even by accident
- A fully-loaded modern dev environment out of the box
- Zero-config for the common case, deep customization when needed
- Persistent state (installed tools survive reboots)
- Cross-platform: macOS and Linux
- Extreme terminal UX: beautiful, fast, productive from second one

**Non-goals:** Remote machine management, multi-tenant access, agent orchestration, fleet provisioning. These are solved by higher-level tools (e.g., Holonex `hx dev`).

---

## 2. Design Philosophy

### Small and Beautiful, Then Broad

Do fewer things, but do each one to the extreme. Every feature that ships must feel polished and complete. If a capability cannot be done well, it is not done at all. Breadth is added only after depth is achieved.

### Safety First

The host machine is sacred. The default configuration must make it **impossible** for any process inside the VM — human or AI — to damage the host filesystem. Safety is not a feature to be toggled; it is the foundation.

### Progressive Disclosure

Simple by default, powerful when needed. No flags for the common case. Discoverable advanced features.

- **Beginner:** `devbox` — just works
- **Intermediate:** `devbox --tools claude-code,go` — customize
- **Advanced:** `devbox create --runtime incus --cpu 4 --memory 8G` — full control

### Out-of-the-Box Delight

Every tool a modern developer expects is pre-installed. The first `devbox` session should feel like opening a fully configured IDE, not a bare terminal.

### NixOS-Powered Declarative Management

The VM runs NixOS. All tools, services, and configurations are declared in Nix. Every change is atomic, reproducible, and rollback-safe. Users never touch Nix directly — devbox provides a friendly CLI/TUI on top.

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
$ devbox create --writable                 # Opt-in to direct host writes (bypass OverlayFS)
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
$ devbox commit                             # Sync OverlayFS changes to host
$ devbox diff                               # Show pending changes vs host
$ devbox discard                            # Throw away overlay changes
$ devbox help zellij                        # Quick reference for any tool
```

### Naming Convention

Default name: derived from the project directory name.
- `~/projects/myapp` -> sandbox named `myapp`
- Collision: error with "use `devbox shell` to reattach or `devbox destroy` first"
- Override: `--name custom-name`

### What Happens on First `devbox`

1. Detect best available runtime (Incus on Linux, Lima on macOS, Docker as fallback)
2. Pull pre-built NixOS VM image (contains all core sets, ~2GB compressed)
3. Create a persistent VM from the image
4. Mount current directory to `/workspace` via OverlayFS (host = read-only lower, writes go to VM overlay)
5. Scan project files for language detection (`go.mod` -> activate `lang-go` set)
6. Apply detected language sets via `nixos-rebuild switch`
7. Launch Zellij with default layout
8. Drop into `/workspace` ready to code

**First boot time target:** < 30 seconds (image pull is one-time; subsequent creates < 10 seconds).

### Empty Directory Behavior

If `devbox` is run in a directory with no recognizable project files:
- Display a confirmation: "No project files detected. Create a general-purpose sandbox? [Y/n]"
- If confirmed, create sandbox with core sets only, no language sets

---

## 4. Architecture

```
+------------------------------------------------------+
|  CLI Layer (clap)                                     |
|  devbox | create | shell | exec | stop | destroy      |
|  list | snapshot | status | upgrade | config           |
|  layout | packages | doctor | prune | init | nix       |
|  commit | diff                                         |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Sandbox Manager                                      |
|  - Smart default logic (create-or-attach)             |
|  - State persistence (~/.devbox/ + devbox.toml)       |
|  - NixOS configuration orchestration                  |
|  - Tool set management                                |
|  - Layout management                                  |
|  - OverlayFS sync (commit/diff)                       |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Nix Set Manager                                      |
|  - Set composition (toggle sets on/off)               |
|  - Individual package management                      |
|  - External flake integration                         |
|  - NixOS generation rollback                          |
+------------------------------------------------------+
           |
+------------------------------------------------------+
|  Runtime Trait                                         |
|  create() start() stop() exec_cmd() destroy()         |
|  snapshot_create() snapshot_restore() list()           |
|  is_available() priority() upgrade()                  |
+------------------------------------------------------+
     |                |                |
+---------+     +---------+     +---------+
|  Incus  |     |  Lima   |     | Docker  |
| (Linux) |     | (macOS) |     |(fallback)|
+---------+     +---------+     +---------+
```

### Runtime Trait (Rust)

```rust
#[async_trait]
pub trait Runtime: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    fn priority(&self) -> u32;  // Higher = preferred

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
    pub writable: bool,     // Direct host write (bypass OverlayFS)
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
- **Docker:** `docker run`, `docker exec` (fallback mode)

No native SDK dependencies. Every operation is a visible subprocess call — easy to debug, easy to understand.

### Runtime Auto-Detection

| Host OS | Runtime | Priority | Detection | Isolation Level |
|---------|---------|----------|-----------|-----------------|
| Linux | Incus (VM mode) | 30 | `incus info` succeeds | Full (separate kernel) |
| macOS | Lima | 20 | `limactl` exists | Full (HVF VM) |
| macOS | Multipass | 15 | `multipass` exists | Full (HVF VM) |
| Any | Docker | 10 | `docker info` succeeds | Partial (shared kernel) |

Selection logic:
- `--runtime <name>`: use specified, error if unavailable
- No flag: pick highest priority available
- Docker selected: print warning "⚠ Docker provides weaker isolation (shared kernel). For full isolation, install Incus (Linux) or Lima (macOS)."
- None available: print install instructions for recommended runtime

---

## 5. Runtime Model

### Why VMs First, Docker as Fallback

VMs are the primary runtime because:

1. **Persistence:** VM state survives reboots. Installed tools, config changes, cached downloads — all persist.
2. **Full system:** VMs run a real init system (systemd). Services like Docker, Tailscale, Ollama run as daemons.
3. **Stronger isolation:** Separate kernel. A rogue process in the VM cannot escape to the host.
4. **Nested virtualization:** Can run Docker, Incus, and other container runtimes inside the VM.
5. **Network stack:** Full network namespace with its own IP. Tailscale can give it a routable address.

Docker is kept as a fallback because many developers already have it installed. When Docker is used:
- Print a clear warning about weaker isolation
- Functionality is identical, but snapshot support uses `docker commit` (less efficient)
- OverlayFS protection still applies (see Security Model)

### Incus (Linux — Primary)

```bash
incus launch devbox-nixos devbox-<name> --vm
incus config device add devbox-<name> workspace disk source=<host> path=/workspace/lower readonly=true
incus exec devbox-<name> -- sudo -u dev zsh
```

- Native VM support via QEMU/KVM
- Excellent snapshot support: `incus snapshot create devbox-<name> <snap>`
- Resource limits via `incus config set`
- Mounts via disk devices (read-only for OverlayFS lower layer)

### Lima (macOS — Primary)

```bash
limactl create --name devbox-<name> devbox-nixos.yaml
limactl shell devbox-<name>
```

- HVF-based VM on Apple Silicon
- Mounts via virtiofs (fast, native) — mounted read-only for OverlayFS
- Lima YAML config for resource limits, port forwarding

### Multipass (macOS — Secondary)

```bash
multipass launch --name devbox-<name> file://devbox-nixos.img
multipass mount <host> devbox-<name>:/workspace/lower --type native
multipass shell devbox-<name>
```

- Canonical's VM manager, free and open-source
- Simple API, good Apple Silicon support
- Less configurable than Lima, but zero-config

### Docker (Any — Fallback)

```bash
docker run -d --name devbox-<name> \
  -v <host>:/workspace/lower:ro \
  devbox-nixos:latest sleep infinity
docker exec -it devbox-<name> sudo -u dev zsh
```

- Widest compatibility (most developers already have Docker)
- Weaker isolation (shared kernel) — explicit warning shown
- Snapshots via `docker commit` (slower, larger)
- Host directory mounted read-only; OverlayFS in container provides write layer

---

## 6. NixOS Base Image

### Why NixOS Instead of Ubuntu + Nix

| Aspect | Ubuntu + Nix (v2 approach) | NixOS (v3 approach) |
|--------|---------------------------|---------------------|
| **First boot** | Create Ubuntu VM → install Nix → pull 127 packages (5-10 min) | Pull pre-built NixOS image with all core sets baked in (< 30 sec) |
| **Service management** | Manual systemd unit files + Nix packages | `services.docker.enable = true;` — one line, fully managed |
| **Atomic upgrades** | `nix profile install` (per-user, imperative) | `nixos-rebuild switch` (system-wide, declarative, atomic) |
| **Rollback** | `nix profile rollback` (package-level only) | Boot into any previous NixOS generation (full system rollback) |
| **Consistency** | Two package managers (apt + nix), potential conflicts | Single source of truth: `configuration.nix` |
| **Shell config** | Manual `.zshrc` generation | `home-manager` module: declarative, reproducible |
| **Reproducibility** | Drift over time as imperative changes accumulate | Entire system state described in one config file |

### NixOS Configuration Structure

The VM's entire state is defined by a NixOS configuration:

```nix
# /etc/nixos/configuration.nix (inside VM)
{ config, pkgs, lib, ... }:

let
  devboxSets = import /etc/devbox/sets { inherit pkgs; };
  devboxConfig = builtins.fromTOML (builtins.readFile /etc/devbox/devbox-state.toml);
in {
  # ── System Foundation ──────────────────────────────
  system.stateVersion = "24.11";
  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  boot.loader.systemd-boot.enable = true;
  networking.hostName = "devbox";
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # ── User ───────────────────────────────────────────
  users.users.dev = {
    isNormalUser = true;
    home = "/home/dev";
    shell = pkgs.zsh;
    extraGroups = [ "wheel" "docker" "networkmanager" ];
  };
  security.sudo.wheelNeedsPassword = false;

  # ── Core Packages (always installed) ───────────────
  environment.systemPackages =
    devboxSets.system
    ++ devboxSets.shell
    ++ devboxSets.tools
    ++ (lib.optionals devboxConfig.sets.editor devboxSets.editor)
    ++ (lib.optionals devboxConfig.sets.git devboxSets.git)
    ++ (lib.optionals devboxConfig.sets.container devboxSets.container)
    ++ (lib.optionals devboxConfig.sets.network devboxSets.network)
    ++ (lib.optionals devboxConfig.sets.ai devboxSets.ai)
    ++ (lib.optionals devboxConfig.languages.go devboxSets.lang-go)
    ++ (lib.optionals devboxConfig.languages.rust devboxSets.lang-rust)
    ++ (lib.optionals devboxConfig.languages.python devboxSets.lang-python)
    ++ (lib.optionals devboxConfig.languages.node devboxSets.lang-node)
    ++ (lib.optionals devboxConfig.languages.java devboxSets.lang-java)
    ++ (lib.optionals devboxConfig.languages.ruby devboxSets.lang-ruby);

  # ── Services ───────────────────────────────────────
  virtualisation.docker.enable = devboxConfig.sets.container;
  services.openssh.enable = true;
  services.tailscale.enable = devboxConfig.sets.network;

  # ── Shell Environment (home-manager) ───────────────
  home-manager.users.dev = import /etc/devbox/home.nix { inherit pkgs; };

  # ── OverlayFS for /workspace ───────────────────────
  fileSystems."/workspace" = {
    device = "overlay";
    fsType = "overlay";
    options = [
      "lowerdir=/workspace/lower"
      "upperdir=/workspace/upper"
      "workdir=/workspace/work"
    ];
  };

  # ── Nix Garbage Collection ─────────────────────────
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
}
```

### Home Manager Configuration

```nix
# /etc/devbox/home.nix
{ pkgs, ... }:
{
  programs.zsh = {
    enable = true;
    autosuggestion.enable = true;
    syntaxHighlighting.enable = true;
    shellAliases = {
      ls = "eza --icons";
      cat = "bat --paging=never";
      top = "htop";
      diff = "delta";
      # Note: find/grep aliases only in interactive mode, not in scripts
    };
    initExtra = ''
      # Devbox identity
      export DEVBOX_NAME="''${DEVBOX_NAME:-devbox}"
      export DEVBOX_RUNTIME="''${DEVBOX_RUNTIME:-unknown}"

      # Modern tool aliases (interactive only, won't break scripts)
      if [[ $- == *i* ]]; then
        alias f='fd'
        alias g='rg'
      fi
    '';
  };

  programs.starship = {
    enable = true;
    settings = {
      format = "$custom$directory$git_branch$git_status$golang$rust$python$nodejs$line_break$character";
      custom.devbox = {
        command = "echo $DEVBOX_NAME";
        when = "true";
        format = "[$output]($style) ";
        style = "bold blue";
      };
    };
  };

  programs.zoxide = {
    enable = true;
    enableZshIntegration = true;
  };

  programs.fzf = {
    enable = true;
    enableZshIntegration = true;
  };

  programs.git = {
    enable = true;
    delta.enable = true;
    extraConfig = {
      init.defaultBranch = "main";
      push.autoSetupRemote = true;
    };
  };

  programs.zellij = {
    enable = true;
  };
}
```

### Pre-built Image Pipeline

The NixOS VM image is built via `nixos-generators` and published as a release artifact:

```bash
# Build pipeline (CI/CD)
nixos-generate -f qcow2 -c ./devbox-image.nix    # For Incus/QEMU
nixos-generate -f raw -c ./devbox-image.nix       # For Lima
docker build -t devbox-nixos:latest .              # For Docker fallback
```

The image includes:
- NixOS 24.11 with all core sets (system + shell + tools + editor + git + container) pre-installed
- Home-manager configuration pre-applied
- Zellij + all layout files
- Starship theme
- Nerd fonts
- All Nix store closures cached (no network needed for core tools)

**Image size target:** ~2GB compressed, ~5GB uncompressed.

**Update strategy:** `devbox self-update` pulls new image; existing VMs updated via `nixos-rebuild switch` (preserves user state).

---

## 7. Tool Definition Layer — Nix Sets

### How It Works

1. **NixOS is the VM's OS** — Nix is native, not bolted on
2. **Devbox defines "sets"** — curated lists of Nix packages for specific purposes
3. **Sets are toggled on/off** — via `devbox.toml` or `devbox packages` TUI
4. **Toggling a set triggers `nixos-rebuild switch`** — atomic, rollback-safe
5. **Users can add custom packages** from Nixpkgs or external flakes
6. **`devbox.toml` captures selections** — committable to git for team consistency
7. **Full system rollback via NixOS generations** — any change can be undone

### Nix Sets Structure

```nix
# /etc/devbox/sets/default.nix
{ pkgs }:
{
  system      = import ./system.nix { inherit pkgs; };
  shell       = import ./shell.nix { inherit pkgs; };
  tools       = import ./tools.nix { inherit pkgs; };
  editor      = import ./editor.nix { inherit pkgs; };
  git         = import ./git.nix { inherit pkgs; };
  container   = import ./container.nix { inherit pkgs; };
  network     = import ./network.nix { inherit pkgs; };
  ai          = import ./ai.nix { inherit pkgs; };
  lang-go     = import ./lang-go.nix { inherit pkgs; };
  lang-rust   = import ./lang-rust.nix { inherit pkgs; };
  lang-python = import ./lang-python.nix { inherit pkgs; };
  lang-node   = import ./lang-node.nix { inherit pkgs; };
  lang-java   = import ./lang-java.nix { inherit pkgs; };
  lang-ruby   = import ./lang-ruby.nix { inherit pkgs; };
}
```

### Set Toggle Flow

```
User: devbox upgrade --tools rust
  ↓
CLI: Update /etc/devbox/devbox-state.toml (languages.rust = true)
  ↓
CLI: Execute in VM: sudo nixos-rebuild switch
  ↓
NixOS: Read updated config → compute new package set → atomic switch
  ↓
CLI: Done. Rust toolchain available immediately.
  ↓
(If anything fails: nixos-rebuild switch --rollback)
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

## 8. Complete Tool Catalog

### SET 1: `system` — OS Foundation (Always Installed, Locked)

> The bedrock. NixOS provides most of these natively; this set ensures consistent versions and availability.

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `coreutils` | `ls`, `cp`, `mv`... | GNU core utilities | Standard POSIX tools |
| 2 | `util-linux` | `mount`, `lsblk`... | Linux system utilities | Disk, partition, process tools |
| 3 | `gcc` | `gcc`, `g++` | C/C++ compiler | Required by packages with native extensions |
| 4 | `gnumake` | `make` | Build automation | Universal build tool |
| 5 | `pkg-config` | `pkg-config` | Library path resolver | Required when compiling against system libraries |
| 6 | `openssl` | `openssl` | TLS/SSL library + CLI | Needed by curl, git, and all network tools |
| 7 | `cacert` | — (CA bundle) | CA certificates | HTTPS trust store |
| 8 | `openssh` | `ssh`, `sshd`, `ssh-keygen` | SSH client + server | Git SSH, remote access, key management |
| 9 | `iproute2` | `ip`, `ss` | Network configuration | View/configure interfaces |
| 10 | `procps` | `ps`, `pgrep`, `kill` | Process utilities | Basic process inspection |
| 11 | `findutils` | `find`, `xargs` | File search (classic) | Fallback when fd is not appropriate |
| 12 | `gawk` | `awk` | Text processing | Shell scripting essential |
| 13 | `gnused` | `sed` | Stream editor | Config file manipulation |
| 14 | `gnutar` | `tar` | Archive tool | Extract tarballs |
| 15 | `gzip` | `gzip`, `gunzip` | Compression | `.tar.gz` archives |
| 16 | `unzip` | `unzip` | ZIP extraction | GitHub releases, etc. |
| 17 | `xz` | `xz` | XZ compression | `.tar.xz` archives |
| 18 | `zstd` | `zstd` | Zstandard compression | Used by Nix itself |
| 19 | `curl` | `curl` | HTTP client | Universal download/API tool |
| 20 | `wget` | `wget` | HTTP downloader | Some scripts require wget specifically |
| 21 | `less` | `less` | Pager | Default for git, man pages |
| 22 | `man-db` | `man` | Manual pages | `man git`, `man docker` |
| 23 | `file` | `file` | File type detection | Identifies file types |
| 24 | `which` | `which` | Binary path lookup | Check tool availability |

> Note: `systemd`, `glibc`, `sudo`, `locale`, `iptables` are managed by NixOS itself and not listed as explicit packages. NixOS handles these at the system level.

### SET 2: `shell` — Terminal and Shell Environment (Always Installed, Locked)

> The developer's home. This is where the "extreme UX" lives.

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `zellij` | `zellij` | Terminal multiplexer + Wasm plugins | Modern tmux replacement. Layout system, session persistence, floating panes. Auto-starts on shell entry. |
| 2 | `zsh` | `zsh` | Z Shell (default shell) | Superior completion, globbing, plugin ecosystem. |
| 3 | `zsh-autosuggestions` | — (plugin) | Fish-like input suggestions | Shows grayed-out completion from history. Accept with right-arrow. |
| 4 | `zsh-syntax-highlighting` | — (plugin) | Real-time command coloring | Valid = green, invalid = red. Catches typos before Enter. |
| 5 | `starship` | `starship` | Cross-shell prompt | Shows devbox name, git branch, language versions, exit code. |
| 6 | `zoxide` | `z` | Smart directory jumper | Learns cd patterns. `z proj` -> `/workspace/project`. |
| 7 | `fzf` | `fzf` | Fuzzy finder | Ctrl+R: history. Ctrl+T: files. `**<TAB>`: paths. |
| 8 | `yazi` | `yazi` | Terminal file manager | Fast, image preview, bulk operations. Built in Rust. |
| 9 | `nerd-fonts` | — (fonts) | Developer icon fonts | Required by eza, yazi, starship for icons. FiraCode Nerd Font. |
| 10 | `tmux` | `tmux` | Classic terminal multiplexer | Fallback for tmux users. Not the default. |

> Note: Ghostty is a GUI terminal emulator and does not belong inside a headless VM. Devbox ships a recommended Ghostty config file for users who have Ghostty on their host machine (see `~/.config/ghostty/config` in the devbox repo).

**Shell configuration** is managed declaratively via home-manager (see section 6). Key features:
- Zsh with autosuggestions + syntax highlighting (via home-manager modules)
- Starship prompt showing devbox name, git branch, language versions
- Modern tool aliases (`ls`→`eza`, `cat`→`bat`, `top`→`htop`, `diff`→`delta`)
- Interactive-only aliases for `find`/`grep` replacements (`f`→`fd`, `g`→`rg`) to avoid breaking scripts
- fzf and zoxide integrations

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
| 1 | `neovim` | `nvim` | Terminal IDE | Pre-configured with AstroNvim. LSP, Treesitter, telescope, AI plugins. |
| 2 | `helix` | `hx` | Post-modern editor | Built-in LSP, no plugins needed. Kakoune-inspired. Rust. |
| 3 | `nano` | `nano` | Simple editor | Fallback for quick edits. |

> Note: `code-server` (VS Code in browser) removed from default set. Can be added via `devbox nix add nixpkgs#code-server` if needed. Focus is on terminal-native editors for the best in-VM experience.

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
| 1 | `docker` | `docker` | Container runtime | Managed by NixOS: `virtualisation.docker.enable = true`. `dev` user in `docker` group. |
| 2 | `docker-compose` | `docker compose` | Multi-container orchestration | Local dev stacks. |
| 3 | `lazydocker` | `lazydocker` | TUI Docker manager | Visual container/image/volume management. |
| 4 | `dive` | `dive` | Image layer analyzer | Find bloat in Docker images. |
| 5 | `skopeo` | `skopeo` | Image operations | Copy between registries, inspect without pulling. |
| 6 | `buildah` | `buildah` | OCI image builder | Rootless, daemonless builds. |

> Note: Incus removed from container set — running Incus inside a VM is unnecessary complexity for v1.

### SET 7: `network` — Networking and Remote Access (Default: OFF)

| # | Nix Package | Binary | Purpose | Notes |
|---|-------------|--------|---------|-------|
| 1 | `tailscale` | `tailscale` | Mesh VPN | Access devbox from any device. Managed by NixOS: `services.tailscale.enable`. |
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

### Language Sets 10-14 (Future — Added Incrementally)

The following language sets follow the same pattern as `lang-go` and will be added as development resources allow:

| Set | Key Packages | Status |
|-----|-------------|--------|
| `lang-rust` | rustup, rust-analyzer, cargo-watch, cargo-edit, cargo-nextest, sccache | Planned |
| `lang-python` | python3, uv, ruff, pyright, poetry, ipython | Planned |
| `lang-node` | nodejs, bun, pnpm, typescript, biome, tsx | Planned |
| `lang-java` | jdk, gradle, maven, jdtls | Planned |
| `lang-ruby` | ruby, bundler, solargraph, rubocop | Planned |

Each language set will be implemented and released independently. The set mechanism is universal — adding a new language is purely a matter of defining its Nix package list.

### Package Count Summary

| Set | Packages | Default State |
|-----|----------|---------------|
| system | 24 | Always ON (locked) |
| shell | 10 | Always ON (locked) |
| tools | 21 | Always ON (locked) |
| editor | 3 | ON |
| git | 6 | ON |
| container | 6 | ON |
| network | 7 | OFF |
| ai | 10 | OFF |
| lang-go | 6 | On-demand / detected |
| **Total (v1)** | **93** | |

---

## 9. Zellij Layouts — Pre-built Workspaces

Devbox ships pre-built Zellij layouts. Users select on first launch or via `devbox shell --layout <name>`.

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
| devbox:myapp | go1.23 | git:main +2 | CPU 12%                 |
+-------------------------------------------------------------------+
Tabs: [workspace] [shell] [git(lazygit)]
```

```kdl
// layouts/default.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
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
| AI-PAIR | claude-code | go1.23 | git:feat/auth | CPU 34%  |
+-----------------------------------------------------------+
Tabs: [ai-pair] [aider] [git(lazygit)]
```

```kdl
// layouts/ai-pair.kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
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
Tabs: [dev] [editor(nvim)] [git(lazygit)]
```

### Layout 4: `tdd` — Test-Driven Development

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
Tabs: [tdd] [git(lazygit)]
```

### Layout 5: `debug` — Debugging Deep-Dive

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
```

### Layout 6: `monitor` — System Monitoring Dashboard

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
```

### Layout 7: `git-review` — Code Review

```
+------------------------------------+---------------------------+
|                                    |                           |
|  lazygit                           |  delta (diff viewer)      |
|  (branches, commits, files)        |  (syntax-highlighted)     |
|                                    |                           |
+------------------------------------+---------------------------+
| gh pr view 123 --comments                                     |
+-----------------------------------------------------------+
```

### Layout 8: `presentation` — Clean Demo Mode

```
+-----------------------------------------------------------+
|                                                           |
|                        $ _                                |
|               (single clean pane)                         |
|               (large text, no clutter)                    |
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

## 10. Configuration — devbox.toml

Optional declarative config. Auto-generated by `devbox init` or first `devbox` run. Commit to git for team-wide consistency.

```toml
# devbox.toml

[sandbox]
runtime = "auto"           # "auto" | "incus" | "lima" | "multipass" | "docker"
layout = "default"         # Default zellij layout
mount_mode = "overlay"     # "overlay" = safe (host read-only, devbox commit to sync)
                           # "writable" = direct (real-time read-write, multi-agent friendly)

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

## 11. TUI Package Manager

`devbox packages` opens an interactive TUI for managing installed tools.

### Set View

```
+-- devbox packages ------------------------------------------+
|                                                              |
|  SETS                  PACKAGES              STATUS          |
|  ---------------------------------------------------------- |
|  # system (locked)     24 packages           active         |
|  # shell  (locked)     10 packages           active         |
|  # tools  (locked)     21 packages           active         |
|  # editor              3 packages            active         |
|  # git                 6 packages            active         |
|  # container           6 packages            active         |
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

### Toggle Flow

When a user toggles a set or package:
1. TUI updates `devbox-state.toml` inside the VM
2. TUI runs `nixos-rebuild switch` in the background
3. Progress bar shows rebuild status
4. On success: package list refreshes, new tools available immediately
5. On failure: auto-rollback, error message displayed

---

## 12. Security Model

### Threat Model

The primary threat is **accidental destruction by AI coding tools or build scripts**. The secondary threat is a malicious dependency (e.g., postinstall script in npm) attempting to exfiltrate or damage host files.

### Defense: OverlayFS Workspace Protection

**This is the core safety mechanism.** By default, the host directory is never written to directly.

```
Host Machine                         VM
─────────────                        ──
~/projects/myapp/ ──mount──> /workspace/lower (READ-ONLY)
                                     │
                              OverlayFS merges:
                              ├── lower: /workspace/lower (host, ro)
                              ├── upper: /workspace/upper (VM-local, rw)
                              └── work:  /workspace/work  (OverlayFS internal)
                                     │
                              /workspace/ ← user sees merged view
```

**How it works:**
- The host directory is mounted **read-only** into the VM at `/workspace/lower`
- OverlayFS creates a merged view at `/workspace` where reads come from the host and writes go to `/workspace/upper` (VM-local storage)
- From the user's perspective, `/workspace` looks and feels like a normal read-write directory
- But the host filesystem is **never modified**

**Syncing changes back to host:**
```bash
devbox diff                    # Show what changed (like git diff)
devbox commit                  # Sync overlay changes back to host
devbox commit --dry-run        # Preview what would be synced
devbox commit --path src/      # Sync only specific paths
devbox discard                 # Throw away all overlay changes
devbox discard --path tmp/     # Discard specific paths
```

**Opt-out for power users:**
```bash
devbox create --writable       # Direct host writes, no OverlayFS
```

Or in `devbox.toml`:
```toml
[sandbox]
writable = true
```

When `--writable` is used, print warning: "⚠ Direct host write mode. Changes are written immediately to your host filesystem."

### Defense: Auto-Snapshot on Entry

Every time `devbox shell` is invoked:
1. Record the current NixOS generation number
2. Create a lightweight VM snapshot (named `auto-<timestamp>`)
3. Auto-snapshots older than 7 days are pruned automatically

This means any `devbox snapshot restore` can roll back the entire VM state — packages, configs, everything.

### Defense: Dangerous Command Interception

A lightweight shell wrapper (installed as a Zsh plugin) intercepts known-dangerous commands **before execution**:

```
$ rm -rf /
⚠ BLOCKED: This command would delete the entire filesystem.
  This is inside a VM, so your host is safe, but your VM state would be lost.
  Use 'devbox snapshot restore' to recover if needed.
  Run with --force to execute anyway.

$ rm -rf /workspace
⚠ WARNING: This would delete all files in /workspace.
  Your host files are protected by OverlayFS (changes are in the overlay layer).
  Proceed? [y/N]

$ chmod -R 777 /
⚠ WARNING: Recursive permission change on root filesystem.
  Proceed? [y/N]
```

This is **not a security boundary** — it's a safety net for common mistakes. A determined user can bypass it. That's fine — the real protection is OverlayFS + VM isolation.

### What Is Isolated

| Layer | Isolation | Notes |
|-------|-----------|-------|
| Filesystem | Full | Host mounted read-only via OverlayFS. VM cannot write to host. |
| Processes | Full | VM processes invisible to host (separate kernel in VM mode) |
| Network | Open | AI tools need API access. No restrictions by default. |
| Kernel | Full (VM) / Shared (Docker) | VM mode: separate kernel. Docker fallback: shared kernel. |

### Environment Variable Handling

Environment variables passed via `--env` or `--env-file` are visible inside the VM:
- They are set in the shell session, not persisted to disk
- They are visible via `/proc/*/environ` inside the VM
- They are **not** visible to the host
- `devbox.toml` `[env]` section can mark vars as `true` (inherit from host) or set explicit values
- Sensitive vars (API keys) should use `--env` rather than `--env-file` to avoid disk persistence

### Docker Fallback Security

When Docker is used instead of a VM:
- Host directory still mounted read-only + OverlayFS inside container
- Kernel is shared (weaker isolation) — warning printed
- Container runs as non-root user `dev`
- No `--privileged` flag
- Snapshots via `docker commit` (functional but slower)

### Multi-VM Collaboration and Mount Modes

When multiple Devbox instances share the same host directory (e.g., multiple AI agents working on the same project), the OverlayFS model introduces a visibility issue: each VM has its own upper layer, so VM-A cannot see VM-B's modifications until they are committed back to the host.

```
Host: ~/projects/myapp/
         │
    ┌────┴────┐
    ▼         ▼
  VM-A       VM-B
  upper-A    upper-B

VM-A edits auth.go    →  only VM-A sees the change
VM-B edits router.go  →  only VM-B sees the change
VM-A commits          →  host updated, but VM-B still sees old auth.go
```

To address this, Devbox supports two mount modes:

**`overlay` mode (default):** Host directory is read-only. All writes go to VM-local overlay. Changes synced back via `devbox commit`. Best for single-agent or human development where safety is the priority.

**`writable` mode:** Host directory is mounted read-write, no OverlayFS. All VMs see each other's changes in real time. Best for multi-agent collaboration where multiple Devbox instances need to coordinate. Safety is delegated to git (commit before starting, revert if needed).

Configuration in `devbox.toml`:

```toml
[sandbox]
# "overlay" = safe mode (default, host read-only, changes via devbox commit)
# "writable" = direct mode (real-time read-write, multi-agent friendly)
mount_mode = "overlay"
```

CLI override:

```bash
devbox create --writable       # Direct host writes, no OverlayFS
devbox create --mount-mode overlay   # Explicit safe mode (default)
```

**Automatic conflict detection:** When creating a new Devbox and the host directory is already mounted by another Devbox instance, the CLI detects this and prompts:

```
⚠ Directory ~/projects/myapp is already mounted by devbox 'myapp-agent1'.
  Overlay mode: each VM has independent changes, invisible to each other.
  Writable mode: all VMs share the same files in real time.

  [o] Overlay (default, safe)
  [w] Writable (multi-agent, real-time)
  [c] Cancel
```

**Persistence clarification:** Regardless of mount mode, all VM state (installed tools, NixOS config, home directory, code changes in overlay upper layer) persists across VM restarts. `devbox commit` is only needed to sync overlay changes back to the host — it is not needed for tools or VM-internal data.

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

## 13. Built-in Quick Reference System

### Problem

Devbox ships with many powerful tools — Zellij, lazygit, Neovim, fzf, yazi, and more. Most developers are not familiar with all of them. Without guidance, users either avoid these tools (wasting the "out-of-the-box delight") or leave the Devbox to search the web for documentation (breaking the workflow).

### Solution: `devbox help` and In-Terminal Cheat Sheets

Every bundled tool has a built-in quick reference card accessible without leaving the terminal. These are concise, visual, and focused on the 20% of features that cover 80% of daily use.

### Access Methods

**Method 1: `devbox help <tool>` command**

```bash
devbox help zellij        # Zellij cheat sheet
devbox help lazygit       # Lazygit cheat sheet
devbox help nvim          # Neovim cheat sheet
devbox help fzf           # fzf cheat sheet
devbox help yazi          # yazi cheat sheet
devbox help git           # Git quick reference
devbox help devbox        # Devbox own commands
devbox help               # List all available cheat sheets
```

Output is rendered via `glow` (terminal Markdown renderer) for a beautiful reading experience. If the terminal is wide enough, the cheat sheet opens in a Zellij floating pane so it stays visible while working.

**Method 2: Zellij hotkey**

Inside any Zellij layout, press `Ctrl+h` (configurable) to open a floating pane with a tool picker:

```
+-- Quick Reference ──────────────────────────+
|                                              |
|  > devbox          Devbox commands           |
|    zellij          Terminal multiplexer       |
|    lazygit         Git TUI                   |
|    nvim            Neovim editor             |
|    fzf             Fuzzy finder              |
|    yazi            File manager              |
|    rg              Ripgrep search            |
|    fd              File finder               |
|    bat             Syntax viewer             |
|    docker          Container commands        |
|    git             Version control           |
|                                              |
|  [↑↓] Select  [Enter] Open  [Esc] Close    |
+----------------------------------------------+
```

The cheat sheet opens in a floating pane that overlays the current workspace. Press `Esc` to dismiss — zero workflow disruption.

**Method 3: Zellij status bar hint**

The status bar shows context-sensitive hints based on the active pane:

```
devbox:myapp | go1.23 | git:main | Ctrl+h: help | Ctrl+p: commands
```

### Cheat Sheet Content Design

Each cheat sheet follows a strict format: essential keybindings first, then common operations, then "next steps." Maximum one screen of content. No scrolling needed for the basics.

**Example: Zellij cheat sheet**

```markdown
# Zellij — Terminal Multiplexer

## Essential Keys (Zellij uses a leader key: Ctrl+<key>)
  Ctrl+p → Pane mode      Ctrl+t → Tab mode
  Ctrl+n → Resize mode    Ctrl+s → Scroll mode
  Ctrl+o → Session mode   Ctrl+h → Help (this menu)

## Pane Operations (press Ctrl+p first, then:)
  n  New pane             d  Down split
  r  Right split          x  Close pane
  f  Toggle fullscreen    w  Toggle floating
  ←↑↓→  Move focus

## Tab Operations (press Ctrl+t first, then:)
  n  New tab              x  Close tab
  r  Rename tab           1-9  Switch to tab N
  ←→  Previous/Next tab

## Quick Actions
  Ctrl+p then f  Fullscreen current pane (great for focusing)
  Ctrl+p then w  Float/unfloat a pane (great for temp terminals)
  Ctrl+t then n  New tab (great for separate workspaces)
```

**Example: lazygit cheat sheet**

```markdown
# lazygit — Git TUI

## Navigation
  Tab / Shift+Tab   Switch panels (files/branches/commits/stash)
  ↑↓  Navigate       Enter  Expand/collapse
  ?   Show all keybindings

## Staging & Committing
  Space  Stage/unstage file     a  Stage all
  c      Commit                 A  Amend last commit
  shift+C  Commit with editor

## Branching
  n  New branch      Space  Checkout branch
  M  Merge into current     r  Rebase

## Everyday Workflow
  1. Navigate to file → Space to stage
  2. Press 'c' → type message → Enter
  3. Press 'P' to push
```

**Example: devbox own commands**

```markdown
# Devbox — Quick Reference

## Everyday Commands
  devbox                Start or attach to sandbox
  devbox stop           Stop sandbox (preserves state)
  devbox diff           Show changes vs host files
  devbox commit         Sync changes back to host

## Workspace
  devbox shell --layout ai-pair    Switch layout
  devbox packages                  Manage tools (TUI)
  devbox exec -- make test         Run one-off command

## Safety
  devbox snapshot save NAME        Create checkpoint
  devbox snapshot restore NAME     Rollback
  devbox discard                   Throw away all changes

## Troubleshooting
  devbox doctor         Diagnose issues
  devbox status         Detailed sandbox info
  devbox help <tool>    Tool-specific help
```

### Cheat Sheet File Structure

```
/etc/devbox/help/
  index.md              # List of all available cheat sheets
  devbox.md             # Devbox own commands
  zellij.md             # Zellij
  lazygit.md            # lazygit
  nvim.md               # Neovim (AstroNvim keybindings)
  fzf.md                # fzf
  yazi.md               # yazi
  rg.md                 # ripgrep
  fd.md                 # fd
  bat.md                # bat
  docker.md             # Docker essentials
  git.md                # Git quick reference
  delta.md              # delta
  httpie.md             # HTTPie
```

These files are plain Markdown, baked into the NixOS image. Users can customize or add their own cheat sheets. The `devbox help` command simply renders the appropriate file via `glow`.

### Implementation

```rust
// cli/help.rs
pub fn run_help(tool: Option<&str>) -> Result<()> {
    let help_dir = Path::new("/etc/devbox/help");
    match tool {
        None => {
            // Show index: list all available cheat sheets
            exec_cmd("glow", &[help_dir.join("index.md").to_str().unwrap()])
        }
        Some(name) => {
            let file = help_dir.join(format!("{}.md", name));
            if file.exists() {
                // Try floating pane first, fall back to direct render
                if is_inside_zellij() {
                    exec_cmd("zellij", &["run", "--floating", "--", "glow", file.to_str().unwrap()])
                } else {
                    exec_cmd("glow", &[file.to_str().unwrap()])
                }
            } else {
                eprintln!("No cheat sheet for '{}'. Run 'devbox help' to see available topics.", name);
                Ok(())
            }
        }
    }
}
```

### Zellij Keybinding Configuration

```kdl
// config.kdl — add to keybindings
keybinds {
    shared {
        bind "Ctrl h" {
            Run "zellij" "run" "--floating" "--" "devbox" "help" {
                close_on_exit true
            }
        }
    }
}
```

---

## 14. Project Structure

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
      commit.rs                 # devbox commit (OverlayFS sync)
      diff.rs                   # devbox diff (OverlayFS diff)
      help.rs                   # devbox help (quick reference system)
    runtime/
      mod.rs                    # Runtime trait + auto-detection
      incus.rs                  # Incus VM implementation
      lima.rs                   # Lima VM implementation
      multipass.rs              # Multipass VM implementation
      docker.rs                 # Docker fallback implementation
    sandbox/
      mod.rs                    # Sandbox manager (lifecycle)
      state.rs                  # ~/.devbox/ state persistence
      config.rs                 # devbox.toml read/write
      overlay.rs                # OverlayFS management (commit/diff/discard)
    tools/
      mod.rs
      detect.rs                 # Project language detection
      registry.rs               # Tool/set definitions
    nix/
      mod.rs                    # NixOS configuration manager
      sets.rs                   # Set composition logic
      rebuild.rs                # nixos-rebuild orchestration
    tui/
      mod.rs                    # TUI framework (ratatui)
      packages.rs               # Package manager TUI
      layout_picker.rs          # Layout selection TUI
  nix/
    flake.nix                   # Master Nix flake for image building
    flake.lock
    image/
      configuration.nix         # Base NixOS VM configuration
      home.nix                  # Home-manager user configuration
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
  help/                         # Built-in quick reference cheat sheets
    index.md                    # List of all available topics
    devbox.md                   # Devbox own commands
    zellij.md                   # Zellij multiplexer
    lazygit.md                  # lazygit TUI
    nvim.md                     # Neovim (AstroNvim)
    fzf.md                      # fzf fuzzy finder
    yazi.md                     # yazi file manager
    rg.md                       # ripgrep
    fd.md                       # fd file finder
    bat.md                      # bat syntax viewer
    docker.md                   # Docker essentials
    git.md                      # Git quick reference
    delta.md                    # delta diff viewer
    httpie.md                   # HTTPie
  host-configs/
    ghostty/config              # Recommended Ghostty config for host
    starship.toml               # Starship config reference
  docs/
    plans/
      2026-03-07-devbox-v3-design.md
    reference/
      2026-03-06-devbox-original-design.md
      2026-03-07-devbox-v2-design.md
```

**Language:** Rust (single binary, cross-platform, maximum performance)

**Dependencies:**
- `clap` — CLI argument parsing
- `ratatui` — TUI framework (package manager, layout picker)
- `tokio` — Async runtime
- `serde` + `toml` — Configuration (devbox.toml)
- `indicatif` — Progress bars and spinners
- `colored` — Terminal colors
- `dialoguer` — Interactive prompts

**Runtime calls:** All via safe subprocess invocation (no native SDK dependencies)

---

## 15. Implementation Scope

### v1: Everything Ships Together

The product philosophy is "small and beautiful" — every feature that ships must be polished. v1 includes the complete feature set described in this document, with one exception: language sets beyond Go are added incrementally.

**v1 includes:**
- CLI: all commands (create, shell, exec, stop, destroy, list, status, snapshot, upgrade, config, doctor, prune, init, nix, commit, diff, discard, layout, packages, help)
- Runtime: Incus + Lima + Multipass + Docker (with auto-detection and fallback)
- NixOS base image: pre-built with all core sets baked in
- Nix set mechanism: toggle sets on/off, `nixos-rebuild switch`
- Core sets: system, shell, tools, editor, git, container (all 6 always available)
- Optional sets: network, ai (toggled via config)
- Language set: lang-go (first language, auto-detected from go.mod)
- Security: OverlayFS workspace protection (overlay/writable mount modes), auto-snapshot, dangerous command interception, multi-VM conflict detection
- Zellij: all 8 layouts + layout picker TUI
- TUI package manager
- Built-in quick reference system (`devbox help`, Ctrl+h floating pane, cheat sheets for all bundled tools)
- devbox.toml configuration
- devbox doctor diagnostics

**Added incrementally after v1:**
- Language sets: lang-rust, lang-python, lang-node, lang-java, lang-ruby
- Each language set is independent and can be developed/released on its own timeline
- Adding a language set requires only: defining the Nix package list + adding detection rules

### Why Not Phase It

Shipping a partial product creates a bad first impression. If the layout picker doesn't work, or the TUI is missing, or OverlayFS isn't there yet — the user's first experience is "this feels incomplete." The only exception is language sets, because:
- The set mechanism itself is complete in v1
- Each language set is just a list of packages — purely additive
- Users who need Rust/Python/Node can use `devbox nix add` immediately
- The auto-detection framework works for any language — only the detection rules need adding

---

## 16. Open Questions

| # | Question | Recommendation |
|---|----------|----------------|
| 1 | Should `devbox.toml` auto-generate on first run? | Yes. First `devbox` creates it. Users can `.gitignore` it if unwanted. |
| 2 | Neovim distro: NVChad or AstroNvim? | AstroNvim (easier for beginners, better defaults). User can switch via `devbox config`. |
| 3 | OverlayFS commit granularity? | `devbox commit` syncs all changes. `devbox commit --path` for selective sync. No auto-sync. |
| 4 | NixOS image distribution? | GitHub Releases for the image. `devbox self-update` pulls new versions. |
| 5 | Tool version pinning? | v1: latest stable. Future: `devbox.toml` supports version constraints per tool. |
| 6 | Nix garbage collection schedule? | Weekly via NixOS systemd timer. Delete generations older than 14 days. Configurable. |
| 7 | Lima vs Multipass default on macOS? | Lima first (more configurable, wider community). Multipass as auto-fallback. |

---

## 17. Changes from Previous Designs

### v1 (2026-03-06) → v2 (2026-03-07) → v3 (2026-03-07)

| Area | v1 Original | v2 Revision | v3 Final |
|------|-------------|-------------|----------|
| **Language** | Go | Rust | Rust |
| **VM Base OS** | Ubuntu 24.04 | Ubuntu 24.04 + Nix | **NixOS** (native, declarative) |
| **Runtime** | Docker > Incus > Lima | Incus VM / Lima VM only | **Incus > Lima > Multipass > Docker** (VM-first, Docker fallback) |
| **Isolation** | Container (shared kernel) | VM only (separate kernel) | **VM primary, Docker fallback** with clear warning |
| **Persistence** | Container state (fragile) | VM state (persistent) | VM state + **NixOS generations** (full system rollback) |
| **Tool management** | apt/manual install | Nix sets (nix profile) | **NixOS modules** (declarative, atomic, system-wide) |
| **Shell config** | bash | Zsh + manual .zshrc | Zsh + **home-manager** (declarative) |
| **Workspace safety** | Warn if uncommitted | Warn if uncommitted | **OverlayFS** (host read-only by default, explicit commit) |
| **Security** | Basic warnings | Basic warnings + snapshot | **OverlayFS + auto-snapshot + command interception** |
| **Ghostty** | Not mentioned | In shell set (always installed) | **Host-side config only** (not in VM) |
| **systemd/glibc** | Not mentioned | In system Nix set | **Managed by NixOS** (not in package sets) |
| **Aliases** | Not mentioned | find→fd, grep→rg (all shells) | **Interactive-only** (f→fd, g→rg) to avoid breaking scripts |
| **Docker** | Primary runtime | Removed entirely | **Fallback runtime** with weaker-isolation warning |
| **macOS fallback** | Lima only | Lima only | **Lima > Multipass** (two free options) |
| **First boot time** | 5-10 min (install everything) | 5-10 min (install Nix + packages) | **< 30 sec** (pre-built NixOS image) |
| **Scope strategy** | MVP then iterate | All features at once | **All features at once**, language sets added incrementally |
| **Philosophy** | Not stated | Not stated | **Small and beautiful, then broad** |
| **Multi-VM** | Not considered | Not considered | **Mount mode selection** (overlay vs writable), conflict detection |
| **User onboarding** | Not considered | Not considered | **Built-in quick reference** (`devbox help`, Ctrl+h floating cheat sheets) |
