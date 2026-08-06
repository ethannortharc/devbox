//! Nix composition — turning a checklist into a `configuration.nix`.
//!
//! v3 baked a fixed package list at provision time. v4 makes the set list a
//! live selection (§6.3): the console renders a checklist, the selection
//! composes a `configuration.nix` that imports **only** the checked set
//! modules, and `nixos-rebuild switch` builds exactly that closure.
//!
//! Everything here is pure. The composition is the part most worth testing —
//! a wrong import list silently ships the wrong box — so it is separated from
//! the I/O that pushes files and runs the rebuild.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{Result, bail};

use super::sets::{NIX_SETS, find_set};
use crate::sandbox::config::DevboxConfig;
use crate::sandbox::state::SandboxState;

/// Sets that cannot be switched off.
///
/// Only `system` is locked, and only because a box without coreutils, a
/// certificate bundle, or a compiler toolchain is not a developer box — it is
/// a broken one. Everything else, including the shell and editor sets, is the
/// user's call: "nothing heavy is built until asked for" (G5) is worth more
/// than a comfortable default nobody can turn off.
pub const LOCKED_SETS: &[&str] = &["system"];

/// A selection of Nix sets plus ad-hoc packages.
///
/// Ordered containers on purpose: composition output must be byte-identical
/// for the same selection, or every rebuild would look like a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Canonical set names, e.g. `system`, `git`, `lang-go`.
    pub sets: BTreeSet<String>,
    /// Extra nixpkgs attribute paths, e.g. `hyperfine`, `python312Packages.ipython`.
    pub packages: BTreeSet<String>,
}

impl Selection {
    /// Build a selection from an explicit list of set names and packages,
    /// forcing the locked sets on.
    pub fn new<I, J>(sets: I, packages: J) -> Self
    where
        I: IntoIterator<Item = String>,
        J: IntoIterator<Item = String>,
    {
        let mut sel = Self {
            sets: sets.into_iter().collect(),
            packages: packages
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        };
        for locked in LOCKED_SETS {
            sel.sets.insert((*locked).to_string());
        }
        sel
    }

    /// The selection a box currently has, read from its persisted state.
    ///
    /// State stores sets and languages separately; languages are just sets
    /// with a `lang-` prefix, so they are folded together here.
    pub fn from_state(state: &SandboxState) -> Self {
        let langs = state.languages.iter().map(|l| format!("lang-{l}"));
        Self::new(state.sets.iter().cloned().chain(langs), std::iter::empty())
    }

    /// Reject anything that would produce a `configuration.nix` Nix cannot
    /// evaluate.
    pub fn validate(&self) -> Result<()> {
        for name in &self.sets {
            if find_set(name).is_none() {
                bail!("unknown set '{name}'");
            }
        }
        for pkg in &self.packages {
            if !is_valid_attr_path(pkg) {
                bail!("'{pkg}' is not a valid nixpkgs attribute path");
            }
        }
        for locked in LOCKED_SETS {
            if !self.sets.contains(*locked) {
                bail!("set '{locked}' cannot be disabled");
            }
        }
        Ok(())
    }

    /// Set names without the `lang-` prefix, in catalogue order.
    pub fn languages(&self) -> Vec<String> {
        NIX_SETS
            .iter()
            .filter_map(|s| s.name.strip_prefix("lang-"))
            .filter(|l| self.sets.contains(&format!("lang-{l}")))
            .map(str::to_string)
            .collect()
    }

    /// Non-language set names, in catalogue order.
    pub fn plain_sets(&self) -> Vec<String> {
        NIX_SETS
            .iter()
            .map(|s| s.name)
            .filter(|n| !n.starts_with("lang-"))
            .filter(|n| self.sets.contains(*n))
            .map(str::to_string)
            .collect()
    }

    /// Project the selection back onto a [`DevboxConfig`], which is what the
    /// existing provisioning path consumes.
    pub fn to_config(&self, base: &DevboxConfig) -> DevboxConfig {
        let mut config = base.clone();
        let has = |n: &str| self.sets.contains(n);

        config.sets.system = has("system");
        config.sets.shell = has("shell");
        config.sets.tools = has("tools");
        config.sets.editor = has("editor");
        config.sets.git = has("git");
        config.sets.container = has("container");
        config.sets.network = has("network");
        config.sets.ai_code = has("ai-code");
        config.sets.ai_infra = has("ai-infra");

        config.languages.go = has("lang-go");
        config.languages.rust = has("lang-rust");
        config.languages.python = has("lang-python");
        config.languages.node = has("lang-node");
        config.languages.java = has("lang-java");
        config.languages.ruby = has("lang-ruby");

        config.custom_packages = self
            .packages
            .iter()
            .map(|p| (p.clone(), "nixpkgs".to_string()))
            .collect();

        config
    }

    /// Every package the selection resolves to, deduplicated and sorted.
    ///
    /// Useful for showing "this adds 6 packages" before committing to a
    /// rebuild, and for asserting in tests that a toggle changed the closure.
    pub fn resolved_packages(&self) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for name in &self.sets {
            if let Some(set) = find_set(name) {
                out.extend(set.packages.iter().map(|p| (*p).to_string()));
            }
        }
        out.extend(self.packages.iter().cloned());
        out.into_iter().collect()
    }
}

/// A nixpkgs attribute path: dotted segments of `[A-Za-z0-9_-]`.
///
/// Deliberately strict. The value is interpolated into a Nix expression that
/// runs as root inside the box, so anything that could close a bracket, start
/// a string, or open a shell must never get through.
pub fn is_valid_attr_path(attr: &str) -> bool {
    !attr.is_empty()
        && attr.len() <= 128
        && attr.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// A Nix attribute-set key for a set name (`lang-go` → `lang_go`).
///
/// Mirrors [`super::sets::generate_sets_default_nix`], which keys the set
/// index the same way.
pub fn nix_key(set_name: &str) -> String {
    set_name.replace('-', "_")
}

/// Compose the `configuration.nix` fragment for a selection.
///
/// The output imports the set index once and concatenates only the selected
/// lists, so an unchecked set is not merely filtered out of the final package
/// list — its expression is never evaluated and its closure is never built.
pub fn compose_configuration_nix(selection: &Selection) -> String {
    let mut nix = String::new();
    nix.push_str(
        "# Auto-generated by devbox — do not edit.\n\
         # Regenerated from the set checklist on every rebuild.\n\
         { pkgs, ... }:\n\
         let\n  \
           sets = import ./sets { inherit pkgs; };\n\
         in\n\
         {\n  \
           environment.systemPackages =\n",
    );

    // Catalogue order, not selection order, so the file reads the same way
    // every time and diffs stay small.
    let chosen: Vec<&str> = NIX_SETS
        .iter()
        .map(|s| s.name)
        .filter(|n| selection.sets.contains(*n))
        .collect();

    if chosen.is_empty() {
        nix.push_str("    []\n");
    } else {
        for (i, name) in chosen.iter().enumerate() {
            let joiner = if i == 0 { "   " } else { "    ++" };
            let _ = writeln!(nix, "{joiner} sets.{}", nix_key(name));
        }
    }

    if !selection.packages.is_empty() {
        nix.push_str("    ++ (with pkgs; [\n");
        for pkg in &selection.packages {
            let _ = writeln!(nix, "      {pkg}");
        }
        nix.push_str("    ])\n");
    }

    nix.push_str("  ;\n}\n");
    nix
}

/// A human-readable summary of what changed between two selections.
pub fn describe_change(before: &Selection, after: &Selection) -> String {
    let added: Vec<&str> = after
        .sets
        .iter()
        .filter(|s| !before.sets.contains(*s))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = before
        .sets
        .iter()
        .filter(|s| !after.sets.contains(*s))
        .map(String::as_str)
        .collect();

    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("+{}", added.join(" +")));
    }
    if !removed.is_empty() {
        parts.push(format!("-{}", removed.join(" -")));
    }

    let pkg_delta = after.packages.len() as i64 - before.packages.len() as i64;
    if pkg_delta != 0 {
        parts.push(format!("{pkg_delta:+} package(s)"));
    }

    if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(names: &[&str]) -> Selection {
        Selection::new(names.iter().map(|s| s.to_string()), std::iter::empty())
    }

    #[test]
    fn locked_sets_are_forced_on() {
        let s = sel(&["git"]);
        assert!(s.sets.contains("system"), "system must be forced on");
        assert!(s.sets.contains("git"));
    }

    #[test]
    fn validate_rejects_unknown_sets() {
        let mut s = sel(&["git"]);
        s.sets.insert("not-a-set".into());
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("not-a-set"), "got: {err}");
    }

    #[test]
    fn validate_rejects_a_missing_locked_set() {
        let mut s = sel(&["git"]);
        s.sets.remove("system");
        assert!(s.validate().unwrap_err().to_string().contains("system"));
    }

    #[test]
    fn attr_paths_are_strictly_validated() {
        for good in [
            "hyperfine",
            "python312Packages.ipython",
            "nodePackages.typescript-language-server",
            "yq-go",
            "jdk21",
        ] {
            assert!(is_valid_attr_path(good), "{good} should be valid");
        }
        // Anything that could break out of the expression must be rejected.
        for bad in [
            "",
            ".",
            "a..b",
            "a.",
            "pkgs; rm -rf /",
            "foo]",
            "foo\"bar",
            "foo\nbar",
            "foo bar",
            "$(whoami)",
            "../etc/passwd",
        ] {
            assert!(!is_valid_attr_path(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn validate_rejects_injectable_packages() {
        let s = Selection::new(
            ["system".to_string()],
            ["hyperfine ]; environment.etc.\"x\".text = \"pwned\"; #".to_string()],
        );
        assert!(s.validate().is_err());
    }

    #[test]
    fn composition_imports_only_selected_sets() {
        let nix = compose_configuration_nix(&sel(&["system", "git", "lang-go"]));

        assert!(nix.contains("sets.system"));
        assert!(nix.contains("sets.git"));
        assert!(nix.contains("sets.lang_go"), "dashes become underscores");
        // Unselected sets must not appear at all — they must never be
        // evaluated, not merely filtered later.
        assert!(!nix.contains("sets.ai_infra"));
        assert!(!nix.contains("sets.lang_rust"));
        assert!(!nix.contains("sets.network"));
    }

    #[test]
    fn composition_is_valid_nix_shape() {
        let nix = compose_configuration_nix(&sel(&["system", "shell"]));
        assert!(nix.starts_with("# Auto-generated by devbox"));
        assert!(nix.contains("{ pkgs, ... }:"));
        assert!(nix.contains("import ./sets { inherit pkgs; }"));
        assert!(nix.contains("environment.systemPackages ="));
        assert!(nix.trim_end().ends_with('}'));
        // The first entry has no `++`, later ones do.
        let body = nix.split("environment.systemPackages =").nth(1).unwrap();
        assert_eq!(body.matches("++").count(), 1, "two sets → one ++: {body}");
    }

    #[test]
    fn composition_is_deterministic_and_catalogue_ordered() {
        let a = Selection::new(
            ["lang-go", "system", "git"].map(String::from),
            std::iter::empty(),
        );
        let b = Selection::new(
            ["git", "lang-go", "system"].map(String::from),
            std::iter::empty(),
        );
        assert_eq!(compose_configuration_nix(&a), compose_configuration_nix(&b));

        // `system` is first in the catalogue, so it is first in the output.
        let nix = compose_configuration_nix(&a);
        let sys = nix.find("sets.system").unwrap();
        let git = nix.find("sets.git").unwrap();
        let go = nix.find("sets.lang_go").unwrap();
        assert!(sys < git && git < go, "catalogue order: {nix}");
    }

    #[test]
    fn composition_includes_ad_hoc_packages() {
        let s = Selection::new(
            ["system".to_string()],
            ["hyperfine".to_string(), "tokei".to_string()],
        );
        let nix = compose_configuration_nix(&s);
        assert!(nix.contains("with pkgs; ["));
        assert!(nix.contains("hyperfine"));
        assert!(nix.contains("tokei"));
    }

    #[test]
    fn a_selection_of_only_locked_sets_still_composes() {
        let nix = compose_configuration_nix(&sel(&[]));
        assert!(nix.contains("sets.system"));
        assert!(!nix.contains("++ sets."), "nothing to concatenate");
    }

    #[test]
    fn round_trips_through_sandbox_state() {
        let state = SandboxState {
            name: "x".into(),
            runtime: "docker".into(),
            project_dir: "/tmp".into(),
            created_at: String::new(),
            mount_mode: "overlay".into(),
            sets: vec!["system".into(), "git".into(), "lang-go".into()],
            languages: vec!["go".into()],
            image: "nixos".into(),
        };
        let s = Selection::from_state(&state);
        assert!(s.sets.contains("git"));
        assert!(s.sets.contains("lang-go"));
        assert_eq!(s.languages(), vec!["go"]);
        assert_eq!(s.plain_sets(), vec!["system", "git"]);
    }

    #[test]
    fn projects_onto_devbox_config() {
        let s = Selection::new(
            ["system", "git", "lang-rust"].map(String::from),
            ["hyperfine".to_string()],
        );
        let config = s.to_config(&DevboxConfig::default());

        assert!(config.sets.git);
        assert!(!config.sets.ai_infra);
        assert!(config.languages.rust);
        assert!(!config.languages.go);
        assert!(config.custom_packages.contains_key("hyperfine"));
        // active_sets() is what provisioning consumes; it must agree.
        let active = config.active_sets();
        assert!(active.contains(&"lang-rust".to_string()));
        assert!(!active.contains(&"lang-go".to_string()));
    }

    #[test]
    fn resolved_packages_are_deduplicated_and_sorted() {
        let s = Selection::new(
            ["system", "lang-python"].map(String::from),
            ["hyperfine".to_string()],
        );
        let pkgs = s.resolved_packages();

        assert!(pkgs.contains(&"coreutils".to_string()));
        assert!(pkgs.contains(&"python312".to_string()));
        assert!(pkgs.contains(&"hyperfine".to_string()));

        let mut sorted = pkgs.clone();
        sorted.sort();
        assert_eq!(pkgs, sorted, "output must be sorted");
        let unique: BTreeSet<_> = pkgs.iter().collect();
        assert_eq!(unique.len(), pkgs.len(), "output must be deduplicated");
    }

    #[test]
    fn toggling_a_set_changes_the_closure() {
        let without = sel(&["system"]);
        let with = sel(&["system", "network"]);
        assert!(!without.resolved_packages().contains(&"nmap".to_string()));
        assert!(with.resolved_packages().contains(&"nmap".to_string()));
    }

    #[test]
    fn describes_what_changed() {
        let before = sel(&["system", "git"]);
        let after = sel(&["system", "lang-go"]);
        let desc = describe_change(&before, &after);
        assert!(desc.contains("+lang-go"), "got: {desc}");
        assert!(desc.contains("-git"), "got: {desc}");

        assert_eq!(describe_change(&before, &before), "no changes");

        let more_pkgs = Selection::new(["system".to_string()], ["tokei".to_string()]);
        assert!(describe_change(&sel(&["system"]), &more_pkgs).contains("+1 package"));
    }
}
