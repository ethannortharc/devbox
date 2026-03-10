# Devbox

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

**The sandbox that AI coding agents deserve.** Isolated developer VMs where Claude, Codex, and Aider can write, build, and test code freely -- without ever touching your host machine.

```bash
cd my-project
devbox
```

That's it. Devbox detects your project type, provisions a NixOS VM with [120+ tools](docs/PACKAGES.md), and drops you into a workspace with AI coding assistants, a brainstorming panel, file browser, and git -- all pre-configured and ready to go.

---

## How It Works

```bash
cd my-project
devbox                          # 1. Create sandbox (auto-detects Go, Rust, Python, etc.)

# ... AI agent writes code, installs packages, does whatever it wants ...

devbox diff                     # 2. See exactly what changed
devbox commit                   # 3. Accept the good changes
devbox discard                  # 3. Or throw everything away
```

Your project directory is mounted **read-only** inside the VM. Every file write goes to an isolated overlay layer. Nothing reaches your real files until you explicitly run `devbox commit`. It's like a code review for your entire filesystem.

> **Claude just `rm -rf`'d your src directory?**
> With devbox: `devbox discard`. Done. Your files were never touched.

---

## Why Devbox?

| Without Devbox | With Devbox |
|----------------|-------------|
| AI agent deletes your files | `devbox discard` — instant recovery |
| Agent installs conflicting deps | Each sandbox is isolated with its own packages |
| Dev tools pollute your host OS | Everything lives in disposable VMs — zero residue |
| "It works on my machine" | Reproducible NixOS VMs with declarative config |
| Reviewing AI changes is painful | `devbox diff` shows every change, `devbox commit --path src/` accepts selectively |
| Security and compliance concerns | Full VM boundary with audit trail |

---

## Default Workspace

![Default Workspace](docs/screenshot-workspace.png)

Four tabs, ready to go: **Workspace** (AI coding + brainstorm + file browser), **DevBox** (monitor + help + management), **Shell** (plain terminal), and **Git** (lazygit).

---

## Customizable Layouts

Devbox uses [Zellij](https://zellij.dev/) for workspace layouts. Pick a built-in layout or create your own in minutes.

| Layout | Description |
|--------|-------------|
| `default` | AI assistant + brainstorm + file browser + monitor + git |
| `ai-pair` | AI coding + editor + terminal (pair programming) |
| `fullstack` | Frontend, backend, and database panes |
| `tdd` | Editor + test runner side-by-side |
| `debug` | Editor + debugger + logs |
| `monitor` | System metrics dashboard |
| `git-review` | Diff viewer + lazygit + editor |
| `presentation` | Wide editor, minimal chrome |

```bash
devbox layout list                    # See all layouts
devbox layout preview ai-pair         # ASCII preview
devbox create --layout tdd            # Use a layout on create
devbox layout set-default tdd         # Set your global default
```

**Create your own layout:**

```bash
devbox layout create my-workflow      # Generates ~/.devbox/layouts/my-workflow.kdl
devbox layout edit my-workflow        # Opens in your $EDITOR
```

Layouts are simple KDL files — define panes, commands, and splits. Your custom layouts override built-ins and are automatically available across all sandboxes.

```kdl
// Example: custom two-pane layout
layout {
    tab name="Dev" {
        pane split_direction="vertical" {
            pane name="editor" size="60%" {
                command "nvim"
                args "."
            }
            pane name="terminal" size="40%"
        }
    }
}
```

---

## Remote Access via SSH

Devbox VMs run a full SSH server, making them accessible from any machine on your network. This is useful for headless servers, remote development, or managing sandboxes from a different workstation.

```bash
# SSH into a sandbox directly (Lima)
ssh -p $(limactl show-ssh --format=port devbox-myapp) $(whoami)@localhost

# Or use Lima's built-in shortcut
limactl shell devbox-myapp

# Incus VMs
incus exec devbox-myapp -- bash
```

**SSH agent forwarding** is enabled by default on Lima, so your host SSH keys (for GitHub, GitLab, etc.) work seamlessly inside the sandbox — no need to copy keys.

**Port forwarding** for web development:

```bash
# Forward port 3000 from the sandbox to your host
ssh -L 3000:localhost:3000 -p $(limactl show-ssh --format=port devbox-myapp) $(whoami)@localhost

# Or use Lima's port forwarding (auto-forwards common ports)
# Access your dev server at localhost:3000 from the host browser
```

**Remote team workflow:**

```bash
# On the server: create a sandbox
devbox create --name shared-api --tools go,docker

# From your laptop: SSH in and attach
ssh yourserver -t "devbox shell --name shared-api"
```

---

## Quick Start

### Prerequisites

- A VM runtime:
  - [Lima](https://lima-vm.io/) (macOS, recommended)
  - [Incus](https://linuxcontainers.org/incus/) (Linux, recommended)
  - [Multipass](https://multipass.run/) (macOS/Linux)
  - [Docker](https://www.docker.com/) (any platform, fallback)

### Install

```bash
curl -fsSL https://raw.githubusercontent.com/ethannortharc/devbox/main/install.sh | sh
```

Or build from source (requires Rust 1.85+):

```bash
git clone https://github.com/ethannortharc/devbox.git
cd devbox
cargo install --path .
```

### Verify your system

```bash
devbox doctor
```

### Create your first sandbox

```bash
# Auto-detect project and create sandbox
cd my-project
devbox

# Or be explicit
devbox create --name myapp --tools go,docker --layout ai-pair

# Ubuntu base image instead of NixOS
devbox create --image ubuntu --tools python
```

### Common workflows

```bash
# Attach to an existing sandbox
devbox shell --name myapp

# Run a one-off command inside the sandbox
devbox exec --name myapp -- make test

# See what files changed in the overlay
devbox diff

# Sync overlay changes back to host
devbox commit

# Discard all changes (safe reset)
devbox discard

# Stop or destroy
devbox stop --name myapp
devbox destroy --name myapp
```

### Managing tools

```bash
devbox upgrade --tools rust       # Add Rust toolchain to running sandbox
devbox packages                   # Open TUI package manager
devbox nix add <package>          # Add any nixpkgs package
devbox guide lazygit              # Show cheat sheet for a tool
```

---

## Security Model

Devbox prioritizes protecting your host filesystem and providing safe, reversible workflows.

| Layer | Protection |
|-------|-----------|
| **OverlayFS isolation** | Host project directory mounted read-only. All writes go to an overlay layer inside the VM. |
| **Explicit commit** | Changes sync to host only when you run `devbox commit`. Review first with `devbox diff`. |
| **Snapshot & rollback** | Auto-snapshots on shell attach. NixOS generations allow full system rollback. |
| **VM boundary** | Full VM isolation (not containers). Your host OS is never modified. |
| **Credential safety** | No credentials are stored in the sandbox state. API keys are passed via environment variables, never written to disk. |
| **Writable opt-in** | Direct host mount requires explicit `--writable` flag. Default is always safe overlay mode. |

```bash
devbox diff                      # Review overlay changes
devbox commit                    # Sync to host
devbox commit --path src/        # Sync only specific paths
devbox discard                   # Throw away all changes
devbox snapshot restore <id>     # Roll back to a snapshot
```

---

## Commands

| Command | Description |
|---------|-------------|
| `devbox` | Create or attach (smart default) |
| `devbox create` | Create a new sandbox |
| `devbox shell` | Attach to a sandbox |
| `devbox exec <cmd>` | Run a command inside the sandbox |
| `devbox stop` | Stop a sandbox |
| `devbox destroy` | Remove a sandbox |
| `devbox list` | List all sandboxes |
| `devbox status` | Show detailed sandbox status |
| `devbox use <name>` | Switch sandbox to current directory |
| `devbox upgrade --tools <set>` | Add tools to a running sandbox |
| `devbox packages` | Open TUI package manager |
| `devbox diff` | Show overlay changes vs host |
| `devbox commit` | Sync overlay changes to host |
| `devbox discard` | Throw away overlay changes |
| `devbox layer status` | Overlay layer summary |
| `devbox layer stash` | Stash current overlay changes |
| `devbox layer stash-pop` | Restore stashed changes |
| `devbox layout list` | List available layouts |
| `devbox layout preview <name>` | ASCII preview of a layout |
| `devbox layout create <name>` | Create a custom layout |
| `devbox layout edit <name>` | Edit a layout in $EDITOR |
| `devbox layout save` | Save layout preference |
| `devbox layout set-default <n>` | Set global default layout |
| `devbox snapshot save` | Create a snapshot |
| `devbox snapshot restore` | Restore a snapshot |
| `devbox guide [tool]` | Built-in cheat sheets |
| `devbox doctor` | Diagnose system issues |
| `devbox reprovision` | Re-push configs and rebuild |
| `devbox self-update` | Update devbox binary |
| `devbox init` | Generate devbox.toml |
| `devbox config show` | Show current configuration |
| `devbox nix add <pkg>` | Add a Nix package |
| `devbox nix remove <pkg>` | Remove a Nix package |
| `devbox prune` | Remove all stopped sandboxes |

---

## Tool Catalog

Devbox ships with [**120+ tools**](docs/PACKAGES.md) organized into toggleable sets. All packages come from [nixpkgs](https://search.nixos.org/packages), the largest and most up-to-date package repository. See the [full package reference](docs/PACKAGES.md) for detailed descriptions of every tool.

### Core Sets (always installed)

<details>
<summary><b>system</b> -- 24 packages</summary>

coreutils, gnugrep, gnused, gawk, findutils, diffutils, gzip, gnutar, xz, bzip2, file, which, tree, less, curl, wget, openssh, openssl, cacert, gnupg, gcc, gnumake, pkg-config, man-db

</details>

<details>
<summary><b>shell</b> -- 11 packages</summary>

| Package | Description |
|---------|-------------|
| zellij | Terminal multiplexer (workspace layouts) |
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

### Default Sets (on by default)

<details>
<summary><b>git</b> -- 6 packages</summary>

git, lazygit (TUI), gh (GitHub CLI), git-lfs, git-crypt, pre-commit

</details>

<details>
<summary><b>ai-code</b> -- 6 packages (AI coding assistants)</summary>

claude-code, codex, opencode, aider-chat, aichat, continue

</details>

### Optional Sets (off by default)

<details>
<summary><b>container</b> -- 6 packages</summary>

docker, docker-compose, lazydocker (TUI), dive (image analyzer), buildkit, skopeo

</details>

<details>
<summary><b>network</b> -- 7 packages</summary>

tailscale, mosh, nmap, tcpdump, bandwhich, trippy, doggo

</details>

<details>
<summary><b>ai-infra</b> -- 5 packages (local AI inference)</summary>

ollama, open-webui, litellm, mcp-hub, huggingface-hub

</details>

### Language Sets (auto-detected or `--tools` flag)

| Language | Detection | Packages |
|----------|-----------|----------|
| **Go** | `go.mod` | go, gopls, golangci-lint, delve, gotools, gore |
| **Rust** | `Cargo.toml` | rustup, rust-analyzer, cargo-watch, cargo-edit, cargo-expand, sccache |
| **Python** | `pyproject.toml`, `requirements.txt` | python 3.12, uv, ruff, pyright, ipython, pytest |
| **Node.js** | `package.json` | node 22, bun, pnpm, typescript, ts-language-server, biome |
| **Java** | `pom.xml`, `build.gradle` | jdk 21, gradle, maven, jdt-language-server |
| **Ruby** | `Gemfile` | ruby 3.3, bundler, solargraph, rubocop |

---

## Configuration

### Project-level (`devbox.toml`)

Generated with `devbox init`, auto-detects your project settings.

```toml
[sandbox]
runtime = "auto"            # auto | lima | incus | multipass | docker
image = "nixos"             # nixos | ubuntu
layout = "default"          # zellij layout name
mount_mode = "overlay"      # overlay (safe) | writable (direct)

[sets]
editor = true               # neovim, helix, nano
git = true                  # git, lazygit, gh
container = false           # docker, compose, lazydocker
network = false             # tailscale, mosh, nmap
ai_code = true              # claude-code, codex, aider, aichat, ...
ai_infra = false            # ollama, open-webui

[languages]
go = true                   # auto-detected from go.mod
rust = false
python = false
node = false

[resources]
cpu = 4
memory = "8GiB"
```

### Global defaults

```bash
devbox config set runtime lima
devbox config set layout ai-pair
devbox config show
```

---

## Base Images

Both images install the same [120+ tools](docs/PACKAGES.md) from [nixpkgs](https://search.nixos.org/packages).

| Image | Method | Rollback | Best For |
|-------|--------|----------|----------|
| **nixos** (default) | `nixos-rebuild switch` | Full system generations | Reproducible, declarative environments |
| **ubuntu** | Nix package manager | `nix profile rollback` | Familiar base OS |

---

## Runtime Support

Devbox auto-detects the best available VM runtime on your system.

| Runtime | Platform | Priority |
|---------|----------|----------|
| Incus | Linux | Highest |
| Lima | macOS | High |
| Multipass | macOS/Linux | Medium |
| Docker | Any | Fallback |

---

## Architecture

```
devbox (single binary)
  |
  |-- CLI Layer (clap)
  |     21 commands with consistent UX
  |
  |-- Sandbox Manager
  |     Lifecycle: create -> start -> attach -> stop -> destroy
  |     State persistence at ~/.devbox/sandboxes/
  |     OverlayFS diff/commit/discard
  |
  |-- Runtime Abstraction
  |     Trait-based backends (Lima, Incus, Multipass, Docker)
  |     Auto-detection with priority scoring
  |     Uniform exec/start/stop/status interface
  |
  |-- NixOS Provisioning
  |     All .nix files embedded in binary (include_str!)
  |     Base64-encoded push via shell commands
  |     Declarative package management via nixos-rebuild
  |
  |-- Built-in Resources (compiled into binary)
        8 Zellij layouts (KDL)
        14 tool cheat sheets (Markdown)
        16 NixOS package set definitions
```

### Provisioning flow

1. VM runtime creates and boots a NixOS (or Ubuntu) image
2. Devbox pushes `.nix` config files into the VM at `/etc/devbox/`
3. NixOS module is imported into the VM's system configuration
4. `nixos-rebuild switch` installs all declared packages from binary cache
5. Devbox binary and help files are copied into the VM
6. Sandbox state is saved to `~/.devbox/sandboxes/<name>/`

---

## Development

```bash
# Build
cargo build --release

# Test (52 unit + 15 integration tests)
cargo test

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt --check
```

For end-to-end testing with real VMs, see the [E2E Test Guide](docs/E2E_TEST_GUIDE.md).

## Contributing

Contributions are welcome. Please open an issue to discuss significant changes before submitting a pull request.

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Write tests for your changes
4. Ensure `cargo test` and `cargo clippy` pass
5. Submit a pull request

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
