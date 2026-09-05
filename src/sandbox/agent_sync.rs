//! Keeping a box's agent identical to the one this host binary carries.
//!
//! Provisioning installs the embedded agent unconditionally, and nothing ever
//! looked at it again. That was survivable while a devbox release only ever
//! shipped one agent build, and stopped being survivable the moment the same
//! version number could mean two different binaries: a box provisioned by a
//! portable build keeps its portable agent forever, and the handshake cannot
//! tell — [`crate::obs::collector`] compares version strings, and both say
//! `0.1.6`.
//!
//! So the judgement here is the content digest. It is the only thing that
//! separates "the agent this host would install" from "the agent this box
//! happens to have", and it is computed on the guest every time it is asked
//! for. A cached answer would be a claim about a file this host does not own.
//!
//! Two things can be out of date, and both are checked in one probe:
//!
//! 1. **The binary.** Replaced through [`super::provision::install_embedded_binary`],
//!    which is the only supported way bytes reach `/usr/local/bin` — staged,
//!    frozen by root, digest-verified, then installed.
//! 2. **The service's arguments.** `-no-ebpf` is decided by the *host* — it
//!    depends on what this binary embedded — but provisioning froze that
//!    decision into a systemd unit. The exec-transport agent has always had
//!    its arguments regenerated per spawn ([`crate::obs::supervisor`]); this
//!    closes the asymmetry for the service.

use std::sync::OnceLock;

use anyhow::{Context, Result};

use crate::runtime::Runtime;

/// Where every runtime's agent lives. The install path, the unit's
/// `ExecStart`, and the exec transport's argv all name this.
pub const AGENT_PATH: &str = "/usr/local/bin/devbox-obsd";

/// The systemd unit that supervises the agent between console sessions.
pub const AGENT_UNIT: &str = "devbox-obsd";

/// The digest of the agent this host binary carries.
///
/// Computed once. The bytes are a compile-time constant of several megabytes,
/// and the collector daemon asks this question on every attach and every
/// retry — the guest's answer has to be recomputed each time, this one never
/// does.
pub fn host_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| super::provision::sha256_hex(crate::embedded::OBSD))
}

/// What the guest said about the agent it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    /// 64 lowercase hex characters, read back from the guest.
    Hex(String),
    /// Nothing executable at [`AGENT_PATH`].
    Missing,
    /// The binary is there but the guest has no tool that can hash it.
    ///
    /// Not a panic and not a shrug: it is treated as stale, because the one
    /// thing that could have proved otherwise is unavailable. A guest in this
    /// state cannot have been provisioned through
    /// [`super::provision::install_embedded_binary`] either — that verifies
    /// its own staged copy with the guest's `sha256sum` — so a re-push will
    /// fail loudly with the real reason rather than silently doing nothing.
    Unhashable,
}

/// One probe of a box: what agent it has, and how its service is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestAgent {
    pub digest: Digest,
    /// The unit's effective `ExecStart`, or `None` when the box has no such
    /// unit — a container without systemd, or a box provisioned before the
    /// service existed.
    pub exec_start: Option<String>,
}

/// Whether the box's agent is the one this host would install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Current,
    /// Stale, and why. The reason is what a human needs; the caller's action
    /// is the same for all three.
    Stale(Stale),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// No agent at all — a box provisioned before the agent existed, or one
    /// whose install failed with a warning nobody read.
    Absent,
    /// A different agent. `installed` is the guest's digest.
    Different { installed: String },
    /// The guest could not hash its own binary, so this cannot be ruled out.
    Unverifiable,
}

impl Verdict {
    pub fn is_stale(&self) -> bool {
        matches!(self, Verdict::Stale(_))
    }
}

/// Compare the host's embedded agent against what the guest reported.
///
/// Pure, so the judgement is testable without a box.
pub fn verdict(host: &str, guest: &Digest) -> Verdict {
    match guest {
        Digest::Hex(installed) if installed == host => Verdict::Current,
        Digest::Hex(installed) => Verdict::Stale(Stale::Different {
            installed: installed.clone(),
        }),
        Digest::Missing => Verdict::Stale(Stale::Absent),
        Digest::Unhashable => Verdict::Stale(Stale::Unverifiable),
    }
}

/// Whether the service is already running with the arguments this host would
/// generate now.
///
/// Only the eBPF decision is host-dependent, and it is expressed by the
/// presence of one flag. A box with no unit has nothing to be wrong: the
/// answer is "current" so that a container without systemd is not reported as
/// permanently out of date.
pub fn unit_is_current(exec_start: Option<&str>, host_uses_ebpf: bool) -> bool {
    let Some(exec_start) = exec_start else {
        return true;
    };
    let unit_disables_ebpf = exec_start
        .split_whitespace()
        .any(|argument| argument == "-no-ebpf");
    unit_disables_ebpf != host_uses_ebpf
}

/// The line `devbox doctor` prints for one box.
///
/// Deliberately next to `capture:`, and deliberately short: the two together
/// answer "is this box's observability what this devbox thinks it is".
pub fn doctor_line(host: &str, guest: &Digest) -> String {
    match verdict(host, guest) {
        Verdict::Current => "matches host embed".to_string(),
        Verdict::Stale(Stale::Absent) => "missing — no agent is installed".to_string(),
        Verdict::Stale(Stale::Unverifiable) => {
            "unverifiable — the guest has no sha256 tool".to_string()
        }
        Verdict::Stale(Stale::Different { installed }) => {
            format!("stale (sha {} vs {})", short(&installed), short(host))
        }
    }
}

fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// The guest-side probe.
///
/// One round trip for both questions, because a probe that runs on every
/// collector attach should not cost two.
///
/// `sha256sum` is the coreutils and busybox spelling and is what
/// `install_embedded_binary` already relies on; `shasum` and `openssl` are
/// there for images that carry neither. All three print the digest as the
/// first space-separated field — busybox prints `<hex>  <path>`, openssl
/// `-r` prints `<hex> *<path>` — so one `cut` reads all of them, and the
/// result is validated as hex on the host rather than trusted.
pub const PROBE: &str = r#"
if [ -x /usr/local/bin/devbox-obsd ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    sha=$(sha256sum /usr/local/bin/devbox-obsd 2>/dev/null | cut -d' ' -f1)
  elif command -v shasum >/dev/null 2>&1; then
    sha=$(shasum -a 256 /usr/local/bin/devbox-obsd 2>/dev/null | cut -d' ' -f1)
  elif command -v openssl >/dev/null 2>&1; then
    sha=$(openssl dgst -sha256 -r /usr/local/bin/devbox-obsd 2>/dev/null | cut -d' ' -f1)
  else
    sha=nohasher
  fi
  [ -n "$sha" ] || sha=nohasher
else
  sha=absent
fi
if command -v systemctl >/dev/null 2>&1; then
  unit=$(systemctl show -p ExecStart --value devbox-obsd 2>/dev/null | tr '\n' ' ')
else
  unit=
fi
printf 'sha=%s\n' "$sha"
printf 'unit=%s\n' "$unit"
"#;

/// Read [`PROBE`] output.
///
/// Anything unexpected reads as `Unhashable` rather than as a match: the
/// failure mode of a misparse must be a redundant re-push, never a box left
/// running an agent nobody checked.
pub fn parse_probe(stdout: &str) -> GuestAgent {
    let mut digest = Digest::Unhashable;
    let mut exec_start = None;
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "sha" => {
                digest = match value {
                    "absent" => Digest::Missing,
                    hex if is_sha256_hex(hex) => Digest::Hex(hex.to_string()),
                    _ => Digest::Unhashable,
                }
            }
            // An empty value means no unit — `systemctl show` on an unknown
            // unit prints nothing for `ExecStart`, and a box without
            // systemd never ran it at all.
            "unit" if !value.is_empty() => exec_start = Some(value.to_string()),
            _ => {}
        }
    }
    GuestAgent { digest, exec_start }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Ask a box what agent it has.
pub async fn probe(runtime: &dyn Runtime, name: &str) -> Result<GuestAgent> {
    let result = runtime
        .exec_cmd(name, &["sh", "-c", PROBE], false)
        .await
        .with_context(|| format!("probe the observability agent in box '{name}'"))?;
    // A non-zero exit still carries whatever the script managed to print, and
    // the parser's default for missing lines is already "assume stale".
    Ok(parse_probe(&result.stdout))
}

/// The box was rebuilt and its egress posture did not come back.
///
/// `nixos-rebuild switch` removes devbox's nftables table, so regenerating the
/// agent's unit reopens egress until the saved posture is reinstalled. When
/// that reinstall fails the box is running unrestricted while `devbox.toml`,
/// the CLI and the console all still say `isolated` — the one failure in this
/// module that a caller must not treat as a warning.
#[derive(Debug)]
pub struct PostureLost;

impl std::fmt::Display for PostureLost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the box's egress posture was not restored after its rebuild")
    }
}

impl std::error::Error for PostureLost {}

/// Whether this failure left a box running without its firewall.
///
/// Through `anyhow`'s chain, so a `with_context` between the cause and the
/// caller cannot turn a fail-closed condition into a warning.
pub fn lost_the_posture(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PostureLost>().is_some()
}

/// How much of the box a refresh may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Binary, unit, and — on NixOS — the rebuild that regenerates the unit.
    ///
    /// For callers holding the per-box lifecycle claim.
    Full,
    /// The binary and a service restart only.
    ///
    /// For the collector daemon. Regenerating a NixOS unit means
    /// `nixos-rebuild switch`, which takes minutes and belongs to a claimed
    /// lifecycle operation, not to a reconciliation tick that a user's own
    /// rebuild could be racing.
    BinaryOnly,
}

/// What a refresh did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refresh {
    pub pushed: bool,
    pub unit_rewritten: bool,
    pub restarted: bool,
    /// What was found but deliberately not acted on.
    pub notes: Vec<String>,
}

impl Refresh {
    pub fn changed(&self) -> bool {
        self.pushed || self.unit_rewritten
    }
}

/// Bring a box's agent up to the one this host binary carries.
///
/// Does nothing — and costs one guest exec — when the box is already current,
/// which is every call after the first.
pub async fn ensure_current(
    manager: &super::SandboxManager,
    runtime: &dyn Runtime,
    name: &str,
    image: &str,
    scope: Scope,
    claim: &crate::web::build::BoxClaim,
) -> Result<Refresh> {
    let host = host_digest();
    let guest = probe(runtime, name).await?;
    let verdict = verdict(host, &guest.digest);
    let host_uses_ebpf = crate::obs::uses_ebpf(runtime.name());
    let unit_current = unit_is_current(guest.exec_start.as_deref(), host_uses_ebpf);

    let mut refresh = Refresh::default();
    if !verdict.is_stale() && unit_current {
        return Ok(refresh);
    }

    if let Verdict::Stale(reason) = &verdict {
        match reason {
            Stale::Absent => println!("Box '{name}' has no observability agent; installing it."),
            Stale::Different { installed } => println!(
                "Box '{name}' has an out-of-date observability agent ({} vs {}); replacing it.",
                short(installed),
                short(host)
            ),
            Stale::Unverifiable => eprintln!(
                "Warning: box '{name}' cannot hash its own observability agent, so it is \
                 treated as out of date and reinstalled. Install coreutils, busybox, or \
                 openssl in the box to make this checkable."
            ),
        }
        super::provision::install_embedded_binary(
            runtime,
            name,
            "devbox-obsd",
            crate::embedded::OBSD,
        )
        .await
        .with_context(|| format!("replace the observability agent in box '{name}'"))?;
        refresh.pushed = true;
    }

    if !unit_current {
        match scope {
            Scope::Full => {
                rewrite_unit(manager, runtime, name, image, claim).await?;
                refresh.unit_rewritten = true;
            }
            Scope::BinaryOnly => refresh.notes.push(format!(
                "the {AGENT_UNIT} unit still passes {}-no-ebpf; it is regenerated by the next \
                 devbox command that starts or enters this box",
                if host_uses_ebpf { "" } else { "no " }
            )),
        }
    }

    // A running agent keeps the bytes it started with. `install` replaced the
    // file, so nothing changes until the processes holding the old inode go.
    refresh.restarted = restart_agents(runtime, name, guest.exec_start.is_some()).await;
    Ok(refresh)
}

/// Regenerate the service definition from this host's decisions.
///
/// The two images take different routes to the same place. Ubuntu's unit is a
/// file devbox writes, so rewriting it and reloading systemd is the whole job.
/// A NixOS unit is *built*: the flag lives in `configuration.nix`, and the
/// capability set the agent needs for eBPF is derived from it inside
/// `obsd-module.nix` — which is why this cannot be a drop-in that only edits
/// `ExecStart`. An agent given `-no-ebpf` removed but not `CAP_BPF` fails its
/// attach and degrades, silently, to exactly the state this was meant to fix.
async fn rewrite_unit(
    manager: &super::SandboxManager,
    runtime: &dyn Runtime,
    name: &str,
    image: &str,
    claim: &crate::web::build::BoxClaim,
) -> Result<()> {
    if image == "nixos" {
        println!("Regenerating the {AGENT_UNIT} service for box '{name}'...");
        super::provision::write_obsd_module(runtime, name).await?;
        super::provision::ensure_nixos_config(runtime, name, true).await?;
        crate::nix::rebuild::nixos_rebuild(runtime, name)
            .await
            .with_context(|| format!("regenerate the {AGENT_UNIT} service in box '{name}'"))?;
        // The rebuild took devbox's nftables table with it. Every other
        // rebuild path in this codebase restores here, and each one was added
        // after the box had already come back with open egress once.
        crate::policy::enforce::restore_after_rebuild(manager, name, claim)
            .await
            .map_err(|error| {
                error
                    .context(PostureLost)
                    .context(format!("box '{name}' was rebuilt to update its agent"))
            })?;
    } else {
        println!("Rewriting the {AGENT_UNIT} service for box '{name}'...");
        super::provision::install_ubuntu_obsd_service(runtime, name).await?;
        let _ = runtime
            .run_as_root(name, "systemctl daemon-reload", false)
            .await;
    }
    Ok(())
}

/// End every agent process still running the old bytes.
///
/// Two of them, and they are ended differently. The service is systemd's, so
/// systemd restarts it. The exec-transport agent belongs to the collector,
/// which is in another process — possibly on another devbox build — so the
/// only thing this side can do is end it and let the collector's existing
/// retry bring back a fresh one. `-stdio` is what tells the two apart: the
/// service never has it.
///
/// Best effort throughout. A box that will not restart its agent is a box
/// whose capture is degraded for a while, not a failed command.
async fn restart_agents(runtime: &dyn Runtime, name: &str, has_unit: bool) -> bool {
    let mut restarted = false;
    if has_unit {
        match runtime
            .run_as_root(name, &format!("systemctl restart {AGENT_UNIT}"), false)
            .await
        {
            Ok(result) if result.exit_code == 0 => restarted = true,
            Ok(result) => tracing::warn!(
                box_id = %name,
                stderr = %result.stderr.trim(),
                "could not restart the observability agent service"
            ),
            Err(error) => {
                tracing::warn!(box_id = %name, %error, "could not restart the observability agent service")
            }
        }
    }
    // The name is validated before a box is ever created, so it contributes no
    // pattern metacharacters here.
    if crate::sandbox::state::is_safe_name(name) {
        let pattern = format!("devbox-obsd -box-id {name} -stdio");
        let _ = runtime
            .run_as_root(
                name,
                &format!("pkill -f -- '{pattern}' 2>/dev/null; true"),
                false,
            )
            .await;
    }
    restarted
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "aa11bb22cc33dd44ee55ff6607182930aabbccddeeff00112233445566778899";
    const OTHER: &str = "0011223344556677889900aabbccddeeff112233445566778899aabbccddeeff";

    #[test]
    fn the_same_bytes_are_current() {
        assert_eq!(
            verdict(HOST, &Digest::Hex(HOST.to_string())),
            Verdict::Current
        );
    }

    #[test]
    fn different_bytes_are_stale_and_name_both_digests() {
        assert_eq!(
            verdict(HOST, &Digest::Hex(OTHER.to_string())),
            Verdict::Stale(Stale::Different {
                installed: OTHER.to_string()
            })
        );
        assert_eq!(
            doctor_line(HOST, &Digest::Hex(OTHER.to_string())),
            "stale (sha 001122334455 vs aa11bb22cc33)"
        );
    }

    #[test]
    fn a_box_with_no_agent_is_stale_not_an_error() {
        assert_eq!(
            verdict(HOST, &Digest::Missing),
            Verdict::Stale(Stale::Absent)
        );
        assert!(doctor_line(HOST, &Digest::Missing).contains("missing"));
    }

    /// The case that must not become a panic or a silent pass: an agent that
    /// exists, and a guest with nothing to hash it with. Unknowable is not the
    /// same as fine.
    #[test]
    fn a_guest_that_cannot_hash_is_stale_and_says_so() {
        assert_eq!(
            verdict(HOST, &Digest::Unhashable),
            Verdict::Stale(Stale::Unverifiable)
        );
        let line = doctor_line(HOST, &Digest::Unhashable);
        assert!(line.contains("unverifiable"), "{line}");
        assert!(line.contains("sha256"), "{line}");
    }

    #[test]
    fn the_version_string_is_not_the_judgement() {
        // Two agents of the same devbox version, different builds. The
        // handshake accepts both; this must not.
        let portable = "1".repeat(64);
        let ebpf = "2".repeat(64);
        assert!(verdict(&ebpf, &Digest::Hex(portable)).is_stale());
    }

    #[test]
    fn coreutils_and_busybox_output_both_parse() {
        // Both print `<hex>  <path>`; the probe already cut the first field.
        let guest = parse_probe(&format!("sha={HOST}\nunit=\n"));
        assert_eq!(guest.digest, Digest::Hex(HOST.to_string()));
        assert_eq!(guest.exec_start, None);
    }

    #[test]
    fn an_unparseable_digest_is_treated_as_unhashable() {
        for value in ["nohasher", "", "not-hex", &"A".repeat(64), &"ab".repeat(40)] {
            let guest = parse_probe(&format!("sha={value}\nunit=\n"));
            assert_eq!(guest.digest, Digest::Unhashable, "value {value:?}");
        }
    }

    #[test]
    fn an_absent_binary_is_reported_as_missing() {
        assert_eq!(parse_probe("sha=absent\nunit=\n").digest, Digest::Missing);
    }

    #[test]
    fn a_probe_that_printed_nothing_is_not_a_match() {
        assert_eq!(parse_probe("").digest, Digest::Unhashable);
        assert_eq!(parse_probe("").exec_start, None);
    }

    #[test]
    fn the_exec_start_line_survives_its_own_equals_signs() {
        let show = "{ path=/usr/local/bin/devbox-obsd ; argv[]=/usr/local/bin/devbox-obsd \
                    -box-id w05c -no-transport -packet=true -no-ebpf ; ignore_errors=no }";
        let guest = parse_probe(&format!("sha=absent\nunit={show}\n"));
        assert_eq!(guest.exec_start.as_deref(), Some(show));
    }

    #[test]
    fn a_unit_carrying_no_ebpf_is_current_only_for_a_host_without_it() {
        let with = Some("… -packet=true -no-ebpf ; …");
        let without = Some("… -packet=true ; …");
        assert!(unit_is_current(with, false));
        assert!(!unit_is_current(with, true));
        assert!(unit_is_current(without, true));
        assert!(!unit_is_current(without, false));
    }

    /// `-no-ebpf` must be matched as an argument, not as a substring: a unit
    /// that mentions it inside a path or a longer flag has not disabled it.
    #[test]
    fn no_ebpf_is_matched_as_a_whole_argument() {
        assert!(unit_is_current(
            Some("… -status-file /run/no-ebpf.json ; …"),
            true
        ));
        assert!(unit_is_current(Some("… -no-ebpf=false ; …"), true));
    }

    #[test]
    fn a_box_without_the_unit_is_never_reported_out_of_date() {
        assert!(unit_is_current(None, true));
        assert!(unit_is_current(None, false));
    }

    /// The probe cannot interpolate a constant, so it repeats the path and the
    /// unit name as literals. This is what stops the two copies drifting.
    /// The fail-closed condition has to survive the context the caller adds
    /// on the way out, or a box left without a firewall is reported as a
    /// warning and entered anyway.
    #[test]
    fn a_lost_posture_is_still_recognisable_under_added_context() {
        let error = anyhow::Error::new(std::io::Error::other("nft: no such table"))
            .context(PostureLost)
            .context("box 'w05c' was rebuilt to update its agent");
        assert!(lost_the_posture(&error));
        assert!(!lost_the_posture(&anyhow::anyhow!("the copy failed")));
    }

    #[test]
    fn the_probe_names_the_same_agent_the_rest_of_devbox_installs() {
        assert!(PROBE.contains(AGENT_PATH), "{PROBE}");
        assert!(PROBE.contains(AGENT_UNIT), "{PROBE}");
    }

    #[test]
    fn the_host_digest_is_the_digest_of_the_embedded_agent() {
        assert_eq!(
            host_digest(),
            super::super::provision::sha256_hex(crate::embedded::OBSD)
        );
        // And it is the same string every time, which is the point of caching
        // it: the collector daemon asks on every attach.
        assert!(std::ptr::eq(host_digest(), host_digest()));
    }
}
