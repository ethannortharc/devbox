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

#[derive(Debug, PartialEq, Eq)]
struct OwnerIdentity {
    pid: i32,
    version: String,
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
            if owner.version == env!("CARGO_PKG_VERSION") {
                return Ok(());
            }
            if !replace_outdated_owner(&probe, &manager.state_dir, &owner)? {
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
        writeln!(file, "pid={} version={}", owner.pid, owner.version)
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
    for field in text.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = Some(
                value
                    .parse::<i32>()
                    .context("collector pid is not an integer")?,
            );
        } else if let Some(value) = field.strip_prefix("version=") {
            version = Some(value.to_string());
        }
    }
    let pid = pid.context("collector identity has no pid")?;
    if pid <= 1 || pid == std::process::id() as i32 {
        bail!("collector identity contains unsafe pid {pid}")
    }
    let version = version
        .filter(|value| !value.is_empty())
        .context("collector identity has no version")?;
    Ok(OwnerIdentity { pid, version })
}

/// Ask an older binary to release the stable daemon lock, then prove it did
/// before launching the replacement. The pid comes from a 0600 record held
/// under the same advisory lock, so another local user cannot redirect the
/// signal. A bounded wait is important: two collectors must never supervise
/// the same boxes just because shutdown got stuck.
fn replace_outdated_owner(probe: &File, state_dir: &Path, owner: &OwnerIdentity) -> Result<bool> {
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
                    .is_some_and(|owner| owner.version == env!("CARGO_PKG_VERSION"))
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

    publish_owner_identity(
        &manager.state_dir,
        &OwnerIdentity {
            pid: i32::try_from(std::process::id()).context("collector pid exceeds i32")?,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )?;

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
            Ok(Some(text.trim().to_string()))
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

    #[test]
    fn collector_owner_identity_round_trips_and_rejects_unsafe_pids() {
        assert_eq!(
            parse_owner_identity("pid=4242 version=1.2.3\n").unwrap(),
            OwnerIdentity {
                pid: 4242,
                version: "1.2.3".to_string(),
            }
        );
        for invalid in ["", "pid=1 version=old", "pid=nope version=old", "pid=42"] {
            assert!(parse_owner_identity(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn collector_owner_identity_is_published_as_a_complete_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let owner = OwnerIdentity {
            pid: 4242,
            version: "1.2.3".to_string(),
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
