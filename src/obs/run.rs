//! Runs — a bounded execution inside a box, with a stable identity (§4).
//!
//! `devbox exec` answers "what did this command print?". A *run* answers "what
//! did this command **do**?" — which files it changed, which hosts it reached,
//! which processes it spawned, and how much of that the capture layer could
//! actually see. The identity is what makes the answer citable later: a report
//! is not a rendering of "the box since I last looked", it is a rendering of
//! one execution that happened at one time.
//!
//! Three things live here:
//!
//! - [`RunId`] — 26 characters, time-sortable, generated on the host.
//! - [`RunRecord`] — the row, mirrored in the `runs` table (`super::store`).
//! - [`Attributor`] — the rule that decides which run an event belongs to, and
//!   which of the three ways it was decided ([`Attribution`]).
//!
//! The guest side is [`wrapper_script`] and [`bootstrap`]: `devbox run` does
//! not exec the command directly, it execs a wrapper that puts the command in
//! its own cgroup and publishes that cgroup's id back to the host.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::event::Event;

/// The pid the agent reports for an observation with no owning process.
///
/// Packet- and netfilter-derived events carry it (`agent/capture/proc.go`'s
/// `UnattributedPID`). It is not a pid, it is the absence of one, and the one
/// rule that keeps it from poisoning attribution is that such an event can
/// *only* ever be attributed by time window — never by cgroup, never by a
/// parent chain, both of which would be reading a sentinel as data.
pub const UNATTRIBUTED_PID: u32 = u32::MAX;

/// What kind of execution a run records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    /// `devbox run` — the only kind that renders a report by default.
    Run,
    /// `devbox exec` — recorded so the Runs tab is complete.
    Exec,
    /// `devbox shell` — likewise.
    Shell,
    /// A sandboxed MCP server (component D, wave 2).
    Mcp,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Run => "run",
            RunKind::Exec => "exec",
            RunKind::Shell => "shell",
            RunKind::Mcp => "mcp",
        }
    }

    /// Whether this kind renders a report unless told not to.
    ///
    /// Open question 2 in the design, decided there: `exec` and `shell` are
    /// recorded, not rendered. A shell session that ends after four hours
    /// should not write three files nobody asked for.
    pub fn renders_report(self) -> bool {
        matches!(self, RunKind::Run | RunKind::Mcp)
    }
}

impl fmt::Display for RunKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "run" => RunKind::Run,
            "exec" => RunKind::Exec,
            "shell" => RunKind::Shell,
            "mcp" => RunKind::Mcp,
            other => bail!("unknown run kind '{other}'"),
        })
    }
}

/// Where a run is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    /// Started, no end recorded. The collector attributes to these.
    Running,
    /// The command exited and the host saw the exit code.
    Finished,
    /// The host lost track of it — devbox was killed, the box went away.
    Aborted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Finished => "finished",
            RunStatus::Aborted => "aborted",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => RunStatus::Running,
            "finished" => RunStatus::Finished,
            "aborted" => RunStatus::Aborted,
            other => bail!("unknown run status '{other}'"),
        })
    }
}

/// How an event was tied to a run — the provenance of the association, stored
/// beside it so a report can say how much of itself it is sure about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attribution {
    /// The event's cgroup id equals the run's. Exact; the kernel decided it.
    Cgroup,
    /// The event's process descends from the run's root pid. Exact as far as
    /// the parent chain was observed, which is only as good as the exec stream.
    Pidtree,
    /// The event has no process and one run's window contains its timestamp.
    /// A guess — a defensible one, and labelled so nobody mistakes it.
    Window,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Attribution::Cgroup => "cgroup",
            Attribution::Pidtree => "pidtree",
            Attribution::Window => "window",
        }
    }
}

impl fmt::Display for Attribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Attribution {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "cgroup" => Attribution::Cgroup,
            "pidtree" => Attribution::Pidtree,
            "window" => Attribution::Window,
            other => bail!("unknown attribution '{other}'"),
        })
    }
}

/// One run, as stored.
///
/// `Default` exists so a caller can fill the half it knows; the store never
/// writes a record without `run_id`, `box_id` and `started_at`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub box_id: String,
    pub kind: String,
    /// The command as the user wrote it, not as the wrapper execs it.
    pub argv: Vec<String>,
    /// Working directory *inside the guest*.
    pub cwd: String,
    pub label: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_code: Option<i32>,
    pub status: String,
    /// The box's posture before the run, and the one in force during it. Equal
    /// unless `--posture` switched it.
    pub posture_before: String,
    pub posture_during: String,
    /// The guest cgroup the command ran in. Zero when the guest could not give
    /// the run an exclusive one — see [`RunScope`].
    pub cgroup_id: u64,
    pub root_pid: u32,
    /// Component E, wired at integration.
    pub checkpoint_start: Option<String>,
    pub checkpoint_end: Option<String>,
    /// Collector health at the end: what was capturing, which agent, what it
    /// lost. Deltas, not totals — see `report::model::Coverage`.
    pub capture_sources: String,
    pub agent_version: String,
    pub dropped_events: u64,
    /// Why the run stopped, when the host knows something the exit code does
    /// not say. See [`EndedBy`].
    pub ended_by: Option<String>,
}

impl RunRecord {
    pub fn kind_enum(&self) -> RunKind {
        self.kind.parse().unwrap_or(RunKind::Run)
    }

    pub fn status_enum(&self) -> RunStatus {
        self.status.parse().unwrap_or(RunStatus::Running)
    }

    /// Duration in milliseconds, when both ends are known and parse.
    pub fn duration_ms(&self) -> Option<i64> {
        let start = chrono::DateTime::parse_from_rfc3339(&self.started_at).ok()?;
        let end = chrono::DateTime::parse_from_rfc3339(self.ended_at.as_deref()?).ok()?;
        Some((end - start).num_milliseconds().max(0))
    }

    /// The command as a single line, for a list or a header.
    pub fn command_line(&self) -> String {
        if self.argv.is_empty() {
            String::new()
        } else {
            self.argv.join(" ")
        }
    }
}

/// Why a run stopped.
///
/// The exit code answers "what did it return", not "who decided it was over",
/// and for a long-lived run the second question is the interesting one. A
/// sandboxed MCP server that its agent closed down is a normal end; one the
/// shim had to kill because it ignored both EOF and SIGTERM is a fact about
/// that server which its exit code — 143, the same as a polite shutdown —
/// cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndedBy {
    /// The command exited on its own.
    Exit,
    /// A signal ended it and the host saw which.
    Signal,
    /// The client closed the run's stdin, which is the MCP shutdown handshake.
    StdinEof,
    /// The host had to take the transport down; the guest was still running.
    Forced,
}

impl EndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            EndedBy::Exit => "exit",
            EndedBy::Signal => "signal",
            EndedBy::StdinEof => "stdin-eof",
            EndedBy::Forced => "forced",
        }
    }
}

impl fmt::Display for EndedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much isolation the guest could actually give a run.
///
/// The wrapper reports this so the host knows whether the cgroup it was handed
/// is the run's alone. A shared cgroup id is worse than none: it would sweep
/// every other process on the box into the report and call it evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunScope {
    /// A transient systemd scope under the user manager. No privilege needed.
    SystemdUser,
    /// A transient systemd scope under the system manager, entered through
    /// passwordless sudo but with the payload's uid/gid preserved.
    SystemdSystem,
    /// A hand-made cgroup under `/sys/fs/cgroup/devbox/`.
    Cgroup,
    /// None available — a container with no systemd and a read-only cgroupfs.
    /// Attribution falls back to the parent chain from the root pid.
    None,
}

impl RunScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RunScope::SystemdUser => "systemd-user",
            RunScope::SystemdSystem => "systemd-system",
            RunScope::Cgroup => "cgroup",
            RunScope::None => "none",
        }
    }

    /// Whether the cgroup the wrapper reported belongs to this run alone.
    pub fn is_exclusive(self) -> bool {
        !matches!(self, RunScope::None)
    }
}

impl FromStr for RunScope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "systemd-user" => RunScope::SystemdUser,
            "systemd-system" => RunScope::SystemdSystem,
            "cgroup" => RunScope::Cgroup,
            "none" => RunScope::None,
            other => bail!("unknown run scope '{other}'"),
        })
    }
}

/// What the guest wrapper publishes to `/run/devbox/runs/<id>.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestScope {
    pub run_id: String,
    #[serde(default)]
    pub cgroup_id: u64,
    #[serde(default)]
    pub cgroup_path: String,
    #[serde(default)]
    pub root_pid: u32,
    #[serde(default)]
    pub scope: String,
}

impl GuestScope {
    pub fn scope_enum(&self) -> RunScope {
        self.scope.parse().unwrap_or(RunScope::None)
    }

    /// The cgroup id worth storing: the reported one when the scope is the
    /// run's alone, zero otherwise.
    pub fn exclusive_cgroup_id(&self) -> u64 {
        if self.scope_enum().is_exclusive() {
            self.cgroup_id
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Run ids
// ---------------------------------------------------------------------------

/// Crockford base32, the ULID alphabet: no I, L, O or U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Ensures two ids minted in the same millisecond still sort in the order they
/// were minted. Holds the last timestamp in the high 48 bits and the last
/// random low bits' top 16 in the low 16, so the comparison and the bump are
/// one atomic operation.
static LAST: AtomicU64 = AtomicU64::new(0);

/// A fresh run id: 26 characters of Crockford base32 over 48 bits of
/// millisecond timestamp and 80 bits of randomness.
///
/// ULID's layout rather than a UUID because the id is also the sort key. Runs
/// are listed newest-first and stored one directory per id; a v4 UUID would
/// have made both a secondary index. Hand-rolled rather than a dependency
/// because it is thirty lines and the encoding is frozen by the format.
///
/// Monotonic within the process: two runs started in the same millisecond
/// still order correctly, because the second one's random field is forced
/// above the first's. Across processes the timestamp decides, and two devbox
/// invocations in the same millisecond on the same box are a tie the report
/// does not depend on breaking.
pub fn new_run_id() -> String {
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64 & 0x0000_FFFF_FFFF_FFFF;
    let mut random: u128 =
        ((rand::random::<u64>() as u128) << 16) | (rand::random::<u16>() as u128);

    // Same millisecond as the previous id? Step the random field instead of
    // rolling a fresh one, so the new id is strictly greater.
    let mut previous = LAST.load(Ordering::Relaxed);
    loop {
        let last_ms = previous >> 16;
        let last_hi = previous & 0xFFFF;
        if now == last_ms {
            // The top 16 bits of the 80-bit random field, incremented. On the
            // (astronomically unlikely) wrap the millisecond will have moved
            // long before it matters.
            let hi = (last_hi + 1) & 0xFFFF;
            random = (random & ((1u128 << 64) - 1)) | ((hi as u128) << 64);
        }
        let hi = ((random >> 64) & 0xFFFF) as u64;
        let next = (now << 16) | hi;
        if next <= previous {
            // Another thread moved it further along; take its value and retry.
            previous = LAST.load(Ordering::Relaxed);
            continue;
        }
        match LAST.compare_exchange_weak(previous, next, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => previous = observed,
        }
    }

    let value = ((now as u128) << 80) | random;
    let mut out = [b'0'; 26];
    // 26 * 5 = 130 bits for a 128-bit value, so the first character only ever
    // carries the top two bits — exactly ULID's layout.
    for (index, slot) in out.iter_mut().enumerate() {
        let shift = 125 - index * 5;
        *slot = CROCKFORD[((value >> shift) & 0x1F) as usize];
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a string is shaped like a run id.
///
/// Ids reach the store, the filesystem (`~/.devbox/runs/<box>/<id>/`) and the
/// console's URLs, so "looks like an id" is a security check as much as a
/// validation: nothing else may become a path component.
pub fn is_run_id(candidate: &str) -> bool {
    candidate.len() == 26
        && candidate
            .bytes()
            .all(|b| CROCKFORD.contains(&b.to_ascii_uppercase()))
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// A run the collector is currently attributing to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub run_id: String,
    /// Zero when the guest could not give the run an exclusive cgroup.
    pub cgroup_id: u64,
    /// Zero until the wrapper has published it.
    pub root_pid: u32,
    pub started_at: String,
    /// Set for a run that has ended but whose trailing events may still be in
    /// flight. `None` while it is running.
    pub ended_at: Option<String>,
}

impl ActiveRun {
    fn window_contains(&self, ts: &str) -> bool {
        if ts < self.started_at.as_str() {
            return false;
        }
        match &self.ended_at {
            Some(end) => ts <= end.as_str(),
            None => true,
        }
    }
}

/// What the store writes beside an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTag {
    pub run_id: String,
    pub attribution: Attribution,
}

/// The most descendants of one run the collector will remember.
///
/// The map is the pid-tree fallback's whole memory. A box that forks
/// continuously would grow it without bound, and this is a long-lived daemon
/// shared by every box on the host — so it is capped, and the cap is per run
/// rather than global so one busy run cannot evict a quiet one's tree.
const MAX_TRACKED_PIDS: usize = 4096;

/// Decides which run an event belongs to, in the order §4.2 fixes:
/// cgroup, then parent chain, then time window.
///
/// Stateful because the second rule is: "descends from the run's root pid" is
/// only answerable if the intervening pids were seen, and they arrive as their
/// own events. The state is a pid → run map seeded with each run's root pid and
/// extended whenever an event's *parent* is already in it.
#[derive(Debug, Default)]
pub struct Attributor {
    active: Vec<ActiveRun>,
    /// pid → run id, for the descendants of every active run's root.
    tree: HashMap<u32, String>,
    /// How many pids each run has contributed, so the cap is per run.
    tracked: HashMap<String, usize>,
}

impl Attributor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the set of runs being attributed to.
    ///
    /// Called with a fresh read of `runs WHERE status = 'running'`. Runs that
    /// disappeared take their pid subtree with them, which is what keeps the
    /// map bounded across a long-lived daemon's life.
    pub fn set_active(&mut self, runs: Vec<ActiveRun>) {
        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|id| !runs.iter().any(|r| &&r.run_id == id))
            .cloned()
            .collect();
        for id in gone {
            self.tree.retain(|_, run| run != &id);
            self.tracked.remove(&id);
        }
        // Seed every root pid the host has learned. A run whose wrapper has
        // not published yet has root_pid 0 and simply contributes nothing.
        for run in &runs {
            if run.root_pid != 0 && run.root_pid != UNATTRIBUTED_PID {
                self.tree.insert(run.root_pid, run.run_id.clone());
                self.tracked.entry(run.run_id.clone()).or_insert(0);
            }
        }
        self.active = runs;
    }

    /// Whether any run is currently being attributed to.
    pub fn is_idle(&self) -> bool {
        self.active.is_empty()
    }

    /// Attribute one event, learning from it on the way.
    pub fn attribute(&mut self, event: &Event) -> Option<RunTag> {
        if self.active.is_empty() {
            return None;
        }

        // 1. cgroup. Exact, and the only rule that needs no history — both the
        //    eBPF source (`bpf_get_current_cgroup_id`) and the proc source
        //    carry it, and it equals the inode of the cgroup directory the
        //    wrapper published.
        if event.pid != UNATTRIBUTED_PID
            && event.cgroup_id != 0
            && let Some(run) = self
                .active
                .iter()
                .find(|r| r.cgroup_id != 0 && r.cgroup_id == event.cgroup_id)
        {
            let id = run.run_id.clone();
            self.learn(event, &id);
            return Some(RunTag {
                run_id: id,
                attribution: Attribution::Cgroup,
            });
        }

        // 2. parent chain. The proc source has no cgroup for a process it did
        //    not see start, and a guest with no cgroup delegation has none at
        //    all; both still have pids.
        if event.pid != UNATTRIBUTED_PID {
            if let Some(id) = self.tree.get(&event.pid).cloned() {
                self.learn(event, &id);
                return Some(RunTag {
                    run_id: id,
                    attribution: Attribution::Pidtree,
                });
            }
            if event.ppid != 0
                && event.ppid != UNATTRIBUTED_PID
                && let Some(id) = self.tree.get(&event.ppid).cloned()
            {
                self.learn(event, &id);
                return Some(RunTag {
                    run_id: id,
                    attribution: Attribution::Pidtree,
                });
            }
            // A real pid that belongs to no run's tree is not the box being
            // quiet — it is another process. Do not fall through to the window
            // rule, which would credit this run with the whole box.
            return None;
        }

        // 3. window. Only for the pid-less events — a blocked packet, a
        //    netfilter verdict — and only when exactly one run could own them.
        let mut hit = None;
        for run in &self.active {
            if run.window_contains(&event.ts_wall) {
                if hit.is_some() {
                    // Two runs overlap this instant. Refusing to choose is the
                    // honest answer; the report counts it as unattributed.
                    return None;
                }
                hit = Some(run.run_id.clone());
            }
        }
        hit.map(|run_id| RunTag {
            run_id,
            attribution: Attribution::Window,
        })
    }

    /// Remember this event's pid as part of `run`, so its children attribute.
    fn learn(&mut self, event: &Event, run: &str) {
        if event.pid == 0 || event.pid == UNATTRIBUTED_PID {
            return;
        }
        if self.tree.contains_key(&event.pid) {
            return;
        }
        let count = self.tracked.entry(run.to_string()).or_insert(0);
        if *count >= MAX_TRACKED_PIDS {
            return;
        }
        *count += 1;
        self.tree.insert(event.pid, run.to_string());
    }
}

// ---------------------------------------------------------------------------
// The guest wrapper
// ---------------------------------------------------------------------------

/// Where the wrapper publishes its scope, preferred first.
///
/// `/run` is a tmpfs the guest clears on boot, which is right for a file that
/// describes a live process. It is also root-owned, so the bootstrap makes it
/// group-writable through sudo when it can and falls back to `/tmp` when it
/// cannot — a box with no passwordless sudo still gets attribution.
pub const RUN_STATE_DIRS: [&str; 2] = ["/run/devbox/runs", "/tmp/.devbox-runs"];

/// The sentinel that tells the wrapper it is already inside the scope.
///
/// Also what [`is_wrapper_command`] recognises. These constants exist because
/// the report has to fold devbox's own plumbing out of the process tree, and a
/// second copy of the string in the folding rule would silently stop matching
/// the day the wrapper changed — leaving a report that renders three lines of
/// shell where the user's command should be, with nothing failing.
pub const SCOPED_FLAG: &str = "--devbox-scoped";

/// The transient systemd unit, and the cgroup directory, one run gets.
pub const UNIT_PREFIX: &str = "devbox-run-";

/// Where the bootstrap writes the wrapper inside the guest.
pub const WRAPPER_PATH_PREFIX: &str = "/tmp/.devbox-run-";

/// The bootstrap's heredoc delimiter — the one word that identifies it even
/// when the rest of the script has been rewritten.
pub const HEREDOC_TAG: &str = "DEVBOX_WRAPPER_EOF";

/// `$0` for the wrapper's shell, so `ps` says what it is.
pub const BOOTSTRAP_ARGV0: &str = "devbox-run";

/// The run's identity in the command's environment.
pub const RUN_ID_ENV: &str = "DEVBOX_RUN_ID";

/// The wrapper, stage 2 and stage 1 in one script (§4.2).
///
/// Stage 1 picks the most exclusive scope the guest can give, then re-execs
/// this same file inside it; stage 2 publishes the cgroup it landed in and
/// execs the command. Two stages because the cgroup id is only knowable from
/// *inside* the cgroup, and only the process that is about to become the
/// command can report a root pid that means anything.
///
/// POSIX `sh`, no bashisms: the Docker boxes are Ubuntu with `dash` as `/bin/sh`.
pub fn wrapper_script() -> &'static str {
    // Placeholders rather than `format!`: the script is full of `${…}` and the
    // escaping would make it unreadable, which is the wrong trade for a shell
    // program people have to be able to check by eye. Substituted once.
    static SCRIPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SCRIPT.get_or_init(|| {
        RAW_WRAPPER
            .replace("@SCOPED@", SCOPED_FLAG)
            .replace("@UNIT@", UNIT_PREFIX)
            .replace("@RUN_ID_ENV@", RUN_ID_ENV)
    })
}

const RAW_WRAPPER: &str = r#"# devbox run wrapper — see src/obs/run.rs.
if [ "${1-}" = "@SCOPED@" ]; then
    shift
    method=$1; rid=$2; state=$3; cwd=$4; home=$5; who=$6
    shift 6
    # Builtins, not `awk`. Every process the wrapper spawns lands in the run's
    # own cgroup and therefore in the run's own report, where `awk -F: /^0::/`
    # is a line the reader has to stop and decode. `stat` has no builtin
    # equivalent and `mv` is buying atomicity, so those two stay.
    cg=
    while IFS= read -r line; do
        case $line in 0::*) cg=${line#0::} ;; esac
    done < /proc/self/cgroup 2>/dev/null
    ino=0
    if [ -n "$cg" ]; then ino=$(stat -c %i "/sys/fs/cgroup$cg" 2>/dev/null || echo 0); fi
    printf '{"run_id":"%s","cgroup_id":%s,"cgroup_path":"%s","root_pid":%s,"scope":"%s"}\n' \
        "$rid" "${ino:-0}" "$cg" "$$" "$method" > "$state.tmp" 2>/dev/null &&
        mv "$state.tmp" "$state" 2>/dev/null
    @RUN_ID_ENV@=$rid; export @RUN_ID_ENV@
    if [ -n "$home" ] && [ "${HOME-}" != "$home" ]; then HOME=$home; export HOME; fi
    if [ -n "$who" ] && [ "${USER-}" != "$who" ]; then USER=$who; LOGNAME=$who; export USER LOGNAME; fi
    if [ -n "$cwd" ] && [ -d "$cwd" ]; then cd "$cwd" || true; fi
    exec "$@"
fi

self=$0
rid=$1; state=$2; cwd=$3
shift 3
unit=@UNIT@$rid
home=${HOME-}; who=${USER-$(id -un 2>/dev/null || echo "")}
method=
if [ -d /run/systemd/system ]; then
    if systemd-run --user --scope --quiet -- true >/dev/null 2>&1; then
        method=systemd-user
    elif sudo -n systemd-run --scope --quiet --uid="$(id -u)" --gid="$(id -g)" -- true >/dev/null 2>&1; then
        method=systemd-system
    fi
fi
if [ -z "$method" ] && mkdir -p "/sys/fs/cgroup/devbox/$unit" 2>/dev/null; then
    method=cgroup
fi
[ -n "$method" ] || method=none

case $method in
systemd-user)
    exec systemd-run --user --scope --unit="$unit" --quiet --same-dir -- \
        sh "$self" @SCOPED@ "$method" "$rid" "$state" "$cwd" "$home" "$who" "$@" ;;
systemd-system)
    exec sudo -n systemd-run --scope --unit="$unit" --quiet --same-dir \
        --uid="$(id -u)" --gid="$(id -g)" -- \
        sh "$self" @SCOPED@ "$method" "$rid" "$state" "$cwd" "$home" "$who" "$@" ;;
cgroup)
    echo $$ > "/sys/fs/cgroup/devbox/$unit/cgroup.procs" 2>/dev/null || method=none
    exec sh "$self" @SCOPED@ "$method" "$rid" "$state" "$cwd" "$home" "$who" "$@" ;;
*)
    exec sh "$self" @SCOPED@ none "$rid" "$state" "$cwd" "$home" "$who" "$@" ;;
esac
"#;

/// The argv `devbox run` hands the runtime.
///
/// One `sh -c` that writes the wrapper into the guest and execs it, with the
/// user's command following as *separate argv words* — which is what keeps a
/// command containing quotes, newlines or `$` from being re-parsed on the way
/// in. Everything interpolated into the script itself is host-generated (a run
/// id, which [`is_run_id`] constrains to base32, and a fixed directory list).
pub fn bootstrap(run_id: &str, cwd: &str) -> Vec<String> {
    let primary = RUN_STATE_DIRS[0];
    let fallback = RUN_STATE_DIRS[1];
    let script = format!(
        r#"d={primary}
{{ mkdir -p "$d" 2>/dev/null && [ -w "$d" ]; }} || sudo -n sh -c 'mkdir -p {primary} && chmod 1777 {primary}' 2>/dev/null
[ -w "$d" ] || {{ d={fallback}; mkdir -p "$d" 2>/dev/null; }}
w={prefix}{run_id}.sh
cat > "$w" <<'{tag}'
{wrapper}
{tag}
exec sh "$w" {run_id} "$d/{run_id}.json" '{cwd}' "$@"
"#,
        primary = primary,
        fallback = fallback,
        prefix = WRAPPER_PATH_PREFIX,
        tag = HEREDOC_TAG,
        run_id = run_id,
        cwd = cwd.replace('\'', ""),
        wrapper = wrapper_script(),
    );
    vec![
        "sh".to_string(),
        "-c".to_string(),
        script,
        BOOTSTRAP_ARGV0.to_string(),
    ]
}

/// The one-shot readback: block in the guest until the wrapper publishes, then
/// print the record.
///
/// One exec rather than a polling loop of them, because every `limactl shell`
/// costs an ssh round trip and shows up in the box's own event stream. Bounded
/// so a wrapper that never gets that far does not hold the readback open for
/// the length of the run.
pub fn readback_argv(run_id: &str, timeout_ms: u64) -> Vec<String> {
    let attempts = (timeout_ms / 50).max(1);
    let primary = RUN_STATE_DIRS[0];
    let fallback = RUN_STATE_DIRS[1];
    let script = format!(
        r#"i=0
while [ $i -lt {attempts} ]; do
  for f in {primary}/{run_id}.json {fallback}/{run_id}.json; do
    if [ -s "$f" ]; then cat "$f"; exit 0; fi
  done
  i=$((i+1))
  sleep 0.05
done
exit 1
"#
    );
    vec!["sh".to_string(), "-c".to_string(), script]
}

/// Where `$home` sits in the wrapper's stage-2 argv, counting from the flag.
///
/// `@SCOPED@ method rid state cwd home who` — see [`RAW_WRAPPER`]'s stage 2,
/// which unpacks exactly these six in this order. Here so the report can fold
/// a guest path back to `~` without a second copy of that ordering.
const HOME_AFTER_FLAG: usize = 5;

/// The guest's `$HOME`, as the wrapper recorded it, from a run's exec events.
///
/// `None` when no wrapper exec was captured — a short run whose first events
/// arrived before the host knew the cgroup, or an `exec`/`shell` run, which
/// has no wrapper at all. The caller's fallback is "fold nothing to `~`",
/// which is a worse-looking report rather than a wrong one.
pub fn home_from_wrapper(argv: &[String]) -> Option<&str> {
    let flag = argv.iter().position(|a| a == SCOPED_FLAG)?;
    argv.get(flag + HOME_AFTER_FLAG)
        .map(String::as_str)
        .filter(|home| home.starts_with('/') && home.len() > 1)
}

/// Whether this argv is devbox's own plumbing rather than the user's command.
///
/// A run report is read by someone asking "what did my command do". Three
/// lines of heredoc, a `systemd-run --scope`, and a `stat` on a cgroup path
/// answer a different question, and they arrive first — so the reader's eye
/// lands on devbox's implementation before it reaches their own program.
///
/// Every marker here is the constant the generator uses, not a copy of it: the
/// failure mode of a copy is a report that silently stops folding, which is
/// invisible until someone reads one.
///
/// Positional where a bare substring would over-match. `sh -c 'echo
/// $DEVBOX_RUN_ID'` is the user's command and mentions the variable; `env --
/// DEVBOX_RUN_ID=… cmd` is devbox's shim and *assigns* it. Only the second is
/// folded.
pub fn is_wrapper_command(argv: &[String]) -> bool {
    if argv.is_empty() {
        return false;
    }
    let word = |i: usize| argv.get(i).map(String::as_str).unwrap_or_default();

    // `env -- K=V … cmd`, from `broker::with_env`.
    if word(0) == "env"
        && word(1) == "--"
        && argv[2..]
            .iter()
            .any(|a| a.starts_with(&format!("{RUN_ID_ENV}=")))
    {
        return true;
    }

    // The wrapper re-execing itself into its scope, and `systemd-run` doing it.
    if argv.iter().any(|a| a == SCOPED_FLAG) {
        return true;
    }
    if word(0) == BOOTSTRAP_ARGV0 {
        return true;
    }

    argv.iter().any(|a| {
        // The bootstrap: `sh -c '…<<DEVBOX_WRAPPER_EOF…'`.
        a.contains(HEREDOC_TAG)
            // The wrapper file itself, and anything that touches it.
            || a.starts_with(WRAPPER_PATH_PREFIX)
            // `systemd-run --unit=devbox-run-<id>`, the `stat` on its cgroup
            // directory, and the `rmdir` that cleans it up.
            || a.contains(UNIT_PREFIX)
            // The scope record the wrapper publishes and the host reads back.
            || RUN_STATE_DIRS.iter().any(|dir| a.starts_with(dir))
    })
}

/// Remove a finished run's wrapper and scope record from the guest.
pub fn cleanup_argv(run_id: &str) -> Vec<String> {
    let primary = RUN_STATE_DIRS[0];
    let fallback = RUN_STATE_DIRS[1];
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "rm -f {prefix}{run_id}.sh {primary}/{run_id}.json {fallback}/{run_id}.json 2>/dev/null; \
             rmdir /sys/fs/cgroup/devbox/{unit}{run_id} 2>/dev/null; true",
            prefix = WRAPPER_PATH_PREFIX,
            unit = UNIT_PREFIX
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Event, EventType, Exec, Net};

    fn event(kind: EventType, pid: u32, ppid: u32, cgroup: u64, ts: &str) -> Event {
        Event {
            ts_wall: ts.to_string(),
            ts_mono_ns: 1,
            box_id: "b".into(),
            cgroup_id: cgroup,
            pid,
            tid: pid,
            ppid,
            comm: "c".into(),
            uid: 1000,
            kind,
            net: match kind {
                EventType::Connect | EventType::Dns | EventType::Tls | EventType::Accept => {
                    Some(Net::default())
                }
                _ => None,
            },
            exec: match kind {
                EventType::Exec => Some(Exec::default()),
                _ => None,
            },
            file: None,
            api: None,
            policy: None,
            credential: None,
        }
    }

    fn run(id: &str, cgroup: u64, root: u32, start: &str, end: Option<&str>) -> ActiveRun {
        ActiveRun {
            run_id: id.to_string(),
            cgroup_id: cgroup,
            root_pid: root,
            started_at: start.to_string(),
            ended_at: end.map(str::to_string),
        }
    }

    #[test]
    fn run_ids_are_twenty_six_crockford_characters() {
        let id = new_run_id();
        assert_eq!(id.len(), 26, "{id}");
        assert!(is_run_id(&id), "{id}");
        assert!(!is_run_id("short"));
        // I, L, O and U are not in the alphabet, so an id that contains one
        // came from somewhere else and must not become a path component.
        assert!(!is_run_id("IIIIIIIIIIIIIIIIIIIIIIIIII"));
    }

    #[test]
    fn run_ids_are_monotonic_even_within_one_millisecond() {
        let ids: Vec<String> = (0..500).map(|_| new_run_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "ids must sort in the order they were minted");
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids must be distinct");
    }

    #[test]
    fn cgroup_wins_when_it_matches() {
        let mut a = Attributor::new();
        a.set_active(vec![run(
            "R1",
            17557,
            900,
            "2026-09-05T00:00:00.000Z",
            None,
        )]);
        let tag = a
            .attribute(&event(
                EventType::Exec,
                1000,
                1,
                17557,
                "2026-09-05T00:00:01.000Z",
            ))
            .expect("cgroup match");
        assert_eq!(tag.attribution, Attribution::Cgroup);
        assert_eq!(tag.run_id, "R1");
    }

    #[test]
    fn the_parent_chain_carries_a_run_with_no_cgroup() {
        let mut a = Attributor::new();
        a.set_active(vec![run("R1", 0, 900, "2026-09-05T00:00:00.000Z", None)]);
        // A child of the root pid.
        let child = a
            .attribute(&event(
                EventType::Exec,
                901,
                900,
                0,
                "2026-09-05T00:00:01.000Z",
            ))
            .expect("child of root");
        assert_eq!(child.attribution, Attribution::Pidtree);
        // And a grandchild, which is only reachable because the child was
        // learned on the way past.
        let grandchild = a
            .attribute(&event(
                EventType::Connect,
                902,
                901,
                0,
                "2026-09-05T00:00:02.000Z",
            ))
            .expect("grandchild");
        assert_eq!(grandchild.attribution, Attribution::Pidtree);
        assert_eq!(grandchild.run_id, "R1");
    }

    #[test]
    fn an_unrelated_process_is_not_attributed_by_window() {
        let mut a = Attributor::new();
        a.set_active(vec![run(
            "R1",
            17557,
            900,
            "2026-09-05T00:00:00.000Z",
            None,
        )]);
        // Real pid, different cgroup, no ancestry: another process on the box.
        // The window rule must not pick it up, or every report becomes a
        // report of the whole box.
        assert!(
            a.attribute(&event(
                EventType::Exec,
                4242,
                1,
                5632,
                "2026-09-05T00:00:01.000Z"
            ))
            .is_none()
        );
    }

    #[test]
    fn a_pidless_event_inside_the_window_is_attributed_as_window() {
        let mut a = Attributor::new();
        a.set_active(vec![run(
            "R1",
            17557,
            900,
            "2026-09-05T00:00:00.000Z",
            None,
        )]);
        let tag = a
            .attribute(&event(
                EventType::Policy,
                UNATTRIBUTED_PID,
                0,
                0,
                "2026-09-05T00:00:01.000Z",
            ))
            .expect("window match");
        assert_eq!(tag.attribution, Attribution::Window);
    }

    #[test]
    fn a_pidless_event_never_matches_a_cgroup() {
        let mut a = Attributor::new();
        // The sentinel pid carrying the run's own cgroup id: still a window
        // decision at best, because the pid is the absence of a process.
        a.set_active(vec![run(
            "R1",
            17557,
            900,
            "2026-09-05T00:00:00.000Z",
            None,
        )]);
        let tag = a
            .attribute(&event(
                EventType::Policy,
                UNATTRIBUTED_PID,
                0,
                17557,
                "2026-09-05T00:00:01.000Z",
            ))
            .expect("window match");
        assert_eq!(tag.attribution, Attribution::Window);
    }

    #[test]
    fn overlapping_windows_refuse_to_guess() {
        let mut a = Attributor::new();
        a.set_active(vec![
            run("R1", 1, 900, "2026-09-05T00:00:00.000Z", None),
            run("R2", 2, 950, "2026-09-05T00:00:00.500Z", None),
        ]);
        assert!(
            a.attribute(&event(
                EventType::Policy,
                UNATTRIBUTED_PID,
                0,
                0,
                "2026-09-05T00:00:01.000Z"
            ))
            .is_none()
        );
    }

    #[test]
    fn an_event_outside_every_window_is_unattributed() {
        let mut a = Attributor::new();
        a.set_active(vec![run(
            "R1",
            1,
            900,
            "2026-09-05T00:00:00.000Z",
            Some("2026-09-05T00:00:02.000Z"),
        )]);
        assert!(
            a.attribute(&event(
                EventType::Policy,
                UNATTRIBUTED_PID,
                0,
                0,
                "2026-09-05T00:00:09.000Z"
            ))
            .is_none()
        );
    }

    #[test]
    fn a_retired_run_releases_its_pid_tree() {
        let mut a = Attributor::new();
        a.set_active(vec![run("R1", 0, 900, "2026-09-05T00:00:00.000Z", None)]);
        a.attribute(&event(
            EventType::Exec,
            901,
            900,
            0,
            "2026-09-05T00:00:01.000Z",
        ));
        assert!(a.tree.contains_key(&901));
        a.set_active(vec![]);
        assert!(a.tree.is_empty(), "the map must not outlive the run");
        assert!(a.is_idle());
    }

    #[test]
    fn a_shared_cgroup_is_never_stored_as_the_runs_own() {
        let shared = GuestScope {
            run_id: "R".into(),
            cgroup_id: 5632,
            cgroup_path: "/user.slice".into(),
            root_pid: 10,
            scope: "none".into(),
        };
        assert_eq!(shared.exclusive_cgroup_id(), 0);
        let exclusive = GuestScope {
            scope: "systemd-user".into(),
            ..shared.clone()
        };
        assert_eq!(exclusive.exclusive_cgroup_id(), 5632);
    }

    #[test]
    fn the_environment_travels_inside_the_wrapper_not_in_front_of_it() {
        // `env -- K=V … sh -c <wrapper>` would be lost on the
        // `sudo -n systemd-run` path, because sudo resets the environment.
        // Inside the wrapper's argv it is carried verbatim through every hop —
        // the wrapper only ever passes `"$@"` along — so it survives sudo, the
        // transient scope, and the re-exec into stage 2.
        let env = vec![
            ("DEVBOX_BROKER_URL".to_string(), "http://h:9".to_string()),
            ("DEVBOX_RUN_ID".to_string(), "01ABC".to_string()),
        ];
        let mut argv = bootstrap("01ABCDEFGHJKMNPQRSTVWXYZ00", "/workspace");
        argv.extend(crate::broker::with_env(&env, &["true".to_string()]));

        assert_eq!(argv[0], "sh", "the wrapper still leads");
        let env_at = argv.iter().position(|a| a == "env").expect("an env prefix");
        let script_at = argv.iter().position(|a| a.contains("DEVBOX_WRAPPER_EOF"));
        assert!(
            script_at.unwrap() < env_at,
            "the environment must be inside the wrapper's argv, not before it"
        );
        assert!(argv.iter().any(|a| a == "DEVBOX_BROKER_URL=http://h:9"));
        assert_eq!(argv.last().unwrap(), "true", "the command comes last");
    }

    #[test]
    fn the_bootstrap_passes_the_command_as_separate_words() {
        let argv = bootstrap("01ABCDEFGHJKMNPQRSTVWXYZ00", "/workspace");
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("DEVBOX_WRAPPER_EOF"));
        // `"$@"` is what keeps the user's argv out of the script's own parse.
        assert!(argv[2].trim_end().ends_with(r#""$@""#));
        assert_eq!(argv[3], "devbox-run", "$0 for the wrapper's shell");
    }
}
