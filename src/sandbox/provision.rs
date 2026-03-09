//! Post-create provisioning for VMs.
//!
//! Supports two image types:
//! - **NixOS**: push nix config files + `nixos-rebuild switch`
//! - **Ubuntu**: install Nix package manager + `nix profile install`
//!
//! Both paths use the same package definitions from nix/sets/*.nix.

use anyhow::Result;

use crate::runtime::Runtime;

// ── Embedded Nix files (for NixOS provisioning) ─────────────

const NIX_DEVBOX_MODULE: &str = include_str!("../../nix/devbox-module.nix");
const NIX_SETS_DEFAULT: &str = include_str!("../../nix/sets/default.nix");
const NIX_SETS_SYSTEM: &str = include_str!("../../nix/sets/system.nix");
const NIX_SETS_SHELL: &str = include_str!("../../nix/sets/shell.nix");
const NIX_SETS_TOOLS: &str = include_str!("../../nix/sets/tools.nix");
const NIX_SETS_EDITOR: &str = include_str!("../../nix/sets/editor.nix");
const NIX_SETS_GIT: &str = include_str!("../../nix/sets/git.nix");
const NIX_SETS_CONTAINER: &str = include_str!("../../nix/sets/container.nix");
const NIX_SETS_NETWORK: &str = include_str!("../../nix/sets/network.nix");
const NIX_SETS_AI_CODE: &str = include_str!("../../nix/sets/ai-code.nix");
const NIX_SETS_AI_INFRA: &str = include_str!("../../nix/sets/ai-infra.nix");
const NIX_SETS_LANG_GO: &str = include_str!("../../nix/sets/lang-go.nix");
const NIX_SETS_LANG_RUST: &str = include_str!("../../nix/sets/lang-rust.nix");
const NIX_SETS_LANG_PYTHON: &str = include_str!("../../nix/sets/lang-python.nix");
const NIX_SETS_LANG_NODE: &str = include_str!("../../nix/sets/lang-node.nix");
const NIX_SETS_LANG_JAVA: &str = include_str!("../../nix/sets/lang-java.nix");
const NIX_SETS_LANG_RUBY: &str = include_str!("../../nix/sets/lang-ruby.nix");

// ── Embedded config files (yazi, etc.) ───────────────────
const YAZI_CONFIG: &str = include_str!("../../configs/yazi/yazi.toml");
const YAZI_KEYMAP: &str = include_str!("../../configs/yazi/keymap.toml");
const YAZI_THEME: &str = include_str!("../../configs/yazi/theme.toml");
const YAZI_INIT: &str = include_str!("../../configs/yazi/init.lua");
const YAZI_GLOW_PLUGIN: &str = include_str!("../../configs/yazi/plugin/glow.yazi/init.lua");
const AICHAT_ROLES: &str = include_str!("../../configs/aichat/roles.yaml");
const MANAGEMENT_SCRIPT: &str = include_str!("../../configs/management.sh");

/// All set nix files: (filename, content)
const NIX_SET_FILES: &[(&str, &str)] = &[
    ("default.nix", NIX_SETS_DEFAULT),
    ("system.nix", NIX_SETS_SYSTEM),
    ("shell.nix", NIX_SETS_SHELL),
    ("tools.nix", NIX_SETS_TOOLS),
    ("editor.nix", NIX_SETS_EDITOR),
    ("git.nix", NIX_SETS_GIT),
    ("container.nix", NIX_SETS_CONTAINER),
    ("network.nix", NIX_SETS_NETWORK),
    ("ai-code.nix", NIX_SETS_AI_CODE),
    ("ai-infra.nix", NIX_SETS_AI_INFRA),
    ("lang-go.nix", NIX_SETS_LANG_GO),
    ("lang-rust.nix", NIX_SETS_LANG_RUST),
    ("lang-python.nix", NIX_SETS_LANG_PYTHON),
    ("lang-node.nix", NIX_SETS_LANG_NODE),
    ("lang-java.nix", NIX_SETS_LANG_JAVA),
    ("lang-ruby.nix", NIX_SETS_LANG_RUBY),
];

// ── Package name mapping (for Ubuntu/Nix profile install) ───
// These map set names to nixpkgs attribute paths for `nix profile install`.
// The names match the nix/sets/*.nix files exactly.

fn nix_packages_for_set(set: &str) -> Vec<&'static str> {
    match set {
        "system" => vec![
            "coreutils", "gnugrep", "gnused", "gawk", "findutils", "diffutils",
            "gzip", "gnutar", "xz", "bzip2", "file", "which", "tree", "less",
            "curl", "wget", "openssh", "openssl", "cacert", "gnupg",
            "gcc", "gnumake", "pkg-config", "man-db",
        ],
        "shell" => vec![
            "zellij", "zsh", "zsh-autosuggestions", "zsh-syntax-highlighting",
            "starship", "fzf", "zoxide", "direnv", "nix-direnv", "yazi", "micro",
        ],
        "tools" => vec![
            "ripgrep", "fd", "bat", "eza", "delta", "sd", "choose",
            "jq", "yq-go", "fx", "htop", "bottom", "procs", "dust", "duf",
            "tokei", "hyperfine", "tealdeer", "httpie", "dog", "glow", "entr",
        ],
        "editor" => vec!["neovim", "helix", "nano"],
        "git" => vec!["git", "lazygit", "gh", "git-lfs", "git-crypt", "pre-commit"],
        "container" => vec!["docker", "docker-compose", "lazydocker", "dive", "buildkit", "skopeo"],
        "network" => vec!["tailscale", "mosh", "nmap", "tcpdump", "bandwhich", "trippy", "doggo"],
        "ai-code" => vec![
            "claude-code", "codex", "opencode", "aider-chat", "aichat", "continue",
        ],
        "ai-infra" => vec![
            "ollama", "open-webui", "litellm", "mcp-hub",
            "python312Packages.huggingface-hub",
        ],
        "lang-go" => vec!["go", "gopls", "golangci-lint", "delve", "gotools", "gore"],
        "lang-rust" => vec!["rustup", "rust-analyzer", "cargo-watch", "cargo-edit", "cargo-expand", "sccache"],
        "lang-python" => vec![
            "python312", "uv", "ruff", "pyright",
            "python312Packages.ipython", "python312Packages.pytest",
        ],
        "lang-node" => vec![
            "nodejs_22", "bun", "pnpm", "typescript",
            "nodePackages.typescript-language-server", "biome",
        ],
        "lang-java" => vec!["jdk21", "gradle", "maven", "jdt-language-server"],
        "lang-ruby" => vec!["ruby_3_3", "bundler", "solargraph", "rubocop"],
        _ => vec![],
    }
}

// ── Public API ──────────────────────────────────────────────

/// Provision a VM with tools based on active sets and languages.
/// Dispatches to NixOS or Ubuntu provisioning based on image type.
pub async fn provision_vm(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
) -> Result<()> {
    match image {
        "ubuntu" => provision_ubuntu(runtime, name, sets, languages).await,
        _ => provision_nixos(runtime, name, sets, languages).await,
    }
}

// ── NixOS Provisioning ─────────────────────────────────────

/// Provision a NixOS VM: push nix config files + nixos-rebuild switch.
async fn provision_nixos(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
) -> Result<()> {
    let username = whoami();

    // 1. Create directory structure
    println!("Setting up NixOS configuration...");
    runtime
        .exec_cmd(
            name,
            &["sudo", "mkdir", "-p", "/etc/devbox/sets", "/etc/devbox/help"],
            false,
        )
        .await?;

    // 2. Generate base NixOS config if it doesn't exist
    //    NixOS Lima images ship with an empty /etc/nixos/ — we need to
    //    run nixos-generate-config to create the hardware and base configs.
    ensure_nixos_config(runtime, name).await?;

    // 3. Push devbox-state.toml
    let state_toml = generate_state_toml(sets, languages, &username);
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &state_toml).await?;

    // 4. Push devbox-module.nix
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-module.nix", NIX_DEVBOX_MODULE).await?;

    // 5. Push all set .nix files
    for (filename, content) in NIX_SET_FILES {
        let path = format!("/etc/devbox/sets/{filename}");
        write_file_to_vm(runtime, name, &path, content).await?;
    }

    // 6. Run nixos-rebuild switch (interactive so user sees progress)
    //    NixOS Lima images use flake-based NIX_PATH (nixpkgs=flake:nixpkgs)
    //    which doesn't include nixos-config. We must set it explicitly.
    println!("Installing packages via nixos-rebuild (this may take a few minutes)...");
    let rebuild_cmd = concat!(
        "export NIX_PATH=\"nixos-config=/etc/nixos/configuration.nix:$NIX_PATH\" && ",
        "export NIXPKGS_ALLOW_UNFREE=1 && ",
        "nixos-rebuild switch"
    );
    let result = runtime
        .exec_cmd(name, &["sudo", "bash", "-c", rebuild_cmd], true)
        .await?;

    if result.exit_code != 0 {
        eprintln!("Warning: nixos-rebuild failed (exit {})", result.exit_code);
        eprintln!("You can retry with `devbox exec --name {name} -- sudo nixos-rebuild switch`");
    } else {
        println!("NixOS rebuild complete.");
    }

    // 8. Copy devbox binary + help files + tool configs
    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;
    setup_management_script(runtime, name).await?;
    setup_yazi_config(runtime, name).await?;
    setup_aichat_config(runtime, name).await?;
    setup_ai_tool_configs(runtime, name).await?;

    Ok(())
}

// ── Ubuntu Provisioning ─────────────────────────────────────

/// Provision an Ubuntu VM: install Nix package manager + nix profile install.
async fn provision_ubuntu(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
) -> Result<()> {
    // 1. Install the Nix package manager
    println!("Installing Nix package manager on Ubuntu...");
    let install_nix = r#"if ! command -v nix >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install --no-confirm 2>&1
fi"#;
    let result = runtime
        .exec_cmd(name, &["bash", "-c", install_nix], false)
        .await?;

    if result.exit_code != 0 {
        eprintln!("Warning: Nix installation may have issues: {}", result.stderr.trim());
    }

    // 2. Collect all package names from active sets
    let mut packages: Vec<&str> = vec![];
    for set in sets {
        packages.extend(nix_packages_for_set(set));
    }
    for lang in languages {
        let set_name = format!("lang-{lang}");
        packages.extend(nix_packages_for_set(&set_name));
    }
    packages.sort();
    packages.dedup();

    if !packages.is_empty() {
        // 3. Install all packages via nix profile install
        let pkg_args: Vec<String> = packages.iter().map(|p| format!("nixpkgs#{p}")).collect();
        let pkg_list = pkg_args.join(" ");

        println!("Installing {} packages via Nix (this may take a few minutes)...", packages.len());

        // Source the nix profile before running nix commands
        let install_cmd = format!(
            ". /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh && nix profile install {pkg_list}"
        );
        let result = runtime
            .exec_cmd(name, &["bash", "-c", &install_cmd], true)
            .await?;

        if result.exit_code != 0 {
            eprintln!("Warning: some packages failed to install.");
            eprintln!("You can retry with: devbox exec --name {name} -- nix profile install <packages>");
        } else {
            println!("Nix package installation complete.");
        }
    }

    // 4. Install services that need apt (Docker, Tailscale)
    install_ubuntu_services(runtime, name, sets).await?;

    // 5. Set up shell environment
    setup_ubuntu_shell(runtime, name).await?;

    // 6. Create devbox directories and copy binary + help
    runtime
        .exec_cmd(
            name,
            &["sudo", "mkdir", "-p", "/etc/devbox/help"],
            false,
        )
        .await?;

    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;
    setup_management_script(runtime, name).await?;
    setup_yazi_config(runtime, name).await?;
    setup_aichat_config(runtime, name).await?;
    setup_ai_tool_configs(runtime, name).await?;

    Ok(())
}

/// Install services that need OS-level integration on Ubuntu.
/// Nix installs the binaries but systemd services need apt packages.
async fn install_ubuntu_services(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
) -> Result<()> {
    let needs_docker = sets.iter().any(|s| s == "container");
    let needs_tailscale = sets.iter().any(|s| s == "network");

    if needs_docker {
        print!("  Setting up Docker service...");
        let cmd = "export DEBIAN_FRONTEND=noninteractive && \
            sudo apt-get update -qq && \
            sudo apt-get install -y -qq docker.io >/dev/null 2>&1 && \
            sudo usermod -aG docker $(whoami) && \
            sudo systemctl enable --now docker";
        let result = runtime.exec_cmd(name, &["bash", "-c", cmd], false).await;
        match result {
            Ok(r) if r.exit_code == 0 => println!(" done"),
            _ => println!(" skipped"),
        }
    }

    if needs_tailscale {
        print!("  Setting up Tailscale service...");
        let cmd = "curl -fsSL https://tailscale.com/install.sh | sh && \
            sudo systemctl enable --now tailscaled";
        let result = runtime.exec_cmd(name, &["bash", "-c", cmd], false).await;
        match result {
            Ok(r) if r.exit_code == 0 => println!(" done"),
            _ => println!(" skipped"),
        }
    }

    Ok(())
}

/// Set up shell environment on Ubuntu (zsh + starship + PATH).
async fn setup_ubuntu_shell(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let username = whoami();

    // Add Nix profile to shell init and set up zsh as default
    let setup = format!(
        r#"
# Set zsh as default shell if installed via Nix
NIX_ZSH="$(. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh && which zsh 2>/dev/null)"
if [ -n "$NIX_ZSH" ]; then
  echo "$NIX_ZSH" | sudo tee -a /etc/shells >/dev/null
  sudo chsh -s "$NIX_ZSH" {username}
fi

# Create .zshrc with Nix integration
cat > /home/{username}/.zshrc << 'ZSHRC'
# Nix
if [ -e '/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh' ]; then
  . '/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh'
fi

# Nix profile binaries
export PATH="$HOME/.nix-profile/bin:$PATH"

# Starship prompt
if command -v starship >/dev/null 2>&1; then
  eval "$(starship init zsh)"
fi

# Zoxide
if command -v zoxide >/dev/null 2>&1; then
  eval "$(zoxide init zsh)"
fi

# fzf
if command -v fzf >/dev/null 2>&1; then
  source <(fzf --zsh) 2>/dev/null
fi

# Editor
export EDITOR=nvim
export VISUAL=nvim

# Aliases
alias ls='eza --icons' 2>/dev/null
alias cat='bat --paging=never' 2>/dev/null
alias top='htop' 2>/dev/null

# Devbox identity
export DEVBOX_NAME="${{DEVBOX_NAME:-devbox}}"
export DEVBOX_RUNTIME="${{DEVBOX_RUNTIME:-unknown}}"
ZSHRC
"#
    );

    let result = runtime.exec_cmd(name, &["bash", "-c", &setup], false).await;
    if let Ok(r) = result {
        if r.exit_code != 0 {
            eprintln!("Warning: shell setup incomplete");
        }
    }

    Ok(())
}

// ── Shared Helpers ──────────────────────────────────────────

/// Generate devbox-state.toml content from active sets and languages.
fn generate_state_toml(sets: &[String], languages: &[String], username: &str) -> String {
    let set_names = ["system", "shell", "tools", "editor", "git", "container", "network", "ai_code", "ai_infra"];
    let lang_names = ["go", "rust", "python", "node", "java", "ruby"];

    let mut toml = String::from("[user]\n");
    toml.push_str(&format!("name = \"{username}\"\n\n"));

    toml.push_str("[sets]\n");
    for s in &set_names {
        // Normalize: active sets use hyphens ("ai-code") but TOML keys use underscores ("ai_code")
        let hyphenated = s.replace('_', "-");
        let enabled = sets.iter().any(|active| active == s || active == &hyphenated);
        toml.push_str(&format!("{s} = {enabled}\n"));
    }

    toml.push_str("\n[languages]\n");
    for l in &lang_names {
        let enabled = languages.iter().any(|active| active == l)
            || sets.iter().any(|active| active == &format!("lang-{l}"));
        toml.push_str(&format!("{l} = {enabled}\n"));
    }

    toml
}

// ── AI Tool Config Detection & Copy ────────────────────────

/// Describes an AI coding tool's host configuration.
struct AiToolConfig {
    name: &'static str,
    /// Files to copy: (host_path_suffix, vm_path_suffix)
    /// Paths are relative to home directory.
    config_files: &'static [(&'static str, &'static str)],
    /// Environment variables that hold API keys.
    env_vars: &'static [&'static str],
}

/// Known AI tool configurations.
static AI_TOOL_CONFIGS: &[AiToolConfig] = &[
    AiToolConfig {
        name: "claude-code",
        config_files: &[
            (".claude/.credentials.json", ".claude/.credentials.json"),
            (".claude/settings.json", ".claude/settings.json"),
        ],
        env_vars: &["ANTHROPIC_API_KEY"],
    },
    AiToolConfig {
        name: "opencode",
        config_files: &[
            (".config/opencode/config.json", ".config/opencode/config.json"),
        ],
        env_vars: &["OPENAI_API_KEY"],
    },
    AiToolConfig {
        name: "codex",
        config_files: &[
            (".codex/config.json", ".codex/config.json"),
            (".codex/auth.json", ".codex/auth.json"),
        ],
        env_vars: &["OPENAI_API_KEY"],
    },
    AiToolConfig {
        name: "aichat",
        config_files: &[
            (".config/aichat/config.yaml", ".config/aichat/config.yaml"),
        ],
        env_vars: &[],
    },
];

/// Detect AI tool configurations on the host and copy them into the VM.
/// Checks for config files and API key env vars in priority order:
/// claude-code → opencode → codex → aichat.
async fn setup_ai_tool_configs(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let username = whoami();
    let vm_home = format!("/home/{username}");
    let mut copied_any = false;

    for tool in AI_TOOL_CONFIGS {
        let mut found_files = vec![];

        // Check which config files exist on host
        for (host_suffix, _vm_suffix) in tool.config_files {
            let host_path = home.join(host_suffix);
            if host_path.exists() {
                found_files.push(host_suffix);
            }
        }

        // Check env vars
        let mut found_env_vars = vec![];
        for var in tool.env_vars {
            if std::env::var(var).is_ok() {
                found_env_vars.push(*var);
            }
        }

        if found_files.is_empty() && found_env_vars.is_empty() {
            continue;
        }

        // Report what we found
        println!("Found {} configuration on host:", tool.name);
        for f in &found_files {
            println!("  config: ~/{f}");
        }
        for v in &found_env_vars {
            println!("  env:    {v}");
        }

        // Copy config files
        for (host_suffix, vm_suffix) in tool.config_files {
            let host_path = home.join(host_suffix);
            if !host_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&host_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  Warning: cannot read ~/{host_suffix}: {e}");
                    continue;
                }
            };

            let vm_path = format!("{vm_home}/{vm_suffix}");

            // Ensure parent directory exists
            let vm_parent = vm_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(&vm_path);
            runtime
                .exec_cmd(
                    name,
                    &["sudo", "mkdir", "-p", vm_parent],
                    false,
                )
                .await?;

            write_file_to_vm(runtime, name, &vm_path, &content).await?;
            copied_any = true;
        }

        // Write env vars to a sourced file
        if !found_env_vars.is_empty() {
            let env_file = format!("{vm_home}/.devbox-ai-env");
            let mut env_content = String::from("# AI tool API keys (sourced by .zshrc)\n");
            for var in &found_env_vars {
                if let Ok(val) = std::env::var(var) {
                    env_content.push_str(&format!("export {var}=\"{val}\"\n"));
                }
            }
            write_file_to_vm(runtime, name, &env_file, &env_content).await?;

            // Source it from .zshrc if not already
            let source_line = "[ -f ~/.devbox-ai-env ] && source ~/.devbox-ai-env";
            let add_source_cmd = format!(
                "grep -qF 'devbox-ai-env' {vm_home}/.zshrc 2>/dev/null || echo '{source_line}' >> {vm_home}/.zshrc"
            );
            runtime
                .exec_cmd(name, &["bash", "-c", &add_source_cmd], false)
                .await?;
            copied_any = true;
        }

        // Fix ownership for all copied files
        let chown_cmd = format!("chown -R {username}:{username} {vm_home}/.claude {vm_home}/.config {vm_home}/.codex {vm_home}/.devbox-ai-env 2>/dev/null; true");
        runtime
            .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
            .await?;
    }

    if copied_any {
        println!("AI tool configurations synced to devbox.");
    }

    // Auto-generate aichat config from detected credentials if no host config was copied.
    let has_aichat_config = home.join(".config/aichat/config.yaml").exists();
    if !has_aichat_config {
        if let Some(config) = generate_aichat_config_from_credentials(&home) {
            let config_dir = format!("{vm_home}/.config/aichat");
            runtime
                .exec_cmd(name, &["sudo", "mkdir", "-p", &config_dir], false)
                .await?;
            let config_path = format!("{config_dir}/config.yaml");
            write_file_to_vm(runtime, name, &config_path, &config).await?;
            let chown_cmd = format!("chown -R {username}:{username} {config_dir}");
            runtime
                .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
                .await?;
            println!("Generated aichat config from detected AI tool credentials.");
        }
    }

    Ok(())
}

/// Try to generate an aichat config.yaml from existing AI tool credentials.
/// Priority: Anthropic (claude-code) → OpenAI (opencode/codex).
/// Returns None if no credentials found.
fn generate_aichat_config_from_credentials(home: &std::path::Path) -> Option<String> {
    // Check for Anthropic API key (env var or claude-code credentials)
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok().or_else(|| {
        // Try to extract from claude-code credentials
        let creds_path = home.join(".claude/.credentials.json");
        let content = std::fs::read_to_string(creds_path).ok()?;
        // credentials.json may contain OAuth tokens, not API keys.
        // Only extract if it looks like an API key.
        if content.contains("sk-ant-") {
            // Simple extraction — look for api_key field
            let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
            parsed.get("apiKey")
                .or_else(|| parsed.get("api_key"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    });

    if let Some(key) = anthropic_key {
        return Some(format!(
            "model: claude:claude-sonnet-4-20250514\n\
             clients:\n\
             - type: claude\n\
               api_key: {key}\n"
        ));
    }

    // Check for OpenAI API key
    let openai_key = std::env::var("OPENAI_API_KEY").ok().or_else(|| {
        // Try opencode config
        let opencode_path = home.join(".config/opencode/config.json");
        if let Ok(content) = std::fs::read_to_string(opencode_path) {
            let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
            parsed.get("apiKey")
                .or_else(|| parsed.get("api_key"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    });

    if let Some(key) = openai_key {
        return Some(format!(
            "model: openai:gpt-4o\n\
             clients:\n\
             - type: openai\n\
               api_key: {key}\n"
        ));
    }

    None
}

/// Write a file into the VM using base64-encoded content via exec_cmd.
async fn write_file_to_vm(
    runtime: &dyn Runtime,
    name: &str,
    path: &str,
    content: &str,
) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let cmd = format!("echo '{encoded}' | base64 -d | sudo tee {path} > /dev/null");
    let result = runtime
        .exec_cmd(name, &["bash", "-c", &cmd], false)
        .await?;
    if result.exit_code != 0 {
        eprintln!("Warning: failed to write {path}: {}", result.stderr.trim());
    }
    Ok(())
}

/// Ensure /etc/nixos/configuration.nix and hardware-configuration.nix exist.
///
/// NixOS Lima images ship with an empty /etc/nixos/ directory.
/// We run `nixos-generate-config` to create hardware-configuration.nix,
/// then write our own minimal configuration.nix with correct bootloader
/// settings and the devbox module import already included.
async fn ensure_nixos_config(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // Generate hardware-configuration.nix (always safe to regenerate)
    let hw_check = runtime
        .exec_cmd(
            name,
            &["test", "-f", "/etc/nixos/hardware-configuration.nix"],
            false,
        )
        .await?;

    if hw_check.exit_code != 0 {
        println!("  Generating hardware configuration...");
        let result = runtime
            .exec_cmd(name, &["sudo", "nixos-generate-config"], false)
            .await?;
        if result.exit_code != 0 {
            eprintln!(
                "Warning: nixos-generate-config failed: {}",
                result.stderr.trim()
            );
        }
    }

    // Detect the actual bootloader: check if GRUB config exists
    let grub_check = runtime
        .exec_cmd(name, &["test", "-f", "/boot/grub/grub.cfg"], false)
        .await?;
    let uses_grub = grub_check.exit_code == 0;

    // Write our own configuration.nix with correct bootloader and devbox import.
    // We always overwrite to ensure a clean, known-good configuration.
    let bootloader_config = if uses_grub {
        r#"  # GRUB bootloader (matches the pre-built image)
  boot.loader.grub.enable = true;
  boot.loader.grub.device = "nodev";
  boot.loader.grub.efiSupport = true;
  boot.loader.grub.efiInstallAsRemovable = true;"#
    } else {
        r#"  # systemd-boot EFI bootloader
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;"#
    };

    let config_nix = format!(
        r#"# Devbox-managed NixOS configuration
# Do not edit — this file is overwritten by devbox provisioning.
{{ config, lib, pkgs, ... }}:

{{
  imports = [
    ./hardware-configuration.nix
    /etc/devbox/devbox-module.nix
  ];

{bootloader_config}

  # Networking
  networking.networkmanager.enable = true;

  # OpenSSH for Lima access
  services.openssh.enable = true;

  # NixOS state version — matches the pre-built image
  system.stateVersion = lib.mkDefault "25.11";
}}
"#
    );

    write_file_to_vm(runtime, name, "/etc/nixos/configuration.nix", &config_nix).await?;

    Ok(())
}

/// Copy the current devbox binary into the VM at /usr/local/bin/devbox.
async fn copy_devbox_to_vm(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();

    if runtime.name() == "lima" {
        let vm_name = format!("devbox-{name}");
        let result = crate::runtime::cmd::run_cmd(
            "limactl",
            &["copy", &exe_str, &format!("{vm_name}:/tmp/devbox")],
        )
        .await;
        if let Ok(r) = result {
            if r.exit_code == 0 {
                let _ = runtime
                    .exec_cmd(
                        name,
                        &[
                            "sudo", "install", "-m", "755",
                            "/tmp/devbox", "/usr/local/bin/devbox",
                        ],
                        false,
                    )
                    .await;
                let _ = runtime
                    .exec_cmd(name, &["rm", "/tmp/devbox"], false)
                    .await;
            }
        }
    }
    Ok(())
}

/// Push yazi config files to all user home directories in the VM.
async fn setup_yazi_config(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let username = whoami();
    let config_dir = format!("/home/{username}/.config/yazi");

    // Create config directory
    runtime
        .exec_cmd(name, &["sudo", "mkdir", "-p", &config_dir], false)
        .await?;

    // Write all yazi config files
    let files: &[(&str, &str)] = &[
        ("yazi.toml", YAZI_CONFIG),
        ("keymap.toml", YAZI_KEYMAP),
        ("theme.toml", YAZI_THEME),
        ("init.lua", YAZI_INIT),
    ];
    for (filename, content) in files {
        let path = format!("{config_dir}/{filename}");
        write_file_to_vm(runtime, name, &path, content).await?;
    }

    // Write glow previewer plugin
    let plugin_dir = format!("{config_dir}/plugins/glow.yazi");
    runtime
        .exec_cmd(name, &["sudo", "mkdir", "-p", &plugin_dir], false)
        .await?;
    write_file_to_vm(runtime, name, &format!("{plugin_dir}/init.lua"), YAZI_GLOW_PLUGIN).await?;

    // Fix ownership
    let chown_cmd = format!("chown -R {username}:{username} /home/{username}/.config/yazi");
    runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
        .await?;

    Ok(())
}

/// Push aichat config (roles) to user home directory in the VM.
async fn setup_aichat_config(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let username = whoami();
    let config_dir = format!("/home/{username}/.config/aichat");

    runtime
        .exec_cmd(name, &["sudo", "mkdir", "-p", &config_dir], false)
        .await?;

    let path = format!("{config_dir}/roles.yaml");
    write_file_to_vm(runtime, name, &path, AICHAT_ROLES).await?;

    let chown_cmd = format!("chown -R {username}:{username} {config_dir}");
    runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
        .await?;

    Ok(())
}

/// Push the management panel script to /etc/devbox/management.sh inside the VM.
async fn setup_management_script(runtime: &dyn Runtime, name: &str) -> Result<()> {
    write_file_to_vm(runtime, name, "/etc/devbox/management.sh", MANAGEMENT_SCRIPT).await?;
    runtime
        .exec_cmd(name, &["sudo", "chmod", "+x", "/etc/devbox/management.sh"], false)
        .await?;
    Ok(())
}

/// Write embedded help files to /etc/devbox/help/ inside the VM.
async fn setup_help_in_vm(runtime: &dyn Runtime, name: &str) -> Result<()> {
    for (help_name, content) in super::super::cli::help::CHEAT_SHEETS.iter() {
        let path = format!("/etc/devbox/help/{help_name}.md");
        write_file_to_vm(runtime, name, &path, content).await?;
    }
    Ok(())
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "dev".to_string())
}

// ── Overlay Mount Setup ─────────────────────────────────────

/// Set up OverlayFS mount inside the VM.
/// Creates overlay directories and a systemd mount unit so that
/// /mnt/host (read-only host mount) is overlaid onto /workspace
/// with a writable upper layer at /var/devbox/overlay/upper.
///
/// Only call this when mount_mode is "overlay" (not "writable").
#[allow(dead_code)]
pub async fn setup_overlay_mount(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // 1. Create overlay directories
    runtime
        .exec_cmd(
            name,
            &[
                "sudo", "mkdir", "-p",
                "/var/devbox/overlay/upper",
                "/var/devbox/overlay/work",
                "/mnt/host",
                "/workspace",
            ],
            false,
        )
        .await?;

    // 2. Write systemd mount unit
    let mount_unit = r#"[Unit]
Description=DevBox OverlayFS workspace
After=local-fs.target
RequiresMountsFor=/mnt/host

[Mount]
What=overlay
Where=/workspace
Type=overlay
Options=lowerdir=/mnt/host,upperdir=/var/devbox/overlay/upper,workdir=/var/devbox/overlay/work

[Install]
WantedBy=multi-user.target
"#;

    write_file_to_vm(runtime, name, "/etc/systemd/system/workspace.mount", mount_unit).await?;

    // 3. Enable and start the mount
    runtime
        .exec_cmd(
            name,
            &["sudo", "systemctl", "daemon-reload"],
            false,
        )
        .await?;

    runtime
        .exec_cmd(
            name,
            &["sudo", "systemctl", "enable", "--now", "workspace.mount"],
            false,
        )
        .await?;

    println!("OverlayFS workspace mount configured.");
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_state_toml_basic() {
        let sets = vec![
            "system".to_string(),
            "shell".to_string(),
            "tools".to_string(),
            "editor".to_string(),
            "git".to_string(),
            "container".to_string(),
        ];
        let langs = vec!["go".to_string()];
        let toml = generate_state_toml(&sets, &langs, "testuser");

        assert!(toml.contains("name = \"testuser\""));
        assert!(toml.contains("system = true"));
        assert!(toml.contains("shell = true"));
        assert!(toml.contains("container = true"));
        assert!(toml.contains("network = false"));
        assert!(toml.contains("ai_code = false"));
        assert!(toml.contains("ai_infra = false"));
        assert!(toml.contains("go = true"));
        assert!(toml.contains("rust = false"));
    }

    #[test]
    fn generate_state_toml_with_lang_prefix() {
        let sets = vec![
            "system".to_string(),
            "shell".to_string(),
            "tools".to_string(),
            "lang-rust".to_string(),
        ];
        let langs = vec![];
        let toml = generate_state_toml(&sets, &langs, "dev");

        assert!(toml.contains("rust = true"));
        assert!(toml.contains("go = false"));
    }

    #[test]
    fn generate_state_toml_hyphenated_sets() {
        // active_sets() returns "ai-code" (hyphen), must match "ai_code" (underscore) in TOML
        let sets = vec![
            "system".to_string(),
            "shell".to_string(),
            "tools".to_string(),
            "ai-code".to_string(),
        ];
        let langs = vec![];
        let toml = generate_state_toml(&sets, &langs, "dev");

        assert!(toml.contains("ai_code = true"));
        assert!(toml.contains("ai_infra = false"));
    }

    #[test]
    fn generate_state_toml_bare() {
        let sets = vec![];
        let langs = vec![];
        let toml = generate_state_toml(&sets, &langs, "user");

        assert!(toml.contains("system = false"));
        assert!(toml.contains("go = false"));
        assert!(toml.contains("name = \"user\""));
    }

    #[test]
    fn nix_packages_system_set() {
        let pkgs = nix_packages_for_set("system");
        assert_eq!(pkgs.len(), 24);
        assert!(pkgs.contains(&"coreutils"));
        assert!(pkgs.contains(&"gcc"));
        assert!(pkgs.contains(&"curl"));
    }

    #[test]
    fn nix_packages_shell_set() {
        let pkgs = nix_packages_for_set("shell");
        assert_eq!(pkgs.len(), 11);
        assert!(pkgs.contains(&"zellij"));
        assert!(pkgs.contains(&"starship"));
        assert!(pkgs.contains(&"yazi"));
    }

    #[test]
    fn nix_packages_tools_set() {
        let pkgs = nix_packages_for_set("tools");
        assert_eq!(pkgs.len(), 22);
        assert!(pkgs.contains(&"ripgrep"));
        assert!(pkgs.contains(&"bat"));
    }

    #[test]
    fn nix_packages_lang_go() {
        let pkgs = nix_packages_for_set("lang-go");
        assert_eq!(pkgs.len(), 6);
        assert!(pkgs.contains(&"go"));
        assert!(pkgs.contains(&"gopls"));
    }

    #[test]
    fn nix_packages_lang_python_has_nested() {
        let pkgs = nix_packages_for_set("lang-python");
        assert!(pkgs.contains(&"python312Packages.ipython"));
        assert!(pkgs.contains(&"uv"));
    }

    #[test]
    fn nix_packages_unknown_set() {
        let pkgs = nix_packages_for_set("nonexistent");
        assert!(pkgs.is_empty());
    }

    #[test]
    fn nix_packages_all_sets_have_packages() {
        let all_sets = [
            "system", "shell", "tools", "editor", "git", "container",
            "network", "ai-code", "ai-infra", "lang-go", "lang-rust", "lang-python",
            "lang-node", "lang-java", "lang-ruby",
        ];
        for set in &all_sets {
            let pkgs = nix_packages_for_set(set);
            assert!(!pkgs.is_empty(), "set '{set}' should have packages");
        }
    }
}
