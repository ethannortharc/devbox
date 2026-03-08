# Devbox

[![CI](https://github.com/northarc/devbox/actions/workflows/ci.yml/badge.svg)](https://github.com/northarc/devbox/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

NixOS-powered developer VMs. One command to create a fully-loaded, safe, persistent development environment.

```
$ devbox
```

That's it. Devbox auto-detects your project, creates a NixOS VM with the right tools, and drops you into a configured workspace.

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

```bash
# Create and enter a VM (auto-detects project)
cd my-project
devbox

# Or be explicit about tools and layout
devbox create --tools go,docker --layout ai-pair

# Attach to an existing sandbox
devbox shell

# Run a one-off command
devbox exec -- make test
```

### First-time setup

```bash
# Generate a project config file
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
name = "myproject"
runtime = "lima"
layout = "default"

[sets]
core = true
shell = true
tools = true
git = true
containers = true

[languages]
go = true

[mounts]
workspace = { host = ".", container = "/workspace", readonly = true }

[resources]
cpus = 4
memory = "8G"
disk = "50G"

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

Devbox organizes 90+ tools into toggleable sets. Locked sets are always installed.

| Set | Contents | Locked |
|-----|----------|--------|
| **core** | zsh, starship, zoxide, coreutils | Yes |
| **shell** | tmux, zellij, neovim | Yes |
| **tools** | eza, bat, fd, ripgrep, fzf, yazi, delta, jq, yq | Yes |
| **git** | git, lazygit, gh, git-crypt | No |
| **network** | curl, wget, httpie, websocat, mtr | No |
| **containers** | docker, docker-compose, podman, buildah, dive, lazydocker | No |
| **go** | Go toolchain, gopls, golangci-lint, delve | No |
| **rust** | Rust toolchain, rust-analyzer, cargo tools | No |
| **node** | Node.js, npm, pnpm, eslint, prettier | No |
| **python** | Python, pip, ruff, mypy, ipython | No |
| **java** | JDK, Maven, Gradle | No |
| **cloud** | aws-cli, terraform, kubectl, helm, k9s | No |
| **ai** | Claude Code, aider, ollama | No |
| **data** | sqlite, postgresql, redis, jq, csvkit | No |

Toggle sets interactively with the TUI package manager:

```bash
devbox packages
```

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
  nix/          # NixOS integration
    sets.rs     # 14 Nix set definitions (93+ packages)
    rebuild.rs  # nixos-rebuild orchestration with rollback
  tui/          # Terminal UIs (ratatui)
    layout_picker.rs  # Interactive layout selector
    packages.rs       # Package manager TUI
  tools/        # Project detection and tool registry
nix/            # NixOS configuration files
  configuration.nix   # VM system config
  home.nix            # home-manager config
  flake.nix           # Build pipeline
  sets/               # Individual set .nix files
layouts/        # 8 Zellij KDL layout files
help/           # 14 markdown cheat sheets (embedded in binary)
host-configs/   # Host-side configs (Ghostty)
tests/          # Integration tests
```

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

39 unit tests covering: config parsing, state management, runtime backends, Nix set generation, layout registry, cheat sheet embedding, platform detection.

### Integration tests

```bash
cargo test --test integration
```

15 integration tests covering: CLI argument parsing, help output, all subcommand `--help`, guide system, list/doctor/layout commands, error cases.

### End-to-end testing

E2E tests require a VM runtime installed. See the [E2E Test Guide](#e2e-test-guide) below.

---

### E2E Test Guide

These tests verify the full devbox lifecycle against a real runtime. Run them manually.

**Prerequisites:** At least one runtime installed (Lima recommended on macOS, Incus on Linux).

#### 1. System check

```bash
devbox doctor
```

Expected: All checks pass. Runtime detected. State directory exists.

#### 2. Project initialization

```bash
mkdir /tmp/devbox-e2e-test && cd /tmp/devbox-e2e-test
git init && echo 'package main' > main.go && echo 'module test' > go.mod
devbox init
cat devbox.toml
```

Expected: `devbox.toml` generated with `go = true` under `[languages]`.

#### 3. Create a sandbox

```bash
devbox create --name e2e-test --bare
```

Expected: Sandbox created, auto-attached to shell inside VM. Type `exit` to return.

#### 4. List and status

```bash
devbox list
devbox status --name e2e-test
```

Expected: `e2e-test` appears in the list with status `Running` (green) or `Stopped` (yellow).

#### 5. Shell attach

```bash
devbox shell --name e2e-test
# Inside VM:
uname -a        # Should show Linux (NixOS kernel)
which zsh        # Should exist
which nvim       # Should exist
exit
```

Expected: Drops into a shell inside the VM. NixOS system visible.

#### 6. Exec one-off command

```bash
devbox exec --name e2e-test -- echo "hello from VM"
echo $?
```

Expected: Prints `hello from VM`, exit code `0`.

#### 7. Overlay operations (if not --writable)

```bash
# Inside the VM, create a file
devbox exec --name e2e-test -- touch /workspace/upper/testfile.txt

# From host
devbox diff --name e2e-test
devbox discard --name e2e-test --force
devbox diff --name e2e-test
```

Expected: First diff shows `testfile.txt` as added. After discard, diff shows no changes.

#### 8. Snapshot operations (Incus/Multipass only)

```bash
devbox snapshot save --name e2e-test
devbox snapshot list --name e2e-test
devbox snapshot restore --name e2e-test --snapshot <id>
```

Expected: Snapshot created, listed, and restored without errors.

#### 9. Guide system

```bash
devbox guide
devbox guide zellij
devbox guide nvim
devbox guide nonexistent
```

Expected: Index shows all topics. Tool-specific guides render content. Unknown tool prints "No cheat sheet" message.

#### 10. Layout commands

```bash
devbox layout list
devbox layout preview ai-pair
devbox layout preview tdd
```

Expected: Lists all 9 layouts with descriptions. Previews show ASCII diagrams.

#### 11. Config management

```bash
devbox config show
devbox config set runtime lima
devbox config get runtime
devbox config set runtime auto
```

Expected: Shows current config. Set/get round-trips correctly. Reset to `auto`.

#### 12. Stop and destroy

```bash
devbox stop --name e2e-test
devbox status --name e2e-test
devbox destroy --name e2e-test --force
devbox list
```

Expected: Status shows `Stopped`. After destroy, sandbox no longer appears in list.

#### 13. Cleanup

```bash
rm -rf /tmp/devbox-e2e-test
```

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, project structure, and PR guidelines.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.
