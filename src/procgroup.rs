//! Process groups: how devbox stops something it started, and everything that
//! something started.
//!
//! Three callers need the same primitives and had been growing their own. The
//! MCP shim reaps a server's whole tree; the collector and broker daemons
//! replace an older build of themselves. All three face the same two hazards,
//! and both have already been paid for once:
//!
//! - **A recorded group id is a number, not a proof.** A process that reads
//!   `getpgrp()` without first making a group of its own records its
//!   *caller's* group, and a reaper then kills the caller. On a Linux CI
//!   runner that caller was `cargo test`, and the job died with 143 and no
//!   failing test to point at. [`lead_own_group`] is the fix: make the group,
//!   then check that we lead it, and refuse to record anything else.
//! - **A pid outlives its process.** The number is free for reuse the moment
//!   it is reaped, so anything that turns a stored id into a signal has to ask
//!   what the process *is* first. [`stop_group`] anchors that on the group
//!   leader: a group led by a live devbox daemon is that daemon's group, and
//!   nothing else can have acquired the id while its leader still holds it.
//!
//! The second hazard is why this module does not check that every member of a
//! group looks like a daemon. A healthy collector's group contains its agent
//! transports — `limactl shell`, and the `ssh` that one spawns — because a
//! child inherits its parent's group. Measured on the host rather than
//! assumed:
//!
//! ```text
//!   PID  PPID  PGID ARGS
//! 69388     1 69388 …/devbox __collector
//! 69535 69388 69388 S limactl shell --workdir /home devbox-devtest -- sh -c …
//! 69539 69535 69388 S /usr/bin/ssh -F /dev/null -o IdentityFile=…
//! ```
//!
//! A rule of "every member must name a daemon" would therefore refuse every
//! real takeover on every real host. Stopping those children is not collateral
//! damage — it is the point: `kill_on_drop` does not run when the daemon is
//! killed rather than dropped, and those are the processes that would be left
//! holding a guest's stdio.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// This process's own group id.
pub fn own_process_group() -> i32 {
    // SAFETY: `getpgrp` takes no arguments, touches no memory, and cannot fail.
    unsafe { libc::getpgrp() }
}

/// The process group of whoever started us, when it can be read.
///
/// `getppid` is exact; turning a ppid into its group needs `getpgid`, which
/// returns `ESRCH` if the parent has already gone — a `None` this treats as
/// "nothing to protect", because a parent that has exited cannot be killed.
pub(crate) fn parent_process_group() -> Option<i32> {
    // SAFETY: neither call takes a pointer, and `getpgid` reports failure
    // through its return value rather than through memory.
    let parent = unsafe { libc::getppid() };
    if parent <= 0 {
        return None;
    }
    let group = unsafe { libc::getpgid(parent) };
    (group > 0).then_some(group)
}

/// Become the leader of a process group containing only us, and return its id.
///
/// Called by a daemon before it publishes anything about itself. The returned
/// id is safe to record and later signal precisely because it was *created*
/// here: it cannot be the caller's group, so a later reap cannot travel back
/// up to whoever started the daemon.
///
/// Two ways in, because both happen. A daemon spawned by devbox already leads
/// its own group (`CommandExt::process_group(0)`), and `setsid` would fail for
/// a group leader — so that case is already done and is checked, not repeated.
/// A daemon run straight from a shell — which is how the integration suite
/// starts one — is in the shell's group, and `setsid` moves it out of the
/// shell's session entirely, which is what a daemon wants anyway.
pub fn lead_own_group() -> Result<i32> {
    let pid = std::process::id() as i32;
    if own_process_group() != pid {
        // SAFETY: neither call takes a pointer; both report failure through
        // their return value.
        let made_session = unsafe { libc::setsid() } >= 0;
        if !made_session {
            let session_error = std::io::Error::last_os_error();
            // SAFETY: as above.
            if unsafe { libc::setpgid(0, 0) } < 0 {
                let group_error = std::io::Error::last_os_error();
                bail!(
                    "could not put this daemon in a process group of its own \
                     (setsid: {session_error}; setpgid: {group_error})"
                );
            }
        }
    }
    let pgid = own_process_group();
    if pgid != pid {
        // Never record it anyway. A group we do not lead is somebody else's,
        // and the whole value of writing the number down is that signalling it
        // later can only reach our own descendants.
        bail!("this daemon is in process group {pgid} but its pid is {pid}");
    }
    Ok(pgid)
}

/// One row of the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// `ps` state, first letter significant. `Z` is a process that has already
    /// exited and is waiting to be reaped.
    pub state: String,
    pub command: String,
}

impl Process {
    /// Already dead, and waiting only for its parent to collect it.
    ///
    /// Nothing may treat one of these as alive. A zombie stays in the process
    /// table — and in its process group — until its *parent* waits on it, and
    /// that parent can be the daemon being replaced, or a test harness that
    /// never waits at all. Counting one as a live member made `stop_group`
    /// wait out its whole deadline, escalate to SIGKILL, and then fail: macOS
    /// returns `EPERM` for a group whose only remaining member is a zombie, so
    /// a takeover that had in fact worked was reported as one that could not
    /// signal anything.
    pub fn is_zombie(&self) -> bool {
        self.state.starts_with('Z')
    }

    /// Whether this row is a devbox daemon of the given kind.
    ///
    /// Matched as a whole argument. A path that merely contains `__collector`
    /// is not a collector, and the difference decides whether something gets
    /// a signal.
    pub fn is(&self, marker: &str) -> bool {
        self.command.split_whitespace().any(|arg| arg == marker)
    }
}

/// Every process this user can see, in one call.
///
/// One `ps` rather than one per pid: this runs on the path of every lifecycle
/// command, and the answer is only useful if it describes a single instant.
pub fn snapshot() -> Result<Vec<Process>> {
    let output = Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,pgid=,state=,command="])
        .stdin(Stdio::null())
        .output()
        .context("read the process table")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    Ok(parse_snapshot(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_snapshot(text: &str) -> Vec<Process> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let pgid = fields.next()?.parse().ok()?;
            let state = fields.next()?.to_string();
            // The command is the rest of the line, spaces and all. Rebuilding
            // it from the iterator would collapse the runs of spaces inside an
            // argument, and the identity check reads those arguments.
            let command = line
                .split_whitespace()
                .take(4)
                .fold(line.trim_start(), |rest, field| {
                    rest.strip_prefix(field).unwrap_or(rest).trim_start()
                })
                .to_string();
            Some(Process {
                pid,
                ppid,
                pgid,
                state,
                command,
            })
        })
        .collect()
}

/// Signal a process group, refusing the ones that are not safe to name.
///
/// Three refusals, and each of them has a way of being reached:
///
/// - **`0`** means "the sender's own group" to `kill(2)`. A zero that reached
///   here would take out the caller, its parent, and everything sharing their
///   group, and it would do it while looking like an ordinary cleanup.
/// - **`1`** is `init`'s group, and `kill(-1, …)` is "every process this user
///   may signal".
/// - **our own group**, which is the same disaster as `0` reached the long way
///   round. It is not hypothetical: on a Linux CI runner this function's
///   predecessor sent SIGTERM to a group that turned out to contain
///   `cargo test`, and the whole step died with 143.
///
/// Through `killpg(2)` rather than `kill(1)`: the shell tool differs between
/// procps, util-linux and BSD, needs a PATH lookup on the shutdown path, and
/// puts a whole subprocess spawn between deciding to signal and signalling —
/// which is exactly the window a reaped-and-recycled pid needs to become
/// somebody else's.
pub fn signal_group(pgid: i32, signal: i32) -> std::result::Result<(), String> {
    if pgid <= 1 {
        return Err(format!(
            "refusing to signal process group {pgid}: it names this process's own \
             group or every process on the host"
        ));
    }
    let ours = own_process_group();
    if pgid == ours {
        return Err(format!(
            "refusing to signal process group {pgid}: it is the group this process is \
             in, so the signal would come back to us"
        ));
    }
    // And our parent's, for the case our own group is not the caller's: a
    // process that someone launched into a group of its own would pass the
    // check above while still being able to signal the process that launched
    // it.
    if let Some(parent) = parent_process_group()
        && pgid == parent
    {
        return Err(format!(
            "refusing to signal process group {pgid}: it is the group of the process \
             that started this one"
        ));
    }
    // SAFETY: `killpg` takes two integers and cannot write through a pointer.
    // A group that has already gone is `ESRCH`, which is the normal outcome of
    // cleaning up after something that cleaned up after itself.
    if unsafe { libc::killpg(pgid, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("could not signal process group {pgid}: {error}"));
        }
    }
    Ok(())
}

/// How a group ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    /// Members present when the first signal was sent.
    pub signalled: usize,
    /// Whether SIGTERM alone was not enough.
    pub escalated: bool,
    /// Members still alive when this gave up. Zero is the expected answer.
    pub survivors: usize,
}

/// Whether `pgid` names a group currently led by a live devbox daemon.
///
/// This is the whole safety argument for signalling a recorded group id. A
/// group id is only reused after its leader is reaped, so a leader that is
/// still alive and still named `marker` proves the group is the one that was
/// recorded — no separate liveness or generation counter is needed.
pub fn led_by_daemon(processes: &[Process], pgid: i32, marker: &str) -> bool {
    processes.iter().any(|process| {
        process.pid == pgid && process.pgid == pgid && !process.is_zombie() && process.is(marker)
    })
}

/// Stop a daemon and everything it started, and say whether it worked.
///
/// Refuses outright unless the group is still led by a daemon of this kind:
/// without that, the id could name whatever inherited the number.
pub fn stop_group(pgid: i32, marker: &str, patience: Duration) -> Result<Stopped> {
    let processes = snapshot()?;
    if !led_by_daemon(&processes, pgid, marker) {
        bail!("process group {pgid} is not led by a devbox {marker}; refusing to signal it");
    }
    let signalled = processes
        .iter()
        .filter(|p| p.pgid == pgid && !p.is_zombie())
        .count();
    signal_group(pgid, libc::SIGTERM).map_err(anyhow::Error::msg)?;

    let deadline = Instant::now() + patience;
    let mut escalated = false;
    loop {
        let alive = members(pgid)?;
        if alive == 0 {
            return Ok(Stopped {
                signalled,
                escalated,
                survivors: 0,
            });
        }
        if Instant::now() >= deadline {
            if escalated {
                return Ok(Stopped {
                    signalled,
                    escalated,
                    survivors: alive,
                });
            }
            // One escalation, then a short second wait. A daemon that ignores
            // SIGTERM is not going to answer a longer wait for it, and leaving
            // it running is how a host ends up with a hundred of them.
            escalated = true;
            signal_group(pgid, libc::SIGKILL).map_err(anyhow::Error::msg)?;
            std::thread::sleep(Duration::from_millis(200));
            return Ok(Stopped {
                signalled,
                escalated,
                survivors: members(pgid)?,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Members of a group that are still running.
fn members(pgid: i32) -> Result<usize> {
    Ok(snapshot()?
        .iter()
        .filter(|p| p.pgid == pgid && !p.is_zombie())
        .count())
}

/// The working directory of a running process, when it can be read.
///
/// A daemon is started with its state directory as its cwd, so this is how a
/// process on the host is tied back to the devbox state it serves — and the
/// only reason it is safe to reap one. `/proc` where there is one; `lsof`
/// otherwise, which is what macOS has.
pub fn working_directories(pids: &[i32]) -> Vec<(i32, PathBuf)> {
    if pids.is_empty() {
        return Vec::new();
    }
    if cfg!(target_os = "linux") {
        return pids
            .iter()
            .filter_map(|pid| {
                std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|path| (*pid, path))
            })
            .collect();
    }
    // One `lsof` for every pid at once. Per-pid it is a subprocess each, and
    // this runs before every lifecycle command.
    let list = pids
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(output) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &list, "-Fpn"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    parse_lsof(&String::from_utf8_lossy(&output.stdout))
}

/// `lsof -F` output: one field per line, tagged by its first character, with
/// `p` opening each process's block and `n` naming the path.
fn parse_lsof(text: &str) -> Vec<(i32, PathBuf)> {
    let mut found = Vec::new();
    let mut current = None;
    for line in text.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => current = value.parse::<i32>().ok(),
            "n" => {
                if let Some(pid) = current {
                    found.push((pid, PathBuf::from(value)));
                    current = None;
                }
            }
            _ => {}
        }
    }
    found
}

/// Daemons of this kind that serve `state_dir` but are not `keep`.
///
/// Deliberately scoped to one state directory. A devbox daemon serving some
/// *other* state directory is not an orphan — it is somebody's isolated
/// checkout, or a second account's — and "every daemon that is not the one I
/// know about" would kill it. The cwd is what makes the question answerable
/// at all: a daemon is started with its state directory as its working
/// directory, so the process itself says which state it belongs to.
pub fn orphans(
    processes: &[Process],
    marker: &str,
    state_dir: &Path,
    keep: &[i32],
) -> Vec<Process> {
    let candidates: Vec<&Process> = processes
        .iter()
        .filter(|process| {
            process.is(marker) && !process.is_zombie() && !keep.contains(&process.pid)
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let pids: Vec<i32> = candidates.iter().map(|process| process.pid).collect();
    let directories = working_directories(&pids);
    serving(&candidates, state_dir, &directories)
}

/// The selection `orphans` makes once it knows where each candidate lives.
///
/// Split out so the rule can be tested without a process table: a process
/// whose directory could not be read is *not* selected, because "we could not
/// tell" must never become "so kill it".
fn serving(
    candidates: &[&Process],
    state_dir: &Path,
    directories: &[(i32, PathBuf)],
) -> Vec<Process> {
    // Both sides resolved, because the two come from different places and
    // spell the same directory differently. On macOS `/tmp` and `/var` are
    // symlinks into `/private`, so a state directory built from a path the
    // process was *given* and a working directory read back from the kernel
    // never compare equal — and the reaper silently finds nothing, which is
    // the failure that looks like success.
    let wanted = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    candidates
        .iter()
        .filter(|process| {
            directories.iter().any(|(pid, cwd)| {
                *pid == process.pid && cwd.canonicalize().unwrap_or_else(|_| cwd.clone()) == wanted
            })
        })
        .map(|process| (*process).clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_table_parses_commands_containing_spaces() {
        let rows = parse_snapshot(
            "69388 1 69388 S /Users/x/devbox __collector\n\
             69535 69388 69388 S limactl shell --workdir /home devbox-devtest -- sh -c if [ 1 ]\n\
             bad line\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 69388);
        assert_eq!(rows[0].ppid, 1);
        assert_eq!(rows[0].pgid, 69388);
        assert_eq!(rows[0].command, "/Users/x/devbox __collector");
        assert!(rows[0].is("__collector"));
        assert_eq!(rows[1].pid, 69535);
        assert!(rows[1].command.starts_with("limactl shell"));
        assert!(rows[1].command.ends_with("sh -c if [ 1 ]"));
    }

    /// The children a collector spawns share its group and name no daemon.
    /// Requiring every member to look like one would refuse every takeover
    /// that has ever mattered.
    #[test]
    fn a_group_is_judged_by_its_leader_not_by_every_member() {
        let rows = parse_snapshot(
            "69388 1 69388 S /Users/x/devbox __collector\n\
             69535 69388 69388 S limactl shell --workdir /home devbox-devtest\n\
             69539 69535 69388 S /usr/bin/ssh -F /dev/null\n",
        );
        assert!(led_by_daemon(&rows, 69388, "__collector"));
        assert!(!rows[1].is("__collector"));
        assert_eq!(rows.iter().filter(|p| p.pgid == 69388).count(), 3);
    }

    #[test]
    fn a_group_whose_leader_is_gone_or_is_something_else_is_refused() {
        let rows = parse_snapshot(
            "4242 1 4242 S /usr/bin/some-other-program\n\
             4300 4242 4242 S a child of it\n",
        );
        // The id is live and leads a group; it is simply not ours.
        assert!(!led_by_daemon(&rows, 4242, "__collector"));
        // And an id with no leader at all.
        assert!(!led_by_daemon(&rows, 9999, "__collector"));
    }

    /// A leader that is in the group but is not the leader of it — the shape a
    /// recycled id produces — must not pass.
    #[test]
    fn a_member_that_does_not_lead_its_group_is_not_a_leader() {
        let rows = parse_snapshot("500 1 400 S /Users/x/devbox __collector\n");
        assert!(!led_by_daemon(&rows, 500, "__collector"));
    }

    /// A zombie is not a member and cannot lead.
    ///
    /// It stays in the process table — and in its group — until its parent
    /// waits on it, and that parent is often the very daemon being replaced.
    /// Counting one as alive made a successful takeover wait out its deadline,
    /// escalate to SIGKILL, and then report `EPERM`, which is what macOS
    /// returns for a group whose only remaining member has already exited.
    #[test]
    fn a_zombie_is_neither_a_leader_nor_a_survivor() {
        let rows = parse_snapshot(
            "100 1 100 Z /x/devbox __collector\n\
             101 100 100 S sleep 30\n",
        );
        assert!(rows[0].is_zombie());
        assert!(!rows[1].is_zombie());
        assert!(!led_by_daemon(&rows, 100, "__collector"));
        // Nor is it ever reaped: there is nothing left to signal.
        let candidates: Vec<&Process> = rows.iter().filter(|p| p.is("__collector")).collect();
        assert_eq!(candidates.len(), 1);
        let live = orphans(&rows, "__collector", Path::new("/nowhere"), &[]);
        assert!(live.is_empty());
    }

    #[test]
    fn the_marker_is_an_argument_not_a_substring() {
        let rows = parse_snapshot("7 1 7 S /opt/__collector-backup/devbox status\n");
        assert!(!rows[0].is("__collector"));
        let real = parse_snapshot("7 1 7 S /opt/x/devbox __collector\n");
        assert!(real[0].is("__collector"));
    }

    #[test]
    fn lsof_field_output_pairs_each_process_with_its_directory() {
        let found = parse_lsof("p123\nfcwd\nn/home/x/.devbox\np456\nfcwd\nn/tmp/.tmpAB/.devbox\n");
        assert_eq!(
            found,
            vec![
                (123, PathBuf::from("/home/x/.devbox")),
                (456, PathBuf::from("/tmp/.tmpAB/.devbox")),
            ]
        );
    }

    /// The reaper's whole safety argument: it may only touch daemons serving
    /// *this* state directory. A daemon of another checkout is somebody's
    /// working setup, not litter.
    #[test]
    fn a_daemon_serving_another_state_directory_is_not_an_orphan() {
        let processes = parse_snapshot(
            "100 1 100 S /x/devbox __collector\n\
             200 1 200 S /x/devbox __collector\n\
             300 1 300 S /x/devbox status\n",
        );
        let directories = [
            (100, PathBuf::from("/home/x/.devbox")),
            (200, PathBuf::from("/tmp/other/.devbox")),
        ];
        let mine = Path::new("/home/x/.devbox");

        // 100 is the live owner and is kept; 200 serves another state
        // directory; 300 is not a daemon at all.
        let candidates: Vec<&Process> = processes
            .iter()
            .filter(|p| p.is("__collector") && p.pid != 100)
            .collect();
        assert!(serving(&candidates, mine, &directories).is_empty());

        // And with the owner no longer spared, it — and only it — is selected.
        let all: Vec<&Process> = processes.iter().filter(|p| p.is("__collector")).collect();
        let found = serving(&all, mine, &directories);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 100);
    }

    /// A candidate whose working directory could not be read is left alone.
    /// "We could not tell" is not a licence to signal.
    #[test]
    fn a_process_with_no_readable_directory_is_never_selected() {
        let processes = parse_snapshot("100 1 100 S /x/devbox __collector\n");
        let candidates: Vec<&Process> = processes.iter().collect();
        assert!(serving(&candidates, Path::new("/home/x/.devbox"), &[]).is_empty());
    }

    /// A leader whose argv names a daemon, plus two children that do not.
    ///
    /// `$0` carries the marker, so `ps` shows it on the leader's command line
    /// exactly as it does for a real `devbox __collector`, while the two
    /// `sleep`s look like what a collector's exec transports look like:
    /// nothing to do with devbox.
    fn daemon_shaped_group(marker: &str, cwd: &Path) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;

        Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30 & wait", marker])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("start a group")
    }

    fn wait_for_members(pgid: i32, want: usize) -> usize {
        for _ in 0..100 {
            let found = snapshot().map(|rows| rows.iter().filter(|p| p.pgid == pgid).count());
            if found.as_ref().is_ok_and(|found| *found >= want) {
                return found.unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        snapshot().map_or(0, |rows| rows.iter().filter(|p| p.pgid == pgid).count())
    }

    /// The whole group goes, not just the process the record names.
    ///
    /// This is the defect: the takeover TERMed one pid, and a daemon's
    /// children — which is where the memory actually is — stayed.
    #[test]
    fn stopping_a_group_stops_every_member_of_it() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut leader = daemon_shaped_group("__collector", dir.path());
        let pgid = leader.id() as i32;
        assert_eq!(wait_for_members(pgid, 3), 3, "the group did not come up");

        let stopped = stop_group(pgid, "__collector", Duration::from_secs(5)).expect("stop");
        assert_eq!(stopped.signalled, 3, "{stopped:?}");
        assert_eq!(stopped.survivors, 0, "{stopped:?}");
        assert!(!stopped.escalated, "SIGTERM alone should have been enough");
        // The leader is this process's child and nothing has waited on it yet,
        // so it is sitting in the table as a zombie — which must not read as a
        // survivor.
        assert_eq!(
            snapshot()
                .unwrap()
                .iter()
                .filter(|p| p.pgid == pgid && !p.is_zombie())
                .count(),
            0,
            "something in the group outlived the stop"
        );
        let _ = leader.wait();
    }

    /// The identity check is on the leader, and it is not a formality: a live
    /// group that is not ours is refused even though its id is perfectly
    /// valid.
    #[test]
    fn a_live_group_that_is_not_ours_is_refused() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut leader = daemon_shaped_group("__something_else", dir.path());
        let pgid = leader.id() as i32;
        wait_for_members(pgid, 3);

        let error = stop_group(pgid, "__collector", Duration::from_millis(50)).unwrap_err();
        assert!(format!("{error}").contains("refusing"), "{error}");
        assert!(
            snapshot().unwrap().iter().any(|p| p.pgid == pgid),
            "a group that was refused must still be running"
        );

        let _ = signal_group(pgid, libc::SIGKILL);
        let _ = leader.wait();
    }

    /// The reaper's scope, against the real process table: it selects a daemon
    /// serving the directory it was asked about, and nothing else — not a
    /// daemon of another directory, and not a process that merely happens to
    /// be running there.
    #[test]
    fn the_reaper_selects_only_devbox_daemons_of_the_directory_it_was_given() {
        let mine = tempfile::tempdir().expect("temporary directory");
        let theirs = tempfile::tempdir().expect("temporary directory");

        let mut ours = daemon_shaped_group("__collector", mine.path());
        let mut elsewhere = daemon_shaped_group("__collector", theirs.path());
        let mut bystander = daemon_shaped_group("__not_a_daemon", mine.path());
        for group in [&ours, &elsewhere, &bystander] {
            wait_for_members(group.id() as i32, 1);
        }

        let processes = snapshot().expect("process table");
        let found = orphans(&processes, "__collector", mine.path(), &[]);
        let pids: Vec<i32> = found.iter().map(|process| process.pid).collect();
        assert_eq!(
            pids,
            vec![ours.id() as i32],
            "selected {pids:?}; ours={} elsewhere={} bystander={}",
            ours.id(),
            elsewhere.id(),
            bystander.id()
        );

        // And sparing the owner leaves nothing to reap.
        assert!(orphans(&processes, "__collector", mine.path(), &[ours.id() as i32]).is_empty());

        for group in [&mut ours, &mut elsewhere, &mut bystander] {
            let _ = signal_group(group.id() as i32, libc::SIGKILL);
            let _ = group.wait();
        }
    }

    #[test]
    fn signalling_a_group_that_would_come_back_to_us_is_refused() {
        assert!(signal_group(0, libc::SIGTERM).is_err());
        assert!(signal_group(1, libc::SIGTERM).is_err());
        assert!(signal_group(own_process_group(), libc::SIGTERM).is_err());
    }

    #[test]
    fn a_group_not_led_by_a_daemon_is_never_signalled() {
        // pid 1 leads its own group and is not a devbox daemon.
        let error = stop_group(1, "__collector", Duration::from_millis(10)).unwrap_err();
        assert!(format!("{error}").contains("refusing"), "{error}");
    }

    /// The daemon must end up leading a group of its own, whichever way it was
    /// started. The test binary is not a group leader, so this exercises the
    /// `setsid` path — and afterwards this process is its own group, which is
    /// exactly what a daemon needs before it records anything.
    #[test]
    fn a_daemon_can_always_make_a_group_of_its_own() {
        // Run in a child, because succeeding here changes this process's
        // group for every other test in the binary.
        let exe = std::env::current_exe().expect("test binary");
        let probe = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "'{}' --exact procgroup::tests::__leader_probe --nocapture --ignored",
                exe.display()
            ))
            .output()
            .expect("run the probe");
        assert!(
            String::from_utf8_lossy(&probe.stdout).contains("LEADER OK"),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );
    }

    #[test]
    #[ignore = "spawned by a_daemon_can_always_make_a_group_of_its_own"]
    fn __leader_probe() {
        let pgid = lead_own_group().expect("make a group");
        assert_eq!(pgid, std::process::id() as i32);
        assert_eq!(pgid, own_process_group());
        // Idempotent: a daemon that already leads its group gets the same id.
        assert_eq!(lead_own_group().expect("again"), pgid);
        println!("LEADER OK");
    }
}
