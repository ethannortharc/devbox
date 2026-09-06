use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// devbox.toml — project-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevboxConfig {
    #[serde(default)]
    pub sandbox: SandboxSection,

    #[serde(default)]
    pub sets: SetsSection,

    #[serde(default)]
    pub languages: LanguagesSection,

    /// What to mount into the box.
    ///
    /// `default_mounts`, not `Default::default`, and the difference is the
    /// whole point. `#[serde(default)]` gave an *empty* map when a
    /// `devbox.toml` had no `[mounts]` table, so a hand-written or trimmed
    /// config produced a box with no project mount at all: `/workspace` still
    /// mounted — an overlay whose lower layer is an empty directory mounts
    /// perfectly well — and none of the user's files were in it. Silently.
    ///
    /// A serde default only applies to a *missing* field, so this keeps the
    /// distinction the user is entitled to: no `[mounts]` at all means "the
    /// usual one", and an explicit empty `[mounts]` means "none".
    #[serde(default = "default_mounts")]
    pub mounts: HashMap<String, MountEntry>,

    #[serde(default)]
    pub resources: ResourcesSection,

    #[serde(default)]
    pub env: HashMap<String, toml::Value>,

    #[serde(default)]
    pub custom_packages: HashMap<String, String>,

    /// Egress and activity control (§8, §12.1).
    #[serde(default)]
    pub policy: crate::policy::Policy,

    /// MCP servers this project runs in a box (§7).
    ///
    /// Last, and it has to stay last: `toml` emits a struct's fields in
    /// declaration order, and TOML requires a table's plain values before its
    /// sub-tables. A map of tables placed above `[policy]` would make
    /// [`Self::save`] produce a file it could not read back.
    ///
    /// Written by `devbox mcp add`, which does *not* go through [`Self::save`]
    /// — see [`crate::mcp::registry`] for why.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: crate::mcp::registry::McpTable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSection {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    #[serde(default = "default_mount_mode")]
    pub mount_mode: String,

    #[serde(default = "default_image")]
    pub image: String,
}

impl Default for SandboxSection {
    fn default() -> Self {
        Self {
            runtime: default_runtime(),
            mount_mode: default_mount_mode(),
            image: default_image(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetsSection {
    #[serde(default = "yes")]
    pub system: bool,
    #[serde(default = "yes")]
    pub shell: bool,
    #[serde(default = "yes")]
    pub tools: bool,
    #[serde(default = "yes")]
    pub editor: bool,
    #[serde(default = "yes")]
    pub git: bool,
    #[serde(default = "yes")]
    pub container: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default = "yes")]
    pub ai_code: bool,
    #[serde(default)]
    pub ai_infra: bool,
}

impl SetsSection {
    /// Every set off.
    ///
    /// `Default` is not this: it is the *starting* selection for a new box,
    /// with shell, tools, editor, git, container and ai-code on. Reaching for
    /// `Default::default()` to mean "clear" therefore turns them all back on —
    /// which is how a fix for `devbox upgrade` re-enabling a disabled set came
    /// to re-enable every disabled set. The two meanings need two names.
    /// Turn on the set named as [`DevboxConfig::active_sets`] names it.
    ///
    /// Returns whether the name was one. The exact inverse of that function,
    /// and the two have to stay in step: `upgrade` reconstructs a box's
    /// selection by feeding recorded set names back in, so a name this cannot
    /// read is a set the box silently loses on the next rebuild.
    ///
    /// That is not hypothetical. This did not exist, and `upgrade` used
    /// `apply_tools` — which understands aliases like `claude` and `mosh` but
    /// has no case for `shell`, `tools`, `editor`, `git` or `container`. Those
    /// five were cleared and never restored, so a routine `devbox upgrade`
    /// rebuilt the box without them.
    pub fn enable(&mut self, name: &str) -> bool {
        match name {
            "system" => self.system = true,
            "shell" => self.shell = true,
            "tools" => self.tools = true,
            "editor" => self.editor = true,
            "git" => self.git = true,
            "container" => self.container = true,
            "network" => self.network = true,
            "ai-code" => self.ai_code = true,
            "ai-infra" => self.ai_infra = true,
            _ => return false,
        }
        true
    }

    pub fn none() -> Self {
        Self {
            system: false,
            shell: false,
            tools: false,
            editor: false,
            git: false,
            container: false,
            network: false,
            ai_code: false,
            ai_infra: false,
        }
    }
}

impl Default for SetsSection {
    fn default() -> Self {
        Self {
            system: true,
            shell: true,
            tools: true,
            editor: true,
            git: true,
            container: true,
            network: false,
            ai_code: true,
            ai_infra: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LanguagesSection {
    #[serde(default)]
    pub go: bool,
    #[serde(default)]
    pub rust: bool,
    #[serde(default)]
    pub python: bool,
    #[serde(default)]
    pub node: bool,
    #[serde(default)]
    pub java: bool,
    #[serde(default)]
    pub ruby: bool,
}

impl LanguagesSection {
    /// Turn on the language named as `active_languages` names it — that is,
    /// without the `lang-` prefix `active_sets` adds.
    pub fn enable(&mut self, name: &str) -> bool {
        match name {
            "go" => self.go = true,
            "rust" => self.rust = true,
            "python" => self.python = true,
            "node" => self.node = true,
            "java" => self.java = true,
            "ruby" => self.ruby = true,
            _ => return false,
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountEntry {
    pub host: String,
    pub target: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourcesSection {
    #[serde(default)]
    pub cpu: u32,
    #[serde(default)]
    pub memory: String,
}

/// The mount a box has unless its `devbox.toml` says otherwise: the project
/// directory at `/workspace`.
///
/// The same entry `devbox init` writes, so a generated config and an absent
/// one describe the same box.
pub fn default_mounts() -> HashMap<String, MountEntry> {
    let mut mounts = HashMap::new();
    mounts.insert(
        "workspace".to_string(),
        MountEntry {
            host: ".".to_string(),
            target: "/workspace".to_string(),
            readonly: false,
        },
    );
    mounts
}

impl Default for DevboxConfig {
    fn default() -> Self {
        let mounts = default_mounts();

        Self {
            sandbox: SandboxSection::default(),
            sets: SetsSection::default(),
            languages: LanguagesSection::default(),
            mounts,
            resources: ResourcesSection::default(),
            env: HashMap::new(),
            custom_packages: HashMap::new(),
            policy: crate::policy::Policy::default(),
            mcp: crate::mcp::registry::McpTable::new(),
        }
    }
}

impl DevboxConfig {
    /// Load from devbox.toml at the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(config)
    }

    /// Save to devbox.toml.
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        super::state::write_atomically(path, content.as_bytes(), "devbox config")
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    /// Load an existing `devbox.toml`, or the default when there is none.
    ///
    /// Unlike [`Self::load_or_default`], a file that exists but does not parse
    /// is an **error**. Any path that goes on to *write* the config must use
    /// this: falling back to defaults and then saving would silently erase the
    /// user's mounts, resources, environment, and set selections.
    pub fn load_for_edit(dir: &Path) -> Result<Self> {
        let path = dir.join("devbox.toml");
        if path.exists() {
            Self::load(&path).with_context(|| {
                format!(
                    "{} exists but could not be parsed; refusing to overwrite it",
                    path.display()
                )
            })
        } else {
            Ok(Self::default())
        }
    }

    /// Try to load from the current directory, or return default.
    ///
    /// Read-only callers only: see [`Self::load_for_edit`].
    pub fn load_or_default(dir: &Path) -> Self {
        let path = dir.join("devbox.toml");
        if path.exists() {
            Self::load(&path).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Apply --tools flags: adds to auto-detected languages and enables sets.
    /// Tools like "go", "rust", "python" enable language sets.
    /// Tools like "claude-code", "aider" enable the "ai-code" set.
    /// Tools like "ollama", "mcp-hub" enable the "ai-infra" set.
    /// "ai" enables both ai-code and ai-infra.
    pub fn apply_tools(&mut self, tools: &[String]) {
        for tool in tools {
            match tool.as_str() {
                "go" => self.languages.go = true,
                "rust" => self.languages.rust = true,
                "python" => self.languages.python = true,
                "node" | "nodejs" => self.languages.node = true,
                "java" => self.languages.java = true,
                "ruby" => self.languages.ruby = true,
                "network" | "tailscale" | "mosh" => self.sets.network = true,
                "ai" => {
                    self.sets.ai_code = true;
                    self.sets.ai_infra = true;
                }
                "ai-code" | "coding" | "claude-code" | "claude" | "aider" | "codex"
                | "opencode" => {
                    self.sets.ai_code = true;
                }
                "ai-infra" | "ollama" | "mcp-hub" | "litellm" | "open-webui" => {
                    self.sets.ai_infra = true;
                }
                // Not an alias — try it as a canonical set or language name.
                // Without this `devbox upgrade --tools git` was accepted and
                // did nothing, because the alias table has no case for the
                // names `active_sets` actually emits.
                other => {
                    self.enable_set(other);
                }
            }
        }
    }

    /// Turn on the set or language named as [`Self::active_sets`] names it.
    ///
    /// Returns whether the name was one, so a caller can fall back to the
    /// alias table in [`Self::apply_tools`] for things a *user* might type.
    pub fn enable_set(&mut self, name: &str) -> bool {
        match name.strip_prefix("lang-") {
            Some(language) => self.languages.enable(language),
            None => self.sets.enable(name),
        }
    }

    /// Return a list of all active set names.
    pub fn active_sets(&self) -> Vec<String> {
        // Only `system` is locked (ADR-0012). Forcing shell/tools/editor on
        // here would silently re-enable sets the user unchecked, and the next
        // reprovision would reinstall them.
        let mut sets = vec!["system".to_string()];
        if self.sets.shell {
            sets.push("shell".to_string());
        }
        if self.sets.tools {
            sets.push("tools".to_string());
        }
        if self.sets.editor {
            sets.push("editor".to_string());
        }
        if self.sets.git {
            sets.push("git".to_string());
        }
        if self.sets.container {
            sets.push("container".to_string());
        }
        if self.sets.network {
            sets.push("network".to_string());
        }
        if self.sets.ai_code {
            sets.push("ai-code".to_string());
        }
        if self.sets.ai_infra {
            sets.push("ai-infra".to_string());
        }
        // Language sets
        if self.languages.go {
            sets.push("lang-go".to_string());
        }
        if self.languages.rust {
            sets.push("lang-rust".to_string());
        }
        if self.languages.python {
            sets.push("lang-python".to_string());
        }
        if self.languages.node {
            sets.push("lang-node".to_string());
        }
        if self.languages.java {
            sets.push("lang-java".to_string());
        }
        if self.languages.ruby {
            sets.push("lang-ruby".to_string());
        }
        sets
    }

    /// Return active language names (without "lang-" prefix).
    pub fn active_languages(&self) -> Vec<String> {
        let mut langs = vec![];
        if self.languages.go {
            langs.push("go".to_string());
        }
        if self.languages.rust {
            langs.push("rust".to_string());
        }
        if self.languages.python {
            langs.push("python".to_string());
        }
        if self.languages.node {
            langs.push("node".to_string());
        }
        if self.languages.java {
            langs.push("java".to_string());
        }
        if self.languages.ruby {
            langs.push("ruby".to_string());
        }
        langs
    }
}

fn default_runtime() -> String {
    "auto".to_string()
}
fn default_mount_mode() -> String {
    "overlay".to_string()
}
fn default_image() -> String {
    "nixos".to_string()
}
fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_workspace_mount() {
        let config = DevboxConfig::default();
        assert!(config.mounts.contains_key("workspace"));
        let ws = &config.mounts["workspace"];
        assert_eq!(ws.host, ".");
        assert_eq!(ws.target, "/workspace");
    }

    #[test]
    fn default_sets_are_correct() {
        let config = DevboxConfig::default();
        assert!(config.sets.system);
        assert!(config.sets.shell);
        assert!(config.sets.tools);
        assert!(config.sets.editor);
        assert!(config.sets.git);
        assert!(config.sets.container);
        assert!(!config.sets.network);
        assert!(config.sets.ai_code);
        assert!(!config.sets.ai_infra);
    }

    #[test]
    fn apply_tools_enables_languages() {
        let mut config = DevboxConfig::default();
        config.apply_tools(&["go".to_string(), "python".to_string()]);
        assert!(config.languages.go);
        assert!(config.languages.python);
        assert!(!config.languages.rust);
    }

    #[test]
    fn apply_tools_enables_ai_set() {
        let mut config = DevboxConfig::default();
        assert!(!config.sets.ai_infra);
        config.apply_tools(&["claude-code".to_string()]);
        assert!(config.sets.ai_code);
        // "ai" enables both
        config.apply_tools(&["ai".to_string()]);
        assert!(config.sets.ai_infra);
    }

    #[test]
    fn active_sets_reflects_config() {
        let mut config = DevboxConfig::default();
        config.languages.go = true;
        config.sets.network = true;
        let sets = config.active_sets();
        assert!(sets.contains(&"lang-go".to_string()));
        assert!(sets.contains(&"network".to_string()));
        assert!(sets.contains(&"system".to_string()));
        assert!(!sets.contains(&"lang-rust".to_string()));
    }

    #[test]
    fn roundtrip_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devbox.toml");

        let mut config = DevboxConfig::default();
        config.languages.go = true;
        config.sets.ai_code = true;
        config.save(&path).unwrap();

        let loaded = DevboxConfig::load(&path).unwrap();
        assert!(loaded.languages.go);
        assert!(loaded.sets.ai_code);
        assert_eq!(loaded.sandbox.runtime, "auto");
    }

    /// The bug this default exists for: a `devbox.toml` that says nothing
    /// about mounts used to produce a box with no project mount, and nothing
    /// anywhere said so. The overlay still mounts — its lower layer is just an
    /// empty directory — so the box looks fine and holds none of the user's
    /// files.
    #[test]
    fn a_config_with_no_mounts_table_still_mounts_the_project() {
        let config: DevboxConfig =
            toml::from_str("[sandbox]\nruntime = \"lima\"\n\n[policy]\negress = \"open\"\n")
                .expect("parses");
        assert_eq!(config.mounts.len(), 1, "{:?}", config.mounts);
        let workspace = &config.mounts["workspace"];
        assert_eq!(workspace.host, ".");
        assert_eq!(workspace.target, "/workspace");
        assert!(!workspace.readonly);
    }

    /// Writing the table and leaving it empty is the one way to say "no
    /// mounts", and it has to keep meaning that.
    #[test]
    fn an_empty_mounts_table_means_no_mounts() {
        let config: DevboxConfig =
            toml::from_str("[sandbox]\nruntime = \"lima\"\n\n[mounts]\n").expect("parses");
        assert!(config.mounts.is_empty(), "{:?}", config.mounts);
    }

    /// And a config that names its own mounts gets exactly those — the
    /// default is not merged in underneath them.
    #[test]
    fn a_custom_mounts_table_replaces_the_default() {
        let config: DevboxConfig = toml::from_str(
            "[mounts.cache]\nhost = \"var/cache\"\ntarget = \"/cache\"\nreadonly = true\n",
        )
        .expect("parses");
        assert_eq!(config.mounts.len(), 1, "{:?}", config.mounts);
        assert!(config.mounts.contains_key("cache"));
        assert!(!config.mounts.contains_key("workspace"));
    }
}

#[cfg(test)]
mod set_roundtrip {
    use super::*;

    /// Everything `active_sets` can emit must be readable by `enable_set`.
    ///
    /// These two are a pair, and `upgrade` closes the loop between them: it
    /// reads a box's recorded set names and feeds them back in to rebuild the
    /// selection. A name one side emits and the other cannot read is a set the
    /// box loses on the next rebuild, silently.
    ///
    /// It happened. `upgrade` went through `apply_tools`, an alias table with
    /// no case for `shell`, `tools`, `editor`, `git` or `container`, so those
    /// five were cleared and never restored. Asserting the round trip covers
    /// every set at once, including ones added later.
    #[test]
    fn every_name_active_sets_emits_can_be_read_back() {
        // Everything on, so `active_sets` emits the full vocabulary.
        let all = DevboxConfig {
            sets: SetsSection {
                system: true,
                shell: true,
                tools: true,
                editor: true,
                git: true,
                container: true,
                network: true,
                ai_code: true,
                ai_infra: true,
            },
            languages: LanguagesSection {
                go: true,
                rust: true,
                python: true,
                node: true,
                java: true,
                ruby: true,
            },
            ..Default::default()
        };
        let names = all.active_sets();
        assert!(names.len() >= 15, "expected the full vocabulary: {names:?}");

        let mut rebuilt = DevboxConfig {
            sets: SetsSection::none(),
            languages: LanguagesSection::default(),
            ..Default::default()
        };
        for name in &names {
            assert!(
                rebuilt.enable_set(name),
                "`active_sets` emits {name:?} and `enable_set` cannot read it — \
                 a box carrying that set loses it on the next upgrade"
            );
        }

        // And the round trip is lossless, not merely accepting.
        let mut back = rebuilt.active_sets();
        let mut expected = names.clone();
        back.sort();
        expected.sort();
        assert_eq!(back, expected, "the selection changed on the way round");
    }

    #[test]
    fn a_set_name_typed_as_a_tool_still_works() {
        // `devbox upgrade --tools git` was accepted and did nothing.
        let mut config = DevboxConfig {
            sets: SetsSection::none(),
            ..Default::default()
        };
        config.apply_tools(&["git".into(), "editor".into(), "rust".into()]);
        assert!(config.sets.git, "--tools git must enable the git set");
        assert!(config.sets.editor);
        assert!(config.languages.rust);
    }
}
