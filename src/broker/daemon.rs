//! The supervised broker process — §6.1, "supervised like the collector".
//!
//! This deliberately copies `obs::daemon`'s shape rather than inventing a
//! second one: an advisory lock on a stable path decides who owns the
//! daemon, a 0600 identity sidecar published by rename says who that is and
//! which version they are, and a newer binary asks an older owner to stand
//! down before taking over. Two of those were hard-won — the sidecar exists
//! because a truncated lock file made `status` lie, and the version check
//! exists because an upgrade otherwise left the old process running.
//!
//! What it does **not** copy: the collector supervises one task per box and
//! publishes a metrics snapshot every second. The broker has neither; it has
//! one listener and one published endpoint, written once at startup. A poll
//! loop that republished an unchanging port every second would be noise in
//! `fs_usage` and nothing else.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::server::BrokerState;
use crate::sandbox::SandboxManager;

const REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerIdentity {
    pid: i32,
    version: String,
    /// The daemon's own process group, or `0` when it has none recorded.
    ///
    /// Same reason as the collector's (see [`crate::obs::daemon`]): a daemon's
    /// children inherit its group, and stopping the pid alone leaves them.
    /// Written only after the daemon has proved it *leads* the group.
    pgid: i32,
}

/// How long an unaccounted broker is given to go quietly.
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(3);

fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("locks").join("broker-daemon.lock")
}

fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join("locks").join("broker-daemon.owner")
}

fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("broker.log")
}

fn open_lock(state_dir: &Path) -> Result<File> {
    let path = lock_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create broker lock directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect broker lock directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open broker daemon lock {}", path.display()))
}

/// Ensure the per-user broker exists.
///
/// Safe to call from every lifecycle entry point, and silent when it cannot:
/// a box must still start, stop, and be entered on a host where no secret has
/// ever been stored. The one thing that would be wrong is failing loudly and
/// stopping the command the user actually asked for.
pub fn ensure_running(manager: &SandboxManager) {
    if let Err(error) = try_ensure_running(manager) {
        tracing::warn!(%error, "credential broker is unavailable");
    }
}

fn try_ensure_running(manager: &SandboxManager) -> Result<()> {
    if std::env::var_os(super::DISABLE_ENV).is_some() {
        return Ok(());
    }
    // Nothing to broker: starting a listener for zero secrets would be a
    // process, a port, and a log file that buy nothing.
    if super::configured_providers(&manager.state_dir).is_empty() {
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
            return Err(anyhow::Error::new(error)).context("evaluate broker daemon ownership");
        }
        Ok(()) => File::unlock(&probe).context("release broker daemon ownership probe")?,
    }

    // Clear anything already serving this state directory that the record does
    // not account for, before adding one more.
    let recorded = read_owner_identity(&manager.state_dir).ok();
    let reaped = reap_orphans(&manager.state_dir, recorded.as_ref());
    if reaped > 0 {
        tracing::info!(reaped, "reclaimed unaccounted broker daemons");
    }

    let path = log_path(&manager.state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create broker log directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect broker log directory {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open broker log {}", path.display()))?;
    let stderr = log.try_clone().context("duplicate broker log for stderr")?;

    let executable = std::env::current_exe().context("locate devbox executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__broker")
        .current_dir(&manager.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .process_group(0);
    command.spawn().context("start the credential broker")?;
    Ok(())
}

/// Brokers serving this state directory that no ownership record accounts for.
///
/// The collector's reaper, for the other daemon, and scoped the same way: a
/// `__broker` whose working directory is some other state directory belongs to
/// somebody else's devbox, not to this one's litter.
fn orphans(state_dir: &Path, owner: Option<&OwnerIdentity>) -> Vec<crate::procgroup::Process> {
    let Ok(processes) = crate::procgroup::snapshot() else {
        return Vec::new();
    };
    let mut spare = Vec::new();
    if let Some(owner) = owner {
        spare.push(owner.pid);
        if owner.pgid != 0 {
            spare.extend(
                processes
                    .iter()
                    .filter(|process| process.pgid == owner.pgid)
                    .map(|process| process.pid),
            );
        }
    }
    crate::procgroup::orphans(&processes, "__broker", state_dir, &spare)
}

/// Broker daemons serving this state directory that nothing accounts for, for
/// `devbox doctor`.
pub fn unaccounted(manager: &SandboxManager) -> Vec<String> {
    let owner = read_owner_identity(&manager.state_dir).ok();
    orphans(&manager.state_dir, owner.as_ref())
        .into_iter()
        .map(|process| format!("pid {} — {}", process.pid, process.command))
        .collect()
}

fn reap_orphans(state_dir: &Path, owner: Option<&OwnerIdentity>) -> usize {
    let mut reaped = 0;
    for orphan in orphans(state_dir, owner) {
        match crate::procgroup::stop_group(orphan.pgid, "__broker", ORPHAN_TIMEOUT) {
            Ok(stopped) if stopped.survivors == 0 => {
                tracing::info!(
                    pid = orphan.pid,
                    pgid = orphan.pgid,
                    "stopped an unaccounted broker serving this state directory"
                );
                reaped += 1;
            }
            Ok(stopped) => tracing::warn!(
                pid = orphan.pid,
                survivors = stopped.survivors,
                "an unaccounted broker would not stop"
            ),
            Err(error) => {
                tracing::warn!(pid = orphan.pid, %error, "could not stop an unaccounted broker")
            }
        }
    }
    reaped
}

fn read_owner_identity(state_dir: &Path) -> Result<OwnerIdentity> {
    let path = identity_path(state_dir);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read broker daemon ownership record {}", path.display()))?;
    parse_owner_identity(&text).with_context(|| {
        format!(
            "broker daemon owns {} but published an invalid identity",
            path.display()
        )
    })
}

fn publish_owner_identity(state_dir: &Path, owner: &OwnerIdentity) -> Result<()> {
    let path = identity_path(state_dir);
    let parent = path.parent().context("broker identity has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create broker identity directory {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect broker identity directory {}", parent.display()))?;

    let temporary = parent.join(format!(
        ".broker-daemon.owner-{}-{:016x}.tmp",
        owner.pid,
        rand::random::<u64>()
    ));
    let published = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create broker identity {}", temporary.display()))?;
        writeln!(
            file,
            "pid={} version={} pgid={}",
            owner.pid, owner.version, owner.pgid
        )
        .context("write broker daemon identity")?;
        file.sync_all().context("flush broker daemon identity")?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("publish broker daemon identity {}", path.display()))?;
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
    // Absent for a record written before this field existed, and a malformed
    // one is no group: nothing here may turn an unparseable number into a
    // signal.
    let mut pgid = 0;
    for field in text.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = Some(
                value
                    .parse::<i32>()
                    .context("broker pid is not an integer")?,
            );
        } else if let Some(value) = field.strip_prefix("version=") {
            version = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("pgid=") {
            pgid = value.parse::<i32>().unwrap_or(0);
        }
    }
    let pid = pid.context("broker identity has no pid")?;
    if pid <= 1 || pid == std::process::id() as i32 {
        bail!("broker identity contains unsafe pid {pid}")
    }
    let version = version
        .filter(|value| !value.is_empty())
        .context("broker identity has no version")?;
    // A group id that is not the owner's own pid is not the owner's group.
    if pgid != pid {
        pgid = 0;
    }
    Ok(OwnerIdentity { pid, version, pgid })
}

/// Ask an older binary to release the lock, then prove it did.
///
/// The pid comes from a 0600 record held under the same advisory lock, and is
/// checked against the process's own command line before any signal is sent,
/// so a stale record cannot be turned into a way to kill an unrelated process.
fn replace_outdated_owner(probe: &File, state_dir: &Path, owner: &OwnerIdentity) -> Result<bool> {
    // The whole group where the owner recorded one, for the reason the
    // collector's takeover does it: a daemon's children share its group, and
    // signalling the pid alone leaves them behind.
    if owner.pgid != 0 {
        let stopped = crate::procgroup::stop_group(owner.pgid, "__broker", REPLACEMENT_TIMEOUT)
            .with_context(|| format!("stop the outdated broker group {}", owner.pgid))?;
        if stopped.survivors > 0 {
            bail!(
                "broker group {} still has {} process(es) after SIGTERM and SIGKILL",
                owner.pgid,
                stopped.survivors
            );
        }
        return match probe.try_lock() {
            Ok(()) => {
                File::unlock(probe).context("release broker replacement probe")?;
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => Err(anyhow::Error::new(error))
                .context("claim ownership after stopping the outdated broker"),
        };
    }

    // No group recorded: an owner from a build before this existed.
    let process = Command::new("ps")
        .args(["-ww", "-p", &owner.pid.to_string(), "-o", "command="])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("inspect outdated broker process {}", owner.pid))?;
    let command_line = String::from_utf8_lossy(&process.stdout);
    if !process.status.success() || !command_line.split_whitespace().any(|arg| arg == "__broker") {
        bail!(
            "broker lock names pid {}, but that process is not a devbox __broker; \
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
        .with_context(|| format!("signal outdated broker process {}", owner.pid))?;

    let deadline = Instant::now() + REPLACEMENT_TIMEOUT;
    loop {
        match probe.try_lock() {
            Ok(()) => {
                File::unlock(probe).context("release broker replacement probe")?;
                return Ok(true);
            }
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
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
                    "broker {} (version {}) did not stop within {} seconds{}",
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
                    .context("wait for outdated broker daemon to release ownership");
            }
        }
    }
}

/// Run the broker until the process is terminated.
///
/// Reached only through the hidden `__broker` subcommand.
pub async fn run(manager: Arc<SandboxManager>) -> Result<()> {
    let claim = open_lock(&manager.state_dir)?;
    match claim.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(anyhow::Error::new(error)).context("claim broker daemon ownership");
        }
    }

    let listener = bind().await?;
    let port = listener.local_addr()?.port();

    // A group of our own before anything is published about us, so the id we
    // record can only ever name this daemon and its descendants.
    let me = OwnerIdentity {
        pid: i32::try_from(std::process::id()).context("broker pid exceeds i32")?,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pgid: crate::procgroup::lead_own_group()
            .context("give the broker daemon a process group of its own")?,
    };
    publish_owner_identity(&manager.state_dir, &me)?;
    super::publish_endpoint(
        &manager.state_dir,
        &super::Endpoint {
            port,
            pid: std::process::id() as i32,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )?;

    let state = Arc::new(BrokerState::new(manager.state_dir.clone())?);
    let served = axum::serve(listener, super::server::router(state))
        .with_graceful_shutdown(shutdown_signal());
    let outcome = served.await.context("serve the credential broker");

    // The endpoint record outlives nothing: a stale port is worse than an
    // absent one, because `guest_env` would hand a box an address that no
    // longer answers and the failure would surface inside the agent.
    let _ = std::fs::remove_file(super::endpoint_path(&manager.state_dir));
    File::unlock(&claim).context("release broker daemon ownership")?;
    outcome
}

/// Bind loopback, preferring the well-known port.
///
/// Loopback only, and that is enough: on Lima the guest's traffic to
/// `host.lima.internal` arrives here from `127.0.0.1` (measured), so a wider
/// bind would add reachable surface without adding reachability. Every request
/// carries a box token regardless.
async fn bind() -> Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(("127.0.0.1", super::DEFAULT_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("bind the credential broker to an ephemeral loopback port")
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "bind the credential broker to 127.0.0.1:{}",
                super::DEFAULT_PORT
            )
        }),
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "install broker SIGTERM handler");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "install broker Ctrl-C handler");
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
            File::unlock(&claim).context("release broker daemon status probe")?;
            Ok(None)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let text = std::fs::read_to_string(identity_path(&manager.state_dir))
                .context("read broker daemon ownership record")?;
            if text.trim().is_empty() {
                bail!("broker daemon owns the lock but published no identity")
            }
            Ok(Some(text.trim().to_string()))
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(anyhow::Error::new(error)).context("evaluate broker daemon status")
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
            root.join("locks").join("broker-daemon.lock")
        );
        assert_eq!(
            identity_path(root),
            root.join("locks").join("broker-daemon.owner")
        );
        assert_eq!(log_path(root), root.join("logs").join("broker.log"));
        // Distinct from the collector's, or the two daemons would fight over
        // one lock and only one of them would ever run.
        assert_ne!(
            lock_path(root),
            root.join("locks").join("collector-daemon.lock")
        );
    }

    #[test]
    fn broker_owner_identity_round_trips_and_rejects_unsafe_pids() {
        assert_eq!(
            parse_owner_identity("pid=4242 version=1.2.3 pgid=4242\n").unwrap(),
            OwnerIdentity {
                pid: 4242,
                version: "1.2.3".to_string(),
                pgid: 4242,
            }
        );
        for invalid in ["", "pid=1 version=old", "pid=nope version=old", "pid=42"] {
            assert!(parse_owner_identity(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn broker_owner_identity_is_published_as_a_complete_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let owner = OwnerIdentity {
            pid: 4242,
            version: "1.2.3".to_string(),
            pgid: 4242,
        };
        publish_owner_identity(dir.path(), &owner).unwrap();
        assert_eq!(read_owner_identity(dir.path()).unwrap(), owner);
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

    #[test]
    fn a_host_with_no_secrets_does_not_start_a_broker() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        try_ensure_running(&manager).unwrap();
        assert!(
            !lock_path(dir.path()).exists(),
            "an empty host must not even create the lock"
        );
    }
}
