use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, Context};
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

    #[serde(default)]
    pub mounts: HashMap<String, MountEntry>,

    #[serde(default)]
    pub resources: ResourcesSection,

    #[serde(default)]
    pub env: HashMap<String, toml::Value>,

    #[serde(default)]
    pub custom_packages: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSection {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    #[serde(default = "default_layout")]
    pub layout: String,

    #[serde(default = "default_mount_mode")]
    pub mount_mode: String,

    #[serde(default = "default_image")]
    pub image: String,
}

impl Default for SandboxSection {
    fn default() -> Self {
        Self {
            runtime: default_runtime(),
            layout: default_layout(),
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
    #[serde(default)]
    pub ai: bool,
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
            ai: false,
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

impl Default for DevboxConfig {
    fn default() -> Self {
        let mut mounts = HashMap::new();
        mounts.insert(
            "workspace".to_string(),
            MountEntry {
                host: ".".to_string(),
                target: "/workspace".to_string(),
                readonly: false,
            },
        );

        Self {
            sandbox: SandboxSection::default(),
            sets: SetsSection::default(),
            languages: LanguagesSection::default(),
            mounts,
            resources: ResourcesSection::default(),
            env: HashMap::new(),
            custom_packages: HashMap::new(),
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
    #[allow(dead_code)]
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .context("Failed to serialize config")?;
        std::fs::write(path, content)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    /// Try to load from the current directory, or return default.
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
    /// Tools like "claude-code", "aider" enable the "ai" set.
    /// Tools like "network", "ai" enable the corresponding set directly.
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
                "ai" | "claude-code" | "claude" | "aider" | "codex" | "ollama" | "opencode" => {
                    self.sets.ai = true;
                }
                _ => {}
            }
        }
    }

    /// Return a list of all active set names.
    pub fn active_sets(&self) -> Vec<String> {
        let mut sets = vec![];
        // Locked sets (always on)
        sets.push("system".to_string());
        sets.push("shell".to_string());
        sets.push("tools".to_string());
        // Toggleable sets
        if self.sets.editor { sets.push("editor".to_string()); }
        if self.sets.git { sets.push("git".to_string()); }
        if self.sets.container { sets.push("container".to_string()); }
        if self.sets.network { sets.push("network".to_string()); }
        if self.sets.ai { sets.push("ai".to_string()); }
        // Language sets
        if self.languages.go { sets.push("lang-go".to_string()); }
        if self.languages.rust { sets.push("lang-rust".to_string()); }
        if self.languages.python { sets.push("lang-python".to_string()); }
        if self.languages.node { sets.push("lang-node".to_string()); }
        if self.languages.java { sets.push("lang-java".to_string()); }
        if self.languages.ruby { sets.push("lang-ruby".to_string()); }
        sets
    }

    /// Return active language names (without "lang-" prefix).
    pub fn active_languages(&self) -> Vec<String> {
        let mut langs = vec![];
        if self.languages.go { langs.push("go".to_string()); }
        if self.languages.rust { langs.push("rust".to_string()); }
        if self.languages.python { langs.push("python".to_string()); }
        if self.languages.node { langs.push("node".to_string()); }
        if self.languages.java { langs.push("java".to_string()); }
        if self.languages.ruby { langs.push("ruby".to_string()); }
        langs
    }
}

fn default_runtime() -> String {
    "auto".to_string()
}
fn default_layout() -> String {
    "default".to_string()
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
        assert!(!config.sets.ai);
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
        assert!(!config.sets.ai);
        config.apply_tools(&["claude-code".to_string()]);
        assert!(config.sets.ai);
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
        config.sets.ai = true;
        config.save(&path).unwrap();

        let loaded = DevboxConfig::load(&path).unwrap();
        assert!(loaded.languages.go);
        assert!(loaded.sets.ai);
        assert_eq!(loaded.sandbox.runtime, "auto");
    }
}
