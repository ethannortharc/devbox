use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Persistent state for a sandbox instance, stored in ~/.devbox/sandboxes/<name>/state.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxState {
    pub name: String,
    pub runtime: String,
    pub project_dir: PathBuf,
    pub created_at: String,
    pub mount_mode: String,
    pub sets: Vec<String>,
    pub languages: Vec<String>,
    /// Base image type: "nixos" or "ubuntu"
    #[serde(default = "default_image")]
    pub image: String,
    /// Ad-hoc nixpkgs attribute paths outside the set catalogue.
    ///
    /// Stored alongside the sets because they are part of the same selection:
    /// without this, an extra package vanishes from the Sets form and is
    /// removed by the next rebuild.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Where each package in `packages` comes from, when it is not nixpkgs.
    ///
    /// `packages` holds names because that is what the checklist and the NixOS
    /// module need. But `devbox use` moves a box to another project, and
    /// `package_pairs` then reads the *new* project's devbox.toml — which has
    /// never heard of a flake package the old one declared, so the source was
    /// silently replaced with `nixpkgs` and reprovisioning installed a
    /// different package under the same name.
    ///
    /// Absent for boxes created before this field existed, which is why
    /// `resolved_packages` reads the list through the selection rather than off
    /// `packages` — this map only ever answered "where does *this* package come
    /// from", so on a box with no packages recorded there was nothing for it to
    /// answer about and the fallback it looked like never existed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub package_sources: std::collections::BTreeMap<String, String>,
    /// Which version of this file's schema wrote it. Zero means "before this
    /// field existed".
    ///
    /// It exists to tell an *absent* field from a legitimately empty one, which
    /// serde cannot: both arrive as an empty `Vec`. `from_state_and_project`
    /// used emptiness as its migration signal and so mistook every box that
    /// genuinely has no custom packages for one predating the field — then
    /// substituted whatever `devbox.toml` happened to declare. Adding a package
    /// to the file was enough to make the Sets tab report it as installed
    /// before anything installed it, and `devbox use`, which repoints a box at
    /// another project entirely, made the substituted list arbitrary.
    ///
    /// A version rather than a bool: the next field added after boxes exist
    /// faces exactly this question, and answering it needs to know *when* a
    /// file was written, not merely that it was written recently.
    #[serde(default)]
    pub schema: u32,
}

/// The schema every save stamps.
///
/// Bump when a field is added whose absence has to be distinguishable from its
/// zero value, and record what each version means here.
///
/// - 0 — before this marker; `packages` and `package_sources` may be absent
///   rather than empty.
/// - 1 — every field written explicitly; empty means empty.
pub const SCHEMA: u32 = 1;

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
        // The same rule `remove` applies, applied on the way in.
        //
        // Guarding only the removal made the two ends disagree: Docker takes a
        // 65-character name, devbox persisted it, and `destroy` then removed
        // the container and refused to remove the state — leaving a box that
        // no longer exists, recorded as existing, blocking recreation under
        // its own name. A name that cannot be cleaned up must not be written.
        if !is_safe_name(&self.name) {
            bail!(
                "refusing to save sandbox state for {:?}: a box name must be 1-64 \
                 characters, not a path component, and free of control characters \
                 — otherwise `devbox destroy` cannot remove it again",
                self.name
            );
        }
        let dir = state_dir.join("sandboxes").join(&self.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create state dir: {}", dir.display()))?;

        let path = dir.join("state.json");
        // Stamped here rather than trusted from the caller: the guarantee is
        // "this file was written by code that writes every field", which is a
        // property of the writer and not of whatever the struct was holding.
        let mut stamped = self.clone();
        stamped.schema = SCHEMA;
        let content =
            serde_json::to_string_pretty(&stamped).context("Failed to serialize sandbox state")?;
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
        // Guarded here, at the `remove_dir_all`, rather than in each caller.
        // A name of `..` joins to the `sandboxes` directory's parent and a
        // name of `.` to `sandboxes` itself — so a crafted request that failed
        // to load a box could still recursively delete every box, or the whole
        // state directory. The route that reaches this is one of several; the
        // dangerous operation is one.
        if !is_safe_name(name) {
            bail!(
                "refusing to remove sandbox state for {name:?}: a box name must not \
                 be a path component"
            );
        }
        let dir = state_dir.join("sandboxes").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove sandbox state: {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Is this a name that can safely be joined onto a directory path?
///
/// Deliberately narrow: everything devbox itself generates satisfies it, and
/// anything that does not is either a mistake or an attempt to escape the
/// state directory. Separators and `.`/`..` are the whole point; the length
/// cap and control-character check keep the rest of the filesystem happy.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_component_is_not_a_box_name() {
        // `..` joins to the parent of `sandboxes`, `.` to `sandboxes` itself —
        // either one turns a failed lookup into a recursive delete of every
        // box devbox knows about.
        for bad in ["..", ".", "", "a/b", "a\\b", "../../etc", "a\0b"] {
            assert!(!is_safe_name(bad), "{bad:?} must be rejected");
        }
        for good in ["myapp", "devbox-e2e", "a_b.c", "box1"] {
            assert!(is_safe_name(good), "{good:?} is a normal box name");
        }
    }

    #[test]
    fn remove_refuses_to_escape_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sandboxes = dir.path().join("sandboxes");
        std::fs::create_dir_all(sandboxes.join("real")).unwrap();

        assert!(SandboxState::remove(dir.path(), "..").is_err());
        assert!(SandboxState::remove(dir.path(), ".").is_err());
        assert!(
            sandboxes.join("real").exists(),
            "a rejected name must not have deleted anything"
        );
    }

    fn test_state() -> SandboxState {
        SandboxState {
            schema: SCHEMA,
            package_sources: Default::default(),
            name: "myapp".to_string(),
            runtime: "lima".to_string(),
            project_dir: PathBuf::from("/Users/test/projects/myapp"),
            created_at: "2026-03-07T12:00:00Z".to_string(),
            mount_mode: "overlay".to_string(),
            sets: vec!["system".into(), "shell".into(), "tools".into()],
            languages: vec!["go".into()],
            image: "nixos".to_string(),
            packages: vec![],
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

#[cfg(test)]
mod writer_audit {
    /// Every path that saves state must record packages before it does.
    ///
    /// `save` stamps `schema`, and that stamp means "every field was written by
    /// code that writes them all". A writer that leaves `packages` untouched
    /// makes the stamp a lie — and because the stamp is exactly what the
    /// migration fallback keys on, the lie is permanent: a v3 box's packages
    /// become unrecoverable the first time any such path runs.
    ///
    /// Round 41 found one writer in that state. There were three. This is the
    /// check that would have found all of them, and it runs over whatever is in
    /// the tree rather than over the list someone remembered to update.
    #[test]
    fn every_state_writer_records_packages_before_stamping_the_schema() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        let mut files = Vec::new();
        collect_rs(&src, &mut files);
        for path in files {
            // `state.rs` defines `save`; it is not a caller of it.
            if path.ends_with("sandbox/state.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable source");
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // A save of sandbox state, as distinct from a config save.
                //
                // Both take the state directory, and a text scan cannot see
                // types — so the global-config writer is excluded by receiver
                // name. That is a real limitation: a `SandboxState` binding
                // literally named `config` would slip past. It is spelled out
                // rather than hidden because the failure mode of this guard is
                // silence, and silence here reads as "every writer is fine".
                if !(line.contains(".save(&manager.state_dir")
                    || line.contains(".save(&self.state_dir"))
                {
                    continue;
                }
                if line.trim_start().starts_with("config.save(") {
                    continue;
                }
                // Look back over the enclosing work for a write to `packages`.
                let from = n.saturating_sub(60);
                let window = lines[from..=n].join("\n");
                if !window.contains(".packages = ") && !window.contains("packages:") {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these save sandbox state — which stamps `schema`, claiming every \
             field was written — without recording `packages`. A box created \
             before that field existed loses them permanently here:\n{}",
            offenders.join("\n")
        );
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
}
