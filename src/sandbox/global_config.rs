use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Global devbox defaults stored at ~/.devbox/config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub default: GlobalDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalDefaults {
    #[serde(default = "default_runtime")]
    pub runtime: String,

    #[serde(default = "default_layout")]
    pub layout: String,

    #[serde(default)]
    pub tools: Vec<String>,
}

impl Default for GlobalDefaults {
    fn default() -> Self {
        Self {
            runtime: default_runtime(),
            layout: default_layout(),
            tools: vec![],
        }
    }
}

impl GlobalConfig {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let path = state_dir.join("config.toml");
        let content = toml::to_string_pretty(self).context("Failed to serialize global config")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "default.runtime" => Some(self.default.runtime.clone()),
            "default.layout" => Some(self.default.layout.clone()),
            "default.tools" => {
                if self.default.tools.is_empty() {
                    Some(String::new())
                } else {
                    Some(self.default.tools.join(","))
                }
            }
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "default.runtime" => {
                let valid = ["auto", "incus", "lima", "multipass", "docker"];
                if !valid.contains(&value) {
                    anyhow::bail!("Invalid runtime '{}'. Options: {}", value, valid.join(", "));
                }
                self.default.runtime = value.to_string();
            }
            "default.layout" => {
                self.default.layout = value.to_string();
            }
            "default.tools" => {
                self.default.tools = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {
                anyhow::bail!(
                    "Unknown config key '{}'. Available keys: default.runtime, default.layout, default.tools",
                    key
                );
            }
        }
        Ok(())
    }
}

fn default_runtime() -> String {
    "auto".to_string()
}
fn default_layout() -> String {
    "default".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_global_config() {
        let config = GlobalConfig::default();
        assert_eq!(config.default.runtime, "auto");
        assert_eq!(config.default.layout, "default");
        assert!(config.default.tools.is_empty());
    }

    #[test]
    fn set_and_get() {
        let mut config = GlobalConfig::default();
        config.set("default.runtime", "lima").unwrap();
        assert_eq!(config.get("default.runtime").unwrap(), "lima");
    }

    #[test]
    fn set_invalid_runtime() {
        let mut config = GlobalConfig::default();
        assert!(config.set("default.runtime", "invalid").is_err());
    }

    #[test]
    fn set_tools_csv() {
        let mut config = GlobalConfig::default();
        config.set("default.tools", "go,rust,claude-code").unwrap();
        assert_eq!(config.default.tools, vec!["go", "rust", "claude-code"]);
        assert_eq!(config.get("default.tools").unwrap(), "go,rust,claude-code");
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = GlobalConfig::default();
        config.set("default.runtime", "docker").unwrap();
        config.set("default.layout", "ai-pair").unwrap();
        config.save(dir.path()).unwrap();

        let loaded = GlobalConfig::load(dir.path()).unwrap();
        assert_eq!(loaded.default.runtime, "docker");
        assert_eq!(loaded.default.layout, "ai-pair");
    }

    #[test]
    fn load_nonexistent_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = GlobalConfig::load(dir.path()).unwrap();
        assert_eq!(config.default.runtime, "auto");
    }
}
