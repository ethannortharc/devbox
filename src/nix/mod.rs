pub mod rebuild;
pub mod sets;

use std::collections::HashMap;

use anyhow::Result;

use crate::runtime::Runtime;
use crate::sandbox::config::DevboxConfig;
use self::rebuild::{nixos_rebuild, write_state_toml, write_nix_file};
use self::sets::{generate_state_toml, generate_set_nix, generate_sets_default_nix, NIX_SETS};

/// Push the full Nix set configuration to a sandbox VM and rebuild.
///
/// Flow:
/// 1. Generate devbox-state.toml from config
/// 2. Write all set .nix files to /etc/devbox/sets/
/// 3. Write devbox-state.toml to /etc/devbox/
/// 4. Run `nixos-rebuild switch`
pub async fn apply_config(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    config: &DevboxConfig,
) -> Result<()> {
    // Build sets/languages maps from config
    let sets = sets_map(config);
    let languages = languages_map(config);

    // Generate and write state TOML
    let state_toml = generate_state_toml(&sets, &languages, &HashMap::new());
    write_state_toml(runtime, sandbox_name, &state_toml).await?;

    // Write all set Nix files
    write_nix_file(
        runtime,
        sandbox_name,
        "default.nix",
        &generate_sets_default_nix(),
    )
    .await?;

    for set in NIX_SETS {
        let content = generate_set_nix(set);
        let filename = format!("{}.nix", set.name);
        write_nix_file(runtime, sandbox_name, &filename, &content).await?;
    }

    // Rebuild
    nixos_rebuild(runtime, sandbox_name).await?;

    Ok(())
}

/// Toggle additional sets/languages on a running sandbox, then rebuild.
pub async fn upgrade_sets(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    config: &mut DevboxConfig,
    tools: &[String],
) -> Result<()> {
    config.apply_tools(tools);
    apply_config(runtime, sandbox_name, config).await
}

/// Add a custom Nix package (from nixpkgs or flake ref) to the sandbox.
pub async fn add_package(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    package: &str,
) -> Result<()> {
    if package.contains(':') || package.contains('#') {
        // Flake reference: github:user/repo#pkg or nixpkgs#pkg
        println!("Adding flake package: {package}");
        let result = runtime
            .exec_cmd(
                sandbox_name,
                &["sudo", "nix", "profile", "install", package],
                false,
            )
            .await?;
        if result.exit_code != 0 {
            anyhow::bail!("Failed to add package: {}", result.stderr.trim());
        }
    } else {
        // Simple nixpkgs package name
        println!("Adding nixpkgs package: {package}");
        let flake_ref = format!("nixpkgs#{package}");
        let result = runtime
            .exec_cmd(
                sandbox_name,
                &["sudo", "nix", "profile", "install", &flake_ref],
                false,
            )
            .await?;
        if result.exit_code != 0 {
            anyhow::bail!("Failed to add package: {}", result.stderr.trim());
        }
    }

    println!("Package '{package}' installed successfully.");
    Ok(())
}

/// Remove a custom Nix package from the sandbox.
pub async fn remove_package(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    package: &str,
) -> Result<()> {
    println!("Removing package: {package}");
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["sudo", "nix", "profile", "remove", package],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        anyhow::bail!("Failed to remove package: {}", result.stderr.trim());
    }

    println!("Package '{package}' removed.");
    Ok(())
}

fn sets_map(config: &DevboxConfig) -> HashMap<String, bool> {
    let mut m = HashMap::new();
    m.insert("system".to_string(), config.sets.system);
    m.insert("shell".to_string(), config.sets.shell);
    m.insert("tools".to_string(), config.sets.tools);
    m.insert("editor".to_string(), config.sets.editor);
    m.insert("git".to_string(), config.sets.git);
    m.insert("container".to_string(), config.sets.container);
    m.insert("network".to_string(), config.sets.network);
    m.insert("ai".to_string(), config.sets.ai);
    m
}

fn languages_map(config: &DevboxConfig) -> HashMap<String, bool> {
    let mut m = HashMap::new();
    m.insert("go".to_string(), config.languages.go);
    m.insert("rust".to_string(), config.languages.rust);
    m.insert("python".to_string(), config.languages.python);
    m.insert("node".to_string(), config.languages.node);
    m.insert("java".to_string(), config.languages.java);
    m.insert("ruby".to_string(), config.languages.ruby);
    m
}

