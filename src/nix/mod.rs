pub mod compose;
pub mod rebuild;
pub mod sets;

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use self::rebuild::{nixos_rebuild, write_devbox_nix, write_nix_file, write_state_toml};
use self::sets::generate_state_toml;
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

    // Generate and write state TOML, carrying the config's own packages.
    //
    // An empty map here silently dropped every ad-hoc package on any path
    // through `apply_config` — notably `devbox upgrade`, which rebuilt the box
    // without them while state.json went on reporting them as installed. The
    // caller has the config; there was never a reason to discard this part
    // of it.
    let state_toml = generate_state_toml(&sets, &languages, &config.custom_packages);
    write_state_toml(runtime, sandbox_name, &state_toml).await?;

    // Write all set Nix files — the checked-in modules, for the reason
    // `write_set_modules` gives. This path had the same defect and no
    // exemption for the AI sets, so `devbox upgrade` replaced their `tryEval`
    // guards with a flat list: one tool missing from the channel then failed
    // the rebuild of a box whose selection the user had not touched.
    for (filename, content) in sets::NIX_SET_FILES {
        write_nix_file(runtime, sandbox_name, filename, content).await?;
    }

    // Rebuild
    nixos_rebuild(runtime, sandbox_name).await?;

    Ok(())
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
    let existing = read_state_toml(runtime, sandbox_name).await?;
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
    // The checked-in modules, verbatim, index included — and no generated
    // fallback. A set is a Nix expression and only some of them are a flat
    // list of names: the AI sets wrap each optional tool in `tryEval` so one
    // tool missing from a channel does not fail the whole rebuild, and
    // `network` builds a derivation to put FRR's daemons on PATH.
    // Regenerating from the package index dropped whatever did not survive
    // that round trip — and dropped it only on `sets apply`, so a box was
    // correct when created and quietly lost those packages the first time its
    // selection was touched. A fallback would reopen the same hole one
    // missing table entry at a time; `every_set_ships_exactly_one_module`
    // pins the table against the catalog instead.
    for (filename, content) in sets::NIX_SET_FILES {
        write_nix_file(runtime, sandbox_name, filename, content).await?;
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
    // Names and sources; `generate_state_toml_with` resolves them to the
    // attributes the module looks up.
    let extra: HashMap<String, String> = selection.declared_sources().into_iter().collect();
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
/// Read the box's own declared state, distinguishing "not there" from "could
/// not be read".
///
/// `Ok(None)` means the file is genuinely absent, which is the first-provision
/// case and the only one where omitting these keys is correct.
///
/// Everything else is an error, because the caller's *purpose* is to preserve
/// the guest username and mount mode across a regeneration — and a value it
/// cannot read is written out as absent, which makes the NixOS module fall back
/// to its defaults. A transport hiccup or a corrupt file would therefore reset
/// precisely what this read exists to protect, silently, during an unrelated
/// Sets change.
async fn read_state_toml(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Option<toml::Value>> {
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["cat", "/etc/devbox/devbox-state.toml"],
            false,
        )
        .await
        .with_context(|| format!("could not read the declared state of box '{sandbox_name}'"))?;
    if result.exit_code != 0 {
        // Absent on a first provision, and non-zero for unreadable too — so
        // ask which it was rather than guessing the harmless answer.
        let probe = runtime
            .exec_cmd(
                sandbox_name,
                &["test", "-e", "/etc/devbox/devbox-state.toml"],
                false,
            )
            .await
            .with_context(|| {
                format!("could not check for the declared state of box '{sandbox_name}'")
            })?;
        if probe.exit_code != 0 {
            return Ok(None); // genuinely not there yet
        }
        bail!(
            "box '{sandbox_name}' has a devbox-state.toml that cannot be read, so \
             regenerating it would drop the guest username and mount mode it \
             records. Refusing rather than resetting them."
        );
    }
    let parsed = toml::from_str(&result.stdout).with_context(|| {
        format!(
            "box '{sandbox_name}' has a devbox-state.toml that cannot be parsed, so \
             regenerating it would drop the guest username and mount mode it records"
        )
    })?;
    Ok(Some(parsed))
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
                &["bash", "-lc", &format!("nix profile install {package}")],
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
                &["bash", "-lc", &format!("nix profile install {flake_ref}")],
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
            &["bash", "-lc", &format!("nix profile remove {package}")],
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
