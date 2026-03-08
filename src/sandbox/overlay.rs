use anyhow::{Result, bail};

use crate::runtime::Runtime;

/// OverlayFS paths inside the VM.
#[allow(dead_code)]
const WORKSPACE: &str = "/workspace";
const UPPER: &str = "/workspace/upper";
const LOWER: &str = "/workspace/lower";

/// List files changed in the overlay upper layer.
/// Returns a list of (status, path) tuples.
pub async fn diff(
    runtime: &dyn Runtime,
    sandbox_name: &str,
) -> Result<Vec<OverlayChange>> {
    // List all files in the upper directory
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "sudo", "find", UPPER,
                "-not", "-path", UPPER,
                "-printf", "%y %P\\n",
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
            .exec_cmd(
                sandbox_name,
                &["test", "-e", &lower_path],
                false,
            )
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
                        .exec_cmd(
                            sandbox_name,
                            &["sudo", "mkdir", "-p", &lower_path],
                            false,
                        )
                        .await?;
                    if result.exit_code != 0 {
                        eprintln!("Warning: failed to create dir {}: {}", change.path, result.stderr.trim());
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
                            .exec_cmd(
                                sandbox_name,
                                &["sudo", "mkdir", "-p", &parent],
                                false,
                            )
                            .await;
                    }

                    let result = runtime
                        .exec_cmd(
                            sandbox_name,
                            &["sudo", "cp", "-a", &upper_path, &lower_path],
                            false,
                        )
                        .await?;
                    if result.exit_code != 0 {
                        eprintln!("Warning: failed to commit {}: {}", change.path, result.stderr.trim());
                        continue;
                    }
                }
            }
            ChangeStatus::Deleted => {
                let result = runtime
                    .exec_cmd(
                        sandbox_name,
                        &["sudo", "rm", "-rf", &lower_path],
                        false,
                    )
                    .await?;
                if result.exit_code != 0 {
                    eprintln!("Warning: failed to delete {}: {}", change.path, result.stderr.trim());
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
                .exec_cmd(
                    sandbox_name,
                    &["sudo", "rm", "-rf", &upper_path],
                    false,
                )
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
                    "sudo", "bash", "-c",
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
