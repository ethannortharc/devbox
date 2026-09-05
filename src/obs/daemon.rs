//! Long-lived host collector process.
//!
//! The guest agent is a service, so tying the other end of its transport to
//! `devbox web` made capture stop whenever the browser console did. This
//! module owns one per-user background process instead. The existing per-box
//! collector claims still protect every database; the daemon claim prevents a
//! pile of idle supervisors when several CLI commands race to start one.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::collector::{Stats, StatsSnapshot};
use crate::sandbox::SandboxManager;

const DISABLE_ENV: &str = "DEVBOX_NO_COLLECTOR_DAEMON";
const REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a build publishes when it cannot identify itself.
///
/// Its own executable can be gone — a worktree deleted while its daemon still
/// runs is not hypothetical — and an identity that cannot be computed must
/// read as "unknown", never as "the same as yours".
const UNKNOWN_BUILD: &str = "unknown";

/// Who owns the daemon lock.
///
/// The version alone was the whole identity, and that was wrong for the same
/// reason the guest agent's version was (see [`crate::sandbox::agent_sync`]):
/// one version number now covers builds with different capture capabilities.
/// A pre-eBPF `0.1.6` daemon left running kept spawning guest agents with
/// `-no-ebpf`, and every newer `0.1.6` binary looked at the version, agreed it
/// was current, and left it alone — so a box could have an eBPF agent
/// installed and still be told not to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerIdentity {
    pid: i32,
    version: String,
    /// The commit this build was made from, as a label for humans. Empty for
    /// an owner from before this field existed.
    commit: String,
    /// sha256 of the owner's own executable — the identity that decides.
    ///
    /// The commit cannot: two builds of one commit differ whenever the working
    /// tree, the embedded agent, or a feature flag differ, which is precisely
    /// the case this exists for. Empty for an owner from before this field
    /// existed, which is itself proof it is not this binary.
    build: String,
}

impl OwnerIdentity {
    /// This process, as an owner.
    fn mine() -> Result<Self> {
        Ok(Self {
            pid: i32::try_from(std::process::id()).context("collector pid exceeds i32")?,
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: build_commit().to_string(),
            build: host_build_digest().to_string(),
        })
    }

    /// A one-line rendering for `devbox doctor`.
    fn describe(&self) -> String {
        let mut described = format!("pid {} version {}", self.pid, self.version);
        if !self.commit.is_empty() {
            described.push_str(&format!(" commit {}", self.commit));
        }
        match self.build.as_str() {
            "" => described.push_str(" build unrecorded"),
            UNKNOWN_BUILD => described.push_str(" build unknown"),
            build => described.push_str(&format!(" build {}", &build[..build.len().min(12)])),
        }
        described
    }
}

/// The commit this binary was built from, or an empty string.
///
/// Stamped by `build.rs`, which reruns when the agent's own inputs change —
/// so it names the commit of the last agent rebuild, not necessarily HEAD.
/// That is why it is a label and [`host_build_digest`] is the identity.
fn build_commit() -> &'static str {
    option_env!("DEVBOX_BUILD_COMMIT").unwrap_or("")
}

/// sha256 of this process's own executable, computed once.
///
/// Streamed rather than read whole: this runs on every lifecycle command, and
/// a debug build is tens of megabytes.
fn host_build_digest() -> &'static str {
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIGEST.get_or_init(|| {
        digest_executable().unwrap_or_else(|error| {
            tracing::debug!(%error, "cannot identify this devbox build");
            UNKNOWN_BUILD.to_string()
        })
    })
}

fn digest_executable() -> Result<String> {
    use std::io::Read as _;

    use sha2::{Digest as _, Sha256};

    let path = std::env::current_exe().context("locate this devbox executable")?;
    let mut file = File::open(&path)
        .with_context(|| format!("read this devbox executable {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .with_context(|| format!("read this devbox executable {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    let mut encoded = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

/// Whether `mine` should take the daemon over from `owner`.
///
/// Version first, because that is the compatibility boundary. Then build
/// identity, because within one version it is the only thing that separates a
/// daemon that can attach eBPF probes from one that cannot.
///
/// Two asymmetries are deliberate:
///
/// - An owner with no build identity is replaced. This binary always publishes
///   one, so an owner without one cannot be this binary.
/// - A challenger with no build identity replaces nobody of its own version.
///   It cannot show it differs, and a takeover it cannot justify is one that
///   repeats on every command.
fn should_replace(owner: &OwnerIdentity, mine: &OwnerIdentity) -> bool {
    if owner.version != mine.version {
        return true;
    }
    if mine.build.is_empty() || mine.build == UNKNOWN_BUILD {
        return false;
    }
    owner.build != mine.build
}

fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("locks").join("collector-daemon.lock")
}

fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join("locks").join("collector-daemon.owner")
}

fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("collector.log")
}

fn stats_path(state_dir: &Path) -> PathBuf {
    state_dir.join("metrics").join("collector.json")
}

fn open_lock(state_dir: &Path) -> Result<File> {
    let path = lock_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create collector lock directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect collector lock directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // Ownership and identity are separate. This file is never truncated;
        // the complete identity is atomically renamed beside it.
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open collector daemon lock {}", path.display()))
}

/// Ensure the per-user collector daemon exists.
///
/// Safe to call from every lifecycle entry point. A held advisory lock means
/// another binary already owns the daemon. If several callers observe the
/// lock free together they may each spawn, but only one child can acquire it;
/// the others exit immediately.
pub fn ensure_running(manager: &SandboxManager) {
    if let Err(error) = try_ensure_running(manager) {
        // Capture is valuable, but it is never a prerequisite for creating,
        // entering, repairing, or tearing down a box. In particular, a stale
        // owner or a hung runtime during an upgrade must not turn every CLI
        // command into an observability-daemon error.
        tracing::warn!(%error, "background observability collector is unavailable");
    }
}

fn try_ensure_running(manager: &SandboxManager) -> Result<()> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return Ok(());
    }

    let probe = open_lock(&manager.state_dir)?;
    match probe.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {
            let owner = read_owner_identity(&manager.state_dir)?;
            let mine = OwnerIdentity::mine()?;
            if !should_replace(&owner, &mine) {
                return Ok(());
            }
            if !replace_outdated_owner(&probe, &manager.state_dir, &owner, &mine)? {
                return Ok(());
            }
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(anyhow::Error::new(error)).context("evaluate collector daemon ownership");
        }
        Ok(()) => File::unlock(&probe).context("release collector daemon ownership probe")?,
    }

    let path = log_path(&manager.state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create collector log directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect collector log directory {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open collector log {}", path.display()))?;
    let stderr = log
        .try_clone()
        .context("duplicate collector log for stderr")?;

    let executable = std::env::current_exe().context("locate devbox executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__collector")
        .current_dir(&manager.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        // Ctrl-C belongs to the foreground command or console, not to the
        // collector it happened to start.
        .process_group(0);
    command
        .spawn()
        .context("start the background observability collector")?;
    Ok(())
}

fn read_owner_identity(state_dir: &Path) -> Result<OwnerIdentity> {
    let path = identity_path(state_dir);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read collector daemon ownership record {}", path.display()))?;
    parse_owner_identity(&text).with_context(|| {
        format!(
            "collector daemon owns {} but published an invalid identity",
            path.display()
        )
    })
}

fn publish_owner_identity(state_dir: &Path, owner: &OwnerIdentity) -> Result<()> {
    let path = identity_path(state_dir);
    let parent = path.parent().context("collector identity has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create collector identity directory {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect collector identity directory {}", parent.display()))?;

    // A unique temporary avoids a crashed predecessor wedging publication.
    // `rename` means a concurrent ensure/status reader observes either the
    // previous complete record or this complete one, never a truncated file.
    let temporary = parent.join(format!(
        ".collector-daemon.owner-{}-{:016x}.tmp",
        owner.pid,
        rand::random::<u64>()
    ));
    let published = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create collector identity {}", temporary.display()))?;
        writeln!(
            file,
            "pid={} version={} commit={} build={}",
            owner.pid, owner.version, owner.commit, owner.build
        )
        .context("write collector daemon identity")?;
        file.sync_all().context("flush collector daemon identity")?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("publish collector daemon identity {}", path.display()))?;
        Ok(())
    })();
    if published.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    published
}

fn parse_owner_identity(text: &str) -> Result<OwnerIdentity> {
    let mut pid = None;
    let mut version = None;
    // Absent, not invalid: a daemon started by a build from before these
    // fields existed publishes neither, and that record must still parse —
    // replacing it is exactly what the missing build identity is evidence for.
    let mut commit = String::new();
    let mut build = String::new();
    for field in text.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = Some(
                value
                    .parse::<i32>()
                    .context("collector pid is not an integer")?,
            );
        } else if let Some(value) = field.strip_prefix("version=") {
            version = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("commit=") {
            commit = value.to_string();
        } else if let Some(value) = field.strip_prefix("build=") {
            build = value.to_string();
        }
    }
    let pid = pid.context("collector identity has no pid")?;
    if pid <= 1 || pid == std::process::id() as i32 {
        bail!("collector identity contains unsafe pid {pid}")
    }
    let version = version
        .filter(|value| !value.is_empty())
        .context("collector identity has no version")?;
    Ok(OwnerIdentity {
        pid,
        version,
        commit,
        build,
    })
}

/// Ask an older binary to release the stable daemon lock, then prove it did
/// before launching the replacement. The pid comes from a 0600 record held
/// under the same advisory lock, so another local user cannot redirect the
/// signal. A bounded wait is important: two collectors must never supervise
/// the same boxes just because shutdown got stuck.
fn replace_outdated_owner(
    probe: &File,
    state_dir: &Path,
    owner: &OwnerIdentity,
    mine: &OwnerIdentity,
) -> Result<bool> {
    let process = Command::new("ps")
        .args(["-ww", "-p", &owner.pid.to_string(), "-o", "command="])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("inspect outdated collector process {}", owner.pid))?;
    let command_line = String::from_utf8_lossy(&process.stdout);
    if !process.status.success()
        || !command_line
            .split_whitespace()
            .any(|arg| arg == "__collector")
    {
        bail!(
            "collector lock names pid {}, but that process is not a devbox __collector; \
             refusing to signal it",
            owner.pid
        );
    }

    let signal = Command::new("kill")
        .args(["-TERM", &owner.pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("signal outdated collector process {}", owner.pid))?;

    let deadline = Instant::now() + REPLACEMENT_TIMEOUT;
    loop {
        match probe.try_lock() {
            Ok(()) => {
                File::unlock(probe).context("release collector replacement probe")?;
                return Ok(true);
            }
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                // Another lifecycle command may have won the release and
                // already spawned this version. Do not wait for that healthy
                // replacement to exit, and do not spawn a duplicate.
                if read_owner_identity(state_dir)
                    .ok()
                    .is_some_and(|current| !should_replace(&current, mine))
                {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let detail = String::from_utf8_lossy(&signal.stderr);
                bail!(
                    "collector {} (version {}) did not stop within {} seconds{}",
                    owner.pid,
                    owner.version,
                    REPLACEMENT_TIMEOUT.as_secs(),
                    if detail.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", detail.trim())
                    }
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(anyhow::Error::new(error))
                    .context("wait for outdated collector daemon to release ownership");
            }
        }
    }
}

/// Run the daemon until the process is terminated.
///
/// This is reached only through the hidden `__collector` subcommand.
pub async fn run(manager: Arc<SandboxManager>) -> Result<()> {
    let claim = open_lock(&manager.state_dir)?;
    match claim.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(anyhow::Error::new(error)).context("claim collector daemon ownership");
        }
    }

    publish_owner_identity(&manager.state_dir, &OwnerIdentity::mine()?)?;

    let stats = Arc::new(Stats::default());
    let (stop, mut stopping) = tokio::sync::watch::channel(false);
    let supervised = tokio::spawn(super::supervisor::run(
        manager.clone(),
        stats.clone(),
        async move {
            let _ = stopping.wait_for(|value| *value).await;
        },
    ));
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = interval.tick() => {
                if let Err(error) = publish_stats(&manager.state_dir, stats.snapshot()) {
                    tracing::warn!(%error, "publish collector metrics snapshot");
                }
            }
        }
    }
    let _ = stop.send(true);
    let _ = supervised.await;
    publish_stats(&manager.state_dir, stats.snapshot())?;
    File::unlock(&claim).context("release collector daemon ownership")?;
    Ok(())
}

fn publish_stats(state_dir: &Path, snapshot: StatsSnapshot) -> Result<()> {
    let path = stats_path(state_dir);
    let parent = path
        .parent()
        .context("collector metrics path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create collector metrics directory {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect collector metrics directory {}", parent.display()))?;
    let temporary = parent.join("collector.json.tmp");
    let body = serde_json::to_vec(&snapshot).context("encode collector metrics snapshot")?;
    std::fs::write(&temporary, body)
        .with_context(|| format!("write collector metrics snapshot {}", temporary.display()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect collector metrics snapshot {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("publish collector metrics snapshot {}", path.display()))?;
    Ok(())
}

/// Last counters published by the background collector.
pub fn stats_snapshot(manager: &SandboxManager) -> Result<StatsSnapshot> {
    let path = stats_path(&manager.state_dir);
    if !path.exists() {
        return Ok(StatsSnapshot::default());
    }
    let raw = std::fs::read(&path)
        .with_context(|| format!("read collector metrics snapshot {}", path.display()))?;
    serde_json::from_slice(&raw).context("decode collector metrics snapshot")
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "install collector SIGTERM handler");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "install collector Ctrl-C handler");
            }
        }
        _ = terminate.recv() => {}
    }
}

/// Read the current ownership record for diagnostics.
pub fn status(manager: &SandboxManager) -> Result<Option<String>> {
    let claim = open_lock(&manager.state_dir)?;
    match claim.try_lock() {
        Ok(()) => {
            File::unlock(&claim).context("release collector daemon status probe")?;
            Ok(None)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let text = std::fs::read_to_string(identity_path(&manager.state_dir))
                .context("read collector daemon ownership record")?;
            if text.trim().is_empty() {
                bail!("collector daemon owns the lock but published no identity")
            }
            // Rendered rather than echoed. The record grew a build digest, and
            // `devbox doctor` is where someone asks "is the daemon serving my
            // boxes the binary I just built?" — a raw 64-character field
            // answers that badly.
            Ok(Some(match parse_owner_identity(&text) {
                Ok(owner) => owner.describe(),
                Err(_) => text.trim().to_string(),
            }))
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(anyhow::Error::new(error)).context("evaluate collector daemon status")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_bookkeeping_stays_under_private_state_directories() {
        let root = Path::new("/tmp/devbox-test-state");
        assert_eq!(
            lock_path(root),
            root.join("locks").join("collector-daemon.lock")
        );
        assert_eq!(
            identity_path(root),
            root.join("locks").join("collector-daemon.owner")
        );
        assert_eq!(log_path(root), root.join("logs").join("collector.log"));
        assert_eq!(
            stats_path(root),
            root.join("metrics").join("collector.json")
        );
    }

    #[test]
    fn metrics_snapshot_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        let expected = StatsSnapshot {
            received: 7,
            stored: 6,
            dropped: 1,
            ..Default::default()
        };
        publish_stats(&manager.state_dir, expected).unwrap();
        assert_eq!(stats_snapshot(&manager).unwrap(), expected);
    }

    fn owner(version: &str, build: &str) -> OwnerIdentity {
        OwnerIdentity {
            pid: 4242,
            version: version.to_string(),
            commit: String::new(),
            build: build.to_string(),
        }
    }

    #[test]
    fn collector_owner_identity_round_trips_and_rejects_unsafe_pids() {
        assert_eq!(
            parse_owner_identity("pid=4242 version=1.2.3 commit=abc123 build=ff00\n").unwrap(),
            OwnerIdentity {
                pid: 4242,
                version: "1.2.3".to_string(),
                commit: "abc123".to_string(),
                build: "ff00".to_string(),
            }
        );
        for invalid in ["", "pid=1 version=old", "pid=nope version=old", "pid=42"] {
            assert!(parse_owner_identity(invalid).is_err(), "{invalid:?}");
        }
    }

    /// A record written before the daemon carried a build identity still
    /// parses — and reads as "not this binary", which is what it is.
    #[test]
    fn an_identity_from_before_the_build_field_still_parses() {
        let old = parse_owner_identity("pid=4242 version=1.2.3\n").unwrap();
        assert_eq!(old.build, "");
        assert_eq!(old.commit, "");
        assert!(should_replace(&old, &owner("1.2.3", "beef")));
    }

    /// The case this exists for: one version number, two builds. Before, the
    /// version match alone left a pre-eBPF daemon running for the life of the
    /// login session, and every box it attached to was told `-no-ebpf`.
    #[test]
    fn the_same_version_built_differently_is_taken_over() {
        assert!(should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", "bbbb")
        ));
    }

    #[test]
    fn the_same_build_is_left_alone() {
        assert!(!should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", "aaaa")
        ));
    }

    #[test]
    fn a_different_version_is_taken_over_whatever_the_build_says() {
        assert!(should_replace(
            &owner("1.2.2", "aaaa"),
            &owner("1.2.3", "aaaa")
        ));
        assert!(should_replace(&owner("1.2.2", ""), &owner("1.2.3", "")));
    }

    /// A challenger that cannot hash its own executable — a deleted worktree,
    /// an unreadable path — must not take over on every command it runs.
    #[test]
    fn a_challenger_that_cannot_identify_itself_replaces_nobody_of_its_version() {
        assert!(!should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", UNKNOWN_BUILD)
        ));
        assert!(!should_replace(&owner("1.2.3", ""), &owner("1.2.3", "")));
        // It is still replaced across a version change, which is the boundary
        // that never depended on build identity.
        assert!(should_replace(
            &owner("1.2.2", "aaaa"),
            &owner("1.2.3", UNKNOWN_BUILD)
        ));
    }

    #[test]
    fn the_doctor_line_names_the_build_without_printing_all_of_it() {
        let described = OwnerIdentity {
            pid: 7,
            version: "0.1.6".into(),
            commit: "11cc51fbf1d3".into(),
            build: "c7d70a0857d42e7fbf0062fec377477658ff76cc8412b3210e60023a1736cca0".into(),
        }
        .describe();
        assert_eq!(
            described,
            "pid 7 version 0.1.6 commit 11cc51fbf1d3 build c7d70a0857d4"
        );
        assert!(owner("0.1.6", "").describe().ends_with("build unrecorded"));
        assert!(
            owner("0.1.6", UNKNOWN_BUILD)
                .describe()
                .ends_with("build unknown")
        );
    }

    /// This binary can always identify itself, so nothing it publishes reads
    /// as an owner worth replacing.
    #[test]
    fn this_build_identifies_itself() {
        let mine = OwnerIdentity::mine().unwrap();
        assert_ne!(mine.build, "", "a live test binary has an executable");
        assert_ne!(mine.build, UNKNOWN_BUILD);
        assert!(!should_replace(&mine, &mine));
    }

    #[test]
    fn collector_owner_identity_is_published_as_a_complete_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let owner = OwnerIdentity {
            pid: 4242,
            version: "1.2.3".to_string(),
            commit: "abc123def456".to_string(),
            build: "c7d70a08".to_string(),
        };
        publish_owner_identity(dir.path(), &owner).unwrap();
        assert_eq!(read_owner_identity(dir.path()).unwrap(), owner);
        assert!(!lock_path(dir.path()).exists());
        assert!(
            std::fs::read_dir(dir.path().join("locks"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
    }
}
