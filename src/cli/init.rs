use std::env;

use anyhow::{Result, Context, bail};
use clap::Args;
use colored::Colorize;

use crate::sandbox::SandboxManager;
use crate::tools::detect::detect_languages;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Force overwrite existing devbox.toml
    #[arg(long, short)]
    pub force: bool,
}

pub async fn run(args: InitArgs, manager: &SandboxManager) -> Result<()> {
    let cwd = env::current_dir().context("Cannot determine current directory")?;
    let config_path = cwd.join("devbox.toml");

    if config_path.exists() && !args.force {
        bail!(
            "devbox.toml already exists. Use --force to overwrite."
        );
    }

    let config = manager.generate_config(&cwd);
    let detected = detect_languages(&cwd);

    // Generate with comments for documentation
    let content = generate_commented_toml(&config, &detected);
    std::fs::write(&config_path, content)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    println!("{} {}", "Created".green().bold(), config_path.display());

    // Show detected languages
    let langs = detected.as_set_names();
    if langs.is_empty() {
        println!("  No project languages detected (core sets only)");
    } else {
        println!(
            "  Detected: {}",
            langs
                .iter()
                .map(|l| l.strip_prefix("lang-").unwrap_or(l))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(())
}

fn generate_commented_toml(
    config: &crate::sandbox::config::DevboxConfig,
    detected: &crate::tools::detect::DetectedLanguages,
) -> String {
    let mut s = String::new();
    s.push_str("# devbox.toml\n\n");

    // [sandbox]
    s.push_str("[sandbox]\n");
    s.push_str(&format!(
        "runtime = \"{}\"           # \"auto\" | \"incus\" | \"lima\" | \"multipass\" | \"docker\"\n",
        config.sandbox.runtime
    ));
    s.push_str(&format!(
        "layout = \"{}\"         # Default zellij layout\n",
        config.sandbox.layout
    ));
    s.push_str(&format!(
        "mount_mode = \"{}\"     # \"overlay\" = safe (host read-only) | \"writable\" = direct\n",
        config.sandbox.mount_mode
    ));
    s.push('\n');

    // [sets]
    s.push_str("[sets]\n");
    s.push_str("# Toggle which Nix sets are active\n");
    s.push_str(&format!("system = true              # Locked: always true\n"));
    s.push_str(&format!("shell = true               # Locked: always true\n"));
    s.push_str(&format!("tools = true               # Locked: always true\n"));
    s.push_str(&format!("editor = {}\n", config.sets.editor));
    s.push_str(&format!("git = {}\n", config.sets.git));
    s.push_str(&format!("container = {}\n", config.sets.container));
    s.push_str(&format!("network = {}\n", config.sets.network));
    s.push_str(&format!("ai_code = {}\n", config.sets.ai_code));
    s.push_str(&format!("ai_infra = {}\n", config.sets.ai_infra));
    s.push('\n');

    // [languages]
    s.push_str("[languages]\n");
    s.push_str("# Auto-detected from project files, or explicit\n");
    let go_comment = if detected.go { "  # Detected: go.mod" } else { "" };
    let rust_comment = if detected.rust { "  # Detected: Cargo.toml" } else { "" };
    let python_comment = if detected.python { "  # Detected: pyproject.toml/setup.py/requirements.txt" } else { "" };
    let node_comment = if detected.node { "  # Detected: package.json" } else { "" };
    let java_comment = if detected.java { "  # Detected: pom.xml/build.gradle" } else { "" };
    let ruby_comment = if detected.ruby { "  # Detected: Gemfile" } else { "" };
    s.push_str(&format!("go = {}{}\n", config.languages.go, go_comment));
    s.push_str(&format!("rust = {}{}\n", config.languages.rust, rust_comment));
    s.push_str(&format!("python = {}{}\n", config.languages.python, python_comment));
    s.push_str(&format!("node = {}{}\n", config.languages.node, node_comment));
    s.push_str(&format!("java = {}{}\n", config.languages.java, java_comment));
    s.push_str(&format!("ruby = {}{}\n", config.languages.ruby, ruby_comment));
    s.push('\n');

    // [mounts]
    s.push_str("[mounts.workspace]\n");
    s.push_str("host = \".\"\n");
    s.push_str("target = \"/workspace\"\n");
    s.push_str("readonly = false\n");
    s.push('\n');

    // [resources]
    s.push_str("[resources]\n");
    s.push_str("cpu = 0                    # 0 = unlimited\n");
    s.push_str("memory = \"\"                # \"\" = unlimited\n");
    s.push('\n');

    // [env]
    s.push_str("[env]\n");
    s.push_str("# Inherit from host (true) or set explicit value\n");
    s.push_str("# ANTHROPIC_API_KEY = true\n");
    s.push_str("# NODE_ENV = \"development\"\n");
    s.push('\n');

    // [custom_packages]
    s.push_str("[custom_packages]\n");
    s.push_str("# Additional Nix packages beyond sets\n");
    s.push_str("# terraform = \"nixpkgs#terraform\"\n");
    s.push_str("# my-tool = \"github:user/flake#pkg\"\n");

    s
}
