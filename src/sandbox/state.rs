use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persistent state for a sandbox instance, stored in ~/.devbox/sandboxes/<name>/state.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxState {
    pub name: String,
    pub runtime: String,
    pub project_dir: PathBuf,
    pub created_at: String,
    pub mount_mode: String,
    pub layout: String,
    pub sets: Vec<String>,
    pub languages: Vec<String>,
    /// Base image type: "nixos" or "ubuntu"
    #[serde(default = "default_image")]
    pub image: String,
}

fn default_image() -> String {
    "nixos".to_string()
}

impl SandboxState {
    /// Load state from a sandbox directory.
    pub fn load(state_dir: &Path, name: &str) -> Result<Self> {
        let path = state_dir.join("sandboxes").join(name).join("state.json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read sandbox state: {}", path.display()))?;
        let state: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse sandbox state: {}", path.display()))?;
        Ok(state)
    }

    /// Save state to the sandbox directory.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let dir = state_dir.join("sandboxes").join(&self.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create state dir: {}", dir.display()))?;

        let path = dir.join("state.json");
        let content =
            serde_json::to_string_pretty(self).context("Failed to serialize sandbox state")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write sandbox state: {}", path.display()))?;
        Ok(())
    }

    /// List all saved sandbox states.
    pub fn list_all(state_dir: &Path) -> Result<Vec<Self>> {
        let sandboxes_dir = state_dir.join("sandboxes");
        if !sandboxes_dir.exists() {
            return Ok(vec![]);
        }

        let mut states = vec![];
        for entry in std::fs::read_dir(&sandboxes_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(state) = Self::load(state_dir, &name) {
                    states.push(state);
                }
            }
        }
        Ok(states)
    }

    /// Remove sandbox state.
    pub fn remove(state_dir: &Path, name: &str) -> Result<()> {
        let dir = state_dir.join("sandboxes").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove sandbox state: {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> SandboxState {
        SandboxState {
            name: "myapp".to_string(),
            runtime: "lima".to_string(),
            project_dir: PathBuf::from("/Users/test/projects/myapp"),
            created_at: "2026-03-07T12:00:00Z".to_string(),
            mount_mode: "overlay".to_string(),
            layout: "default".to_string(),
            sets: vec!["system".into(), "shell".into(), "tools".into()],
            languages: vec!["go".into()],
            image: "nixos".to_string(),
        }
    }

    #[test]
    fn save_and_load_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state();

        state.save(dir.path()).unwrap();
        let loaded = SandboxState::load(dir.path(), "myapp").unwrap();

        assert_eq!(loaded.name, "myapp");
        assert_eq!(loaded.runtime, "lima");
        assert_eq!(loaded.languages, vec!["go"]);
    }

    #[test]
    fn list_all_states() {
        let dir = tempfile::tempdir().unwrap();

        let mut s1 = test_state();
        s1.name = "app1".to_string();
        s1.save(dir.path()).unwrap();

        let mut s2 = test_state();
        s2.name = "app2".to_string();
        s2.save(dir.path()).unwrap();

        let all = SandboxState::list_all(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn remove_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state();
        state.save(dir.path()).unwrap();

        assert!(dir.path().join("sandboxes/myapp").exists());
        SandboxState::remove(dir.path(), "myapp").unwrap();
        assert!(!dir.path().join("sandboxes/myapp").exists());
    }

    #[test]
    fn list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let all = SandboxState::list_all(dir.path()).unwrap();
        assert!(all.is_empty());
    }
}
