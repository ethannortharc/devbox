//! Every path that disturbs a box's network stack must restore its posture.
//!
//! This is the single most repeated defect in this codebase's review history.
//! Nine separate call sites have been found and fixed one at a time — start,
//! attach, exec, `code`, the lab substrate, `reprovision`, both Sets paths, and
//! the bare-`devbox` console — each discovered only after the previous fix
//! shipped. Two more (`use` and `upgrade`) were found by enumerating the call
//! sites instead of waiting for the next round.
//!
//! The property is simple and the failure is severe: a rebuild or a restart
//! removes devbox's nftables table, so a box comes back with open egress while
//! `devbox.toml`, the CLI, and the console all keep reporting `isolated`.
//!
//! So this is a source-level guard rather than a behavioural test. It cannot
//! run a NixOS rebuild, but it can insist that no new call site appears without
//! the accompanying restore — which is exactly the mistake that keeps recurring.

use std::path::Path;

/// Calls that restart a box or rebuild its system configuration.
const DISTURBING: &[&str] = &[
    "runtime.start(",
    "nixos_rebuild(",
    "provision_vm_full(",
    "upgrade_sets(",
];

/// Files exempt, with the reason.
fn exempt(path: &str) -> bool {
    // The runtime implementations *are* `start`; the rebuild helpers are what
    // the callers call. Neither knows about policy, and neither should.
    path.starts_with("src/runtime/")
        || path == "src/nix/rebuild.rs"
        || path == "src/nix/mod.rs"
        // Defines `provision_vm_full` rather than calling it; the callers are
        // what must restore, and they are checked.
        || path == "src/sandbox/provision.rs"
        // The web build path restores inside `apply_selection`, several
        // hundred lines from the `rebuild_argv` it streams.
        || path == "src/web/build.rs"
}

#[test]
fn every_network_disturbing_call_site_restores_the_posture() {
    let mut offenders = Vec::new();

    for entry in walk("src") {
        let rel = entry.strip_prefix("./").unwrap_or(&entry).to_string();
        if exempt(&rel) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let disturbs = DISTURBING.iter().any(|needle| body.contains(needle));
        if !disturbs {
            continue;
        }
        // `apply_saved` is the restore; `enforce::apply` is the direct form
        // used where the policy is already in hand.
        let restores = body.contains("apply_saved") || body.contains("enforce::apply(");
        if !restores {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "these files restart or rebuild a box without restoring its egress \
         posture, so it comes back unrestricted while every surface reports \
         otherwise: {offenders:?}\n\n  \
         Add `policy::enforce::apply_saved(...)` after the call, or add the \
         file to `exempt()` with the reason it does not need one."
    );
}

fn walk(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![Path::new(dir).to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path.display().to_string());
            }
        }
    }
    out
}
