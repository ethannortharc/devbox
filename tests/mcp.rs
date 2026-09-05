//! `devbox mcp` — the registry and the stdio shim, end to end (§7).
//!
//! Two things are checked here that the unit tests in `src/mcp/` cannot:
//!
//! - The shim run through a **real [`Runtime`]**, so the argv the runtime
//!   builds is the argv the shim spawns, and the byte stream survives that
//!   composition rather than only surviving a direct `sh -c cat`.
//! - The **CLI surface**, driven as a user drives it: `add`, `ls`, `rm` against
//!   a real `devbox.toml` in a temporary project, with its comments intact
//!   afterwards.
//!
//! No box is created and none is started. The runtime here is a local stand-in
//! that "enters a box" by running the command on this host, which is the part
//! of the contract the shim actually depends on: `argv()` returns a host-side
//! command line whose stdio is the guest process's stdio.

use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use devbox::mcp::registry::{self, McpEntry};
use devbox::mcp::shim::{self, ShimOptions};
use devbox::runtime::{
    CreateOpts, ExecResult, Mount, MountUpdate, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

// ── a runtime whose "box" is this host ──────────────────

/// The smallest thing that satisfies [`Runtime`] honestly.
///
/// `argv` is the method the shim uses and the only one that has to be right:
/// it prefixes the command exactly as `limactl shell … --` and `docker exec -i
/// …` do, and the result is a host command line the caller owns the stdio of.
/// `exec_cmd` is implemented too, because the reaper goes through it.
struct LocalRuntime;

#[async_trait::async_trait]
impl Runtime for LocalRuntime {
    fn name(&self) -> &str {
        "local"
    }
    fn is_available(&self) -> bool {
        true
    }
    fn priority(&self) -> u32 {
        0
    }
    fn argv(&self, _name: &str, cmd: &[&str], _interactive: bool) -> Vec<String> {
        // `env --` stands in for `limactl shell <vm> --`: a real prefix that
        // execs the rest, so a bug in how the shim assembles argv shows up
        // here rather than only inside a VM.
        let mut argv = vec!["env".to_string(), "--".to_string()];
        argv.extend(cmd.iter().map(|s| s.to_string()));
        argv
    }
    async fn exec_cmd(&self, _: &str, cmd: &[&str], _: bool) -> Result<ExecResult> {
        let output = Command::new(cmd[0]).args(&cmd[1..]).output()?;
        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
    async fn create(&self, _: &CreateOpts) -> Result<SandboxInfo> {
        unimplemented!()
    }
    async fn start(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn stop(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn destroy(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn status(&self, _: &str) -> Result<SandboxStatus> {
        Ok(SandboxStatus::Running)
    }
    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        unimplemented!()
    }
    async fn snapshot_create(&self, _: &str, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn snapshot_restore(&self, _: &str, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn snapshot_list(&self, _: &str) -> Result<Vec<SnapshotInfo>> {
        unimplemented!()
    }
    async fn upgrade(&self, _: &str, _: &[String]) -> Result<()> {
        unimplemented!()
    }
    async fn update_mounts(&self, _: &str, _: &[Mount]) -> Result<MountUpdate> {
        unimplemented!()
    }
    async fn rollback_mounts(&self, _: &str, _: &MountUpdate) -> Result<()> {
        unimplemented!()
    }
}

/// The argv `mcp run` builds: registry entry → guest wrapper → runtime prefix.
fn run_argv(pgid_file: &str, entry: &McpEntry) -> Vec<String> {
    let guest = shim::wrap_guest_command(pgid_file, entry.env.iter(), &entry.command);
    let refs: Vec<&str> = guest.iter().map(String::as_str).collect();
    LocalRuntime.argv("devtest", &refs, false)
}

fn entry(command: &[&str]) -> McpEntry {
    McpEntry {
        command: command.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

// ── the shim over a Runtime ─────────────────────────────

/// One megabyte of the bytes a JSON-RPC transport is most likely to mangle,
/// through the whole composed command line, compared by content.
#[tokio::test]
async fn a_megabyte_survives_the_runtime_and_the_wrapper_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let pgid = dir.path().join("run.pgid");

    let mut payload = Vec::with_capacity(1 << 20);
    let mut seed = 0x9e3779b97f4a7c15u64;
    while payload.len() < (1 << 20) {
        payload.extend_from_slice(b"\r\n\x00\xff\x1a\x04\x7f\x80\xc3\x28{\"jsonrpc\":\"2.0\"}\r\n");
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        payload.extend_from_slice(&seed.to_le_bytes());
    }
    payload.truncate(1 << 20);
    let expected = payload.clone();

    let (mut host, agent) = duplex(1 << 16);
    let (mut sink_read, sink_write) = duplex(1 << 16);
    let argv = run_argv(pgid.to_str().unwrap(), &entry(&["cat"]));

    let run = tokio::spawn(async move {
        shim::pump(
            &argv,
            agent,
            sink_write,
            &ShimOptions::new(dir.path().join("mcp.log")),
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
    let outcome = run.await.unwrap().unwrap();

    assert_eq!(received.len(), 1 << 20, "the shim changed the byte count");
    assert_eq!(received, expected, "the shim changed the bytes");
    assert_eq!(outcome.exit_code, 0);
}

/// Request in, response out, with the request never touching a disk or a
/// parser on the way — the shape of a real MCP session, minus the server.
#[tokio::test]
async fn a_json_rpc_exchange_round_trips_while_the_session_stays_open() {
    let dir = tempfile::tempdir().unwrap();
    let pgid = dir.path().join("run.pgid");
    let argv = run_argv(pgid.to_str().unwrap(), &entry(&["cat"]));

    let (mut host, agent) = duplex(1 << 16);
    let (mut sink_read, sink_write) = duplex(1 << 16);
    let run = tokio::spawn(async move {
        shim::pump(
            &argv,
            agent,
            sink_write,
            &ShimOptions::new(dir.path().join("mcp.log")),
            std::future::pending::<()>(),
        )
        .await
    });

    for id in 1..=3 {
        let request = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n");
        host.write_all(request.as_bytes()).await.unwrap();
        host.flush().await.unwrap();

        let mut echoed = vec![0u8; request.len()];
        tokio::time::timeout(Duration::from_secs(10), sink_read.read_exact(&mut echoed))
            .await
            .expect("a response must arrive without waiting for the session to end")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&echoed), request);
    }

    host.shutdown().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("closing stdin ends the session")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.exit_code, 0);
}

#[tokio::test]
async fn the_guest_exit_code_reaches_the_agent_through_the_runtime() {
    for code in [0, 3, 42, 127] {
        let dir = tempfile::tempdir().unwrap();
        let pgid = dir.path().join("run.pgid");
        let argv = run_argv(
            pgid.to_str().unwrap(),
            &entry(&["sh", "-c", &format!("exit {code}")]),
        );
        let (_host, agent) = duplex(64);
        let (_out, sink) = duplex(1 << 16);
        let outcome = shim::pump(
            &argv,
            agent,
            sink,
            &ShimOptions::new(dir.path().join("mcp.log")),
            std::future::pending::<()>(),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.exit_code, code,
            "exit {code} did not survive the runtime"
        );
    }
}

/// The registry's `env` reaches the process, and reaches it as a value rather
/// than as something a shell gets to look at.
#[tokio::test]
async fn registered_env_reaches_the_guest_process_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let pgid = dir.path().join("run.pgid");
    let mut e = entry(&["sh", "-c", "printf '%s' \"$MCP_TEST\""]);
    e.env
        .insert("MCP_TEST".to_string(), "a $HOME 'quoted' value".to_string());
    let argv = run_argv(pgid.to_str().unwrap(), &e);

    let (_host, agent) = duplex(64);
    let (mut out, sink) = duplex(1 << 16);
    let run = tokio::spawn(async move {
        shim::pump(
            &argv,
            agent,
            sink,
            &ShimOptions::new(dir.path().join("mcp.log")),
            std::future::pending::<()>(),
        )
        .await
    });
    let mut seen = Vec::new();
    out.read_to_end(&mut seen).await.unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(String::from_utf8_lossy(&seen), "a $HOME 'quoted' value");
}

/// A server that ignores its stdin is killed, and the reaper — the command
/// `mcp run` sends back into the box — takes the process group with it.
#[tokio::test]
async fn a_deaf_server_and_the_children_it_spawned_are_both_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let pgid = dir.path().join("run.pgid");
    let marker = dir.path().join("child-alive");

    // A parent that ignores stdin, and a grandchild that outlives it — the
    // `uvx` → `python` shape.
    let script = format!(
        "( while true; do touch {}; sleep 0.2; done ) & exec sleep 300",
        marker.display()
    );
    let argv = run_argv(pgid.to_str().unwrap(), &entry(&["sh", "-c", &script]));

    let mut options = ShimOptions::new(dir.path().join("mcp.log"));
    options.eof_grace = Duration::from_millis(200);

    let (host, agent) = duplex(64);
    let (_out, sink) = duplex(1 << 16);
    drop(host);

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        shim::pump(&argv, agent, sink, &options, std::future::pending::<()>()),
    )
    .await
    .expect("the shim must not wait forever on a deaf server")
    .unwrap();
    assert!(
        outcome.forced,
        "a killed server was reported as a clean exit"
    );

    // Before running the reaper *in this process*, check what it is about to
    // be pointed at. `LocalRuntime` has no box to hide behind: the reaper runs
    // in the harness's own process group, so a recorded id that is the
    // harness's group is a `kill` aimed at `cargo test`. That is not a
    // hypothetical — it is what this suite did on Linux CI, where the step
    // died with 143 and this was the only case that never printed `ok`.
    let recorded: i32 = std::fs::read_to_string(&pgid)
        .expect("the wrapper recorded a group")
        .trim()
        .parse()
        .expect("and recorded it as a number");
    assert_ne!(
        recorded,
        shim::own_process_group(),
        "the wrapper recorded the harness's own process group"
    );
    assert!(recorded > 1, "process group {recorded} names every process");

    // Bounded, because it is the only wait in this test that had no bound.
    // `exec_cmd` ends in `Command::output()`, which waits for EOF on the
    // reaper's pipes as well as for the reaper — so a descendant that survives
    // holding one of them is a wait with no end, and a test that hangs takes
    // the whole binary's step down with it and names no failing case. A CI job
    // cancelled at 73 seconds is what that looks like from the outside.
    let reaper = shim::reaper_script(pgid.to_str().unwrap());
    let refs: Vec<&str> = reaper.iter().map(String::as_str).collect();
    let reaped = tokio::time::timeout(
        Duration::from_secs(30),
        LocalRuntime.exec_cmd("devtest", &refs, false),
    )
    .await
    .expect("the reaper must finish, or say which descendant is holding its pipes")
    .unwrap();
    assert_ne!(
        reaped.exit_code, 2,
        "the reaper refused the group it was given: {}",
        reaped.stderr
    );
    assert!(!pgid.exists(), "the reaper left its marker file behind");

    // The grandchild refreshes the marker five times a second; if it is still
    // alive the mtime keeps moving.
    let _ = std::fs::remove_file(&marker);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !marker.exists(),
        "a grandchild of the MCP server outlived the shim"
    );
}

// ── the CLI, against a real devbox.toml ─────────────────

fn devbox(project: &std::path::Path, state: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devbox"));
    command.current_dir(project).env("HOME", state);
    command
}

const PROJECT: &str = "\
# devbox.toml
# a comment the user wrote

[sandbox]
runtime = \"lima\"

[policy]
egress = \"allowlist\"
allow = [\"pypi.org\"]
";

/// `add` prints the line the user pastes into the agent, and `ls` and `rm`
/// agree with it — while the file keeps every comment it had.
#[test]
fn add_ls_rm_round_trip_through_the_cli_and_the_file_keeps_its_comments() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let toml = project.path().join("devbox.toml");
    std::fs::write(&toml, PROJECT).unwrap();

    let added = devbox(project.path(), home.path())
        .args([
            "mcp",
            "add",
            "fetch",
            "--box",
            "devtest",
            "--",
            "uvx",
            "mcp-server-fetch",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&added.stdout);
    assert!(
        added.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        stdout.contains("claude mcp add fetch -- devbox mcp run fetch"),
        "add must print the line the agent needs:\n{stdout}"
    );
    assert!(
        stdout.contains("mcp\", \"run\", \"fetch\"") || stdout.contains("codex mcp add fetch"),
        "add must print the Codex spelling too:\n{stdout}"
    );

    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(text.starts_with(PROJECT), "the file was rewritten:\n{text}");
    assert!(text.contains("# a comment the user wrote"));
    let table = registry::load(project.path()).unwrap();
    assert_eq!(table["fetch"].command, ["uvx", "mcp-server-fetch"]);
    assert_eq!(table["fetch"].box_name.as_deref(), Some("devtest"));

    let listed = devbox(project.path(), home.path())
        .args(["mcp", "ls"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(listed.contains("fetch"), "{listed}");
    assert!(listed.contains("devtest"), "{listed}");
    assert!(listed.contains("uvx mcp-server-fetch"), "{listed}");

    let removed = devbox(project.path(), home.path())
        .args(["mcp", "rm", "fetch"])
        .output()
        .unwrap();
    assert!(removed.status.success());
    let text = std::fs::read_to_string(&toml).unwrap();
    assert_eq!(text, PROJECT, "removal did not restore the file exactly");
    assert!(registry::load(project.path()).unwrap().is_empty());
}

/// The recorded posture reaches the file in the spelling `devbox policy` uses.
#[test]
fn a_recorded_posture_round_trips_and_the_project_box_warning_is_printed() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("devbox.toml"), PROJECT).unwrap();

    let output = devbox(project.path(), home.path())
        .args(["mcp", "add", "srv", "--posture", "mirror-only", "--", "cat"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("posture is box-granular"),
        "§7.2 requires a warning on a project box:\n{stderr}"
    );

    let table = registry::load(project.path()).unwrap();
    assert_eq!(
        table["srv"].posture,
        Some(devbox::policy::Posture::MirrorOnly)
    );
    let text = std::fs::read_to_string(project.path().join("devbox.toml")).unwrap();
    assert!(text.contains("posture = \"mirror-only\""), "{text}");
}

/// A project with no `devbox.toml` is not one `mcp add` will invent.
///
/// A file containing only `[mcp.…]` parses, but every section it omits reads
/// back as the *serde* default — and that default for `[mounts]` is empty, not
/// the workspace mount `DevboxConfig::default()` carries. `devbox create`
/// would then build a box with nothing mounted, from a config nobody wrote.
#[test]
fn adding_in_a_bare_directory_is_refused_and_names_both_ways_forward() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let output = devbox(project.path(), home.path())
        .args(["mcp", "add", "srv", "--", "cat"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "a bare directory must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("devbox init"), "{stderr}");
    assert!(stderr.contains("--global"), "{stderr}");
    assert!(
        !project.path().join("devbox.toml").exists(),
        "a refused `mcp add` still wrote a project config"
    );
}

/// The reason the global registry exists: an agent's working directory is its
/// own business, and a project-scoped registration disappears from anywhere
/// else. `--global` is visible from a directory with no `devbox.toml` at all.
#[test]
fn a_global_registration_is_visible_from_a_directory_that_has_no_project() {
    let home = tempfile::tempdir().unwrap();
    let registered_from = tempfile::tempdir().unwrap();
    let somewhere_else = tempfile::tempdir().unwrap();

    let added = devbox(registered_from.path(), home.path())
        .args([
            "mcp",
            "add",
            "fetch",
            "--global",
            "--box",
            "mcp-tools",
            "--",
            "uvx",
            "srv",
        ])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}{}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );
    let global = home.path().join(".devbox").join("mcp.toml");
    assert!(global.exists(), "--global did not write ~/.devbox/mcp.toml");
    assert_eq!(
        registry::load_file(&global).unwrap()["fetch"].command,
        ["uvx", "srv"]
    );

    // From an unrelated directory: listed, and resolvable.
    let listed = devbox(somewhere_else.path(), home.path())
        .args(["mcp", "ls"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(listed.contains("fetch"), "{listed}");
    assert!(listed.contains("global"), "{listed}");

    // `run` finds it too — it gets as far as the box, which does not exist.
    let ran = devbox(somewhere_else.path(), home.path())
        .args(["mcp", "run", "fetch"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(
        stderr.contains("mcp-tools"),
        "`mcp run` did not resolve the global entry: {stderr}"
    );

    // And `rm` takes it out of the file it was actually in.
    let removed = devbox(somewhere_else.path(), home.path())
        .args(["mcp", "rm", "fetch"])
        .output()
        .unwrap();
    assert!(removed.status.success());
    assert!(
        String::from_utf8_lossy(&removed.stdout).contains("mcp.toml"),
        "`mcp rm` did not say which file it edited"
    );
    assert!(registry::load_file(&global).unwrap().is_empty());
}

/// Both files can hold the same name. The project one is what runs, `ls` says
/// so rather than listing one of them, and removing it uncovers the other.
#[test]
fn a_project_entry_shadows_the_global_one_and_ls_says_which() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("devbox.toml"), PROJECT).unwrap();

    devbox(project.path(), home.path())
        .args([
            "mcp",
            "add",
            "srv",
            "--global",
            "--box",
            "global-box",
            "--",
            "global-cmd",
        ])
        .output()
        .unwrap();
    let added = devbox(project.path(), home.path())
        .args([
            "mcp",
            "add",
            "srv",
            "--box",
            "project-box",
            "--",
            "project-cmd",
        ])
        .output()
        .unwrap();
    assert!(added.status.success());

    let listed = devbox(project.path(), home.path())
        .args(["mcp", "ls"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(listed.contains("project-cmd"), "{listed}");
    assert!(
        listed.contains("global-cmd"),
        "both entries must be listed:\n{listed}"
    );
    assert!(
        listed.contains("global*"),
        "the shadowed entry must be marked:\n{listed}"
    );

    // The project entry is the one that runs.
    let ran = devbox(project.path(), home.path())
        .args(["mcp", "run", "srv"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(
        stderr.contains("project-box"),
        "the global entry won: {stderr}"
    );

    // Removing it removes the project one and says the global one is now live.
    let removed = devbox(project.path(), home.path())
        .args(["mcp", "rm", "srv"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&removed.stdout).to_string();
    assert!(stdout.contains("devbox.toml"), "{stdout}");
    assert!(
        stdout.contains("keeps working"),
        "`mcp rm` did not say the global entry is now uncovered:\n{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("devbox.toml")).unwrap(),
        PROJECT
    );
    let global = home.path().join(".devbox").join("mcp.toml");
    assert_eq!(
        registry::load_file(&global).unwrap()["srv"].command,
        ["global-cmd"]
    );
}

/// The global file is edited with the same text surgery, so a comment someone
/// put in it survives too.
#[test]
fn the_global_registry_keeps_its_comments() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let global = home.path().join(".devbox").join("mcp.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let original = "# my own note about these servers\n\n[mcp.kept]\ncommand = [\"kept\"]\n";
    std::fs::write(&global, original).unwrap();

    devbox(project.path(), home.path())
        .args(["mcp", "add", "added", "--global", "--", "added-cmd"])
        .output()
        .unwrap();
    let after_add = std::fs::read_to_string(&global).unwrap();
    assert!(after_add.starts_with(original), "{after_add}");

    devbox(project.path(), home.path())
        .args(["mcp", "rm", "added"])
        .output()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&global).unwrap(),
        original,
        "removing from the global registry did not restore it exactly"
    );
}

#[test]
fn an_unknown_server_is_an_error_that_names_the_file_it_looked_in() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("devbox.toml"), PROJECT).unwrap();

    for verb in ["run", "rm"] {
        let output = devbox(project.path(), home.path())
            .args(["mcp", verb, "absent"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "`mcp {verb} absent` should fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("absent"), "{stderr}");
        assert!(stderr.contains("devbox.toml"), "{stderr}");
        assert!(
            stderr.contains("mcp.toml"),
            "the error must name the global registry too: {stderr}"
        );
    }
}

#[test]
fn the_help_offers_the_whole_component() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let output = devbox(project.path(), home.path())
        .args(["mcp", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    for verb in ["add", "run", "ls", "rm", "report", "self"] {
        assert!(help.contains(verb), "`mcp --help` omits {verb}:\n{help}");
    }
}

/// A server that has never run has no report, and the message has to say that
/// rather than an id nobody can act on.
#[test]
fn reporting_on_a_server_that_has_never_run_says_so() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("devbox.toml"), PROJECT).unwrap();
    devbox(project.path(), home.path())
        .args([
            "mcp",
            "add",
            "fetch",
            "--box",
            "mcp-tools",
            "--",
            "uvx",
            "srv",
        ])
        .output()
        .unwrap();

    let output = devbox(project.path(), home.path())
        .args(["mcp", "report", "fetch"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no recorded run"), "{stderr}");
    assert!(stderr.contains("mcp-tools"), "{stderr}");
}

#[test]
fn reporting_on_a_server_that_is_not_registered_says_that_instead() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("devbox.toml"), PROJECT).unwrap();
    let output = devbox(project.path(), home.path())
        .args(["mcp", "report", "absent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no MCP server named 'absent'"), "{stderr}");
}

/// `devbox mcp self` speaks the protocol over real pipes — the thing an agent
/// actually does, rather than the in-process dispatch the unit tests cover.
#[test]
fn mcp_self_answers_a_real_stdio_session() {
    use std::io::Write as _;

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let mut child = devbox(project.path(), home.path())
        .args(["mcp", "self"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let session = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    );
    stdin.write_all(session.as_bytes()).unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "one response per request and none for the notification:\n{stdout}"
    );

    let init: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    let tools: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(tools["id"], 2);
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["list_runs", "run_report", "behavior_summary", "watch"]
    );
}
