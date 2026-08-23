use std::collections::HashMap;

/// The checked-in Nix module for each set, embedded at compile time.
///
/// This — not [`generate_set_nix`] — is what a box actually gets. The two used
/// to disagree: provisioning pushed these files, and `sets apply` regenerated
/// every non-guarded set from [`NIX_SETS`] as a flat package list, so the first
/// apply silently replaced the checked-in module with a lossy reconstruction of
/// it. Anything a set expressed that a bare list of attribute names cannot —
/// a comment, a `runCommand` wrapper, a `tryEval` guard — survived provisioning
/// and then vanished. The `network` set lost the derivation that puts FRR's
/// routing daemons on PATH, which is the entire reason `devbox lab` can find
/// them, and the box reported `missing: zebra bgpd` after a rebuild that
/// claimed success.
///
/// [`NIX_SETS`] keeps its job: it is the package *index* — what the console
/// lists, what `resolved_packages` diffs, and what the non-NixOS `nix profile
/// install` path consumes. `set_files_match_index` pins the two together.
pub static NIX_SET_FILES: &[(&str, &str)] = &[
    ("default.nix", include_str!("../../nix/sets/default.nix")),
    ("system.nix", include_str!("../../nix/sets/system.nix")),
    ("shell.nix", include_str!("../../nix/sets/shell.nix")),
    ("tools.nix", include_str!("../../nix/sets/tools.nix")),
    ("editor.nix", include_str!("../../nix/sets/editor.nix")),
    ("git.nix", include_str!("../../nix/sets/git.nix")),
    (
        "container.nix",
        include_str!("../../nix/sets/container.nix"),
    ),
    ("network.nix", include_str!("../../nix/sets/network.nix")),
    ("ai-code.nix", include_str!("../../nix/sets/ai-code.nix")),
    ("ai-infra.nix", include_str!("../../nix/sets/ai-infra.nix")),
    ("lang-go.nix", include_str!("../../nix/sets/lang-go.nix")),
    (
        "lang-rust.nix",
        include_str!("../../nix/sets/lang-rust.nix"),
    ),
    (
        "lang-python.nix",
        include_str!("../../nix/sets/lang-python.nix"),
    ),
    (
        "lang-node.nix",
        include_str!("../../nix/sets/lang-node.nix"),
    ),
    (
        "lang-java.nix",
        include_str!("../../nix/sets/lang-java.nix"),
    ),
    (
        "lang-ruby.nix",
        include_str!("../../nix/sets/lang-ruby.nix"),
    ),
];

/// The checked-in module for a set, by set name (no `.nix` suffix).
pub fn set_file(name: &str) -> Option<&'static str> {
    let filename = format!("{name}.nix");
    NIX_SET_FILES
        .iter()
        .find(|(f, _)| *f == filename)
        .map(|(_, content)| *content)
}

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
            // Role services and the DHCP client used by `ztp-blank` nodes.
            // They are processes inside network namespaces, so a package set
            // (rather than a system service) is exactly what the lab needs.
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

    /// Every line with its `#` comment removed.
    fn comments_stripped(text: &str) -> String {
        text.lines()
            .map(|l| l.split('#').next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The double-quoted tokens that look like package names.
    fn quoted_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
        text.split('"')
            .skip(1)
            .step_by(2)
            .filter(|t| {
                !t.is_empty()
                    && t.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            })
            .map(str::to_string)
    }

    /// Explicit `pkgs.<attr.path>` references (dynamic `pkgs.${…}` excluded).
    fn pkgs_attr_paths(text: &str) -> impl Iterator<Item = String> + '_ {
        text.split("pkgs.").skip(1).filter_map(|rest| {
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
                .collect();
            let token = token.trim_end_matches('.').to_string();
            (!token.is_empty() && !rest.starts_with("${")).then_some(token)
        })
    }

    /// Split a module body into bare attribute names and the parenthesised
    /// sub-expressions that were removed to find them.
    ///
    /// A set module is a Nix expression, not a manifest: `network` builds a
    /// derivation to put FRR's routing daemons on PATH, because the package
    /// hides them in `libexec` where a system profile does not look. Only the
    /// bare names at the top level of the list are what the catalog mirrors —
    /// but the removed expressions are returned rather than discarded, so the
    /// caller can check that each one is the helper it expects. Blanking them
    /// silently meant `(pkgs.htop)` was invisible: a package could be
    /// installed by any module with the catalog never hearing of it.
    fn bare_list_only(body: &str) -> (String, Vec<String>) {
        let mut out = String::with_capacity(body.len());
        let mut groups = Vec::new();
        let mut group = String::new();
        let mut chars = body.chars().peekable();
        let mut depth = 0usize;
        let mut in_string = false;
        while let Some(c) = chars.next() {
            let quote = c == '\'' && chars.peek() == Some(&'\'');
            if quote {
                chars.next();
                in_string = !in_string;
                if depth > 0 {
                    group.push(' ');
                }
                out.push(' ');
                continue;
            }
            if in_string {
                if depth > 0 {
                    group.push(' ');
                }
                out.push(' ');
                continue;
            }
            match c {
                '(' => {
                    if depth > 0 {
                        group.push(c);
                    }
                    depth += 1;
                    out.push(' ');
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        groups.push(group.split_whitespace().collect::<Vec<_>>().join(" "));
                        group.clear();
                    } else {
                        group.push(c);
                    }
                    out.push(' ');
                }
                _ => {
                    if depth > 0 {
                        group.push(c);
                        out.push(' ');
                    } else {
                        out.push(c);
                    }
                }
            }
        }
        (out, groups)
    }

    /// A checked-in module and the catalog must describe the same packages.
    ///
    /// `nix/sets/<name>.nix` is what a box installs — both provisioning and
    /// every later Sets apply push it verbatim. `NIX_SETS` is the index the
    /// console lists and `resolved_packages` diffs against. A package in one
    /// and not the other is either installed and invisible, or shown and
    /// absent.
    ///
    /// The two used to disagree by construction: Sets apply regenerated the
    /// module *from* the catalog, so anything the module expressed that a flat
    /// list cannot was dropped at the first rebuild — which is how the box
    /// reported `missing: zebra bgpd` after an apply that printed success.
    /// Now the module is the source of truth and this test is what keeps the
    /// index honest about it.
    ///
    /// The AI sets are compared one way only: they wrap each optional tool in
    /// `tryEval`, so their module is deliberately not a list of names.
    #[test]
    fn every_checked_in_module_matches_the_catalog() {
        const HAND_WRITTEN: &[&str] = &["ai-code", "ai-infra"];

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("nix/sets");
        let mut drift = Vec::new();

        for set in super::NIX_SETS {
            let path = dir.join(format!("{}.nix", set.name));
            let Ok(module) = std::fs::read_to_string(&path) else {
                drift.push(format!(
                    "the catalog has a '{}' set with no nix/sets/{}.nix to install",
                    set.name, set.name
                ));
                continue;
            };

            if HAND_WRITTEN.contains(&set.name) {
                let catalogued: std::collections::BTreeSet<&str> =
                    set.packages.iter().copied().collect();
                for package in &catalogued {
                    if !module.contains(package) {
                        drift.push(format!(
                            "{}.nix never names '{package}', which the catalog has: \
                             the console offers a package the box does not install",
                            set.name
                        ));
                    }
                }
                // And the other way. The guarded modules name each package as
                // a quoted string (`tryAttr "ollama"`) or an explicit
                // `pkgs.<attr>` path, so those are the installable tokens: one
                // the catalog does not list is a package the box installs and
                // the console cannot show. One-directional checking is exactly
                // the asymmetry ADR-0052 exists to close.
                let body = comments_stripped(&module);
                for token in quoted_tokens(&body).chain(pkgs_attr_paths(&body)) {
                    if !catalogued.contains(token.as_str()) {
                        drift.push(format!(
                            "{}.nix installs '{token}', which the catalog does not \
                             list: the console cannot show or remove it",
                            set.name
                        ));
                    }
                }
                continue;
            }

            // The bracketed list, with comments stripped.
            let Some(body) = module
                .split_once('[')
                .and_then(|(_, rest)| rest.rsplit_once(']').map(|(list, _)| list.to_string()))
            else {
                drift.push(format!("{}.nix has no package list", set.name));
                continue;
            };
            let body = comments_stripped(&body);
            let (body, removed) = bare_list_only(&body);
            // A parenthesised expression installs whatever it evaluates to,
            // invisibly to the name comparison below — so each one must be a
            // shape this test recognises. Today that is exactly one shape:
            // the `runCommand` helper `network` uses to symlink FRR's daemons
            // onto PATH. Anything else is a package smuggled past the
            // catalog.
            for group in &removed {
                if !group.starts_with("runCommand ") {
                    drift.push(format!(
                        "{}.nix contains the expression `({group})`, which this \
                         test cannot map to catalogued packages; if it is a new \
                         helper, teach the test its shape",
                        set.name
                    ));
                }
            }
            let listed: std::collections::BTreeSet<&str> = body.split_whitespace().collect();
            let catalogued: std::collections::BTreeSet<&str> =
                set.packages.iter().copied().collect();

            for extra in listed.difference(&catalogued) {
                drift.push(format!(
                    "{}.nix installs '{extra}', which the catalog does not list: \
                     the console cannot show or remove it",
                    set.name
                ));
            }
            for missing in catalogued.difference(&listed) {
                drift.push(format!(
                    "{}.nix omits '{missing}', which the catalog has: the console \
                     offers a package the box does not install",
                    set.name
                ));
            }
        }

        assert!(drift.is_empty(), "{}", drift.join("\n"));
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

    /// The set index and the catalog must map the same sets, exactly.
    ///
    /// Substring matching on the raw file passed a commented-out import: the
    /// pathname was still in the text while evaluation would fail. So the
    /// index is parsed — comments stripped, then one `key = import ./name.nix`
    /// binding per line — and compared in both directions: a catalogued set
    /// the index does not import cannot be selected, and an import the
    /// catalog does not know is a module the console cannot toggle.
    #[test]
    fn set_index_imports_every_set() {
        let default = comments_stripped(set_file("default").expect("default.nix is embedded"));

        let mut imported = std::collections::BTreeMap::new();
        for line in default.lines() {
            let Some((key, rest)) = line.split_once('=') else {
                continue;
            };
            let Some(rest) = rest.trim().strip_prefix("import ./") else {
                continue;
            };
            let Some(name) = rest.split(".nix").next() else {
                continue;
            };
            imported.insert(key.trim().to_string(), name.to_string());
        }

        for set in NIX_SETS {
            let key = set.name.replace('-', "_");
            assert_eq!(
                imported.get(&key).map(String::as_str),
                Some(set.name),
                "nix/sets/default.nix does not bind `{key} = import ./{}.nix`, so \
                 selecting the '{}' set would fail to evaluate",
                set.name,
                set.name
            );
        }
        for (key, name) in &imported {
            assert!(
                NIX_SETS.iter().any(|set| set.name == *name),
                "nix/sets/default.nix imports './{name}.nix' as `{key}`, which the \
                 catalog does not know: the console cannot toggle it"
            );
        }
    }

    /// Every catalogued set ships exactly one embedded module, and every
    /// embedded module is either the index or a catalogued set.
    ///
    /// `write_set_modules` and `apply_config` push `NIX_SET_FILES` verbatim
    /// with no generated fallback, so an entry missing from the table is a
    /// module the box silently never receives — the failure mode the removed
    /// fallback used to paper over with a lossy reconstruction.
    #[test]
    fn every_set_ships_exactly_one_module() {
        for set in NIX_SETS {
            assert!(
                set_file(set.name).is_some(),
                "the '{}' set has no entry in NIX_SET_FILES: provisioning and \
                 Sets apply would never write its module",
                set.name
            );
        }
        for (filename, _) in NIX_SET_FILES {
            let name = filename.strip_suffix(".nix").unwrap_or(filename);
            assert!(
                name == "default" || NIX_SETS.iter().any(|set| set.name == name),
                "NIX_SET_FILES ships '{filename}', which is neither the index nor \
                 a catalogued set"
            );
        }
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
