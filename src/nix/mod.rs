pub mod compose;
pub mod rebuild;
pub mod sets;

use std::collections::HashMap;

use anyhow::Result;

use self::rebuild::{nixos_rebuild, write_devbox_nix, write_nix_file, write_state_toml};
use self::sets::{NIX_SETS, generate_set_nix, generate_sets_default_nix, generate_state_toml};
use crate::runtime::Runtime;
use crate::sandbox::config::DevboxConfig;

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

/// Sets whose module is checked in rather than generated.
///
/// Their packages are optional by nature, so the module guards each one with
/// `tryEval`. A generated flat list has no such guard.
const GUARDED_SETS: &[&str] = &["ai-code", "ai-infra"];

/// The checked-in module source for a guarded set.
///
/// Embedded at compile time so the binary carries it: these are pushed into a
/// box that has no copy of the repository.
fn embedded_set_nix(name: &str) -> Option<&'static str> {
    match name {
        "ai-code" => Some(include_str!("../../nix/sets/ai-code.nix")),
        "ai-infra" => Some(include_str!("../../nix/sets/ai-infra.nix")),
        _ => None,
    }
}

/// Push the Nix set modules and the composed `configuration.nix` for a
/// selection, without rebuilding.
///
/// Split out from [`apply_config`] so the web console can do the fast,
/// quiet part itself and then stream the slow `nixos-rebuild` (§6.3).
pub async fn write_set_modules(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    selection: &compose::Selection,
) -> Result<()> {
    // Read back what the box already declares, so regenerating the state file
    // does not reset the guest username or the mount mode (both of which the
    // NixOS module reads from it).
    let existing = read_state_toml(runtime, sandbox_name).await;
    let username = existing
        .as_ref()
        .and_then(|t| toml_string(t, "user", "name"));
    let mount_mode = existing
        .as_ref()
        .and_then(|t| toml_string(t, "sandbox", "mount_mode"));

    // The set index and every set module are pushed regardless of selection:
    // they are small text files, and having them all present means toggling a
    // set on later needs no extra round trip. What the selection controls is
    // `configuration.nix`, which imports only the chosen ones — so only those
    // closures are ever evaluated or built.
    write_nix_file(
        runtime,
        sandbox_name,
        "default.nix",
        &generate_sets_default_nix(),
    )
    .await?;

    for set in NIX_SETS {
        // The AI sets ship as checked-in modules that wrap each optional tool
        // in `tryEval`, because some of them are absent or broken on a given
        // nixpkgs channel. Regenerating them as flat package lists throws that
        // away, so one unavailable optional tool fails the entire rebuild —
        // for the set that is on by default.
        if GUARDED_SETS.contains(&set.name) {
            write_nix_file(
                runtime,
                sandbox_name,
                &format!("{}.nix", set.name),
                embedded_set_nix(set.name).unwrap_or(&generate_set_nix(set)),
            )
            .await?;
            continue;
        }
        write_nix_file(
            runtime,
            sandbox_name,
            &format!("{}.nix", set.name),
            &generate_set_nix(set),
        )
        .await?;
    }

    write_devbox_nix(
        runtime,
        sandbox_name,
        &compose::compose_configuration_nix(selection),
    )
    .await?;

    // `devbox.nix` is the readable record of the selection, but the NixOS
    // module the box already imports reads `devbox-state.toml`. Writing only
    // the first would let `sets apply` report success while the installed
    // closure never changed — so both are written, from the same selection.
    let config = selection.to_config(&DevboxConfig::default());
    let mut extra = HashMap::new();
    for pkg in &selection.packages {
        extra.insert(pkg.clone(), "nixpkgs".to_string());
    }
    let state_toml = crate::nix::sets::generate_state_toml_with(
        &sets_map(&config),
        &languages_map(&config),
        &extra,
        username.as_deref(),
        mount_mode.as_deref(),
    );
    write_state_toml(runtime, sandbox_name, &state_toml).await
}

/// Read the box's current `devbox-state.toml`, if it has one.
async fn read_state_toml(runtime: &dyn Runtime, sandbox_name: &str) -> Option<toml::Value> {
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["cat", "/etc/devbox/devbox-state.toml"],
            false,
        )
        .await
        .ok()?;
    if result.exit_code != 0 {
        return None;
    }
    toml::from_str(&result.stdout).ok()
}

/// Pull `table.key` out of a parsed TOML document.
fn toml_string(doc: &toml::Value, table: &str, key: &str) -> Option<String> {
    doc.get(table)?.get(key)?.as_str().map(str::to_string)
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
pub async fn add_package(runtime: &dyn Runtime, sandbox_name: &str, package: &str) -> Result<()> {
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
    m.insert("ai_code".to_string(), config.sets.ai_code);
    m.insert("ai_infra".to_string(), config.sets.ai_infra);
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
