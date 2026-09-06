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
///
/// Fallible, and that is the point. A `cat` that fails records *nothing* about
/// why, and "nothing" used to be stored as `None` — the same value that means
/// "this file did not exist". Rollback reads `None` as absent and runs `rm -f`,
/// so a guest transport hiccup or a permissions error during the snapshot
/// turned a rollback into a deletion of the last good generated sources.
///
/// Existence is probed separately from reading, so the two answers cannot be
/// confused: an absent file is a fact worth recording, and an unreadable one is
/// a reason to stop before anything is mutated.
pub async fn snapshot_generated(
    runtime: &dyn crate::runtime::Runtime,
    box_name: &str,
) -> Result<Generated> {
    let mut out = Vec::new();
    for path in GENERATED_FILES {
        // Does it exist? A transport failure here is not an answer.
        let probe = runtime
            .exec_cmd(box_name, &["test", "-e", path], false)
            .await
            .with_context(|| {
                format!(
                    "could not check whether {path} exists in box '{box_name}', so a \
                     rollback could not tell an absent file from an unread one — \
                     refusing to rebuild rather than risk deleting it"
                )
            })?;
        if probe.exit_code != 0 {
            out.push((*path, None)); // genuinely absent
            continue;
        }

        let read = runtime
            .exec_cmd(box_name, &["cat", path], false)
            .await
            .with_context(|| format!("could not read {path} in box '{box_name}'"))?;
        if read.exit_code != 0 {
            bail!(
                "{path} exists in box '{box_name}' but could not be read (exit \
                 {}), so a failed rebuild could not put it back. Refusing to \
                 rebuild rather than risk deleting it.",
                read.exit_code
            );
        }
        out.push((*path, Some(read.stdout)));
    }

    // The per-set modules too, as a tarball. Their names are not a fixed list
    // — the catalog changes — so copying the directory is the only honest way
    // to put it back exactly as it was.
    // Elevated, and its failure recorded. `/etc/devbox/sets` is root-owned,
    // and on a VM runtime these commands run as the ordinary guest user — so
    // an unprivileged `tar` silently produced no backup, and the restore then
    // reported success without restoring anything.
    // Existence first, and fallibly.
    //
    // `is_ok_and(exit_code == 0)` turned every transport failure into `false`,
    // and `false` here means "the directory was not there" — which `restore`
    // acts on by removing it and its backup. So a probe that merely failed to
    // run made the rollback delete a set-module directory that existed.
    //
    // The same defect as the per-file snapshot above, in the same function, and
    // it survived that fix because the fix was applied to the instance that was
    // reported rather than to the pair the function actually returns.
    let probe = runtime
        .exec_cmd(
            box_name,
            &["sh", "-c", &format!("[ -d {GENERATED_SETS_DIR} ]")],
            false,
        )
        .await
        .with_context(|| {
            format!(
                "could not check for {GENERATED_SETS_DIR} in box '{box_name}', so a \
                 rollback could not tell an absent directory from an unchecked one — \
                 refusing to rebuild rather than risk deleting it"
            )
        })?;
    let existed = probe.exit_code == 0;

    // Elevated, and its failure fatal. `/etc/devbox/sets` is root-owned, and on
    // a VM runtime these commands run as the ordinary guest user — so an
    // unprivileged `tar` silently produced no backup, and the restore then
    // reported success without restoring anything.
    //
    // Recording that and carrying on was the earlier repair. It is not enough:
    // a rebuild whose rollback cannot put the modules back is a rebuild with no
    // way home, and starting it anyway only moves the failure somewhere less
    // recoverable.
    let archived = if existed {
        let result = runtime
            .exec_cmd(
                box_name,
                &[
                    "sh",
                    "-c",
                    &crate::policy::enforce::elevated(&format!(
                        "rm -f {SETS_BACKUP} && \
                         tar cf {SETS_BACKUP} -C {GENERATED_SETS_DIR} ."
                    )),
                ],
                false,
            )
            .await
            .with_context(|| {
                format!("could not archive {GENERATED_SETS_DIR} in box '{box_name}'")
            })?;
        if result.exit_code != 0 {
            bail!(
                "{GENERATED_SETS_DIR} exists in box '{box_name}' but could not be \
                 archived (exit {}: {}), so a failed rebuild could not put the set \
                 modules back. Refusing to rebuild rather than proceed without a \
                 rollback.",
                result.exit_code,
                result.stderr.trim()
            );
        }
        true
    } else {
        // Nothing to archive, which is a first or legacy box rather than a
        // failure.
        false
    };

    Ok(Generated {
        files: out,
        sets_archived: archived,
        sets_existed: existed,
    })
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

    let rebuild = crate::nix::rebuild::rebuild_argv();
    let rebuild: Vec<&str> = rebuild.iter().map(String::as_str).collect();
    let argv = runtime.argv(box_name, &rebuild, false);
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
    // The form posts names, because names are what the field shows. Where an
    // aliased package comes from lives on the box, so it is reattached before
    // anything is validated or written — validating the selection first would
    // check the alias against itself and pass.
    //
    // Through the same fallback the page used to render the form, not from
    // `sandbox.package_sources` directly. On a box predating those state
    // fields the map is empty, so taking it raw discarded the sources the
    // detail route had just recovered from `devbox.toml` — `my-tf` resolved to
    // `pkgs.my-tf`, the module filtered it out, and the rebuild reported
    // success while Terraform vanished from a box whose UI still showed it
    // selected. Round 30 fixed this on the CLI path and left this one.
    // Across processes, not just across handlers. The `AppState` guard the
    // route took only knows about this console.
    let claim = claim_box(&manager.state_dir, box_name)?;
    // Re-read under it. `sandbox` was fetched before the claim, and a
    // `devbox use` finishing in that gap releases its own claim — so this one
    // succeeds over a snapshot naming the project the box has just left, and
    // the rebuild would use the old project's selection and save its
    // `project_dir` back over the new one.
    let sandbox = manager.get_sandbox(box_name)?;

    let project = DevboxConfig::load_or_default(&sandbox.project_dir);
    let recovered = Selection::from_state_and_project(&sandbox, &project);
    let selection = &selection.clone().with_sources(recovered.sources);
    selection.validate()?;

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
    // Already holding the claim for this box, so the claiming form would
    // refuse itself.
    crate::web::service::ensure_running_holding_claim(manager, box_name, &claim).await?;

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
    // Before anything is mutated. A snapshot that could not be taken is not a
    // reason to proceed carefully — it is a reason not to proceed, because the
    // rollback this rebuild depends on would be working from a guess.
    let backup = snapshot_generated(runtime.as_ref(), box_name).await?;
    // Fallibly, and *before* the box is touched. `load_or_default` turns a
    // malformed devbox.toml into defaults, and step 3 writes the derived
    // config back over the original — so a syntax error anywhere in the file
    // would silently discard the user's mounts, resources, environment, and
    // policy.
    // The value is deliberately discarded: what this call is for is the
    // refusal. The config the selection is projected onto is read again after
    // the rebuild, because this one is about to go stale — see step 3.
    DevboxConfig::load_for_edit(&sandbox.project_dir).context(
        "refusing to apply a selection: this box's devbox.toml cannot be read, and \
         applying would overwrite it with defaults",
    )?;

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
            crate::policy::enforce::restore_after_rebuild(manager, box_name, &claim).await
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
    // The strict rebuild form uses the claim already held by this worker and
    // propagates any failure, so the console cannot report success while the
    // saved egress posture is absent.
    if let Err(e) = crate::policy::enforce::restore_after_rebuild(manager, box_name, &claim).await {
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
    // Read the project file again, now, and project the selection onto *that*.
    //
    // The copy read before the rebuild is minutes old by this point and the
    // console stayed live throughout, so a policy saved in the meantime is
    // already in devbox.toml — and writing the old copy back would silently
    // revert it. Not a general lost edit: specifically the posture, which
    // `restore_after_rebuild` has just applied to the box from the *new*
    // value a few lines above. The firewall would be enforcing what the user
    // asked for while the file said the opposite, and the next thing to read
    // the file would undo the firewall.
    //
    // Only the selection's own fields are projected, so anything else edited
    // during the rebuild survives.
    let latest = DevboxConfig::load_for_edit(&sandbox.project_dir).context(
        "rebuilt the box, but its devbox.toml can no longer be read, so the new \
         selection could not be recorded",
    )?;
    let config = selection.to_config(&latest);

    let mut sandbox = sandbox;
    sandbox.sets = config.active_sets();
    sandbox.languages = config.active_languages();
    sandbox.packages = selection.packages.iter().cloned().collect();
    // And their sources, from the config this selection was composed against.
    // Keeping them on the box is what survives a later `devbox use`.
    sandbox.package_sources = config
        .custom_packages
        .iter()
        .filter(|(_, source)| source.as_str() != "nixpkgs")
        .map(|(name, source)| (name.clone(), source.clone()))
        .collect();
    {
        // Read-modify-write under one claim, so a policy saved between the
        // read above and this write is not reverted by it.
        //
        // Off the worker: this waits for the holder, and the holder here is
        // another request that is itself `await`ing. A rebuild reaching
        // persistence while a policy save holds the project lock parked a
        // Tokio worker, which a single-worker runtime never recovers from —
        // the holder can only finish on a worker that is now blocked on it.
        let lock_dir = manager.state_dir.clone();
        let lock_project = sandbox.project_dir.clone();
        let _edit =
            claim_project_off_worker(move || claim_project(&lock_dir, &lock_project)).await?;
        let latest = DevboxConfig::load_for_edit(&sandbox.project_dir).context(
            "rebuilt the box, but its devbox.toml can no longer be read, so the new \
             selection could not be recorded",
        )?;
        let persisted = selection.to_config(&latest);
        sandbox.sets = persisted.active_sets();
        sandbox.languages = persisted.active_languages();
        sandbox.package_sources = persisted
            .custom_packages
            .iter()
            .filter(|(_, source)| source.as_str() != "nixpkgs")
            .map(|(name, source)| (name.clone(), source.clone()))
            .collect();
        manager
            .save_config_and_state(&persisted, &sandbox)
            .context("rebuilt the box, but could not atomically record the new selection")?;
    }

    state.publish(ConsoleEvent::new(
        status_event(box_name),
        "<span class=\"term-ok\">rebuild complete</span>".to_string(),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runtime::stub::StubRuntime;
    use crate::runtime::{ExecResult, SandboxStatus};

    /// A guest whose `cat` fails for a reason that is not "the file is absent":
    /// `test -e` says the file is there, and reading it does not work.
    fn flaky_guest(read_fails: bool) -> StubRuntime {
        StubRuntime::new()
            .with_name("flaky")
            .with_status(SandboxStatus::Running)
            .with_exec_cmd(move |_: &str, argv: &[&str], _: bool| {
                match argv.first().copied() {
                    // Present.
                    Some("test") => Ok(ExecResult {
                        exit_code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                    Some("cat") if read_fails => Ok(ExecResult {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "permission denied".into(),
                    }),
                    Some("cat") => Ok(ExecResult {
                        exit_code: 0,
                        stdout: "contents".into(),
                        stderr: String::new(),
                    }),
                    // The sets tarball and anything else.
                    _ => Ok(ExecResult {
                        exit_code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                }
            })
    }

    #[tokio::test]
    async fn a_file_that_exists_but_cannot_be_read_stops_the_rebuild() {
        // `None` in the snapshot means "this file was absent", and rollback
        // acts on that with `rm -f`. A failed read recorded the same value, so
        // a permissions error or a guest transport hiccup during the snapshot
        // turned the rollback into a deletion of the last good generated
        // sources — the exact thing the snapshot exists to protect.
        //
        // Refusing before anything is mutated is the only safe answer: there is
        // no rollback to fall back on if the rollback is the thing that is
        // broken.
        let guest = flaky_guest(true);
        let err = match snapshot_generated(&guest, "b").await {
            Ok(_) => panic!("an unreadable file must stop the rebuild"),
            Err(e) => e,
        };
        let text = err.to_string();
        assert!(
            text.contains("could not be read") && text.contains("Refusing"),
            "the refusal should say what and why: {text}"
        );
    }

    #[tokio::test]
    async fn a_readable_guest_still_snapshots() {
        let guest = flaky_guest(false);
        let snap = match snapshot_generated(&guest, "b").await {
            Ok(snap) => snap,
            Err(e) => panic!("a healthy guest must snapshot: {e}"),
        };
        assert!(
            snap.files.iter().any(|(_, c)| c.is_some()),
            "expected contents to be captured"
        );
    }

    #[test]
    fn a_lock_path_is_one_flat_file_per_box() {
        // Box names are directory names: they admit `/` and `.`, so a path
        // built from one raw would escape the lock directory or collide with
        // another box's.
        let dir = std::path::Path::new("/tmp/devbox-state");

        let a = super::rebuild_lock_path(dir, "a/b");
        assert_eq!(a.parent().unwrap(), dir.join("locks"), "no escaping: {a:?}");
        assert!(!a.file_name().unwrap().to_str().unwrap().contains('/'));

        // Same box, same lock; different boxes, different locks — including
        // names that differ only in a character the encoding has to preserve.
        assert_eq!(a, super::rebuild_lock_path(dir, "a/b"));
        assert_ne!(a, super::rebuild_lock_path(dir, "a.b"));
        assert_ne!(
            super::rebuild_lock_path(dir, "alpha"),
            super::rebuild_lock_path(dir, "beta")
        );
    }

    #[test]
    fn claiming_a_rebuild_creates_the_lock_where_it_says() {
        // The claim itself: whether two *processes* exclude each other is the
        // platform's advisory locking, which this cannot exercise — std
        // documents intra-process re-locking as platform-dependent, so a test
        // asserting it would pin an accident. What is checked is that the
        // claim succeeds and lands on the path the other half computes.
        let dir = tempfile::tempdir().unwrap();
        let _held = super::claim_box(dir.path(), "alpha").expect("first claim");
        assert!(super::rebuild_lock_path(dir.path(), "alpha").exists());

        // And a second box is never blocked by the first.
        let _other = super::claim_box(dir.path(), "beta").expect("a different box");
    }

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

/// Exclusive claim on one box's lifecycle, held across processes.
///
/// Refuses immediately when another holder has it: a second rebuild of the same
/// box is a mistake to report, not a queue to join.
///
/// Also a *capability*. Functions that change what a box enforces take one by
/// reference, so a call site that does not hold the claim cannot be written —
/// which is the difference between an invariant and a habit. Nine of eighteen
/// policy applications had no claim when this was documentation.
///
/// The lock is the open file: closing it releases the advisory lock, so
/// dropping this releases the claim however the caller left — including on a
/// panic, and including when the process dies without unwinding, which is the
/// case a lock file containing a pid gets wrong.
#[derive(Debug)]
pub struct BoxClaim {
    /// Which box this is a claim on.
    ///
    /// Carried so a function handed a claim can check it is the right one. A
    /// claim on box A proves nothing about box B, and without this the type
    /// would accept either.
    box_name: String,
    _file: std::fs::File,
}

impl BoxClaim {
    /// The box this claim covers.
    pub fn box_name(&self) -> &str {
        &self.box_name
    }
}

/// Exclusive claim on one project's `devbox.toml`, held across processes.
///
/// *Waits* for the holder, unlike [`BoxClaim`] — a read-modify-write of one
/// file is short and refusing it would be a spurious failure.
///
/// A separate type on purpose. Both were `RebuildLock`, so nothing in the
/// signature distinguished a claim that refuses from one that blocks, and the
/// two were repeatedly reasoned about as if they were one thing: two paths were
/// once "fixed" for a deadlock that only the waiting one could have. Types that
/// behave differently under contention should not be interchangeable.
#[derive(Debug)]
pub struct ProjectClaim {
    _file: std::fs::File,
}

/// Claim the right to rebuild `box_name`, or say who has it.
///
/// The console's in-memory guard only ever protected one `AppState`. A
/// `devbox sets apply` running beside an open console — or a second `devbox
/// web` — had its own rebuild slot or none at all, so both could snapshot the
/// same generated files, overwrite them, rebuild, and then persist different
/// selections. What is left is a `state.json` and a `devbox.toml` describing a
/// generation that was never activated, and the box running one neither of
/// them names.
///
/// The lock lives beside the state rather than in the box, because the
/// contention is between *host* processes and the box may not even be running
/// when one of them starts.
/// Where one box's rebuild lock lives.
///
/// A box name is a directory name and admits `/` and `.`; built raw, the path
/// would escape the lock directory or collide with another box's. The
/// console's URL-segment encoding gives a flat unambiguous filename, and it is
/// already the agreed way to make this name safe somewhere else.
///
/// Separated from the locking because this half is decidable on any host and
/// the other half is not: whether two *processes* exclude each other is a
/// property of the platform's advisory locks, and std documents intra-process
/// re-locking as platform-dependent — so a unit test that asserted it would be
/// pinning an accident rather than the contract.
pub fn rebuild_lock_path(state_dir: &std::path::Path, box_name: &str) -> std::path::PathBuf {
    state_dir.join("locks").join(format!(
        "rebuild-{}.lock",
        crate::web::encode_segment(box_name)
    ))
}

/// Claim a project's `devbox.toml` for one read-modify-write.
///
/// Every writer of that file re-reads it, changes one section, and writes the
/// whole thing back — so two writers interleaving means one section is
/// silently reverted. The Sets rebuild records a selection while the Policy
/// tab records a posture, and each was overwriting whichever the other had
/// just saved, leaving the file, the saved state, and the live firewall
/// describing three different boxes.
///
/// **The lock must be taken before the load, not before the save.** Loading
/// first and locking second changes nothing: the copy in hand is already
/// stale, and writing it under a lock overwrites the newer file just as
/// surely. The whole read-modify-write belongs inside.
///
/// Distinct from the rebuild lock, and deliberately short-lived. Holding the
/// rebuild lock for a policy edit would block it for the minutes a
/// `nixos-rebuild` takes — and a policy edit *during* a rebuild is exactly the
/// case that has to keep working.
///
/// Kept in the state directory, not the project. The first version put a
/// zero-byte `.devbox.toml.lock` in the user's repository and never removed
/// it, so ordinary use left an untracked file in `git status` — and a
/// read-only or unusual checkout could not be edited at all. devbox's own
/// directory is where devbox's own bookkeeping goes.
pub fn claim_project(
    state_dir: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<ProjectClaim> {
    let dir = state_dir.join("locks");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;

    // Keyed by the canonical path, so two boxes sharing a project share the
    // claim and a relative path does not become a second lock for one file.
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let path = dir.join(format!("project-{}.lock", project_key(&canonical)));

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    // Blocking, unlike the rebuild claim: this is held for one file rewrite,
    // so waiting is right where refusing would be a spurious failure.
    file.lock()
        .with_context(|| format!("could not lock {}", path.display()))?;
    Ok(ProjectClaim { _file: file })
}

/// A filename-safe key for a project directory.
fn project_key(path: &std::path::Path) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Take a lock from async code without blocking the runtime.
///
/// These are OS file locks, and acquiring a held one *blocks the calling
/// thread* until it is released. In the console that thread is a Tokio worker
/// and the holder is another request which is itself `await`ing — so a
/// single-worker runtime deadlocks on the first overlap, and a multi-worker one
/// starves into the same state once enough saves overlap. Neither recovers,
/// because the holder can only finish on a worker that is now blocked waiting
/// for it.
///
/// The CLI does not need this and does not use it: there, blocking the one
/// command until the lock is free is exactly the intended behaviour.
pub async fn claim_project_off_worker<F>(acquire: F) -> Result<ProjectClaim>
where
    F: FnOnce() -> Result<ProjectClaim> + Send + 'static,
{
    tokio::task::spawn_blocking(acquire)
        .await
        .context("the task waiting for a devbox lock was cancelled")?
}

pub fn claim_box(state_dir: &std::path::Path, box_name: &str) -> Result<BoxClaim> {
    match try_claim_box(state_dir, box_name)? {
        Some(claim) => Ok(claim),
        None => bail!(
            "another devbox process is already rebuilding box '{box_name}'.\n  \
             Two rebuilds of one box overwrite each other's generated files and \
             then record different selections, so this one is refused rather \
             than run. Wait for the other to finish."
        ),
    }
}

/// The same, telling contention apart from a broken lock directory.
///
/// `Ok(None)` means someone else holds it — expected, and the only outcome a
/// caller is entitled to shrug at. `Err` means the claim could not be evaluated
/// at all: an unwritable state directory, a filesystem that will not lock.
///
/// The distinction exists because a caller that stepped aside on contention was
/// also stepping aside on those. An unwritable lock directory silently became
/// "a rebuild is running", so `attach`, `exec` and `code` carried on and applied
/// no egress policy at all — the failure that most needs saying out loud,
/// reported as the one that needs nothing.
///
/// `TryLockError` draws the line for us: `WouldBlock` is contention, and
/// anything else is the lock system itself failing.
pub fn try_claim_box(state_dir: &std::path::Path, box_name: &str) -> Result<Option<BoxClaim>> {
    let dir = state_dir.join("locks");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;

    let path = rebuild_lock_path(state_dir, box_name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(BoxClaim {
            box_name: box_name.to_string(),
            _file: file,
        })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::Error::new(e))
            .with_context(|| format!("could not evaluate the claim on box '{box_name}'")),
    }
}

#[cfg(test)]
mod lock_audit {
    /// No `async fn` may take the *waiting* devbox lock on its own thread.
    ///
    /// `lock_project_config` calls `File::lock`, which blocks the calling
    /// thread until the holder releases. In the console that thread is a Tokio worker, and
    /// the holder is either another request that is itself `await`ing or a
    /// rebuild that runs for minutes. A single-worker runtime deadlocks on the
    /// first overlap; a larger one starves once enough overlap, and neither
    /// recovers, because the holder can only finish on a worker that is now
    /// blocked waiting for it.
    ///
    /// Round 41 reported the one instance there was. An attempt to widen that
    /// to "every lock" was wrong: `lock_rebuild` is a `try_lock` and refuses
    /// rather than waits, so two paths were changed for a defect they never
    /// had. The check is narrow on purpose, and says why.
    ///
    /// Synchronous callers are fine and are the point of the exemption: in the
    /// CLI, blocking the one command until the lock frees is the intended
    /// behaviour.
    #[test]
    fn no_async_path_blocks_a_worker_on_a_lock() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&src, &mut files);

        let mut offenders = Vec::new();
        for path in files {
            // No exemption for this file, deliberately.
            //
            // It had one — "this file defines the locks and the escape hatch" —
            // and that is how round 42 found a real instance here: `build.rs`
            // both defines the locks *and* calls one inside an `async fn`, so
            // the guard written to catch exactly that had excused the file it
            // lived in. None is needed. The definition of
            // `lock_project_config` is not `async`, and `lock_blocking` takes a
            // closure rather than naming the lock, so neither trips the check
            // on its own.
            //
            // An exemption scoped to a file rather than to the lines that need
            // it hides whatever else that file comes to contain.
            // The CLI is exempt, and the exemption is the whole distinction.
            //
            // Its commands are `async` because the runtime APIs are, not
            // because anything else is being served: one process, one job, and
            // blocking it until the lock frees is precisely what the user
            // asked for when they ran a command against a box that is mid
            // rebuild. The console is the opposite — the thread it would block
            // is one it needs to answer every other request, including the
            // stream that would show the rebuild finishing.
            if path.components().any(|c| c.as_os_str() == "cli") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable source");
            let lines: Vec<&str> = text.lines().collect();
            let mut in_async = false;

            for (n, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                // Track the enclosing function. Indentation is the signal that
                // a new item started, which is enough for this codebase's
                // layout and is why the check is a lint and not a proof.
                if trimmed.starts_with("pub async fn ") || trimmed.starts_with("async fn ") {
                    in_async = true;
                } else if trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("fn ")
                    || trimmed.starts_with("impl ")
                {
                    in_async = false;
                }
                if trimmed.starts_with("//") || !in_async {
                    continue;
                }
                // `lock_project_config` only. It calls `File::lock`, which
                // waits for the holder; `lock_rebuild` calls `try_lock` and
                // refuses at once with a conflict, so it cannot park anything.
                //
                // That distinction is the whole content of this check, and
                // getting it wrong is how two paths were "fixed" for a defect
                // they never had. If `lock_rebuild` ever starts waiting, add
                // it here — and not before.
                let takes_lock = trimmed.contains("claim_project(");
                // Inside the `lock_blocking` closure is exactly where these
                // belong, so a line that mentions both is correct.
                if takes_lock && !line.contains("claim_project_off_worker") {
                    // Code only. The first version of this looked at the raw
                    // preceding lines, and the comment above the call site
                    // explains the fix by *naming* `lock_blocking` — so the
                    // prose satisfied the check and the guard passed on code
                    // that had been reverted. A guard defeated by its own
                    // explanation is worse than no guard: it reports the thing
                    // it was written to catch as absent.
                    let code_above: String = lines[n.saturating_sub(4)..n]
                        .iter()
                        .filter(|l| !l.trim_start().starts_with("//"))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !code_above.contains("claim_project_off_worker") {
                        offenders.push(format!("{}:{}: {}", path.display(), n + 1, trimmed));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these take an OS file lock directly inside an `async fn`, which \
             blocks a Tokio worker until the holder releases — and the holder \
             may be a rebuild that runs for minutes. Wrap them in \
             `build::claim_project_off_worker`:\n{}",
            offenders.join("\n")
        );
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
}
