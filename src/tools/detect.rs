use std::path::Path;

/// Detected languages for a project directory.
#[derive(Debug, Default)]
pub struct DetectedLanguages {
    pub go: bool,
    pub rust: bool,
    pub python: bool,
    pub node: bool,
    pub java: bool,
    pub ruby: bool,
}

impl DetectedLanguages {
    pub fn as_set_names(&self) -> Vec<String> {
        let mut sets = vec![];
        if self.go {
            sets.push("lang-go".to_string());
        }
        if self.rust {
            sets.push("lang-rust".to_string());
        }
        if self.python {
            sets.push("lang-python".to_string());
        }
        if self.node {
            sets.push("lang-node".to_string());
        }
        if self.java {
            sets.push("lang-java".to_string());
        }
        if self.ruby {
            sets.push("lang-ruby".to_string());
        }
        sets
    }
}

/// Scan a project directory for language indicators.
pub fn detect_languages(dir: &Path) -> DetectedLanguages {
    let mut detected = DetectedLanguages::default();

    if dir.join("go.mod").exists() || dir.join("go.sum").exists() {
        detected.go = true;
    }
    if dir.join("Cargo.toml").exists() {
        detected.rust = true;
    }
    if dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
        || dir.join("requirements.txt").exists()
        || dir.join("Pipfile").exists()
    {
        detected.python = true;
    }
    if dir.join("package.json").exists() {
        detected.node = true;
    }
    if dir.join("pom.xml").exists()
        || dir.join("build.gradle").exists()
        || dir.join("build.gradle.kts").exists()
    {
        detected.java = true;
    }
    if dir.join("Gemfile").exists() || dir.join(".ruby-version").exists() {
        detected.ruby = true;
    }

    detected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detect_go_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module test").unwrap();
        let detected = detect_languages(dir.path());
        assert!(detected.go);
        assert!(!detected.rust);
    }

    #[test]
    fn detect_rust_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let detected = detect_languages(dir.path());
        assert!(detected.rust);
        assert!(!detected.go);
    }

    #[test]
    fn detect_multi_language() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        let detected = detect_languages(dir.path());
        assert!(detected.node);
        assert!(detected.python);
        assert!(!detected.go);
    }

    #[test]
    fn detect_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let detected = detect_languages(dir.path());
        assert!(!detected.go);
        assert!(!detected.rust);
        assert!(!detected.python);
        assert!(!detected.node);
    }

    #[test]
    fn as_set_names_correct() {
        let detected = DetectedLanguages {
            go: true,
            rust: false,
            python: true,
            node: false,
            java: false,
            ruby: false,
        };
        let names = detected.as_set_names();
        assert_eq!(names, vec!["lang-go", "lang-python"]);
    }
}
