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
const NIX_SETS_AI: &str = include_str!("../../nix/sets/ai.nix");
const NIX_SETS_LANG_GO: &str = include_str!("../../nix/sets/lang-go.nix");
const NIX_SETS_LANG_RUST: &str = include_str!("../../nix/sets/lang-rust.nix");
const NIX_SETS_LANG_PYTHON: &str = include_str!("../../nix/sets/lang-python.nix");
const NIX_SETS_LANG_NODE: &str = include_str!("../../nix/sets/lang-node.nix");
const NIX_SETS_LANG_JAVA: &str = include_str!("../../nix/sets/lang-java.nix");
const NIX_SETS_LANG_RUBY: &str = include_str!("../../nix/sets/lang-ruby.nix");

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
    ("ai.nix", NIX_SETS_AI),
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
            "starship", "fzf", "zoxide", "direnv", "nix-direnv", "yazi",
        ],
        "tools" => vec![
            "ripgrep", "fd", "bat", "eza", "delta", "sd", "choose",
            "jq", "yq-go", "fx", "htop", "procs", "du-dust", "duf",
            "tokei", "hyperfine", "tealdeer", "httpie", "dog", "glow", "entr",
        ],
        "editor" => vec!["neovim", "helix", "nano"],
        "git" => vec!["git", "lazygit", "gh", "git-lfs", "git-crypt", "pre-commit"],
        "container" => vec!["docker", "docker-compose", "lazydocker", "dive", "buildkit", "skopeo"],
        "network" => vec!["tailscale", "mosh", "nmap", "tcpdump", "bandwhich", "trippy", "doggo"],
        "ai" => vec![
            "claude-code", "aider-chat", "ollama", "open-webui", "codex",
            "python312Packages.huggingface-hub", "mcp-hub", "litellm", "continue", "opencode",
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

    // 2. Push devbox-state.toml
    let state_toml = generate_state_toml(sets, languages, &username);
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &state_toml).await?;

    // 3. Push devbox-module.nix
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-module.nix", NIX_DEVBOX_MODULE).await?;

    // 4. Push all set .nix files
    for (filename, content) in NIX_SET_FILES {
        let path = format!("/etc/devbox/sets/{filename}");
        write_file_to_vm(runtime, name, &path, content).await?;
    }

    // 5. Add import to the VM's /etc/nixos/configuration.nix
    add_devbox_import(runtime, name).await?;

    // 6. Run nixos-rebuild switch
    println!("Installing packages via nixos-rebuild (this may take a few minutes)...");
    let result = runtime
        .exec_cmd(
            name,
            &["sudo", "nixos-rebuild", "switch", "--show-trace"],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        eprintln!("Warning: nixos-rebuild failed (exit {}):", result.exit_code);
        let stderr_lines: Vec<&str> = result.stderr.lines().collect();
        let start = stderr_lines.len().saturating_sub(20);
        for line in &stderr_lines[start..] {
            eprintln!("  {line}");
        }
        eprintln!("You can retry with `devbox exec --name {name} -- sudo nixos-rebuild switch`");
    } else {
        println!("NixOS rebuild complete.");
    }

    // 7. Copy devbox binary + help files
    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;

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
            .exec_cmd(name, &["bash", "-c", &install_cmd], false)
            .await?;

        if result.exit_code != 0 {
            eprintln!("Warning: some packages failed to install:");
            let stderr_lines: Vec<&str> = result.stderr.lines().collect();
            let start = stderr_lines.len().saturating_sub(15);
            for line in &stderr_lines[start..] {
                eprintln!("  {line}");
            }
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
    let set_names = ["system", "shell", "tools", "editor", "git", "container", "network", "ai"];
    let lang_names = ["go", "rust", "python", "node", "java", "ruby"];

    let mut toml = String::from("[user]\n");
    toml.push_str(&format!("name = \"{username}\"\n\n"));

    toml.push_str("[sets]\n");
    for s in &set_names {
        let enabled = sets.iter().any(|active| active == s);
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

/// Add devbox module import to the VM's /etc/nixos/configuration.nix.
async fn add_devbox_import(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let check = runtime
        .exec_cmd(
            name,
            &["grep", "-q", "devbox-module", "/etc/nixos/configuration.nix"],
            false,
        )
        .await?;

    if check.exit_code == 0 {
        return Ok(());
    }

    runtime
        .exec_cmd(
            name,
            &[
                "sudo", "cp",
                "/etc/nixos/configuration.nix",
                "/etc/nixos/configuration.nix.pre-devbox",
            ],
            false,
        )
        .await?;

    let add_import = r#"
if grep -q 'imports' /etc/nixos/configuration.nix; then
  sudo sed -i '/imports\s*=\s*\[/a\    /etc/devbox/devbox-module.nix' /etc/nixos/configuration.nix
else
  sudo sed -i '/^{/a\  imports = [ /etc/devbox/devbox-module.nix ];' /etc/nixos/configuration.nix
fi
"#;

    let result = runtime
        .exec_cmd(name, &["bash", "-c", add_import], false)
        .await?;

    if result.exit_code != 0 {
        eprintln!(
            "Warning: failed to add devbox import: {}",
            result.stderr.trim()
        );
    }

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
        assert!(toml.contains("ai = false"));
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
        assert_eq!(pkgs.len(), 10);
        assert!(pkgs.contains(&"zellij"));
        assert!(pkgs.contains(&"starship"));
        assert!(pkgs.contains(&"yazi"));
    }

    #[test]
    fn nix_packages_tools_set() {
        let pkgs = nix_packages_for_set("tools");
        assert_eq!(pkgs.len(), 21);
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
            "network", "ai", "lang-go", "lang-rust", "lang-python",
            "lang-node", "lang-java", "lang-ruby",
        ];
        for set in &all_sets {
            let pkgs = nix_packages_for_set(set);
            assert!(!pkgs.is_empty(), "set '{set}' should have packages");
        }
    }
}
