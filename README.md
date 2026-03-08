# Devbox

[![CI](https://github.com/northarc/devbox/actions/workflows/ci.yml/badge.svg)](https://github.com/northarc/devbox/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

NixOS-powered developer VMs. One command to create a fully-loaded, safe, persistent development environment.

```
$ devbox
```

That's it. Devbox auto-detects your project, creates a VM with the right tools, and drops you into a configured workspace. Choose NixOS (default, declarative) or Ubuntu (familiar) as your base image -- both use the same 90+ tool catalog from nixpkgs.

---

## Table of Contents

- [Features](#features)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Quick Start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Tool Sets](#tool-sets)
- [Workspace Layouts](#workspace-layouts)
- [Security Model](#security-model)
- [Runtime Support](#runtime-support)
- [Architecture](#architecture)
- [Development](#development)
- [Testing](#testing)
- [Contributing](#contributing)
- [License](#license)

---

## Features

- **Zero-config** -- auto-detects project type and installs the right language tools
- **Safe by default** -- host filesystem mounted read-only via OverlayFS; changes require explicit `devbox commit`
- **Pre-loaded** -- 90+ modern developer tools installed out of the box (neovim, lazygit, ripgrep, fzf, yazi, httpie, and more)
- **Persistent** -- VM state survives reboots; NixOS generations provide full system rollback
- **Fast** -- pre-built NixOS image boots in under 30 seconds
- **Cross-platform** -- macOS (Lima, Multipass) and Linux (Incus, Docker fallback)
- **Workspace layouts** -- 8 pre-built Zellij layouts (ai-pair, fullstack, tdd, debug, and more)
- **TUI package manager** -- interactive set/package toggling with live NixOS rebuild
- **Built-in help** -- quick-reference cheat sheets for every bundled tool (`devbox guide <tool>`)

## Prerequisites

- **Rust** 1.85+ (edition 2024)
- At least one VM runtime installed:
  - [Lima](https://lima-vm.io/) (macOS, recommended)
  - [Incus](https://linuxcontainers.org/incus/) (Linux, recommended)
  - [Multipass](https://multipass.run/) (macOS/Linux)
  - [Docker](https://www.docker.com/) (any platform, fallback)

## Installation

### From source

```bash
git clone https://github.com/northarc/devbox.git
cd devbox
cargo install --path .
```

### Self-update

```bash
devbox self-update          # Update to latest release
devbox self-update --check  # Check for updates without installing
```

## Quick Start

For a detailed walkthrough with explanations of what happens at each step, see the [Quick Start Guide](docs/QUICKSTART.md).

```bash
# Check your system is ready
devbox doctor

# Create and enter a VM (auto-detects project language)
cd my-project
devbox

# Or be explicit about tools, layout, and base image
devbox create --tools go,docker --layout ai-pair
devbox create --image ubuntu --tools python    # Ubuntu base + Nix packages

# Attach to an existing sandbox
devbox shell

# Run a one-off command
devbox exec -- make test
```

### What happens when you run `devbox create`:

1. **Downloads a NixOS image** (~800MB, cached after first run) and creates a VM via Lima/Incus
2. **Pushes NixOS configuration** into the VM (tool sets, language packs, system settings)
3. **Runs `nixos-rebuild switch`** to install all packages declaratively from the Nix binary cache
4. **Copies devbox + help files** into the VM so `devbox guide` works inside
5. **Saves state** to `~/.devbox/sandboxes/<name>/`

First create takes 5-15 minutes (image download + package install). Subsequent creates are faster.

### First-time setup

```bash
# Generate a project config file (auto-detects languages)
devbox init

# Check that your system is ready
devbox doctor

# Optional: set a recommended Ghostty terminal config
cp host-configs/ghostty/config ~/.config/ghostty/config
```

## Commands

| Command | Description |
|---------|-------------|
| `devbox` | Create or attach (smart default) |
| `devbox create` | Create a new sandbox |
| `devbox shell` | Attach to a sandbox |
| `devbox exec <cmd>` | Run a one-off command |
| `devbox stop` | Stop a sandbox |
| `devbox destroy` | Remove a sandbox |
| `devbox list` | List all sandboxes |
| `devbox status` | Show detailed status |
| `devbox upgrade --tools go` | Add tools to a running sandbox |
| `devbox packages` | Open TUI package manager |
| `devbox layout list` | Show available layouts |
| `devbox diff` | Show overlay changes vs host |
| `devbox commit` | Sync overlay changes to host |
| `devbox discard` | Throw away overlay changes |
| `devbox snapshot save` | Create a snapshot |
| `devbox snapshot restore` | Restore a snapshot |
| `devbox guide` | Show tool quick reference |
| `devbox guide <tool>` | Cheat sheet for a specific tool |
| `devbox doctor` | Diagnose issues |
| `devbox self-update` | Update devbox |
| `devbox init` | Generate devbox.toml |
| `devbox config` | Get/set configuration |
| `devbox nix add <pkg>` | Add a Nix package |
| `devbox nix remove <pkg>` | Remove a Nix package |
| `devbox prune` | Remove all stopped sandboxes |

## Configuration

### Project-level (`devbox.toml`)

```toml
[sandbox]
runtime = "auto"            # auto | lima | incus | multipass | docker
image = "nixos"             # nixos (declarative) | ubuntu (familiar)
layout = "default"          # zellij layout name
mount_mode = "overlay"      # overlay (safe) | writable (direct)

[sets]
system = true               # core OS tools (locked, always on)
shell = true                # terminal tools (locked, always on)
tools = true                # modern CLI (locked, always on)
editor = true               # neovim, helix, nano
git = true                  # git, lazygit, gh
container = true            # docker, compose, lazydocker
network = false             # tailscale, mosh, nmap
ai = false                  # claude-code, aider, ollama

[languages]
go = true                   # auto-detected from go.mod
rust = false
python = false
node = false

[mounts.workspace]
host = "."
target = "/workspace"
readonly = false

[resources]
cpu = 4                     # 0 = default (4 cores)
memory = "8GiB"             # "" = default (8GiB)

[env]
EDITOR = "nvim"
```

### Global defaults (`~/.devbox/config.toml`)

```bash
devbox config set runtime lima
devbox config set layout ai-pair
devbox config set tools go,rust
devbox config show
```

## Tool Sets

Devbox organizes 90+ tools into toggleable sets declared as NixOS packages.

### Core sets (always installed)

| Set | Packages |
|-----|----------|
| **system** | coreutils, gcc, gnumake, curl, wget, openssh, openssl, gnupg, tree, less, file, pkg-config, and more (24 packages) |
| **shell** | zellij, zsh, starship, fzf, zoxide, direnv, yazi (10 packages) |
| **tools** | ripgrep, fd, bat, eza, delta, jq, yq, htop, dust, httpie, glow, hyperfine, and more (21 packages) |

### Default sets (on by default, can be disabled)

| Set | Packages |
|-----|----------|
| **editor** | neovim, helix, nano |
| **git** | git, lazygit, gh, git-lfs, git-crypt, pre-commit |
| **container** | docker, docker-compose, lazydocker, dive, buildkit, skopeo |

### Optional sets (off by default)

| Set | Packages |
|-----|----------|
| **network** | tailscale, mosh, nmap, tcpdump, bandwhich, trippy, doggo |
| **ai** | claude-code, aider, ollama, open-webui, codex, huggingface-hub, and more (10 packages) |

### Language sets (enabled by detection or `--tools` flag)

| Set | Packages |
|-----|----------|
| **lang-go** | go, gopls, golangci-lint, delve, gotools, gore |
| **lang-rust** | rustup, rust-analyzer, cargo-watch, cargo-edit, cargo-expand, sccache |
| **lang-python** | python 3.12, uv, ruff, pyright, ipython, pytest |
| **lang-node** | node 22, bun, pnpm, typescript, typescript-language-server, biome |
| **lang-java** | jdk 21, gradle, maven, jdt-language-server |
| **lang-ruby** | ruby 3.3, bundler, solargraph, rubocop |

Toggle sets interactively with the TUI package manager:

```bash
devbox packages
```

For a detailed explanation of how sets work with NixOS, see the [Quick Start Guide](docs/QUICKSTART.md#whats-installed-by-default).

## Workspace Layouts

Pre-built Zellij layouts for common workflows:

| Layout | Description |
|--------|-------------|
| `default` | Editor + terminal split |
| `ai-pair` | Editor + AI assistant + terminal |
| `fullstack` | Frontend, backend, and database panes |
| `tdd` | Editor + test runner side-by-side |
| `debug` | Editor + debugger + logs |
| `monitor` | System metrics dashboard |
| `git-review` | Diff viewer + lazygit + editor |
| `presentation` | Wide editor, minimal chrome |
| `plain` | No Zellij (raw shell) |

```bash
devbox layout list              # List all layouts
devbox layout preview ai-pair   # ASCII preview
devbox create --layout tdd      # Use a layout on create
devbox layout set-default tdd   # Set your default
```

## Security Model

Devbox is designed for safety-first operation:

1. **OverlayFS (default)** -- Host workspace is mounted read-only. All writes happen in an overlay layer inside the VM.
2. **Explicit commit** -- Changes are only synced to the host when you run `devbox commit`.
3. **Diff before commit** -- Review all changes with `devbox diff` before syncing.
4. **Auto-snapshots** -- A snapshot is taken automatically each time you enter the shell.
5. **Rollback** -- NixOS generations allow full system rollback if a tool install goes wrong.
6. **Writable opt-in** -- Pass `--writable` to mount the host directly (use with caution).

```bash
devbox diff                   # See what changed
devbox commit                 # Sync all changes to host
devbox commit --path src/     # Sync only specific paths
devbox discard                # Throw away all changes
devbox snapshot restore <id>  # Roll back to a snapshot
```

## Base Images

Devbox supports two base images. Both install the same 90+ tools from [nixpkgs](https://search.nixos.org/packages).

| Image | Install Method | Rollback | Best For |
|-------|---------------|----------|----------|
| **nixos** (default) | `nixos-rebuild switch` | Full system generations | Declarative, reproducible environments |
| **ubuntu** | Nix package manager | `nix profile rollback` | Familiar base OS, lighter image |

```bash
devbox create --image nixos     # Default: NixOS image + nixos-rebuild
devbox create --image ubuntu    # Ubuntu 24.04 + Nix package manager
```

On **NixOS**, packages are installed system-wide via `nixos-rebuild switch`. The entire OS is declaratively configured. Rollback is instant via NixOS generations.

On **Ubuntu**, the Nix package manager is installed first, then packages are added via `nix profile install`. The base OS is familiar Ubuntu, but all developer tools come from the same nixpkgs repository. Services like Docker are installed via apt for systemd integration.

## Runtime Support

| Runtime | Platform | Isolation | Snapshots | Priority |
|---------|----------|-----------|-----------|----------|
| Incus | Linux | Full VM | Yes | 30 (highest) |
| Lima | macOS | Full VM | Planned | 20 |
| Multipass | macOS/Linux | Full VM | Yes | 15 |
| Docker | Any | Container | No | 10 (fallback) |

Devbox auto-detects the best available runtime. Override with:

```bash
devbox create --runtime lima
devbox config set runtime incus
```

## Architecture

```
src/
  cli/          # 21 CLI command handlers (clap)
  runtime/      # Runtime backends (Lima, Incus, Multipass, Docker)
    mod.rs      # Runtime trait definition
    cmd.rs      # Shared subprocess execution helper
    detect.rs   # Auto-detection with priority scoring
  sandbox/      # Sandbox lifecycle, state persistence, overlay management
    config.rs   # devbox.toml parsing
    state.rs    # ~/.devbox/sandboxes/ state persistence
    overlay.rs  # OverlayFS diff/commit/discard
    provision.rs # NixOS provisioning (push nix files + nixos-rebuild)
  nix/          # NixOS integration
    sets.rs     # 14 Nix set definitions (93+ packages)
    rebuild.rs  # nixos-rebuild orchestration with rollback
  tui/          # Terminal UIs (ratatui)
    layout_picker.rs  # Interactive layout selector
    packages.rs       # Package manager TUI
  tools/        # Project detection and tool registry
nix/            # NixOS configuration files (embedded in binary)
  devbox-module.nix   # NixOS module pushed into VMs
  configuration.nix   # Standalone config (for custom image builds)
  home.nix            # home-manager config
  flake.nix           # Build pipeline
  sets/               # 14 individual set .nix files
layouts/        # 8 Zellij KDL layout files
help/           # 14 markdown cheat sheets (embedded in binary)
host-configs/   # Host-side configs (Ghostty)
docs/           # Quick start, E2E test guide
tests/          # Integration tests
```

### How provisioning works

All `.nix` files and help files are compiled into the devbox binary via `include_str!`. When you create a sandbox:

1. Lima creates and boots a stock NixOS VM (from [nixos-lima](https://github.com/nixos-lima/nixos-lima) images)
2. Devbox pushes the nix files into the VM at `/etc/devbox/` via base64-encoded shell commands
3. The devbox NixOS module is imported into the VM's `/etc/nixos/configuration.nix`
4. `nixos-rebuild switch` installs all declared packages
5. The devbox binary and help files are copied into the VM

## Development

### Build

```bash
cargo build            # Debug build
cargo build --release  # Release build
```

### Format and lint

```bash
cargo fmt              # Auto-format
cargo fmt --check      # Check formatting (CI)
cargo clippy           # Lint
cargo clippy -- -D warnings  # Lint with warnings as errors (CI)
```

## Testing

### Unit tests

```bash
cargo test
```

51 unit tests covering: config parsing, state management, runtime backends, Nix set generation, NixOS/Ubuntu provisioning, package name mapping, layout registry, cheat sheet embedding, platform detection.

### Integration tests

```bash
cargo test --test integration
```

15 integration tests covering: CLI argument parsing, help output, all subcommand `--help`, guide system, list/doctor/layout commands, error cases.

### End-to-end testing

E2E tests require a VM runtime installed. See the [E2E Test Guide](docs/E2E_TEST_GUIDE.md) for detailed steps.

---

### E2E Test Guide

The full [E2E Test Guide](docs/E2E_TEST_GUIDE.md) covers 13 test scenarios including:

- System check, project init, sandbox create/destroy lifecycle
- Tool verification inside the VM (zellij, ripgrep, lazygit, etc.)
- Language tool installation (Go, Rust, Python, etc.)
- Guide system, layout commands, config management
- Overlay operations and snapshots
- Troubleshooting nixos-rebuild failures

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, project structure, and PR guidelines.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.
