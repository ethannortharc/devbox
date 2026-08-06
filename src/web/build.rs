//! Streaming builds.
//!
//! A `nixos-rebuild switch` can run for minutes. `Runtime::exec_cmd` captures
//! output and hands it back at the end, which is fine for a CLI and useless in
//! a browser — the user needs to see the build move. This module runs a
//! runtime command with piped stdio and publishes each line to the console's
//! SSE channel as it arrives (§6.3).

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::state::{AppState, ConsoleEvent};
use crate::nix::compose::Selection;
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;

/// SSE event name carrying build output for a box.
///
/// Scoped per box so two concurrent rebuilds never interleave in one log
/// panel; the page subscribes to its own box's stream only.
pub fn output_event(box_name: &str) -> String {
    format!("build-{box_name}")
}

/// SSE event name carrying a build's terminal state (`ok` or `failed: …`).
pub fn status_event(box_name: &str) -> String {
    format!("build-status-{box_name}")
}

/// Escape a build line for embedding in HTML.
///
/// Build output is attacker-influenced in the only sense that matters here: a
/// package name or an error message can contain `<`. Escaping keeps it text.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap a build line as the HTML fragment the log panel appends.
pub fn line_fragment(line: &str) -> String {
    format!("<div class=\"logline\">{}</div>", escape_html(line))
}

/// Run a host-side command, publishing every output line to `on_line`.
///
/// stdout and stderr are merged in arrival order, which is what a build log
/// should look like. Returns the exit status.
pub async fn stream_command<F>(argv: &[String], mut on_line: F) -> Result<i32>
where
    F: FnMut(&str) + Send,
{
    let (program, args) = argv
        .split_first()
        .context("command argv must not be empty")?;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;

    let stdout = child.stdout.take().context("child has no stdout")?;
    let stderr = child.stderr.take().context("child has no stderr")?;

    let mut out = BufReader::new(stdout).lines();
    let mut err = BufReader::new(stderr).lines();
    let mut out_done = false;
    let mut err_done = false;

    while !(out_done && err_done) {
        tokio::select! {
            line = out.next_line(), if !out_done => match line {
                Ok(Some(l)) => on_line(&l),
                _ => out_done = true,
            },
            line = err.next_line(), if !err_done => match line {
                Ok(Some(l)) => on_line(&l),
                _ => err_done = true,
            },
        }
    }

    let status = child.wait().await.context("failed to reap the child")?;
    Ok(status.code().unwrap_or(-1))
}

/// The generated files a Sets apply replaces, and their previous contents.
///
/// `None` for a file means it did not exist — restoring then removes it, so a
/// box that never had a selection does not end up with an empty one.
type Generated = Vec<(&'static str, Option<String>)>;

/// Files `write_set_modules` overwrites.
const GENERATED_FILES: &[&str] = &["/etc/devbox/devbox.nix", "/etc/devbox/devbox-state.toml"];

/// Read the generated files so a failed rebuild can put them back.
async fn snapshot_generated(runtime: &dyn crate::runtime::Runtime, box_name: &str) -> Generated {
    let mut out = Vec::new();
    for path in GENERATED_FILES {
        let content = runtime
            .exec_cmd(box_name, &["cat", path], false)
            .await
            .ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout);
        out.push((*path, content));
    }
    out
}

/// Put the generated files back. Best effort: a box that is now unreachable
/// cannot be repaired from here, and saying so is the rebuild error's job.
async fn restore_generated(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
    backup: &Generated,
) -> bool {
    let mut restored = false;
    for (path, content) in backup {
        let script = match content {
            Some(text) => {
                format!("cat > {path} << 'DEVBOX_RESTORE_EOF'\n{text}\nDEVBOX_RESTORE_EOF")
            }
            None => format!("rm -f {path}"),
        };
        let ok = runtime
            .exec_cmd(box_name, &["sudo", "bash", "-c", &script], false)
            .await
            .is_ok_and(|r| r.exit_code == 0);
        restored |= ok;
    }
    restored
}

/// Apply a set selection to a box and rebuild it, streaming progress.
///
/// The sequence mirrors `nix::apply_config` — write the set modules, write the
/// composed `configuration.nix`, rebuild — with the long step streamed and the
/// box's persisted state updated only after the rebuild actually succeeds.
pub async fn apply_selection(
    manager: &Arc<SandboxManager>,
    state: &AppState,
    box_name: &str,
    selection: &Selection,
) -> Result<()> {
    selection.validate()?;

    let sandbox = manager.get_sandbox(box_name)?;

    // The same guard the CLI applies. Without it the Sets tab pushes new files
    // into the box and only then discovers there is no `nixos-rebuild` — a
    // half-applied selection plus an opaque error, instead of the actionable
    // message.
    if sandbox.image != "nixos" {
        bail!(
            "box '{box_name}' uses the '{}' image, which has no nixos-rebuild. \
             Use `devbox nix add <pkg>` / `devbox nix remove <pkg>` there instead.",
            sandbox.image
        );
    }

    let runtime = manager.runtime_for_sandbox(&sandbox)?;
    // Everything below runs *inside* the guest — the snapshot, the writes, the
    // rebuild — so a stopped box fails on the first exec. The CLI path already
    // did this; the background path did not, and simply never rebuilt.
    crate::web::service::ensure_running(manager, box_name).await?;

    let publish = |line: &str| {
        state.publish(ConsoleEvent::new(
            output_event(box_name),
            line_fragment(line),
        ));
    };

    publish(&format!(
        "devbox: applying selection ({} sets, {} extra package(s))",
        selection.sets.len(),
        selection.packages.len()
    ));

    // 1. Push the set modules and the composed configuration.
    //
    // Snapshot what is there first. A failed rebuild leaves the active
    // generation untouched, so the box really is unchanged — but the generated
    // *sources* have already been replaced, and a later manual `nixos-rebuild`
    // would then quietly apply the selection the console reported as rolled
    // back. Worse, a source that failed to build keeps failing until someone
    // notices why.
    let backup = snapshot_generated(runtime.as_ref(), box_name).await;
    let base = DevboxConfig::load_or_default(&sandbox.project_dir);
    let config = selection.to_config(&base);
    crate::nix::write_set_modules(runtime.as_ref(), box_name, selection)
        .await
        .context("failed to push Nix set modules into the box")?;
    publish("devbox: set modules written to /etc/devbox/sets/");

    // 2. Rebuild, streamed.
    let argv = runtime.argv(box_name, &crate::nix::rebuild::rebuild_argv(), false);
    publish(&format!("devbox: {}", argv.join(" ")));

    let code = stream_command(&argv, |line| publish(line)).await?;
    if code != 0 {
        let restored = restore_generated(runtime.as_ref(), box_name, &backup).await;
        if restored {
            publish("devbox: generated files restored to the last good selection");
        }
        state.publish(ConsoleEvent::new(
            status_event(box_name),
            format!(
                "<span class=\"term-err\">rebuild failed (exit {code}) — the box is unchanged</span>"
            ),
        ));
        bail!("nixos-rebuild switch failed with exit code {code}");
    }

    // 3. Only now is the selection real: record it.
    let mut sandbox = sandbox;
    sandbox.sets = config.active_sets();
    sandbox.languages = config.active_languages();
    sandbox.packages = selection.packages.iter().cloned().collect();
    sandbox.save(&manager.state_dir)?;

    state.publish(ConsoleEvent::new(
        status_event(box_name),
        "<span class=\"term-ok\">rebuild complete</span>".to_string(),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_scoped_per_box() {
        assert_eq!(output_event("myapp"), "build-myapp");
        assert_eq!(status_event("myapp"), "build-status-myapp");
        assert_ne!(output_event("a"), output_event("b"));
    }

    #[test]
    fn build_lines_are_escaped() {
        let frag = line_fragment("copying path '/nix/store/<x>' & \"y\"");
        assert!(!frag.contains("<x>"));
        assert!(frag.contains("&lt;x&gt;"));
        assert!(frag.contains("&amp;"));
        assert!(frag.contains("&quot;y&quot;"));
        assert!(frag.starts_with("<div class=\"logline\">"));
    }

    #[test]
    fn escaping_leaves_ordinary_output_alone() {
        assert_eq!(
            escape_html("building '/nix/store/abc-hello'"),
            "building '/nix/store/abc-hello'".replace('\'', "&#39;")
        );
    }

    #[tokio::test]
    async fn streams_stdout_line_by_line() {
        let mut lines = Vec::new();
        let code = stream_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo one; echo two; echo three".to_string(),
            ],
            |l| lines.push(l.to_string()),
        )
        .await
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn merges_stderr_into_the_same_log() {
        let mut lines = Vec::new();
        stream_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo to-stdout; echo to-stderr 1>&2".to_string(),
            ],
            |l| lines.push(l.to_string()),
        )
        .await
        .unwrap();

        assert!(lines.contains(&"to-stdout".to_string()));
        assert!(lines.contains(&"to-stderr".to_string()));
    }

    #[tokio::test]
    async fn reports_a_non_zero_exit() {
        let code = stream_command(
            &["sh".to_string(), "-c".to_string(), "exit 3".to_string()],
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(code, 3);
    }

    #[tokio::test]
    async fn an_empty_argv_is_an_error_not_a_panic() {
        assert!(stream_command(&[], |_| {}).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error() {
        assert!(
            stream_command(&["devbox-no-such-program-xyz".to_string()], |_| {})
                .await
                .is_err()
        );
    }
}
