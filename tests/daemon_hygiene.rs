//! No test may leave a background daemon behind.
//!
//! A source-level guard, like `policy_lifecycle.rs`, and for the same reason:
//! the defect is invisible from inside the suite that causes it. Running the
//! real binary with `HOME` pointed at a `tempfile::tempdir` is the normal way
//! to test the CLI, and any command that `needs_collector` starts a collector
//! and a broker *before* it runs — before it has even decided it is going to
//! fail. Those daemons then supervise a directory that is deleted moments
//! later. Nothing can ever find them again: they hold no lock anyone contends
//! for, and their ownership sidecar went with the directory.
//!
//! So every one of them lives until the machine is rebooted, and the tests
//! that made them pass. Measured on the development host before this guard:
//! 147 `devbox __collector` processes, 1.9 GB resident, 54 still alive across
//! 54 distinct temporary state directories — three per `cargo test` run of
//! `tests/mcp.rs`, accumulated over every run in every worktree.

use std::path::Path;

/// Files that run the built binary and are allowed not to disable the daemons.
fn exempt(path: &str) -> bool {
    // It starts a collector on purpose — replacing an outdated one is what it
    // is testing — and it tracks every pid it creates through
    // `CollectorCleanup`, which checks what a pid *is* before signalling it.
    // Measured: this file leaks none.
    path == "tests/integration.rs"
        // The guard itself names the binary only in this comment and in the
        // strings it scans for.
        || path == "tests/daemon_hygiene.rs"
}

#[test]
fn a_test_that_runs_devbox_with_a_temporary_home_starts_no_daemons() {
    let mut offenders = Vec::new();

    for entry in tests_files() {
        let relative = entry.trim_start_matches("./").to_string();
        if exempt(&relative) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&entry) else {
            continue;
        };
        if !body.contains("CARGO_BIN_EXE_devbox") {
            continue;
        }
        // Both, because a lifecycle command starts both. Disabling only the
        // collector leaves the broker, which is the same leak with a different
        // name — and the broker only stays quiet today because a temporary
        // `HOME` has no providers configured, which is luck, not design.
        let disables =
            body.contains("DEVBOX_NO_COLLECTOR_DAEMON") && body.contains("DEVBOX_NO_BROKER");
        if !disables {
            offenders.push(relative);
        }
    }

    assert!(
        offenders.is_empty(),
        "these tests run the devbox binary without disabling its background \
         daemons, so every command they run that needs a collector leaves one \
         supervising a deleted temporary directory forever: {offenders:?}\n\n  \
         Set DEVBOX_NO_COLLECTOR_DAEMON=1 and DEVBOX_NO_BROKER=1 on the \
         command, or add the file to `exempt()` with the reason it is safe."
    );
}

fn tests_files() -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(Path::new("tests")) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(format!("tests/{}", entry.file_name().to_string_lossy()));
        }
    }
    found
}

/// The guard only works if the environment variables it names are the ones the
/// binary reads. A rename on one side and not the other would leave it passing
/// while every command started a daemon again.
#[test]
fn the_variables_the_guard_names_are_the_ones_devbox_honours() {
    let sources = [
        std::fs::read_to_string("src/obs/daemon.rs").expect("collector daemon source"),
        std::fs::read_to_string("src/broker/mod.rs").expect("broker source"),
    ];
    for variable in ["DEVBOX_NO_COLLECTOR_DAEMON", "DEVBOX_NO_BROKER"] {
        assert!(
            sources.iter().any(|source| source.contains(variable)),
            "{variable} is not read by devbox any more; this guard is checking \
             for something that no longer turns anything off"
        );
    }
}
