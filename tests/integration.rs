//! Integration tests for devbox CLI.
//!
//! These tests run the compiled binary and verify CLI behavior.
//! They do NOT require a running VM or runtime — they test CLI parsing,
//! help output, config commands, and error handling.

use std::process::Command;
use std::time::{Duration, Instant};

// The collector is a separate copy of the debug binary. On macOS and on
// loaded CI hosts, launching that binary can take substantially longer when
// the rest of this process-based integration suite is running in parallel.
// This is only the test's process-start allowance; the production replacement
// deadline remains intentionally bounded in `obs::daemon`.
const COLLECTOR_TEST_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn completed_v4_progress_keeps_the_overnight_termination_sentinel() {
    let progress = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/PROGRESS.md"));
    assert_eq!(
        progress.lines().next(),
        Some("ALL PHASES DONE"),
        "run-overnight.sh reads only PROGRESS.md line 1 to stop spawning review sessions"
    );
}

fn devbox() -> Command {
    Command::new(env!("CARGO_BIN_EXE_devbox"))
}

fn collector_identity(path: &std::path::Path) -> Option<(i32, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid = text
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))?
        .parse()
        .ok()?;
    let version = text
        .split_whitespace()
        .find_map(|field| field.strip_prefix("version="))?
        .to_string();
    Some((pid, version))
}

fn wait_for_collector(path: &std::path::Path) -> (i32, String) {
    let deadline = Instant::now() + COLLECTOR_TEST_TIMEOUT;
    loop {
        if let Some(identity) = collector_identity(path) {
            return identity;
        }
        assert!(
            Instant::now() < deadline,
            "collector never claimed the lock"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_replacement(path: &std::path::Path, old_pid: i32) -> (i32, String) {
    let deadline = Instant::now() + COLLECTOR_TEST_TIMEOUT;
    loop {
        if let Some(identity) = collector_identity(path)
            && identity.0 != old_pid
            && identity.1 == env!("CARGO_PKG_VERSION")
        {
            return identity;
        }
        assert!(
            Instant::now() < deadline,
            "replacement collector never claimed the lock"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Stops whichever collector owns the lock when the test ends.
///
/// The pid is recorded as the test observes it, not read back at drop time.
/// The temporary home is declared first, so it is deleted first — and reading
/// the identity file *after* that found nothing to signal, leaving the
/// replacement daemon running after every run. Strays accumulate, slow the
/// machine, and make the next run's timings look like a regression.
#[derive(Default)]
struct CollectorCleanup {
    pids: std::cell::RefCell<Vec<i32>>,
}

impl CollectorCleanup {
    fn watch(&self, pid: i32) {
        self.pids.borrow_mut().push(pid);
    }

    /// Stop watching a process the test has already reaped.
    ///
    /// Signalling a reaped pid is not harmless: the number is free for reuse
    /// the moment it is waited on, so a teardown that fires later can land on
    /// an unrelated process.
    fn forget(&self, pid: i32) {
        self.pids.borrow_mut().retain(|watched| *watched != pid);
    }
}

impl Drop for CollectorCleanup {
    fn drop(&mut self) {
        for pid in self.pids.borrow().iter() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
}

#[test]
fn version_flag() {
    let output = devbox().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("devbox"));
}

#[test]
fn a_lifecycle_command_replaces_an_outdated_collector_daemon() {
    let home = tempfile::tempdir().expect("temporary home");
    let identity = home.path().join(".devbox/locks/collector-daemon.owner");
    let cleanup = CollectorCleanup::default();

    let mut original = devbox();
    let mut original = original
        .arg("__collector")
        .env("HOME", home.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start original collector");
    // Recorded at spawn, not after it claims the lock: if that wait times out
    // and panics, the child is dropped without ever being killed.
    cleanup.watch(original.id() as i32);
    let (old_pid, _) = wait_for_collector(&identity);

    // The process still owns the advisory lock; only its published release is
    // made stale, exactly as it is after replacing the devbox executable.
    std::fs::write(&identity, format!("pid={old_pid} version=0.0.0-old\n"))
        .expect("publish simulated old release");

    // Any command that `needs_collector` will do; `watch` on a box that does
    // not exist is the cheapest one that still succeeds, so a failure here is
    // the replacement and not the trigger.
    let trigger = devbox()
        .args(["watch", "no-such-box"])
        .env("HOME", home.path())
        .output()
        .expect("run lifecycle command");
    assert!(
        trigger.status.success(),
        "replacement trigger failed: {}",
        String::from_utf8_lossy(&trigger.stderr)
    );

    let (new_pid, new_version) = wait_for_replacement(&identity, old_pid);
    cleanup.watch(new_pid);
    assert_ne!(new_pid, old_pid, "the outdated process still owns the lock");
    assert_eq!(new_version, env!("CARGO_PKG_VERSION"));
    original.wait().expect("reap original collector");
    cleanup.forget(original.id() as i32);
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
    assert!(stdout.contains("web"));
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
fn devbox_guide_documents_the_real_use_command_shape() {
    let output = devbox().args(["guide", "devbox"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cd /path/to/project && devbox use <name>"));
    assert!(stdout.contains("Lima/Incus"));
    assert!(!stdout.contains("devbox use /path/to/project"));
}

#[test]
fn shipped_guidance_uses_positional_lifecycle_box_names() {
    let readme = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
    let quickstart = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/QUICKSTART.md"));
    let reprovision = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/cli/reprovision.rs"
    ));
    let sandbox = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sandbox/mod.rs"));
    // v5 made the box a positional everywhere, so guidance that still spells it
    // `--name` teaches the deprecated form. These four were the ones a failing
    // provision, a blocked `devbox use`, and the E2E runbook actually printed.
    let provision = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/sandbox/provision.rs"
    ));
    let use_cmd = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/use_cmd.rs"));
    let e2e = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/E2E_TEST_GUIDE.md"
    ));
    let devbox_guide = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/help/devbox.md"));
    for stale in [
        "devbox shell --name",
        "devbox stop --name",
        "devbox destroy --name",
        "devbox destroy --force --name",
        "devbox shell --writable",
        "devbox exec --name",
        "devbox diff --name",
        "devbox discard --name",
        "devbox commit --name",
        "devbox status --name",
        "devbox upgrade --name",
        "devbox reprovision --name",
        "devbox code --name",
        "devbox watch --name",
        "devbox layer commit --name",
        "devbox layer discard --name",
        "devbox layer stash --name",
        "devbox layer stash-pop --name",
        "devbox layer refresh --name",
        "devbox layer status --name",
        "devbox layer diff --name",
        "devbox snapshot list --name",
        "devbox nix add --name",
        "devbox nix remove --name",
        "devbox behavior summary --name",
        "devbox sets apply --name",
        "devbox policy show --name",
        "devbox policy set --name",
        "--sandbox",
    ] {
        for (source, contents) in [
            ("README", readme),
            ("quickstart", quickstart),
            ("reprovision guidance", reprovision),
            ("sandbox recovery guidance", sandbox),
            ("provision retry guidance", provision),
            ("use guidance", use_cmd),
            ("E2E guide", e2e),
            ("devbox cheat sheet", devbox_guide),
        ] {
            assert!(
                !contents.contains(stale),
                "{source} still documents {stale}"
            );
        }
    }
}

#[test]
fn lima_readiness_guidance_names_both_diagnostic_logs() {
    let lima = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime/lima.rs"));
    assert!(lima.contains("ha.stderr.log"));
    assert!(lima.contains("serial*.log"));
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
fn retired_commands_are_gone() {
    // v4 retires the TUI and the Zellij layout manager (§5). Both must fail
    // as unknown subcommands rather than lingering as no-ops.
    for cmd in ["layout", "packages"] {
        let output = devbox().arg(cmd).output().unwrap();
        assert!(
            !output.status.success(),
            "`devbox {cmd}` should no longer exist"
        );
    }
}

#[test]
fn web_help_documents_the_console() {
    let output = devbox().args(["web", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--port"));
    assert!(stdout.contains("--no-open"));
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

/// Every command that selects an existing box, as its `--help` invocation.
///
/// `create` is absent: its `--name` names a box that does not exist yet.
/// `use` is absent: its box is required, so it was never optional. `policy
/// allow` is absent for the reason its own test below states.
const BOX_COMMANDS: &[&[&str]] = &[
    &["shell"],
    &["exec"],
    &["stop"],
    &["destroy"],
    &["status"],
    &["upgrade"],
    &["commit"],
    &["diff"],
    &["discard"],
    &["reprovision"],
    &["code"],
    &["watch"],
    &["snapshot", "save"],
    &["snapshot", "restore"],
    &["snapshot", "list"],
    &["nix", "add"],
    &["nix", "remove"],
    &["layer", "status"],
    &["layer", "diff"],
    &["layer", "commit"],
    &["layer", "discard"],
    &["layer", "refresh"],
    &["layer", "conflicts"],
    &["layer", "stash"],
    &["layer", "stash-pop"],
    &["sets", "list"],
    &["sets", "apply"],
    &["behavior", "summary"],
    &["behavior", "diff"],
    &["behavior", "pcap"],
    &["policy", "show"],
    &["policy", "rules"],
    &["policy", "set"],
    &["policy", "test"],
];

fn help_for(path: &[&str]) -> String {
    let output = devbox()
        .args(path)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("run devbox {} --help: {e}", path.join(" ")));
    assert!(
        output.status.success(),
        "devbox {} --help failed: {}",
        path.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The shipped help is what a user actually reads, so the parse-level contract
/// in `cli::box_arg` is checked again here through the real binary: one
/// spelling, one wording, and no advert for the deprecated flag.
#[test]
fn every_box_command_documents_an_optional_positional_name() {
    for path in BOX_COMMANDS {
        let help = help_for(path);
        let usage = help
            .lines()
            .find(|l| l.starts_with("Usage:"))
            .unwrap_or_else(|| panic!("devbox {} --help has no usage line", path.join(" ")));
        assert!(
            usage.contains("[NAME]"),
            "devbox {} does not take an optional [NAME]: {usage}",
            path.join(" ")
        );
        assert!(
            help.contains("Box name; defaults to the box registered for the current directory"),
            "devbox {} describes its box name differently:\n{help}",
            path.join(" ")
        );
        assert!(
            !help.contains("--name"),
            "devbox {} still advertises the deprecated --name:\n{help}",
            path.join(" ")
        );
        assert!(
            !help.contains("--sandbox"),
            "devbox {} still advertises the deprecated --sandbox:\n{help}",
            path.join(" ")
        );
    }
}

/// The alias is hidden, not gone: a v3 script must keep working for now, and a
/// line that names the box twice must fail rather than pick one.
#[test]
fn the_legacy_name_flag_is_accepted_but_never_alongside_the_positional() {
    let home = tempfile::tempdir().expect("temporary home");
    let accepted = devbox()
        .args(["status", "--name", "no-such-box"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&accepted.stderr);
    assert!(
        stderr.contains("Sandbox 'no-such-box' not found."),
        "--name no longer reaches box resolution: {stderr}"
    );

    let conflict = devbox()
        .args(["status", "one", "--name", "two"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "naming the box twice was not rejected: {stderr}"
    );
}

/// `devbox policy allow` is the one command where the box stays a flag:
/// `<ENTRIES>...` is a required variadic positional, so there is no free slot
/// for an optional `[NAME]` on either side of it. The flag therefore stays
/// visible — hiding the only way to name a box would be worse than the
/// inconsistency.
#[test]
fn policy_allow_documents_its_name_flag_because_it_has_no_positional() {
    let help = help_for(&["policy", "allow"]);
    assert!(help.contains("<ENTRIES>..."));
    assert!(
        help.contains("--name <NAME>"),
        "policy allow hid the only way to name a box:\n{help}"
    );
    assert!(help.contains("Box name; defaults to the box registered for the current directory"));
}

/// `devbox exec` puts the box before `--` and the command after it, so the bare
/// form has to keep meaning "this directory's box".
#[test]
fn exec_usage_shows_the_box_before_the_command_separator() {
    let help = help_for(&["exec"]);
    assert!(
        help.contains("Usage: devbox exec [NAME] -- <COMMAND>..."),
        "exec's usage no longer separates the box from the command:\n{help}"
    );
}
