use std::collections::HashMap;

/// A Nix set with its package list mapped to nixpkgs attribute paths.
#[derive(Debug, Clone)]
pub struct NixSet {
    pub name: &'static str,
    pub packages: &'static [&'static str],
}

/// All Nix sets with their nixpkgs attribute paths.
pub static NIX_SETS: &[NixSet] = &[
    NixSet {
        name: "system",
        packages: &[
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
            // nftables, because every enforcing egress posture loads a ruleset
            // with it. Pointing a user at some optional set to get their
            // firewall would make enforcement conditional on a checkbox they
            // have no reason to connect to it.
            "nftables",
            // conntrack, because tightening a policy must drop the sessions it
            // no longer allows — `ct state established,related accept` sits
            // first in the ruleset, so without a flush an open→isolated box
            // keeps every connection it already had, while reporting success.
            // It was in the checked-in system.nix but only the *network* set
            // here, so any Sets apply silently regenerated the box without it.
            "conntrack-tools",
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
    },
    NixSet {
        name: "shell",
        packages: &[
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
    },
    NixSet {
        name: "tools",
        packages: &[
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
    },
    NixSet {
        name: "editor",
        packages: &["neovim", "helix", "nano"],
    },
    NixSet {
        name: "git",
        packages: &["git", "lazygit", "gh", "git-lfs", "git-crypt", "pre-commit"],
    },
    NixSet {
        name: "container",
        packages: &[
            "docker",
            "docker-compose",
            "lazydocker",
            "dive",
            "buildkit",
            "skopeo",
        ],
    },
    NixSet {
        name: "network",
        packages: &[
            // The routing stack a `devbox lab` substrate needs: without it
            // `lab up` starts zebra and gets command-not-found for every
            // routed topology.
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
    },
    NixSet {
        name: "ai-code",
        packages: &["claude-code", "codex", "opencode", "aider-chat", "aichat"],
    },
    NixSet {
        name: "ai-infra",
        packages: &[
            "ollama",
            "open-webui",
            "litellm",
            "mcp-hub",
            "python312Packages.huggingface-hub",
        ],
    },
    NixSet {
        name: "lang-go",
        packages: &["go", "gopls", "golangci-lint", "delve", "gotools", "gore"],
    },
    NixSet {
        name: "lang-rust",
        packages: &[
            "rustup",
            "rust-analyzer",
            "cargo-watch",
            "cargo-edit",
            "cargo-expand",
            "sccache",
        ],
    },
    NixSet {
        name: "lang-python",
        packages: &[
            "python312",
            "uv",
            "ruff",
            "pyright",
            "python312Packages.ipython",
            "python312Packages.pytest",
        ],
    },
    NixSet {
        name: "lang-node",
        packages: &[
            "nodejs_22",
            "bun",
            "pnpm",
            "typescript",
            "nodePackages.typescript-language-server",
            "biome",
        ],
    },
    NixSet {
        name: "lang-java",
        packages: &["jdk21", "gradle", "maven", "jdt-language-server"],
    },
    NixSet {
        name: "lang-ruby",
        packages: &["ruby_3_3", "bundler", "solargraph", "rubocop"],
    },
];

/// Lookup a NixSet by name.
#[allow(dead_code)]
pub fn find_set(name: &str) -> Option<&'static NixSet> {
    NIX_SETS.iter().find(|s| s.name == name)
}

/// Generate a Nix expression file for a single set.
/// Returns: `{ pkgs }: with pkgs; [ pkg1 pkg2 ... ]`
pub fn generate_set_nix(set: &NixSet) -> String {
    let mut nix = format!("# Auto-generated by devbox — {} set\n", set.name);
    nix.push_str("{ pkgs }:\nwith pkgs;\n[\n");
    for pkg in set.packages {
        nix.push_str(&format!("  {pkg}\n"));
    }
    nix.push_str("]\n");
    nix
}

/// Generate the /etc/devbox/sets/default.nix that imports all sets.
pub fn generate_sets_default_nix() -> String {
    let mut nix = String::from("# Auto-generated by devbox — set index\n{ pkgs }:\n{\n");
    for set in NIX_SETS {
        nix.push_str(&format!(
            "  {} = import ./{}.nix {{ inherit pkgs; }};\n",
            set.name.replace('-', "_"),
            set.name
        ));
    }
    nix.push_str("}\n");
    nix
}

/// Generate a devbox-state.toml from active sets and languages.
pub fn generate_state_toml(
    sets: &HashMap<String, bool>,
    languages: &HashMap<String, bool>,
    custom_packages: &HashMap<String, String>,
) -> String {
    generate_state_toml_with(sets, languages, custom_packages, None, None)
}

/// Generate `devbox-state.toml`, preserving the guest identity and mount mode.
///
/// `devbox-module.nix` reads `[user].name` and `[sandbox].mount_mode` from this
/// file. Regenerating it from sets alone silently resets the guest username to
/// `dev` and the mount mode to `overlay` — which breaks a box whose guest user
/// differs, or whose workspace is writable, on the very next rebuild.
pub fn generate_state_toml_with(
    sets: &HashMap<String, bool>,
    languages: &HashMap<String, bool>,
    custom_packages: &HashMap<String, String>,
    username: Option<&str>,
    mount_mode: Option<&str>,
) -> String {
    let mut toml = String::new();

    if let Some(name) = username {
        toml.push_str(&format!("[user]\nname = \"{name}\"\n\n"));
    }
    if let Some(mode) = mount_mode {
        toml.push_str(&format!("[sandbox]\nmount_mode = \"{mode}\"\n\n"));
    }

    toml.push_str("[sets]\n");
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
    for name in &set_names {
        let enabled = sets.get(*name).copied().unwrap_or(false);
        toml.push_str(&format!("{name} = {enabled}\n"));
    }

    toml.push_str("\n[languages]\n");
    let lang_names = ["go", "rust", "python", "node", "java", "ruby"];
    for name in &lang_names {
        let enabled = languages.get(*name).copied().unwrap_or(false);
        toml.push_str(&format!("{name} = {enabled}\n"));
    }

    if !custom_packages.is_empty() {
        toml.push_str("\n[custom_packages]\n");
        // Keyed by the *attribute*, resolved here rather than by each caller.
        //
        // `devbox-module.nix` builds its lookup path from the key and reads the
        // value only to see whether it is a nested table, so an alias like
        // `my-tf = "nixpkgs#terraform"` written under its declared name
        // resolves to null and is filtered out in silence — the package
        // vanishes while state goes on reporting it selected.
        //
        // Round 25 fixed that in the Sets writer and left `apply_config`
        // untouched, which is how `devbox upgrade` kept writing the alias. So
        // it is done here instead: this is the one place the table is emitted,
        // and every path that reaches it is now correct without its author
        // having to know any of the above. The resolution is idempotent, so a
        // caller that already resolved loses nothing.
        //
        // Sorted, because this map is a `HashMap` and its iteration order is
        // not stable — the same selection produced a different file on each
        // run, and a rebuild that always looks like a change is a rebuild
        // nobody can read.
        let mut resolved: Vec<(String, &String)> = custom_packages
            .iter()
            .map(|(name, source)| {
                (
                    crate::sandbox::provision::nixos_attr_path(name, source).to_string(),
                    source,
                )
            })
            .collect();
        resolved.sort();
        // Two declarations can resolve to one attribute — `terraform =
        // "nixpkgs"` beside `my-tf = "nixpkgs#terraform"` — and emitting the
        // key twice is not merely redundant, it is invalid TOML: the file
        // fails to parse and the rebuild fails with it, for a config that
        // looked reasonable. `Selection::validate` refuses the collision with
        // an explanation; this coalesces, because an emitter that can produce
        // an unparseable file is a worse failure than a dropped duplicate.
        resolved.dedup_by(|a, b| a.0 == b.0);
        for (attr, source) in resolved {
            // Quoted: a bare dotted key is a nested table, and while the module
            // flattens both, one literal key is what this means.
            toml.push_str(&format!("\"{attr}\" = \"{source}\"\n"));
        }
    }

    toml
}

#[cfg(test)]
mod tests {
    /// Every writer of the guest state file gets the attribute right, because
    /// the file itself resolves it.
    ///
    /// Round 25 put the resolution in the Sets writer and round 26 found
    /// `apply_config` — the `devbox upgrade` path — still writing the declared
    /// name. Two writers, one already fixed, and the fix had not been asked
    /// the only question that mattered: who else emits this table. So it lives
    /// here now, at the single point the table is produced.
    #[test]
    fn the_guest_state_table_is_keyed_by_the_attribute() {
        let packages = std::collections::HashMap::from([
            ("my-tf".to_string(), "nixpkgs#terraform".to_string()),
            ("ripgrep".to_string(), "nixpkgs".to_string()),
        ]);
        let toml = super::generate_state_toml(&Default::default(), &Default::default(), &packages);

        assert!(
            toml.contains("\"terraform\" = \"nixpkgs#terraform\""),
            "the module builds its lookup path from the key:\n{toml}"
        );
        assert!(
            !toml.contains("my-tf"),
            "an alias as the key resolves to null and is dropped in silence:\n{toml}"
        );
        assert!(toml.contains("\"ripgrep\" = \"nixpkgs\""), "{toml}");
    }

    /// The same input must produce the same file, byte for byte.
    ///
    /// `custom_packages` is a `HashMap`, so this table came out in a different
    /// order on each run and every rebuild looked like a change — against the
    /// rule `Selection` states for exactly this reason. Nothing reported it
    /// because nothing compares two runs; a sorted emitter is what makes the
    /// comparison meaningful when someone finally does.
    #[test]
    fn the_guest_state_file_is_byte_identical_across_runs() {
        let packages = std::collections::HashMap::from([
            ("alpha".to_string(), "nixpkgs".to_string()),
            ("beta".to_string(), "nixpkgs".to_string()),
            ("gamma".to_string(), "nixpkgs".to_string()),
            ("delta".to_string(), "nixpkgs".to_string()),
            ("epsilon".to_string(), "nixpkgs".to_string()),
        ]);
        let once = super::generate_state_toml(&Default::default(), &Default::default(), &packages);
        for _ in 0..16 {
            assert_eq!(
                once,
                super::generate_state_toml(&Default::default(), &Default::default(), &packages),
                "the same selection must produce the same file"
            );
        }
    }

    /// The Ubuntu package mapping must carry everything the catalog does.
    ///
    /// Ubuntu provisioning does not read `NIX_SETS`; it uses a separate
    /// `nix_packages_for_set` mapping. A package added to one and not the
    /// other means an Ubuntu box is reported provisioned without a tool that
    /// a later command shells out to — `nft` for a policy, `frr` for a lab.
    #[test]
    fn the_ubuntu_mapping_covers_every_catalogued_package() {
        for set in super::NIX_SETS {
            let ubuntu = crate::sandbox::provision::nix_packages_for_set(set.name);
            if ubuntu.is_empty() {
                continue; // not offered on Ubuntu at all
            }
            for package in set.packages {
                assert!(
                    ubuntu.contains(package),
                    "`{package}` is in the `{}` catalog but not in the Ubuntu \
                     mapping; an Ubuntu box would be provisioned without it",
                    set.name
                );
            }
        }
    }

    /// **Every** set's catalog entry and checked-in module must agree.
    ///
    /// Provisioning pushes the checked-in modules; `write_set_modules`
    /// regenerates from this catalog. When the two disagree, a box gets one
    /// set of packages at create and a different one after any Sets apply.
    /// That is how `conntrack` came to be present on a fresh box and absent
    /// once the user touched a checkbox — silently disarming the flush that
    /// makes a tightened policy take effect.
    ///
    /// Written for all sets rather than the one that broke: the same drift can
    /// happen in any of them, and the two earlier instances of this class were
    /// each fixed individually before anyone generalized it.
    #[test]
    fn every_set_matches_its_checked_in_module() {
        // (name, module source) — `include_str!` needs a literal path.
        let modules: &[(&str, &str)] = &[
            ("system", include_str!("../../nix/sets/system.nix")),
            ("shell", include_str!("../../nix/sets/shell.nix")),
            ("tools", include_str!("../../nix/sets/tools.nix")),
            ("editor", include_str!("../../nix/sets/editor.nix")),
            ("git", include_str!("../../nix/sets/git.nix")),
            ("container", include_str!("../../nix/sets/container.nix")),
            ("network", include_str!("../../nix/sets/network.nix")),
            ("ai-code", include_str!("../../nix/sets/ai-code.nix")),
            ("ai-infra", include_str!("../../nix/sets/ai-infra.nix")),
            ("lang-go", include_str!("../../nix/sets/lang-go.nix")),
            ("lang-rust", include_str!("../../nix/sets/lang-rust.nix")),
            (
                "lang-python",
                include_str!("../../nix/sets/lang-python.nix"),
            ),
            ("lang-node", include_str!("../../nix/sets/lang-node.nix")),
            ("lang-java", include_str!("../../nix/sets/lang-java.nix")),
            ("lang-ruby", include_str!("../../nix/sets/lang-ruby.nix")),
        ];

        // Every catalogued set needs a module here, or the check silently
        // stops covering it — the failure mode this test exists to prevent.
        for set in super::NIX_SETS {
            assert!(
                modules.iter().any(|(name, _)| *name == set.name),
                "set `{}` has no checked-in module in this list; add it",
                set.name
            );
        }

        for (name, module) in modules {
            let set = super::NIX_SETS
                .iter()
                .find(|s| s.name == *name)
                .unwrap_or_else(|| panic!("`{name}` is not in the catalog"));

            for package in set.packages {
                assert!(
                    module.contains(package),
                    "`{package}` is in the `{name}` catalog but not in \
                     nix/sets/{name}.nix; a Sets apply would change what the \
                     box has"
                );
            }
        }
    }

    use super::*;

    #[test]
    fn all_sets_present() {
        assert_eq!(NIX_SETS.len(), 15);
        assert!(find_set("system").is_some());
        assert!(find_set("lang-go").is_some());
        assert!(find_set("nonexistent").is_none());
    }

    #[test]
    fn generate_set_nix_output() {
        let set = find_set("editor").unwrap();
        let nix = generate_set_nix(set);
        assert!(nix.contains("{ pkgs }:"));
        assert!(nix.contains("neovim"));
        assert!(nix.contains("helix"));
    }

    #[test]
    fn generate_default_nix_output() {
        let nix = generate_sets_default_nix();
        assert!(nix.contains("system = import ./system.nix"));
        assert!(nix.contains("lang_go = import ./lang-go.nix"));
    }

    #[test]
    fn state_toml_preserves_guest_identity_and_mount_mode() {
        // The NixOS module reads these; regenerating without them resets the
        // guest username to `dev` and the mount mode to `overlay` on the very
        // next rebuild.
        let toml = generate_state_toml_with(
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Some("ethan.linux"),
            Some("writable"),
        );
        assert!(toml.contains("[user]"));
        assert!(toml.contains("name = \"ethan.linux\""));
        assert!(toml.contains("[sandbox]"));
        assert!(toml.contains("mount_mode = \"writable\""));

        // Absent when unknown, rather than written as a wrong default.
        let bare = generate_state_toml(&HashMap::new(), &HashMap::new(), &HashMap::new());
        assert!(!bare.contains("[user]"));
        assert!(!bare.contains("[sandbox]"));
    }

    #[test]
    fn generate_state_toml_output() {
        let mut sets = HashMap::new();
        sets.insert("system".to_string(), true);
        sets.insert("ai_code".to_string(), true);
        let mut langs = HashMap::new();
        langs.insert("go".to_string(), true);
        let toml = generate_state_toml(&sets, &langs, &HashMap::new());
        assert!(toml.contains("system = true"));
        assert!(toml.contains("ai_code = true"));
        assert!(toml.contains("go = true"));
        assert!(toml.contains("network = false"));
    }
}
