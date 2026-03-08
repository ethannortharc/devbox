# Devbox E2E Test Guide

End-to-end tests that verify the full devbox lifecycle against a real VM runtime.
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
| 3 | [Create sandbox](#3-create-a-sandbox) | Verified |
| 4 | [List and status](#4-list-and-status) | Verified |
| 5 | [Exec commands](#5-exec-one-off-commands) | Verified |
| 6 | [Shell attach](#6-shell-attach) | Verified |
| 7 | [Guide system](#7-guide-system) | Verified |
| 8 | [Layout commands](#8-layout-commands) | Verified |
| 9 | [Config management](#9-config-management) | Verified |
| 10 | [Stop and destroy](#10-stop-and-destroy) | Verified |
| 11 | [Overlay operations](#11-overlay-operations) | Manual |
| 12 | [Snapshots](#12-snapshot-operations) | Manual |

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

## 3. Create a sandbox

```bash
devbox create --name e2e-test --bare
```

**Expected:**
- "Creating NixOS VM 'devbox-e2e-test'..."
- "Starting NixOS VM 'devbox-e2e-test'..."
- "Setting up NixOS configuration..."
- "Installing packages via nixos-rebuild (this may take a few minutes)..."
- "NixOS rebuild complete."
- "Sandbox 'e2e-test' created successfully (runtime: lima)"

**Notes:**
- First run downloads NixOS Lima image (~800MB), subsequent creates are faster
- The `--bare` flag skips auto-detection for a minimal VM
- NixOS rebuild downloads and installs packages from nixpkgs (may take 5-10 min on first run)

## 4. List and status

```bash
devbox list
devbox status e2e-test
```

**Expected for `list`:**
```
NAME                 RUNTIME      LAYOUT     PROJECT DIR
------------------------------------------------------------------------
e2e-test             lima         default    /path/to/your/cwd

1 sandbox(es)
```

**Expected for `status`:**
```
Sandbox:     e2e-test
Status:      Running (green)
Runtime:     lima
Project:     /path/to/your/cwd
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
- `whoami`: prints your host username (Lima maps it)
- `cat /etc/os-release`: shows NixOS info
- All commands exit with code 0

## 6. Shell attach

```bash
devbox shell e2e-test
```

**Expected:**
- Drops into an interactive zsh shell inside the VM
- After NixOS provisioning: zsh with starship prompt is default
- Type `exit` to return to host

**Inside the VM, verify:**
```bash
uname -a       # Should show Linux (NixOS)
cat /etc/os-release  # Should show NixOS
hostname       # Should show lima-devbox-e2e-test or devbox

# Verify tools are installed (from system/shell/tools sets):
zellij --version    # Terminal multiplexer
rg --version        # ripgrep
fd --version        # fd-find
bat --version       # bat (cat replacement)
lazygit --version   # Git TUI
nvim --version      # Neovim
starship --version  # Shell prompt
eza --version       # ls replacement

# Verify devbox is available inside VM:
devbox guide        # Should show help index
devbox guide zellij # Should show zellij keybindings

exit
```

## 7. Guide system

```bash
devbox guide
devbox guide zellij
devbox guide lazygit
devbox guide nonexistent
```

**Expected:**
- `guide`: Shows index with all 13 available cheat sheets
- `guide zellij`: Renders Zellij keybinding reference
- `guide lazygit`: Renders lazygit workflow reference
- `guide nonexistent`: Prints "No cheat sheet for 'nonexistent'" to stderr

## 8. Layout commands

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

## 9. Config management

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

## 10. Stop and destroy

```bash
devbox stop e2e-test
devbox status e2e-test     # Should show Stopped (yellow)
devbox destroy e2e-test --force
devbox list                # Should show "No sandboxes found."
```

**Expected:**
- Stop: "Sandbox 'e2e-test' stopped."
- Status after stop: `Status: Stopped` (yellow)
- Destroy: "Sandbox 'e2e-test' destroyed."
- List after destroy: "No sandboxes found."

## 11. Overlay operations

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

## 12. Snapshot operations

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
devbox destroy overlay-test --force 2>/dev/null
devbox destroy snap-test --force 2>/dev/null

# Remove test project
rm -rf /tmp/devbox-e2e
```

## Known Behaviors

1. **First-run image download**: The first `devbox create` downloads a NixOS Lima image (~800MB). Subsequent creates reuse the cached image.

2. **NixOS rebuild time**: The first `nixos-rebuild switch` downloads and builds packages from nixpkgs, which may take 5-10 minutes depending on network speed and selected sets/languages. Subsequent rebuilds are faster due to Nix's caching.

3. **Shell**: After provisioning, zsh is the default shell with starship prompt. If provisioning is incomplete, the shell falls back to bash.

4. **Self-update**: `devbox self-update --check` will fail until GitHub Releases are published for the repository.
