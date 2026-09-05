//! Clean-checkout contract for the files under `examples/`.

use std::path::{Path, PathBuf};

use devbox::sandbox::config::DevboxConfig;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn every_policy_example_is_a_valid_project_config() {
    let directory = root().join("examples/policies");
    let mut count = 0usize;
    for entry in std::fs::read_dir(&directory).expect("read policy examples") {
        let path = entry.expect("policy example entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let config = DevboxConfig::load(&path)
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(config.policy.egress.enforces(), "{}", path.display());
        count += 1;
    }
    assert!(count >= 2, "expected mirror-only and allowlist examples");
}
