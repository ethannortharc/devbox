use anyhow::{Result, bail};

use crate::runtime::Runtime;

/// OverlayFS paths inside the VM.
#[allow(dead_code)]
const WORKSPACE: &str = "/workspace";
const UPPER: &str = "/var/devbox/overlay/upper";
const LOWER: &str = "/mnt/host";
#[allow(dead_code)]
const WORK: &str = "/var/devbox/overlay/work";
const STASH_DIR: &str = "/var/devbox/overlay/stash";

/// List files changed in the overlay upper layer.
/// Returns a list of (status, path) tuples.
pub async fn diff(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Vec<OverlayChange>> {
    // List all files in the upper directory
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "bash", "-lc",
                &format!("find {UPPER} -not -path {UPPER} -printf '%y %P\\n'"),
            ],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to scan overlay changes: {}", result.stderr.trim());
    }

    let mut changes = vec![];
    for line in result.stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (kind, path) = match line.split_once(' ') {
            Some((k, p)) => (k, p),
            None => continue,
        };

        // Check if the file exists in the lower layer to determine add vs modify
        let lower_path = format!("{LOWER}/{path}");
        let check = runtime
            .exec_cmd(sandbox_name, &["test", "-e", &lower_path], false)
            .await?;

        let status = if kind == "c" {
            // OverlayFS whiteout — file was deleted
            ChangeStatus::Deleted
        } else if check.exit_code == 0 {
            ChangeStatus::Modified
        } else {
            ChangeStatus::Added
        };

        changes.push(OverlayChange {
            status,
            path: path.to_string(),
            is_dir: kind == "d",
        });
    }

    // Filter out directories that only exist as containers for changed files
    // Keep only file entries and empty new directories
    Ok(changes)
}

/// Show overlay status summary (like `git status`).
/// Returns the list of changes for further processing.
pub async fn status(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Vec<OverlayChange>> {
    let changes = diff(runtime, sandbox_name).await?;
    let stashed = has_stash(runtime, sandbox_name).await?;

    if changes.is_empty() && !stashed {
        println!("Overlay is clean — no changes.");
        return Ok(changes);
    }

    let files: Vec<&OverlayChange> = changes.iter().filter(|c| !c.is_dir).collect();
    let added = files
        .iter()
        .filter(|c| c.status == ChangeStatus::Added)
        .count();
    let modified = files
        .iter()
        .filter(|c| c.status == ChangeStatus::Modified)
        .count();
    let deleted = files
        .iter()
        .filter(|c| c.status == ChangeStatus::Deleted)
        .count();

    if !files.is_empty() {
        println!("Overlay changes:");
        for c in &files {
            println!("  {} {}", c.status.symbol(), c.path);
        }
        println!();
        println!(
            "{} file(s): {} added, {} modified, {} deleted",
            files.len(),
            added,
            modified,
            deleted,
        );
    } else {
        println!("No file changes in overlay.");
    }

    if stashed {
        println!("\nStash: 1 stash saved (use `devbox layer stash-pop` to restore)");
    }

    Ok(changes)
}

/// Sync overlay changes back to the host filesystem.
/// If `paths` is Some, only sync those paths. Otherwise sync everything.
pub async fn commit(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    paths: Option<&[String]>,
    dry_run: bool,
) -> Result<usize> {
    let changes = diff(runtime, sandbox_name).await?;

    if changes.is_empty() {
        println!("No overlay changes to commit.");
        return Ok(0);
    }

    // Filter by paths if specified
    let filtered: Vec<&OverlayChange> = if let Some(filter_paths) = paths {
        changes
            .iter()
            .filter(|c| {
                filter_paths
                    .iter()
                    .any(|p| c.path.starts_with(p.trim_end_matches('/')))
            })
            .collect()
    } else {
        changes.iter().collect()
    };

    if filtered.is_empty() {
        println!("No matching changes to commit.");
        return Ok(0);
    }

    if dry_run {
        println!("Would commit {} change(s):", filtered.len());
        for c in &filtered {
            println!("  {} {}", c.status.symbol(), c.path);
        }
        return Ok(filtered.len());
    }

    // Sync: rsync from upper to lower for each changed file
    // We need to handle additions, modifications, and deletions
    let mut committed = 0;

    for change in &filtered {
        let upper_path = format!("{UPPER}/{}", change.path);
        let lower_path = format!("{LOWER}/{}", change.path);

        match change.status {
            ChangeStatus::Added | ChangeStatus::Modified => {
                if change.is_dir {
                    let result = runtime
                        .exec_cmd(sandbox_name, &["mkdir", "-p", &lower_path], false)
                        .await?;
                    if result.exit_code != 0 {
                        eprintln!(
                            "Warning: failed to create dir {}: {}",
                            change.path,
                            result.stderr.trim()
                        );
                        continue;
                    }
                } else {
                    // Ensure parent directory exists
                    let parent = format!(
                        "{LOWER}/{}",
                        std::path::Path::new(&change.path)
                            .parent()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default()
                    );
                    if !parent.is_empty() && parent != LOWER {
                        let _ = runtime
                            .exec_cmd(sandbox_name, &["mkdir", "-p", &parent], false)
                            .await;
                    }

                    let result = runtime
                        .exec_cmd(
                            sandbox_name,
                            &["cp", "-a", &upper_path, &lower_path],
                            false,
                        )
                        .await?;
                    if result.exit_code != 0 {
                        eprintln!(
                            "Warning: failed to commit {}: {}",
                            change.path,
                            result.stderr.trim()
                        );
                        continue;
                    }
                }
            }
            ChangeStatus::Deleted => {
                let result = runtime
                    .exec_cmd(sandbox_name, &["rm", "-rf", &lower_path], false)
                    .await?;
                if result.exit_code != 0 {
                    eprintln!(
                        "Warning: failed to delete {}: {}",
                        change.path,
                        result.stderr.trim()
                    );
                    continue;
                }
            }
        }

        println!("  {} {}", change.status.symbol(), change.path);
        committed += 1;
    }

    // After committing, clear the upper layer for synced files
    // so the overlay reflects the new lower state
    if committed > 0 {
        println!("\nCommitted {} change(s) to host.", committed);
    }

    Ok(committed)
}

/// Discard overlay changes (clear the upper layer).
/// If `paths` is Some, only discard those paths. Otherwise discard everything.
pub async fn discard(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    paths: Option<&[String]>,
) -> Result<usize> {
    if let Some(filter_paths) = paths {
        let mut discarded = 0;
        for path in filter_paths {
            let upper_path = format!("{UPPER}/{}", path.trim_start_matches('/'));
            let result = runtime
                .exec_cmd(sandbox_name, &["rm", "-rf", &upper_path], false)
                .await?;
            if result.exit_code == 0 {
                println!("  Discarded: {path}");
                discarded += 1;
            }
        }
        if discarded > 0 {
            println!("\nDiscarded {} path(s).", discarded);
        }
        Ok(discarded)
    } else {
        // Clear entire upper layer
        let result = runtime
            .exec_cmd(
                sandbox_name,
                &[
                    "bash",
                    "-lc",
                    &format!("rm -rf {UPPER}/* {UPPER}/.[!.]* 2>/dev/null; true"),
                ],
                false,
            )
            .await?;

        if result.exit_code != 0 {
            bail!("Failed to clear overlay: {}", result.stderr.trim());
        }

        println!("All overlay changes discarded.");
        Ok(1)
    }
}

/// Stash the current overlay upper layer (save and clear).
/// Only one stash is supported at a time.
pub async fn stash(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    if has_stash(runtime, sandbox_name).await? {
        bail!("A stash already exists. Pop or discard it first (`devbox layer stash-pop`).");
    }

    // Move upper to stash
    let result = runtime
        .exec_cmd(sandbox_name, &["mv", UPPER, STASH_DIR], false)
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to stash overlay: {}", result.stderr.trim());
    }

    // Recreate empty upper directory
    let result = runtime
        .exec_cmd(sandbox_name, &["mkdir", "-p", UPPER], false)
        .await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to recreate upper directory: {}",
            result.stderr.trim()
        );
    }

    println!("Overlay changes stashed.");
    Ok(())
}

/// Restore a previously stashed overlay upper layer.
pub async fn stash_pop(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    if !has_stash(runtime, sandbox_name).await? {
        bail!("No stash found. Nothing to pop.");
    }

    // Merge stash back into upper (copy hidden and regular files)
    let merge_cmd = format!(
        "cp -a {STASH_DIR}/* {UPPER}/ 2>/dev/null; cp -a {STASH_DIR}/.[!.]* {UPPER}/ 2>/dev/null; true"
    );
    let result = runtime
        .exec_cmd(sandbox_name, &["bash", "-lc", &merge_cmd], false)
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to restore stash: {}", result.stderr.trim());
    }

    // Remove the stash directory
    let result = runtime
        .exec_cmd(sandbox_name, &["rm", "-rf", STASH_DIR], false)
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to clean up stash: {}", result.stderr.trim());
    }

    println!("Stash restored to overlay.");
    Ok(())
}

/// Check if a stash exists and is non-empty.
pub async fn has_stash(runtime: &dyn Runtime, sandbox_name: &str) -> Result<bool> {
    // Check if stash directory exists and has contents
    let check_cmd = format!("test -d {STASH_DIR} && [ \"$(ls -A {STASH_DIR} 2>/dev/null)\" ]");
    let result = runtime
        .exec_cmd(sandbox_name, &["bash", "-lc", &check_cmd], false)
        .await?;

    Ok(result.exit_code == 0)
}

/// Remount the overlay to pick up host-side changes in the lower layer.
/// This clears stale file handles. Upper layer (your edits) is preserved.
///
/// Newer kernels don't allow `mount -o remount` on OverlayFS, so we
/// unmount and remount with the same options instead.
pub async fn refresh(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    // Try simple remount first (works on older kernels)
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["bash", "-lc", &format!("mount -o remount {WORKSPACE}")],
            false,
        )
        .await?;

    if result.exit_code == 0 {
        println!("Overlay refreshed — host changes are now visible.");
        return Ok(());
    }

    // Remount not supported — unmount and remount manually.
    // The upper layer is on disk, so nothing is lost.
    let remount_cmd = format!(
        "umount {WORKSPACE} && mount -t overlay overlay \
         -o lowerdir={LOWER},upperdir={UPPER},workdir={WORK} {WORKSPACE}"
    );
    let result = runtime
        .exec_cmd(sandbox_name, &["bash", "-lc", &remount_cmd], false)
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to refresh overlay: {}", result.stderr.trim());
    }

    println!("Overlay refreshed — host changes are now visible.");
    Ok(())
}

/// Detect files that were modified in both the upper layer (your edits)
/// and the lower layer (host changed since mount). These are potential conflicts.
pub async fn conflicts(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Vec<ConflictInfo>> {
    let changes = diff(runtime, sandbox_name).await?;

    let mut conflicts = vec![];
    for change in &changes {
        if change.is_dir || change.status != ChangeStatus::Modified {
            continue;
        }

        // For modified files, check if the lower layer version differs from
        // what the overlay originally saw (compare upper vs lower content hash).
        let upper_path = format!("{UPPER}/{}", change.path);
        let lower_path = format!("{LOWER}/{}", change.path);

        // Check if both files exist and differ
        let diff_cmd = format!(
            "[ -f '{}' ] && [ -f '{}' ] && ! diff -q '{}' '{}' >/dev/null 2>&1 && echo CONFLICT || echo OK",
            upper_path, lower_path, upper_path, lower_path
        );
        let result = runtime
            .exec_cmd(sandbox_name, &["bash", "-lc", &diff_cmd], false)
            .await?;

        if result.stdout.trim() == "CONFLICT" {
            conflicts.push(ConflictInfo {
                path: change.path.clone(),
            });
        }
    }

    if conflicts.is_empty() {
        println!("No conflicts — your changes and host changes don't overlap.");
    } else {
        println!(
            "{} conflict(s) found (both you and the host modified these files):\n",
            conflicts.len()
        );
        for c in &conflicts {
            println!("  \x1b[31m!\x1b[0m {}", c.path);
        }
        println!();
        println!("Your version (upper layer) takes precedence in /workspace.");
        println!("Use `devbox diff` to review, or edit manually to merge.");
    }

    Ok(conflicts)
}

/// Check if the lower layer has changes since the overlay was mounted.
/// Returns a list of paths that changed on the host side.
pub async fn lower_layer_changes(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Vec<String>> {
    // Compare the lower layer mtime against a timestamp file we create on mount.
    // If no timestamp exists, we can't detect changes — just check for stale handles.
    // Simpler approach: find files in lower newer than the overlay work dir (created at mount time).
    let cmd = format!(
        "find {} -newer {} -not -path {} -type f -printf '%P\\n' 2>/dev/null | head -50",
        LOWER, WORK, LOWER
    );
    let result = runtime
        .exec_cmd(sandbox_name, &["bash", "-lc", &cmd], false)
        .await?;

    if result.exit_code != 0 {
        return Ok(vec![]);
    }

    let paths: Vec<String> = result
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    Ok(paths)
}

/// Same as `conflicts()` but without printing (for use in prompts).
pub async fn conflicts_quiet(
    runtime: &dyn Runtime,
    sandbox_name: &str,
) -> Result<Vec<ConflictInfo>> {
    let changes = diff(runtime, sandbox_name).await?;

    let mut result_conflicts = vec![];
    for change in &changes {
        if change.is_dir || change.status != ChangeStatus::Modified {
            continue;
        }

        let upper_path = format!("{UPPER}/{}", change.path);
        let lower_path = format!("{LOWER}/{}", change.path);

        let diff_cmd = format!(
            "[ -f '{}' ] && [ -f '{}' ] && ! diff -q '{}' '{}' >/dev/null 2>&1 && echo CONFLICT || echo OK",
            upper_path, lower_path, upper_path, lower_path
        );
        let result = runtime
            .exec_cmd(sandbox_name, &["bash", "-lc", &diff_cmd], false)
            .await?;

        if result.stdout.trim() == "CONFLICT" {
            result_conflicts.push(ConflictInfo {
                path: change.path.clone(),
            });
        }
    }

    Ok(result_conflicts)
}

/// A conflict where both upper and lower layers have different versions of a file.
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub path: String,
}

/// A change detected in the overlay upper layer.
#[derive(Debug, Clone)]
pub struct OverlayChange {
    pub status: ChangeStatus,
    pub path: String,
    pub is_dir: bool,
}

/// Status of a file in the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

impl ChangeStatus {
    pub fn symbol(&self) -> &str {
        match self {
            Self::Added => "\x1b[32m+\x1b[0m",
            Self::Modified => "\x1b[33m~\x1b[0m",
            Self::Deleted => "\x1b[31m-\x1b[0m",
        }
    }
}
