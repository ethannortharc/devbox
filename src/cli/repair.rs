//! `devbox repair` — the fixes that need the user's consent.
//!
//! Most of what devbox puts right, it puts right on its own: a stale agent, an
//! sshd that drops the broker's environment, a passwd entry that no longer
//! names the home its login shell uses. Those are safe because they change
//! configuration, and configuration is devbox's to change.
//!
//! What lives here is the other kind: a repair that touches the user's own
//! files. Nothing in this module runs without being asked for by name.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

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

    /// Archive and merge, but leave the directory in the box
    ///
    /// Credentials are still deleted: they are never archived and never
    /// merged, so keeping them would only keep the leak.
    #[arg(long)]
    pub keep: bool,
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

    let host_path = archive_path(&manager.state_dir, &name, chrono::Utc::now());

    print!("{}", describe(&stale, &survey, &real_home));
    println!();
    println!("Settings worth keeping are copied into {real_home} — never over a file already");
    println!(
        "there. Everything is archived to {} on this",
        host_path.display()
    );
    println!("machine, not inside the box, and only then is the directory removed.");
    println!("Any credential found there is deleted rather than archived.");
    if args.keep {
        println!();
        println!("--keep: the directory stays in the box. Credentials are still deleted —");
        println!("they are neither archived nor merged, so keeping them would keep only the leak.");
    }

    if args.dry_run {
        println!("\n--dry-run: nothing was changed.");
        return Ok(());
    }
    if !args.yes && !confirm()? {
        println!("Left alone.");
        return Ok(());
    }

    // Named after the archive it becomes, so a pack left behind by a crash is
    // recognisable rather than mysterious.
    let guest_pack = format!(
        "/tmp/{}",
        host_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "devbox-stale-home.tar.gz".to_string())
    );

    apply(runtime.as_ref(), &name, &stale, &real_home, &username).await?;
    let archived =
        archive_to_host(runtime.as_ref(), &name, &stale, &guest_pack, &host_path).await?;
    std::fs::set_permissions(&host_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect {}", host_path.display()))?;

    println!(
        "archived to {} ({})",
        host_path.display(),
        archived.describe()
    );

    if args.keep {
        println!("{stale} was left in the box (--keep).");
    } else {
        let removed = runtime
            .run_as_root(&name, &format!("rm -rf '{stale}'"), false)
            .await?;
        if removed.exit_code != 0 {
            bail!(
                "the archive is safe on the host, but {stale} could not be removed from the \
                 box: {}",
                removed.stderr.trim()
            );
        }
        println!("removed {stale} from the box.");
    }
    drop(claim);
    Ok(())
}

/// Where a box's stale-home archive goes on the host.
///
/// `<state_dir>/archives/`, not `boxes/<name>/`: that directory is removed
/// with the box, and this is user data rescued *from* the box — it has to
/// outlive `devbox destroy`, which is exactly the command someone reaches for
/// after deciding the box is beyond saving.
///
/// The timestamp is UTC and sorts lexically, so a directory of these reads in
/// the order they were made whatever the host's locale does.
pub fn archive_path(
    state_dir: &Path,
    box_name: &str,
    when: chrono::DateTime<chrono::Utc>,
) -> PathBuf {
    state_dir.join("archives").join(format!(
        "{box_name}-stale-home-{}.tar.gz",
        when.format("%Y%m%dT%H%M%SZ")
    ))
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

/// Merge what is worth keeping, and delete what must never leave the box.
///
/// Everything here is reversible-ish and cheap; the archive and the removal
/// are the parts that are not, and they happen after this returns.
async fn apply(
    runtime: &dyn Runtime,
    name: &str,
    stale: &str,
    real_home: &str,
    username: &str,
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
    // is a credential this repair put back into the box — and now that the
    // tarball leaves the box, one that reached it would be on the host too.
    for entry in CREDENTIALS {
        let path = format!("{stale}/{entry}");
        let command = format!(
            "if [ -e '{path}' ]; then rm -rf '{path}' && echo removed; else echo absent; fi"
        );
        let result = runtime.run_as_root(name, &command, false).await?;
        if result.stdout.trim() == "removed" {
            println!("  deleted {entry} (a credential — not archived)");
        }
    }

    Ok(())
}

/// Pack the stale directory in the box, bring it to the host, and prove it
/// arrived intact.
///
/// Nothing in the box is removed here. The caller does that, and only after
/// this has returned — which is the whole ordering: an archive that is still
/// only inside the box is not an archive, and a directory deleted before the
/// copy is verified is data destroyed on the strength of a transfer nobody
/// checked.
///
/// On any failure the box is left exactly as it was, the temporary pack is
/// removed from it, and a half-written file on the host is removed too. A
/// partial `.tar.gz` sitting in `~/.devbox/archives` would look like a
/// successful rescue.
async fn archive_to_host(
    runtime: &dyn Runtime,
    name: &str,
    stale: &str,
    guest_pack: &str,
    host_path: &Path,
) -> Result<Archived> {
    let pack = format!(
        "set -e; \
         rm -f '{guest_pack}'; \
         tar czf '{guest_pack}' -C \"$(dirname '{stale}')\" \"$(basename '{stale}')\"; \
         printf 'bytes=%s\\n' \"$(stat -c %s '{guest_pack}')\"; \
         printf 'sha=%s\\n' \"$(sha256sum '{guest_pack}' | cut -d' ' -f1)\""
    );
    let packed = runtime.run_as_root(name, &pack, false).await?;
    if packed.exit_code != 0 {
        bail!(
            "nothing was changed: the archive could not be built in the box: {}",
            packed.stderr.trim()
        );
    }
    let Some(expected) = Archived::parse(&packed.stdout) else {
        let _ = remove_guest_pack(runtime, name, guest_pack).await;
        bail!(
            "nothing was changed: the box did not report the archive's size and checksum, so \
             the copy could not have been verified"
        );
    };

    if let Some(parent) = host_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect {}", parent.display()))?;
    }

    let transfer = runtime.copy_from(name, guest_pack, host_path).await;
    let verdict = transfer
        .and_then(|()| measure_host_file(host_path))
        .and_then(|actual| {
            if actual == expected {
                Ok(actual)
            } else {
                Err(anyhow::anyhow!(
                    "the copy does not match what the box packed ({} vs {})",
                    actual.describe(),
                    expected.describe()
                ))
            }
        });

    // The pack inside the box goes either way: it has served its purpose on
    // success, and on failure it is 200 MB of the box's disk with nothing
    // pointing at it.
    let _ = remove_guest_pack(runtime, name, guest_pack).await;

    match verdict {
        Ok(archived) => Ok(archived),
        Err(error) => {
            let _ = std::fs::remove_file(host_path);
            Err(error.context(format!(
                "nothing was removed from box '{name}': the archive did not reach the host \
                 intact"
            )))
        }
    }
}

async fn remove_guest_pack(runtime: &dyn Runtime, name: &str, guest_pack: &str) -> Result<()> {
    runtime
        .run_as_root(name, &format!("rm -f '{guest_pack}'"), false)
        .await
        .map(|_| ())
}

/// The size and checksum of an archive, on whichever side reported them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub bytes: u64,
    pub sha256: String,
}

impl Archived {
    /// Read `bytes=` and `sha=` out of the packing command's output.
    ///
    /// `None` unless both arrived and the checksum looks like one. A missing
    /// value must not become a comparison that trivially succeeds — that would
    /// turn the verification into a formality and delete the directory on the
    /// strength of it.
    pub fn parse(stdout: &str) -> Option<Self> {
        let mut bytes = None;
        let mut sha256 = None;
        for line in stdout.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("bytes=") {
                bytes = value.trim().parse::<u64>().ok().filter(|n| *n > 0);
            } else if let Some(value) = line.strip_prefix("sha=") {
                let value = value.trim();
                if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    sha256 = Some(value.to_ascii_lowercase());
                }
            }
        }
        Some(Self {
            bytes: bytes?,
            sha256: sha256?,
        })
    }

    pub fn describe(&self) -> String {
        format!(
            "{}, sha256 {}",
            crate::cli::watch::human_bytes(self.bytes),
            &self.sha256[..12]
        )
    }
}

/// Hash and measure the file that arrived, without holding it in memory.
///
/// A home directory's archive is hundreds of megabytes; reading it into a
/// `Vec` to hash it would work and would also be the kind of thing that only
/// shows up on someone else's larger box.
fn measure_host_file(path: &Path) -> Result<Archived> {
    use sha2::{Digest, Sha256};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("read back {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .with_context(|| format!("read back {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(sha256, "{byte:02x}");
    }
    Ok(Archived { bytes, sha256 })
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

    /// The archive lives on the host, under `archives/`, and outlives the box.
    ///
    /// Not `boxes/<name>/`: that goes with `devbox destroy`, which is the
    /// command someone reaches for right after deciding the box is beyond
    /// saving — taking the rescued data with it.
    #[test]
    fn the_archive_goes_somewhere_destroy_does_not_reach() {
        let when = chrono::DateTime::parse_from_rfc3339("2026-09-06T17:55:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let path = archive_path(Path::new("/home/u/.devbox"), "devtest", when);
        assert_eq!(
            path,
            PathBuf::from("/home/u/.devbox/archives/devtest-stale-home-20260906T175530Z.tar.gz")
        );
        let inside_box = Path::new("/home/u/.devbox/boxes/devtest");
        assert!(!path.starts_with(inside_box), "{}", path.display());
        // Sorts lexically in the order they were made, whatever the locale.
        let later = archive_path(
            Path::new("/home/u/.devbox"),
            "devtest",
            when + chrono::Duration::seconds(1),
        );
        assert!(path < later);
    }

    /// Both halves must arrive, and the checksum must look like one. A missing
    /// value that defaulted would make the verification a formality — and the
    /// directory is deleted on the strength of it.
    #[test]
    fn a_half_reported_archive_is_not_a_verification() {
        let good = Archived::parse(&format!("bytes=193333663\nsha={}\n", "ab".repeat(32)))
            .expect("both halves");
        assert_eq!(good.bytes, 193_333_663);
        assert!(good.describe().contains("184.4MB"), "{}", good.describe());

        for stdout in [
            "",
            "bytes=193333663\n",
            &format!("sha={}\n", "ab".repeat(32)),
            // Zero bytes is not an archive of a directory with files in it.
            &format!("bytes=0\nsha={}\n", "ab".repeat(32)),
            // Not a digest.
            "bytes=10\nsha=nohasher\n",
            "bytes=10\nsha=\n",
        ] {
            assert_eq!(Archived::parse(stdout), None, "{stdout:?}");
        }
    }

    // ── the transfer, scripted ────────────────────────────

    use crate::runtime::ExecResult;
    use crate::runtime::stub::StubRuntime;

    const PAYLOAD: &[u8] = b"a tarball's worth of bytes";

    /// A box that packs `PAYLOAD` and reports its real size and digest.
    fn box_that_packs(sha: &str, bytes: u64) -> StubRuntime {
        let reply = format!("bytes={bytes}\nsha={sha}\n");
        StubRuntime::new()
            .with_exec_cmd(move |_, cmd, _| {
                let joined = cmd.join(" ");
                Ok(ExecResult {
                    exit_code: 0,
                    stdout: if joined.contains("tar czf") {
                        reply.clone()
                    } else {
                        String::new()
                    },
                    stderr: String::new(),
                })
            })
            .with_copy_from(|_, _, host_path| {
                std::fs::write(host_path, PAYLOAD)?;
                Ok(())
            })
    }

    fn payload_sha() -> String {
        crate::sandbox::provision::sha256_hex(PAYLOAD)
    }

    /// The good path: what the box packed is what arrived, so the caller is
    /// told the size and digest it can go on to delete a directory over.
    #[tokio::test]
    async fn a_verified_copy_reports_what_arrived_and_clears_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("archives/devtest.tar.gz");
        let guest = box_that_packs(&payload_sha(), PAYLOAD.len() as u64);

        let archived = archive_to_host(&guest, "devtest", "/home/u", "/tmp/pack", &host)
            .await
            .expect("the copy matches what was packed");

        assert_eq!(archived.bytes, PAYLOAD.len() as u64);
        assert_eq!(archived.sha256, payload_sha());
        assert!(host.exists());
        assert!(guest.called("copy_from"));
        // The pack inside the box has served its purpose.
        assert!(
            guest
                .exec_commands()
                .iter()
                .any(|c| c.contains("rm -f '/tmp/pack'")),
            "{:?}",
            guest.exec_commands()
        );
        // The directory itself is the caller's to remove, never this function's.
        assert!(
            !guest
                .exec_commands()
                .iter()
                .any(|c| c.contains("rm -rf '/home/u'")),
            "{:?}",
            guest.exec_commands()
        );
    }

    /// A copy that does not match is a copy that did not happen. The host's
    /// half-written file goes, and the error says the box was left alone —
    /// because the caller is about to decide whether to delete a directory.
    #[tokio::test]
    async fn a_mismatched_copy_leaves_nothing_behind_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("archives/devtest.tar.gz");
        // The box claims a digest that is not the payload's.
        let guest = box_that_packs(&"ab".repeat(32), PAYLOAD.len() as u64);

        let error = archive_to_host(&guest, "devtest", "/home/u", "/tmp/pack", &host)
            .await
            .expect_err("a mismatch is not an archive");

        let message = format!("{error:#}");
        assert!(message.contains("nothing was removed"), "{message}");
        assert!(!host.exists(), "the half-written archive is gone");
        assert!(
            guest
                .exec_commands()
                .iter()
                .any(|c| c.contains("rm -f '/tmp/pack'")),
            "{:?}",
            guest.exec_commands()
        );
    }

    /// And a transfer that never happened is the same answer, by the same
    /// route: nothing on the host, nothing removed from the box.
    #[tokio::test]
    async fn a_failed_transfer_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("archives/devtest.tar.gz");
        let reply = format!("bytes=26\nsha={}\n", payload_sha());
        let guest = StubRuntime::new()
            .with_exec_cmd(move |_, _, _| {
                Ok(ExecResult {
                    exit_code: 0,
                    stdout: reply.clone(),
                    stderr: String::new(),
                })
            })
            .with_copy_from(|_, _, _| bail!("no route to the box"));

        let error = archive_to_host(&guest, "devtest", "/home/u", "/tmp/pack", &host)
            .await
            .expect_err("a transfer that failed is not an archive");
        assert!(format!("{error:#}").contains("nothing was removed"));
        assert!(!host.exists());
    }

    /// A box that cannot say what it packed cannot have the copy verified, so
    /// it is not one — even though the transfer itself would have succeeded.
    #[tokio::test]
    async fn a_box_that_reports_no_checksum_is_refused_before_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("archives/devtest.tar.gz");
        let guest = StubRuntime::new().with_exec_cmd(|_, _, _| {
            Ok(ExecResult {
                exit_code: 0,
                stdout: "bytes=26\n".to_string(),
                stderr: String::new(),
            })
        });

        let error = archive_to_host(&guest, "devtest", "/home/u", "/tmp/pack", &host)
            .await
            .expect_err("no checksum, no verification");
        assert!(format!("{error:#}").contains("could not have been verified"));
        assert!(!guest.called("copy_from"), "it never got as far as copying");
        assert!(!host.exists());
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
