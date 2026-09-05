//! Overlay checkpoints — a copy of the upper layer at a moment, plus a manifest.
//!
//! A checkpoint is deliberately *not* a new overlay layer. Stacking lower dirs
//! would need a remount, and a remount tears down every open file handle in
//! `/workspace`; the upper layer is small by construction (it holds only what
//! the box has written), so copying it is both simpler and cheaper than the
//! mount gymnastics would be.
//!
//! Guest layout:
//!
//! ```text
//! /var/devbox/overlay/upper/                  live upper (owned by `overlay`)
//! /var/devbox/checkpoints/<id>/upper/         cp -a --reflink=auto of it
//! /var/devbox/checkpoints/<id>/manifest.json  id, label, created_at, run_id, files, bytes
//! ```
//!
//! Whiteouts (character devices with rdev 0/0) and opaque directories
//! (`trusted.overlay.opaque`) are copied verbatim — verified on the devtest
//! guest, where `cp -a` preserves the device node, the `trusted.*` xattr and
//! the nanosecond mtime — so a checkpoint is a faithful upper and restoring one
//! puts the box back exactly where it was.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::obs::Store;
use crate::obs::run::ActiveRun;
use crate::runtime::Runtime;
use crate::sandbox::overlay::{
    self, ChangeStatus, EntryKind, OverlayChange, TreeEntry, UPPER, enumerate_tree,
};

/// Where checkpoints live inside the guest.
pub const CHECKPOINTS_DIR: &str = "/var/devbox/checkpoints";

/// How many unpinned checkpoints survive a [`prune`] by default.
pub const DEFAULT_KEEP: usize = 20;

/// A checkpoint's identity: time-sortable, short, and safe to paste.
///
/// The first ten characters are the creation time in milliseconds, so plain
/// lexicographic order is chronological order and `list` never needs to parse a
/// date to sort. The last four are random, so two checkpoints taken in the same
/// millisecond do not collide. The alphabet is Crockford base32 minus its
/// ambiguous letters (no `i`, `l`, `o`, `u`), which also means an id can never
/// contain a shell metacharacter — the ids are interpolated into guest paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckpointId(String);

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Encode the low `width * 5` bits of `value`, most significant digit first.
fn base32(mut value: u64, width: usize) -> String {
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 31) as usize];
        value >>= 5;
    }
    String::from_utf8(out).expect("the Crockford alphabet is ASCII")
}

impl CheckpointId {
    /// Build an id from an explicit clock and entropy.
    ///
    /// Separate from [`CheckpointId::generate`] so the sort order can be
    /// tested without sleeping.
    pub fn at(millis: u64, entropy: u64) -> Self {
        Self(format!("{}{}", base32(millis, 10), base32(entropy, 4)))
    }

    /// A fresh id for right now.
    ///
    /// Not `new`: an id is a value with a clock and a coin flip in it, not a
    /// default-constructible thing, and `Default::default()` handing back a
    /// different id every call would be a trap.
    pub fn generate() -> Self {
        let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
        Self::at(millis, rand::random::<u64>())
    }

    /// Accept an id typed by a person.
    ///
    /// Everything downstream interpolates the id into a guest shell command, so
    /// this is the one gate: only the Crockford alphabet gets through, which
    /// leaves nothing a shell could act on.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("a checkpoint id cannot be empty");
        }
        if trimmed.len() > 32 {
            bail!("'{trimmed}' is not a checkpoint id (too long)");
        }
        if let Some(bad) = trimmed
            .chars()
            .find(|c| !CROCKFORD.contains(&(*c as u8)) || !c.is_ascii())
        {
            bail!("'{trimmed}' is not a checkpoint id (unexpected character '{bad}')");
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckpointId {
    /// `f.pad`, not `write_str`: the listing puts ids in a `{:<16}` column, and
    /// a `Display` that writes straight to the formatter silently ignores the
    /// width.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&self.0)
    }
}

/// What `manifest.json` holds, and what the API hands back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    /// The `--label` the user gave, if any.
    pub label: Option<String>,
    /// RFC3339, UTC, second precision — the same spelling the rest of devbox
    /// writes timestamps in.
    pub created_at: String,
    /// The run this checkpoint belongs to, set by [`create_for_run`].
    ///
    /// [`prune_plan`] never deletes a checkpoint that has one: a run report
    /// links to its start and end checkpoints, and a report whose evidence has
    /// been garbage-collected is worse than a slightly larger directory.
    pub run_id: Option<String>,
    /// Non-directory entries in the saved upper — regular files, symlinks and
    /// whiteouts. Whiteouts count because a deletion is a change the
    /// checkpoint carries.
    pub files: usize,
    /// Sum of the regular files' sizes.
    pub bytes: u64,
}

/// Which tree a [`diff`] compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Another checkpoint.
    Checkpoint(CheckpointId),
    /// The live upper layer, i.e. the box as it is right now.
    Live,
}

/// Refuse to rewrite the upper out from under a run that is still going.
///
/// A restore replaces `/workspace` wholesale. Doing that under a live run
/// destroys the very thing that run's report is about — its end checkpoint
/// would describe a tree the command never produced — and it is invisible
/// afterwards, because the report is assembled from whatever the upper holds
/// at the end.
///
/// Sync rather than async: the answer is one indexed SQLite read on a store
/// the caller already holds open, and marking it `async` would promise an
/// await point it does not have. [`restore`] is the only caller and is async
/// itself, so nothing about the call site changes.
fn active_run_guard(store: &Store, box_name: &str) -> Result<()> {
    let active = store
        .active_runs()
        .with_context(|| format!("could not check whether box '{box_name}' has a live run"))?;
    refuse_while_running(box_name, &active)
}

/// The rule itself, over the runs rather than over a database.
///
/// Split out so it can be tested without a guest and without a store. The
/// interesting part is the message: a refusal that says "a run is in progress"
/// without saying *which* leaves the reader with nowhere to go.
pub fn refuse_while_running(box_name: &str, active: &[ActiveRun]) -> Result<()> {
    if active.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = active.iter().map(|r| r.run_id.as_str()).collect();
    // Written as one literal rather than as a `\`-continued one. rustfmt joins
    // a continued literal back onto a single line and the continuation's
    // indentation survives into the message, which is how this refusal spent a
    // round reading "would rewrite          /workspace".
    bail!(
        "Box '{box_name}' has {} run(s) in progress ({}); restoring would rewrite /workspace underneath them. Wait for them to finish, or run `devbox runs {box_name}` to see what is still going.",
        active.len(),
        ids.join(", ")
    );
}

fn checkpoint_dir(id: &CheckpointId) -> String {
    format!("{CHECKPOINTS_DIR}/{id}")
}

fn checkpoint_upper(id: &CheckpointId) -> String {
    format!("{CHECKPOINTS_DIR}/{id}/upper")
}

/// Take a checkpoint of the box's current upper layer.
///
/// The manifest is written last, so a copy that dies halfway leaves a directory
/// [`list`] ignores rather than a checkpoint that lies about its contents; the
/// partial directory is removed on the way out too.
pub async fn create(
    runtime: &dyn Runtime,
    box_name: &str,
    label: Option<&str>,
) -> Result<Checkpoint> {
    create_inner(runtime, box_name, label, None).await
}

/// Take a checkpoint that belongs to a run (§4.1, §5.1).
///
/// The only difference from [`create`] is the `run_id` in the manifest, and
/// that field is not decoration: [`prune_plan`] never drops a checkpoint that
/// has one, so a report can still show its file diff months later. A run takes
/// two — `run-start` and `run-end` — and the pair is what makes the report's
/// Files section describe *the run* rather than the box's whole history.
pub async fn create_for_run(
    runtime: &dyn Runtime,
    box_name: &str,
    run_id: &str,
    label: Option<&str>,
) -> Result<Checkpoint> {
    create_inner(runtime, box_name, label, Some(run_id)).await
}

async fn create_inner(
    runtime: &dyn Runtime,
    box_name: &str,
    label: Option<&str>,
    run_id: Option<&str>,
) -> Result<Checkpoint> {
    let id = CheckpointId::generate();
    let dir = checkpoint_dir(&id);

    let copy = format!(
        "set -e; \
         if [ ! -d '{UPPER}' ]; then echo 'this box has no overlay upper layer' >&2; exit 3; fi; \
         if [ -e '{dir}' ]; then echo 'checkpoint {id} already exists' >&2; exit 4; fi; \
         mkdir -p '{dir}'; \
         cp -a --reflink=auto '{UPPER}' '{dir}/upper'"
    );
    let result = runtime.run_as_root(box_name, &copy, false).await?;
    if result.exit_code != 0 {
        let _ = runtime
            .run_as_root(box_name, &format!("rm -rf '{dir}'"), false)
            .await;
        bail!(
            "Failed to copy the overlay upper layer: {}",
            result.stderr.trim()
        );
    }

    // Count from the copy rather than from the live upper: it is the thing the
    // manifest describes, and reading it back also proves the copy landed.
    let entries = match enumerate_tree(runtime, box_name, &checkpoint_upper(&id)).await {
        Ok(entries) => entries,
        Err(error) => {
            let _ = runtime
                .run_as_root(box_name, &format!("rm -rf '{dir}'"), false)
                .await;
            return Err(error);
        }
    };

    let checkpoint = Checkpoint {
        id: id.clone(),
        label: label.map(str::to_string),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        run_id: run_id.map(str::to_string),
        files: entries.iter().filter(|e| e.kind != EntryKind::Dir).count(),
        bytes: entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.size)
            .sum(),
    };

    // `to_string` and not `to_string_pretty`: the manifest goes into the guest
    // through a quoted heredoc, and compact JSON has no raw newline in it —
    // serde escapes any newline inside a label — so a label can never close
    // the heredoc early.
    let json = serde_json::to_string(&checkpoint)?;
    let write =
        format!("set -e; cat > '{dir}/manifest.json' << 'DEVBOX_CP_EOF'\n{json}\nDEVBOX_CP_EOF");
    let result = runtime.run_as_root(box_name, &write, false).await?;
    if result.exit_code != 0 {
        let _ = runtime
            .run_as_root(box_name, &format!("rm -rf '{dir}'"), false)
            .await;
        bail!(
            "Failed to write the checkpoint manifest: {}",
            result.stderr.trim()
        );
    }

    // Retention lives here rather than in the CLI so that every caller —
    // including component A, which takes a checkpoint at the start of a run —
    // gets it. Best effort: a checkpoint that exists is worth more than a tidy
    // directory, so a failed prune warns instead of undoing the create.
    match prune(runtime, box_name, DEFAULT_KEEP).await {
        Ok(dropped) if !dropped.is_empty() => {
            println!(
                "Pruned {} old checkpoint(s), keeping the newest {DEFAULT_KEEP}.",
                dropped.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("Warning: could not prune old checkpoints: {error:#}"),
    }

    Ok(checkpoint)
}

/// Every checkpoint on the box, oldest first.
///
/// A directory without a readable manifest is skipped rather than reported: it
/// is either a half-written checkpoint or something that is not a checkpoint at
/// all, and neither is worth failing the whole listing over.
pub async fn list(runtime: &dyn Runtime, box_name: &str) -> Result<Vec<Checkpoint>> {
    let cmd = format!(
        "for f in {CHECKPOINTS_DIR}/*/manifest.json; do [ -f \"$f\" ] || continue; cat \"$f\"; echo; done"
    );
    let result = runtime.run_as_root(box_name, &cmd, false).await?;
    if result.exit_code != 0 {
        bail!("Failed to list checkpoints: {}", result.stderr.trim());
    }

    let mut checkpoints = parse_manifests(&result.stdout);
    checkpoints.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(checkpoints)
}

/// Parse one manifest per line.
///
/// Pure so the "a stray file in the directory must not sink the listing" rule
/// can be tested without a guest.
pub fn parse_manifests(stdout: &str) -> Vec<Checkpoint> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Checkpoint>(line).ok())
        .collect()
}

/// Resolve what the user typed to exactly one checkpoint.
///
/// An exact id wins outright; otherwise a unique prefix is accepted, because
/// fourteen characters is more than anyone wants to retype and the ids sort by
/// time, so the first few characters are usually enough.
pub fn resolve_id(checkpoints: &[Checkpoint], typed: &CheckpointId) -> Result<CheckpointId> {
    if let Some(exact) = checkpoints.iter().find(|c| c.id == *typed) {
        return Ok(exact.id.clone());
    }
    let matches: Vec<&Checkpoint> = checkpoints
        .iter()
        .filter(|c| c.id.as_str().starts_with(typed.as_str()))
        .collect();
    match matches.as_slice() {
        [] => bail!(
            "no checkpoint '{typed}' on this box (run `devbox layer checkpoints` to see what there is)"
        ),
        [only] => Ok(only.id.clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|c| c.id.as_str()).collect();
            bail!(
                "'{typed}' matches {} checkpoints: {}",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

/// Compare a checkpoint against another checkpoint or against the live upper.
///
/// The result is the same [`OverlayChange`] list `devbox diff` produces, so the
/// CLI, the Files tab and the run report all render one representation.
pub async fn diff(
    runtime: &dyn Runtime,
    box_name: &str,
    from: &CheckpointId,
    to: Target,
) -> Result<Vec<OverlayChange>> {
    let known = list(runtime, box_name).await?;
    let from = resolve_id(&known, from)?;

    let from_entries = enumerate_tree(runtime, box_name, &checkpoint_upper(&from)).await?;
    let to_entries = match &to {
        Target::Live => enumerate_tree(runtime, box_name, UPPER).await?,
        Target::Checkpoint(id) => {
            let id = resolve_id(&known, id)?;
            enumerate_tree(runtime, box_name, &checkpoint_upper(&id)).await?
        }
    };

    Ok(diff_trees(&from_entries, &to_entries))
}

/// The state a path is in, as far as one upper layer is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State<'a> {
    /// The upper says nothing about this path — whatever the lower has shows
    /// through.
    Absent,
    /// A whiteout: the upper says this path is deleted.
    Whiteout,
    /// The upper holds this entry.
    Present(&'a TreeEntry),
}

impl<'a> State<'a> {
    fn of(entry: Option<&'a TreeEntry>) -> Self {
        match entry {
            None => Self::Absent,
            Some(e) if e.is_whiteout => Self::Whiteout,
            Some(e) => Self::Present(e),
        }
    }
}

/// Diff two enumerated trees.
///
/// The pure half of [`diff`] — everything that can be got wrong lives here, and
/// none of it needs a VM.
///
/// The statuses describe what happened to `/workspace`, not to the upper
/// directory, which is why a whiteout appearing counts as `Deleted` and a
/// whiteout disappearing counts as `Added`. Two of the transitions cannot be
/// decided from the uppers alone:
///
/// - an entry present in `from` and absent in `to` reverts to whatever the
///   lower layer holds. That is `Deleted` when the lower has nothing — the
///   common case, since files the box created live only in the upper — and
///   really a revert when it does. Calling it `Deleted` matches what `from`
///   shows the user.
/// - a directory's own size and mtime move whenever a child changes, so
///   directories are compared on opaqueness alone. A directory that merely
///   contains a changed file is not itself a change.
pub fn diff_trees(from: &[TreeEntry], to: &[TreeEntry]) -> Vec<OverlayChange> {
    let index = |entries: &'_ [TreeEntry]| -> std::collections::BTreeMap<String, TreeEntry> {
        entries
            .iter()
            .map(|e| (e.path.clone(), e.clone()))
            .collect()
    };
    let from_index = index(from);
    let to_index = index(to);

    let mut paths: Vec<&String> = from_index.keys().chain(to_index.keys()).collect();
    paths.sort();
    paths.dedup();

    let mut changes = vec![];
    for path in paths {
        let before = State::of(from_index.get(path));
        let after = State::of(to_index.get(path));

        let (status, is_dir) = match (before, after) {
            // Nothing to say.
            (State::Absent, State::Absent) | (State::Whiteout, State::Whiteout) => continue,

            // The path gained content, either from nothing or by undoing a
            // deletion (`Whiteout -> Absent` means the lower shows through
            // again).
            (State::Absent, State::Present(e)) | (State::Whiteout, State::Present(e)) => {
                (ChangeStatus::Added, e.kind == EntryKind::Dir)
            }
            (State::Whiteout, State::Absent) => (ChangeStatus::Added, false),

            // The path lost its content.
            (State::Absent, State::Whiteout) => (ChangeStatus::Deleted, false),
            (State::Present(e), State::Absent) | (State::Present(e), State::Whiteout) => {
                (ChangeStatus::Deleted, e.kind == EntryKind::Dir)
            }

            (State::Present(a), State::Present(b)) => {
                let changed = if a.kind != b.kind {
                    true
                } else if b.kind == EntryKind::Dir {
                    a.is_opaque != b.is_opaque
                } else {
                    a.size != b.size || a.mtime != b.mtime
                };
                if !changed {
                    continue;
                }
                (ChangeStatus::Modified, b.kind == EntryKind::Dir)
            }
        };

        changes.push(OverlayChange {
            status,
            path: path.clone(),
            is_dir,
        });
    }

    changes
}

/// Put the box's upper layer back to what a checkpoint holds.
///
/// The upper is cleared the way `overlay::discard` clears it, then the
/// checkpoint is copied back in. `src/.` rather than a `*` glob so dotfiles
/// come along without a second command.
///
/// The overlay is remounted afterwards because the kernel does not expect the
/// upper to move under a live mount: `readdir` picks the change up immediately,
/// but a cached `read` keeps serving the old contents until the mount is
/// rebuilt — verified on devtest, kernel 6.19. `overlay::refresh` already
/// handles the kernels that refuse `mount -o remount`. A remount needs
/// `/workspace` to be idle, so a failure is a warning rather than an error:
/// the restore itself has already happened by then, and `devbox diff` (which
/// reads the upper directly) will agree with it.
pub async fn restore(
    runtime: &dyn Runtime,
    store: &Store,
    box_name: &str,
    id: &CheckpointId,
) -> Result<()> {
    active_run_guard(store, box_name)?;

    let known = list(runtime, box_name).await?;
    let id = resolve_id(&known, id)?;
    let upper = checkpoint_upper(&id);

    let cmd = format!(
        "set -e; \
         if [ ! -d '{upper}' ]; then echo 'checkpoint {id} has no saved upper layer' >&2; exit 3; fi; \
         rm -rf {UPPER}/* {UPPER}/.[!.]* 2>/dev/null; true; \
         mkdir -p '{UPPER}'; \
         cp -a --reflink=auto '{upper}/.' '{UPPER}/'"
    );
    let result = runtime.run_as_root(box_name, &cmd, false).await?;
    if result.exit_code != 0 {
        bail!(
            "Failed to restore checkpoint {id}: {}",
            result.stderr.trim()
        );
    }

    println!("Restored checkpoint {id}.");

    if let Err(error) = overlay::refresh(runtime, box_name).await {
        eprintln!(
            "Warning: the upper layer is restored but /workspace could not be remounted: {error:#}"
        );
        eprintln!(
            "         Processes with files open under /workspace may still read the old contents."
        );
        eprintln!("         Close them and run `devbox layer refresh {box_name}`.");
    }

    Ok(())
}

/// Delete one checkpoint.
pub async fn delete(runtime: &dyn Runtime, box_name: &str, id: &CheckpointId) -> Result<()> {
    let known = list(runtime, box_name).await?;
    let id = resolve_id(&known, id)?;
    let dir = checkpoint_dir(&id);

    let result = runtime
        .run_as_root(box_name, &format!("rm -rf '{dir}'"), false)
        .await?;
    if result.exit_code != 0 {
        bail!("Failed to delete checkpoint {id}: {}", result.stderr.trim());
    }
    Ok(())
}

/// Drop the oldest checkpoints until only `keep` unpinned ones remain.
///
/// Returns what it deleted.
pub async fn prune(
    runtime: &dyn Runtime,
    box_name: &str,
    keep: usize,
) -> Result<Vec<CheckpointId>> {
    let checkpoints = list(runtime, box_name).await?;
    let doomed = prune_plan(&checkpoints, keep);

    for id in &doomed {
        let dir = checkpoint_dir(id);
        let result = runtime
            .run_as_root(box_name, &format!("rm -rf '{dir}'"), false)
            .await?;
        if result.exit_code != 0 {
            bail!("Failed to prune checkpoint {id}: {}", result.stderr.trim());
        }
    }

    Ok(doomed)
}

/// Which checkpoints a [`prune`] would delete, oldest first.
///
/// A checkpoint with a `run_id` belongs to a run and is never dropped — the run
/// report links to it, and a report whose checkpoint has been garbage-collected
/// is worse than a slightly larger checkpoints directory. Pinned checkpoints do
/// not consume the `keep` budget either, so pinning one never silently evicts
/// an unpinned one.
pub fn prune_plan(checkpoints: &[Checkpoint], keep: usize) -> Vec<CheckpointId> {
    let mut unpinned: Vec<&Checkpoint> =
        checkpoints.iter().filter(|c| c.run_id.is_none()).collect();
    unpinned.sort_by(|a, b| a.id.cmp(&b.id));

    let doomed = unpinned.len().saturating_sub(keep);
    unpinned.iter().take(doomed).map(|c| c.id.clone()).collect()
}
