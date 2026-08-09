//! Post-create provisioning for VMs.
//!
//! Supports two image types:
//! - **NixOS**: push nix config files + `nixos-rebuild switch`
//! - **Ubuntu**: install Nix package manager + `nix profile install`
//!
//! Both paths use the same package definitions from nix/sets/*.nix.

use anyhow::{Result, bail};

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
const AICHAT_ROLE_ARCHITECT: &str = include_str!("../../configs/aichat/roles/architect.md");
const AICHAT_ROLE_REVIEWER: &str = include_str!("../../configs/aichat/roles/reviewer.md");
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
    resolved_packages(state)
        .0
        .into_iter()
        .map(|(name, source)| (name, source))
        .collect()
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
            "conntrack-tools",
            "tailscale",
            "mosh",
            "nmap",
            "tcpdump",
            "bandwhich",
            "trippy",
            "doggo",
        ],
        "ai-code" => vec![
            "claude-code",
            "codex",
            "opencode",
            "aider-chat",
            "aichat",
            "continue",
        ],
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
    match image {
        "ubuntu" => provision_ubuntu(runtime, name, sets, languages, packages).await,
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
            provision_nixos(runtime, name, sets, languages, mount_mode, &names).await
        }
    }
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
) -> Result<()> {
    let username = whoami();

    // 1. Create directory structure
    println!("Setting up NixOS configuration...");
    runtime
        .exec_cmd(
            name,
            &[
                "sudo",
                "mkdir",
                "-p",
                "/etc/devbox/sets",
                "/etc/devbox/help",
            ],
            false,
        )
        .await?;

    // 2. Generate base NixOS config if it doesn't exist
    //    NixOS Lima images ship with an empty /etc/nixos/ — we need to
    //    run nixos-generate-config to create the hardware and base configs.
    ensure_nixos_config(runtime, name).await?;

    // 3. Push devbox-state.toml (includes mount_mode for overlay setup)
    let state_toml = generate_state_toml(sets, languages, &username, mount_mode, packages);
    write_file_to_vm(runtime, name, "/etc/devbox/devbox-state.toml", &state_toml).await?;

    // 4. Push devbox-module.nix
    write_file_to_vm(
        runtime,
        name,
        "/etc/devbox/devbox-module.nix",
        NIX_DEVBOX_MODULE,
    )
    .await?;

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

    // 8. Set up user shell (zshrc with PATH, aliases, etc.)
    setup_nixos_shell(runtime, name).await?;

    // 9. Install latest claude-code (nixpkgs version lags behind)
    if sets.iter().any(|s| s == "ai-code" || s == "ai_code") {
        install_latest_claude_code(runtime, name).await;
    }

    // 10. Copy host git config into VM
    setup_git_config(runtime, name).await?;

    // 11. Copy devbox binary + help files + tool configs
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
    extra: &[(String, String)],
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
        let result = runtime
            .exec_cmd(name, &["bash", "-c", &install_cmd], true)
            .await?;

        if result.exit_code != 0 {
            eprintln!("Warning: some packages failed to install.");
            eprintln!(
                "You can retry with: devbox exec --name {name} -- nix profile install <packages>"
            );
        } else {
            println!("Nix package installation complete.");
        }
    }

    // 4. Install services that need apt (Docker, Tailscale)
    install_ubuntu_services(runtime, name, sets).await?;

    // 5. Set up shell environment
    setup_ubuntu_shell(runtime, name).await?;

    // 6. Copy host git config into VM
    setup_git_config(runtime, name).await?;

    // 7. Create devbox directories and copy binary + help
    runtime
        .exec_cmd(name, &["sudo", "mkdir", "-p", "/etc/devbox/help"], false)
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
async fn install_ubuntu_services(runtime: &dyn Runtime, name: &str, sets: &[String]) -> Result<()> {
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

    let result = runtime.exec_cmd(name, &["bash", "-c", &setup], false).await;
    if let Ok(r) = result
        && r.exit_code != 0
    {
        eprintln!("Warning: shell setup incomplete");
    }

    Ok(())
}

/// Set up user shell environment on NixOS (zshrc with PATH, aliases, workspace cd).
async fn setup_nixos_shell(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let username = whoami();
    let zshrc_path = format!("/home/{username}/.zshrc");

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

        let chown_cmd = format!("chown {username}:users {zshrc_path}");
        runtime
            .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
            .await?;
    }

    // Also create .profile for bash login shells (used by layout panes with bash -lc)
    let profile_path = format!("/home/{username}/.profile");
    let profile_check = runtime
        .exec_cmd(name, &["test", "-f", &profile_path], false)
        .await?;
    if profile_check.exit_code != 0 {
        let profile = r#"# Devbox bash profile
# Latest tools first (npm-global and official installers take precedence over nixpkgs)
export PATH="$HOME/.npm-global/bin:$HOME/.local/bin:$HOME/.claude/bin:$PATH"
"#;
        write_file_to_vm(runtime, name, &profile_path, profile).await?;
        let chown_cmd = format!("chown {username}:users {profile_path}");
        runtime
            .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
            .await?;
    }

    Ok(())
}

// ── Git Config ─────────────────────────────────────────────

/// Copy host ~/.gitconfig into the VM so git user.name, user.email,
/// remote aliases, and other settings carry over automatically.
async fn setup_git_config(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let gitconfig_path = home.join(".gitconfig");

    if !gitconfig_path.exists() {
        return Ok(());
    }

    let content = match std::fs::read_to_string(&gitconfig_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    let username = whoami();
    let vm_path = format!("/home/{username}/.gitconfig");
    write_file_to_vm(runtime, name, &vm_path, &content).await?;

    let chown_cmd = format!("chown {username}:users {vm_path}");
    runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
        .await?;

    println!("Synced host git config to VM.");
    Ok(())
}

// ── Shared Helpers ──────────────────────────────────────────

/// Generate devbox-state.toml content from active sets and languages.
fn generate_state_toml(
    sets: &[String],
    languages: &[String],
    username: &str,
    mount_mode: &str,
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

    let mut toml = String::from("[user]\n");
    toml.push_str(&format!("name = \"{username}\"\n\n"));

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
    toml.push_str(&format!("mount_mode = \"{mount_mode}\"\n"));

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
        config_files: &[(
            ".config/opencode/config.json",
            ".config/opencode/config.json",
        )],
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
        config_files: &[(".config/aichat/config.yaml", ".config/aichat/config.yaml")],
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
            println!("  found: ~/{f}");
        }
        for v in &found_env_vars {
            println!("  env:   {v}");
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

            // Ensure parent directory exists with correct ownership
            let vm_parent = vm_path.rsplit_once('/').map(|(p, _)| p).unwrap_or(&vm_path);
            runtime
                .exec_cmd(name, &["sudo", "mkdir", "-p", vm_parent], false)
                .await?;

            write_file_to_vm(runtime, name, &vm_path, &content).await?;
            println!("  copied: ~/{host_suffix} → {vm_path}");

            // Set restrictive permissions for credential/auth files
            if vm_suffix.contains("credential") || vm_suffix.contains("auth") {
                runtime
                    .exec_cmd(name, &["sudo", "chmod", "600", &vm_path], false)
                    .await?;
            }
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
        let chown_cmd = format!(
            "chown -R {username}:users {vm_home}/.claude {vm_home}/.config {vm_home}/.codex {vm_home}/.devbox-ai-env 2>/dev/null; true"
        );
        runtime
            .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
            .await?;
    }

    if copied_any {
        println!("AI tool configurations synced to devbox.");
    }

    // Auto-generate aichat config from detected credentials if no host config was copied.
    let has_aichat_config = home.join(".config/aichat/config.yaml").exists();
    if !has_aichat_config && let Some(config) = generate_aichat_config_from_credentials(&home) {
        let config_dir = format!("{vm_home}/.config/aichat");
        runtime
            .exec_cmd(name, &["sudo", "mkdir", "-p", &config_dir], false)
            .await?;
        let config_path = format!("{config_dir}/config.yaml");
        write_file_to_vm(runtime, name, &config_path, &config).await?;
        let chown_cmd = format!("chown -R {username}:users {config_dir}");
        runtime
            .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
            .await?;
        println!("Generated aichat config from detected AI tool credentials.");
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
            parsed
                .get("apiKey")
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
            parsed
                .get("apiKey")
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
    let result = runtime.exec_cmd(name, &["bash", "-c", &cmd], false).await?;
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
        if let Ok(r) = result
            && r.exit_code == 0
        {
            let _ = runtime
                .exec_cmd(
                    name,
                    &[
                        "sudo",
                        "install",
                        "-m",
                        "755",
                        "/tmp/devbox",
                        "/usr/local/bin/devbox",
                    ],
                    false,
                )
                .await;
            let _ = runtime.exec_cmd(name, &["rm", "/tmp/devbox"], false).await;
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
    write_file_to_vm(
        runtime,
        name,
        &format!("{plugin_dir}/init.lua"),
        YAZI_GLOW_PLUGIN,
    )
    .await?;

    // Fix ownership
    let chown_cmd = format!("chown -R {username}:users /home/{username}/.config/yazi");
    runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
        .await?;

    Ok(())
}

/// Push aichat config (roles) to user home directory in the VM.
/// Writes both legacy roles.yaml and modern roles/*.md format for compatibility.
async fn setup_aichat_config(runtime: &dyn Runtime, name: &str) -> Result<()> {
    let username = whoami();
    let config_dir = format!("/home/{username}/.config/aichat");
    let roles_dir = format!("{config_dir}/roles");

    runtime
        .exec_cmd(name, &["sudo", "mkdir", "-p", &roles_dir], false)
        .await?;

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

    let chown_cmd = format!("chown -R {username}:users {config_dir}");
    runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
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
    runtime
        .exec_cmd(
            name,
            &["sudo", "chmod", "+x", "/etc/devbox/management.sh"],
            false,
        )
        .await?;
    Ok(())
}

/// Install the latest claude-code via npm with a writable prefix.
///
/// On NixOS, the official binary installer fails (non-standard dynamic linker),
/// and `npm install -g` fails (Nix store is read-only). We work around this by
/// setting NPM_CONFIG_PREFIX to ~/.npm-global, then adding that to PATH.
async fn install_latest_claude_code(runtime: &dyn Runtime, name: &str) {
    let username = whoami();
    println!("Installing latest claude-code...");

    // Install via npm with a writable global prefix.
    // On NixOS, npm may not be in PATH (claude-code nix pkg bundles its own node
    // but doesn't expose npm). Use nix-env (stable, no experimental features needed)
    // to install nodejs to user profile first if needed.
    let install_cmd = concat!(
        "export PATH=\"$HOME/.nix-profile/bin:/run/current-system/sw/bin:$PATH\"; ",
        "export NPM_CONFIG_PREFIX=\"$HOME/.npm-global\"; ",
        "mkdir -p \"$HOME/.npm-global\"; ",
        "if ! command -v npm >/dev/null 2>&1; then ",
        "echo 'npm not found, installing nodejs via nix-env...'; ",
        "nix-env -iA nixos.nodejs_22 2>&1; ",
        "export PATH=\"$HOME/.nix-profile/bin:$PATH\"; ",
        "fi; ",
        "echo \"Using npm: $(which npm 2>/dev/null || echo 'not found')\"; ",
        "if command -v npm >/dev/null 2>&1; then ",
        "npm install -g @anthropic-ai/claude-code@latest 2>&1; ",
        "echo \"Installed: $($HOME/.npm-global/bin/claude --version 2>/dev/null || echo 'failed')\"; ",
        "else ",
        "echo 'ERROR: npm still not available after nix-env install'; ",
        "fi"
    );
    let result = runtime
        .exec_cmd(name, &["bash", "-lc", install_cmd], true)
        .await;
    match result {
        Ok(r) if r.exit_code == 0 => {
            println!("claude-code installed (latest).");
        }
        _ => {
            eprintln!("Warning: could not install latest claude-code. Using nixpkgs version.");
        }
    }

    // Ensure ~/.npm-global/bin is at front of PATH in both .zshrc and .profile
    // so latest claude takes precedence over the nixpkgs system version.
    // .profile is needed because layout panes use `bash -lc` (not zsh).
    let path_line =
        r#"export PATH="$HOME/.npm-global/bin:$HOME/.local/bin:$HOME/.claude/bin:$PATH""#;
    for rc_file in &[".zshrc", ".profile"] {
        let rc_path = format!("/home/{username}/{rc_file}");
        let add_path_cmd = format!(
            "grep -qF '.npm-global/bin' {rc_path} 2>/dev/null || \
             echo '{path_line}' >> {rc_path}"
        );
        let _ = runtime
            .exec_cmd(name, &["bash", "-c", &add_path_cmd], false)
            .await;
    }
    // Fix ownership
    let chown_cmd = format!(
        "chown {username}:users /home/{username}/.zshrc /home/{username}/.profile 2>/dev/null; true"
    );
    let _ = runtime
        .exec_cmd(name, &["sudo", "bash", "-c", &chown_cmd], false)
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

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "dev".to_string())
}

// Overlay mount is now handled declaratively by devbox-module.nix via
// fileSystems."/workspace" when mount_mode = "overlay" in devbox-state.toml.
// The nixos-rebuild switch creates the systemd mount automatically.

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
        let toml = generate_state_toml(&sets, &langs, "testuser", "overlay", &[]);

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
        let toml = generate_state_toml(&sets, &langs, "dev", "overlay", &[]);

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
        let toml = generate_state_toml(&sets, &langs, "dev", "overlay", &[]);

        assert!(toml.contains("ai_code = true"));
        assert!(toml.contains("ai_infra = false"));
    }

    #[test]
    fn generate_state_toml_bare() {
        let sets = vec![];
        let langs = vec![];
        let toml = generate_state_toml(&sets, &langs, "user", "overlay", &[]);

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
