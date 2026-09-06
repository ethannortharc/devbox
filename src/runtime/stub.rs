//! One scripted [`Runtime`] for tests, in place of a hand-written double per
//! test module.
//!
//! Every real implementation of [`Runtime`] shells out to a hypervisor, so the
//! code above the trait can only be tested against a double. Five of those had
//! been written by hand, and each paid the same tax: a dozen `unimplemented!()`
//! arms for methods the test never calls, re-typed in five places whenever the
//! trait grew a method, and no two of them recording calls the same way.
//!
//! [`StubRuntime`] is that double, once. Nothing is scripted by default, and
//! an unscripted method panics with its own name in the message. It panics
//! rather than returning an error on purpose: reaching a call the test did not
//! think about is a fact about the test, and an error can be swallowed by the
//! code under test and never reported — which is exactly the semantics the
//! `unimplemented!()` arms had. What changes is only that the message now says
//! *which* call it was, instead of leaving the reader to find it. Every call is
//! recorded, so the ordering assertions the doubles existed for come from
//! [`StubRuntime::calls`] and [`StubRuntime::exec_commands`] instead of from a
//! `Mutex<Vec<String>>` re-invented per module.
//!
//! It is compiled into the library rather than gated behind `#[cfg(test)]`
//! because `tests/mcp.rs` needs it too, and an integration test links against
//! the library's public API only. That is the same reason this crate is a
//! library plus a thin binary at all — see the crate docs.
//!
//! ```no_run
//! # use devbox::runtime::stub::StubRuntime;
//! # use devbox::runtime::{ExecResult, Runtime, SandboxStatus};
//! let guest = StubRuntime::new()
//!     .with_name("flaky")
//!     .with_status(SandboxStatus::Running)
//!     .with_exec_cmd(|_: &str, cmd: &[&str], _: bool| {
//!         Ok(ExecResult {
//!             exit_code: i32::from(cmd.first() == Some(&"cat")),
//!             stdout: String::new(),
//!             stderr: String::new(),
//!         })
//!     });
//! # let _ = guest.called("exec_cmd");
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use super::{
    CreateOpts, ExecResult, Mount, MountUpdate, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo,
};

/// One call made against a [`StubRuntime`], recorded in the order it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Create(String),
    Start(String),
    Stop(String),
    Destroy(String),
    Status(String),
    Exec {
        name: String,
        cmd: Vec<String>,
        interactive: bool,
    },
    Argv {
        name: String,
        cmd: Vec<String>,
        interactive: bool,
    },
    CopyFrom {
        name: String,
        guest_path: String,
        host_path: PathBuf,
    },
}

impl Call {
    /// The trait method this call came from, spelled the way
    /// [`StubRuntime::called`] wants it.
    pub fn method(&self) -> &'static str {
        match self {
            Self::Create(_) => "create",
            Self::Start(_) => "start",
            Self::Stop(_) => "stop",
            Self::Destroy(_) => "destroy",
            Self::Status(_) => "status",
            Self::Exec { .. } => "exec_cmd",
            Self::Argv { .. } => "argv",
            Self::CopyFrom { .. } => "copy_from",
        }
    }
}

/// Every method [`StubRuntime`] records, and the only names
/// [`StubRuntime::called`] accepts.
const RECORDED: &[&str] = &[
    "create",
    "start",
    "stop",
    "destroy",
    "status",
    "exec_cmd",
    "argv",
    "copy_from",
];

type CreateFn = Box<dyn Fn(&CreateOpts) -> Result<SandboxInfo> + Send + Sync>;
type BoxNameFn = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;
type StatusFn = Box<dyn Fn(&str) -> Result<SandboxStatus> + Send + Sync>;
type ExecCmdFn = Box<dyn Fn(&str, &[&str], bool) -> Result<ExecResult> + Send + Sync>;
type ArgvFn = Box<dyn Fn(&str, &[&str], bool) -> Vec<String> + Send + Sync>;
type CopyFromFn = Box<dyn Fn(&str, &str, &Path) -> Result<()> + Send + Sync>;

/// How an unscripted method fails: by name, so a test that reaches a call it
/// did not think about is told which one.
///
/// Returning `!` lets the fallible methods and [`Runtime::argv`], which has no
/// error channel at all, fail the same way.
fn unscripted(method: &str) -> ! {
    panic!("StubRuntime: {method} not scripted")
}

fn owned(cmd: &[&str]) -> Vec<String> {
    cmd.iter().map(|word| (*word).to_string()).collect()
}

/// A [`Runtime`] that does nothing except what a test scripts, and remembers
/// everything it was asked to do.
///
/// `is_available()` is always `true` and `priority()` is always `0` — no test
/// double has ever wanted anything else, and neither is reachable through
/// [`super::detect`], which names its runtimes explicitly.
pub struct StubRuntime {
    name: String,
    exec_runs_as_root: bool,
    calls: Mutex<Vec<Call>>,
    create: Option<CreateFn>,
    start: Option<BoxNameFn>,
    stop: Option<BoxNameFn>,
    destroy: Option<BoxNameFn>,
    status: Option<StatusFn>,
    exec_cmd: Option<ExecCmdFn>,
    argv: Option<ArgvFn>,
    copy_from: Option<CopyFromFn>,
}

impl Default for StubRuntime {
    fn default() -> Self {
        Self {
            name: "stub".to_string(),
            exec_runs_as_root: false,
            calls: Mutex::new(Vec::new()),
            create: None,
            start: None,
            stop: None,
            destroy: None,
            status: None,
            exec_cmd: None,
            argv: None,
            copy_from: None,
        }
    }
}

impl StubRuntime {
    /// A stub with nothing scripted: every method panics, and says which one.
    pub fn new() -> Self {
        Self::default()
    }

    /// What [`Runtime::name`] reports. Defaults to `"stub"`.
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    /// What [`Runtime::exec_runs_as_root`] reports — which decides whether
    /// [`Runtime::run_as_root`] prefixes `sudo`. Defaults to `false`.
    pub fn with_exec_runs_as_root(mut self, yes: bool) -> Self {
        self.exec_runs_as_root = yes;
        self
    }

    pub fn with_create<F>(mut self, f: F) -> Self
    where
        F: Fn(&CreateOpts) -> Result<SandboxInfo> + Send + Sync + 'static,
    {
        self.create = Some(Box::new(f));
        self
    }

    pub fn with_start<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.start = Some(Box::new(f));
        self
    }

    pub fn with_stop<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.stop = Some(Box::new(f));
        self
    }

    pub fn with_destroy<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.destroy = Some(Box::new(f));
        self
    }

    /// The one status this box always reports.
    pub fn with_status(self, status: SandboxStatus) -> Self {
        self.with_status_fn(move |_: &str| Ok(status.clone()))
    }

    /// A status that can also fail, for the runtime that cannot see its guest.
    pub fn with_status_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<SandboxStatus> + Send + Sync + 'static,
    {
        self.status = Some(Box::new(f));
        self
    }

    /// The guest's answer to a command. [`Runtime::run_as_root`] and
    /// [`Runtime::exec_as_user`] funnel into this, so scripting it covers all
    /// three.
    pub fn with_exec_cmd<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &[&str], bool) -> Result<ExecResult> + Send + Sync + 'static,
    {
        self.exec_cmd = Some(Box::new(f));
        self
    }

    /// The host-side argv that would enter this box.
    pub fn with_argv<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &[&str], bool) -> Vec<String> + Send + Sync + 'static,
    {
        self.argv = Some(Box::new(f));
        self
    }

    /// What [`Runtime::copy_from`] does. The host path is passed through, so a
    /// script that wants to stand in for a real transfer can write the file
    /// the caller is about to check.
    pub fn with_copy_from<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &str, &Path) -> Result<()> + Send + Sync + 'static,
    {
        self.copy_from = Some(Box::new(f));
        self
    }

    /// Every call, in the order it was made — including the ones whose scripted
    /// answer was an error, because "it was asked and refused" and "it was
    /// never asked" are different bugs.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("stub call log").clone()
    }

    /// The commands handed to [`Runtime::exec_cmd`], each joined by spaces the
    /// way the guest would receive it.
    pub fn exec_commands(&self) -> Vec<String> {
        self.calls()
            .iter()
            .filter_map(|call| match call {
                Call::Exec { cmd, .. } => Some(cmd.join(" ")),
                _ => None,
            })
            .collect()
    }

    /// Whether `method` was called at least once.
    ///
    /// Panics on a name this does not record, so that a typo fails loudly
    /// rather than reading as "no, it was never called".
    pub fn called(&self, method: &str) -> bool {
        assert!(
            RECORDED.contains(&method),
            "StubRuntime records no method named {method:?}; it records {RECORDED:?}"
        );
        self.calls().iter().any(|call| call.method() == method)
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("stub call log").push(call);
    }
}

#[async_trait]
impl Runtime for StubRuntime {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_available(&self) -> bool {
        true
    }

    fn priority(&self) -> u32 {
        0
    }

    fn exec_runs_as_root(&self) -> bool {
        self.exec_runs_as_root
    }

    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo> {
        self.record(Call::Create(opts.name.clone()));
        match &self.create {
            Some(f) => f(opts),
            None => unscripted("create"),
        }
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.record(Call::Start(name.to_string()));
        match &self.start {
            Some(f) => f(name),
            None => unscripted("start"),
        }
    }

    async fn stop(&self, name: &str) -> Result<()> {
        self.record(Call::Stop(name.to_string()));
        match &self.stop {
            Some(f) => f(name),
            None => unscripted("stop"),
        }
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        self.record(Call::Destroy(name.to_string()));
        match &self.destroy {
            Some(f) => f(name),
            None => unscripted("destroy"),
        }
    }

    async fn status(&self, name: &str) -> Result<SandboxStatus> {
        self.record(Call::Status(name.to_string()));
        match &self.status {
            Some(f) => f(name),
            None => unscripted("status"),
        }
    }

    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult> {
        self.record(Call::Exec {
            name: name.to_string(),
            cmd: owned(cmd),
            interactive,
        });
        match &self.exec_cmd {
            Some(f) => f(name, cmd, interactive),
            None => unscripted("exec_cmd"),
        }
    }

    fn argv(&self, name: &str, cmd: &[&str], interactive: bool) -> Vec<String> {
        self.record(Call::Argv {
            name: name.to_string(),
            cmd: owned(cmd),
            interactive,
        });
        match &self.argv {
            Some(f) => f(name, cmd, interactive),
            None => unscripted("argv"),
        }
    }

    async fn copy_from(&self, name: &str, guest_path: &str, host_path: &Path) -> Result<()> {
        self.record(Call::CopyFrom {
            name: name.to_string(),
            guest_path: guest_path.to_string(),
            host_path: host_path.to_path_buf(),
        });
        match &self.copy_from {
            Some(f) => f(name, guest_path, host_path),
            None => unscripted("copy_from"),
        }
    }

    // No test double has ever scripted anything below, so none of these has a
    // builder. They keep the `unimplemented!()` semantics the five doubles had,
    // and add the method's name to the message.

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        unscripted("list")
    }

    async fn snapshot_create(&self, _name: &str, _snap: &str) -> Result<()> {
        unscripted("snapshot_create")
    }

    async fn snapshot_restore(&self, _name: &str, _snap: &str) -> Result<()> {
        unscripted("snapshot_restore")
    }

    async fn snapshot_list(&self, _name: &str) -> Result<Vec<SnapshotInfo>> {
        unscripted("snapshot_list")
    }

    async fn upgrade(&self, _name: &str, _tools: &[String]) -> Result<()> {
        unscripted("upgrade")
    }

    async fn update_mounts(&self, _name: &str, _mounts: &[Mount]) -> Result<MountUpdate> {
        unscripted("update_mounts")
    }

    async fn rollback_mounts(&self, _name: &str, _update: &MountUpdate) -> Result<()> {
        unscripted("rollback_mounts")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::bail;

    /// The whole point of the replacement: `unimplemented!()` said only that
    /// something was missing, and left the reader to find which call it was.
    #[tokio::test]
    #[should_panic(expected = "StubRuntime: stop not scripted")]
    async fn an_unscripted_method_names_itself() {
        let _ = StubRuntime::new().stop("box").await;
    }

    /// The transfer primitive records what it was asked to move, so a test can
    /// assert on the guest path and the destination without the script having
    /// to remember them itself.
    #[tokio::test]
    async fn copy_from_records_both_ends() {
        let stub = StubRuntime::new().with_copy_from(|_, _, _| Ok(()));
        stub.copy_from(
            "devtest",
            "/tmp/archive.tar.gz",
            Path::new("/host/out.tar.gz"),
        )
        .await
        .expect("scripted");
        assert!(stub.called("copy_from"));
        assert_eq!(
            stub.calls(),
            vec![Call::CopyFrom {
                name: "devtest".into(),
                guest_path: "/tmp/archive.tar.gz".into(),
                host_path: PathBuf::from("/host/out.tar.gz"),
            }]
        );
    }

    /// And a refused transfer is still recorded: "it was asked and failed" and
    /// "it was never asked" are the two outcomes this whole change turns on.
    #[tokio::test]
    async fn a_refused_copy_is_still_recorded() {
        let stub = StubRuntime::new().with_copy_from(|_, _, _| bail!("no route to the box"));
        assert!(
            stub.copy_from("devtest", "/tmp/a", Path::new("/host/a"))
                .await
                .is_err()
        );
        assert!(stub.called("copy_from"));
    }

    /// Including the tail with no builder at all, which is where the five
    /// doubles spent most of their `unimplemented!()` arms.
    #[tokio::test]
    #[should_panic(expected = "StubRuntime: snapshot_list not scripted")]
    async fn a_method_with_no_builder_names_itself_too() {
        let _ = StubRuntime::new().snapshot_list("box").await;
    }

    /// `argv` has no error channel, so this is the one method whose failure
    /// mode could never have been anything else.
    #[test]
    #[should_panic(expected = "StubRuntime: argv not scripted")]
    fn an_unscripted_argv_names_itself() {
        StubRuntime::new().argv("box", &["true"], false);
    }

    /// "It was asked and refused" and "it was never asked" are different bugs,
    /// so the call is recorded before the scripted answer runs.
    #[tokio::test]
    async fn a_call_is_recorded_even_when_its_answer_is_a_failure() {
        let stub = StubRuntime::new().with_destroy(|_: &str| bail!("runtime refused delete"));
        stub.destroy("box").await.expect_err("scripted to fail");
        assert!(stub.called("destroy"));
        assert!(!stub.called("stop"));
        assert_eq!(stub.calls(), vec![Call::Destroy("box".to_string())]);
    }

    /// `run_as_root` funnels into `exec_cmd`, which is what the overlay tests
    /// depend on: they assert on commands they never call `exec_cmd` for.
    #[tokio::test]
    async fn root_commands_arrive_as_exec_commands() {
        let stub = StubRuntime::new().with_exec_cmd(|_: &str, _: &[&str], _: bool| {
            Ok(ExecResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        stub.run_as_root("box", "mount -o remount /workspace", false)
            .await
            .expect("scripted");
        assert_eq!(
            stub.exec_commands(),
            vec!["sudo bash -lc mount -o remount /workspace".to_string()]
        );

        let root = StubRuntime::new()
            .with_exec_runs_as_root(true)
            .with_exec_cmd(|_: &str, _: &[&str], _: bool| {
                Ok(ExecResult {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            });
        root.run_as_root("box", "id -un", false).await.expect("ok");
        assert_eq!(root.exec_commands(), vec!["bash -lc id -un".to_string()]);
    }

    #[test]
    #[should_panic(expected = "records no method named")]
    fn a_misspelled_method_is_not_silently_absent() {
        let _ = StubRuntime::new().called("destory");
    }
}
