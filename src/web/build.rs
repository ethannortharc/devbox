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
pub struct Generated {
    /// The composition files and their previous contents; `None` means absent.
    files: Vec<(&'static str, Option<String>)>,
    /// Whether the per-set module directory was successfully archived.
    ///
    /// Tracked rather than assumed: a snapshot that silently did not happen
    /// makes the later "restored" message a lie, which is worse than saying
    /// the rollback was incomplete.
    sets_archived: bool,
    /// Whether the directory existed at all when the snapshot was taken.
    ///
    /// "No archive" has two meanings and they need opposite handling. On a
    /// first or legacy box the directory was simply absent, and a failed apply
    /// *creates* it — so restoring means deleting what the failed run wrote,
    /// not leaving it in place for a later rebuild to evaluate.
    sets_existed: bool,
}

/// Files `write_set_modules` overwrites.
const GENERATED_FILES: &[&str] = &["/etc/devbox/devbox.nix", "/etc/devbox/devbox-state.toml"];

/// The directory of per-set modules `write_set_modules` also rewrites.
///
/// Backing up only the two composition files left every `sets/*.nix` replaced
/// after a failed rebuild, so a later manual rebuild would evaluate the new
/// modules and reintroduce exactly the change that was reported as rolled
/// back. The whole directory is snapshotted as a tarball: the set of files is
/// not fixed, and restoring a stale list would be its own bug.
const GENERATED_SETS_DIR: &str = "/etc/devbox/sets";

/// Where the pre-rebuild copy of the sets directory lives inside the box.
const SETS_BACKUP: &str = "/etc/devbox/.sets-backup.tar";

/// Read the generated files so a failed rebuild can put them back.
pub async fn snapshot_generated(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
) -> Generated {
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

    // The per-set modules too, as a tarball. Their names are not a fixed list
    // — the catalog changes — so copying the directory is the only honest way
    // to put it back exactly as it was.
    // Elevated, and its failure recorded. `/etc/devbox/sets` is root-owned,
    // and on a VM runtime these commands run as the ordinary guest user — so
    // an unprivileged `tar` silently produced no backup, and the restore then
    // reported success without restoring anything.
    let archived = runtime
        .exec_cmd(
            box_name,
            &[
                "sh",
                "-c",
                &crate::policy::enforce::elevated(&format!(
                    "if [ -d {GENERATED_SETS_DIR} ]; then \
                       rm -f {SETS_BACKUP} && \
                       tar cf {SETS_BACKUP} -C {GENERATED_SETS_DIR} .; \
                     fi"
                )),
            ],
            false,
        )
        .await
        .is_ok_and(|r| r.exit_code == 0);

    let existed = runtime
        .exec_cmd(
            box_name,
            &["sh", "-c", &format!("[ -d {GENERATED_SETS_DIR} ]")],
            false,
        )
        .await
        .is_ok_and(|r| r.exit_code == 0);

    Generated {
        files: out,
        sets_archived: archived,
        sets_existed: existed,
    }
}

/// Put the generated files back. Best effort: a box that is now unreachable
/// cannot be repaired from here, and saying so is the rebuild error's job.
pub async fn restore_generated(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
    backup: &Generated,
) -> bool {
    // The set modules first, so a partially written directory is replaced
    // wholesale rather than merged with what the failed run left behind.
    // Only claim to restore the modules if they were actually archived.
    let mut restored = if !backup.sets_existed {
        // Nothing was there before, so putting it back means removing what the
        // failed run created. Leaving it left failed modules staged for the
        // next rebuild to pick up — the thing the rollback exists to prevent.
        runtime
            .exec_cmd(
                box_name,
                &[
                    "sh",
                    "-c",
                    &crate::policy::enforce::elevated(&format!(
                        "rm -rf {GENERATED_SETS_DIR} {SETS_BACKUP}"
                    )),
                ],
                false,
            )
            .await
            .is_ok_and(|r| r.exit_code == 0)
    } else if backup.sets_archived {
        runtime
            .exec_cmd(
                box_name,
                &[
                    "sh",
                    "-c",
                    // No archive means the box had no sets directory to begin
                    // with — nothing to restore, which is success. A failed
                    // extraction is not.
                    &crate::policy::enforce::elevated(&format!(
                        "if [ -f {SETS_BACKUP} ]; then \
                           rm -rf {GENERATED_SETS_DIR} && mkdir -p {GENERATED_SETS_DIR} && \
                           tar xf {SETS_BACKUP} -C {GENERATED_SETS_DIR} && \
                           rm -f {SETS_BACKUP}; \
                         fi"
                    )),
                ],
                false,
            )
            .await
            .is_ok_and(|r| r.exit_code == 0)
    } else {
        false
    };

    // Every artifact, not any. `restored` used to be an OR across the files,
    // so one successful write made the console print "restored to the last
    // good selection" while the rest of the sources were still the failed
    // ones — the message the user most needs to be able to trust.
    for (path, content) in &backup.files {
        let script = match content {
            Some(text) => {
                format!("cat > {path} << 'DEVBOX_RESTORE_EOF'\n{text}\nDEVBOX_RESTORE_EOF")
            }
            None => format!("rm -f {path}"),
        };
        let ok = runtime
            .exec_cmd(
                box_name,
                &["sh", "-c", &crate::policy::enforce::elevated(&script)],
                false,
            )
            .await
            .is_ok_and(|r| r.exit_code == 0);
        restored &= ok;
    }
    restored
}

/// Which system generation the box is currently running.
///
/// `readlink /run/current-system` is the cheapest honest answer: it points at
/// the store path of the active generation and changes exactly when a switch
/// activates one. `None` means the question could not be answered, and callers
/// treat that as "assume it may have changed" — the conservative direction,
/// since an unnecessary rollback is recoverable and a skipped one is not.
pub async fn current_generation(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
) -> Option<String> {
    runtime
        .exec_cmd(box_name, &["readlink", "-f", "/run/current-system"], false)
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| r.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the modules and rebuild, with everything after the snapshot fallible.
///
/// Split out so a single `?` covers each step: the caller restores on any
/// error, which is the property that was missing when only the non-zero exit
/// code triggered a rollback.
async fn apply_after_snapshot(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
    selection: &Selection,
    publish: &(impl Fn(&str) + Sync),
) -> Result<()> {
    crate::nix::write_set_modules(runtime, box_name, selection)
        .await
        .context("failed to push Nix set modules into the box")?;
    publish("devbox: set modules written to /etc/devbox/sets/");

    // Which generation is current *before* the rebuild. `switch --rollback`
    // activates the generation before the current one — so running it after an
    // evaluation or build failure, where the profile never moved, would undo
    // the user's last *successful* configuration. Only a switch that actually
    // changed the profile should be reversed.
    let before = current_generation(runtime, box_name).await;

    let argv = runtime.argv(box_name, &crate::nix::rebuild::rebuild_argv(), false);
    publish(&format!("devbox: {}", argv.join(" ")));

    let code = stream_command(&argv, |line| publish(line)).await?;
    if code != 0 {
        let after = current_generation(runtime, box_name).await;
        if before.is_some() && before == after {
            // Nothing was activated: evaluation or build failed, and the box
            // is genuinely untouched. Rolling back here would be the bug.
            publish("devbox: rebuild failed before activation — the box is unchanged");
            bail!(
                "nixos-rebuild switch failed with exit code {code} before activation; \
                 the box is unchanged"
            );
        }

        // `nixos-rebuild switch` can fail *during activation*, after the new
        // generation has been made current — only evaluation and build
        // failures leave the box genuinely untouched. Restoring the sources
        // alone would then leave the guest running the new generation while
        // the console reported a rollback, so switch back explicitly. The CLI
        // path has always done this; the streamed one did not.
        publish("devbox: rebuild failed — rolling back to the previous generation");
        let rollback = runtime
            .exec_cmd(
                box_name,
                &["sudo", "nixos-rebuild", "switch", "--rollback"],
                false,
            )
            .await;
        match rollback {
            Ok(r) if r.exit_code == 0 => {
                publish("devbox: rolled back to the previous generation");
                bail!(
                    "nixos-rebuild switch failed with exit code {code}; \
                     rolled back to the previous generation"
                );
            }
            _ => bail!(
                "nixos-rebuild switch failed with exit code {code}, and the \
                 rollback also failed — the box may be on the new generation"
            ),
        }
    }
    Ok(())
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
    // Fallibly, and *before* the box is touched. `load_or_default` turns a
    // malformed devbox.toml into defaults, and step 3 writes the derived
    // config back over the original — so a syntax error anywhere in the file
    // would silently discard the user's mounts, resources, environment, and
    // policy.
    let base = DevboxConfig::load_for_edit(&sandbox.project_dir).context(
        "refusing to apply a selection: this box's devbox.toml cannot be read, and \
         applying would overwrite it with defaults",
    )?;
    let config = selection.to_config(&base);

    // Every step past the snapshot rolls back on failure, not just the
    // rebuild. A write that fails partway leaves some modules replaced and
    // some not, and a runtime command that fails to spawn leaves all of them
    // replaced — both reported as "the box is unchanged" while the sources
    // said otherwise.
    let outcome = apply_after_snapshot(runtime.as_ref(), box_name, selection, &publish).await;

    if let Err(e) = outcome {
        if restore_generated(runtime.as_ref(), box_name, &backup).await {
            publish("devbox: generated files restored to the last good selection");
        }
        // And the firewall. A rebuild that failed during activation has
        // already torn the old network stack down, so the box is sitting
        // unrestricted on precisely the path where something went wrong.
        // Composed into one terminal status rather than two competing ones.
        // Publishing the rebuild error separately overwrote this — same event
        // key, last value retained — so the browser showed the *less* severe
        // message. The route only publishes if nothing here did.
        if let Err(policy_err) =
            crate::policy::enforce::restore_after_rebuild(manager, &sandbox, box_name).await
        {
            // Reported as its own terminal status, not folded into the rebuild
            // error: "the rebuild failed" and "and now the firewall is gone
            // too" are different facts, and the second is the one that leaves
            // the box exposed.
            state.publish(ConsoleEvent::new(
                status_event(box_name),
                format!(
                    "<span class=\"term-err\">rebuild failed AND the egress posture \
                     could not be restored — this box is unrestricted: {}</span>",
                    escape_html(&policy_err.to_string())
                ),
            ));
        } else {
            // Only when the firewall did come back. Otherwise the message
            // above — the one that says the box is exposed — stands.
            state.publish(ConsoleEvent::new(
                status_event(box_name),
                format!(
                    "<span class=\"term-err\">{}</span>",
                    escape_html(&e.to_string())
                ),
            ));
        }
        return Err(e);
    }

    // Before persistence, not after.
    //
    // A read-only devbox.toml or a failed state write returned through `?`
    // and the firewall was never reattempted — so a rebuild that succeeded
    // left the box live and unrestricted, reporting only a bookkeeping error.
    // The box is already running the new configuration by this point; getting
    // its firewall back matters more than recording what it is running.
    //
    // A Sets rebuild restarts the box's network stack — toggling `network` or
    // `container` certainly does — which takes devbox's nftables table with
    // it. Reprovision already re-applies the posture for exactly this reason;
    // this path did not, so a box could finish a rebuild reporting `isolated`
    // with open egress.
    // `apply` rather than `apply_saved`: the latter reports and returns Ok by
    // design (ADR-0044), because a *user asking for access* must not be locked
    // out. A rebuild is not that — nobody is waiting at a prompt, and the
    // console is about to print a verdict — so this path wants the strict
    // form, which propagates.
    if let Err(e) = crate::policy::enforce::restore_after_rebuild(manager, &sandbox, box_name).await
    {
        // Terminal status, not a log line among the build output. "rebuild
        // complete" printed underneath a warning nobody scrolled back to read
        // is the console saying the box is fine while its firewall is gone.
        state.publish(ConsoleEvent::new(
            status_event(box_name),
            format!(
                "<span class=\"term-err\">rebuilt, but the egress posture was NOT \
                 restored — this box is running unrestricted: {}</span>",
                escape_html(&e.to_string())
            ),
        ));
        return Err(e).context("rebuild succeeded but the egress posture could not be restored");
    }
    // 3. Only now is the selection real: record it, in both places.
    //
    // `state.json` is devbox's own bookkeeping; `devbox.toml` is the project's
    // source of truth, and box *creation* reads it. Writing only the former
    // meant destroying and recreating a box restored the selection from before
    // the checklist was ever touched — the change survived every restart and
    // vanished on the one operation people use to get a clean box.
    // devbox.toml first. If it fails, `state.json` has not been touched, so
    // the two still agree — on the old selection, which the box no longer
    // has, but a mismatch the user can see and re-apply. Saving state first
    // and failing here would leave devbox reporting the new selection with
    // the project file describing the old one, and a later recreate silently
    // reverting the box.
    let mut sandbox = sandbox;
    sandbox.sets = config.active_sets();
    sandbox.languages = config.active_languages();
    sandbox.packages = selection.packages.iter().cloned().collect();
    config
        .save(&sandbox.project_dir.join("devbox.toml"))
        .context("rebuilt the box, but could not record the selection in devbox.toml")?;
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
