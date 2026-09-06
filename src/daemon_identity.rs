//! Who owns a devbox daemon lock, and whether this build should take over.
//!
//! Both background daemons — the observability collector and the credential
//! broker — answer the same three questions: who holds the lock, is that
//! process still this build, and what happens when the record cannot be read.
//! They used to answer them in two copies of the same code, and the copies
//! drifted exactly as copies do: the collector learned to compare build
//! digests (W0-5c) and the broker did not, so a pre-fix broker kept running
//! for the life of a login session while every newer binary looked at the
//! version, agreed it was current, and left it alone. The broker is the
//! process that injects credentials, which is not the one to leave stale.
//!
//! So the judgement lives here once, and each daemon supplies only what makes
//! it different: its [`Kind`].

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// What a build publishes when it cannot identify itself.
///
/// Its own executable can be gone — a worktree deleted while its daemon still
/// runs is not hypothetical — and an identity that cannot be computed must
/// read as "unknown", never as "the same as yours".
pub const UNKNOWN_BUILD: &str = "unknown";

/// How often an owning daemon looks for rivals on its own state directory.
///
/// Slow on purpose. Nothing is waiting on the answer, and the usual answer is
/// "none" — the cost that matters is the one a user's command would pay, and a
/// periodic sweep is what moves it off that path entirely.
///
/// Shared, because the two daemons had drifted here before: the collector
/// swept and the broker only looked before starting one, so a rival broker
/// that appeared afterwards ran until the next broker start — which on a host
/// with no secrets is never.
pub const ORPHAN_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Lifecycle commands that must find the record unreadable before the daemon
/// holding the lock is treated as broken.
///
/// One is a race: the record is published just after the lock is taken, so a
/// command arriving in that window reads nothing and would otherwise kill a
/// daemon that was two milliseconds from being healthy. Three consecutive
/// commands cannot all land in that window, and the count is kept against the
/// holder's pid so a different process starts the tally again.
pub const UNREADABLE_STRIKES: u32 = 3;

/// Which daemon a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Collector,
    Broker,
}

impl Kind {
    /// The hidden subcommand, and so the argument that identifies the process.
    ///
    /// Matched as a whole argument by [`crate::procgroup::Process::is`]; a
    /// path that merely contains the word is not a daemon.
    pub fn marker(self) -> &'static str {
        match self {
            Kind::Collector => "__collector",
            Kind::Broker => "__broker",
        }
    }

    /// What to call it in a message a human reads.
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Collector => "collector",
            Kind::Broker => "broker",
        }
    }

    fn stem(self) -> &'static str {
        match self {
            Kind::Collector => "collector-daemon",
            Kind::Broker => "broker-daemon",
        }
    }

    pub fn lock_path(self, state_dir: &Path) -> PathBuf {
        state_dir
            .join("locks")
            .join(format!("{}.lock", self.stem()))
    }

    pub fn identity_path(self, state_dir: &Path) -> PathBuf {
        state_dir
            .join("locks")
            .join(format!("{}.owner", self.stem()))
    }

    /// Where the consecutive-unreadable tally lives.
    fn strikes_path(self, state_dir: &Path) -> PathBuf {
        state_dir
            .join("locks")
            .join(format!("{}.unreadable", self.stem()))
    }
}

/// Who owns the daemon lock.
///
/// The version alone was the whole identity, and that was wrong for the same
/// reason the guest agent's version was (see [`crate::sandbox::agent_sync`]):
/// one version number now covers builds with different capabilities. A
/// pre-eBPF `0.1.6` collector kept spawning guest agents with `-no-ebpf`, and
/// every newer `0.1.6` binary agreed it was current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerIdentity {
    pub pid: i32,
    pub version: String,
    /// The commit this build was made from, as a label for humans. Empty for
    /// an owner from before this field existed.
    pub commit: String,
    /// sha256 of the owner's own executable — the identity that decides.
    ///
    /// The commit cannot: two builds of one commit differ whenever the working
    /// tree, the embedded agent, or a feature flag differ, which is precisely
    /// the case this exists for. Empty for an owner from before this field
    /// existed, which is itself proof it is not this binary.
    pub build: String,
    /// The daemon's own process group, or `0` when it has none recorded.
    ///
    /// A daemon's children inherit this group, and `kill_on_drop` does not run
    /// when the daemon is killed rather than dropped — so signalling the pid
    /// alone stops the daemon and leaves its children. Written only after the
    /// daemon has proved it *leads* the group
    /// ([`crate::procgroup::lead_own_group`]); zero means an older build that
    /// never did, and the takeover falls back to the single pid.
    pub pgid: i32,
}

impl OwnerIdentity {
    /// This process, as an owner.
    ///
    /// `pgid` is left at zero: a process that has not yet made a group of its
    /// own must not claim one, because the group it is in belongs to whoever
    /// started it. `run` fills it in after [`crate::procgroup::lead_own_group`].
    pub fn mine(kind: Kind) -> Result<Self> {
        Ok(Self {
            pid: i32::try_from(std::process::id())
                .with_context(|| format!("{} pid exceeds i32", kind.noun()))?,
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: build_commit().to_string(),
            build: host_build_digest().to_string(),
            pgid: 0,
        })
    }

    /// The build, without the process.
    ///
    /// For the handover note, where the pid would be a lie: the note is
    /// written by the *command* that decided, and the daemon that ends up
    /// running is a child it has not spawned yet. What carries across that gap
    /// is which build is taking over, which is also the thing that decided.
    pub fn describe_build(&self) -> String {
        let mut described = format!("version {}", self.version);
        if !self.commit.is_empty() {
            described.push_str(&format!(" commit {}", self.commit));
        }
        match self.build.as_str() {
            "" => described.push_str(" build unrecorded"),
            UNKNOWN_BUILD => described.push_str(" build unknown"),
            build => described.push_str(&format!(" build {}", short(build))),
        }
        described
    }

    /// A one-line rendering for `devbox doctor`.
    pub fn describe(&self) -> String {
        let mut described = format!("pid {} version {}", self.pid, self.version);
        if !self.commit.is_empty() {
            described.push_str(&format!(" commit {}", self.commit));
        }
        match self.build.as_str() {
            "" => described.push_str(" build unrecorded"),
            UNKNOWN_BUILD => described.push_str(" build unknown"),
            build => described.push_str(&format!(" build {}", short(build))),
        }
        match self.pgid {
            0 => described.push_str(" group unrecorded"),
            pgid => described.push_str(&format!(" group {pgid}")),
        }
        described
    }
}

fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// The commit this binary was built from, or an empty string.
///
/// Stamped by `build.rs`, which reruns when the agent's own inputs change — so
/// it names the commit of the last agent rebuild, not necessarily HEAD. That
/// is why it is a label and [`host_build_digest`] is the identity.
pub fn build_commit() -> &'static str {
    option_env!("DEVBOX_BUILD_COMMIT").unwrap_or("")
}

/// sha256 of this process's own executable, computed once.
///
/// Streamed rather than read whole: this runs on every lifecycle command, and
/// a debug build is tens of megabytes.
pub fn host_build_digest() -> &'static str {
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
/// identity, because within one version it is the only thing that separates
/// two daemons with different capabilities.
///
/// Two asymmetries are deliberate:
///
/// - An owner with no build identity is replaced. This binary always publishes
///   one, so an owner without one cannot be this binary.
/// - A challenger with no build identity replaces nobody of its own version.
///   It cannot show it differs, and a takeover it cannot justify is one that
///   repeats on every command.
pub fn should_replace(owner: &OwnerIdentity, mine: &OwnerIdentity) -> bool {
    if owner.version != mine.version {
        return true;
    }
    if mine.build.is_empty() || mine.build == UNKNOWN_BUILD {
        return false;
    }
    owner.build != mine.build
}

/// Read a daemon's ownership record.
pub fn read(state_dir: &Path, kind: Kind) -> Result<OwnerIdentity> {
    let path = kind.identity_path(state_dir);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {} ownership record {}", kind.noun(), path.display()))?;
    parse(&text, kind).with_context(|| {
        format!(
            "{} daemon owns {} but published an invalid identity",
            kind.noun(),
            path.display()
        )
    })
}

pub fn parse(text: &str, kind: Kind) -> Result<OwnerIdentity> {
    let mut pid = None;
    let mut version = None;
    // Absent, not invalid: a daemon started by a build from before these
    // fields existed publishes neither, and that record must still parse —
    // replacing it is exactly what the missing build identity is evidence for.
    let mut commit = String::new();
    let mut build = String::new();
    let mut pgid = 0;
    for field in text.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = Some(
                value
                    .parse::<i32>()
                    .with_context(|| format!("{} pid is not an integer", kind.noun()))?,
            );
        } else if let Some(value) = field.strip_prefix("version=") {
            version = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("commit=") {
            commit = value.to_string();
        } else if let Some(value) = field.strip_prefix("build=") {
            build = value.to_string();
        } else if let Some(value) = field.strip_prefix("pgid=") {
            // A malformed group is no group. Nothing here may turn an
            // unparseable number into a signal.
            pgid = value.parse::<i32>().unwrap_or(0);
        }
    }
    let pid = pid.with_context(|| format!("{} identity has no pid", kind.noun()))?;
    if pid <= 1 || pid == std::process::id() as i32 {
        bail!("{} identity contains unsafe pid {pid}", kind.noun())
    }
    let version = version
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{} identity has no version", kind.noun()))?;
    // A group id that is not the owner's own pid is not the owner's group. The
    // daemon only ever writes one it leads, so anything else came from a
    // corrupted record — and signalling it would reach a stranger.
    if pgid != pid {
        pgid = 0;
    }
    Ok(OwnerIdentity {
        pid,
        version,
        commit,
        build,
        pgid,
    })
}

/// Publish a daemon's ownership record, replacing any previous one.
pub fn publish(state_dir: &Path, kind: Kind, owner: &OwnerIdentity) -> Result<()> {
    let path = kind.identity_path(state_dir);
    let parent = path
        .parent()
        .with_context(|| format!("{} identity has no parent", kind.noun()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "create {} identity directory {}",
            kind.noun(),
            parent.display()
        )
    })?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
        || {
            format!(
                "protect {} identity directory {}",
                kind.noun(),
                parent.display()
            )
        },
    )?;

    // A unique temporary avoids a crashed predecessor wedging publication.
    // `rename` means a concurrent ensure/status reader observes either the
    // previous complete record or this complete one, never a truncated file.
    let temporary = parent.join(format!(
        ".{}.owner-{}-{:016x}.tmp",
        kind.stem(),
        owner.pid,
        rand::random::<u64>()
    ));
    let published = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create {} identity {}", kind.noun(), temporary.display()))?;
        writeln!(
            file,
            "pid={} version={} commit={} build={} pgid={}",
            owner.pid, owner.version, owner.commit, owner.build, owner.pgid
        )
        .with_context(|| format!("write {} daemon identity", kind.noun()))?;
        file.sync_all()
            .with_context(|| format!("flush {} daemon identity", kind.noun()))?;
        drop(file);
        std::fs::rename(&temporary, &path).with_context(|| {
            format!("publish {} daemon identity {}", kind.noun(), path.display())
        })?;
        Ok(())
    })();
    if published.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    published
}

// ── saying, in the log, that a handover happened ────────────

/// Why one daemon replaced another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Different devbox versions.
    Version { from: String, to: String },
    /// Same version, different executables — the case a version comparison
    /// cannot see, and the one that put a pre-eBPF collector in charge of an
    /// eBPF box for a whole login session.
    Build { from: String, to: String },
    /// The record could not be read for this many consecutive commands.
    Unreadable { strikes: u32 },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Version { from, to } => write!(formatter, "version {from} -> {to}"),
            Reason::Build { from, to } => {
                write!(
                    formatter,
                    "same version, build {} -> {}",
                    short(from),
                    short(to)
                )
            }
            Reason::Unreadable { strikes } => {
                write!(formatter, "identity unreadable for {strikes} commands")
            }
        }
    }
}

/// Why `mine` is replacing `owner`, for the record.
pub fn reason(owner: &OwnerIdentity, mine: &OwnerIdentity) -> Reason {
    if owner.version != mine.version {
        Reason::Version {
            from: owner.version.clone(),
            to: mine.version.clone(),
        }
    } else {
        Reason::Build {
            from: owner.build.clone(),
            to: mine.build.clone(),
        }
    }
}

/// A handover in progress, left where both sides of it can find it.
///
/// Neither side can describe a handover alone. The daemon being stopped knows
/// only that it was signalled; the daemon starting knows only that the lock is
/// free. Whoever decided knows both, and is a third process that writes to the
/// user's terminal rather than to the daemon log — so a handover left the log
/// with an unexplained shutdown followed by an unexplained startup, which is
/// what made W3-7 take two rounds to find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    /// The pid being replaced.
    pub from: i32,
    /// How the replacement describes itself.
    pub to: String,
    /// Rendered, because the log wants a sentence and this file is read by a
    /// process that must not have to reconstruct the decision.
    pub reason: String,
}

fn handover_path(kind: Kind, state_dir: &Path) -> PathBuf {
    state_dir
        .join("locks")
        .join(format!("{}.handover", kind.stem()))
}

/// Leave the note, just before signalling.
pub fn note_handover(state_dir: &Path, kind: Kind, from: i32, mine: &OwnerIdentity, why: &Reason) {
    let path = handover_path(kind, state_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!("from={from}\nto={}\nreason={why}\n", mine.describe_build());
    if let Err(error) = std::fs::write(&path, body) {
        // The handover still happens; only its explanation is lost.
        tracing::debug!(%error, "could not record a {} handover", kind.noun());
        return;
    }
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
}

/// Read the note, if there is one for us.
///
/// `from` is checked against the reader's own pid where the reader is the
/// daemon being stopped: a note left for somebody else is not ours to report.
pub fn read_handover(state_dir: &Path, kind: Kind) -> Option<Handover> {
    let text = std::fs::read_to_string(handover_path(kind, state_dir)).ok()?;
    let mut from = None;
    let mut to = None;
    let mut why = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("from=") {
            from = value.trim().parse::<i32>().ok();
        } else if let Some(value) = line.strip_prefix("to=") {
            to = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("reason=") {
            why = Some(value.trim().to_string());
        }
    }
    Some(Handover {
        from: from?,
        to: to?,
        reason: why.unwrap_or_else(|| "unrecorded".to_string()),
    })
}

pub fn clear_handover(state_dir: &Path, kind: Kind) {
    let path = handover_path(kind, state_dir);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

/// The line a daemon writes when it is stopped, so the gap in the log has a
/// cause next to it.
pub fn log_being_replaced(state_dir: &Path, kind: Kind, me: i32) {
    match read_handover(state_dir, kind) {
        Some(note) if note.from == me => tracing::info!(
            pid = me,
            successor = %note.to,
            reason = %note.reason,
            "stopping: this {} is being replaced",
            kind.noun()
        ),
        // Signalled by something that left no note — a person, a reboot, a
        // supervisor. Worth saying so, because the alternative reading of a
        // silent exit is that the daemon crashed.
        _ => tracing::info!(
            pid = me,
            "stopping: this {} was signalled, with no handover recorded",
            kind.noun()
        ),
    }
}

/// The matching line from the other side, written once the successor is up.
pub fn log_replacing(state_dir: &Path, kind: Kind, me: &OwnerIdentity) {
    if let Some(note) = read_handover(state_dir, kind) {
        tracing::info!(
            pid = me.pid,
            replaced = note.from,
            reason = %note.reason,
            "this {} took over",
            kind.noun()
        );
        // One handover, one note. Leaving it would make the next unrelated
        // start claim a takeover that did not happen.
        clear_handover(state_dir, kind);
    }
}

// ── the lock is held but the record cannot be read ──────────

/// What a devbox process found when it asked who owns a daemon lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Nobody holds it.
    Free,
    /// Held, and the holder said who it is.
    Held(OwnerIdentity),
    /// Held, and the record could not be read or parsed.
    ///
    /// The dangerous state, because it is the one where a devbox process knows
    /// something is there and knows nothing about it. Both possible answers
    /// are bad if taken alone: refusing forever lets a daemon become immortal,
    /// and signalling on sight makes a damaged file into a reason to kill a
    /// healthy process.
    Unreadable(Unreadable),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// Why the record could not be read, for a human.
    pub detail: String,
    /// The process holding the lock, when it could be identified at all.
    pub holder: Option<Holder>,
    /// Consecutive lifecycle commands that have found it unreadable, including
    /// this one. Zero when nothing has counted yet.
    pub strikes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: i32,
    pub pgid: i32,
    pub command: String,
    /// Whether its command line names this daemon.
    ///
    /// The gate on every signal. A lock held by something that is not a devbox
    /// daemon is never signalled, however long it holds it: devbox does not
    /// know what it is, and a lock file is not a licence.
    pub is_daemon: bool,
}

/// Find the process holding a daemon lock, without trusting the record.
///
/// Two ways, because neither is reliable alone. `lsof` on the lock file names
/// the holder exactly but is absent on minimal Linux images and can be
/// refused; the working-directory scan needs no extra tool but only finds a
/// daemon devbox itself started, since that is what sets the state directory
/// as its cwd. Whichever answers, the result is filtered the same way.
pub fn find_holder(state_dir: &Path, kind: Kind) -> Option<Holder> {
    let processes = crate::procgroup::snapshot().ok()?;
    let me = std::process::id() as i32;

    // Every process with the lock file open, minus this one — `open_lock` has
    // it open right now in order to have discovered it was held.
    let by_lock: Vec<i32> = crate::procgroup::holders_of(&kind.lock_path(state_dir))
        .into_iter()
        .filter(|pid| *pid != me)
        .collect();

    if by_lock.is_empty() {
        // Nothing named the holder, so fall back to "a daemon of this kind
        // whose working directory is this state directory". That is the same
        // evidence the orphan reaper uses, and sound for the same reason: a
        // daemon is started with its state directory as its cwd.
        return crate::procgroup::orphans(&processes, kind.marker(), state_dir, &[me])
            .first()
            .map(|process| holder_from(process, kind));
    }
    let candidates: Vec<&crate::procgroup::Process> = processes
        .iter()
        .filter(|process| by_lock.contains(&process.pid) && !process.is_zombie())
        .collect();

    // Prefer a candidate that is a daemon of this kind. Where none is, the
    // first holder is still reported, because "held by something that is not a
    // devbox daemon" is exactly what the caller needs to be told.
    candidates
        .iter()
        .find(|process| process.is(kind.marker()))
        .or(candidates.first())
        .map(|process| holder_from(process, kind))
}

fn holder_from(process: &crate::procgroup::Process, kind: Kind) -> Holder {
    Holder {
        pid: process.pid,
        pgid: process.pgid,
        command: process.command.clone(),
        is_daemon: process.is(kind.marker()),
    }
}

/// Who owns this daemon's lock right now.
///
/// Read-only: it counts nothing and signals nothing, so `devbox doctor` can
/// ask without changing what the next lifecycle command will decide.
pub fn ownership(state_dir: &Path, kind: Kind) -> Result<Ownership> {
    let probe = open_lock(state_dir, kind)?;
    match probe.try_lock() {
        Ok(()) => {
            File::unlock(&probe)
                .with_context(|| format!("release {} ownership probe", kind.noun()))?;
            Ok(Ownership::Free)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(match read(state_dir, kind) {
            Ok(owner) => Ownership::Held(owner),
            Err(error) => Ownership::Unreadable(Unreadable {
                detail: format!("{error:#}"),
                holder: find_holder(state_dir, kind),
                strikes: read_strikes(state_dir, kind).map_or(0, |(_, strikes)| strikes),
            }),
        }),
        Err(std::fs::TryLockError::Error(error)) => Err(anyhow::Error::new(error))
            .with_context(|| format!("evaluate {} daemon ownership", kind.noun())),
    }
}

/// Open the lock file, creating the directory that holds it.
pub fn open_lock(state_dir: &Path, kind: Kind) -> Result<File> {
    let path = kind.lock_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create {} lock directory {}", kind.noun(), parent.display())
        })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "protect {} lock directory {}",
                    kind.noun(),
                    parent.display()
                )
            },
        )?;
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
        .with_context(|| format!("open {} daemon lock {}", kind.noun(), path.display()))
}

/// Count one lifecycle command that found the record unreadable.
///
/// Kept against the holder's pid: a different process is a different problem
/// and starts the tally again, so a run of unrelated failures cannot add up
/// into a licence to signal.
pub fn record_unreadable(state_dir: &Path, kind: Kind, holder: Option<i32>) -> u32 {
    let previous = read_strikes(state_dir, kind);
    let strikes = match (&previous, holder) {
        (Some((recorded, strikes)), Some(holder)) if *recorded == holder => strikes + 1,
        // No holder identified, or a different one: this is the first strike
        // against whatever is there now.
        _ => 1,
    };
    let path = kind.strikes_path(state_dir);
    let body = format!("pid={} strikes={strikes}\n", holder.unwrap_or(0));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, body) {
        // Not fatal, and deliberately so: failing to write the tally means the
        // daemon is never treated as broken, which is the safe direction.
        tracing::debug!(%error, "could not record an unreadable {} identity", kind.noun());
        return 1;
    }
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    strikes
}

/// Forget the tally. Called whenever the record reads cleanly again.
pub fn clear_unreadable(state_dir: &Path, kind: Kind) {
    let path = kind.strikes_path(state_dir);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

fn read_strikes(state_dir: &Path, kind: Kind) -> Option<(i32, u32)> {
    let text = std::fs::read_to_string(kind.strikes_path(state_dir)).ok()?;
    let mut pid = None;
    let mut strikes = None;
    for field in text.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse::<i32>().ok();
        } else if let Some(value) = field.strip_prefix("strikes=") {
            strikes = value.parse::<u32>().ok();
        }
    }
    Some((pid?, strikes?))
}

/// What a lifecycle command should do about a lock whose record it cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Broken {
    /// Leave it alone and say nothing more; it may be healthy and mid-publish.
    Wait { strikes: u32 },
    /// Leave it alone permanently. The lock is held by something devbox did
    /// not start, and a lock file is not a licence to signal a stranger.
    NotOurs { pid: i32, command: String },
    /// Nothing identifiable holds it. Nothing to signal, and nothing to blame.
    Unidentified,
    /// Treat the record as damaged and take the daemon over by its group.
    Replace { pid: i32, pgid: i32, strikes: u32 },
}

/// Decide what to do, having found the record unreadable.
///
/// Pure, given the holder and the tally, so the rule can be read and tested
/// without a lock, a process table, or a filesystem.
pub fn decide_broken(holder: Option<&Holder>, strikes: u32) -> Broken {
    let Some(holder) = holder else {
        return Broken::Unidentified;
    };
    if !holder.is_daemon {
        return Broken::NotOurs {
            pid: holder.pid,
            command: holder.command.clone(),
        };
    }
    // A daemon that never made a group of its own cannot be stopped by group,
    // and this path has no record to fall back on — signalling the bare pid
    // here would leave its children behind, which is the defect W2-9 fixed.
    if holder.pgid != holder.pid {
        return Broken::Wait { strikes };
    }
    if strikes < UNREADABLE_STRIKES {
        return Broken::Wait { strikes };
    }
    Broken::Replace {
        pid: holder.pid,
        pgid: holder.pgid,
        strikes,
    }
}

/// Act on a lock whose ownership record cannot be read.
///
/// Returns `true` when the lock is now free and the caller should start a
/// daemon of its own; `false` means "leave it, and try again next command".
///
/// The two failure modes this navigates between are both real. Refusing
/// forever lets a daemon whose record was damaged — a crash between taking the
/// lock and publishing, a truncated file, a full disk — run until the host is
/// rebooted, immune to every takeover because nothing can read what it is.
/// Signalling on sight makes a damaged file into a reason to kill a healthy
/// process, and the record is unreadable for a few milliseconds *every time a
/// daemon starts*, between the lock and the publish.
///
/// So: only a process that is provably a devbox daemon of this kind, only
/// after [`UNREADABLE_STRIKES`] consecutive commands have agreed, and only by
/// its own process group.
pub fn take_over_broken(
    probe: &File,
    state_dir: &Path,
    kind: Kind,
    reason: &anyhow::Error,
    patience: std::time::Duration,
) -> Result<bool> {
    let holder = find_holder(state_dir, kind);
    // Only a daemon's silence is counted. A stranger holding the lock can
    // never reach a verdict however long it holds it, so tallying it would be
    // a write on every command that decides nothing — and would make the file
    // mean something other than what its name says.
    let strikes = match &holder {
        Some(held) if held.is_daemon => record_unreadable(state_dir, kind, Some(held.pid)),
        _ => read_strikes(state_dir, kind).map_or(0, |(_, strikes)| strikes),
    };
    match decide_broken(holder.as_ref(), strikes) {
        Broken::Wait { strikes } => {
            tracing::warn!(
                strikes,
                needed = UNREADABLE_STRIKES,
                reason = %format!("{reason:#}"),
                "the {} daemon holds its lock but its identity cannot be read",
                kind.noun()
            );
            Ok(false)
        }
        Broken::NotOurs { pid, command } => {
            // Never signalled, however long it holds the lock. devbox does not
            // know what this is, and a lock file is not a licence.
            tracing::warn!(
                pid,
                command = %command,
                "the {} lock is held by a process that is not a devbox daemon; \
                 it will not be signalled",
                kind.noun()
            );
            Ok(false)
        }
        Broken::Unidentified => {
            tracing::warn!(
                reason = %format!("{reason:#}"),
                "the {} lock is held, its identity cannot be read, and the holder \
                 could not be identified",
                kind.noun()
            );
            Ok(false)
        }
        Broken::Replace { pid, pgid, strikes } => {
            tracing::warn!(
                pid,
                pgid,
                strikes,
                "replacing a {} daemon whose identity has been unreadable for \
                 {UNREADABLE_STRIKES} commands",
                kind.noun()
            );
            note_handover(
                state_dir,
                kind,
                pid,
                &OwnerIdentity::mine(kind)?,
                &Reason::Unreadable { strikes },
            );
            let stopped = crate::procgroup::stop_group(pgid, kind.marker(), patience)
                .with_context(|| {
                    format!(
                        "stop the {} daemon with an unreadable identity",
                        kind.noun()
                    )
                })?;
            if stopped.survivors > 0 {
                bail!(
                    "{} group {pgid} still has {} process(es) after SIGTERM and SIGKILL",
                    kind.noun(),
                    stopped.survivors
                );
            }
            clear_unreadable(state_dir, kind);
            match probe.try_lock() {
                Ok(()) => {
                    File::unlock(probe)
                        .with_context(|| format!("release {} replacement probe", kind.noun()))?;
                    Ok(true)
                }
                // Somebody else won the free lock in between. Theirs to serve.
                Err(std::fs::TryLockError::WouldBlock) => Ok(false),
                Err(error) => Err(anyhow::Error::new(error)).with_context(|| {
                    format!("claim ownership after stopping the broken {}", kind.noun())
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(version: &str, build: &str) -> OwnerIdentity {
        OwnerIdentity {
            pid: 4242,
            version: version.to_string(),
            commit: String::new(),
            build: build.to_string(),
            pgid: 4242,
        }
    }

    fn daemon_holder(pid: i32, pgid: i32) -> Holder {
        Holder {
            pid,
            pgid,
            command: "/x/devbox __broker".to_string(),
            is_daemon: true,
        }
    }

    /// The case this module exists for, and the one the broker was missing:
    /// one version number, two builds.
    #[test]
    fn the_same_version_built_differently_is_taken_over() {
        assert!(should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", "bbbb")
        ));
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

    #[test]
    fn a_challenger_that_cannot_identify_itself_replaces_nobody_of_its_version() {
        assert!(!should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", UNKNOWN_BUILD)
        ));
        assert!(!should_replace(
            &owner("1.2.3", "aaaa"),
            &owner("1.2.3", "")
        ));
        assert!(should_replace(
            &owner("1.2.2", "aaaa"),
            &owner("1.2.3", UNKNOWN_BUILD)
        ));
    }

    #[test]
    fn an_identity_from_before_the_build_field_still_parses_and_is_replaced() {
        for kind in [Kind::Collector, Kind::Broker] {
            let old = parse("pid=4242 version=1.2.3\n", kind).unwrap();
            assert_eq!(old.build, "");
            assert_eq!(old.pgid, 0);
            assert!(should_replace(&old, &owner("1.2.3", "beef")));
        }
    }

    #[test]
    fn the_record_round_trips_for_both_daemons() {
        for kind in [Kind::Collector, Kind::Broker] {
            let dir = tempfile::tempdir().unwrap();
            let recorded = OwnerIdentity {
                pid: 4242,
                version: "1.2.3".into(),
                commit: "abc123def456".into(),
                build: "c7d70a08".into(),
                pgid: 4242,
            };
            publish(dir.path(), kind, &recorded).unwrap();
            assert_eq!(read(dir.path(), kind).unwrap(), recorded);
            // Published through a rename, so no half-written temporary is left
            // for the next reader to find.
            assert!(
                std::fs::read_dir(dir.path().join("locks"))
                    .unwrap()
                    .all(|entry| !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp"))
            );
            // And the two daemons never share a file.
            assert_ne!(
                kind.identity_path(dir.path()),
                match kind {
                    Kind::Collector => Kind::Broker,
                    Kind::Broker => Kind::Collector,
                }
                .identity_path(dir.path())
            );
        }
    }

    #[test]
    fn each_daemon_keeps_its_own_lock_record_and_tally() {
        let dir = Path::new("/state");
        assert_ne!(Kind::Collector.lock_path(dir), Kind::Broker.lock_path(dir));
        assert_ne!(
            Kind::Collector.strikes_path(dir),
            Kind::Broker.strikes_path(dir)
        );
        assert_eq!(
            Kind::Collector.lock_path(dir),
            dir.join("locks").join("collector-daemon.lock")
        );
        assert_eq!(
            Kind::Broker.identity_path(dir),
            dir.join("locks").join("broker-daemon.owner")
        );
    }

    #[test]
    fn an_unsafe_pid_is_refused_for_both_daemons() {
        for kind in [Kind::Collector, Kind::Broker] {
            for invalid in ["", "pid=1 version=old", "pid=nope version=old", "pid=42"] {
                assert!(parse(invalid, kind).is_err(), "{invalid:?} {kind:?}");
            }
        }
    }

    #[test]
    fn the_doctor_line_names_the_build_and_the_group_without_printing_all_of_it() {
        let described = OwnerIdentity {
            pid: 7,
            version: "0.2.0".into(),
            commit: "11cc51fbf1d3".into(),
            build: "c7d70a0857d42e7fbf0062fec377477658ff76cc8412b3210e60023a1736cca0".into(),
            pgid: 7,
        }
        .describe();
        assert_eq!(
            described,
            "pid 7 version 0.2.0 commit 11cc51fbf1d3 build c7d70a0857d4 group 7"
        );
        assert!(owner("0.2.0", "").describe().contains("build unrecorded"));
        assert!(
            owner("0.2.0", UNKNOWN_BUILD)
                .describe()
                .contains("build unknown")
        );
        let mut groupless = owner("0.2.0", "aaaa");
        groupless.pgid = 0;
        assert!(groupless.describe().ends_with("group unrecorded"));
    }

    /// This binary can always identify itself, so nothing it publishes reads
    /// as an owner worth replacing.
    #[test]
    fn this_build_identifies_itself() {
        for kind in [Kind::Collector, Kind::Broker] {
            let mine = OwnerIdentity::mine(kind).unwrap();
            assert_ne!(mine.build, "", "a live test binary has an executable");
            assert_ne!(mine.build, UNKNOWN_BUILD);
            assert!(!should_replace(&mine, &mine));
        }
    }

    /// The two daemons sweep on the same schedule. They had drifted here
    /// before — the collector swept, the broker only looked before starting
    /// one — and a rival broker on a host whose secrets were later removed
    /// therefore ran until the machine was rebooted.
    #[test]
    fn both_daemons_sweep_on_one_schedule() {
        assert_eq!(ORPHAN_SWEEP_INTERVAL, std::time::Duration::from_secs(300));
        let collector = std::fs::read_to_string("src/obs/daemon.rs").expect("collector source");
        let broker = std::fs::read_to_string("src/broker/daemon.rs").expect("broker source");
        for (name, source) in [("collector", &collector), ("broker", &broker)] {
            assert!(
                source.contains("identity::ORPHAN_SWEEP_INTERVAL"),
                "the {name} does not sweep on the shared schedule"
            );
            assert!(
                source.contains("reap_orphans"),
                "the {name} does not reap orphans at all"
            );
        }
    }

    // ── saying that a handover happened ─────────────────

    #[test]
    fn a_handover_note_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Collector;
        assert_eq!(read_handover(dir.path(), kind), None);

        let mine = owner("0.2.0", "bbbb");
        note_handover(
            dir.path(),
            kind,
            500,
            &mine,
            &Reason::Build {
                from: "aaaa".into(),
                to: "bbbb".into(),
            },
        );
        let note = read_handover(dir.path(), kind).expect("a note");
        assert_eq!(note.from, 500);
        // The build, and not a pid: the note is written by the command that
        // decided, whose pid is not the daemon that will be running.
        assert_eq!(note.to, "version 0.2.0 build bbbb");
        assert!(!note.to.contains("pid"), "{note:?}");
        assert_eq!(note.reason, "same version, build aaaa -> bbbb");

        clear_handover(dir.path(), kind);
        assert_eq!(read_handover(dir.path(), kind), None);
        // The two daemons never read each other's.
        note_handover(
            dir.path(),
            Kind::Broker,
            7,
            &mine,
            &Reason::Unreadable { strikes: 3 },
        );
        assert_eq!(read_handover(dir.path(), Kind::Collector), None);
        assert_eq!(
            read_handover(dir.path(), Kind::Broker).unwrap().reason,
            "identity unreadable for 3 commands"
        );
    }

    /// The reason has to name the thing that decided. A version change and a
    /// build change look identical in a log that only says "replaced".
    #[test]
    fn the_reason_names_what_actually_differed() {
        let old_version = owner("0.1.6", "aaaa");
        let new_version = owner("0.2.0", "aaaa");
        assert_eq!(
            reason(&old_version, &new_version).to_string(),
            "version 0.1.6 -> 0.2.0"
        );
        let long = "c7d70a0857d42e7fbf0062fec377477658ff76cc8412b3210e60023a1736cca0";
        let same_version = reason(&owner("0.2.0", "aaaa"), &owner("0.2.0", long));
        assert_eq!(
            same_version.to_string(),
            "same version, build aaaa -> c7d70a0857d4"
        );
        assert_eq!(
            Reason::Unreadable { strikes: 3 }.to_string(),
            "identity unreadable for 3 commands"
        );
    }

    /// A note left for another pid is not this daemon's to report: it would
    /// otherwise claim, on the way out, a handover it was not part of.
    #[test]
    fn a_note_addressed_to_somebody_else_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Collector;
        note_handover(
            dir.path(),
            kind,
            500,
            &owner("0.2.0", "bbbb"),
            &Reason::Unreadable { strikes: 3 },
        );
        let note = read_handover(dir.path(), kind).unwrap();
        assert_ne!(note.from, 999);
        // `log_being_replaced` compares the two; both branches are reachable
        // and neither panics.
        log_being_replaced(dir.path(), kind, 500);
        log_being_replaced(dir.path(), kind, 999);
    }

    #[test]
    fn a_truncated_note_is_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Broker;
        let path = handover_path(kind, dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "to=somebody\n").unwrap();
        assert_eq!(read_handover(dir.path(), kind), None);
        std::fs::write(&path, "from=5\n").unwrap();
        assert_eq!(read_handover(dir.path(), kind), None);
        // A note with no reason still names both sides; the reason is extra.
        std::fs::write(&path, "from=5\nto=x\n").unwrap();
        assert_eq!(
            read_handover(dir.path(), kind).unwrap().reason,
            "unrecorded"
        );
    }

    // ── the unreadable-record rule ──────────────────────

    /// One unreadable read is a race, not a diagnosis: the record is published
    /// just after the lock is taken, and a command landing in that window
    /// would otherwise kill a daemon that was about to be healthy.
    #[test]
    fn a_daemon_is_not_broken_until_three_commands_agree() {
        let holder = daemon_holder(500, 500);
        assert_eq!(decide_broken(Some(&holder), 1), Broken::Wait { strikes: 1 });
        assert_eq!(decide_broken(Some(&holder), 2), Broken::Wait { strikes: 2 });
        assert_eq!(
            decide_broken(Some(&holder), 3),
            Broken::Replace {
                pid: 500,
                pgid: 500,
                strikes: 3
            }
        );
    }

    /// The rule that keeps a damaged file from becoming a reason to kill a
    /// stranger. However long it holds the lock, it is never signalled.
    #[test]
    fn a_lock_held_by_something_that_is_not_a_daemon_is_never_signalled() {
        let stranger = Holder {
            pid: 900,
            pgid: 900,
            command: "/usr/bin/some-other-program".to_string(),
            is_daemon: false,
        };
        for strikes in [1, 3, 100] {
            assert_eq!(
                decide_broken(Some(&stranger), strikes),
                Broken::NotOurs {
                    pid: 900,
                    command: "/usr/bin/some-other-program".to_string()
                }
            );
        }
    }

    /// A stranger never accumulates a tally, because no tally could ever
    /// license signalling it. The file keeps meaning what its name says.
    #[test]
    fn a_stranger_never_accumulates_a_tally() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Broker;
        let stranger = Holder {
            pid: 900,
            pgid: 900,
            command: "/usr/bin/python3 -c ...".to_string(),
            is_daemon: false,
        };
        // The decision for a stranger does not depend on the count, so the
        // count is never taken.
        for strikes in [0, 1, 7] {
            assert!(matches!(
                decide_broken(Some(&stranger), strikes),
                Broken::NotOurs { .. }
            ));
        }
        assert!(!kind.strikes_path(dir.path()).exists());
    }

    #[test]
    fn a_holder_that_cannot_be_identified_is_left_alone() {
        assert_eq!(decide_broken(None, 100), Broken::Unidentified);
    }

    /// A daemon that never made a group of its own cannot be stopped by group,
    /// and there is no record here to fall back on. Signalling its bare pid
    /// would leave its children — the defect W2-9 fixed — so it waits instead.
    #[test]
    fn a_daemon_that_leads_no_group_is_not_replaced_on_this_path() {
        let groupless = daemon_holder(500, 400);
        assert_eq!(
            decide_broken(Some(&groupless), 100),
            Broken::Wait { strikes: 100 }
        );
    }

    #[test]
    fn the_tally_counts_up_and_a_different_holder_starts_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Collector;
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 1);
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 2);
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 3);
        // Another process is another problem.
        assert_eq!(record_unreadable(dir.path(), kind, Some(700)), 1);
        // And an unidentifiable holder never accumulates.
        assert_eq!(record_unreadable(dir.path(), kind, None), 1);
        assert_eq!(record_unreadable(dir.path(), kind, None), 1);
    }

    #[test]
    fn a_readable_record_forgets_the_tally() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Broker;
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 1);
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 2);
        clear_unreadable(dir.path(), kind);
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 1);
        // Clearing one daemon's tally does not touch the other's.
        assert_eq!(record_unreadable(dir.path(), Kind::Collector, Some(500)), 1);
        clear_unreadable(dir.path(), Kind::Collector);
        assert_eq!(record_unreadable(dir.path(), kind, Some(500)), 2);
    }

    /// Holding the lock open, with a chosen argv, so the reverse lookup can be
    /// tested against a real process rather than a fixture.
    ///
    /// `$0` carries the marker, which is where `ps` shows it for a real
    /// daemon, and `exec 9<` keeps the lock file open for as long as the
    /// process lives.
    fn hold_the_lock(state_dir: &Path, kind: Kind, argv0: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;

        let lock = kind.lock_path(state_dir);
        // Create it first: the holder only opens it for reading.
        drop(open_lock(state_dir, kind).expect("create the lock"));
        std::process::Command::new("sh")
            .args([
                "-c",
                "exec 9< \"$1\"; sleep 30",
                argv0,
                lock.to_str().expect("lock path is UTF-8"),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("hold the lock")
    }

    fn wait_for_holder(state_dir: &Path, kind: Kind, pid: i32) -> Option<Holder> {
        for _ in 0..100 {
            if let Some(found) = find_holder(state_dir, kind)
                && found.pid == pid
            {
                return Some(found);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        find_holder(state_dir, kind)
    }

    /// The reverse lookup, against a real open file descriptor: this is what
    /// stands in for the ownership record when the record cannot be read.
    #[test]
    fn the_lock_holder_is_found_and_identified_without_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Collector;
        let mut holder = hold_the_lock(dir.path(), kind, "__collector");
        let pid = holder.id() as i32;

        let found = wait_for_holder(dir.path(), kind, pid).expect("the holder was not found");
        assert_eq!(found.pid, pid);
        assert!(found.is_daemon, "{found:?}");
        assert_eq!(found.pgid, pid, "it was spawned as its own group leader");
        // And with no record on disk, that is enough to reach a verdict.
        assert_eq!(
            decide_broken(Some(&found), UNREADABLE_STRIKES),
            Broken::Replace {
                pid,
                pgid: pid,
                strikes: UNREADABLE_STRIKES
            }
        );

        let _ = crate::procgroup::signal_group(pid, libc::SIGKILL);
        let _ = holder.wait();
    }

    /// The same lookup, and the opposite verdict: a stranger holding the lock
    /// is named and never signalled.
    #[test]
    fn a_stranger_holding_the_lock_is_named_and_spared() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Broker;
        let mut holder = hold_the_lock(dir.path(), kind, "__not_a_devbox_daemon");
        let pid = holder.id() as i32;

        let found = wait_for_holder(dir.path(), kind, pid).expect("the holder was not found");
        assert_eq!(found.pid, pid);
        assert!(!found.is_daemon, "{found:?}");
        assert_eq!(
            decide_broken(Some(&found), 100),
            Broken::NotOurs {
                pid,
                command: found.command.clone()
            }
        );
        // Still running: a verdict of "not ours" is a verdict never to signal.
        assert!(
            crate::procgroup::snapshot()
                .unwrap()
                .iter()
                .any(|process| process.pid == pid && !process.is_zombie())
        );

        let _ = crate::procgroup::signal_group(pid, libc::SIGKILL);
        let _ = holder.wait();
    }

    /// A held lock with no record reads as `Unreadable`, not as `Free` — the
    /// distinction the whole rule rests on.
    #[test]
    fn a_held_lock_with_no_record_is_unreadable_not_free() {
        let dir = tempfile::tempdir().unwrap();
        let kind = Kind::Collector;
        let mut holder = hold_the_lock(dir.path(), kind, "__collector");
        let pid = holder.id() as i32;
        wait_for_holder(dir.path(), kind, pid);

        // The holder has it open for reading only, so take the advisory lock
        // from a second descriptor to produce the state the rule is about: the
        // lock is held and no record has been published. `flock` is per open
        // file description, so a second descriptor in this process contends
        // with this one exactly as another process would.
        let held = open_lock(dir.path(), kind).unwrap();
        held.lock().expect("take the advisory lock");

        let found = ownership(dir.path(), kind).expect("read ownership");
        let Ownership::Unreadable(unreadable) = found else {
            panic!("a held lock with no record read as {found:?}, not Unreadable");
        };
        assert_eq!(
            unreadable.holder.as_ref().map(|held| held.pid),
            Some(pid),
            "{unreadable:?}"
        );
        assert!(unreadable.holder.is_some_and(|held| held.is_daemon));
        // Read-only: `ownership` counts nothing, so `doctor` cannot change
        // what the next lifecycle command will decide.
        assert_eq!(unreadable.strikes, 0);
        assert!(!kind.strikes_path(dir.path()).exists());

        let _ = File::unlock(&held);
        let _ = crate::procgroup::signal_group(pid, libc::SIGKILL);
        let _ = holder.wait();
    }

    #[test]
    fn an_empty_state_directory_has_a_free_lock() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [Kind::Collector, Kind::Broker] {
            assert_eq!(ownership(dir.path(), kind).unwrap(), Ownership::Free);
        }
    }
}
