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
        // `apply_saved` is the strict access-path restore; `enforce::apply` is
        // the direct form used where the policy is already in hand, and
        // `restore_after_rebuild` reuses the claim held by a rebuild. Each
        // counts here because this guard is about whether the posture returns.
        let restores = body.contains("apply_saved")
            || body.contains("restore_after_rebuild")
            || body.contains("enforce::apply(");
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

/// Only `apply` decides whether an `open` posture installs a table.
///
/// The same shape as the guard above, for the same reason. `open` used to mean
/// "no table" without exception, so three places tested `Posture::Open` and
/// cleared. Then `open` with an allowlist and alerts started meaning
/// observe-and-warn, `apply` learned that, and the other two did not — so
/// start, attach, access and every rebuild silently switched observing off
/// until someone set the policy again.
///
/// A rule restated in a second place is a rule that goes stale. This insists
/// there is one statement of it.
#[test]
fn only_apply_decides_what_an_open_posture_installs() {
    // Across the whole tree, not one file.
    //
    // The first version of this guard read `enforce.rs` alone, because that is
    // where the two stale copies were. The very next round found two more in
    // `cli/policy.rs` and `cli/reprovision.rs` — a guard scoped more narrowly
    // than the class it guards catches the instances you already know about,
    // which is the same as catching nothing.
    let mut offenders = Vec::new();

    for entry in walk("src") {
        let rel = entry.strip_prefix("./").unwrap_or(&entry).to_string();
        if !rel.ends_with(".rs") {
            continue;
        }
        // `policy/mod.rs` defines the enum and its own behaviour, and
        // `nftables.rs` is where the ruleset for each posture is written —
        // both must name it. Everything else should be asking `apply`.
        if rel == "src/policy/mod.rs" || rel == "src/policy/nftables.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&entry).expect("readable source");

        let mut in_tests = false;
        for (n, line) in source.lines().enumerate() {
            if line.trim_start().starts_with("mod tests") {
                in_tests = true;
            }
            if in_tests {
                continue;
            }
            if !line.contains("Posture::Open") {
                continue;
            }
            // Testing it to *build* a policy is fine; testing it to decide
            // whether to install one is what goes stale.
            if line.contains("audits(") || line.contains("egress: Posture::Open") {
                continue;
            }
            offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "these decide for themselves what `open` means instead of calling \
         `apply`, which is how observe-and-warn came to be disabled by every \
         routine lifecycle operation:\n{}",
        offenders.join("\n")
    );
}

/// Every path that rewrites a box's generated configuration claims it first.
///
/// The same shape as the guards above, for the same reason. The lock landed on
/// the two Sets paths — the ones the finding named — while `upgrade`,
/// `reprovision`, and overlay `use` went on writing the same files unclaimed.
/// A guard scoped to the instances already found is the mistake this file
/// exists to stop repeating.
#[test]
fn every_rebuild_entry_point_claims_the_box() {
    /// Calls that rewrite a box's generated Nix configuration.
    const REBUILDS: &[&str] = &["upgrade_sets(", "provision_vm_full(", "write_set_modules("];

    let mut offenders = Vec::new();

    for entry in walk("src") {
        let rel = entry.strip_prefix("./").unwrap_or(&entry).to_string();
        // The definitions themselves, and the module that owns the lock.
        if rel == "src/nix/mod.rs" || rel == "src/sandbox/provision.rs" || rel == "src/web/build.rs"
        {
            continue;
        }
        // Creating a box is not rebuilding one: nothing else can be touching a
        // box that does not exist yet, and the name is not resolvable until it
        // does.
        if rel == "src/sandbox/mod.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&entry).expect("readable source");
        if !REBUILDS.iter().any(|call| source.contains(call)) {
            continue;
        }
        // `claim_box`, renamed from `lock_rebuild` when the two locks were split
        // into distinct types — the box claim refuses, the project claim waits,
        // and sharing one guard type was how they came to be reasoned about as
        // one thing.
        if !source.contains("claim_box(") {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "these rewrite a box's generated configuration without claiming it, so \
         a console rebuild running beside one leaves the active generation and \
         the recorded selection describing different things:\n{}",
        offenders.join("\n")
    );
}

/// No function may claim a box *after* reading the state that claim protects.
///
/// The claim is what makes a snapshot still true. Taken afterwards it is a
/// correct-looking claim over a stale read: a `devbox use` completing in the gap
/// releases its own claim, so this one succeeds — and the command then proceeds
/// against the project the box used to belong to. It writes the selection into
/// the old project's `devbox.toml`, or applies the old project's policy to a box
/// now serving the new one, which is how an `open` posture lands on a box
/// recorded as `isolated`.
///
/// Round 45 found this in one path and it was fixed there. Round 46 found it in
/// four more, because the fix was applied to the instance and the invariant was
/// only ever written down in a commit message. `SandboxManager::claim_and_read`
/// pairs the two; this makes the pairing the only shape that passes.
#[test]
fn no_path_claims_a_box_after_reading_its_state() {
    let mut offenders = Vec::new();

    for entry in walk("src") {
        let rel = entry.strip_prefix("./").unwrap_or(&entry).to_string();
        // The pairing helper is where the two legitimately appear in order.
        if rel == "src/sandbox/mod.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&entry).expect("readable source");
        let lines: Vec<&str> = source.lines().collect();

        let mut fn_start = 0usize;
        let mut read_at: Option<usize> = None;
        for (n, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("fn ")
                || t.starts_with("pub fn ")
                || t.starts_with("async fn ")
                || t.starts_with("pub async fn ")
            {
                fn_start = n;
                read_at = None;
            }
            if t.starts_with("//") {
                continue;
            }
            // `claim_and_read` is the fix, not an instance of the problem.
            if t.contains("claim_and_read(") {
                read_at = None;
                continue;
            }
            if t.contains("get_sandbox(") && read_at.is_none() {
                read_at = Some(n);
            }
            if t.contains("claim_box(")
                && let Some(r) = read_at
            {
                // A re-read under the claim is the other legitimate shape, so
                // only complain when nothing is read again afterwards.
                let after = lines[n..(n + 12).min(lines.len())].join("\n");
                if !after.contains("get_sandbox(") {
                    offenders.push(format!(
                        "{rel}: reads box state at line {}, claims it at line {} \
                         (fn starting line {})",
                        r + 1,
                        n + 1,
                        fn_start + 1
                    ));
                }
                read_at = None;
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these take the box claim after reading the state it protects, so the \
         claim covers a snapshot that may already name the wrong project. Use \
         `SandboxManager::claim_and_read`, or re-read under the claim:\n{}",
        offenders.join("\n")
    );
}
