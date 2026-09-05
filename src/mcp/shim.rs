//! The stdio shim: an MCP server that runs somewhere else.
//!
//! The agent launches `devbox mcp run <name>` and talks to it exactly as it
//! would talk to the server itself. This module is the part in the middle: it
//! spawns the host-side argv a runtime gives for "run this in the box"
//! ([`crate::runtime::Runtime::argv`]), and carries bytes between the agent's
//! pipes and the guest process's pipes.
//!
//! Three things it deliberately does not do:
//!
//! - **Parse.** JSON-RPC framing belongs to the two endpoints. Buffers are
//!   copied whole; a message containing CRLF, a NUL, or invalid UTF-8 is not
//!   this layer's business. (`exec_in_sandbox` cannot be used for the same
//!   reason: it captures output into a `String` through `from_utf8_lossy`, and
//!   feeds the guest `/dev/null` for stdin.)
//! - **Buffer.** Every chunk is flushed on arrival. A request that sits in a
//!   buffer waiting for a newline is a request the agent thinks timed out.
//! - **Trust the transport to clean up.** `limactl shell` is `ssh` with no tty,
//!   and a channel that closes delivers no SIGHUP: measured on a Lima box, a
//!   guest process that ignores stdin outlives both its `ssh` and its
//!   `limactl`. The guest command is therefore wrapped so it records its
//!   process group id in the box ([`wrap_guest_command`]), and a shim that has
//!   to force its transport down asks the box to kill that group
//!   ([`reaper_script`]).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

/// Copy buffer. Large enough that a big MCP payload is a handful of writes,
/// small enough that a small one is not delayed behind a full read.
const COPY_BUFFER: usize = 64 * 1024;

/// How long the output tasks get to finish after the transport has ended.
///
/// On a clean exit they finish at once: the pipes close and both reads return
/// EOF. It is a forced shutdown that needs a bound — see the `join!` in
/// [`pump`] for whose descriptor it is that never closes.
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Longest stderr line written to the log with its own timestamp. A server
/// that emits a megabyte without a newline gets it in pieces rather than
/// buffered in this process until it stops.
const MAX_LOG_LINE: usize = 64 * 1024;

/// How the shim should behave for one run.
#[derive(Debug, Clone)]
pub struct ShimOptions {
    /// Where the guest's stderr goes, one timestamped line at a time.
    pub log_path: PathBuf,
    /// Also copy those lines to this process's stderr. `mcp run` sets it when
    /// stderr is a terminal — a human typed the command and would otherwise
    /// watch a silent hang.
    pub mirror_stderr: bool,
    /// How long the guest process gets to exit after its stdin reaches EOF.
    /// Closing stdin *is* the MCP shutdown signal, so this is a grace period,
    /// not a policy.
    pub eof_grace: Duration,
    /// How long it gets after the shim is asked to terminate.
    pub signal_grace: Duration,
}

impl ShimOptions {
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            log_path,
            mirror_stderr: false,
            eof_grace: Duration::from_secs(5),
            signal_grace: Duration::from_secs(5),
        }
    }
}

/// What happened to the guest process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimOutcome {
    /// The exit code to pass to the agent. A transport killed by signal `n`
    /// reports `128 + n`, the shell convention.
    pub exit_code: i32,
    /// The shim had to kill the transport rather than watch it exit. The guest
    /// process may still be alive, which is what [`reaper_script`] is for.
    pub forced: bool,
    /// Who decided the session was over.
    ///
    /// The exit code cannot say this: a server that shut down politely on EOF
    /// and one the shim had to SIGKILL both report 143. For a run that lasted
    /// hours, which of the two happened is the interesting half.
    pub stopped_by: Stop,
}

/// Why the shim stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest process exited on its own.
    Exited,
    /// The agent closed its end of stdin — the MCP shutdown handshake.
    HostEof,
    /// The shim was asked to terminate.
    Terminated,
}

impl Stop {
    /// How this end is recorded on the run (§4.6), given whether the shim had
    /// to kill the transport to make it happen.
    pub fn ended_by(self, forced: bool) -> crate::obs::run::EndedBy {
        use crate::obs::run::EndedBy;
        match (self, forced) {
            (Stop::Exited, _) => EndedBy::Exit,
            (_, true) => EndedBy::Forced,
            (Stop::HostEof, false) => EndedBy::StdinEof,
            (Stop::Terminated, false) => EndedBy::Signal,
        }
    }
}

/// The guest-side wrapper.
///
/// `sh -c SCRIPT devbox-mcp <pgid-file> <argv...>`: record the process group
/// the box gave this ssh channel, then `exec` the real command into it, so the
/// pid that ends up running the server is the one the file describes and the
/// group covers everything the server itself spawns (`uvx` → `python`, `npx` →
/// `node`).
///
/// Recording is best effort — a guest without `/proc` writes nothing and the
/// shim falls back to closing the transport, which is enough for any server
/// that reads its stdin.
const GUEST_WRAPPER: &str = "read -r _ _ _ _ g _ < /proc/self/stat 2>/dev/null && \
printf '%s' \"$g\" > \"$1\" 2>/dev/null; shift; exec \"$@\"";

/// Kill the process group recorded by [`GUEST_WRAPPER`], then remove the file.
///
/// One line, like the wrapper, because both of these are `exec`ed inside the
/// box and therefore appear verbatim in `devbox watch --type exec`. A
/// ten-line script renders as ten lines of shell in the audit of every MCP
/// session, which buries the events the audit is for.
///
/// TERM first, KILL only if the group is still there five seconds later: an
/// MCP server that is mid-write to a file it owns deserves the same courtesy
/// as any other process.
const GUEST_REAPER: &str = "p=$(cat \"$1\" 2>/dev/null); rm -f \"$1\" 2>/dev/null; \
case \"$p\" in ''|*[!0-9]*) exit 0;; esac; kill -0 -$p 2>/dev/null || exit 0; \
kill -TERM -$p 2>/dev/null; i=0; \
while [ $i -lt 25 ] && kill -0 -$p 2>/dev/null; do sleep 0.2; i=$((i+1)); done; \
kill -0 -$p 2>/dev/null && kill -KILL -$p 2>/dev/null; exit 0";

/// `$0` for the wrapper's shell, and for the reaper's, so `ps` says what they
/// are — and so [`is_wrapper_command`] can recognise them without matching on
/// the script text, which changes.
pub const WRAPPER_ARGV0: &str = "devbox-mcp";
pub const REAPER_ARGV0: &str = "devbox-mcp-reap";

/// Where the process-group file lives inside the guest.
pub const PGID_PATH_PREFIX: &str = "/tmp/devbox-mcp-";

/// A guest path for the process-group file, unique per shim invocation.
pub fn pgid_file_path(name: &str) -> String {
    format!(
        "{PGID_PATH_PREFIX}{name}-{:016x}.pgid",
        rand::random::<u64>()
    )
}

/// Whether this argv is the MCP shim's own plumbing rather than the server.
///
/// The same job [`crate::obs::run::is_wrapper_command`] does for `devbox run`,
/// and for the same reason: an MCP session's report should open on the server
/// the user registered, not on the `read -r _ _ _ _ g _ < /proc/self/stat`
/// that put it in a killable process group.
pub fn is_wrapper_command(argv: &[String]) -> bool {
    let word = |i: usize| argv.get(i).map(String::as_str).unwrap_or_default();
    if word(0) == WRAPPER_ARGV0 || word(0) == REAPER_ARGV0 {
        return true;
    }
    // `sh -c <script> devbox-mcp …`: the shell that runs it has the argv0 at
    // index 3, and the scripts themselves are the other half of the pair.
    if word(3) == WRAPPER_ARGV0 || word(3) == REAPER_ARGV0 {
        return true;
    }
    argv.iter()
        .any(|a| a == GUEST_WRAPPER || a == GUEST_REAPER || a.starts_with(PGID_PATH_PREFIX))
}

/// The argv to hand [`crate::runtime::Runtime::argv`] for a registered server.
///
/// `env` is applied with `env(1)` rather than a shell `export`, because the
/// command is an argv vector and must stay one: a value with a space, a quote,
/// or a `$` in it has no business being re-parsed by a shell.
pub fn wrap_guest_command<'a, E>(pgid_file: &str, env: E, command: &[String]) -> Vec<String>
where
    E: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        GUEST_WRAPPER.to_string(),
        WRAPPER_ARGV0.to_string(),
        pgid_file.to_string(),
    ];
    let assignments: Vec<String> = env
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !assignments.is_empty() {
        argv.push("env".to_string());
        argv.extend(assignments);
    }
    argv.extend(command.iter().cloned());
    argv
}

/// The argv that cleans up after a shim that had to force its transport down.
pub fn reaper_script(pgid_file: &str) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        GUEST_REAPER.to_string(),
        REAPER_ARGV0.to_string(),
        pgid_file.to_string(),
    ]
}

/// Run `argv`, carrying `input` and `output` to and from it.
///
/// `shutdown` is the request to stop — SIGTERM in production, a channel in
/// tests. It is a parameter because the signal path has to be exercised, and a
/// test that raises a real SIGTERM at its own process is a test that kills the
/// harness when it regresses.
pub async fn pump<I, O, S>(
    argv: &[String],
    input: I,
    output: O,
    options: &ShimOptions,
    shutdown: S,
) -> Result<ShimOutcome>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    S: std::future::Future<Output = ()> + Send,
{
    let (program, rest) = argv
        .split_first()
        .context("the runtime produced an empty command line")?;

    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, for two reasons. A Ctrl-C in the terminal
        // that launched the agent must not race the shim's own shutdown; and
        // the transport is `limactl` with an `ssh` child, so the thing that
        // has to be killed is a group, not a process. Killing `limactl` alone
        // leaves the `ssh` holding the channel open and the guest process
        // alive behind it — measured, not assumed.
        .process_group(0)
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start '{program}' to reach the box"))?;
    let pid = child.id().map(|id| id as i32);

    let child_stdin = child.stdin.take().context("child stdin was not piped")?;
    let child_stdout = child.stdout.take().context("child stdout was not piped")?;
    let child_stderr = child.stderr.take().context("child stderr was not piped")?;

    // EOF on the agent's stdin is the MCP shutdown handshake. The task that
    // notices it is also the task that propagates it, by dropping the guest's
    // stdin, so the two cannot get out of step.
    let (eof_tx, eof_rx) = oneshot::channel::<()>();
    let stdin_task = tokio::spawn(async move {
        let _ = copy_flushing(input, child_stdin).await;
        let _ = eof_tx.send(());
    });
    let stdout_task = tokio::spawn(async move {
        let _ = copy_flushing(child_stdout, output).await;
    });
    let stderr_task = tokio::spawn(log_stderr(
        child_stderr,
        options.log_path.clone(),
        options.mirror_stderr,
    ));

    let stop = tokio::select! {
        biased;
        _ = child.wait() => Stop::Exited,
        _ = shutdown => Stop::Terminated,
        _ = eof_rx => Stop::HostEof,
    };

    let (status, forced) = match stop {
        // `wait` in the select above already reaped it; ask again for the
        // status, which `tokio` caches.
        Stop::Exited => (child.wait().await.ok(), false),
        Stop::HostEof => settle(&mut child, pid, options.eof_grace).await,
        Stop::Terminated => settle(&mut child, pid, options.signal_grace).await,
    };

    // Output first: the last bytes the guest wrote are the response the agent
    // is still waiting for, and they are only in flight until this returns.
    //
    // Bounded, and the bound is not paranoia. `limactl shell` reaches the box
    // through `ssh -o ControlMaster=auto -o ControlPersist=yes`, and the mux
    // master takes the client's stdio descriptors by fd-passing. Kill the ssh
    // client while the *guest* process is still running and the master holds
    // the write end of these pipes open: a persistent process that is `ppid 1`
    // in a process group of its own, shared with every other devbox command,
    // and therefore neither ours to signal nor going to close them. Waiting
    // for EOF on a pipe in that state is waiting forever, which is exactly
    // what a first cut of this function did — the shim stayed alive with a
    // dead transport, and the guest process it was supposed to clean up
    // outlived it because the cleanup is on the other side of this await.
    tokio::join!(
        drain_bounded(stdout_task, DRAIN_BUDGET),
        drain_bounded(stderr_task, DRAIN_BUDGET),
    );
    stdin_task.abort();

    Ok(ShimOutcome {
        exit_code: exit_code(status),
        forced,
        stopped_by: stop,
    })
}

/// Wait for a copy task to finish, then stop waiting.
///
/// Aborting rather than detaching: the task is parked on a read that will
/// never complete, and a detached task keeps its pipe — and the runtime's
/// interest in it — alive for as long as the process is.
async fn drain_bounded(task: tokio::task::JoinHandle<()>, budget: Duration) {
    let mut task = task;
    if tokio::time::timeout(budget, &mut task).await.is_err() {
        task.abort();
    }
}

/// Wait `grace` for the transport to end on its own, then take it down.
async fn settle(
    child: &mut tokio::process::Child,
    pid: Option<i32>,
    grace: Duration,
) -> (Option<std::process::ExitStatus>, bool) {
    if let Ok(status) = tokio::time::timeout(grace, child.wait()).await {
        return (status.ok(), false);
    }
    if let Some(pid) = pid {
        signal_group(pid, "TERM");
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            return (status.ok(), true);
        }
        signal_group(pid, "KILL");
    }
    let _ = child.start_kill();
    (child.wait().await.ok(), true)
}

/// Signal the child's whole process group.
///
/// Through `kill(1)` rather than `killpg(2)`: devbox does not depend on
/// `libc`, and this is the shutdown path — one bounded subprocess, on a code
/// path that runs once per MCP session. If a future change brings `libc` in
/// for other reasons, this is the first thing that should use it.
fn signal_group(pid: i32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The code the agent sees.
///
/// An unknown status is 1, not 0: "the server is gone and we cannot say why"
/// must not read as success.
fn exit_code(status: Option<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(status) => status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1),
        None => 1,
    }
}

/// Copy every byte, flushing each chunk.
///
/// A closed destination is the normal end of a stdio session, not a failure:
/// the agent goes away and the write to the guest fails, or the guest exits
/// and the write to the agent does. Both mean "stop copying".
async fn copy_flushing<R, W>(mut reader: R, mut writer: W) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if writer.write_all(&buffer[..read]).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
        total += read as u64;
    }
    let _ = writer.shutdown().await;
    Ok(total)
}

/// Append the guest's stderr to the log, one timestamped line at a time.
async fn log_stderr<R>(reader: R, path: PathBuf, mirror: bool)
where
    R: AsyncRead + Unpin,
{
    let mut sink = match open_log(&path) {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!("devbox mcp: cannot write {}: {error:#}", path.display());
            None
        }
    };
    let mut reader = BufReader::new(reader);
    // Lines are assembled here rather than with `read_until`, so that a server
    // emitting a megabyte without a newline is written out in pieces instead
    // of buffered in this process until it stops.
    let mut line: Vec<u8> = Vec::with_capacity(1024);
    loop {
        let chunk = match reader.fill_buf().await {
            Ok([]) => break,
            Ok(chunk) => chunk,
            Err(_) => break,
        };
        let (taken, complete) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (chunk.len(), false),
        };
        line.extend_from_slice(&chunk[..taken]);
        reader.consume(taken);
        if complete || line.len() >= MAX_LOG_LINE {
            write_log_line(&mut sink, mirror, &line);
            line.clear();
        }
    }
    // Whatever the server wrote without a final newline is still something it
    // said, and a crash message is exactly the kind of line that lacks one.
    if !line.is_empty() {
        write_log_line(&mut sink, mirror, &line);
    }
}

fn write_log_line(sink: &mut Option<std::fs::File>, mirror: bool, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    let stamped = format!("{} {}\n", timestamp(), text.trim_end_matches(['\n', '\r']));
    if let Some(file) = sink.as_mut() {
        use std::io::Write as _;
        if file.write_all(stamped.as_bytes()).is_err() {
            *sink = None;
        }
    }
    if mirror {
        eprint!("{stamped}");
    }
}

fn open_log(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// SIGTERM or SIGINT, whichever comes first — the production `shutdown`.
pub async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(handler) => handler,
        Err(error) => {
            eprintln!("devbox mcp: cannot install a SIGTERM handler: {error}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = terminate.recv() => {}
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                eprintln!("devbox mcp: cannot install a Ctrl-C handler: {error}");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Whether a human is watching this shim's stderr.
pub fn stderr_is_a_terminal() -> bool {
    std::io::stderr().is_terminal()
}

/// The last `lines` lines of a log, for an error message that would otherwise
/// send the user to a file to find out what happened.
pub fn log_tail(path: &Path, lines: usize) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let all: Vec<&str> = text.lines().collect();
    Ok(all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|s| s.to_string())
        .collect())
}

/// Refuse an argv that is not a command.
pub fn validate_command(command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!(
            "no command given; write it after `--`, e.g. `devbox mcp add fetch -- uvx mcp-server-fetch`"
        );
    }
    if command[0].trim().is_empty() {
        bail!("the command's program name is empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn options(dir: &tempfile::TempDir) -> ShimOptions {
        ShimOptions::new(dir.path().join("mcp").join("t.log"))
    }

    fn argv(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    /// Bytes in, the same bytes out — and nothing about them is inspected.
    #[tokio::test]
    async fn a_hostile_payload_survives_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // CRLF, NUL, a lone CR, invalid UTF-8, and a ^D for good measure.
        let mut payload = Vec::new();
        for i in 0..8192u32 {
            payload.extend_from_slice(b"\r\n\x00\xff\x1a\x04\x7f\x80\xc3\x28");
            payload.extend_from_slice(&i.to_le_bytes());
        }
        let expected = payload.clone();

        let (mut host, agent) = duplex(1 << 16);
        let (mut sink_read, sink_write) = duplex(1 << 16);

        let pump = tokio::spawn(async move {
            pump(
                &argv("cat"),
                agent,
                sink_write,
                &ShimOptions::new(dir.path().join("t.log")),
                std::future::pending::<()>(),
            )
            .await
        });

        let writer = tokio::spawn(async move {
            host.write_all(&payload).await.unwrap();
            host.shutdown().await.unwrap();
        });

        let mut received = Vec::new();
        sink_read.read_to_end(&mut received).await.unwrap();
        writer.await.unwrap();
        let outcome = pump.await.unwrap().unwrap();

        assert_eq!(received.len(), expected.len());
        assert_eq!(received, expected, "the shim altered the byte stream");
        assert_eq!(outcome.exit_code, 0);
        assert!(!outcome.forced);
    }

    #[tokio::test]
    async fn the_exit_code_is_the_guest_process_s_own() {
        for code in [0, 1, 7, 42, 127] {
            let dir = tempfile::tempdir().unwrap();
            let (_host, agent) = duplex(64);
            let (_out, sink) = duplex(64);
            let outcome = pump(
                &argv(&format!("exit {code}")),
                agent,
                sink,
                &options(&dir),
                std::future::pending::<()>(),
            )
            .await
            .unwrap();
            assert_eq!(
                outcome.exit_code, code,
                "exit {code} was not passed through"
            );
        }
    }

    #[tokio::test]
    async fn a_signalled_guest_reports_the_shell_convention() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, agent) = duplex(64);
        let (_out, sink) = duplex(64);
        let outcome = pump(
            &argv("kill -TERM $$"),
            agent,
            sink,
            &options(&dir),
            std::future::pending::<()>(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 143);
    }

    #[tokio::test]
    async fn stderr_is_logged_with_a_timestamp_and_never_reaches_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(&dir);
        let (_host, agent) = duplex(64);
        let (mut out_read, sink) = duplex(1 << 16);

        let outcome = pump(
            &argv("echo to-stdout; echo to-stderr >&2; echo second-line >&2"),
            agent,
            sink,
            &options,
            std::future::pending::<()>(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 0);

        let mut stdout = Vec::new();
        out_read.read_to_end(&mut stdout).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&stdout), "to-stdout\n");

        let log = std::fs::read_to_string(&options.log_path).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].ends_with(" to-stderr"), "{log}");
        assert!(lines[1].ends_with(" second-line"), "{log}");
        for line in &lines {
            let stamp = line.split(' ').next().unwrap();
            chrono::DateTime::parse_from_rfc3339(stamp)
                .unwrap_or_else(|e| panic!("{stamp:?} is not a timestamp: {e}"));
        }
    }

    /// A second run appends; the log is a record, not a scratch file.
    #[tokio::test]
    async fn the_log_is_appended_to_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(&dir);
        for message in ["first", "second"] {
            let (_host, agent) = duplex(64);
            let (_out, sink) = duplex(1 << 16);
            pump(
                &argv(&format!("echo {message} >&2")),
                agent,
                sink,
                &options,
                std::future::pending::<()>(),
            )
            .await
            .unwrap();
        }
        let log = std::fs::read_to_string(&options.log_path).unwrap();
        assert!(log.contains("first") && log.contains("second"), "{log}");
    }

    /// Closing the agent's stdin is the MCP shutdown handshake, and a server
    /// that honours it must not be waited on forever.
    #[tokio::test]
    async fn host_eof_ends_a_server_that_reads_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let (host, agent) = duplex(64);
        let (_out, sink) = duplex(1 << 16);
        drop(host);

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            pump(
                &argv("cat"),
                agent,
                sink,
                &options(&dir),
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("EOF must end the run")
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert!(
            !outcome.forced,
            "a server that exited on its own was not forced"
        );
    }

    /// ...and one that ignores it is killed rather than hung on.
    #[tokio::test]
    async fn host_eof_forces_a_server_that_ignores_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(&dir);
        options.eof_grace = Duration::from_millis(200);

        let (host, agent) = duplex(64);
        let (_out, sink) = duplex(1 << 16);
        drop(host);

        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            pump(
                &argv("exec sleep 120"),
                agent,
                sink,
                &options,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("a deaf server must still be stopped")
        .unwrap();
        assert!(outcome.forced, "the shim reported a clean exit for a kill");
    }

    #[tokio::test]
    async fn a_termination_request_stops_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(&dir);
        options.signal_grace = Duration::from_millis(200);

        // Held open so stdin never reaches EOF: the only reason this run ends
        // is the shutdown future.
        let (_host, agent) = duplex(64);
        let (_out, sink) = duplex(1 << 16);
        let (tx, rx) = oneshot::channel::<()>();

        let run = tokio::spawn(async move {
            pump(&argv("exec sleep 120"), agent, sink, &options, async move {
                let _ = rx.await;
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(()).unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("SIGTERM must stop the run")
            .unwrap()
            .unwrap();
        assert!(outcome.forced);
    }

    #[tokio::test]
    async fn a_transport_that_cannot_start_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, agent) = duplex(64);
        let (_out, sink) = duplex(64);
        let error = pump(
            &["devbox-no-such-runtime-binary".to_string()],
            agent,
            sink,
            &options(&dir),
            std::future::pending::<()>(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("to reach the box"), "{error}");
    }

    #[test]
    fn the_wrapper_execs_the_command_and_keeps_its_argv_intact() {
        let command = vec!["uvx".to_string(), "mcp-server-fetch".to_string()];
        let env = std::collections::BTreeMap::new();
        let argv = wrap_guest_command("/tmp/x.pgid", env.iter(), &command);
        assert_eq!(&argv[..2], &["sh".to_string(), "-c".to_string()]);
        assert_eq!(argv[3], "devbox-mcp");
        assert_eq!(argv[4], "/tmp/x.pgid");
        assert_eq!(&argv[5..], &command[..]);
        assert!(argv[2].contains("exec \"$@\""), "{}", argv[2]);
    }

    /// Both guest snippets are `exec`ed inside the box, so they are printed
    /// verbatim by `devbox watch --type exec`. One line each, or every MCP
    /// session pushes the events the audit is for off the screen.
    #[test]
    fn the_guest_snippets_are_one_line_each() {
        for (what, script) in [("wrapper", GUEST_WRAPPER), ("reaper", GUEST_REAPER)] {
            assert!(
                !script.contains('\n'),
                "the guest {what} spans lines and will do so in every audit:\n{script}"
            );
        }
    }

    #[test]
    fn env_is_applied_with_env_1_so_values_are_never_reparsed() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "A".to_string(),
            "a value with $HOME and 'quotes'".to_string(),
        );
        let argv = wrap_guest_command("/tmp/x.pgid", env.iter(), &["srv".to_string()]);
        assert_eq!(argv[5], "env");
        assert_eq!(argv[6], "A=a value with $HOME and 'quotes'");
        assert_eq!(argv[7], "srv");
    }

    /// The wrapper and the reaper agree on where the process group id lives,
    /// and the pair works against a real process group.
    #[tokio::test]
    async fn the_wrapper_records_a_group_the_reaper_can_kill() {
        if !Path::new("/proc/self/stat").exists() {
            // The wrapper reads Linux `/proc`; every devbox guest is Linux,
            // but the host running this test need not be.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pgid_file = dir.path().join("run.pgid");
        let argv = wrap_guest_command(
            pgid_file.to_str().unwrap(),
            std::collections::BTreeMap::new().iter(),
            &["sleep".to_string(), "120".to_string()],
        );
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        for _ in 0..100 {
            if pgid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recorded = std::fs::read_to_string(&pgid_file).unwrap();
        assert_eq!(
            recorded.trim().parse::<i32>().unwrap(),
            child.id().unwrap() as i32,
            "the wrapper recorded a group that is not the child's"
        );

        let reaper = reaper_script(pgid_file.to_str().unwrap());
        let status = Command::new(&reaper[0])
            .args(&reaper[1..])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert!(!pgid_file.exists(), "the reaper left its file behind");
        let exited = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("the reaper did not kill the group");
        assert!(!exited.unwrap().success());
    }

    /// The regression for the hang this function was written to prevent.
    ///
    /// A copy task parked on a descriptor nobody will ever close is not a
    /// hypothetical: `ssh`'s `ControlPersist` master holds the transport's
    /// stderr write end after the client is killed, and it is `ppid 1` in a
    /// process group of its own. Waiting on that task is waiting forever, and
    /// everything that cleans up the guest is on the other side of the wait.
    #[tokio::test]
    async fn a_copy_task_that_will_never_finish_does_not_hold_the_shim_open() {
        let never = tokio::spawn(async { std::future::pending::<()>().await });
        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            drain_bounded(never, Duration::from_millis(200)),
        )
        .await
        .expect("the drain must give up");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the drain waited {:?} on a task that never finishes",
            started.elapsed()
        );
    }

    /// ...while a task that is merely slow is still allowed to finish, because
    /// the last bytes it is carrying are the agent's answer.
    #[tokio::test]
    async fn a_slow_copy_task_is_waited_for() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let slow = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        drain_bounded(slow, Duration::from_secs(10)).await;
        assert!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            "the drain gave up on a task that was about to finish"
        );
    }

    #[test]
    fn a_command_has_to_be_a_command() {
        assert!(validate_command(&[]).is_err());
        assert!(validate_command(&["  ".to_string()]).is_err());
        validate_command(&["uvx".to_string()]).unwrap();
    }
}
