# Devbox Quick Start

This guide walks you through creating your first developer VM with devbox. It explains exactly what happens at each step so you can understand the system.

## What is Devbox?

Devbox creates a **Linux virtual machine** on your laptop and fills it with developer tools. Instead of installing Go, Rust, Docker, Neovim, etc. on your host machine (and dealing with version conflicts, homebrew issues, or cluttering your system), you get a clean, reproducible VM with everything pre-configured.

You choose your base image:
- **NixOS** (default) -- fully declarative, atomic upgrades, instant rollback
- **Ubuntu** -- familiar base OS, same tools installed via the Nix package manager

Both images install the exact same 90+ developer tools from [nixpkgs](https://search.nixos.org/packages).

**Key concepts:**

- **NixOS** -- A Linux distribution where the entire system configuration is declared in code. Installing software means adding a package name to a configuration file and running `nixos-rebuild switch`. This is reproducible, rollbackable, and atomic (it either works or nothing changes).
- **Ubuntu** -- The widely-used Linux distribution. When used with devbox, the Nix package manager is installed on top to provide the same tool catalog without changing the familiar Ubuntu base.
- **Nix package manager** -- A package manager that can run on any Linux (or macOS). It stores packages in `/nix/store/` with unique hashes, so multiple versions never conflict. Used standalone on Ubuntu, or as the foundation of NixOS.
- **Lima** -- A lightweight VM manager for macOS. It uses Apple's Virtualization Framework (like Docker Desktop does) to run Linux VMs with near-native performance. Lima handles networking, file sharing, and SSH automatically.
- **Sets** -- Groups of related tools (e.g., "shell" set includes zellij, fzf, starship). You toggle sets on/off to control what's installed.

## Prerequisites

**macOS:**
```bash
# Install Lima (VM runtime)
brew install lima

# Verify
limactl --version
```

**Linux:**
```bash
# Install Incus (VM runtime)
# On Ubuntu/Debian:
sudo apt install incus
# On Fedora:
sudo dnf install incus
```

**Build devbox:**
```bash
git clone <your-repo-url>
cd devbox
cargo build --release
# Add to PATH
cp target/release/devbox /usr/local/bin/
```

## Step 1: Check Your System

```bash
devbox doctor
```

This checks:
- Whether Lima (macOS) or Incus (Linux) is installed
- Supporting tools like Zellij (terminal multiplexer)
- State directory (`~/.devbox/`) exists

If something is missing, `devbox doctor` prints install instructions.

## Step 2: Create Your First Sandbox

Navigate to any project directory and run:

```bash
cd ~/my-project
devbox create --name my-first-vm --bare
```

### What happens when you run this:

```
Step 1/6: Lima VM creation
┌─────────────────────────────────────────────────────────┐
│ devbox create                                           │
│   └─ limactl create --name devbox-my-first-vm ...       │
│        └─ Downloads NixOS image (~800MB, first run only)│
│        └─ Creates a VM with 4 CPUs, 8GB RAM             │
│        └─ Sets up 9p file sharing for host mounts        │
└─────────────────────────────────────────────────────────┘

Step 2/6: Boot the VM
┌─────────────────────────────────────────────────────────┐
│ limactl start devbox-my-first-vm                        │
│   └─ NixOS boots inside the VM                          │
│   └─ Lima maps your macOS username into the VM          │
│   └─ SSH access is configured automatically             │
└─────────────────────────────────────────────────────────┘

Step 3/6: Push NixOS configuration into the VM
┌─────────────────────────────────────────────────────────┐
│ Devbox writes these files into the VM:                  │
│                                                         │
│ /etc/devbox/                                            │
│   ├── devbox-state.toml    ← which sets/languages are on│
│   ├── devbox-module.nix    ← NixOS module for packages  │
│   ├── sets/                                             │
│   │   ├── default.nix      ← index of all sets          │
│   │   ├── system.nix       ← 24 core OS packages        │
│   │   ├── shell.nix        ← 10 terminal/shell tools    │
│   │   ├── tools.nix        ← 21 modern CLI tools        │
│   │   ├── editor.nix       ← neovim, helix, nano        │
│   │   ├── git.nix          ← git, lazygit, gh           │
│   │   ├── container.nix    ← docker, compose, dive      │
│   │   ├── lang-go.nix      ← go, gopls, delve           │
│   │   ├── lang-rust.nix    ← rustup, rust-analyzer      │
│   │   └── ...              ← (14 set files total)       │
│   └── help/                                             │
│       ├── zellij.md        ← cheat sheets for tools     │
│       ├── lazygit.md                                    │
│       └── ...              ← (14 help files)            │
│                                                         │
│ The devbox-module.nix is imported into the VM's         │
│ existing /etc/nixos/configuration.nix                   │
└─────────────────────────────────────────────────────────┘

Step 4/6: nixos-rebuild switch
┌─────────────────────────────────────────────────────────┐
│ sudo nixos-rebuild switch                               │
│                                                         │
│ NixOS reads the configuration and:                      │
│   1. Resolves all package names to exact versions       │
│   2. Downloads pre-compiled packages from cache.nixos.org│
│      (binary cache — not building from source)          │
│   3. Creates a new "generation" (system snapshot)       │
│   4. Atomically switches to the new system              │
│                                                         │
│ If anything fails, NixOS rolls back automatically.      │
│ Your VM is never left in a broken state.                │
│                                                         │
│ First run: 5-10 minutes (downloading ~500MB of packages)│
│ Subsequent: seconds (Nix caches everything)             │
└─────────────────────────────────────────────────────────┘

Step 5/6: Copy devbox binary into VM
┌─────────────────────────────────────────────────────────┐
│ limactl copy devbox devbox-my-first-vm:/tmp/devbox      │
│ sudo install /tmp/devbox /usr/local/bin/devbox           │
│                                                         │
│ Now you can run `devbox guide` inside the VM.            │
└─────────────────────────────────────────────────────────┘

Step 6/6: Save state
┌─────────────────────────────────────────────────────────┐
│ Sandbox metadata saved to:                              │
│   ~/.devbox/sandboxes/my-first-vm/state.json            │
│                                                         │
│ Contains: name, runtime, project dir, sets, languages   │
└─────────────────────────────────────────────────────────┘
```

### Using Ubuntu instead

If you prefer a familiar Ubuntu base:

```bash
devbox create --name my-first-vm --image ubuntu --bare
```

The flow is similar but Step 4 changes:
- Instead of `nixos-rebuild switch`, devbox installs the **Nix package manager** on Ubuntu
- Then runs `nix profile install nixpkgs#zellij nixpkgs#ripgrep ...` for all packages
- Services like Docker are installed via `apt` for systemd integration
- Same tools, same versions, just a different base OS

## Step 3: Enter Your VM

```bash
devbox shell my-first-vm
```

You're now inside a NixOS VM with 55+ developer tools installed. Try them:

```bash
# Modern file navigation
eza --icons              # ls with icons and colors
fd "*.go"                # fast file search
yazi                     # terminal file manager (q to quit)

# Search and view
rg "TODO" .              # fast grep
bat README.md            # cat with syntax highlighting

# Git workflow
lazygit                  # full Git TUI (q to quit)

# Terminal multiplexer
zellij                   # split terminal (Ctrl+p, d to detach)

# Get help on any tool
devbox guide zellij      # keybinding reference
devbox guide lazygit     # workflow guide
devbox guide             # list all available guides

# Leave the VM
exit
```

## Step 4: Project with Language Detection

For real projects, devbox auto-detects your language and installs the right toolchain:

```bash
cd ~/my-go-project       # has go.mod
devbox init              # creates devbox.toml, detects Go
cat devbox.toml          # shows go = true under [languages]
devbox create --name my-go-project
```

This time, devbox includes the `lang-go` set in the NixOS rebuild, which adds:
- `go` -- Go compiler
- `gopls` -- Go language server (for editor integration)
- `golangci-lint` -- Go linter
- `delve` -- Go debugger
- `gotools` -- Go code tools (goimports, etc.)
- `gore` -- Go REPL

Same for other languages:
```bash
# Explicit tools
devbox create --tools rust,python,docker

# Multiple languages detected automatically
cd ~/fullstack-project   # has package.json + go.mod
devbox init              # detects node + go
devbox
```

## Step 5: Day-to-Day Workflow

```bash
# Enter your sandbox (creates if needed, starts if stopped)
cd ~/my-project
devbox

# Run a one-off command without entering the VM
devbox exec -- make test
devbox exec -- go build ./...

# Stop the VM (preserves all state)
devbox stop

# Resume later (VM starts in seconds)
devbox shell

# Destroy when done (removes VM completely)
devbox destroy --force
```

## File Safety: The Overlay System

By default, devbox mounts your host project directory **read-only** inside the VM and places a copy-on-write (CoW) overlay on top. Any file you edit inside the VM is written to the overlay layer, not to your host disk. This means a runaway `rm -rf` or a bad code-generation tool cannot damage your real files.

```
┌─────────────────────────────────────────────┐
│           VM sees merged view               │
│                                             │
│  ┌───────────────────────────────────────┐  │
│  │  Overlay (upper)  ← writes land here  │  │
│  ├───────────────────────────────────────┤  │
│  │  Host mount (lower) ← read-only       │  │
│  └───────────────────────────────────────┘  │
│                                             │
│  Reads: overlay first, then host            │
│  Writes: always go to overlay               │
└─────────────────────────────────────────────┘
```

### Reviewing and applying changes

```bash
# See what changed in the overlay vs your host files
devbox diff

# Happy with the changes? Copy them back to the host
devbox commit

# Made a mess? Throw away all overlay changes
devbox discard
```

`devbox diff` shows a unified diff of every file the VM modified. `devbox commit` copies those changes back to your host directory atomically. `devbox discard` wipes the overlay layer and resets the VM view to match your host files exactly.

### Opting out of overlay mode

If you want the VM to write directly to your host filesystem (the traditional shared-folder behavior), you have two options:

```bash
# Per-session: pass the flag when creating or entering
devbox create --name my-vm --writable
devbox shell --writable

# Per-project: set it in devbox.toml
# [sandbox]
# mount_mode = "writable"
```

**Warning:** In writable mode there is no safety net. Any process inside the VM can modify or delete your host files immediately. Use this only when you need real-time two-way sync (e.g., a host-side editor saving while the VM compiles).

## What's Installed by Default

When you create a sandbox, these sets are always installed:

### Core Sets (always on)

| Set | Packages | Purpose |
|-----|----------|---------|
| **system** | coreutils, gcc, gnumake, curl, wget, openssh, openssl, gnupg, tree, less, file, and more (24 packages) | Essential OS tools |
| **shell** | zellij, zsh, starship, fzf, zoxide, direnv, yazi (10 packages) | Terminal experience |
| **tools** | ripgrep, fd, bat, eza, delta, jq, yq, htop, dust, httpie, glow, and more (21 packages) | Modern CLI replacements |

### Default Sets (on by default, can be disabled)

| Set | Packages | Purpose |
|-----|----------|---------|
| **editor** | neovim, helix, nano | Text editors |
| **git** | git, lazygit, gh, git-lfs, git-crypt, pre-commit | Version control |
| **container** | docker, docker-compose, lazydocker, dive, buildkit, skopeo | Containers |

### Optional Sets (off by default)

| Set | Packages | Purpose |
|-----|----------|---------|
| **network** | tailscale, mosh, nmap, tcpdump, bandwhich, trippy, doggo | Networking |
| **ai** | claude-code, aider, ollama, open-webui, codex, and more (10 packages) | AI/ML tools |

### Language Sets (enabled by detection or `--tools` flag)

| Set | Packages |
|-----|----------|
| **lang-go** | go, gopls, golangci-lint, delve, gotools, gore |
| **lang-rust** | rustup, rust-analyzer, cargo-watch, cargo-edit, cargo-expand, sccache |
| **lang-python** | python 3.12, uv, ruff, pyright, ipython, pytest |
| **lang-node** | node 22, bun, pnpm, typescript, typescript-language-server, biome |
| **lang-java** | jdk 21, gradle, maven, jdt-language-server |
| **lang-ruby** | ruby 3.3, bundler, solargraph, rubocop |

## Understanding NixOS (for the curious)

If you've never used NixOS, here's what you need to know:

### How is this different from apt/brew?

| | apt/brew | NixOS |
|---|---------|-------|
| Install | `apt install go` | Add `go` to configuration.nix, run `nixos-rebuild switch` |
| Rollback | Manual | `nixos-rebuild switch --rollback` (instant) |
| Reproducible | No (depends on when you installed) | Yes (same config = same system, always) |
| Multiple versions | Painful | `nix-shell -p go_1_21 go_1_22` (trivially) |
| Breaks other things? | Sometimes | Never (each package in its own path) |

### Where are packages stored?

NixOS stores packages in `/nix/store/`, with each package in a unique hash-addressed directory:
```
/nix/store/abc123-go-1.22.0/bin/go
/nix/store/def456-ripgrep-14.1.0/bin/rg
```

The system PATH and symlinks are managed by NixOS. Multiple versions of the same package can coexist without conflicts.

### What is a "generation"?

Each `nixos-rebuild switch` creates a new "generation" -- a complete system snapshot. You can list and switch between them:

```bash
# Inside the VM:
sudo nixos-rebuild list-generations
sudo nixos-rebuild switch --rollback   # go back to previous generation
```

### What is the binary cache?

NixOS doesn't compile packages from source (usually). The official binary cache at `cache.nixos.org` has pre-compiled packages for common architectures. When you run `nixos-rebuild`, Nix downloads the pre-compiled versions. This is why the first rebuild takes minutes (downloading), not hours (compiling).

## Configuration Reference

### devbox.toml (project-level)

Generated by `devbox init`, lives in your project root:

```toml
[sandbox]
runtime = "auto"        # auto | lima | incus | multipass | docker
image = "nixos"         # nixos (declarative) | ubuntu (familiar)
layout = "default"      # zellij layout name
mount_mode = "overlay"  # overlay (safe) | writable (direct)

[sets]
system = true           # always on (locked)
shell = true            # always on (locked)
tools = true            # always on (locked)
editor = true           # on by default
git = true              # on by default
container = true        # on by default
network = false         # off by default
ai = false              # off by default

[languages]
go = false              # auto-detected or manual
rust = false
python = false
node = false
java = false
ruby = false

[mounts.workspace]
host = "."              # current directory
target = "/workspace"   # mount point inside VM
readonly = false

[resources]
cpu = 0                 # 0 = default (4 cores)
memory = ""             # "" = default (8GiB)
```

### Global config (~/.devbox/config.toml)

```bash
devbox config set runtime lima       # always use Lima
devbox config set layout ai-pair     # default layout
devbox config set tools go,rust      # always install these languages
devbox config show                   # view current settings
```

## Common Commands

| Command | What it does |
|---------|-------------|
| `devbox` | Smart default: create or attach to sandbox for current directory |
| `devbox create` | Create a new sandbox |
| `devbox shell` | Attach to a sandbox (starts it if stopped) |
| `devbox exec -- <cmd>` | Run a one-off command inside the sandbox |
| `devbox stop` | Stop the VM (preserves state) |
| `devbox destroy` | Remove the VM permanently |
| `devbox list` | Show all sandboxes |
| `devbox status` | Detailed status of a sandbox |
| `devbox guide` | Built-in tool cheat sheets |
| `devbox doctor` | System diagnostics |
| `devbox init` | Generate devbox.toml for current project |
| `devbox upgrade --tools rust` | Add tools to a running sandbox |
| `devbox packages` | Interactive TUI to toggle sets |
| `devbox layout list` | Available workspace layouts |
| `devbox diff` | Show overlay changes vs host |
| `devbox commit` | Sync overlay changes to host |
| `devbox prune` | Remove all stopped sandboxes |
