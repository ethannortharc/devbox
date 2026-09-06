//! `devbox repair` — the fixes that need the user's consent.
//!
//! Most of what devbox puts right, it puts right on its own: a stale agent, an
//! sshd that drops the broker's environment, a passwd entry that no longer
//! names the home its login shell uses. Those are safe because they change
//! configuration, and configuration is devbox's to change.
//!
//! What lives here is the other kind: a repair that touches the user's own
//! files. Nothing in this module runs without being asked for by name.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::cli::box_arg::BoxArg;
use crate::runtime::Runtime;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct RepairArgs {
    #[command(subcommand)]
    pub command: RepairCommand,
}

#[derive(Subcommand, Debug)]
pub enum RepairCommand {
    /// Clean up the home directory an older devbox wrote into by mistake
    #[command(name = "stale-home")]
    StaleHome(StaleHomeArgs),
}

#[derive(Args, Debug)]
pub struct StaleHomeArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// List what would happen and stop
    #[arg(long)]
    pub dry_run: bool,

    /// Do not ask before merging, archiving and removing
    #[arg(long)]
    pub yes: bool,
}

/// The settings worth carrying over, in the order they are reported.
///
/// Deliberately short. Everything here is something a person configured and
/// would notice the loss of; everything else in that directory is a cache, a
/// profile symlink, or a package manager's scratch space, and is in the
/// archive if it turns out to have mattered.
const WORTH_MERGING: &[&str] = &[
    ".gitconfig",
    ".claude/settings.json",
    ".codex/config.toml",
    // Named one by one rather than as `.config`. That directory is where most
    // programs put their settings *and* where several put their tokens —
    // `.config/gh/hosts.yml` is a GitHub OAuth token, `.config/gcloud` is a
    // credential store — so copying it wholesale would carry them into the
    // real home under the name of a settings merge. These four are the ones
    // devbox itself writes, so they are the ones it knows are settings.
    ".config/aichat",
    ".config/yazi",
    ".config/opencode/config.json",
    ".config/go",
];

/// Files that are credentials, and are deleted rather than archived.
///
/// v4 copied these into the box; v5 removed that and purges them on every
/// reprovision, because a key sitting in the guest is the thing the broker
/// exists to avoid. A box provisioned before that still has them in the stale
/// home — and archiving one would write it straight back out as a tarball that
/// *stays in the box*, which would make this repair a way of reintroducing the
/// exact leak. So they are removed, and the removal is reported rather than
/// done quietly: the user should know a credential was found and where it was.
const CREDENTIALS: &[&str] = &[
    // What devbox v4 put there itself.
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".devbox-ai-env",
    // And what anything the user ran inside the box may have left. None of
    // these is devbox's doing, but all of them would end up in an archive that
    // stays in the guest, which is the one outcome this repair must not have.
    ".config/gh/hosts.yml",
    ".config/gcloud",
    ".netrc",
    ".npmrc",
    ".docker/config.json",
    ".aws/credentials",
];

pub async fn run(args: RepairArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        RepairCommand::StaleHome(args) => stale_home(args, manager).await,
    }
}

/// Which directory an older devbox wrote into, if the box has one.
///
/// Before W3-3, devbox took the box user's home from `/etc/passwd`. On Lima
/// that is `/home/<user>` while the login shell's home — where the keys, the
/// shell rc and the settings actually are — is `/home/<user>.guest`. So every
/// box provisioned before that fix has a second home directory holding a git
/// config, AI tool settings and a nix profile that nothing in the box reads.
///
/// Only the passwd default is ever a candidate, and only when it is not the
/// real home. Anything else under `/home` belongs to somebody and is not
/// devbox's to tidy.
pub fn stale_home_path(username: &str, real_home: &str) -> Option<String> {
    let candidate = format!("/home/{username}");
    (candidate != real_home.trim_end_matches('/')).then_some(candidate)
}

/// What the guest reported about the stale directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Survey {
    pub files: usize,
    pub bytes: u64,
    /// Relative paths, capped by the probe.
    pub sample: Vec<String>,
}

/// Read the survey command's output.
///
/// Separated from the guest call because a miscount here becomes a number in
/// front of someone deciding whether to delete their files.
pub fn parse_survey(stdout: &str) -> Survey {
    let mut survey = Survey::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("files=") {
            survey.files = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("bytes=") {
            survey.bytes = value.trim().parse().unwrap_or(0);
        } else if let Some(path) = line.strip_prefix("path=") {
            survey.sample.push(path.to_string());
        }
    }
    survey
}

/// How many paths the listing shows before it stops.
const SAMPLE: usize = 20;

/// What the user is shown before being asked.
pub fn describe(stale: &str, survey: &Survey, real_home: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{stale} holds {} file(s), {} in total.",
        survey.files,
        crate::cli::watch::human_bytes(survey.bytes)
    );
    let _ = writeln!(
        out,
        "An older devbox wrote them there; this box reads {real_home} instead."
    );
    out.push('\n');
    for path in survey.sample.iter().take(SAMPLE) {
        let _ = writeln!(out, "  {path}");
    }
    if survey.files > SAMPLE {
        let _ = writeln!(out, "  … and {} more", survey.files - SAMPLE);
    }
    out
}

async fn stale_home(args: StaleHomeArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    let crate::sandbox::Prepared {
        state,
        runtime,
        claim,
    } = manager.prepare_running_for_use(&name).await?;
    let _ = state;

    let (username, real_home) = crate::sandbox::provision::guest_identity(runtime.as_ref(), &name)
        .await
        .with_context(|| format!("find out which home box '{name}' uses"))?;

    let Some(stale) = stale_home_path(&username, &real_home) else {
        println!("Box '{name}' has nothing to clean up: its passwd home is the one it uses.");
        return Ok(());
    };

    let survey = survey_stale(runtime.as_ref(), &name, &stale).await?;
    if survey.files == 0 {
        println!("Box '{name}' has no leftover files in {stale}.");
        return Ok(());
    }

    print!("{}", describe(&stale, &survey, &real_home));
    println!();
    println!("Settings worth keeping are copied into {real_home} — never over a file already");
    println!("there. The rest is archived, then the directory is removed.");
    println!("Any credential left there by devbox v4 is deleted, not archived.");

    if args.dry_run {
        println!("\n--dry-run: nothing was changed.");
        return Ok(());
    }
    if !args.yes && !confirm()? {
        println!("Left alone.");
        return Ok(());
    }

    let archive = format!(
        "{real_home}/.devbox-stale-home-{}.tar.gz",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    apply(
        runtime.as_ref(),
        &name,
        &stale,
        &real_home,
        &username,
        &archive,
    )
    .await?;
    drop(claim);
    println!("Archived to {archive} and removed {stale}.");
    Ok(())
}

fn confirm() -> Result<bool> {
    use std::io::Write as _;
    print!("\nMerge, archive and remove? [y/N] ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

/// Count, measure and sample the stale directory.
async fn survey_stale(runtime: &dyn Runtime, name: &str, stale: &str) -> Result<Survey> {
    let command = format!(
        "set -e; \
         if [ ! -d '{stale}' ]; then echo 'files=0'; exit 0; fi; \
         printf 'files=%s\\n' \"$(find '{stale}' -type f 2>/dev/null | wc -l | tr -d ' ')\"; \
         printf 'bytes=%s\\n' \"$(du -sb '{stale}' 2>/dev/null | cut -f1)\"; \
         find '{stale}' -mindepth 1 -maxdepth 2 2>/dev/null | head -n {SAMPLE} \
           | sed 's|^|path=|'"
    );
    let result = runtime.run_as_root(name, &command, false).await?;
    if result.exit_code != 0 {
        bail!("could not read {stale}: {}", result.stderr.trim());
    }
    Ok(parse_survey(&result.stdout))
}

/// Merge what is worth keeping, archive everything, then remove the directory.
///
/// The archive is of the *whole* directory rather than only the part that was
/// not merged. It is the safety net, and a net with holes cut in it for the
/// files that were merged successfully is worth less than the disk it saves —
/// it is also the only record of what a skipped merge left behind.
async fn apply(
    runtime: &dyn Runtime,
    name: &str,
    stale: &str,
    real_home: &str,
    username: &str,
    archive: &str,
) -> Result<()> {
    for entry in WORTH_MERGING {
        let from = format!("{stale}/{entry}");
        let to = format!("{real_home}/{entry}");
        // `cp -rn`: never over something already there. The report is the
        // point — a silent skip would leave the user believing a setting had
        // been carried over when it had not.
        let command = format!(
            "if [ -e '{from}' ]; then \
               mkdir -p \"$(dirname '{to}')\"; \
               before=$(find '{to}' 2>/dev/null | wc -l | tr -d ' '); \
               cp -rn '{from}' \"$(dirname '{to}')/\" 2>/dev/null || true; \
               after=$(find '{to}' 2>/dev/null | wc -l | tr -d ' '); \
               chown -R {username}:users '{to}' 2>/dev/null || true; \
               if [ \"$before\" = \"$after\" ]; then echo 'kept'; else echo 'merged'; fi; \
             else echo 'absent'; fi"
        );
        let result = runtime.run_as_root(name, &command, false).await?;
        match result.stdout.trim() {
            "merged" => println!("  merged  {entry}"),
            "kept" => println!("  kept    {entry} (already in {real_home}, not overwritten)"),
            _ => {}
        }
    }

    // A gitconfig carried over from the stale home brings whatever it had,
    // and what these had was a `[credential]` section pointing at a helper on
    // the *host* — `!/opt/homebrew/bin/gh auth git-credential`, a path that
    // does not exist in the box. Not a leak, but every credential lookup in
    // the box then fails on a binary that was never there. Provisioning has
    // stripped those sections since v5; the merge goes through the same
    // function rather than a second copy of the rule.
    let gitconfig = format!("{real_home}/.gitconfig");
    let read = runtime
        .exec_cmd(name, &["cat", &gitconfig], false)
        .await
        .ok()
        .filter(|result| result.exit_code == 0)
        .map(|result| result.stdout);
    if let Some(before) = read {
        let after = crate::sandbox::provision::strip_credential_sections(&before);
        if after != before {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(after.as_bytes());
            let write = format!("printf %s '{encoded}' | base64 -d > '{gitconfig}'");
            if runtime.run_as_root(name, &write, false).await.is_ok() {
                println!("  stripped a [credential] section from the merged .gitconfig");
            }
        }
    }

    // Before the archive, never after: a credential that reached the tarball
    // is a credential this repair put back into the box.
    for entry in CREDENTIALS {
        let path = format!("{stale}/{entry}");
        let command = format!(
            "if [ -e '{path}' ]; then rm -f '{path}' && echo removed; else echo absent; fi"
        );
        let result = runtime.run_as_root(name, &command, false).await?;
        if result.stdout.trim() == "removed" {
            println!("  deleted {entry} (a credential — not archived)");
        }
    }

    let command = format!(
        "set -e; \
         tar czf '{archive}' -C \"$(dirname '{stale}')\" \"$(basename '{stale}')\"; \
         chown {username}:users '{archive}'; \
         chmod 600 '{archive}'; \
         rm -rf '{stale}'"
    );
    let result = runtime.run_as_root(name, &command, false).await?;
    if result.exit_code != 0 {
        bail!(
            "nothing was removed: the archive could not be written: {}",
            result.stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Lima shape this exists for: passwd said one thing, the login shell
    /// another, and devbox wrote into the first for months.
    #[test]
    fn the_passwd_default_is_the_candidate_when_it_is_not_the_real_home() {
        assert_eq!(
            stale_home_path("ethan", "/home/ethan.guest").as_deref(),
            Some("/home/ethan")
        );
        assert_eq!(
            stale_home_path("dev", "/home/dev/").as_deref(),
            None,
            "a trailing slash is the same directory"
        );
    }

    /// A box whose two homes agree has nothing to clean, and must not be
    /// offered a deletion of the directory it is actually living in.
    #[test]
    fn a_box_whose_homes_agree_has_no_candidate() {
        assert_eq!(stale_home_path("dev", "/home/dev"), None);
        assert_eq!(stale_home_path("ethan", "/home/ethan"), None);
    }

    #[test]
    fn the_survey_reads_counts_and_a_sample() {
        let survey = parse_survey(
            "files=50\nbytes=317718528\npath=/home/ethan/.gitconfig\npath=/home/ethan/.npm\n",
        );
        assert_eq!(survey.files, 50);
        assert_eq!(survey.bytes, 317_718_528);
        assert_eq!(survey.sample.len(), 2);
    }

    /// A box that answered nothing must read as "nothing to do", never as
    /// "zero bytes, go ahead".
    #[test]
    fn an_unanswered_survey_is_empty() {
        assert_eq!(parse_survey(""), Survey::default());
        assert_eq!(parse_survey("files=0\n").files, 0);
    }

    /// The listing is what someone reads before agreeing to delete their
    /// files, so it has to carry the size, the count, and where the box reads
    /// instead.
    #[test]
    fn the_listing_says_how_much_and_where_the_box_really_reads() {
        let survey = Survey {
            files: 50,
            bytes: 317_718_528,
            sample: vec!["/home/ethan/.gitconfig".into()],
        };
        let text = describe("/home/ethan", &survey, "/home/ethan.guest");
        assert!(text.contains("50 file(s)"), "{text}");
        assert!(text.contains("303"), "{text}");
        assert!(text.contains("/home/ethan.guest"), "{text}");
        assert!(text.contains("/home/ethan/.gitconfig"), "{text}");
    }

    /// A credential must never appear in the merge list, and must never be
    /// reachable by merging a directory that contains one.
    ///
    /// The second half is the one that bites. `.config` was on the merge list
    /// until W4-5, and `.config/gh/hosts.yml` is a GitHub OAuth token: merging
    /// the directory would have copied the token into the real home and called
    /// it a settings merge.
    #[test]
    fn no_credential_is_carried_over_or_archived() {
        assert!(!CREDENTIALS.is_empty());
        for credential in CREDENTIALS {
            assert!(!WORTH_MERGING.contains(credential), "{credential}");
            for merged in WORTH_MERGING {
                assert!(
                    !credential.starts_with(&format!("{merged}/")),
                    "merging {merged} would carry {credential}"
                );
                // And the other direction: a merged path must not sit inside
                // something being deleted as a credential, or the merge would
                // read from a directory that is about to go.
                assert!(
                    !merged.starts_with(&format!("{credential}/")),
                    "{merged} lives inside {credential}, which is deleted"
                );
            }
        }
    }

    /// Nothing on the merge list may be a whole directory that other programs
    /// also write into. `.config` is the example; the entries under it name
    /// one program each.
    #[test]
    fn the_merge_list_never_names_a_shared_directory() {
        assert!(!WORTH_MERGING.contains(&".config"));
        assert!(!WORTH_MERGING.contains(&".claude"));
        assert!(!WORTH_MERGING.contains(&".codex"));
        for merged in WORTH_MERGING {
            assert_ne!(*merged, ".", "{merged}");
            assert!(!merged.starts_with('/'), "{merged}");
        }
    }

    /// Long listings stop, and say that they stopped.
    #[test]
    fn a_long_listing_is_cut_off_and_says_so() {
        let survey = Survey {
            files: 500,
            bytes: 1,
            sample: (0..40).map(|i| format!("/home/ethan/f{i}")).collect(),
        };
        let text = describe("/home/ethan", &survey, "/home/ethan.guest");
        assert_eq!(text.matches("/home/ethan/f").count(), SAMPLE);
        assert!(text.contains("and 480 more"), "{text}");
    }
}
