//! Post-create provisioning for VMs.
//!
//! Supports two image types:
//! - **NixOS**: push nix config files + `nixos-rebuild switch`
//! - **Ubuntu**: install Nix package manager + `nix profile install`
//!
//! Both paths use the same package definitions from nix/sets/*.nix.

use anyhow::{Context, Result, bail};

use crate::runtime::{ExecResult, Runtime};

/// Browser provisioning reports long-running command output into the box's
/// SSE build panel. CLI provisioning passes no reporter and keeps inheriting
/// the caller's terminal, preserving the familiar progress display there.
pub type ProvisionReporter<'a> = &'a (dyn Fn(&str) + Sync);

// ── Embedded Nix files (for NixOS provisioning) ─────────────

pub(crate) const NIX_DEVBOX_MODULE: &str = include_str!("../../nix/devbox-module.nix");
const NIX_OBSD_MODULE: &str = include_str!("../../nix/obsd-module.nix");

// ── Embedded config files (yazi, etc.) ───────────────────
const YAZI_CONFIG: &str = include_str!("../../configs/yazi/yazi.toml");
const YAZI_KEYMAP: &str = include_str!("../../configs/yazi/keymap.toml");
const YAZI_THEME: &str = include_str!("../../configs/yazi/theme.toml");
const YAZI_INIT: &str = include_str!("../../configs/yazi/init.lua");
const YAZI_GLOW_PLUGIN: &str = include_str!("../../configs/yazi/plugin/glow.yazi/init.lua");
const AICHAT_ROLES: &str = include_str!("../../configs/aichat/roles.yaml");
const AICHAT_ROLE_ARCHITECT: &str = include_str!("../../configs/aichat/roles/architect.md");
const AICHAT_ROLE_REVIEWER: &str = include_str!("../../configs/aichat/roles/reviewer.md");
const MANAGEMENT_SCRIPT: &str = include_str!("../../configs/management.sh");

// ── Package name mapping (for Ubuntu/Nix profile install) ───
// These map set names to nixpkgs attribute paths for `nix profile install`.
// The names match the nix/sets/*.nix files exactly.

/// The flake reference to install for a configured package.
///
/// The value in `[custom_packages]` says where the package comes from, and it
/// takes three shapes in the wild:
///
/// * `"nixpkgs"` — the bare attribute name in nixpkgs, the common case;
/// * `"nixpkgs#terraform"` — already a complete reference;
/// * `"github:user/flake#pkg"` — some other flake.
///
/// Only the first needs the name attached; treating the other two as bare
/// attribute names installed something else, or nothing.
/// Is this a complete installable reference that is safe to put in a shell?
///
/// Deliberately narrower than what nix accepts. Everything devbox generates
/// satisfies it, a hand-written `devbox.toml` entry that does not is a typo or
/// an attack, and the value ends up in a root command either way.
/// Can this image install every configured package?
///
/// Called before a box is created as well as during provisioning: by the time
/// provisioning refuses, the box exists and the caller goes on to save state
/// and report success, so the user gets a sandbox that reports a package it
/// does not have.
/// The attribute path the NixOS module will resolve for a declared package.
///
/// `my-tf = "nixpkgs#terraform"` names the attribute in its *source*; the key
/// is only what the user calls it. Handing the module the key made
/// `lib.attrByPath` resolve to null, and null is filtered out — silently, so
/// that one stale name cannot fail a whole rebuild. The package was therefore
/// accepted by the source check, recorded in state as selected, shown as
/// selected in the checklist, and never installed.
///
/// Deliberately shared by the check and the projection. They worked this out
/// separately before, and validating one string while writing a different one
/// is not validation.
pub(crate) fn nixos_attr_path<'a>(name: &'a str, source: &'a str) -> &'a str {
    source.strip_prefix("nixpkgs#").unwrap_or(name)
}

pub fn check_packages_supported(image: &str, packages: &[(String, String)]) -> Result<()> {
    if image == "ubuntu" {
        // `nix profile install` takes a flake reference directly, so any
        // *source* is supported here — but the reference still has to be one
        // the installer will accept. `provision_ubuntu` checks that, after the
        // box exists, where the failure is downgraded to a warning and state
        // is saved anyway: `devbox create` reported a box it had made, with a
        // package it had not installed and no error the user would see.
        //
        // Round 24 moved this whole check ahead of `runtime.create` for the
        // NixOS path and left the Ubuntu path returning `Ok` on the way past.
        // The reference `nix profile install` is actually given, which is
        // `installable(name, source)` and not the source alone.
        //
        // Checking the source was wrong in both directions. A key like
        // `bad;name` with source `nixpkgs` passed, because `nixpkgs` is a fine
        // source — and then provisioning built `nixpkgs#bad;name` and rejected
        // it after the box existed. Meanwhile `tool = "github:owner/repo"` was
        // refused here, because a bare flake URL is not an attribute path,
        // even though provisioning would have appended the key and installed
        // it happily.
        //
        // This is the sentence I wrote on `nixos_attr_path` in round 24 —
        // validating one string while writing a different one is not
        // validation — repeated in the branch I added five rounds later.
        let malformed: Vec<&str> = packages
            .iter()
            .filter(|(name, source)| !is_safe_installable(&installable(name, source)))
            .map(|(pkg, _)| pkg.as_str())
            .collect();
        if !malformed.is_empty() {
            bail!(
                "these packages do not name an installable nix reference: {}\n  \
                 Expected `nixpkgs#name`, `github:owner/repo#name`, or a bare \
                 attribute path.",
                malformed.join(", ")
            );
        }
        return Ok(());
    }
    let unsupported: Vec<&str> = packages
        .iter()
        .filter(|(_, source)| source != "nixpkgs" && !source.starts_with("nixpkgs#"))
        .map(|(pkg, _)| pkg.as_str())
        .collect();
    if !unsupported.is_empty() {
        bail!(
            "these packages come from a flake, which the NixOS image cannot install \
             yet: {}\n  \
             Point them at nixpkgs in devbox.toml, or use the ubuntu image \
             (`image = \"ubuntu\"`), which installs flake references directly.",
            unsupported.join(", ")
        );
    }
    // The attribute path is written into the box's `devbox-state.toml` and
    // resolved under `pkgs`. The web path has validated it since round 12; the
    // `create` path reached `generate_state_toml` without ever checking, so
    // this is the same guard arriving at the second entry point to the same
    // file — the sibling-surface question, asked for once.
    let invalid: Vec<&str> = packages
        .iter()
        .filter(|(name, source)| {
            !crate::nix::compose::is_valid_attr_path(nixos_attr_path(name, source))
        })
        .map(|(pkg, _)| pkg.as_str())
        .collect();
    if !invalid.is_empty() {
        bail!(
            "these packages do not name a valid nixpkgs attribute path: {}",
            invalid.join(", ")
        );
    }

    // And no two of them may resolve to the same attribute.
    //
    // `Selection::validate` has refused that since round 29, which is exactly
    // the problem: creation did not, so a box could be *made* carrying
    // `terraform = "nixpkgs"` beside `my-tf = "nixpkgs#terraform"`. The guest
    // table deduplicates, both names persist in state, and then every Sets
    // operation on that box fails validation — a box created successfully and
    // unmanageable from the moment it existed, repairable only by hand-editing
    // its configuration.
    //
    // Same rule, both entry points, phrased the same way.
    let mut by_attr: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (name, source) in packages {
        let attr = nixos_attr_path(name, source);
        if let Some(first) = by_attr.insert(attr, name)
            && first != name
        {
            bail!(
                "'{first}' and '{name}' both resolve to the nixpkgs attribute \
                 '{attr}'; drop one of them"
            );
        }
    }
    Ok(())
}

pub(crate) fn is_safe_installable(reference: &str) -> bool {
    if reference.is_empty() || reference.len() > 256 {
        return false;
    }
    // No shell metacharacters at all, and no whitespace.
    if reference.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                ';' | '&' | '|' | '$' | '`' | '(' | ')' | '<' | '>' | '\'' | '"' | '\\' | '\n'
            )
    }) {
        return false;
    }
    // `flake-ref#attr.path` or a bare attribute path. Both halves are checked;
    // the fragment with the existing attribute rule, the flake part with the
    // characters a flake URL legitimately needs.
    let (flake, attr) = match reference.split_once('#') {
        Some((flake, attr)) => (Some(flake), attr),
        None => (None, reference),
    };
    if !crate::nix::compose::is_valid_attr_path(attr) {
        return false;
    }
    match flake {
        None => true,
        Some(flake) => {
            !flake.is_empty()
                && flake.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, ':' | '/' | '.' | '-' | '_' | '+' | '?' | '=' | '&')
                })
        }
    }
}

/// A box's packages paired with the source its project config declares.
///
/// `state.packages` records only the names — enough for the checklist, not
/// enough to install. `reprovision` and `use` passed those bare names to
/// provisioning, so the first lifecycle operation after adding a flake package
/// replaced it with a same-named nixpkgs attribute, or dropped it, while the
/// UI went on reporting it selected.
pub fn package_pairs(state: &crate::sandbox::state::SandboxState) -> Vec<(String, String)> {
    resolved_packages(state).0
}

/// A box's packages, recovered for a box that predates `state.packages`.
///
/// The list itself has to come through the selection, not off `state.packages`.
/// This function used to iterate that field directly and fall back to the
/// project config only for a package's *source* — which reads as a legacy
/// fallback and is not one: a v3 box has no entries at all, so there was
/// nothing to find sources for and the whole list came back empty.
///
/// `reprovision` and `devbox use` both rebuild from this, so both silently
/// dropped every custom package such a box had. That was survivable while the
/// absence stayed legible; once `save` began stamping `schema`, each of them
/// wrote "this file is current and has no packages" over a box whose packages
/// they had just discarded, and nothing could recover them afterwards.
///
/// Returned as a pair so the callers that *persist* get the same answer as the
/// callers that build. The upgrade path needed both and only did one, which is
/// the whole shape of this defect.
pub fn resolved_packages(
    state: &crate::sandbox::state::SandboxState,
) -> (Vec<(String, String)>, crate::nix::compose::Selection) {
    let config = crate::sandbox::config::DevboxConfig::load_or_default(&state.project_dir);
    let selection = crate::nix::compose::Selection::from_state_and_project(state, &config);
    let pairs = selection
        .packages
        .iter()
        .map(|name| {
            // What the box recorded wins over what the current directory
            // declares: after `devbox use` the two are different projects, and
            // the box's own record is the one that describes the box. The
            // selection has already applied that precedence.
            let source = selection
                .sources
                .get(name)
                .cloned()
                .unwrap_or_else(|| "nixpkgs".to_string());
            (name.clone(), source)
        })
        .collect();
    (pairs, selection)
}

pub(crate) fn installable(name: &str, source: &str) -> String {
    if source == "nixpkgs" || source.is_empty() {
        format!("nixpkgs#{name}")
    } else if source.contains('#') {
        source.to_string()
    } else {
        format!("{source}#{name}")
    }
}

pub(crate) fn nix_packages_for_set(set: &str) -> Vec<&'static str> {
    match set {
        // Kept in step with `NIX_SETS`; the test below asserts it. The Ubuntu
        // path uses this mapping rather than the catalog, so a package added
        // there and forgotten here means an Ubuntu box is reported provisioned
        // without the tool a later command shells out to.
        "system" => vec![
            "nftables",
            "conntrack-tools",
            "coreutils",
            "gnugrep",
            "gnused",
            "gawk",
            "findutils",
            "diffutils",
            "gzip",
            "gnutar",
            "xz",
            "bzip2",
            "file",
            "which",
            "tree",
            "less",
            "curl",
            "wget",
            "openssh",
            "openssl",
            "cacert",
            "gnupg",
            "gcc",
            "gnumake",
            "pkg-config",
            "man-db",
        ],
        "shell" => vec![
            "zsh",
            "zsh-autosuggestions",
            "zsh-syntax-highlighting",
            "starship",
            "fzf",
            "zoxide",
            "direnv",
            "nix-direnv",
            "yazi",
            "micro",
        ],
        "tools" => vec![
            "ripgrep",
            "fd",
            "bat",
            "eza",
            "delta",
            "sd",
            "choose",
            "jq",
            "yq-go",
            "fx",
            "htop",
            "bottom",
            "procs",
            "dust",
            "duf",
            "tokei",
            "hyperfine",
            "tealdeer",
            "httpie",
            "dog",
            "glow",
            "entr",
        ],
        "editor" => vec!["neovim", "helix", "nano"],
        "git" => vec!["git", "lazygit", "gh", "git-lfs", "git-crypt", "pre-commit"],
        "container" => vec![
            "docker",
            "docker-compose",
            "lazydocker",
            "dive",
            "buildkit",
            "skopeo",
        ],
        "network" => vec![
            "frr",
            "dnsmasq",
            "chrony",
            "busybox",
            "iproute2",
            "conntrack-tools",
            "tailscale",
            "mosh",
            "nmap",
            "tcpdump",
            "bandwhich",
            "trippy",
            "doggo",
        ],
        // claude-code is installed separately via npm (latest version, smaller footprint)
        "ai-code" => vec!["codex", "opencode", "aider-chat", "aichat", "continue"],
        "ai-infra" => vec![
            "ollama",
            "open-webui",
            "litellm",
            "mcp-hub",
            "python312Packages.huggingface-hub",
        ],
        "lang-go" => vec!["go", "gopls", "golangci-lint", "delve", "gotools", "gore"],
        "lang-rust" => vec![
            "rustup",
            "rust-analyzer",
            "cargo-watch",
            "cargo-edit",
            "cargo-expand",
            "sccache",
        ],
        "lang-python" => vec![
            "python312",
            "uv",
            "ruff",
            "pyright",
            "python312Packages.ipython",
            "python312Packages.pytest",
        ],
        "lang-node" => vec![
            "nodejs_22",
            "bun",
            "pnpm",
            "typescript",
            "nodePackages.typescript-language-server",
            "biome",
        ],
        "lang-java" => vec!["jdk21", "gradle", "maven", "jdt-language-server"],
        "lang-ruby" => vec!["ruby_3_3", "bundler", "solargraph", "rubocop"],
        _ => vec![],
    }
}

// ── Cache Key ───────────────────────────────────────────────

/// Hash of all embedded nix configuration files.
/// Changes when any nix set file or module is updated → automatic cache invalidation.
fn config_version() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    for (_, content) in crate::nix::sets::NIX_SET_FILES {
        content.hash(&mut hasher);
    }
    NIX_DEVBOX_MODULE.hash(&mut hasher);
    NIX_OBSD_MODULE.hash(&mut hasher);
    hasher.finish()
}

/// Compute a deterministic cache key for one provisioning outcome.
///
/// Everything provisioning bakes into the guest is an input: the image, the
/// mount mode, the sets, the languages, the ad-hoc packages *with their
/// sources* (a flake-sourced package installs something different from the
/// nixpkgs attribute of the same name), and the embedded Nix configuration via
/// `config_version`. Same inputs, same key; any change invalidates the cache.
pub fn cache_key(
    image: &str,
    sets: &[String],
    languages: &[String],
    mount_mode: &str,
    packages: &[(String, String)],
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    image.hash(&mut hasher);
    mount_mode.hash(&mut hasher);
    let mut sorted_sets: Vec<&String> = sets.iter().collect();
    sorted_sets.sort();
    sorted_sets.hash(&mut hasher);
    let mut sorted_langs: Vec<&String> = languages.iter().collect();
    sorted_langs.sort();
    sorted_langs.hash(&mut hasher);
    let mut sorted_packages: Vec<&(String, String)> = packages.iter().collect();
    sorted_packages.sort();
    sorted_packages.hash(&mut hasher);
    config_version().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ── Public API ──────────────────────────────────────────────

/// Provision a VM with tools based on active sets and languages.
/// Dispatches to NixOS or Ubuntu provisioning based on image type.
#[allow(dead_code)]
pub async fn provision_vm(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
) -> Result<()> {
    provision_vm_with_mode(runtime, name, sets, languages, image, "overlay").await
}

pub async fn provision_vm_with_mode(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
    mount_mode: &str,
) -> Result<()> {
    provision_vm_full(runtime, name, sets, languages, image, mount_mode, &[]).await
}

/// Provision, carrying the box's ad-hoc packages through.
///
/// The three-argument form drops them, so creating a box with
/// `custom_packages` — or reprovisioning one that gained a package through the
/// Sets tab — wrote a guest state file without them while the host state kept
/// reporting them as selected. The two then disagreed until the next Sets
/// apply, with the console showing the host's version.
pub async fn provision_vm_full(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
    mount_mode: &str,
    packages: &[(String, String)],
) -> Result<()> {
    provision_vm_full_reported(
        runtime, name, sets, languages, image, mount_mode, packages, None,
    )
    .await
}

/// Provision with an optional progress sink owned by the caller.
///
/// Supplying a reporter is the web-console mode: long-running guest commands
/// run with piped stdio and publish each line rather than inheriting (and
/// potentially taking over) the terminal that launched `devbox web`.
#[allow(clippy::too_many_arguments)]
pub async fn provision_vm_full_reported(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
    mount_mode: &str,
    packages: &[(String, String)],
    reporter: Option<ProvisionReporter<'_>>,
) -> Result<()> {
    match image {
        "ubuntu" => provision_ubuntu(runtime, name, sets, languages, packages, reporter).await,
        // NixOS writes `[custom_packages]` keys, which the module resolves as
        // attribute paths under `pkgs` — so it wants the name, not the
        // installable reference. Ubuntu runs `nix profile install` and wants
        // the reference. Same input, different projection.
        _ => {
            // A flake-sourced package cannot be expressed here, so say so
            // rather than dropping it.
            //
            // The NixOS path writes `[custom_packages]` keys that
            // `devbox-module.nix` resolves with `lib.attrByPath … pkgs`, and a
            // name that is not a nixpkgs attribute resolves to null and is
            // filtered out — silently, because filtering is what keeps one
            // stale name from failing the whole rebuild. So a package like
            // `my-tool = "github:user/flake#pkg"` was recorded as selected,
            // reported as selected, and never installed. Supporting it means
            // teaching the module about flake inputs, which is a real change;
            // until then this refuses, which is the same call as ADR-0036.
            check_packages_supported(image, packages)?;
            let names: Vec<String> = packages
                .iter()
                .map(|(n, s)| nixos_attr_path(n, s).to_string())
                .collect();
            provision_nixos(runtime, name, sets, languages, mount_mode, &names, reporter).await
        }
    }
}

/// Run a long install step according to the caller's output policy.
async fn run_install_step(
    runtime: &dyn Runtime,
    name: &str,
    cmd: &[&str],
    reporter: Option<ProvisionReporter<'_>>,
) -> Result<ExecResult> {
    if let Some(report) = reporter {
        let argv = runtime.argv(name, cmd, false);
        let exit_code = crate::web::build::stream_command(&argv, |line| report(line)).await?;
        Ok(ExecResult {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
        })
    } else {
        runtime.exec_cmd(name, cmd, true).await
    }
}

/// Wait for the guest to answer an exec again after system activation.
///
/// On Incus, `nixos-rebuild switch` restarts the guest agent and the exec
/// session dies with it; on Lima the SSH session may drop briefly. Either way
/// the next step needs a guest that answers, and "it will probably be back" is
/// not a state provisioning can continue from — so this is bounded and fails.
async fn wait_for_guest_exec(runtime: &dyn Runtime, name: &str) -> Result<()> {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let max_attempts = 40; // 40 * 3s = 120s
    for i in 0..max_attempts {
        if let Ok(r) = runtime.exec_cmd(name, &["echo", "ready"], false).await
            && r.exit_code == 0
            && r.stdout.trim() == "ready"
        {
            return Ok(());
        }
        if i > 0 && i % 10 == 0 {
            println!("  Still waiting for the guest... ({}s)", i * 3);
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    bail!("the guest did not answer within 120 seconds after system activation")
}

fn require_install_success(result: &ExecResult, step: &str, retry: &str) -> Result<()> {
    if result.exit_code == 0 {
        return Ok(());
    }
    let detail = result.stderr.trim();
    if detail.is_empty() {
        bail!(
            "{step} failed with exit code {}. Retry with: {retry}",
            result.exit_code
        );
    }
    bail!(
        "{step} failed with exit code {}: {detail}. Retry with: {retry}",
        result.exit_code
    )
}

/// Lightweight setup after launching from a cached image.
///
/// The cache key already covers everything `nixos-rebuild` produced, so this
/// applies only what is specific to *this* box or *this* host: the state file,
/// the host's git and AI-tool configuration, the devbox binary, and the
/// observability agent — which is version-pinned to the host binary and must
/// match it even when the cached guest was published by an older release.
pub async fn post_cache_setup(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    image: &str,
    mount_mode: &str,
    packages: &[(String, String)],
) -> Result<()> {
    let username = whoami();

    // Cached images may boot with a different NIC name/MAC than the original VM.
    // Restart networking to ensure DHCP picks up an IP on the new interface.
    ensure_network_after_cache(runtime, name).await?;

    // Detect VM user/home (no network needed — just reading /etc/passwd)
    let vm_user = detect_vm_username(runtime, name).await;
    let vm_home = detect_vm_home(runtime, name, &vm_user).await;

    // The same projection provisioning uses (see `provision_vm_full_reported`):
    // NixOS state names attribute paths, Ubuntu names installables.
    let package_names: Vec<String> = if image == "ubuntu" {
        packages.iter().map(|(n, s)| installable(n, s)).collect()
    } else {
        check_packages_supported(image, packages)?;
        packages
            .iter()
            .map(|(n, s)| nixos_attr_path(n, s).to_string())
            .collect()
    };

    // Update state file with current sandbox metadata
    let shape = crate::nix::sets::GuestShape {
        user: Some(username.clone()),
        home: Some(vm_home.clone()),
        runtime: Some(runtime.name().to_string()),
        mount_mode: Some(mount_mode.to_string()),
        workspace_nofail: workspace_nofail_for(runtime, name).await,
    };
    let state_toml = generate_state_toml(sets, languages, &shape, &package_names);
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &state_toml).await?;

    if let Err(error) = install_obsd_binary(runtime, name).await {
        eprintln!("Warning: observability agent was not installed: {error}");
    }

    // Copy current host git config (host-specific, may have changed)
    setup_git_config(runtime, name, &vm_user, &vm_home).await?;

    // Update devbox binary + help files (may have been updated since cache was created)
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;
    setup_management_script(runtime, name).await?;
    setup_ai_tool_configs(runtime, name, &vm_user, &vm_home).await?;

    Ok(())
}

/// Ensure network is up after launching from a cached image.
/// The cached image may have been built with a different NIC name/MAC.
/// We restart DHCP/NetworkManager and wait for an IP address.
async fn ensure_network_after_cache(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // Restart networking services to pick up DHCP on potentially new interfaces
    let _ = run_in_vm(
        runtime,
        name,
        "systemctl restart systemd-networkd 2>/dev/null; \
         systemctl restart NetworkManager 2>/dev/null; \
         systemctl restart dhcpcd 2>/dev/null; \
         true",
        false,
    )
    .await;

    // Wait for an IP address to appear (up to 30s)
    let attempts = 10;
    for i in 0..attempts {
        let result = runtime
            .exec_cmd(
                name,
                &[
                    "bash",
                    "-lc",
                    "ip -4 addr show scope global | grep -q 'inet '",
                ],
                false,
            )
            .await;
        if let Ok(r) = result
            && r.exit_code == 0
        {
            return Ok(());
        }
        if i == 0 {
            print!("Waiting for network...");
        } else {
            print!(".");
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    println!();
    eprintln!("Warning: VM may not have network connectivity. SSH and package installs may fail.");
    Ok(())
}

// ── NixOS Provisioning ─────────────────────────────────────

/// Provision a NixOS VM: push nix config files + nixos-rebuild switch.
#[allow(clippy::too_many_arguments)]
async fn provision_nixos(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    mount_mode: &str,
    packages: &[String],
    reporter: Option<ProvisionReporter<'_>>,
) -> Result<()> {
    let username = whoami();

    // 0. Wait for network connectivity (DNS resolution can lag behind the agent)
    wait_for_network(runtime, name).await?;

    // 1. Create directory structure
    println!("Setting up NixOS configuration...");
    run_in_vm(
        runtime,
        name,
        "mkdir -p /etc/devbox/sets /etc/devbox/help",
        false,
    )
    .await?;

    // The agent and its module are part of the same host binary, so a box can
    // never accidentally retain a sidecar from another devbox release.
    let obsd_installed = match install_obsd_binary(runtime, name).await {
        Ok(()) => true,
        Err(error) => {
            eprintln!("Warning: observability agent was not installed: {error}");
            false
        }
    };
    write_obsd_module(runtime, name).await?;

    // 2. Ensure NixOS channel is available (images:nixos/* may not have it)
    //    nixos-rebuild needs `<nixpkgs/nixos>` in NIX_PATH, which comes from
    //    the nixos channel. If the channel isn't set up, add and update it.
    ensure_nixos_channel(runtime, name).await?;

    // 3. Generate base NixOS config if it doesn't exist
    //    NixOS Lima images ship with an empty /etc/nixos/ — we need to
    //    run nixos-generate-config to create the hardware and base configs.
    ensure_nixos_config(runtime, name, obsd_installed).await?;

    // 4. Ensure user home directory exists (the user may not exist yet on
    //    fresh images:nixos/* images — nixos-rebuild will create it via
    //    devbox-module.nix, but we need the homedir for writing config files
    //    before rebuild. We create it manually and let NixOS fix ownership later.)
    //
    //    Asked of the box rather than assumed to be `/home/<name>`. Lima has
    //    already created this user, with a home its cloud-init chose — on a
    //    Mac that is `/home/<name>.guest`, because the host's own home is
    //    mounted and the two would otherwise collide — and it put the box's
    //    `authorized_keys` there. Building `/home/<name>` here and letting
    //    NixOS re-home the passwd entry onto it is what left sshd looking for
    //    keys in an empty directory.
    let home_dir = detect_vm_home(runtime, name, &username).await;
    run_in_vm(
        runtime,
        name,
        &format!("mkdir -p {home_dir} && chown $(id -u {username} 2>/dev/null || echo 1000):users {home_dir} 2>/dev/null; true"),
        false,
    )
    .await?;

    // 5. Push devbox-state.toml (includes mount_mode for overlay setup)
    let shape = crate::nix::sets::GuestShape {
        user: Some(username.clone()),
        home: Some(home_dir.clone()),
        runtime: Some(runtime.name().to_string()),
        mount_mode: Some(mount_mode.to_string()),
        workspace_nofail: workspace_nofail_for(runtime, name).await,
    };
    let state_toml = generate_state_toml(sets, languages, &shape, packages);
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &state_toml).await?;

    // 6. Push devbox-module.nix
    write_file_to_vm(
        runtime,
        name,
        "/etc/devbox/devbox-module.nix",
        NIX_DEVBOX_MODULE,
    )
    .await?;

    // 5. Push all set .nix files
    for (filename, content) in crate::nix::sets::NIX_SET_FILES {
        let path = format!("/etc/devbox/sets/{filename}");
        write_file_to_vm(runtime, name, &path, content).await?;
    }

    // 8. Run nixos-rebuild switch (interactive so user sees progress)
    //    We must set NIX_PATH explicitly because:
    //    - incus exec doesn't source /etc/profile (no login shell)
    //    - images:nixos/* may not have channels in the default NIX_PATH
    //    - After nix-channel --update, nixpkgs lives at the channel profile path
    println!("Installing packages via nixos-rebuild (this may take a few minutes)...");
    // NIX_PATH is set explicitly rather than inherited: `incus exec` gives a
    // bare environment, `images:nixos/*` may carry no channel in the default
    // path, and after `nix-channel --update` nixpkgs lives at the channel
    // profile. On a Lima guest this is exactly what root's login shell already
    // exports, so it is a no-op there and a fix on Incus.
    let rebuild_cmd = "\
         export NIX_PATH=\"nixpkgs=/nix/var/nix/profiles/per-user/root/channels/nixos:\
         nixos-config=/etc/nixos/configuration.nix:\
         /nix/var/nix/profiles/per-user/root/channels\" && \
         export NIXPKGS_ALLOW_UNFREE=1 && \
         nixos-rebuild switch";
    let elevated = crate::policy::enforce::elevated_login(rebuild_cmd);
    let rebuild_argv = ["sh", "-c", elevated.as_str()];
    let mut result = run_install_step(runtime, name, &rebuild_argv, reporter).await?;

    // `nixos-rebuild switch` restarts the guest agent during activation on
    // Incus, which drops the exec session with exit 255 whether or not the
    // switch finished. That code is not a verdict, so wait for the guest to
    // answer again and run the (idempotent) switch once more: a completed
    // activation makes it a fast no-op, an interrupted one is finished, and a
    // real failure is reported by the rerun instead of being assumed away.
    if result.exit_code == 255 {
        println!("Connection lost during system activation; waiting for the guest to return...");
        wait_for_guest_exec(runtime, name).await?;
        result = run_install_step(runtime, name, &rebuild_argv, reporter).await?;
    }
    let retry = format!("devbox exec {name} -- sudo nixos-rebuild switch");
    require_install_success(&result, "nixos-rebuild switch", &retry)?;
    println!("NixOS rebuild complete.");
    // A freshly provisioned box is built from the module this build ships;
    // say so, or `agent_sync` reads it as unknown and rebuilds it once for
    // nothing on the very next command.
    stamp_module_build(runtime, name).await;

    // Detect the actual VM user/home after nixos-rebuild (may differ from
    // host username — e.g. Lima creates "ethan.linux" from host "ethan").
    let vm_user = detect_vm_username(runtime, name).await;
    let vm_home = detect_vm_home(runtime, name, &vm_user).await;

    // 9. Set up user shell (zshrc with PATH, aliases, etc.)
    setup_nixos_shell(runtime, name, &vm_user, &vm_home).await?;

    // 10. Install latest claude-code (nixpkgs version lags behind)
    if sets.iter().any(|s| s == "ai-code" || s == "ai_code") {
        install_latest_claude_code(runtime, name, &vm_user, &vm_home).await;
    }

    // 11. Copy host git config into VM
    setup_git_config(runtime, name, &vm_user, &vm_home).await?;

    // 12. Copy devbox binary + help files + tool configs
    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;
    setup_management_script(runtime, name).await?;
    setup_yazi_config(runtime, name, &vm_user, &vm_home).await?;
    setup_aichat_config(runtime, name, &vm_user, &vm_home).await?;
    setup_ai_tool_configs(runtime, name, &vm_user, &vm_home).await?;

    Ok(())
}

// ── Ubuntu Provisioning ─────────────────────────────────────

/// Provision an Ubuntu VM: install Nix package manager + nix profile install.
async fn provision_ubuntu(
    runtime: &dyn Runtime,
    name: &str,
    sets: &[String],
    languages: &[String],
    extra: &[(String, String)],
    reporter: Option<ProvisionReporter<'_>>,
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
        eprintln!(
            "Warning: Nix installation may have issues: {}",
            result.stderr.trim()
        );
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
    // The box's ad-hoc packages too. They were accepted, persisted in sandbox
    // state, and reported as selected — and never installed, because only the
    // NixOS path read them.
    let mut packages: Vec<String> = packages.into_iter().map(str::to_string).collect();

    // Validated before they reach a shell. These come from `[custom_packages]`
    // in a hand-written devbox.toml, are interpolated unquoted into
    // `bash -c "nix profile install …"`, and nothing else checks them — so a
    // key like `foo; touch /tmp/pwned; #` ran during provisioning. The NixOS
    // path validates via `Selection::validate`; this one had no equivalent.
    for (name, source) in extra {
        let reference = installable(name, source);
        // The *whole* reference, not the fragment after `#`.
        //
        // Validating only the tail reopened the injection this guard was added
        // to close: `github:user/repo; touch /tmp/pwn; #pkg` has a clean
        // fragment and a shell command in front of it, and the value is joined
        // unquoted into `bash -c "nix profile install …"`. I introduced that
        // gap last round by teaching this path about flake references without
        // extending the check to cover them.
        if !is_safe_installable(&reference) {
            bail!(
                "custom package '{name}' is not a valid nixpkgs attribute path; \
                 names may contain letters, digits, '_', '-', and '.' only"
            );
        }
    }
    packages.extend(extra.iter().map(|(n, s)| installable(n, s)));
    packages.sort();
    packages.dedup();

    if !packages.is_empty() {
        // 3. Install all packages via nix profile install
        // Set packages are bare attribute names; the ad-hoc ones already
        // arrive as complete references (see `installable`), so a blanket
        // `nixpkgs#` prefix would corrupt every flake reference.
        let pkg_args: Vec<String> = packages
            .iter()
            .map(|p| {
                if p.contains('#') {
                    p.clone()
                } else {
                    format!("nixpkgs#{p}")
                }
            })
            .collect();
        let pkg_list = pkg_args.join(" ");

        println!(
            "Installing {} packages via Nix (this may take a few minutes)...",
            packages.len()
        );

        // Source the nix profile before running nix commands
        let install_cmd = format!(
            ". /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh && nix profile install {pkg_list}"
        );
        let result =
            run_install_step(runtime, name, &["bash", "-c", &install_cmd], reporter).await?;
        let retry = format!("devbox exec {name} -- nix profile install <packages>");
        require_install_success(&result, "nix profile install", &retry)?;
        println!("Nix package installation complete.");
    }

    // 4. Install services that need apt (Docker, Tailscale)
    install_ubuntu_services(runtime, name, sets).await?;

    // 5. Set up shell environment
    setup_ubuntu_shell(runtime, name).await?;

    // Detect the actual VM user/home (may differ from host username)
    let vm_user = detect_vm_username(runtime, name).await;
    let vm_home = detect_vm_home(runtime, name, &vm_user).await;

    // 6. Copy host git config into VM
    setup_git_config(runtime, name, &vm_user, &vm_home).await?;

    // 7. Create devbox directories and copy binary + help
    run_in_vm(runtime, name, "mkdir -p /etc/devbox/help", false).await?;

    println!("Copying devbox into VM...");
    copy_devbox_to_vm(runtime, name).await?;
    setup_help_in_vm(runtime, name).await?;
    setup_management_script(runtime, name).await?;
    setup_yazi_config(runtime, name, &vm_user, &vm_home).await?;
    setup_aichat_config(runtime, name, &vm_user, &vm_home).await?;
    setup_ai_tool_configs(runtime, name, &vm_user, &vm_home).await?;

    // Observability is useful but not a prerequisite for a usable box. Keep it
    // after package and shell setup, and leave a loud warning if its runtime-
    // specific transport cannot be installed.
    match install_obsd_binary(runtime, name).await {
        Ok(()) => {
            if let Err(error) = install_ubuntu_obsd_service(runtime, name).await {
                eprintln!("Warning: observability service was not enabled: {error}");
            }
        }
        Err(error) => eprintln!("Warning: observability agent was not installed: {error}"),
    }

    Ok(())
}

/// Install services that need OS-level integration on Ubuntu.
/// Nix installs the binaries but systemd services need apt packages.
async fn install_ubuntu_services(runtime: &dyn Runtime, name: &str, sets: &[String]) -> Result<()> {
    let needs_docker = sets.iter().any(|s| s == "container");
    let needs_tailscale = sets.iter().any(|s| s == "network");

    if needs_docker {
        print!("  Setting up Docker service...");
        let cmd = "export DEBIAN_FRONTEND=noninteractive && \
            apt-get update -qq && \
            apt-get install -y -qq docker.io >/dev/null 2>&1 && \
            usermod -aG docker $(whoami) && \
            systemctl enable --now docker";
        let result = run_in_vm(runtime, name, cmd, false).await;
        match result {
            Ok(r) if r.exit_code == 0 => println!(" done"),
            _ => println!(" skipped"),
        }
    }

    if needs_tailscale {
        print!("  Setting up Tailscale service...");
        let cmd = "curl -fsSL https://tailscale.com/install.sh | sh && \
            systemctl enable --now tailscaled";
        let result = run_in_vm(runtime, name, cmd, false).await;
        match result {
            Ok(r) if r.exit_code == 0 => println!(" done"),
            _ => println!(" skipped"),
        }
    }

    // Never fatal: a box whose sshd will not take the drop-in is a box where
    // `devbox code` has no broker, not a box that failed to provision.
    if let Err(error) = write_sshd_accept_env(runtime, name).await {
        eprintln!("Warning: could not configure sshd to accept the broker environment: {error:#}");
    }

    Ok(())
}

/// The sshd drop-in that lets the credential broker's variables through.
pub(crate) const SSHD_DROPIN_PATH: &str = "/etc/ssh/sshd_config.d/60-devbox-broker.conf";

/// Tell a non-NixOS box's sshd to accept the broker's environment.
///
/// Only for images whose sshd reads `/etc/ssh/sshd_config.d/`, which is
/// Debian and Ubuntu's layout — their stock `sshd_config` opens with an
/// `Include` of that directory. NixOS has neither the `Include` nor a writable
/// `/etc/ssh`: its `sshd_config` is a symlink into the store, so its half of
/// this lives in `nix/devbox-module.nix` and arrives through a rebuild.
///
/// **Unverified against a real Ubuntu box** — no Ubuntu image was provisioned
/// on the machine this was written on. The path and the `Include` behaviour
/// are Debian policy, not a measurement here.
pub(crate) async fn write_sshd_accept_env(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let content = format!(
        "# Written by devbox. Lets `devbox code` carry the credential broker's\n\
         # variables into a Remote SSH session; no credential is stored here.\n\
         {}\n",
        crate::broker::accept_env_line()
    );
    run_in_vm(runtime, name, "mkdir -p /etc/ssh/sshd_config.d", false).await?;
    write_file_to_vm(runtime, name, SSHD_DROPIN_PATH, &content).await?;
    // A drop-in nothing re-reads is a drop-in that does nothing. `reload`
    // rather than `restart`, so an editor already attached over ssh keeps its
    // session.
    let _ = run_in_vm(
        runtime,
        name,
        "systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true",
        false,
    )
    .await;
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
  echo "$NIX_ZSH" | tee -a /etc/shells >/dev/null
  chsh -s "$NIX_ZSH" {username}
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
alias vim='nvim' 2>/dev/null
alias vi='nvim' 2>/dev/null

# Devbox identity
export DEVBOX_NAME="${{DEVBOX_NAME:-devbox}}"
export DEVBOX_RUNTIME="${{DEVBOX_RUNTIME:-unknown}}"

# Default to workspace directory
[ -d /workspace ] && cd /workspace
ZSHRC
"#
    );

    let result = run_in_vm(runtime, name, &setup, false).await;
    if let Ok(r) = result
        && r.exit_code != 0
    {
        eprintln!("Warning: shell setup incomplete");
    }

    Ok(())
}

/// Set up user shell environment on NixOS (zshrc with PATH, aliases, workspace cd).
async fn setup_nixos_shell(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) -> Result<()> {
    let zshrc_path = format!("{vm_home}/.zshrc");

    // Only create if .zshrc doesn't exist yet (don't overwrite user customizations)
    let check = runtime
        .exec_cmd(name, &["test", "-f", &zshrc_path], false)
        .await?;

    if check.exit_code != 0 {
        let zshrc = r#"# Devbox shell configuration
# Latest tools first (official installers take precedence over nixpkgs)
export PATH="$HOME/.npm-global/bin:$HOME/.local/bin:$HOME/.claude/bin:$PATH"

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
alias vim='nvim' 2>/dev/null
alias vi='nvim' 2>/dev/null

# Devbox identity
export DEVBOX_NAME="${DEVBOX_NAME:-devbox}"
export DEVBOX_RUNTIME="${DEVBOX_RUNTIME:-unknown}"

# Default to workspace directory
[ -d /workspace ] && cd /workspace
"#;
        write_file_to_vm(runtime, name, &zshrc_path, zshrc).await?;

        run_in_vm(
            runtime,
            name,
            &format!("chown {vm_user}:users {zshrc_path}"),
            false,
        )
        .await?;
    }

    // Also create .profile for bash login shells (used by layout panes with bash -lc)
    let profile_path = format!("{vm_home}/.profile");
    let profile_check = runtime
        .exec_cmd(name, &["test", "-f", &profile_path], false)
        .await?;
    if profile_check.exit_code != 0 {
        let profile = r#"# Devbox bash profile
# Latest tools first (npm-global and official installers take precedence over nixpkgs)
export PATH="$HOME/.npm-global/bin:$HOME/.local/bin:$HOME/.claude/bin:$PATH"
"#;
        write_file_to_vm(runtime, name, &profile_path, profile).await?;
        run_in_vm(
            runtime,
            name,
            &format!("chown {vm_user}:users {profile_path}"),
            false,
        )
        .await?;
    }

    Ok(())
}

// ── Git Config ─────────────────────────────────────────────

/// Copy host ~/.gitconfig into the VM so git user.name, user.email,
/// remote aliases, and other settings carry over — and then rewrite the
/// devbox-managed section that points github at the credential broker (§6.3).
///
/// The host gitconfig is copied for its identity settings, not its
/// credentials: a `[credential]` helper naming a host binary or a host
/// keychain is meaningless in the guest, and a helper that stores a token in
/// plaintext must not be carried across. Those sections are dropped.
async fn setup_git_config(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let gitconfig_path = home.join(".gitconfig");

    let host = match std::fs::read_to_string(&gitconfig_path) {
        Ok(content) => strip_credential_sections(&content),
        Err(_) => String::new(),
    };

    // The broker's address changes whenever the box restarts onto a different
    // port, so the section is rewritten rather than appended — git takes the
    // last `insteadOf` that matches, and a stale one above a fresh one wins.
    let broker = broker_base_url(runtime, name).await;
    let content = crate::broker::apply_gitconfig_section(&host, broker.as_deref());
    if content.trim().is_empty() {
        return Ok(());
    }

    let vm_path = format!("{vm_home}/.gitconfig");
    write_file_to_vm(runtime, name, &vm_path, &content).await?;

    let chown_cmd = format!("chown {vm_user}:users {vm_path}");
    run_in_vm(runtime, name, &chown_cmd, false).await?;

    if broker.is_some() {
        println!("Synced host git config, and pointed github.com at the devbox broker.");
    } else {
        println!("Synced host git config to VM.");
    }
    Ok(())
}

/// The broker URL this box should use, or `None` when there is nothing to
/// broker or no verified way to reach the host.
async fn broker_base_url(runtime: &dyn Runtime, name: &str) -> Option<String> {
    let manager = crate::sandbox::SandboxManager::new().ok()?;
    if !crate::broker::configured_providers(&manager.state_dir)
        .iter()
        .any(|p| p == crate::broker::providers::GITHUB)
    {
        return None;
    }
    let endpoint = crate::broker::endpoint(&manager.state_dir)?;
    let reach = tokio::time::timeout(
        crate::broker::reach::REACH_TIMEOUT,
        runtime.host_reach(name, endpoint.port),
    )
    .await
    .ok()?
    .ok()?;
    Some(reach.base_url())
}

/// Drop `[credential ...]` sections from a gitconfig.
///
/// A credential helper is a host-side thing — `osxkeychain`, a binary under
/// `/opt/homebrew`, a `store` file in the host's home — and every one of those
/// either fails in the guest or, worse, names a plaintext token file that the
/// copy would be pointing at. The broker is how the guest gets git
/// credentials now, so there is nothing here worth carrying across.
pub(crate) fn strip_credential_sections(content: &str) -> String {
    let mut out = String::new();
    let mut in_credential = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').to_ascii_lowercase();
            in_credential = header.starts_with("credential");
        }
        if !in_credential {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ── Shared Helpers ──────────────────────────────────────────

/// Which user a box runs as, and the home that user's login shell actually
/// uses.
///
/// The pair the rest of devbox derives everything else from, exposed because
/// `devbox repair stale-home` has to know which of a box's two home
/// directories is the real one before it offers to delete the other.
pub(crate) async fn guest_identity(runtime: &dyn Runtime, name: &str) -> Result<(String, String)> {
    let username = detect_vm_username(runtime, name).await;
    let home = detect_vm_home(runtime, name, &username).await;
    Ok((username, home))
}

/// What the guest answered when asked whether it is a box being born.
///
/// Every field is what the *box* said rather than what the host inferred, and
/// `answered` is the one that matters most: a probe that never ran must not be
/// read as "this box has no state file".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BirthProbe {
    /// The probe ran to completion in the guest.
    pub answered: bool,
    /// `/etc/devbox/devbox-state.toml` exists.
    pub has_state: bool,
    /// What that file says about `workspace_nofail`, if anything.
    pub state_says_nofail: bool,
    /// `/etc/fstab` already declares a `/workspace` mount.
    pub fstab_has_workspace: bool,
    /// That declaration carries `nofail`.
    ///
    /// From `/etc/fstab`, deliberately, and not from the running mount.
    /// `nofail` is an fstab and systemd option: it never reaches the kernel,
    /// so `findmnt -no OPTIONS /workspace` does not report it on a box that
    /// has it — measured on a box built with it, which lists only
    /// `rw,relatime,lowerdir=…,upperdir=…,workdir=…,uuid=on`. Comparing the
    /// recorded key against the *mount* would therefore call every such box a
    /// mismatch and refuse every rebuild it ever needed.
    pub fstab_has_nofail: bool,
    /// `/workspace` is mounted right now.
    pub workspace_mounted: bool,
}

/// Ask the box the questions the grant depends on, in one command.
///
/// The trailing `probe=ok` is the whole point. Lima's `exec_cmd` shells out to
/// `limactl shell`, and a VM that is not up — or an ssh connection that
/// stutters for a moment — comes back as a **non-zero exit rather than an
/// `Err`**. An exit code therefore cannot tell "this box has no state file"
/// apart from "this box could not be asked". A marker the guest prints only
/// after every question has been answered can.
pub(crate) const BIRTH_PROBE: &str = r#"
if [ -e /etc/devbox/devbox-state.toml ]; then
  printf 'state=present\n'
  printf 'nofail=%s\n' "$(awk -F= '/^[[:space:]]*workspace_nofail[[:space:]]*=/ { gsub(/[[:space:]]/, "", $2); print $2; exit }' /etc/devbox/devbox-state.toml 2>/dev/null)"
else
  printf 'state=absent\n'
fi
workspace_line=$(awk '$2 == "/workspace" { print; exit }' /etc/fstab 2>/dev/null)
if [ -n "$workspace_line" ]; then
  printf 'fstab=yes\n'
  case ",$(printf '%s' "$workspace_line" | awk '{print $4}')," in
    *,nofail,*) printf 'fstabnofail=yes\n' ;;
    *) printf 'fstabnofail=no\n' ;;
  esac
else
  printf 'fstab=no\n'
fi
if findmnt -n /workspace >/dev/null 2>&1; then
  printf 'mounted=yes\n'
else
  printf 'mounted=no\n'
fi
printf 'probe=ok\n'
"#;

/// Read [`BIRTH_PROBE`]'s output.
pub(crate) fn parse_birth_probe(stdout: &str) -> BirthProbe {
    let mut probe = BirthProbe::default();
    for line in stdout.lines() {
        match line.trim() {
            "probe=ok" => probe.answered = true,
            "state=present" => probe.has_state = true,
            "nofail=true" => probe.state_says_nofail = true,
            "fstab=yes" => probe.fstab_has_workspace = true,
            "fstabnofail=yes" => probe.fstab_has_nofail = true,
            "mounted=yes" => probe.workspace_mounted = true,
            _ => {}
        }
    }
    probe
}

/// Whether this box's `/workspace` may carry `nofail`.
///
/// Decided once, at the box's birth, and never revisited. `nofail` is what
/// keeps an overlay that cannot be assembled from taking `local-fs.target`
/// down and dropping the guest into emergency mode with no sshd — but adding
/// it to a box that already has the mount is what W3-10 found leaves that box
/// unable to complete any rebuild at all: `switch-to-configuration` reloads a
/// mount unit whose options changed, a reload of an overlay is a remount, and
/// overlayfs answers every one of those with `No changes allowed in
/// reconfigure`. The switch exits 4 and NixOS rolls the generation back, for
/// good, because `/etc` has already moved.
///
/// A grant therefore needs two independent things to be true, and a box that
/// could not answer gets neither:
///
/// 1. the box has no state file, so it has never been provisioned;
/// 2. it has no `/workspace` in `/etc/fstab` and none mounted, so there is no
///    mount whose options a rebuild could be asked to change.
///
/// The second is not redundant. W4-2 granted on the first alone and read a
/// non-zero exit as "no state file" — which is also what a stuttering ssh
/// connection looks like. One unlucky moment during a `reprovision` of an old
/// box would have written the key onto a box whose mount does not have it, and
/// that box could then never be rebuilt again. Narrow window, permanent
/// consequence.
pub(crate) fn grant_workspace_nofail(probe: &BirthProbe) -> bool {
    if !probe.answered {
        return false;
    }
    if probe.has_state {
        // This box has been through this before. What it decided then is what
        // its mount was built with, and that is not ours to revise.
        return probe.state_says_nofail;
    }
    !probe.fstab_has_workspace && !probe.workspace_mounted
}

async fn workspace_nofail_for(runtime: &dyn Runtime, name: &str) -> bool {
    let stdout = match runtime
        .exec_cmd(name, &["sh", "-c", BIRTH_PROBE], false)
        .await
    {
        Ok(result) => result.stdout,
        Err(_) => String::new(),
    };
    grant_workspace_nofail(&parse_birth_probe(&stdout))
}

/// Refuse to rebuild a box whose recorded workspace options are not the ones
/// it is actually mounted with.
///
/// The key is a record of how the mount was built, not a wish about how it
/// should be. If the two disagree — because someone edited the state file by
/// hand — the next rebuild changes the mount's options, overlayfs refuses the
/// remount, the switch exits 4, and the box can never be rebuilt again. Worth
/// stopping in front of, because there is no stopping after.
///
/// The comparison is against `/etc/fstab`, not against the running mount:
/// `nofail` never reaches the kernel, so a box that has it reports mount
/// options without it and would look like a mismatch forever. fstab is also
/// the right thing to compare — it is what `switch-to-configuration` diffs to
/// decide whether the unit needs the reload that cannot succeed.
///
/// A box with no `/workspace` line has nothing to disagree with, which is the
/// state every box being provisioned is in.
pub(crate) fn refuse_on_workspace_mismatch(box_name: &str, probe: &BirthProbe) -> Result<()> {
    if !probe.answered || !probe.fstab_has_workspace {
        return Ok(());
    }
    if probe.state_says_nofail == probe.fstab_has_nofail {
        return Ok(());
    }
    let (says, has) = if probe.state_says_nofail {
        ("nofail", "without it")
    } else {
        ("no nofail", "with it")
    };
    // `concat!` with positional arguments rather than a `\`-continued literal:
    // rustfmt rejoins a continued string and leaves its indentation inside.
    bail!(
        concat!(
            "box '{}' was not rebuilt. Its devbox-state.toml records the workspace mount as ",
            "'{}' while /etc/fstab declares it {}, and overlayfs refuses to remount ",
            "with different options — so a rebuild from here would fail to activate and roll ",
            "back, permanently. That key records how the mount was built; it is not a setting. ",
            "If it was edited by hand, put it back.",
        ),
        box_name,
        says,
        has
    )
}

/// Generate devbox-state.toml content from active sets and languages.
fn generate_state_toml(
    sets: &[String],
    languages: &[String],
    shape: &crate::nix::sets::GuestShape,
    packages: &[String],
) -> String {
    let set_names = [
        "system",
        "shell",
        "tools",
        "editor",
        "git",
        "container",
        "network",
        "ai_code",
        "ai_infra",
    ];
    let lang_names = ["go", "rust", "python", "node", "java", "ruby"];

    let username = shape.user.as_deref().unwrap_or("dev");
    let mut toml = String::from("[user]\n");
    toml.push_str(&format!("name = \"{username}\"\n"));
    // The home the box's login shell actually uses, so `devbox-module.nix` can
    // declare it. Without it NixOS defaults `isNormalUser` to `/home/<name>`
    // and rewrites the passwd entry Lima wrote, which is how a box ends up
    // with its `authorized_keys` in one directory and sshd looking in another.
    // Omitted rather than guessed when the box could not be asked: a wrong
    // value here is worse than the default, because it moves the entry away
    // from wherever the keys really are.
    if let Some(home) = &shape.home {
        toml.push_str(&format!("home = \"{home}\"\n"));
    }
    toml.push('\n');

    toml.push_str("[sets]\n");
    for s in &set_names {
        // Normalize: active sets use hyphens ("ai-code") but TOML keys use underscores ("ai_code")
        let hyphenated = s.replace('_', "-");
        let enabled = sets
            .iter()
            .any(|active| active == s || active == &hyphenated);
        toml.push_str(&format!("{s} = {enabled}\n"));
    }

    toml.push_str("\n[languages]\n");
    for l in &lang_names {
        let enabled = languages.iter().any(|active| active == l)
            || sets.iter().any(|active| active == &format!("lang-{l}"));
        toml.push_str(&format!("{l} = {enabled}\n"));
    }

    toml.push_str("\n[sandbox]\n");
    let mount_mode = shape.mount_mode.as_deref().unwrap_or("overlay");
    toml.push_str(&format!("mount_mode = \"{mount_mode}\"\n"));
    // Which hypervisor the box runs under. The module gates the Incus guest
    // agent on it: on Lima there is no incus host to talk to, so the agent
    // fails and systemd restarts it every five seconds forever.
    let runtime = shape.runtime.as_deref().unwrap_or("incus");
    toml.push_str(&format!("runtime = \"{runtime}\"\n"));
    // Only when true, and only ever decided at the box's birth: writing the
    // key onto a box that never had it is what changes a mounted overlay's
    // options, which no rebuild can then apply.
    if shape.workspace_nofail {
        toml.push_str("workspace_nofail = true\n");
    }

    // Ad-hoc packages. Quoted, because an attribute path like
    // `python312Packages.ipython` is otherwise read as a nested table and the
    // module resolves the wrong thing.
    if !packages.is_empty() {
        toml.push_str("\n[custom_packages]\n");
        for pkg in packages {
            toml.push_str(&format!("\"{pkg}\" = \"nixpkgs\"\n"));
        }
    }

    toml
}

// ── AI Tool Settings (never credentials) ───────────────────

/// Describes an AI coding tool's host configuration.
struct AiToolConfig {
    name: &'static str,
    /// Settings files to copy: (host_path_suffix, vm_path_suffix), relative to
    /// the home directory. **Credential files are not on this list, and adding
    /// one would break the promise the whole broker exists to keep** (§6.2).
    config_files: &'static [(&'static str, &'static str)],
}

/// Known AI tool configurations — settings only.
///
/// v4 also copied `~/.claude/.credentials.json` and `~/.codex/auth.json` into
/// the guest, wrote every `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` it could find
/// into `~/.devbox-ai-env` and sourced it from `.zshrc`, and synthesised an
/// `aichat` config with a key inlined. All four are gone: they contradicted
/// the README's own "credential safety" line, and they are what the credential
/// broker replaces. The agent now gets a per-box broker token and a URL; the
/// credential stays on the host.
static AI_TOOL_CONFIGS: &[AiToolConfig] = &[
    AiToolConfig {
        name: "claude-code",
        config_files: &[(".claude/settings.json", ".claude/settings.json")],
    },
    AiToolConfig {
        name: "opencode",
        config_files: &[(
            ".config/opencode/config.json",
            ".config/opencode/config.json",
        )],
    },
    AiToolConfig {
        name: "codex",
        config_files: &[(".codex/config.json", ".codex/config.json")],
    },
    AiToolConfig {
        name: "aichat",
        config_files: &[(".config/aichat/config.yaml", ".config/aichat/config.yaml")],
    },
];

/// Copy the host's AI tool *settings* into the VM.
///
/// Every file is scanned first, and one that carries credential material is
/// skipped with a message rather than copied. That check is not paranoia about
/// files we happen to know: `~/.claude/settings.json` has an `env` block that
/// can hold `ANTHROPIC_API_KEY`, `~/.config/aichat/config.yaml` normally has
/// an `api_key:` line, and `~/.config/opencode/config.json` was the file v4's
/// own key-scraper read `apiKey` out of. A settings file is only a settings
/// file until someone puts a key in it.
async fn setup_ai_tool_configs(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut copied_any = false;

    for tool in AI_TOOL_CONFIGS {
        let mut announced = false;
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

            if let Some(marker) = credential_material(&content) {
                eprintln!(
                    "  skipped: ~/{host_suffix} — it contains what looks like a credential \
                     ({marker}). Credentials stay on the host; run `devbox secret set` instead."
                );
                continue;
            }

            if !announced {
                println!("Found {} configuration on host:", tool.name);
                announced = true;
            }

            let vm_path = format!("{vm_home}/{vm_suffix}");
            let vm_parent = vm_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(&vm_path);
            run_in_vm(runtime, name, &format!("mkdir -p {vm_parent}"), false).await?;

            write_file_to_vm(runtime, name, &vm_path, &content).await?;
            println!("  copied: ~/{host_suffix} → {vm_path}");
            copied_any = true;
        }
    }

    if copied_any {
        let chown_cmd = format!(
            "chown -R {vm_user}:users {vm_home}/.claude {vm_home}/.config {vm_home}/.codex 2>/dev/null; true"
        );
        run_in_vm(runtime, name, &chown_cmd, false).await?;
        println!("AI tool settings synced to devbox. No credentials were copied.");
    }

    // v4 left `~/.devbox-ai-env` behind on every box it provisioned, with the
    // host's API keys in it, sourced from `.zshrc`. Re-provisioning an
    // existing box has to remove it, or the box keeps the keys the upgrade
    // was supposed to take away.
    let purge = format!(
        "rm -f {vm_home}/.devbox-ai-env {vm_home}/.claude/.credentials.json \
         {vm_home}/.codex/auth.json 2>/dev/null; \
         if [ -f {vm_home}/.zshrc ]; then \
           sed -i '/devbox-ai-env/d' {vm_home}/.zshrc 2>/dev/null || true; \
         fi; true"
    );
    run_in_vm(runtime, name, &purge, false).await?;

    Ok(())
}

/// Whether a settings file carries something that looks like a credential.
///
/// Shape-based, and deliberately blunt. A false positive costs one skipped
/// settings file and a printed reason; a false negative writes a live key into
/// the guest, which is the exact failure this component exists to end.
fn credential_material(content: &str) -> Option<&'static str> {
    const PREFIXES: &[(&str, &str)] = &[
        ("sk-ant-", "an Anthropic key"),
        ("sk-proj-", "an OpenAI project key"),
        ("sk-or-", "an OpenRouter key"),
        ("ghp_", "a GitHub personal access token"),
        ("gho_", "a GitHub OAuth token"),
        ("github_pat_", "a GitHub fine-grained token"),
        ("xoxb-", "a Slack token"),
        ("AKIA", "an AWS access key id"),
    ];
    for (needle, label) in PREFIXES {
        if content.contains(needle) {
            return Some(label);
        }
    }
    // A key-shaped field with a non-empty value. `"api_key": ""` and
    // `apiKeyHelper` are common and harmless; `"api_key": "x"` is not.
    const FIELDS: &[&str] = &[
        "api_key",
        "apikey",
        "auth_token",
        "access_token",
        "refresh_token",
    ];
    for line in content.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(field) = FIELDS.iter().find(|f| lower.contains(*f)) else {
            continue;
        };
        let Some((_, after)) = lower.split_once(field) else {
            continue;
        };
        // Strip the punctuation on both sides so `"api_key": ""` and
        // `api_key: ''` — the shape of a config skeleton — read as empty,
        // while `"api_key": "x"` does not. The closing brace and bracket are
        // in the set because a one-line JSON object ends `""}` and trimming
        // only quotes would leave a `}` and call it a value.
        let value = after
            .trim_start_matches(['"', '\'', ' ', ':', '=', '\t'])
            .trim_end_matches([',', '"', '\'', ' ', '\t', '}', ']', ';']);
        if !value.is_empty() && value != "null" && value != "{}" && !value.starts_with("helper") {
            return Some("a key-shaped field with a value");
        }
    }
    // `sk-` alone is too common a substring to match on, but a quoted value
    // that starts with it is not.
    if content.contains("\"sk-") || content.contains("'sk-") {
        return Some("a quoted sk- value");
    }
    None
}

/// Run a command as root inside the VM with a login shell.
/// Delegates to `runtime.run_as_root()` which handles the platform
/// difference: Incus runs as root directly, Lima wraps in `sudo`.
async fn run_in_vm(
    runtime: &dyn Runtime,
    name: &str,
    cmd: &str,
    interactive: bool,
) -> Result<crate::runtime::ExecResult> {
    runtime.run_as_root(name, cmd, interactive).await
}

/// Write a file into the VM using base64-encoded content.
/// Runs the entire pipeline as root via `run_as_root`.
async fn write_file_to_vm(
    runtime: &dyn Runtime,
    name: &str,
    path: &str,
    content: &str,
) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let cmd = format!("echo '{encoded}' | base64 -d | tee {path} > /dev/null");
    let result = runtime.run_as_root(name, &cmd, false).await?;
    if result.exit_code != 0 {
        bail!("failed to write {path}: {}", result.stderr.trim());
    }
    Ok(())
}

/// Push the NixOS module that supervises the agent.
///
/// Named rather than inlined because the agent refresh path rewrites it too:
/// the module is where `enableEbpf` turns into both the `-no-ebpf` flag and
/// the capability set that flag needs, so a box whose agent is replaced has to
/// receive the module this host build ships, not the one it was born with.
/// Push the current `devbox-module.nix` into a box.
///
/// The module is the declarative half of several devbox behaviours — the
/// overlay mount, the package set, sshd's `AcceptEnv` — and a box keeps
/// whichever copy was current when it was provisioned. Anything that changes
/// the module has to push it before rebuilding, or the rebuild imports the old
/// one and reports success.
pub(crate) async fn write_devbox_module(runtime: &dyn Runtime, name: &str) -> Result<()> {
    write_file_to_vm(
        runtime,
        name,
        "/etc/devbox/devbox-module.nix",
        NIX_DEVBOX_MODULE,
    )
    .await
}

/// Record the box's real login home in its `devbox-state.toml`.
///
/// Read-modify-write rather than regenerate: this runs from `agent_sync`,
/// which knows nothing about the box's sets, languages or packages, and
/// rewriting the file from what it does know would reset all three.
pub(crate) async fn record_guest_home(runtime: &dyn Runtime, name: &str, home: &str) -> Result<()> {
    let current = runtime
        .exec_cmd(name, &["cat", "/etc/devbox/devbox-state.toml"], false)
        .await
        .with_context(|| format!("read the declared state of box '{name}'"))?;
    if current.exit_code != 0 {
        bail!(
            "box '{name}' has no readable devbox-state.toml, so its guest home \
             cannot be recorded"
        );
    }
    let updated = with_user_home(&current.stdout, home)?;
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &updated).await
}

/// Record which hypervisor a box runs under in its `devbox-state.toml`.
///
/// The module gates the Incus guest agent on this key, and a box provisioned
/// before devbox wrote it has no key at all — where the module has to assume
/// Incus, because switching the agent off on a real Incus box would break
/// `incus exec` after every rebuild. So the repair supplies the answer rather
/// than leaving the module to guess it.
pub(crate) async fn record_guest_runtime(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let current = runtime
        .exec_cmd(name, &["cat", "/etc/devbox/devbox-state.toml"], false)
        .await
        .with_context(|| format!("read the declared state of box '{name}'"))?;
    if current.exit_code != 0 {
        bail!("box '{name}' has no readable devbox-state.toml, so its runtime cannot be recorded");
    }
    let updated = with_state_value(&current.stdout, "sandbox", "runtime", runtime.name())?;
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &updated).await
}

/// Set `[user].home` in a `devbox-state.toml`, leaving everything else alone.
///
/// Split out from the guest round trip because the part that can be wrong is
/// this one: the file also carries the sets, the languages and the ad-hoc
/// package table, and losing any of them would change what the next rebuild
/// installs.
fn with_user_home(state: &str, home: &str) -> Result<String> {
    with_state_value(state, "user", "home", home)
}

/// Set one `[table].key` in a `devbox-state.toml`, leaving everything else
/// alone.
///
/// Read-modify-write rather than regenerate, because the callers run from
/// `agent_sync`, which knows nothing about the box's sets, languages or
/// packages — and rewriting the file from what it does know would reset all
/// three.
fn with_state_value(state: &str, table: &str, key: &str, value: &str) -> Result<String> {
    let mut doc: toml::Value = state
        .parse()
        .context("the box's devbox-state.toml is not valid TOML")?;
    let root = doc
        .as_table_mut()
        .context("the box's devbox-state.toml is not a table")?;
    let section = root
        .entry(table)
        .or_insert_with(|| toml::Value::Table(Default::default()));
    section
        .as_table_mut()
        .with_context(|| {
            format!("the box's devbox-state.toml has a [{table}] that is not a table")
        })?
        .insert(key.to_string(), toml::Value::String(value.to_string()));
    toml::to_string(&doc).context("could not re-serialise the box's devbox-state.toml")
}

/// Point a non-NixOS box's passwd entry at the home its login shell uses.
///
/// Deliberately without `-m`. The directory already exists and holds the box's
/// `authorized_keys`, its shell rc and its settings; `usermod -m` would move
/// that content to the *other* path, which is the opposite of the repair.
pub(crate) async fn realign_passwd_home(
    runtime: &dyn Runtime,
    name: &str,
    home: &str,
) -> Result<()> {
    let user = detect_vm_username(runtime, name).await;
    let result = runtime
        .run_as_root(name, &format!("usermod -d {home} {user}"), false)
        .await
        .with_context(|| format!("move the passwd home of box '{name}'"))?;
    if result.exit_code != 0 {
        bail!(
            "could not point '{user}' at {home} in box '{name}': {}",
            result.stderr.trim()
        );
    }
    Ok(())
}

/// Record which `devbox-module.nix` this box was last built from.
///
/// Written only after a `nixos-rebuild` that succeeded, because that is the
/// question `agent_sync` asks: not "which file is in /etc/devbox" — that one
/// is put there before the rebuild and stays there when it fails — but "which
/// bytes is this system actually built from".
///
/// Best effort. A box that cannot record it reports no stamp, which reads as
/// "cannot tell" and costs one redundant rebuild, never a wrong answer.
pub(crate) async fn stamp_module_build(runtime: &dyn Runtime, name: &str) {
    let digest = crate::sandbox::agent_sync::host_module_digest();
    if let Err(error) =
        write_file_to_vm(runtime, name, "/etc/devbox/module-built.sha", digest).await
    {
        tracing::debug!(box_id = %name, %error, "could not stamp the module build");
    }
}

pub(crate) async fn write_obsd_module(runtime: &dyn Runtime, name: &str) -> Result<()> {
    write_file_to_vm(
        runtime,
        name,
        "/etc/devbox/obsd-module.nix",
        NIX_OBSD_MODULE,
    )
    .await
}

/// Materialize the embedded Go agent through the runtime's native copy path.
/// Binary payloads are not passed through argv: even base64 exceeds the OS
/// argument limit long before a statically linked agent does.
async fn install_obsd_binary(runtime: &dyn Runtime, name: &str) -> Result<()> {
    install_embedded_binary(runtime, name, "devbox-obsd", crate::embedded::OBSD).await
}

/// Materialize a release-pinned guest binary through the runtime's native
/// copy path, then freeze and verify it before the privileged install.
///
/// `pub` on purpose: this is the supported entry point for anything outside
/// this crate that needs its own binary inside a box devbox provisioned. The
/// staging, digest check, and privileged install are the contract — a caller
/// that copies bytes in by hand gets none of them.
pub async fn install_embedded_binary(
    runtime: &dyn Runtime,
    name: &str,
    binary_name: &str,
    payload: &[u8],
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let staging = tempfile::Builder::new()
        .prefix(&format!("{binary_name}-"))
        .tempdir()
        .with_context(|| format!("create private {binary_name} staging directory"))?;
    let temp = staging.path().join(binary_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("create staged agent {}", temp.display()))?;
    file.write_all(payload)
        .with_context(|| format!("write staged {binary_name} {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("flush staged agent {}", temp.display()))?;
    drop(file);

    let nonce = hex_bytes(&rand::random::<[u8; 16]>());
    let guest_stage = format!("/tmp/{binary_name}-{nonce}");
    let root_dir = format!("/run/devbox-install-{nonce}");
    let root_copy = format!("{root_dir}/{binary_name}");
    let expected_digest = sha256_hex(payload);

    // The runtime copy for Lima and Multipass runs as the guest login user.
    // Treat that file as hostile until root has frozen it in a directory the
    // guest cannot enter and the host has verified the embedded-byte digest.
    // A workload may win the race before the root copy, but then verification
    // fails; after the copy it cannot change the bytes that will be installed.
    let mut root_created = false;
    let install_result: Result<()> = async {
        copy_file_to_box(runtime, name, &temp, &guest_stage).await?;

        let made = runtime
            .exec_cmd(
                name,
                &["sudo", "mkdir", "-m", "0700", "--", &root_dir],
                false,
            )
            .await?;
        if made.exit_code != 0 {
            bail!(
                "create private agent install directory: {}",
                made.stderr.trim()
            );
        }
        root_created = true;

        let frozen = runtime
            .exec_cmd(
                name,
                &["sudo", "cp", "--", &guest_stage, &root_copy],
                false,
            )
            .await?;
        if frozen.exit_code != 0 {
            bail!("freeze staged {binary_name}: {}", frozen.stderr.trim());
        }

        let digest = runtime
            .exec_cmd(name, &["sudo", "sha256sum", "--", &root_copy], false)
            .await?;
        if digest.exit_code != 0 {
            bail!("verify staged {binary_name}: {}", digest.stderr.trim());
        }
        let actual_digest = digest.stdout.split_whitespace().next().unwrap_or_default();
        if actual_digest != expected_digest {
            bail!(
                "staged {binary_name} changed before privileged install (expected {expected_digest}, got {actual_digest})"
            );
        }

        let installed = runtime
            .exec_cmd(
                name,
                &[
                    "sudo",
                    "install",
                    "-o",
                    "root",
                    "-g",
                    "root",
                    "-m",
                    "0755",
                    "-D",
                    &root_copy,
                    &format!("/usr/local/bin/{binary_name}"),
                ],
                false,
            )
            .await?;
        if installed.exit_code != 0 {
            bail!("install {binary_name} in box: {}", installed.stderr.trim());
        }
        Ok(())
    }
    .await;

    // Best-effort on both success and failure. The root path is an exact,
    // random child of /run created above; no guest-controlled component is
    // accepted here.
    if root_created {
        let _ = runtime
            .exec_cmd(name, &["sudo", "rm", "-rf", "--", &root_dir], false)
            .await;
    }
    let _ = runtime
        .exec_cmd(name, &["rm", "-f", "--", &guest_stage], false)
        .await;
    install_result
}

pub(crate) fn sha256_hex(payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_bytes(&Sha256::digest(payload))
}

fn hex_bytes(payload: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(payload.len() * 2);
    for byte in payload {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

async fn copy_file_to_box(
    runtime: &dyn Runtime,
    name: &str,
    source: &std::path::Path,
    destination: &str,
) -> Result<()> {
    let source = source
        .to_str()
        .context("temporary agent path is not UTF-8")?;
    let instance = format!("devbox-{name}");
    let (program, args): (&str, Vec<String>) = match runtime.name() {
        "lima" => (
            "limactl",
            vec![
                "copy".into(),
                source.into(),
                format!("{instance}:{destination}"),
            ],
        ),
        "multipass" => (
            "multipass",
            vec![
                "transfer".into(),
                source.into(),
                format!("{instance}:{destination}"),
            ],
        ),
        "incus" => (
            "incus",
            vec![
                "file".into(),
                "push".into(),
                source.into(),
                format!("{instance}{destination}"),
            ],
        ),
        "docker" => (
            "docker",
            vec![
                "cp".into(),
                source.into(),
                format!("{instance}:{destination}"),
            ],
        ),
        other => bail!("runtime '{other}' has no host-to-box copy implementation"),
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::runtime::cmd::run_ok(program, &refs)
        .await
        .with_context(|| format!("copy embedded agent into box '{name}'"))?;
    Ok(())
}

pub(crate) async fn install_ubuntu_obsd_service(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let no_ebpf = if crate::obs::uses_ebpf(runtime.name()) {
        ""
    } else {
        " -no-ebpf"
    };
    let transport = if crate::obs::uses_host_socket(runtime.name()) {
        " -socket /run/devbox-host/obsd.sock"
    } else {
        " -no-transport"
    };
    // The same scope the console's exec agent gets, from the same probe: two
    // agents watching one box must not disagree about which paths produce
    // file events, or the feed changes shape depending on who attached.
    let file_scope = crate::obs::supervisor::guest_file_scope(runtime, name).await;
    let unit = format!(
        "[Unit]\nDescription=devbox observability agent\nAfter=network.target\n\n\
         [Service]\nType=simple\nExecStart=/usr/local/bin/devbox-obsd -box-id {name}{transport} \
         -packet=true -status-file /run/devbox/obsd-status.json \
         -file-scope {file_scope} \
         -policy /etc/devbox/policy.json{no_ebpf}\n\
         Restart=always\nRestartSec=2s\nRuntimeDirectory=devbox\n\
         CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN CAP_SYSLOG CAP_BPF CAP_PERFMON CAP_SYS_RESOURCE\n\
         AmbientCapabilities=CAP_NET_RAW CAP_NET_ADMIN CAP_SYSLOG CAP_BPF CAP_PERFMON CAP_SYS_RESOURCE\n\
         NoNewPrivileges=true\nProtectSystem=strict\nPrivateTmp=true\nMemoryMax=256M\nCPUQuota=25%\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    );
    write_file_to_vm(
        runtime,
        name,
        "/etc/systemd/system/devbox-obsd.service",
        &unit,
    )
    .await?;
    let result = runtime
        .exec_cmd(
            name,
            &["sudo", "systemctl", "enable", "--now", "devbox-obsd"],
            false,
        )
        .await?;
    if result.exit_code != 0 {
        bail!("enable devbox-obsd: {}", result.stderr.trim());
    }
    Ok(())
}

/// Wait for network connectivity inside the VM.
///
/// On freshly booted Incus VMs, the network (especially DNS) may not be ready
/// even after the agent responds. We first wait for basic IP connectivity
/// (ping), then check DNS resolution. If basic connectivity never comes up,
/// we bail early with actionable diagnostics instead of letting every
/// subsequent download time out.
async fn wait_for_network(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // Phase 1: Wait for basic IP connectivity (ping 8.8.8.8)
    // This distinguishes "network not ready yet" from "no route / firewall blocks"
    let ping_attempts = 10; // 10 * 3s = 30s
    let mut got_ping = false;
    for i in 0..ping_attempts {
        let result = run_in_vm(
            runtime,
            name,
            "ping -c 1 -W 2 8.8.8.8 >/dev/null 2>&1 && echo ok",
            false,
        )
        .await?;
        if result.exit_code == 0 && result.stdout.trim() == "ok" {
            got_ping = true;
            break;
        }
        if i == 0 {
            print!("Waiting for network connectivity...");
        } else if i % 5 == 0 {
            print!(" ({}s)", i * 3);
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    if !got_ping {
        println!();
        eprintln!("\x1b[31mError: VM has no network connectivity.\x1b[0m");
        eprintln!("The VM cannot reach the internet. This is usually caused by");
        eprintln!("missing iptables FORWARD rules for the Incus bridge.\n");
        eprintln!("Quick fix (run on the host):");
        eprintln!("  sudo iptables -I FORWARD -i incusbr0 -j ACCEPT");
        eprintln!(
            "  sudo iptables -I FORWARD -o incusbr0 -m state --state RELATED,ESTABLISHED -j ACCEPT"
        );
        eprintln!(
            "  sudo iptables -t nat -A POSTROUTING -s 10.195.64.0/24 ! -o incusbr0 -j MASQUERADE\n"
        );
        eprintln!("Run `devbox doctor` for full network diagnostics.");
        anyhow::bail!(
            "VM network connectivity check failed — cannot provision without internet access"
        );
    }

    // Phase 2: Wait for DNS resolution
    let dns_attempts = 10; // 10 * 3s = 30s
    for i in 0..dns_attempts {
        let result = run_in_vm(
            runtime,
            name,
            "getent hosts cache.nixos.org >/dev/null 2>&1 && echo ok",
            false,
        )
        .await?;
        if result.exit_code == 0 && result.stdout.trim() == "ok" {
            println!(" ready.");
            return Ok(());
        }
        if i % 5 == 0 {
            print!(" (DNS {}s)", i * 3);
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    println!();
    eprintln!(
        "Warning: DNS resolution not working yet — provisioning will continue but downloads may fail."
    );
    Ok(())
}

/// Ensure the NixOS channel is available so `nixos-rebuild` can find `<nixpkgs/nixos>`.
///
/// The `images:nixos/*` Incus images may not have channels configured, causing
/// `nixos-rebuild` to fail with "file 'nixpkgs/nixos' was not found in the Nix search path".
/// We check if the nixos channel exists and add it if missing.
async fn ensure_nixos_channel(runtime: &dyn Runtime, name: &str) -> Result<()> {
    // Check if the nixos channel is already available for root.
    // We check the channel profile path directly since NIX_PATH may not be set
    // in the non-login incus exec shell.
    let check = run_in_vm(
        runtime,
        name,
        "test -d /nix/var/nix/profiles/per-user/root/channels/nixos && echo found",
        false,
    )
    .await?;

    if check.stdout.trim() == "found" {
        return Ok(());
    }

    println!("Setting up NixOS channel (required for nixos-rebuild)...");
    let channel_cmd = concat!(
        "nix-channel --add https://nixos.org/channels/nixos-25.05 nixos && ",
        "nix-channel --update"
    );
    let result = run_in_vm(runtime, name, channel_cmd, true).await?;
    if result.exit_code != 0 {
        eprintln!(
            "Warning: failed to set up NixOS channel: {}",
            result.stderr.trim()
        );
    } else {
        println!("NixOS channel configured.");
    }

    Ok(())
}

/// Ensure /etc/nixos/configuration.nix and hardware-configuration.nix exist.
///
/// NixOS Lima images ship with an empty /etc/nixos/ directory.
/// We run `nixos-generate-config` to create hardware-configuration.nix,
/// then write our own minimal configuration.nix with correct bootloader
/// settings and the devbox module import already included.
pub(crate) async fn ensure_nixos_config(
    runtime: &dyn Runtime,
    name: &str,
    enable_obsd_service: bool,
) -> Result<()> {
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
        let result = run_in_vm(runtime, name, "nixos-generate-config", false).await?;
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

    // The same scope `install_ubuntu_obsd_service` writes into its unit, from
    // the same probe. The NixOS unit had been taking the agent's own default
    // of `/workspace` alone, so a NixOS box and an Ubuntu box reported
    // different file events for identical work — and the console's exec agent,
    // which has always been given the probed scope, disagreed with the service
    // running beside it. `scope_with_home` validates the value, so what lands
    // in the Nix string is one absolute path list and nothing else.
    let file_scope = crate::obs::supervisor::guest_file_scope(runtime, name).await;

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
    /etc/devbox/obsd-module.nix
  ];

  services.devbox-obsd = {{
    enable = {enable_obsd_service};
    boxId = {box_id:?};
    socket = "/run/devbox-host/obsd.sock";
    noTransport = {no_transport};
    enableEbpf = {enable_ebpf};
    fileScope = {file_scope:?};
  }};

{bootloader_config}

  # ── Lima's own mounts ──────────────────────────────
  #
  # `nixos-generate-config` records whatever is mounted when it runs, by UUID.
  # For the cidata ISO that is a guarantee of failure: Lima rebuilds
  # `cidata.iso` on every `limactl start`, and an iso9660 volume's UUID *is*
  # its creation timestamp, so the device pinned at provision time never exists
  # again. `local-fs.target` then fails and the box comes up in emergency mode
  # with no sshd — reachable never again, from the user's point of view.
  #
  # Measured on a box built without this override, from its own journal:
  #
  #   Timed out waiting for device /dev/disk/by-uuid/2026-09-05-22-26-41-44.
  #   Dependency failed for /mnt/lima-cidata.
  #   Dependency failed for Local File Systems.
  #   Reached target Emergency Mode.
  #
  # A plain Lima VM never hits this because Lima's own `/etc/fstab` names the
  # volume by label and is rewritten on every boot. `nixos-rebuild` is what
  # takes that self-healing away: it makes `/etc/fstab` a read-only symlink
  # into the store, so Lima can no longer correct it.
  #
  # The label is stable across regeneration. `nofail` is the belt to that
  # brace: a Lima mount that is missing for any other reason should leave the
  # box bootable and diagnosable, not dead.
  fileSystems."/mnt/lima-cidata" = lib.mkForce {{
    device = "/dev/disk/by-label/cidata";
    fsType = "auto";
    options = [ "ro" "nofail" "x-systemd.device-timeout=5s" ];
  }};

  # Networking
  networking.networkmanager.enable = true;

  # OpenSSH for Lima access
  services.openssh.enable = true;

  # NixOS state version — matches the pre-built image
  system.stateVersion = lib.mkDefault "25.11";
}}
"#,
        box_id = name,
        enable_obsd_service = enable_obsd_service,
        no_transport = !crate::obs::uses_host_socket(runtime.name()),
        enable_ebpf = crate::obs::uses_ebpf(runtime.name()),
        file_scope = file_scope,
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
        if let Ok(r) = result
            && r.exit_code == 0
        {
            let _ = run_in_vm(
                runtime,
                name,
                "install -m 755 /tmp/devbox /usr/local/bin/devbox",
                false,
            )
            .await;
            let _ = runtime.exec_cmd(name, &["rm", "/tmp/devbox"], false).await;
        }
    }
    Ok(())
}

/// Push yazi config files to all user home directories in the VM.
async fn setup_yazi_config(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) -> Result<()> {
    let config_dir = format!("{vm_home}/.config/yazi");

    // Create config directory
    run_in_vm(runtime, name, &format!("mkdir -p {config_dir}"), false).await?;

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
    run_in_vm(runtime, name, &format!("mkdir -p {plugin_dir}"), false).await?;
    write_file_to_vm(
        runtime,
        name,
        &format!("{plugin_dir}/init.lua"),
        YAZI_GLOW_PLUGIN,
    )
    .await?;

    // Fix ownership
    run_in_vm(
        runtime,
        name,
        &format!("chown -R {vm_user}:users {vm_home}/.config/yazi"),
        false,
    )
    .await?;

    Ok(())
}

/// Push aichat config (roles) to user home directory in the VM.
/// Writes both legacy roles.yaml and modern roles/*.md format for compatibility.
async fn setup_aichat_config(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) -> Result<()> {
    let config_dir = format!("{vm_home}/.config/aichat");
    let roles_dir = format!("{config_dir}/roles");

    run_in_vm(runtime, name, &format!("mkdir -p {roles_dir}"), false).await?;

    // Legacy format (older aichat versions)
    write_file_to_vm(
        runtime,
        name,
        &format!("{config_dir}/roles.yaml"),
        AICHAT_ROLES,
    )
    .await?;

    // Modern format: individual .md files in roles/ directory
    let role_files: &[(&str, &str)] = &[
        ("architect.md", AICHAT_ROLE_ARCHITECT),
        ("reviewer.md", AICHAT_ROLE_REVIEWER),
    ];
    for (filename, content) in role_files {
        write_file_to_vm(runtime, name, &format!("{roles_dir}/{filename}"), content).await?;
    }

    run_in_vm(
        runtime,
        name,
        &format!("chown -R {vm_user}:users {config_dir}"),
        false,
    )
    .await?;

    Ok(())
}

/// Push the management panel script to /etc/devbox/management.sh inside the VM.
async fn setup_management_script(runtime: &dyn Runtime, name: &str) -> Result<()> {
    write_file_to_vm(
        runtime,
        name,
        "/etc/devbox/management.sh",
        MANAGEMENT_SCRIPT,
    )
    .await?;
    run_in_vm(runtime, name, "chmod +x /etc/devbox/management.sh", false).await?;
    Ok(())
}

/// Install the latest claude-code via npm with a writable prefix.
///
/// On NixOS, the official binary installer fails (non-standard dynamic linker),
/// and `npm install -g` fails (Nix store is read-only). We work around this by
/// setting NPM_CONFIG_PREFIX to ~/.npm-global, then adding that to PATH.
async fn install_latest_claude_code(
    runtime: &dyn Runtime,
    name: &str,
    vm_user: &str,
    vm_home: &str,
) {
    println!("Installing latest claude-code...");

    // Install via npm with a writable global prefix.
    // We explicitly set HOME to the VM user's home directory because on
    // Incus exec_cmd runs as root ($HOME=/root). We need claude installed
    // to the user's home so Zellij panes (which run as user) find it.
    let install_cmd = format!(
        "export HOME={vm_home}; \
         export PATH=\"{vm_home}/.nix-profile/bin:/run/current-system/sw/bin:$PATH\"; \
         export NPM_CONFIG_PREFIX=\"{vm_home}/.npm-global\"; \
         mkdir -p \"{vm_home}/.npm-global\"; \
         if ! command -v npm >/dev/null 2>&1; then \
           echo 'npm not found, installing nodejs via nix-env...'; \
           nix-env -iA nixos.nodejs_22 2>&1; \
           export PATH=\"{vm_home}/.nix-profile/bin:$PATH\"; \
         fi; \
         echo \"Using npm: $(which npm 2>/dev/null || echo 'not found')\"; \
         if command -v npm >/dev/null 2>&1; then \
           npm install -g @anthropic-ai/claude-code@latest 2>&1; \
           echo \"Installed: $({vm_home}/.npm-global/bin/claude --version 2>/dev/null || echo 'failed')\"; \
         else \
           echo 'ERROR: npm still not available after nix-env install'; \
         fi"
    );
    let result = runtime
        .exec_cmd(name, &["bash", "-lc", &install_cmd], true)
        .await;
    match result {
        Ok(r) if r.exit_code == 0 => {
            println!("claude-code installed (latest).");
        }
        _ => {
            eprintln!("Warning: could not install latest claude-code. Using nixpkgs version.");
        }
    }

    // Fix ownership of installed files (may have been created as root on Incus)
    let _ = run_in_vm(
        runtime, name,
        &format!("chown -R {vm_user}:users {vm_home}/.npm-global {vm_home}/.nix-profile 2>/dev/null; true"),
        false,
    ).await;

    // Ensure ~/.npm-global/bin is at front of PATH in both .zshrc and .profile
    // so latest claude takes precedence over the nixpkgs system version.
    let path_line =
        r#"export PATH="$HOME/.npm-global/bin:$HOME/.local/bin:$HOME/.claude/bin:$PATH""#;
    for rc_file in &[".zshrc", ".profile"] {
        let rc_path = format!("{vm_home}/{rc_file}");
        let add_path_cmd = format!(
            "grep -qF '.npm-global/bin' {rc_path} 2>/dev/null || \
             echo '{path_line}' >> {rc_path}"
        );
        let _ = runtime
            .exec_cmd(name, &["bash", "-c", &add_path_cmd], false)
            .await;
    }
    // Fix ownership
    let _ = run_in_vm(
        runtime,
        name,
        &format!("chown {vm_user}:users {vm_home}/.zshrc {vm_home}/.profile 2>/dev/null; true"),
        false,
    )
    .await;
}

/// Write embedded help files to /etc/devbox/help/ inside the VM.
async fn setup_help_in_vm(runtime: &dyn Runtime, name: &str) -> Result<()> {
    for (help_name, content) in super::super::cli::help::CHEAT_SHEETS.iter() {
        let path = format!("/etc/devbox/help/{help_name}.md");
        write_file_to_vm(runtime, name, &path, content).await?;
    }
    Ok(())
}

/// Returns the host username (for state TOML and NixOS user creation).
fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "dev".to_string())
}

/// The name of the ordinary user inside the box.
///
/// Asked of the box rather than deduced from its `/etc/passwd`. The scan this
/// used to be — the first account with a UID in 1000..65534 whose home is
/// under `/home` — never matched on Lima, which gives its guest user the
/// *host's* uid (501 on a Mac). Every Lima box therefore fell through to the
/// host's own `$USER` and was right only by coincidence: a box whose account
/// is not named after the person running devbox (Lima's `<user>.linux`, an
/// image that ships its own account) got the wrong name, and with it the wrong
/// home, the wrong `chown` target, and a [`pick_vm_home`] identity check that
/// could no longer match — which is the failure W2-3 fixed the *home* half of
/// and left this half of.
///
/// `id -un` only answers the right question where `exec_cmd` runs as the box
/// user, which is what [`Runtime::exec_runs_as_root`] reports. Where it runs
/// as root the answer is `root`, which is not the account devbox provisions —
/// so there, and wherever a box says `root` anyway because a runtime can be
/// wrong about itself, the passwd scan is still the best guess available.
async fn detect_vm_username(runtime: &dyn Runtime, name: &str) -> String {
    if !runtime.exec_runs_as_root() {
        let probe = "printf 'devbox-user=%s\\n' \"$(id -un 2>/dev/null)\"";
        if let Ok(result) = runtime.exec_cmd(name, &["sh", "-lc", probe], false).await
            && result.exit_code == 0
            && let Some(user) = usable_username(&marker_field(&result.stdout, "devbox-user="))
        {
            return user;
        }
    }

    // Filters: UID 1000-65533, home under /home/ (excludes NixOS nixbld* users
    // which have UID 30001+ but home /var/empty).
    let scan = runtime
        .exec_cmd(
            name,
            &["bash", "-lc", "awk -F: '$3 >= 1000 && $3 < 65534 && $6 ~ /^\\/home\\// { print $1; exit }' /etc/passwd"],
            false,
        )
        .await;
    if let Ok(result) = scan
        && let Some(user) = usable_username(result.stdout.trim())
    {
        return user;
    }

    usable_username(&whoami()).unwrap_or_else(|| "dev".to_string())
}

/// One `devbox-<key>=<value>` line out of a guest probe's stdout.
///
/// The probes run under a login shell, so a profile that prints a banner would
/// otherwise be parsed as the answer. Shared by the username and home probes
/// because they are the same trick and drifted apart once already.
fn marker_field(stdout: &str, key: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(key))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A guest username devbox is willing to act on, or `None`.
///
/// Two jobs. The first is rejecting answers that are not the account we mean:
/// nothing at all, or `root` — devbox provisions an ordinary user, and writing
/// its gitconfig and AI tool settings into `/root` (or `chown root:users`-ing
/// them) is not a degraded result, it is a wrong one.
///
/// The second is that this name is interpolated straight into guest shell
/// commands — `chown {user}:users {path}`, `getent passwd {user}` — and it now
/// comes from *inside* the box rather than from the host environment. The
/// character set is the portable one plus `.`, which Lima needs for
/// `<user>.linux`; anything else cannot be a username and must not become
/// shell syntax.
fn usable_username(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == "root" || candidate.len() > 64 {
        return None;
    }
    if !candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(candidate.to_string())
}

/// Detect the home directory for a given username inside the VM.
///
/// Asks the guest for three things in one command — who the shell is running
/// as, what `$HOME` is, and what `/etc/passwd` says — because on Lima those
/// last two disagree and the file everything reads is the one `$HOME` names.
/// See [`pick_vm_home`] for which wins.
///
/// Markers rather than bare lines: this is a login shell, and a profile that
/// prints a banner would otherwise be parsed as the answer.
async fn detect_vm_home(runtime: &dyn Runtime, name: &str, username: &str) -> String {
    let probe = format!(
        "printf 'devbox-user=%s\\n' \"$(id -un 2>/dev/null)\"; \
         printf 'devbox-home=%s\\n' \"$([ -d \"$HOME\" ] && echo \"$HOME\")\"; \
         printf 'devbox-passwd=%s\\n' \"$(getent passwd {username} | cut -d: -f6)\""
    );
    let result = runtime
        .exec_cmd(name, &["bash", "-lc", &probe], false)
        .await;
    let stdout = match result {
        Ok(r) => r.stdout,
        Err(_) => String::new(),
    };
    pick_vm_home(
        username,
        &marker_field(&stdout, "devbox-user="),
        &marker_field(&stdout, "devbox-home="),
        &marker_field(&stdout, "devbox-passwd="),
    )
}

/// Which of the guest's two answers is the home devbox should write into.
///
/// The login shell's `$HOME` wins, when the shell is running as the user we
/// asked about. Lima gives its guest user the *host's* uid and a home of
/// `/home/<user>.guest` that the passwd entry does not name — measured on
/// devtest, where `getent passwd ethan` says `/home/ethan` while every shell
/// in the box has `HOME=/home/ethan.guest`, sshd finds `authorized_keys`
/// under the latter, and provisioning had been writing `.gitconfig`, `.zshrc`
/// and the AI tool settings into the former, where nothing ever read them.
///
/// The identity check is what makes this safe on the other runtimes: Incus and
/// Docker exec as root, so their `$HOME` is `/root` and says nothing about the
/// box user. There, `id -un` is not `username` and passwd — which is right on
/// those runtimes — is used instead.
///
/// `shell_home` arrives empty unless the guest confirmed it is a directory —
/// `write_file_to_vm` does not create parents, so a `$HOME` that does not
/// exist yet would turn every settings write into a failure rather than into a
/// misplaced file.
///
/// Pure, so the decision can be tested without a hypervisor; the probe above
/// is the only part that needs one.
fn pick_vm_home(username: &str, shell_user: &str, shell_home: &str, passwd_home: &str) -> String {
    // A home becomes a path prefix for a dozen guest writes, so anything that
    // is not a single absolute path is treated as no answer at all.
    let usable =
        |path: &str| path.starts_with('/') && path != "/" && path.split_whitespace().count() == 1;

    if shell_user == username && usable(shell_home) {
        return shell_home.to_string();
    }
    if usable(passwd_home) {
        return passwd_home.to_string();
    }
    format!("/home/{username}")
}

// Overlay mount is now handled declaratively by devbox-module.nix via
// fileSystems."/workspace" when mount_mode = "overlay" in devbox-state.toml.
// The nixos-rebuild switch creates the systemd mount automatically.

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The box shape most of these tests do not care about.
    fn shape(
        user: &str,
        home: Option<&str>,
        runtime: &str,
        mount_mode: &str,
    ) -> crate::nix::sets::GuestShape {
        crate::nix::sets::GuestShape {
            user: Some(user.to_string()),
            home: home.map(str::to_string),
            runtime: Some(runtime.to_string()),
            mount_mode: Some(mount_mode.to_string()),
            workspace_nofail: false,
        }
    }

    use crate::runtime::SandboxStatus;
    use crate::runtime::stub::StubRuntime;

    /// A box that answers the username probe however the test says.
    ///
    /// `root_exec` is what `Runtime::exec_runs_as_root` reports; `id_un` is the
    /// stdout of the `id -un` probe, and `passwd_scan` that of the /etc/passwd
    /// scan.
    fn stub_guest(root_exec: bool, id_un: &'static str, passwd_scan: &'static str) -> StubRuntime {
        StubRuntime::new()
            .with_name("stub")
            .with_exec_runs_as_root(root_exec)
            .with_status(SandboxStatus::Running)
            .with_exec_cmd(move |_: &str, cmd: &[&str], _: bool| {
                let joined = cmd.join(" ");
                let stdout = if joined.contains("id -un") {
                    id_un.to_string()
                } else if joined.contains("/etc/passwd") {
                    passwd_scan.to_string()
                } else {
                    String::new()
                };
                Ok(ExecResult {
                    exit_code: 0,
                    stdout,
                    stderr: String::new(),
                })
            })
    }

    fn probe(lines: &str) -> BirthProbe {
        parse_birth_probe(lines)
    }

    /// A box being born: no state file, no `/workspace` anywhere. The only
    /// moment the mount options are free.
    #[test]
    fn a_box_with_no_state_and_no_workspace_gets_nofail() {
        let p = probe("state=absent\nfstab=no\nmounted=no\nprobe=ok\n");
        assert!(grant_workspace_nofail(&p));
    }

    /// The case W4-2 got wrong from the other direction: no state file, but
    /// the box plainly already has a workspace. Granting here would change a
    /// mounted overlay's options, which overlayfs refuses and NixOS rolls the
    /// whole generation back over.
    #[test]
    fn a_box_with_no_state_but_a_workspace_gets_nothing() {
        assert!(!grant_workspace_nofail(&probe(
            "state=absent\nfstab=yes\nmounted=no\nprobe=ok\n"
        )));
        assert!(!grant_workspace_nofail(&probe(
            "state=absent\nfstab=no\nmounted=yes\nprobe=ok\n"
        )));
    }

    /// The bug this whole change exists for. `limactl shell` answers a VM that
    /// is not up, or an ssh stutter, with a non-zero exit and no output — not
    /// with an `Err`. Without the marker that reads exactly like "no state
    /// file", and one unlucky moment during a reprovision writes the key onto
    /// a box that must never have it.
    #[test]
    fn a_probe_that_did_not_answer_grants_nothing() {
        // What an ssh failure actually looks like: empty stdout.
        assert!(!grant_workspace_nofail(&probe("")));
        // And a half-answer, cut off before the marker.
        assert!(!grant_workspace_nofail(&probe("state=absent\nfstab=no\n")));
        // Even one that would otherwise have said yes.
        assert!(!grant_workspace_nofail(&probe(
            "state=absent\nfstab=no\nmounted=no\n"
        )));
    }

    /// A box that has been through this keeps what it decided, either way.
    #[test]
    fn a_box_that_already_decided_is_not_asked_again() {
        assert!(grant_workspace_nofail(&probe(
            "state=present\nnofail=true\nfstab=yes\nmounted=yes\nprobe=ok\n"
        )));
        assert!(!grant_workspace_nofail(&probe(
            "state=present\nnofail=\nfstab=yes\nmounted=yes\nprobe=ok\n"
        )));
    }

    /// The state file and the mount are a record and the thing recorded. When
    /// they disagree the box is one rebuild from being unrebuildable, so the
    /// rebuild does not happen.
    #[test]
    fn a_state_file_that_disagrees_with_fstab_stops_the_rebuild() {
        let says_yes = refuse_on_workspace_mismatch(
            "devtest",
            &probe("state=present\nnofail=true\nfstab=yes\nfstabnofail=no\nprobe=ok\n"),
        )
        .expect_err("a record that disagrees with fstab is refused");
        assert!(says_yes.to_string().contains("'nofail'"), "{says_yes}");
        assert!(says_yes.to_string().contains("without it"), "{says_yes}");

        let says_no = refuse_on_workspace_mismatch(
            "devtest",
            &probe("state=present\nnofail=\nfstab=yes\nfstabnofail=yes\nprobe=ok\n"),
        )
        .expect_err("and so is the other direction");
        assert!(says_no.to_string().contains("'no nofail'"), "{says_no}");
        assert!(says_no.to_string().contains("with it"), "{says_no}");

        for message in [says_yes.to_string(), says_no.to_string()] {
            assert!(message.contains("devtest"), "{message}");
            assert!(message.contains("edited by hand"), "{message}");
            // rustfmt rejoins a `\`-continued literal and leaves its
            // indentation in the text; this is what notices.
            assert!(!message.contains("  "), "{message}");
        }
    }

    /// The comparison is against `/etc/fstab`, never the running mount.
    /// `nofail` is an fstab option and never reaches the kernel, so a box
    /// built *with* it reports mount options *without* it — measured on a
    /// fresh box:
    /// `rw,relatime,lowerdir=…,upperdir=…,workdir=…,uuid=on`. Comparing the
    /// key against that would call every such box a mismatch and refuse every
    /// rebuild it ever needed.
    #[test]
    fn agreement_and_a_box_with_no_workspace_line_both_pass() {
        for stdout in [
            // key and fstab agree, both ways
            "state=present\nnofail=true\nfstab=yes\nfstabnofail=yes\nprobe=ok\n",
            "state=present\nnofail=\nfstab=yes\nfstabnofail=no\nprobe=ok\n",
            // no /workspace declared at all: every box being provisioned
            "state=absent\nfstab=no\nmounted=no\nprobe=ok\n",
            // and a probe that did not answer must not refuse anything either
            "",
        ] {
            assert!(
                refuse_on_workspace_mismatch("devtest", &probe(stdout)).is_ok(),
                "{stdout:?}"
            );
        }
    }

    /// The Lima case, and the whole reason for the change: the guest user
    /// carries the host's uid, so the passwd scan finds nobody, and the name
    /// devbox needs is the one the box itself reports.
    #[tokio::test]
    async fn the_box_is_asked_who_it_is_rather_than_scanned_for() {
        // The passwd scan comes back empty, the way every Lima box answers it.
        let guest = stub_guest(false, "devbox-user=ethan.linux\n", "");
        assert_eq!(detect_vm_username(&guest, "devtest").await, "ethan.linux");
    }

    /// Incus execs as root. Asking `id -un` there would name the exec, not the
    /// account devbox provisions, so the scan is what it uses.
    #[tokio::test]
    async fn a_root_exec_runtime_is_not_asked_id_un_at_all() {
        // `id -un` would win if it were consulted. It must not be.
        let guest = stub_guest(true, "devbox-user=root\n", "dev\n");
        assert_eq!(detect_vm_username(&guest, "devtest").await, "dev");
    }

    /// A runtime can be wrong about itself — Docker says its exec runs as the
    /// user and the image's default account is commonly root.
    #[tokio::test]
    async fn a_box_that_answers_root_falls_through_to_the_scan() {
        let guest = stub_guest(false, "devbox-user=root\n", "dev\n");
        assert_eq!(detect_vm_username(&guest, "devtest").await, "dev");
    }

    /// A login shell that greets the user must not have its banner mistaken
    /// for the answer — which is what the marker is for.
    #[tokio::test]
    async fn a_chatty_login_shell_does_not_rename_the_user() {
        let guest = stub_guest(
            false,
            "Welcome to NixOS!\nLast login: Fri\ndevbox-user=ethan\n",
            "dev\n",
        );
        assert_eq!(detect_vm_username(&guest, "devtest").await, "ethan");
    }

    #[test]
    fn a_username_is_something_that_can_be_a_username() {
        for good in ["ethan", "ethan.linux", "dev", "user-1", "_svc", "ubuntu"] {
            assert_eq!(usable_username(good).as_deref(), Some(good), "{good}");
        }
        // `root` is not a degraded answer, it is the wrong account.
        for bad in ["", "   ", "root", "root\n"] {
            assert_eq!(usable_username(bad), None, "{bad:?}");
        }
        // This name is interpolated into `chown {user}:users {path}` in the
        // guest, and it now comes from inside the box.
        for hostile in [
            "dev; rm -rf /",
            "dev users",
            "../root",
            "a:b",
            "$(id -un)",
            "dev\ttab",
        ] {
            assert_eq!(usable_username(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn a_marker_is_read_off_whatever_line_it_lands_on() {
        let stdout = "motd\ndevbox-user=ethan\ndevbox-home=/home/ethan.guest\n";
        assert_eq!(marker_field(stdout, "devbox-user="), "ethan");
        assert_eq!(marker_field(stdout, "devbox-home="), "/home/ethan.guest");
        assert_eq!(marker_field(stdout, "devbox-passwd="), "");
        assert_eq!(marker_field("", "devbox-user="), "");
    }

    /// Captured from devtest (Lima 2.x, vmType vz, NixOS): the guest user has
    /// the host's uid 501, so passwd and the shell disagree about home and the
    /// shell is the one that is right.
    #[test]
    fn lima_home_comes_from_the_login_shell_not_from_passwd() {
        assert_eq!(
            pick_vm_home("ethan", "ethan", "/home/ethan.guest", "/home/ethan"),
            "/home/ethan.guest",
        );
    }

    /// Incus and Docker exec as root, so `$HOME` is `/root` and describes the
    /// exec, not the box user. Preferring it there would put the gitconfig and
    /// every AI tool setting in root's home.
    #[test]
    fn a_root_exec_does_not_donate_its_home_to_the_box_user() {
        assert_eq!(
            pick_vm_home("dev", "root", "/root", "/home/dev"),
            "/home/dev",
        );
    }

    #[test]
    fn passwd_is_the_fallback_when_the_shell_says_nothing_useful() {
        // No `$HOME` at all, and a `$HOME` that is not one absolute path.
        assert_eq!(pick_vm_home("dev", "dev", "", "/home/dev"), "/home/dev");
        assert_eq!(pick_vm_home("dev", "dev", "/", "/home/dev"), "/home/dev");
        assert_eq!(
            pick_vm_home("dev", "dev", "/home/a b", "/home/dev"),
            "/home/dev",
        );
        assert_eq!(
            pick_vm_home("dev", "dev", "relative/path", "/home/dev"),
            "/home/dev",
        );
    }

    /// A box that answers nothing at all still gets a home rather than an
    /// empty prefix that would turn `{home}/.gitconfig` into `/.gitconfig`.
    #[test]
    fn a_silent_box_falls_back_to_the_conventional_path() {
        assert_eq!(pick_vm_home("dev", "", "", ""), "/home/dev");
        assert_eq!(pick_vm_home("dev", "dev", "", "not-a-path"), "/home/dev");
    }

    #[test]
    fn a_nonzero_guest_install_is_a_provisioning_error() {
        let result = ExecResult {
            exit_code: 17,
            stdout: String::new(),
            stderr: "the selected package did not build".into(),
        };
        let error = require_install_success(
            &result,
            "nixos-rebuild switch",
            "devbox exec demo -- sudo nixos-rebuild switch",
        )
        .expect_err("a launched guest command with a non-zero exit is not success");
        let message = error.to_string();
        assert!(message.contains("exit code 17"));
        assert!(message.contains("selected package did not build"));
        assert!(message.contains("Retry with"));
    }

    #[test]
    fn embedded_agent_digest_is_stable_sha256() {
        let digest = sha256_hex(b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn install_nonce_is_path_safe_hex() {
        let encoded = hex_bytes(&[0x00, 0xab, 0xff]);
        assert_eq!(encoded, "00abff");
        assert!(encoded.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

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
        let toml = generate_state_toml(
            &sets,
            &langs,
            &shape("testuser", Some("/home/testuser.guest"), "lima", "overlay"),
            &[],
        );

        assert!(toml.contains("name = \"testuser\""));
        // The home the box actually uses, so `devbox-module.nix` can declare
        // it and NixOS stops re-homing the passwd entry onto /home/<name>.
        assert!(toml.contains("home = \"/home/testuser.guest\""), "{toml}");
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
        let toml = generate_state_toml(&sets, &langs, &shape("dev", None, "lima", "overlay"), &[]);

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
        let toml = generate_state_toml(&sets, &langs, &shape("dev", None, "lima", "overlay"), &[]);

        assert!(toml.contains("ai_code = true"));
        assert!(toml.contains("ai_infra = false"));
    }

    /// The repair rewrites one key and must not disturb the rest: the sets,
    /// the languages and the ad-hoc package table all decide what the next
    /// rebuild installs.
    #[test]
    fn recording_the_home_leaves_the_rest_of_the_state_file_alone() {
        let before = generate_state_toml(
            &["system".to_string(), "shell".to_string()],
            &["go".to_string()],
            &shape("ethan", None, "incus", "writable"),
            &["ripgrep".to_string()],
        );
        let after = with_user_home(&before, "/home/ethan.guest").expect("rewritten");

        let parsed: toml::Value = after.parse().expect("valid TOML");
        assert_eq!(parsed["user"]["home"].as_str(), Some("/home/ethan.guest"));
        assert_eq!(parsed["user"]["name"].as_str(), Some("ethan"));
        assert_eq!(parsed["sandbox"]["mount_mode"].as_str(), Some("writable"));
        assert_eq!(parsed["sets"]["system"].as_bool(), Some(true));
        assert_eq!(parsed["sets"]["network"].as_bool(), Some(false));
        assert_eq!(parsed["languages"]["go"].as_bool(), Some(true));
        assert!(
            parsed["custom_packages"].get("ripgrep").is_some(),
            "{after}"
        );
    }

    /// Idempotent, because the drift check runs on every box entry and a box
    /// that has already been repaired must not be rewritten forever.
    #[test]
    fn recording_the_home_twice_says_the_same_thing() {
        let base = generate_state_toml(&[], &[], &shape("ethan", None, "lima", "overlay"), &[]);
        let once = with_user_home(&base, "/home/ethan.guest").unwrap();
        let twice = with_user_home(&once, "/home/ethan.guest").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_state_file_that_is_not_toml_is_refused_rather_than_replaced() {
        assert!(with_user_home("this is not = = toml", "/home/x").is_err());
    }

    /// The module gates the Incus guest agent on this key. On a Lima box that
    /// agent has no host to talk to, fails, and is restarted every five
    /// seconds for the life of the box.
    #[test]
    fn the_state_file_records_which_hypervisor_the_box_runs_under() {
        for runtime in ["lima", "incus", "docker"] {
            let toml = generate_state_toml(&[], &[], &shape("dev", None, runtime, "overlay"), &[]);
            let parsed: toml::Value = toml.parse().expect("the state file is valid TOML");
            assert_eq!(parsed["sandbox"]["runtime"].as_str(), Some(runtime));
            assert_eq!(parsed["sandbox"]["mount_mode"].as_str(), Some("overlay"));
        }
    }

    /// A box that could not be asked gets no `home` key at all. Writing a
    /// guessed one would move the passwd entry away from wherever the keys
    /// really are, which is strictly worse than the NixOS default.
    #[test]
    fn a_home_that_is_not_known_is_left_out_rather_than_guessed() {
        let toml = generate_state_toml(&[], &[], &shape("dev", None, "lima", "overlay"), &[]);
        assert!(toml.contains("name = \"dev\""), "{toml}");
        assert!(!toml.contains("home ="), "{toml}");
        // The section still has to parse: `[user]` then a blank line then
        // `[sets]`, with or without the home in between.
        let parsed: toml::Value = toml.parse().expect("the state file is valid TOML");
        assert!(parsed["user"].get("home").is_none());
        assert_eq!(parsed["user"]["name"].as_str(), Some("dev"));
    }

    #[test]
    fn a_known_home_round_trips_through_the_state_file() {
        let toml = generate_state_toml(
            &[],
            &[],
            &shape("ethan", Some("/home/ethan.guest"), "lima", "overlay"),
            &[],
        );
        let parsed: toml::Value = toml.parse().expect("the state file is valid TOML");
        assert_eq!(parsed["user"]["home"].as_str(), Some("/home/ethan.guest"));
        assert_eq!(parsed["user"]["name"].as_str(), Some("ethan"));
    }

    #[test]
    fn generate_state_toml_bare() {
        let sets = vec![];
        let langs = vec![];
        let toml = generate_state_toml(&sets, &langs, &shape("user", None, "lima", "overlay"), &[]);

        assert!(toml.contains("system = false"));
        assert!(toml.contains("go = false"));
        assert!(toml.contains("name = \"user\""));
    }

    #[test]
    fn a_flake_reference_cannot_carry_shell_syntax() {
        // The regression this exists for: validating only the fragment after
        // `#` let a command sit in front of it, and the whole value is joined
        // unquoted into `bash -c "nix profile install …"`.
        for hostile in [
            "github:user/repo; touch /tmp/pwn; #pkg",
            "$(reboot)#pkg",
            "nixpkgs#pkg; id",
            "`id`#pkg",
            "nixpkgs #pkg",
            "nixpkgs#pkg\nid",
        ] {
            assert!(
                !is_safe_installable(hostile),
                "{hostile:?} must be rejected"
            );
        }
        for ok in [
            "nixpkgs#ripgrep",
            "nixpkgs#python312Packages.ipython",
            "github:user/repo#pkg",
            "git+https://example.com/r.git?ref=main#pkg",
            "ripgrep",
        ] {
            assert!(is_safe_installable(ok), "{ok:?} is a real reference");
        }
    }

    #[test]
    fn a_custom_package_name_cannot_carry_shell_syntax() {
        // `[custom_packages]` is hand-written and its keys are interpolated
        // unquoted into `bash -c "nix profile install …"` on the Ubuntu path.
        use crate::nix::compose::is_valid_attr_path;
        for hostile in [
            "foo; touch /tmp/pwned; #",
            "$(reboot)",
            "`id`",
            "a b",
            "../../etc/passwd",
            "foo\nbar",
        ] {
            assert!(!is_valid_attr_path(hostile), "{hostile:?} must be rejected");
        }
        // And the shapes a real attribute path takes still pass.
        for ok in [
            "ripgrep",
            "python312Packages.ipython",
            "nodePackages_latest.pnpm",
        ] {
            assert!(is_valid_attr_path(ok), "{ok:?} is a real package");
        }
    }

    #[test]
    fn a_nixpkgs_fragment_is_what_the_module_gets_asked_for() {
        // `my-tf = "nixpkgs#terraform"` passed the source check and then had
        // its source discarded, so the module was asked for `my-tf`.
        // `lib.attrByPath` resolved that to null and null is filtered out — on
        // purpose, so one stale name cannot fail a whole rebuild — which is why
        // nothing complained. The package was accepted, recorded as selected,
        // displayed as selected, and never installed.
        assert_eq!(nixos_attr_path("my-tf", "nixpkgs#terraform"), "terraform");
        assert_eq!(
            nixos_attr_path("ipython", "nixpkgs#python312Packages.ipython"),
            "python312Packages.ipython"
        );
        // A plain nixpkgs package is still named by its key.
        assert_eq!(nixos_attr_path("ripgrep", "nixpkgs"), "ripgrep");
    }

    #[test]
    fn an_ubuntu_box_refuses_a_malformed_reference_before_it_exists() {
        // Ubuntu installs a flake reference directly, so any *source* is
        // supported — but it still has to be one `nix profile install` will
        // take. That was checked only in `provision_ubuntu`, after
        // `runtime.create`, where the failure is downgraded to a warning and
        // state is saved anyway: `devbox create` reported a box it had made,
        // carrying a package it had not installed, with nothing the user would
        // see. Round 24 moved this check ahead of creation for NixOS and left
        // the Ubuntu path returning `Ok` on the way past.
        let hostile = [("tool".to_string(), "github:user/repo;touch#pkg".to_string())];
        assert!(
            check_packages_supported("ubuntu", &hostile).is_err(),
            "a reference with shell syntax must not reach the box"
        );

        // The references Ubuntu really does take still pass.
        for ok in ["nixpkgs#ripgrep", "github:owner/repo#tool", "ripgrep"] {
            let pkgs = [("tool".to_string(), ok.to_string())];
            assert!(
                check_packages_supported("ubuntu", &pkgs).is_ok(),
                "{ok:?} is a real installable reference"
            );
        }
    }

    #[test]
    fn the_ubuntu_check_validates_the_reference_that_gets_installed() {
        // The preflight checked the *source* while provisioning installs
        // `installable(name, source)`, so it was wrong in both directions.
        //
        // This is the sentence written on `nixos_attr_path` in round 24 —
        // validating one string while writing a different one is not
        // validation — repeated in the branch added five rounds later.

        // Passed, because `nixpkgs` is a fine source; then provisioning built
        // `nixpkgs#bad;name` and refused it after the box existed, where the
        // failure is a warning and state is saved anyway.
        let smuggled = [("bad;name".to_string(), "nixpkgs".to_string())];
        assert!(check_packages_supported("ubuntu", &smuggled).is_err());

        // Refused, because a bare flake URL is not an attribute path — even
        // though provisioning appends the key and installs it happily.
        let legitimate = [("tool".to_string(), "github:owner/repo".to_string())];
        assert!(
            check_packages_supported("ubuntu", &legitimate).is_ok(),
            "provisioning would install github:owner/repo#tool"
        );
    }

    #[test]
    fn a_box_is_not_created_with_two_names_for_one_attribute() {
        // `Selection::validate` has refused this since round 29, which is
        // exactly the problem: creation did not. So a box could be *made*
        // carrying both — the guest table deduplicates, both names persist in
        // state, and then every Sets operation on it fails validation. A box
        // created successfully and unmanageable from the moment it existed.
        let collide = [
            ("terraform".to_string(), "nixpkgs".to_string()),
            ("my-tf".to_string(), "nixpkgs#terraform".to_string()),
        ];
        let err = check_packages_supported("nixos", &collide)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("terraform") && err.contains("my-tf"),
            "the message must name both: {err}"
        );

        // One name for one attribute is fine, however it is spelled.
        let fine = [
            ("ripgrep".to_string(), "nixpkgs".to_string()),
            ("my-tf".to_string(), "nixpkgs#terraform".to_string()),
        ];
        assert!(check_packages_supported("nixos", &fine).is_ok());
    }

    #[test]
    fn the_check_validates_the_string_that_gets_written() {
        // The `create` path reached `generate_state_toml` without validating
        // anything: only the web path ran `is_valid_attr_path`. Now that the
        // fragment is what gets written, the fragment is what gets checked —
        // validating the key while writing the fragment would be worse than
        // not validating, because it reads like a guard.
        let ok = [("tf".to_string(), "nixpkgs#terraform".to_string())];
        assert!(check_packages_supported("nixos", &ok).is_ok());

        for hostile in ["nixpkgs#a; touch /tmp/pwn", "nixpkgs#$(reboot)", "nixpkgs#"] {
            let pkgs = [("tf".to_string(), hostile.to_string())];
            assert!(
                check_packages_supported("nixos", &pkgs).is_err(),
                "{hostile:?} must be refused before it reaches the box"
            );
        }

        // A hostile *key* is still caught when the source is plain nixpkgs.
        let bad_key = [("a; touch /tmp/pwn".to_string(), "nixpkgs".to_string())];
        assert!(check_packages_supported("nixos", &bad_key).is_err());
    }

    #[test]
    fn nix_packages_system_set() {
        let pkgs = nix_packages_for_set("system");
        assert!(pkgs.contains(&"coreutils"));
        assert!(pkgs.contains(&"gcc"));
        assert!(pkgs.contains(&"curl"));
        // The tools enforcement shells out to. A count assertion used to sit
        // here; it only ever said "somebody edited this list", which is not a
        // property worth failing a build over — naming what has to be present
        // says why.
        assert!(
            pkgs.contains(&"nftables"),
            "policies load a ruleset with it"
        );
        assert!(
            pkgs.contains(&"conntrack-tools"),
            "tightening a policy has to drop the sessions it no longer allows"
        );
    }

    #[test]
    fn nix_packages_shell_set() {
        let pkgs = nix_packages_for_set("shell");
        assert_eq!(pkgs.len(), 10);
        assert!(pkgs.contains(&"starship"));
        assert!(pkgs.contains(&"yazi"));
        // v4 retires the multiplexer from the default path (§5); the console
        // is the multi-pane experience now.
        assert!(!pkgs.contains(&"zellij"));
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
            "system",
            "shell",
            "tools",
            "editor",
            "git",
            "container",
            "network",
            "ai-code",
            "ai-infra",
            "lang-go",
            "lang-rust",
            "lang-python",
            "lang-node",
            "lang-java",
            "lang-ruby",
        ];
        for set in &all_sets {
            let pkgs = nix_packages_for_set(set);
            assert!(!pkgs.is_empty(), "set '{set}' should have packages");
        }
    }

    #[test]
    fn cache_key_deterministic() {
        let sets = vec!["system".to_string(), "shell".to_string()];
        let langs = vec!["go".to_string()];
        let k1 = cache_key("nixos", &sets, &langs, "overlay", &[]);
        let k2 = cache_key("nixos", &sets, &langs, "overlay", &[]);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16); // 16-char hex string
    }

    #[test]
    fn cache_key_order_independent() {
        let sets_a = vec!["shell".to_string(), "system".to_string()];
        let sets_b = vec!["system".to_string(), "shell".to_string()];
        let langs = vec!["go".to_string()];
        let k1 = cache_key("nixos", &sets_a, &langs, "overlay", &[]);
        let k2 = cache_key("nixos", &sets_b, &langs, "overlay", &[]);
        assert_eq!(k1, k2, "cache key should be order-independent");
    }

    #[test]
    fn cache_key_differs_on_inputs() {
        let sets = vec!["system".to_string()];
        let langs = vec![];
        let k_nixos = cache_key("nixos", &sets, &langs, "overlay", &[]);
        let k_ubuntu = cache_key("ubuntu", &sets, &langs, "overlay", &[]);
        assert_ne!(k_nixos, k_ubuntu, "different image → different key");

        let k_overlay = cache_key("nixos", &sets, &langs, "overlay", &[]);
        let k_writable = cache_key("nixos", &sets, &langs, "writable", &[]);
        assert_ne!(
            k_overlay, k_writable,
            "different mount_mode → different key"
        );

        let sets2 = vec!["system".to_string(), "ai-code".to_string()];
        let k_more = cache_key("nixos", &sets2, &langs, "overlay", &[]);
        assert_ne!(k_nixos, k_more, "different sets → different key");

        let pkgs = vec![("ripgrep".to_string(), "nixpkgs".to_string())];
        let k_pkgs = cache_key("nixos", &sets, &langs, "overlay", &pkgs);
        assert_ne!(k_nixos, k_pkgs, "an ad-hoc package → different key");
        let flake = vec![("ripgrep".to_string(), "github:u/r#ripgrep".to_string())];
        let k_flake = cache_key("nixos", &sets, &langs, "overlay", &flake);
        assert_ne!(
            k_pkgs, k_flake,
            "same name, different source → different key"
        );
    }

    #[test]
    fn config_version_nonzero() {
        let v = config_version();
        assert_ne!(v, 0, "config_version should be non-zero");
    }
}

#[cfg(test)]
mod resolved_packages_tests {
    use super::resolved_packages;
    use crate::sandbox::state::SandboxState;

    fn box_in(project: &std::path::Path, schema: u32, packages: Vec<String>) -> SandboxState {
        SandboxState {
            schema,
            packages,
            package_sources: Default::default(),
            name: "b".into(),
            runtime: "docker".into(),
            project_dir: project.to_path_buf(),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec!["system".into()],
            languages: vec![],
            image: "nixos".into(),
        }
    }

    fn project_with_packages(dir: &std::path::Path) {
        std::fs::write(
            dir.join("devbox.toml"),
            "[custom_packages]\nripgrep = \"nixpkgs\"\nmy-tf = \"nixpkgs#terraform\"\n",
        )
        .expect("write devbox.toml");
    }

    #[test]
    fn a_v3_box_gets_its_packages_from_the_project_file() {
        // The list, not merely each package's source. This read `state.packages`
        // directly and fell back only for a source — which looks like a legacy
        // fallback and is not one: a v3 box has no entries, so there was
        // nothing to find sources for and the whole list came back empty.
        //
        // `reprovision` and `devbox use` both rebuild from this, so both
        // dropped every custom package such a box had.
        let dir = tempfile::tempdir().unwrap();
        project_with_packages(dir.path());

        let (pairs, selection) = resolved_packages(&box_in(dir.path(), 0, vec![]));

        assert!(
            pairs.iter().any(|(n, _)| n == "ripgrep"),
            "the package list was lost: {pairs:?}"
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(n, _)| n == "my-tf")
                .map(|(_, s)| s.as_str()),
            Some("nixpkgs#terraform"),
            "and an alias must keep its source"
        );
        // The same answer the caller persists, so the rebuild and the record
        // cannot disagree.
        assert!(selection.packages.contains("ripgrep"));
    }

    #[test]
    fn a_current_box_with_no_packages_stays_empty() {
        // The other direction, which the schema marker exists to protect: a box
        // written by current code that genuinely has none must not inherit
        // whatever the project file happens to declare.
        let dir = tempfile::tempdir().unwrap();
        project_with_packages(dir.path());

        let (pairs, _) =
            resolved_packages(&box_in(dir.path(), crate::sandbox::state::SCHEMA, vec![]));
        assert!(
            pairs.is_empty(),
            "inherited the project's packages: {pairs:?}"
        );
    }
}

#[cfg(test)]
mod credential_hygiene_tests {
    use super::{credential_material, strip_credential_sections};

    /// The list of files provisioning copies is the promise. Two of them —
    /// `~/.claude/.credentials.json` and `~/.codex/auth.json` — were on it in
    /// v4, and this is the test that notices if they come back.
    #[test]
    fn no_credential_file_is_on_the_copy_list() {
        let copied: Vec<&str> = super::AI_TOOL_CONFIGS
            .iter()
            .flat_map(|tool| tool.config_files.iter().map(|(host, _)| *host))
            .collect();
        for forbidden in [
            ".claude/.credentials.json",
            ".codex/auth.json",
            ".aws/credentials",
            ".netrc",
        ] {
            assert!(
                !copied.contains(&forbidden),
                "{forbidden} must never be copied into a box"
            );
        }
        for suffix in &copied {
            assert!(
                !suffix.contains("credential") && !suffix.contains("auth"),
                "{suffix} is named like a credential file"
            );
        }
    }

    #[test]
    fn a_settings_file_carrying_a_key_is_recognised() {
        // `~/.claude/settings.json` genuinely has an `env` block, and people
        // put keys in it.
        assert!(
            credential_material(r#"{"env":{"ANTHROPIC_API_KEY":"sk-ant-api03-abc"}}"#).is_some()
        );
        assert!(credential_material("api_key: sk-ant-oat01-xyz\n").is_some());
        assert!(credential_material(r#"{"apiKey": "anything"}"#).is_some());
        assert!(credential_material("token: ghp_0123456789abcdef\n").is_some());
        assert!(credential_material(r#"{"access_token":"x"}"#).is_some());
        assert!(credential_material("aws_access_key_id = AKIAIOSFODNN7EXAMPLE").is_some());

        // Settings that are only settings.
        assert!(credential_material(r#"{"model":"claude-sonnet-4-5"}"#).is_none());
        assert!(credential_material("model: claude:claude-sonnet-4-5\nclients: []\n").is_none());
        assert!(
            credential_material(r#"{"permissions":{"allow":["Bash(git:*)"]}}"#).is_none(),
            "an allow-list is not a credential"
        );
        // An empty field is what a config skeleton looks like.
        assert!(credential_material(r#"{"api_key": ""}"#).is_none());
        // `apiKeyHelper` names a command, not a key.
        assert!(credential_material(r#"{"apiKeyHelper": "helper.sh"}"#).is_none());
    }

    #[test]
    fn credential_helpers_do_not_cross_into_the_guest() {
        let host = "\
[user]
\tname = Ethan
[credential]
\thelper = osxkeychain
[credential \"https://github.com\"]
\thelper = store --file /Users/ethan/.git-token
[core]
\teditor = vim
";
        let out = strip_credential_sections(host);
        assert!(out.contains("[user]"));
        assert!(out.contains("name = Ethan"));
        assert!(out.contains("[core]"));
        assert!(out.contains("editor = vim"));
        assert!(
            !out.contains("osxkeychain") && !out.contains(".git-token"),
            "a host credential helper is meaningless or dangerous in the guest: {out}"
        );
        assert!(!out.contains("[credential"));

        // A config with nothing to strip comes back unchanged in substance.
        let plain = "[user]\n\tname = Ethan\n";
        assert_eq!(strip_credential_sections(plain), plain);
    }
}
