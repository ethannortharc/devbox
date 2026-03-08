# Devbox E2E Test Guide

End-to-end tests that verify the full devbox lifecycle against a real NixOS VM runtime.
These tests should be run manually after significant changes.

## Prerequisites

- Release binary built: `cargo build --release`
- At least one VM runtime installed (Lima recommended on macOS, Incus on Linux)
- Run `devbox doctor` to verify your setup

## Test Summary

| # | Test | Status |
|---|------|--------|
| 1 | [System check](#1-system-check) | Verified |
| 2 | [Project init](#2-project-initialization) | Verified |
| 3 | [Create sandbox (bare)](#3-create-a-bare-sandbox) | Verified |
| 4 | [List and status](#4-list-and-status) | Verified |
| 5 | [Exec commands](#5-exec-one-off-commands) | Verified |
| 6 | [Shell attach + tool verification](#6-shell-attach-and-tool-verification) | Verified |
| 7 | [Guide system (inside VM)](#7-guide-system-inside-vm) | Verified |
| 8 | [Create sandbox (with tools)](#8-create-sandbox-with-language-tools) | Manual |
| 9 | [Layout commands](#9-layout-commands) | Verified |
| 10 | [Config management](#10-config-management) | Verified |
| 11 | [Stop and destroy](#11-stop-and-destroy) | Verified |
| 12 | [Overlay operations](#12-overlay-operations) | Manual |
| 13 | [Snapshots](#13-snapshot-operations) | Manual |

---

## What Happens Under the Hood

When you run `devbox create`, the following steps occur:

1. **Lima VM creation** -- `limactl create` downloads a stock NixOS Lima image (~800MB on first run) and creates a virtual machine using Apple's Virtualization Framework (macOS) or QEMU (Linux).

2. **VM boot** -- `limactl start` boots the NixOS VM. Lima maps your host username into the VM and sets up SSH access automatically.

3. **NixOS provisioning** -- Devbox pushes configuration files into the VM:
   - `/etc/devbox/devbox-state.toml` -- which tool sets and languages to install
   - `/etc/devbox/devbox-module.nix` -- NixOS module declaring packages
   - `/etc/devbox/sets/*.nix` -- individual set definitions (system, shell, tools, etc.)
   - The devbox module is imported into the VM's `/etc/nixos/configuration.nix`

4. **nixos-rebuild switch** -- NixOS reads the configuration, downloads required packages from the Nix binary cache (pre-compiled), and atomically switches to the new system configuration. This is the step that installs all your developer tools.

5. **Post-provisioning** -- The devbox binary and help files are copied into the VM so `devbox guide` works inside the sandbox.

6. **State saved** -- Sandbox metadata is stored in `~/.devbox/sandboxes/<name>/state.json` on the host.

---

## 1. System check

```bash
devbox doctor
```

**Expected output:**
- Runtime section shows your installed runtime as `installed` (green)
- Only platform-relevant runtimes are checked (macOS: Lima; Linux: Incus)
- Missing optional tools show install instructions
- Auto-detected runtime shown with priority
- Global config, state directory, and supporting tools listed

**Example (macOS with Lima):**
```
Runtime availability:
  Lima: installed

Auto-detected runtime: lima (priority 20)

Supporting tools:
  Zellij: installed
  Nix: not found
    Install: curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh
```

## 2. Project initialization

```bash
mkdir /tmp/devbox-e2e && cd /tmp/devbox-e2e
echo 'package main' > main.go
echo 'module test' > go.mod
devbox init
cat devbox.toml
```

**Expected:**
- `devbox.toml` created with `go = true` under `[languages]`
- Console prints `Detected: go`
- File contains all config sections: `[sandbox]`, `[sets]`, `[languages]`, `[mounts]`, `[resources]`, `[env]`

## 3. Create a bare sandbox

```bash
devbox create --name e2e-test --bare
```

**What happens:**
1. Lima downloads the NixOS image (first run only, cached afterward)
2. A VM named `devbox-e2e-test` is created and started
3. Devbox pushes NixOS configuration files into the VM
4. `nixos-rebuild switch` installs core packages (system, shell, tools sets)
5. The devbox binary and help files are copied into the VM
6. Sandbox state is saved to `~/.devbox/sandboxes/e2e-test/`

**Expected console output:**
```
Creating NixOS VM 'devbox-e2e-test'...
  (first run downloads NixOS image, this may take a few minutes)
Starting NixOS VM 'devbox-e2e-test'...
Setting up NixOS configuration...
Installing packages via nixos-rebuild (this may take a few minutes)...
NixOS rebuild complete.
Copying devbox into VM...
Sandbox 'e2e-test' created successfully (runtime: lima)
```

**Timing:**
- First run: 5-15 min (image download + first nixos-rebuild)
- Subsequent creates: 3-8 min (image cached, nixos-rebuild still runs)
- The `--bare` flag skips auto-detection for a minimal VM (no language sets)

## 4. List and status

```bash
devbox list
devbox status e2e-test
```

**Expected for `list`:**
```
NAME                 RUNTIME      LAYOUT     PROJECT DIR
------------------------------------------------------------------------
e2e-test             lima         default    /tmp/devbox-e2e

1 sandbox(es)
```

**Expected for `status`:**
```
Sandbox:     e2e-test
Status:      Running (green)
Runtime:     lima
Project:     /tmp/devbox-e2e
Mount mode:  overlay
Layout:      default
Sets:        system, shell, tools, editor, git, container
```

## 5. Exec one-off commands

```bash
devbox exec --name e2e-test -- echo "hello from VM"
devbox exec --name e2e-test -- uname -a
devbox exec --name e2e-test -- whoami
devbox exec --name e2e-test -- cat /etc/os-release
```

**Expected:**
- `echo`: prints `hello from VM`
- `uname -a`: prints Linux kernel info (e.g., `Linux lima-devbox-e2e-test ... aarch64 GNU/Linux`)
- `whoami`: prints your host username (Lima maps it automatically)
- `cat /etc/os-release`: shows NixOS info (e.g., `NAME=NixOS`, `VERSION_ID="24.11"`)
- All commands exit with code 0

## 6. Shell attach and tool verification

```bash
devbox shell e2e-test
```

**Expected:**
- Drops into an interactive zsh shell inside the VM
- zsh with starship prompt is the default shell after provisioning
- Type `exit` to return to host

**Inside the VM, verify NixOS and installed tools:**
```bash
# Verify NixOS
uname -a                 # Should show Linux
cat /etc/os-release      # Should show NixOS
hostname                 # Should show lima-devbox-e2e-test or similar

# Verify core tools (from system set):
gcc --version            # GNU C compiler
curl --version           # HTTP client
tree --version           # Directory tree

# Verify shell tools (from shell set):
zellij --version         # Terminal multiplexer
starship --version       # Shell prompt
fzf --version            # Fuzzy finder
yazi --version           # File manager

# Verify developer tools (from tools set):
rg --version             # ripgrep (fast grep)
fd --version             # fd (fast find)
bat --version            # bat (cat with syntax highlighting)
eza --version            # eza (modern ls)
delta --version          # delta (git diff viewer)
jq --version             # JSON processor
htop --version           # Process viewer
httpie --version         # HTTP client for APIs
glow --version           # Markdown renderer

# Verify editor tools (from editor set):
nvim --version           # Neovim
helix --version          # Helix editor

# Verify git tools (from git set):
git --version            # Git
lazygit --version        # Git TUI
gh --version             # GitHub CLI

# Verify devbox binary works inside VM:
devbox guide             # Should show help index
devbox guide zellij      # Should show zellij keybindings

exit
```

**If any tool is missing**, nixos-rebuild may have failed. Check with:
```bash
devbox exec --name e2e-test -- sudo nixos-rebuild switch
```

## 7. Guide system (inside VM)

```bash
# From host (uses embedded cheat sheets):
devbox guide
devbox guide zellij
devbox guide lazygit
devbox guide nonexistent

# From inside the VM (uses /etc/devbox/help/ files):
devbox shell e2e-test
devbox guide
devbox guide nvim
exit
```

**Expected:**
- `guide`: Shows index with all 13 available cheat sheets
- `guide zellij`: Renders Zellij keybinding reference
- `guide lazygit`: Renders lazygit workflow reference
- `guide nonexistent`: Prints "No cheat sheet for 'nonexistent'" to stderr
- Guide works both on the host and inside the VM

## 8. Create sandbox with language tools

This test verifies that language-specific tools are installed when detected or specified.

```bash
cd /tmp/devbox-e2e   # Should still have main.go and go.mod from step 2
devbox destroy e2e-test --force

# Create with auto-detection (should detect Go)
devbox create --name e2e-lang
```

**What happens differently from bare:**
1. `devbox init` auto-detects Go from `main.go` and `go.mod`
2. `devbox-state.toml` includes `go = true` under `[languages]`
3. `nixos-rebuild switch` installs the lang-go set (Go, gopls, golangci-lint, delve, gotools, gore)

**Inside the VM, verify Go tools:**
```bash
devbox shell e2e-lang
go version               # Go compiler
gopls version             # Go language server
golangci-lint --version   # Go linter
dlv version               # Delve debugger
exit
```

**Or test with explicit tools:**
```bash
devbox destroy e2e-lang --force
devbox create --name e2e-explicit --tools go,rust,python --bare
devbox shell e2e-explicit
go version
rustup --version
python3 --version
exit
devbox destroy e2e-explicit --force
```

## 9. Layout commands

```bash
devbox layout list
devbox layout preview default
devbox layout preview ai-pair
devbox layout preview tdd
```

**Expected for `list`:**
```
NAME             DESCRIPTION
------------------------------------------------------------
default          Clean workspace: editor + terminal + files
ai-pair          AI assistant + editor + output
...
plain            No layout, just a shell

9 layout(s) available
```

**Expected for `preview`:** ASCII diagram showing pane arrangement with percentages and tab names.

## 10. Config management

```bash
devbox config show
devbox config set runtime lima
devbox config get runtime
devbox config set runtime auto
```

**Expected:**
- `show`: Displays current global config (runtime, layout, tools)
- `set`/`get`: Round-trip works correctly
- Values persist in `~/.devbox/config.toml`

## 11. Stop and destroy

```bash
devbox stop e2e-test 2>/dev/null || devbox stop e2e-lang
devbox status e2e-lang     # Should show Stopped (yellow)
devbox destroy e2e-lang --force
devbox list                # Should show "No sandboxes found."
```

**Expected:**
- Stop: "Sandbox '...' stopped."
- Status after stop: `Status: Stopped` (yellow)
- Destroy: "Sandbox '...' destroyed."
- List after destroy: "No sandboxes found."

## 12. Overlay operations

> Requires a sandbox with files in the overlay upper layer.

```bash
devbox create --name overlay-test --bare
devbox exec --name overlay-test -- touch /workspace/upper/testfile.txt
devbox diff --name overlay-test
devbox discard --name overlay-test
devbox diff --name overlay-test
devbox destroy overlay-test --force
```

**Expected:**
- First diff: shows `testfile.txt` as Added
- After discard: diff shows no changes
- Note: overlay behavior depends on NixOS image having the overlay mount configured

## 13. Snapshot operations

> Requires Incus or Multipass runtime (Lima snapshot support varies by version).

```bash
devbox create --name snap-test --bare
devbox snapshot save snap-test
devbox snapshot list snap-test
devbox snapshot restore snap-test <snapshot-name>
devbox destroy snap-test --force
```

**Expected:**
- Save: Creates a snapshot without errors
- List: Shows the saved snapshot with name and timestamp
- Restore: Restores to the snapshot state

---

## Cleanup

```bash
# Remove any leftover test VMs
devbox destroy e2e-test --force 2>/dev/null
devbox destroy e2e-lang --force 2>/dev/null
devbox destroy e2e-explicit --force 2>/dev/null
devbox destroy overlay-test --force 2>/dev/null
devbox destroy snap-test --force 2>/dev/null

# Remove test project
rm -rf /tmp/devbox-e2e
```

## Troubleshooting

### nixos-rebuild fails

If provisioning fails during `nixos-rebuild switch`, you can retry manually:

```bash
# Re-run nixos-rebuild inside the VM
devbox exec --name <sandbox> -- sudo nixos-rebuild switch --show-trace

# Check what NixOS configuration is active
devbox exec --name <sandbox> -- nixos-rebuild list-generations

# View the devbox state file
devbox exec --name <sandbox> -- cat /etc/devbox/devbox-state.toml
```

### VM won't start

```bash
# Check Lima VM status directly
limactl list

# View Lima VM logs
limactl shell devbox-<name> -- journalctl -b --no-pager | tail -50

# Force stop and retry
limactl stop devbox-<name>
limactl start devbox-<name>
```

### Tools missing after create

This usually means nixos-rebuild encountered an error. Check:

```bash
# View the pushed configuration
devbox exec --name <sandbox> -- cat /etc/devbox/devbox-module.nix
devbox exec --name <sandbox> -- cat /etc/devbox/devbox-state.toml

# Re-run rebuild
devbox exec --name <sandbox> -- sudo nixos-rebuild switch 2>&1
```

## Known Behaviors

1. **First-run image download**: The first `devbox create` downloads a NixOS Lima image (~800MB). Subsequent creates reuse the cached image.

2. **NixOS rebuild time**: The first `nixos-rebuild switch` downloads packages from the Nix binary cache. Most packages are pre-compiled, but some may need building. First rebuild takes 5-10 minutes; subsequent rebuilds with the same sets are near-instant due to Nix's content-addressed store.

3. **Shell**: After provisioning, zsh is the default shell with starship prompt. If provisioning is incomplete, the shell falls back to bash.

4. **Lima user mapping**: Lima automatically maps your macOS username into the VM. You don't need to create a `dev` user -- your host username works directly.

5. **Nix binary cache**: NixOS downloads pre-compiled packages from `cache.nixos.org`. If a package isn't cached (rare), Nix builds it from source, which takes longer.

6. **Self-update**: `devbox self-update --check` will fail until GitHub Releases are published for the repository.
