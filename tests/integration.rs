//! Integration tests for devbox CLI.
//!
//! These tests run the compiled binary and verify CLI behavior.
//! They do NOT require a running VM or runtime — they test CLI parsing,
//! help output, config commands, and error handling.

use std::process::Command;

fn devbox() -> Command {
    Command::new(env!("CARGO_BIN_EXE_devbox"))
}

#[test]
fn version_flag() {
    let output = devbox().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("devbox"));
}

#[test]
fn help_flag() {
    let output = devbox().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Isolated developer VMs"));
    assert!(stdout.contains("create"));
    assert!(stdout.contains("shell"));
    assert!(stdout.contains("guide"));
}

#[test]
fn subcommand_help() {
    let subcommands = [
        "create",
        "shell",
        "exec",
        "stop",
        "destroy",
        "list",
        "status",
        "snapshot",
        "upgrade",
        "config",
        "doctor",
        "layout",
        "packages",
        "prune",
        "init",
        "nix",
        "commit",
        "diff",
        "discard",
        "guide",
        "self-update",
    ];

    for cmd in subcommands {
        let output = devbox().args([cmd, "--help"]).output().unwrap();
        assert!(
            output.status.success(),
            "'{cmd} --help' failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn guide_index() {
    let output = devbox().arg("guide").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("zellij"));
    assert!(stdout.contains("lazygit"));
    assert!(stdout.contains("nvim"));
}

#[test]
fn guide_specific_tool() {
    let output = devbox().args(["guide", "git"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("git"));
}

#[test]
fn guide_unknown_tool() {
    let output = devbox()
        .args(["guide", "nonexistent-tool"])
        .output()
        .unwrap();
    assert!(output.status.success()); // exits 0, prints to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No cheat sheet"));
}

#[test]
fn config_show() {
    let output = devbox().args(["config", "show"]).output().unwrap();
    // May succeed or fail depending on state, but should not panic
    assert!(output.status.success() || !String::from_utf8_lossy(&output.stderr).contains("panic"));
}

#[test]
fn list_empty() {
    let output = devbox().arg("list").output().unwrap();
    // Should succeed even with no sandboxes
    assert!(output.status.success());
}

#[test]
fn list_json_format() {
    let output = devbox()
        .args(["list", "--output", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn doctor_runs() {
    let output = devbox().arg("doctor").output().unwrap();
    // Doctor should always succeed (it reports issues, doesn't fail)
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Runtime"));
}

#[test]
fn layout_list() {
    let output = devbox().args(["layout", "list"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("default"));
    assert!(stdout.contains("ai-pair"));
}

#[test]
fn layout_preview() {
    let output = devbox()
        .args(["layout", "preview", "default"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // ASCII preview should contain box characters or layout description
    assert!(!stdout.is_empty());
}

#[test]
fn self_update_check_flag() {
    // --check should not crash (may fail without network, that's ok)
    let output = devbox().args(["self-update", "--check"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should at least print current version
    assert!(stdout.contains("Current version"));
}

#[test]
fn invalid_subcommand() {
    let output = devbox().arg("nonexistent").output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn exec_requires_command() {
    let output = devbox().arg("exec").output().unwrap();
    // Should fail because no command provided
    assert!(!output.status.success());
}
