//! Observability — the glass box (§7).
//!
//! The agent inside a box captures kernel-level activity and streams it here;
//! this module receives it, stores it, and answers questions about it.
//!
//! - [`event`] — the canonical schema (§11.1), mirrored in `agent/event`.
//! - [`store`] — the per-box SQLite store and its query API (§7.4).
//! - [`behavior`] — the run summary and behaviour diff (§7.6).
//! - [`collector`] — the socket listener and the agent handshake (§11.3).
//! - [`health`] — per-box capture health, so an empty view can say why.
//! - [`supervisor`] — one live collector per registered box.
//! - [`correlate`] — the join that turns a wall of events into a story (§7.2).
//! - [`run`] — a bounded execution with an identity, and its attribution (§4).
//! - [`redact`] — credentials out of an argv, on the way out.

pub mod behavior;
pub mod collector;
pub mod correlate;
pub mod daemon;
pub mod event;
pub mod health;
pub mod pcap;
pub mod redact;
pub mod run;
pub mod store;
pub mod supervisor;

pub use event::{Event, EventType};
pub use run::{Attribution, RunKind, RunRecord, RunStatus};
pub use store::{Query, Retention, Store};

/// Reads back what the daemon would have written to its log.
///
/// The observability daemon reports itself in `tracing` lines and nothing
/// else, so for a few facts — an agent's stream ended, a collector is being
/// restarted — the log line *is* the feature. A line nothing asserts on is
/// one `git checkout --` away from being gone with every test still green,
/// which is the same silence this whole area keeps having to fix.
///
/// The sink is global and the buffer is per-thread, which is not the obvious
/// arrangement — a thread-local subscriber would be. That does not work:
/// `tracing` decides once, process-wide, whether a call site is worth
/// evaluating, and it decides it on whichever thread reaches the site first.
/// Under `cargo test` that is routinely some other test running in parallel
/// with no subscriber installed, and the answer it caches is "nobody is
/// listening" — after which the line is silently skipped on the very thread
/// that installed a listener. Installing globally, once, makes the answer
/// "somebody is listening" for every thread; the per-thread buffer then keeps
/// each test reading only its own output.
#[cfg(test)]
pub(crate) mod testlog {
    use std::cell::RefCell;
    use std::io::Write;

    thread_local! {
        static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// This thread's share of the daemon's log.
    pub(crate) struct Captured;

    impl Captured {
        /// Start collecting this thread's log lines, discarding anything the
        /// thread logged earlier.
        pub(crate) fn install() -> Self {
            static INSTALLED: std::sync::Once = std::sync::Once::new();
            INSTALLED.call_once(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(ToCallingThread)
                    .with_max_level(tracing::Level::INFO)
                    // Plain text: colour codes land between a field's name and
                    // its value, so an assertion on `box_id=alpha` fails
                    // against a line that says exactly that.
                    .with_ansi(false)
                    .without_time()
                    .finish();
                let _ = tracing::subscriber::set_global_default(subscriber);
            });
            BUFFER.with(|buffer| buffer.borrow_mut().clear());
            Self
        }

        /// The one line carrying `needle`, or a panic showing everything that
        /// was logged instead — a missing line is otherwise the least
        /// informative failure there is.
        pub(crate) fn line(&self, needle: &str) -> String {
            let text = BUFFER.with(|buffer| String::from_utf8_lossy(&buffer.borrow()).into_owned());
            match text.lines().find(|line| line.contains(needle)) {
                Some(line) => line.to_string(),
                None => panic!("nothing logged {needle:?}; the log held:\n{text}"),
            }
        }
    }

    struct ToCallingThread;

    impl Write for ToCallingThread {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            BUFFER.with(|buffer| buffer.borrow_mut().extend_from_slice(buf));
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for ToCallingThread {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            Self
        }
    }
}

/// Whether a box can connect to a host-owned Unix-domain socket through a
/// bind mount.
///
/// Only native Linux Docker shares the host kernel. Docker Desktop runs the
/// container in a Linux VM, where a socket inode exposed by virtiofs is data,
/// not a connectable endpoint. Those boxes use the runtime's authenticated
/// exec stdio transport just like Lima, Multipass and Incus.
pub(crate) fn uses_host_socket(runtime: &str) -> bool {
    cfg!(target_os = "linux") && runtime == "docker"
}

/// Whether this runtime may attach the embedded eBPF probes.
///
/// The programs deliberately trace the one box's whole kernel. That is
/// containment only for dedicated VM kernels; Docker shares its kernel with
/// other containers and must always use proc+packet capture.
pub(crate) fn uses_ebpf(runtime: &str) -> bool {
    crate::embedded::obsd_has_ebpf() && runtime != "docker"
}

/// Whether this box has a run the collector is still attributing to.
///
/// The question both the daemon handover and the agent refresh have to ask
/// before they stop anything: the stdio agent is a run's only route to the
/// host, so restarting it mid-run drops whatever had not yet been delivered.
///
/// `status = 'running'` is the flag, and `ended_at IS NULL` is the check on
/// it. `finish_run` writes both in one statement so they normally agree, and
/// this refuses to take a half-written row as evidence of a live run — a
/// stuck `running` with an end time would otherwise defer every handover and
/// every agent update on that box forever.
pub fn run_in_flight(state_dir: &std::path::Path, name: &str) -> bool {
    if !crate::sandbox::state::is_safe_name(name) {
        return false;
    }
    let path = collector::store_path(state_dir, name);
    if !path.exists() {
        return false;
    }
    let Ok(store) = store::Store::open(&path) else {
        // Unreadable is not evidence of a run. Treating it as one would make a
        // damaged store into a permanent block on updating that box's agent.
        return false;
    };
    store.active_runs().is_ok_and(|runs| {
        runs.iter()
            .any(|run| run.ended_at.as_ref().is_none_or(|ended| ended.is_empty()))
    })
}

/// The same question for the whole host: which box, if any, has a run in
/// flight.
///
/// The collector handover is host-wide — it ends one daemon and every stdio
/// agent that is its child — so it has to ask about every box, not just one.
pub fn any_run_in_flight(state_dir: &std::path::Path) -> Option<String> {
    let boxes = std::fs::read_dir(state_dir.join("boxes")).ok()?;
    for entry in boxes.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if run_in_flight(state_dir, &name) {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod run_flight_tests {
    use super::*;
    use crate::obs::run::{RunKind, RunRecord, RunStatus};

    /// A state directory with one box whose store holds `run`.
    fn state_with(run: RunRecord) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary state directory");
        let path = collector::store_path(dir.path(), &run.box_id);
        std::fs::create_dir_all(path.parent().unwrap()).expect("box directory");
        let store = store::Store::open(&path).expect("store");
        store.insert_run(&run).expect("insert the run");
        dir
    }

    fn running() -> RunRecord {
        RunRecord {
            run_id: "r1".into(),
            box_id: "alpha".into(),
            kind: RunKind::Run.as_str().to_string(),
            started_at: "2026-09-06T00:00:00.000Z".into(),
            ended_at: None,
            status: RunStatus::Running.as_str().to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_running_run_is_in_flight() {
        let dir = state_with(running());
        assert!(run_in_flight(dir.path(), "alpha"));
        assert_eq!(any_run_in_flight(dir.path()).as_deref(), Some("alpha"));
    }

    #[test]
    fn a_finished_run_is_not() {
        let dir = state_with(RunRecord {
            ended_at: Some("2026-09-06T00:00:01.000Z".into()),
            status: RunStatus::Finished.as_str().to_string(),
            ..running()
        });
        assert!(!run_in_flight(dir.path(), "alpha"));
        assert_eq!(any_run_in_flight(dir.path()), None);
    }

    /// The half-written row: `status` still says running, but the end time is
    /// there. `finish_run` writes both in one statement, so this can only come
    /// from something that went wrong — and taking it as a live run would
    /// defer every handover and every agent update on that box forever.
    #[test]
    fn a_row_that_says_running_but_has_an_end_time_is_not_in_flight() {
        let dir = state_with(RunRecord {
            ended_at: Some("2026-09-06T00:00:01.000Z".into()),
            ..running()
        });
        assert!(!run_in_flight(dir.path(), "alpha"));
    }

    #[test]
    fn a_box_with_no_store_and_an_unsafe_name_are_both_quiet() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!run_in_flight(dir.path(), "never-seen"));
        assert!(!run_in_flight(dir.path(), "../escape"));
        assert_eq!(any_run_in_flight(dir.path()), None);
    }

    /// One box's run does not hold up another box's agent, but it does hold up
    /// the host-wide handover — which ends every box's agent at once.
    #[test]
    fn one_boxs_run_is_the_hosts_business_but_not_another_boxs() {
        let dir = state_with(running());
        assert!(run_in_flight(dir.path(), "alpha"));
        assert!(!run_in_flight(dir.path(), "beta"));
        assert!(any_run_in_flight(dir.path()).is_some());
    }
}

#[cfg(test)]
mod transport_tests {
    #[test]
    fn only_native_linux_docker_uses_a_host_socket() {
        assert_eq!(super::uses_host_socket("docker"), cfg!(target_os = "linux"));
        for runtime in ["lima", "multipass", "incus"] {
            assert!(!super::uses_host_socket(runtime));
        }
    }

    #[test]
    fn shared_kernel_docker_never_uses_ebpf() {
        assert!(!super::uses_ebpf("docker"));
        for runtime in ["lima", "multipass", "incus"] {
            assert_eq!(super::uses_ebpf(runtime), crate::embedded::obsd_has_ebpf());
        }
    }
}
