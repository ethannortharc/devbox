use anyhow::{Result, bail};

use crate::runtime::Runtime;

/// OverlayFS paths inside the VM.
#[allow(dead_code)]
const WORKSPACE: &str = "/workspace";
pub(crate) const UPPER: &str = "/var/devbox/overlay/upper";
const LOWER: &str = "/mnt/host";
#[allow(dead_code)]
const WORK: &str = "/var/devbox/overlay/work";
const STASH_DIR: &str = "/var/devbox/overlay/stash";

/// What `find`'s `%y` reports for one entry.
///
/// Kept as a small enum rather than the raw letter so that the two callers
/// that care — this module's add/modify/delete classification, and
/// [`crate::sandbox::checkpoint`]'s tree-against-tree diff — cannot disagree
/// about what "is a directory" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// A character device. In an overlay upper this is almost always a
    /// whiteout, but see [`TreeEntry::is_whiteout`] for the real test.
    Char,
    /// Block device, fifo, socket, or anything else `find` reports.
    Other,
}

impl EntryKind {
    /// Map `find -printf '%y'` onto the enum.
    pub fn from_find(letter: &str) -> Self {
        match letter {
            "f" => Self::File,
            "d" => Self::Dir,
            "l" => Self::Symlink,
            "c" => Self::Char,
            _ => Self::Other,
        }
    }
}

/// One entry of a guest-side directory tree, as [`enumerate_tree`] sees it.
///
/// This is deliberately more than `overlay::diff` needs today: a checkpoint is
/// diffed against another *tree*, not against the lower layer, so it has no
/// `test -e` to fall back on and has to decide "changed?" from the metadata
/// alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Path relative to the tree root, no leading slash.
    pub path: String,
    pub kind: EntryKind,
    /// `find`'s `%s`: bytes for a regular file, target length for a symlink,
    /// and the directory's own on-disk size for a directory — which is why the
    /// tree diff ignores it there.
    pub size: u64,
    /// `find`'s `%T@` verbatim (`seconds.nanoseconds`). Only ever compared for
    /// equality, so it is never parsed into a float and never loses precision.
    /// `cp -a` preserves it exactly, which is what makes a copied checkpoint
    /// compare equal to the upper it came from.
    pub mtime: String,
    /// A character device with rdev 0/0 — OverlayFS's "this name is deleted".
    /// A genuine `/dev`-style character node in the upper is *not* a whiteout,
    /// which is the case `kind == Char` alone gets wrong.
    pub is_whiteout: bool,
    /// A directory carrying `trusted.overlay.opaque=y` — OverlayFS's "ignore
    /// whatever the lower layer has under this name".
    pub is_opaque: bool,
}

/// The guest shell that lists one tree.
///
/// Three passes rather than one because GNU `find -printf` can report neither
/// a device node's major/minor nor an xattr:
///
/// - `E` — every entry, with type, size, mtime and path;
/// - `W` — the major/minor of each character device, so whiteouts (0/0) can be
///   told apart from real device nodes;
/// - `O` — the directories that carry `trusted.overlay.opaque`.
///
/// The path is the last field of every record, so a path containing spaces or
/// tabs still parses. A path containing a newline does not — the same
/// limitation `overlay::diff` has always had.
fn tree_listing_command(root: &str) -> String {
    let root = root.trim_end_matches('/');
    format!(
        "set -e; \
         find '{root}' -mindepth 1 -printf 'E\\t%y\\t%s\\t%T@\\t%P\\n'; \
         find '{root}' -mindepth 1 -type c -exec stat -c 'W %t %T %n' {{}} +; \
         find '{root}' -mindepth 1 -type d -exec sh -c \
         'for d in \"$@\"; do v=$(getfattr -n trusted.overlay.opaque --only-values \"$d\" 2>/dev/null || true); \
         if [ \"$v\" = y ]; then printf \"O %s\\n\" \"$d\"; fi; done' _ {{}} +"
    )
}

/// Parse [`tree_listing_command`]'s output into entries.
///
/// Split out from the guest call so the format — which is the part that breaks
/// — can be tested without a VM.
pub fn parse_tree_listing(stdout: &str, root: &str) -> Vec<TreeEntry> {
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let mut entries: Vec<TreeEntry> = vec![];
    let mut whiteouts: Vec<String> = vec![];
    let mut opaque: Vec<String> = vec![];

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "E" => {
                // "\t<kind>\t<size>\t<mtime>\t<path>"
                let mut fields = rest.trim_start_matches('\t').splitn(4, '\t');
                let (Some(kind), Some(size), Some(mtime), Some(path)) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                entries.push(TreeEntry {
                    path: path.to_string(),
                    kind: EntryKind::from_find(kind),
                    size: size.parse().unwrap_or(0),
                    mtime: mtime.to_string(),
                    is_whiteout: false,
                    is_opaque: false,
                });
            }
            "W" => {
                // " <major> <minor> <absolute path>"
                let rest = rest.trim_start_matches(' ');
                let mut fields = rest.splitn(3, ' ');
                let (Some(major), Some(minor), Some(path)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                // `stat -c '%t %T'` prints hex with no padding; 0/0 is the
                // whiteout rdev and every other value is a real device node.
                if major.trim_start_matches('0').is_empty()
                    && minor.trim_start_matches('0').is_empty()
                {
                    whiteouts.push(path.strip_prefix(&prefix).unwrap_or(path).to_string());
                }
            }
            "O" => {
                let path = rest.trim_start_matches(' ');
                opaque.push(path.strip_prefix(&prefix).unwrap_or(path).to_string());
            }
            _ => {}
        }
    }

    for entry in &mut entries {
        entry.is_whiteout = whiteouts.contains(&entry.path);
        entry.is_opaque = opaque.contains(&entry.path);
    }
    entries
}

/// List one guest directory tree, root excluded.
///
/// `overlay::diff` walks the live upper with it; `checkpoint` walks a saved
/// upper with the same call, so the two can never drift apart on what an
/// entry is.
pub async fn enumerate_tree(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    root: &str,
) -> Result<Vec<TreeEntry>> {
    let result = runtime
        .run_as_root(sandbox_name, &tree_listing_command(root), false)
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to scan {root}: {}", result.stderr.trim());
    }

    Ok(parse_tree_listing(&result.stdout, root))
}

/// List files changed in the overlay upper layer.
/// Returns a list of (status, path) tuples.
pub async fn diff(runtime: &dyn Runtime, sandbox_name: &str) -> Result<Vec<OverlayChange>> {
    // List all files in the upper directory (needs root for overlay dirs)
    let entries = enumerate_tree(runtime, sandbox_name, UPPER)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to scan overlay changes: {e:#}"))?;

    let mut changes = vec![];
    for entry in entries {
        // Check if the file exists in the lower layer to determine add vs modify
        let lower_path = format!("{LOWER}/{}", entry.path);
        let check = runtime
            .exec_cmd(sandbox_name, &["test", "-e", &lower_path], false)
            .await?;

        let status = if entry.kind == EntryKind::Char {
            // OverlayFS whiteout — file was deleted
            ChangeStatus::Deleted
        } else if check.exit_code == 0 {
            ChangeStatus::Modified
        } else {
            ChangeStatus::Added
        };

        changes.push(OverlayChange {
            status,
            path: entry.path,
            is_dir: entry.kind == EntryKind::Dir,
        });
    }

    // Filter out directories that only exist as containers for changed files
    // Keep only file entries and empty new directories
    Ok(changes)
}

/// Changes that carry user data rather than merely containing another entry.
///
/// `find` reports every upper-layer parent directory. Existing/ancestor
/// directories are structural noise, but a leaf directory newly added to the
/// overlay is itself meaningful: commit intentionally recreates it even when
/// it is empty. Destroy therefore must protect it just like a changed file.
pub fn meaningful_changes(changes: &[OverlayChange]) -> Vec<&OverlayChange> {
    changes
        .iter()
        .filter(|change| {
            if !change.is_dir {
                return true;
            }
            if change.status != ChangeStatus::Added {
                return false;
            }
            let prefix = format!("{}/", change.path.trim_end_matches('/'));
            !changes
                .iter()
                .any(|other| other.path != change.path && other.path.starts_with(prefix.as_str()))
        })
        .collect()
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

    // Sync: copy from upper to lower for each changed file
    let mut committed = 0;

    for change in &filtered {
        let upper_path = format!("{UPPER}/{}", change.path);
        let lower_path = format!("{LOWER}/{}", change.path);

        match change.status {
            ChangeStatus::Added | ChangeStatus::Modified => {
                if change.is_dir {
                    let cmd = format!("mkdir -p {lower_path}");
                    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;
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
                        let cmd = format!("mkdir -p {parent}");
                        let _ = runtime.run_as_root(sandbox_name, &cmd, false).await;
                    }

                    let cmd = format!("cp -a {upper_path} {lower_path}");
                    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;
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
                let cmd = format!("rm -rf {lower_path}");
                let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;
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
///
/// The overlay is remounted afterwards, for the reason
/// [`crate::sandbox::checkpoint::restore`] remounts: the kernel does not
/// expect the upper to move under a live mount, so `readdir` notices the
/// removal immediately while a cached `read` keeps serving the discarded
/// contents. Without this, `devbox layer discard` followed by `cat` returned
/// the very edit that had just been thrown away.
///
/// A remount needs `/workspace` idle, so a failure is a warning rather than an
/// error: the discard itself has already happened, and `devbox diff` — which
/// reads the upper directly — agrees with it either way.
pub async fn discard(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    paths: Option<&[String]>,
) -> Result<usize> {
    let discarded = if let Some(filter_paths) = paths {
        let mut discarded = 0;
        for path in filter_paths {
            let upper_path = format!("{UPPER}/{}", path.trim_start_matches('/'));
            let cmd = format!("rm -rf {upper_path}");
            let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;
            if result.exit_code == 0 {
                println!("  Discarded: {path}");
                discarded += 1;
            }
        }
        if discarded > 0 {
            println!("\nDiscarded {} path(s).", discarded);
        }
        discarded
    } else {
        // Clear entire upper layer
        let cmd = format!("rm -rf {UPPER}/* {UPPER}/.[!.]* 2>/dev/null; true");
        let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

        if result.exit_code != 0 {
            bail!("Failed to clear overlay: {}", result.stderr.trim());
        }

        println!("All overlay changes discarded.");
        1
    };

    // Nothing was removed, so nothing can be stale.
    if discarded == 0 {
        return Ok(discarded);
    }

    if let Err(error) = refresh(runtime, sandbox_name).await {
        eprintln!(
            "Warning: the changes are discarded but /workspace could not be remounted: {error:#}"
        );
        eprintln!(
            "         Processes with files open under /workspace may still read the old contents."
        );
        eprintln!("         Close them and run `devbox layer refresh {sandbox_name}`.");
    }

    Ok(discarded)
}

/// Stash the current overlay upper layer (save and clear).
/// Only one stash is supported at a time.
pub async fn stash(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    if has_stash(runtime, sandbox_name).await? {
        bail!("A stash already exists. Pop or discard it first (`devbox layer stash-pop`).");
    }

    // Move upper to stash
    let cmd = format!("mv {UPPER} {STASH_DIR}");
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

    if result.exit_code != 0 {
        bail!("Failed to stash overlay: {}", result.stderr.trim());
    }

    // Recreate empty upper directory
    let cmd = format!("mkdir -p {UPPER}");
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

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
    let result = runtime.run_as_root(sandbox_name, &merge_cmd, false).await?;

    if result.exit_code != 0 {
        bail!("Failed to restore stash: {}", result.stderr.trim());
    }

    // Remove the stash directory
    let cmd = format!("rm -rf {STASH_DIR}");
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

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
    let cmd = format!("mount -o remount {WORKSPACE}");
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

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
        .run_as_root(sandbox_name, &remount_cmd, false)
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
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::runtime::{ExecResult, SandboxStatus};

    /// A guest that records every privileged command and can be told to refuse
    /// the remount.
    ///
    /// `discard` is otherwise untestable without a hypervisor, and the thing
    /// that keeps being wrong is the *ordering* — whether the remount happens
    /// at all, and whether it happens after the upper is cleared.
    struct RecordingGuest {
        commands: Mutex<Vec<String>>,
        /// Both the `mount -o remount` and the umount/mount fallback fail, the
        /// way a busy `/workspace` fails.
        remount_fails: bool,
    }

    impl RecordingGuest {
        fn new(remount_fails: bool) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                remount_fails,
            }
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Runtime for RecordingGuest {
        fn name(&self) -> &str {
            "recording"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn priority(&self) -> u32 {
            0
        }
        // `run_as_root` funnels into this, so recording here catches the
        // command the way the guest would receive it.
        async fn exec_cmd(&self, _: &str, cmd: &[&str], _: bool) -> Result<ExecResult> {
            let joined = cmd.join(" ");
            self.commands.lock().unwrap().push(joined.clone());
            let failed = self.remount_fails && joined.contains("mount");
            Ok(ExecResult {
                exit_code: i32::from(failed),
                stdout: String::new(),
                stderr: if failed {
                    "target is busy".into()
                } else {
                    String::new()
                },
            })
        }
        async fn create(
            &self,
            _: &crate::runtime::CreateOpts,
        ) -> Result<crate::runtime::SandboxInfo> {
            unimplemented!()
        }
        async fn start(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn stop(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn destroy(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn status(&self, _: &str) -> Result<SandboxStatus> {
            Ok(SandboxStatus::Running)
        }
        fn argv(&self, _: &str, _: &[&str], _: bool) -> Vec<String> {
            unimplemented!()
        }
        async fn list(&self) -> Result<Vec<crate::runtime::SandboxInfo>> {
            unimplemented!()
        }
        async fn snapshot_create(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn snapshot_restore(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn snapshot_list(&self, _: &str) -> Result<Vec<crate::runtime::SnapshotInfo>> {
            unimplemented!()
        }
        async fn upgrade(&self, _: &str, _: &[String]) -> Result<()> {
            unimplemented!()
        }
        async fn update_mounts(
            &self,
            _: &str,
            _: &[crate::runtime::Mount],
        ) -> Result<crate::runtime::MountUpdate> {
            unimplemented!()
        }
        async fn rollback_mounts(&self, _: &str, _: &crate::runtime::MountUpdate) -> Result<()> {
            unimplemented!()
        }
    }

    /// The stale-read fix: clearing the upper is only half of a discard,
    /// because a cached `read` under `/workspace` keeps serving the contents
    /// that were just thrown away until the mount is rebuilt.
    #[tokio::test]
    async fn discarding_everything_remounts_the_overlay_afterwards() {
        let guest = RecordingGuest::new(false);
        let count = discard(&guest, "devtest", None).await.expect("discard");
        assert_eq!(count, 1);

        let commands = guest.commands();
        let cleared = commands
            .iter()
            .position(|c| c.contains("rm -rf") && c.contains(UPPER))
            .expect("the upper is cleared");
        let remounted = commands
            .iter()
            .position(|c| c.contains("mount -o remount") && c.contains(WORKSPACE))
            .expect("the overlay is remounted");
        assert!(
            cleared < remounted,
            "the remount has to come after the clear: {commands:?}"
        );
    }

    #[tokio::test]
    async fn discarding_named_paths_remounts_too() {
        let guest = RecordingGuest::new(false);
        let paths = vec!["src/main.rs".to_string()];
        let count = discard(&guest, "devtest", Some(&paths))
            .await
            .expect("discard");
        assert_eq!(count, 1);
        assert!(
            guest
                .commands()
                .iter()
                .any(|c| c.contains("mount -o remount")),
            "a path-scoped discard leaves the same stale reads behind"
        );
    }

    /// A busy `/workspace` is the ordinary case (an editor, a shell sitting in
    /// it). The discard has already happened by then, so it must not be
    /// reported as a failure.
    #[tokio::test]
    async fn a_refused_remount_is_a_warning_not_a_failure() {
        let guest = RecordingGuest::new(true);
        let count = discard(&guest, "devtest", None)
            .await
            .expect("a busy workspace does not fail the discard");
        assert_eq!(count, 1);
    }

    /// Nothing was removed, so nothing can be stale — and an unnecessary
    /// remount would drop file handles for no reason.
    #[tokio::test]
    async fn a_discard_that_removed_nothing_leaves_the_mount_alone() {
        let guest = RecordingGuest::new(false);
        let count = discard(&guest, "devtest", Some(&[]))
            .await
            .expect("discard");
        assert_eq!(count, 0);
        assert!(guest.commands().is_empty(), "{:?}", guest.commands());
    }

    fn change(path: &str, status: ChangeStatus, is_dir: bool) -> OverlayChange {
        OverlayChange {
            status,
            path: path.into(),
            is_dir,
        }
    }

    #[test]
    fn meaningful_changes_keep_empty_new_directories_but_not_structural_parents() {
        let changes = vec![
            change("src", ChangeStatus::Modified, true),
            change("src/new", ChangeStatus::Added, true),
            change("src/new/file.txt", ChangeStatus::Added, false),
            change("empty", ChangeStatus::Added, true),
        ];

        let paths: Vec<&str> = meaningful_changes(&changes)
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(paths, vec!["src/new/file.txt", "empty"]);
    }
}
