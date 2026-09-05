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
| 8b | [Create Ubuntu sandbox](#8b-create-ubuntu-sandbox) | Manual |
| 9 | [Recorded runs](#9-recorded-runs) | Manual |
| 10 | [Config management](#10-config-management) | Verified |
| 11 | [Stop and destroy](#11-stop-and-destroy) | Verified |
| 12 | [Overlay operations and checkpoints](#12-overlay-operations-and-checkpoints) | Manual |
| 13 | [Snapshots](#13-snapshot-operations) | Manual |
| 14 | [Credential broker](#14-credential-broker) | Manual |
| 15 | [MCP server in a box](#15-mcp-server-in-a-box) | Manual |

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
  Nix: not found
    Install: curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh
```

`doctor` also reports the collector daemon, the credential broker, and — for
every running box — its kernel, BTF, nftables, agent, agent-binary freshness,
capture source and file scope. Those lines are what tell you whether capture is
real or degraded:

```
Running box capabilities:
  e2e-test (lima):
    kernel: 6.19.0
    btf: ready
    nftables: ready
    vsock: ready
    agent: devbox-obsd 0.2.0 (<commit>)
    agent binary: matches host embed
    capture: ebpf+packet+netfilter
    file scope: /workspace, /home
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
NAME                 RUNTIME      MOUNT      PROJECT DIR
------------------------------------------------------------------------
e2e-test             lima         overlay    /tmp/devbox-e2e

1 sandbox(es)
```

**Expected for `status`:**
```
Sandbox:     e2e-test
Status:      Running (green)
Runtime:     lima
Image:       nixos
Project:     /tmp/devbox-e2e
Mount mode:  overlay
Created:     <RFC 3339 timestamp>
Sets:        system, shell, tools, editor, git, container
```

## 5. Exec one-off commands

```bash
devbox exec e2e-test -- echo "hello from VM"
devbox exec e2e-test -- uname -a
devbox exec e2e-test -- whoami
devbox exec e2e-test -- cat /etc/os-release
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
devbox guide lazygit     # Should show the lazygit sheet

exit
```

**If any tool is missing**, nixos-rebuild may have failed. Check with:
```bash
devbox exec e2e-test -- sudo nixos-rebuild switch
```

## 7. Guide system (inside VM)

```bash
# From host (uses embedded cheat sheets):
devbox guide
devbox guide lazygit
devbox guide rg
devbox guide nonexistent

# From inside the VM (uses /etc/devbox/help/ files):
devbox shell e2e-test
devbox guide
devbox guide nvim
exit
```

**Expected:**
- `guide`: Shows index with all 13 available cheat sheets
- `guide lazygit`: Renders the lazygit workflow reference
- `guide rg`: Renders the ripgrep reference
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

## 8b. Create Ubuntu sandbox

This test verifies the Ubuntu image with Nix package manager provisioning.

```bash
devbox create --name e2e-ubuntu --image ubuntu --bare
```

**What happens differently from NixOS:**
1. Lima downloads Ubuntu 24.04 cloud image (instead of NixOS)
2. The Nix package manager is installed via the Determinate Systems installer
3. Packages are installed via `nix profile install nixpkgs#...` (instead of nixos-rebuild)
4. Shell environment (zsh, starship, zoxide) is configured via dotfiles (instead of NixOS module)

**Expected console output:**
```
Creating Ubuntu VM 'devbox-e2e-ubuntu'...
Installing Nix package manager on Ubuntu...
Installing N packages via Nix (this may take a few minutes)...
Nix package installation complete.
Copying devbox into VM...
Sandbox 'e2e-ubuntu' created successfully (runtime: lima)
```

**Inside the VM, verify:**
```bash
devbox shell e2e-ubuntu

# Should be Ubuntu
cat /etc/os-release      # Should show Ubuntu 24.04

# Nix should be installed
nix --version            # Nix package manager

# Same tools as NixOS image:
rg --version             # ripgrep
fd --version             # fd
bat --version            # bat
lazygit --version        # lazygit

# devbox guide should work
devbox guide

exit
```

**Cleanup:**
```bash
devbox destroy e2e-ubuntu --force
```

## 9. Recorded runs

> v4 removed the Zellij layout subsystem; `devbox layout` no longer exists.
> This slot now covers `devbox run`, the v5 replacement for "what did that
> command actually do?"

```bash
devbox run e2e-test --label smoke -- sh -c 'curl -sS -o /dev/null https://example.com; echo hi > /workspace/run-probe.txt; sleep 1'
devbox runs e2e-test
devbox report <RUN_ID> --format md
devbox layer checkpoints e2e-test
```

**Expected for `run`** — a five-line summary, then a report path:
```
run 01M1S6K0XSDYN51E45Y3JS6NK7 · 1.9s · exit 0 · finished
  files    1 changed (1 added, 0 modified, 0 deleted) · scope: run
  network  1 peers · 1 DNS · ↑1.9KB ↓6.3KB
  process  24 in the tree
  coverage full (ebpf+packet+netfilter) · 53 events · 0 dropped
  report   ~/.devbox/runs/e2e-test/01M1S6K0XSDYN51E45Y3JS6NK7/report.html
```

**Check, in order:**
- `files` names `run-probe.txt` and says `scope: run`, not `scope: box`.
- `network` names `example.com` with non-zero bytes. Zero bytes with a
  successful `curl` means the `close` probe did not attach — check
  `devbox doctor` for `capture: ebpf…`.
- `process` includes `curl` with a real pid (not `4294967295`).
- `coverage` reads `full`, and `dropped` is 0.
- `devbox layer checkpoints` shows a `run-start` and a `run-end` pair for the
  run; `devbox layer checkpoint-rm <one of them>` refuses without `--force`.

## 10. Config management

```bash
devbox config show
devbox config set runtime lima
devbox config get runtime
devbox config set runtime auto
```

**Expected:**
- `show`: Displays the current global config (runtime, image, tools)
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

## 12. Overlay operations and checkpoints

> Requires a sandbox with files in the overlay upper layer. `/workspace` is the
> merged view; the upper layer lives at `/var/devbox/overlay/upper` and is not
> something you write to directly.

```bash
devbox create --name overlay-test --bare
devbox exec overlay-test -- touch /workspace/testfile.txt
devbox diff overlay-test
devbox layer checkpoint overlay-test --label base   # note the id it prints
devbox exec overlay-test -- sh -c 'echo more > /workspace/testfile.txt'
devbox layer diff overlay-test --from <ID>          # any unique prefix works
devbox layer restore <ID> overlay-test
devbox diff overlay-test
devbox discard overlay-test
devbox diff overlay-test
devbox destroy overlay-test --force
```

**Expected:**
- First diff: shows `testfile.txt` as Added
- `layer checkpoint`: prints an id, a file count and a byte count
- `layer diff --from <id>`: shows `~ testfile.txt` (modified), even though the
  file is the same length — the comparison uses size *and* mtime
- `layer restore`: prints `Restored checkpoint <id>` and refreshes the overlay,
  so the next `cat` reads the restored content rather than a cached one
- After `discard`: diff shows no changes
- Note: overlay behavior depends on the NixOS image having the overlay mount
  configured

## 13. Snapshot operations

> Requires Incus or Multipass runtime (Lima snapshot support varies by version).

```bash
devbox create --name snap-test --bare
devbox snapshot save nightly snap-test     # <SNAPSHOT> first, then the box
devbox snapshot list snap-test
devbox snapshot restore nightly snap-test
devbox destroy snap-test --force
```

**Expected:**
- Save: Creates a snapshot without errors
- List: Shows the saved snapshot with name and timestamp
- Restore: Restores to the snapshot state

## 14. Credential broker

> Uses a dummy provider and a local echo upstream. Never point this test at a
> real key.

```bash
printf 'not-a-real-value\n' | devbox secret set e2e-probe \
  --url http://127.0.0.1:9 --header 'Authorization: Bearer' --stdin
devbox secret ls
devbox secret scope e2e-probe
devbox broker status
devbox broker reach e2e-test
devbox exec e2e-test -- sh -c 'env | grep DEVBOX_BROKER'
devbox secret rm e2e-probe
```

**Expected:**
- `secret set`: "Stored 'e2e-probe' in the keychain store. It is never written
  into a box."
- `secret ls`: one row with the provider name, the backend and the upstream —
  **never** the value
- `broker reach`: an address and how it was verified, e.g.
  `e2e-test: http://host.lima.internal:7879 (lima user-mode network)`
- `exec … env`: `DEVBOX_BROKER_URL` and `DEVBOX_BROKER_TOKEN` are present, and
  the secret's value appears nowhere
- `secret rm`: removes it from the keychain (`security find-generic-password -s
  devbox -a e2e-probe` finds nothing afterwards)

## 15. MCP server in a box

```bash
devbox mcp add echo-test --box e2e-test -- sh -c 'cat'
devbox mcp ls
printf '{"jsonrpc":"2.0","id":1,"method":"ping"}\n' | devbox mcp run echo-test
devbox mcp rm echo-test
```

**Expected:**
- `mcp add`: prints the registration plus the `claude mcp add` / `codex mcp add`
  lines to paste, and warns if the box lacks the interpreter the command needs
- `mcp run`: echoes the JSON-RPC line back byte for byte and exits 0
- stdout carries **only** JSON-RPC; anything the server writes to stderr lands
  in `~/.devbox/mcp/echo-test.log`
- `mcp rm`: leaves `devbox.toml` byte-identical to what it was before `mcp add`,
  comments included; the log is kept

---

## Cleanup

```bash
# Remove any leftover test VMs
devbox destroy e2e-test --force 2>/dev/null
devbox destroy e2e-lang --force 2>/dev/null
devbox destroy e2e-explicit --force 2>/dev/null
devbox destroy e2e-ubuntu --force 2>/dev/null
devbox destroy overlay-test --force 2>/dev/null
devbox destroy snap-test --force 2>/dev/null

# Remove any test credential and MCP registration
devbox secret rm e2e-probe 2>/dev/null
devbox mcp rm echo-test 2>/dev/null

# Remove test project
rm -rf /tmp/devbox-e2e
```

## Troubleshooting

### nixos-rebuild fails

If provisioning fails during `nixos-rebuild switch`, you can retry manually:

```bash
# Re-run nixos-rebuild inside the VM
devbox exec <sandbox> -- sudo nixos-rebuild switch --show-trace

# Check what NixOS configuration is active
devbox exec <sandbox> -- nixos-rebuild list-generations

# View the devbox state file
devbox exec <sandbox> -- cat /etc/devbox/devbox-state.toml
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
devbox exec <sandbox> -- cat /etc/devbox/devbox-module.nix
devbox exec <sandbox> -- cat /etc/devbox/devbox-state.toml

# Re-run rebuild
devbox exec <sandbox> -- sudo nixos-rebuild switch 2>&1
```

## Known Behaviors

1. **First-run image download**: The first `devbox create` downloads a NixOS Lima image (~800MB). Subsequent creates reuse the cached image.

2. **NixOS rebuild time**: The first `nixos-rebuild switch` downloads packages from the Nix binary cache. Most packages are pre-compiled, but some may need building. First rebuild takes 5-10 minutes; subsequent rebuilds with the same sets are near-instant due to Nix's content-addressed store.

3. **Shell**: After provisioning, zsh is the default shell with starship prompt. If provisioning is incomplete, the shell falls back to bash.

4. **Lima user mapping**: Lima automatically maps your macOS username into the VM. You don't need to create a `dev` user -- your host username works directly.

5. **Nix binary cache**: NixOS downloads pre-compiled packages from `cache.nixos.org`. If a package isn't cached (rare), Nix builds it from source, which takes longer.

6. **Self-update**: `devbox self-update --check` will fail until GitHub Releases are published for the repository.

7. **Agent replacement on first entry**: starting or entering a box built by an
   older devbox prints `Box '<name>' has an out-of-date observability agent
   (<sha> vs <sha>); replacing it.` and, on NixOS, runs one `nixos-rebuild
   switch` to regenerate the unit. It happens once per stale box and is
   idempotent afterwards.

8. **Runs need the collector**: `devbox run`, `exec`, `shell`, `watch`,
   `behavior`, `policy`, `web` and `mcp run` start the background collector if
   it is not already up. `devbox runs` and `devbox report` do not — they only
   read.
