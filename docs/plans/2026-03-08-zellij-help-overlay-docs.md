# Zellij Shell, Help System, Overlay Docs & Package Reference

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make devbox shell launch Zellij with a polished default layout (editor/terminal/file-browser + devbox management tab), integrate pretty help into Zellij, add overlay docs to quickstart, and create a package reference with official links.

**Architecture:** Four independent workstreams: (1) new default Zellij layout + shell launch changes, (2) help system with floating pane + help pane in layout, (3) docs/QUICKSTART.md overlay section, (4) docs/PACKAGES.md reference. Workstreams 1+2 are coupled (layout defines where help lives), 3+4 are pure docs.

**Tech Stack:** Zellij KDL layouts, Rust (clap, ratatui, tokio), glow (markdown rendering), existing Lima runtime exec

---

## Task 1: New Default Zellij Layout

**Files:**
- Modify: `layouts/default.kdl`
- Modify: `src/tui/mod.rs` (update preview ASCII art)

**Step 1: Rewrite `layouts/default.kdl`**

Replace the current layout with a 2-tab layout:

```kdl
// Tab 1 "Workspace": left-top=nvim(60%), left-bottom=terminal(40%), right=yazi(50%)
// Tab 2 "DevBox": left same, right-top=help(50%), right-bottom=management shell(50%)
// Tab 3 "Shell": plain shell
// Tab 4 "Git": lazygit
```

The KDL structure:

```kdl
layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
        children
    }

    tab name="Workspace" focus=true {
        pane split_direction="vertical" {
            pane split_direction="horizontal" size="50%" {
                pane name="editor" size="60%" {
                    command "nvim"
                    args "."
                }
                pane name="terminal" size="40%" focus=true
            }
            pane name="files" size="50%" {
                command "yazi"
                args "/workspace"
            }
        }
    }

    tab name="DevBox" {
        pane split_direction="vertical" {
            pane split_direction="horizontal" size="50%" {
                pane name="editor" size="60%" {
                    command "nvim"
                    args "."
                }
                pane name="terminal" size="40%"
            }
            pane split_direction="horizontal" size="50%" {
                pane name="help" size="50%" {
                    command "bash"
                    args "-c" "devbox guide | glow - 2>/dev/null || devbox guide; exec bash"
                }
                pane name="management" size="50%"
            }
        }
    }

    tab name="Shell" {
        pane name="main"
    }

    tab name="Git" {
        pane {
            command "lazygit"
        }
    }
}
```

**Step 2: Update ASCII preview in `src/tui/mod.rs`**

Update the `default` layout entry's `preview` field to match the new arrangement:

```
Tab 1 [Workspace]:
┌──────────────┬──────────────┐
│ nvim    60%  │              │
│              │  yazi        │
├──────────────┤  files  50%  │
│ terminal 40% │              │
└──────────────┴──────────────┘
       50%

Tab 2 [DevBox]:
┌──────────────┬──────────────┐
│ nvim    60%  │ help    50%  │
│              ├──────────────┤
├──────────────┤ management   │
│ terminal 40% │         50%  │
└──────────────┴──────────────┘
```

**Step 3: Run tests**

Run: `cargo test tui`
Expected: layout tests still pass (update test if description changed)

**Step 4: Commit**

```bash
git add layouts/default.kdl src/tui/mod.rs
git commit -m "feat: new default layout with workspace + devbox management tabs"
```

---

## Task 2: Launch Zellij from `devbox shell`

**Files:**
- Modify: `src/sandbox/mod.rs` — change `attach()` to launch Zellij
- Modify: `src/cli/shell.rs` — pass layout arg through to attach
- Modify: `src/sandbox/state.rs` — ensure layout is available

**Step 1: Update `ShellArgs` to pass layout through**

In `src/cli/shell.rs`, pass the layout option to `attach`:

```rust
pub async fn run(args: ShellArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    manager.attach(&name, args.layout.as_deref()).await
}
```

**Step 2: Update `attach()` in `src/sandbox/mod.rs`**

Change the attach method signature and implementation:

```rust
pub async fn attach(&self, name: &str, layout_override: Option<&str>) -> Result<()> {
    // ... existing start-if-stopped logic ...

    // Determine layout: CLI flag > saved state > "default"
    let layout = layout_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.layout.clone());

    // If layout is "plain", launch raw shell (no Zellij)
    if layout == "plain" {
        let shell = probe_shell(runtime.as_ref(), name).await;
        runtime.exec_cmd(name, &[&shell, "-l"], true).await?;
        return Ok(());
    }

    // Push the layout KDL file into the VM, then launch Zellij with it
    let layout_content = lookup_layout_kdl(&layout);
    push_layout_to_vm(runtime.as_ref(), name, &layout, &layout_content).await?;

    println!("Attaching to sandbox '{name}' (layout: {layout})...");
    runtime
        .exec_cmd(
            name,
            &["zellij", "--layout", &format!("/tmp/devbox-layout-{layout}.kdl")],
            true,
        )
        .await?;
    Ok(())
}
```

Add helper functions:

```rust
async fn probe_shell(runtime: &dyn Runtime, name: &str) -> String {
    let probe = runtime.exec_cmd(name, &["which", "zsh"], false).await;
    if probe.is_ok() && probe.unwrap().exit_code == 0 { "zsh".to_string() }
    else { "bash".to_string() }
}
```

**Step 3: Add layout KDL lookup**

Embed all layout KDL files in the binary (similar to help files). Add to `src/tui/mod.rs`:

```rust
pub static LAYOUT_FILES: &[(&str, &str)] = &[
    ("default", include_str!("../../layouts/default.kdl")),
    ("ai-pair", include_str!("../../layouts/ai-pair.kdl")),
    ("fullstack", include_str!("../../layouts/fullstack.kdl")),
    ("tdd", include_str!("../../layouts/tdd.kdl")),
    ("debug", include_str!("../../layouts/debug.kdl")),
    ("monitor", include_str!("../../layouts/monitor.kdl")),
    ("git-review", include_str!("../../layouts/git-review.kdl")),
    ("presentation", include_str!("../../layouts/presentation.kdl")),
];

pub fn lookup_layout_kdl(name: &str) -> String {
    LAYOUT_FILES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| {
            // Fall back to default
            LAYOUT_FILES[0].1.to_string()
        })
}
```

**Step 4: Add `push_layout_to_vm` helper in `src/sandbox/mod.rs`**

Use the existing `write_file_to_vm` pattern from provision.rs, or add a small helper:

```rust
async fn push_layout_to_vm(
    runtime: &dyn crate::runtime::Runtime,
    name: &str,
    layout_name: &str,
    content: &str,
) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let path = format!("/tmp/devbox-layout-{layout_name}.kdl");
    let cmd = format!("echo '{encoded}' | base64 -d > {path}");
    runtime.exec_cmd(name, &["bash", "-c", &cmd], false).await?;
    Ok(())
}
```

**Step 5: Update `create_or_attach` to pass layout=None**

In `src/sandbox/mod.rs`, update the call:

```rust
self.attach(&name, None).await
```

**Step 6: Run tests and verify**

Run: `cargo test`
Expected: all tests pass

Run: `cargo build --release` and test manually with a VM

**Step 7: Commit**

```bash
git add src/sandbox/mod.rs src/cli/shell.rs src/tui/mod.rs
git commit -m "feat: launch Zellij with layout on devbox shell"
```

---

## Task 3: Floating Help Pane (`devbox guide` inside Zellij)

**Files:**
- Modify: `src/cli/help.rs` — detect Zellij and open floating pane

**Step 1: Add Zellij floating pane support to `show_cheat_sheet`**

When running inside Zellij (check `$ZELLIJ` env var), open a floating pane instead of inline output:

```rust
fn show_cheat_sheet(name: &str) -> Result<()> {
    // Check if inside Zellij — use floating pane for pretty display
    if std::env::var("ZELLIJ").is_ok() {
        if let Some(content) = lookup_embedded(name) {
            return show_in_zellij_float(name, content);
        }
    }

    // ... existing fallback logic ...
}

fn show_in_zellij_float(name: &str, content: &str) -> Result<()> {
    use std::io::Write;

    // Write content to a temp file
    let tmp_path = format!("/tmp/devbox-guide-{name}.md");
    std::fs::write(&tmp_path, content)?;

    // Open floating pane with glow rendering
    let cmd = format!(
        "glow -p {} 2>/dev/null || less {}",
        tmp_path, tmp_path
    );
    std::process::Command::new("zellij")
        .args(["run", "--floating", "--close-on-exit", "--name", &format!("guide: {name}"), "--", "bash", "-c", &cmd])
        .status()?;

    Ok(())
}
```

**Step 2: Run tests**

Run: `cargo test cli::help`
Expected: existing tests pass (they don't run inside Zellij so the fallback path is tested)

**Step 3: Commit**

```bash
git add src/cli/help.rs
git commit -m "feat: devbox guide opens floating Zellij pane with pretty rendering"
```

---

## Task 4: Overlay Documentation in QUICKSTART.md

**Files:**
- Modify: `docs/QUICKSTART.md`

**Step 1: Add overlay section after "Day-to-Day Workflow"**

Insert a new section:

```markdown
## File Safety: The Overlay System

By default, devbox mounts your project directory **read-only** inside the VM using OverlayFS. All file changes happen in a separate overlay layer — your host files are never modified directly.

### How it works

```
Host filesystem (read-only base)
         │
    OverlayFS
         │
┌────────┴────────┐
│  Your changes   │  ← overlay "upper" layer (inside VM)
│  (writes here)  │
└─────────────────┘
```

When you edit a file inside the VM, the change is stored in the overlay layer. The original file on your host is untouched. This means:

- **You can experiment freely** — nothing changes on your host until you say so
- **You can always go back** — `devbox discard` throws away all overlay changes
- **You review before syncing** — `devbox diff` shows exactly what changed

### Workflow

```bash
# See what changed vs your host files
devbox diff

# Sync specific changes to your host
devbox commit --path src/

# Sync everything to your host
devbox commit

# Throw away all VM changes (reset to host state)
devbox discard
```

### Opting out

If you prefer direct file access (like a traditional VM), use writable mode:

```bash
devbox create --writable        # mount host directly
```

Or set it in `devbox.toml`:
```toml
[sandbox]
mount_mode = "writable"         # "overlay" (safe) | "writable" (direct)
```

**Warning:** In writable mode, file changes inside the VM immediately affect your host filesystem.
```

**Step 2: Commit**

```bash
git add docs/QUICKSTART.md
git commit -m "docs: add overlay system explanation to quickstart guide"
```

---

## Task 5: Package Reference with Links (`docs/PACKAGES.md`)

**Files:**
- Create: `docs/PACKAGES.md`

**Step 1: Write the package reference**

Create a comprehensive table for every set, with each package having:
- Name (as displayed to user)
- One-line description
- Official URL

Organize by set, with set description header. Cover all 14 sets and 90+ packages.

Key packages to document (representative sample — full list in implementation):

**System set (24 packages):** coreutils, gcc, gnumake, curl, wget, openssh, openssl, gnupg, tree, less, file, pkg-config, man-db, etc.

**Shell set (10 packages):** zellij (terminal multiplexer), zsh, starship (prompt), fzf (fuzzy finder), zoxide (cd replacement), direnv, yazi (file manager), etc.

**Tools set (21 packages):** ripgrep, fd, bat, eza, delta, sd, choose, jq, yq, fx, htop, procs, dust, duf, tokei, hyperfine, tealdeer, httpie, dog, glow, entr.

**Editor set:** neovim, helix, nano

**Git set:** git, lazygit, gh, git-lfs, git-crypt, pre-commit

**Container set:** docker, docker-compose, lazydocker, dive, buildkit, skopeo

**Network set:** tailscale, mosh, nmap, tcpdump, bandwhich, trippy, doggo

**AI set:** claude-code, aider-chat, ollama, open-webui, codex, huggingface-hub, mcp-hub, litellm, continue, opencode

**Language sets:** go+gopls+golangci-lint+delve, rustup+rust-analyzer+cargo-watch, python+uv+ruff+pyright, nodejs+bun+pnpm+typescript+biome, jdk+gradle+maven, ruby+bundler+solargraph

Format:
```markdown
## Shell Set (10 packages)

Terminal experience — multiplexer, prompt, navigation, and file management.

| Package | Description | Homepage |
|---------|------------|----------|
| zellij | Terminal multiplexer with built-in layouts | https://zellij.dev |
| ...
```

**Step 2: Add link to PACKAGES.md from README.md**

In the Tool Sets section of README.md, add: "See [Package Reference](docs/PACKAGES.md) for descriptions and official links for all 90+ packages."

**Step 3: Commit**

```bash
git add docs/PACKAGES.md README.md
git commit -m "docs: add package reference with descriptions and official links"
```

---

## Task 6: Fix `du-dust` → `dust` in `src/nix/sets.rs`

**Files:**
- Modify: `src/nix/sets.rs`

**Step 1: Fix the rename**

Change `"du-dust"` to `"dust"` in the tools set packages array.

**Step 2: Run tests**

Run: `cargo test nix::sets`
Expected: PASS

**Step 3: Commit**

```bash
git add src/nix/sets.rs
git commit -m "fix: rename du-dust to dust in nix sets (renamed in nixpkgs)"
```

---

## Execution Order

Tasks can be partially parallelized:
- **Task 6** (quick fix) → do first
- **Task 1** (layout KDL) → then this
- **Task 2** (Zellij shell launch) → depends on Task 1
- **Task 3** (floating help) → after Task 2
- **Task 4** (overlay docs) → independent, anytime
- **Task 5** (PACKAGES.md) → independent, anytime

Tasks 4+5 are pure documentation and can run in parallel with 1-3.
