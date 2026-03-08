//! Post-create provisioning for NixOS VMs.
//!
//! After the VM boots with a stock NixOS Lima image, this module:
//! 1. Pushes devbox nix configuration files into the VM
//! 2. Generates devbox-state.toml with active sets/languages
//! 3. Runs `nixos-rebuild switch` to install all declared packages
//! 4. Copies the devbox binary and help files into the VM

use anyhow::Result;

use crate::runtime::Runtime;

// ── Embedded Nix files ──────────────────────────────────────
// These are compiled into the binary so provisioning works without
// needing the nix/ directory at runtime.

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

/// Provision a NixOS VM with tools based on active sets and languages.
pub async fn provision_vm(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
) -> Result<()> {
    let username = whoami();

    // 1. Create directory structure inside the VM
    println!("Setting up NixOS configuration...");
    runtime
        .exec_cmd(
            name,
            &["sudo", "mkdir", "-p", "/etc/devbox/sets", "/etc/devbox/help"],
            false,
        )
        .await?;

    // 2. Generate and push devbox-state.toml
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
        // Show last few lines of stderr for debugging
        let stderr_lines: Vec<&str> = result.stderr.lines().collect();
        let start = stderr_lines.len().saturating_sub(20);
        for line in &stderr_lines[start..] {
            eprintln!("  {line}");
        }
        eprintln!("Tools may be incomplete. You can retry with `devbox exec --name {name} -- sudo nixos-rebuild switch`");
    } else {
        println!("NixOS rebuild complete.");
    }

    // 7. Copy devbox binary into the VM
    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;

    // 8. Write help files into the VM
    setup_help_in_vm(runtime, name).await?;

    Ok(())
}

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
/// This avoids shell escaping issues with single quotes, newlines, etc.
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
/// Backs up the original config and creates a wrapper that imports both.
async fn add_devbox_import(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // Check if already imported
    let check = runtime
        .exec_cmd(
            name,
            &["grep", "-q", "devbox-module", "/etc/nixos/configuration.nix"],
            false,
        )
        .await?;

    if check.exit_code == 0 {
        // Already imported, skip
        return Ok(());
    }

    // Backup original configuration.nix
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

    // Add import line after the first `imports = [` or create one
    // Strategy: use sed to add our import to the imports list
    let add_import = r#"
if grep -q 'imports' /etc/nixos/configuration.nix; then
  # Add to existing imports list
  sudo sed -i '/imports\s*=\s*\[/a\    /etc/devbox/devbox-module.nix' /etc/nixos/configuration.nix
else
  # Add imports list after the opening brace of the module
  sudo sed -i '/^{/a\  imports = [ /etc/devbox/devbox-module.nix ];' /etc/nixos/configuration.nix
fi
"#;

    let result = runtime
        .exec_cmd(name, &["bash", "-c", add_import], false)
        .await?;

    if result.exit_code != 0 {
        eprintln!(
            "Warning: failed to add devbox import to configuration.nix: {}",
            result.stderr.trim()
        );
    }

    Ok(())
}

/// Copy the current devbox binary into the VM at /usr/local/bin/devbox.
async fn copy_devbox_to_vm(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();

    // Lima supports limactl copy for file transfer
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

        // lang-rust in sets should enable rust in languages section
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
}
