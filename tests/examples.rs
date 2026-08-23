//! Clean-checkout contract for the files under `examples/`.

use std::path::{Path, PathBuf};

use devbox::lab::Lab;
use devbox::sandbox::config::DevboxConfig;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn every_lab_example_parses_plans_and_renders() {
    let directory = root().join("examples/labs");
    let mut count = 0usize;
    for entry in std::fs::read_dir(&directory).expect("read lab examples") {
        let path = entry.expect("lab example entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let lab = Lab::resolve(path.to_str().expect("UTF-8 example path"))
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(!lab.summary().nodes.is_empty(), "{}", path.display());
        assert!(!lab.up_commands().unwrap().is_empty(), "{}", path.display());
        assert!(!lab.down_commands().is_empty(), "{}", path.display());
        assert_eq!(
            lab.router_configs().len(),
            lab.topology.routers().len(),
            "{}",
            path.display()
        );
        count += 1;
    }
    assert!(count >= 2, "expected the routed and ZTP lab examples");
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
