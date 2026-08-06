//! Applying a policy to a live box — the other half of §8.
//!
//! [`super::nftables::ruleset`] decides *what* the rules are; this pushes them
//! into the guest and loads them. Keeping the two apart is what lets the hard
//! part — the rules — be unit-tested on a Mac with no kernel involved, while
//! this module stays thin enough to read in one sitting.
//!
//! Until this existed, `devbox policy set isolated` wrote the posture to
//! `devbox.toml`, printed "apply it with `devbox reprovision`", and nothing in
//! any provisioning path ever generated or loaded a ruleset. A box could report
//! `isolated` and still reach the whole internet. A posture that is displayed
//! but not enforced is worse than no posture at all, because it is believed.

use anyhow::{Result, bail};

use super::{Policy, Posture};
use crate::runtime::Runtime;

/// Where the generated ruleset lands inside the box.
pub const RULESET_PATH: &str = "/etc/devbox/devbox.nft";

/// Push the policy into the box and load it.
///
/// Idempotent, because the generated ruleset destroys the devbox table before
/// recreating it: applying twice leaves what applying once leaves.
pub async fn apply(runtime: &dyn Runtime, sandbox_name: &str, policy: &Policy) -> Result<()> {
    // `open` is the absence of a policy, not a policy of its own. Loading a
    // ruleset for it would leave an empty devbox table sitting in the box
    // suggesting something is being enforced.
    if policy.egress == Posture::Open {
        return clear(runtime, sandbox_name).await;
    }

    let ruleset = super::nftables::ruleset(policy);

    let write = runtime
        .exec_cmd(
            sandbox_name,
            &["sudo", "bash", "-c", &write_command(&ruleset)],
            false,
        )
        .await?;
    if write.exit_code != 0 {
        bail!(
            "failed to write the ruleset into box '{sandbox_name}': {}",
            write.stderr.trim()
        );
    }

    let load = runtime
        .exec_cmd(sandbox_name, &["sudo", "nft", "-f", RULESET_PATH], false)
        .await?;
    if load.exit_code != 0 {
        // The most common cause by far, worth naming rather than making the
        // user decode an nft error: the box has no nftables at all.
        bail!(
            "failed to load the egress ruleset in box '{sandbox_name}': {}\n\n  \
             If nftables is missing, enable the `network` set \
             (`devbox sets enable network`) and rebuild.",
            load.stderr.trim()
        );
    }

    Ok(())
}

/// Remove devbox's table, returning the box to whatever egress it had before.
///
/// `destroy` rather than `delete` so this succeeds on a box that never had a
/// policy applied — the case that runs every time someone sets `open` on a
/// fresh box.
pub async fn clear(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &["sudo", "bash", "-c", &clear_command()],
            false,
        )
        .await?;
    if result.exit_code != 0 {
        bail!(
            "failed to clear the egress ruleset in box '{sandbox_name}': {}",
            result.stderr.trim()
        );
    }
    Ok(())
}

/// The shell that writes the ruleset into the guest.
///
/// Split out so the command is testable without a runtime: what goes wrong
/// here is quoting, and quoting is exactly what a unit test can pin.
fn write_command(ruleset: &str) -> String {
    format!(
        "mkdir -p /etc/devbox && cat > {RULESET_PATH} << 'DEVBOX_NFT_EOF'\n{ruleset}\nDEVBOX_NFT_EOF"
    )
}

/// The shell that removes devbox's table and its generated file.
fn clear_command() -> String {
    format!(
        "nft destroy table inet {} 2>/dev/null || true; rm -f {RULESET_PATH}",
        super::nftables::TABLE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ruleset_is_written_then_loaded_from_a_stable_path() {
        let policy = Policy {
            egress: Posture::Isolated,
            ..Default::default()
        };
        let cmd = write_command(&super::super::nftables::ruleset(&policy));

        // A quoted heredoc delimiter: the ruleset contains `$` and `{`, and an
        // unquoted heredoc would have the guest shell expand them.
        assert!(cmd.contains("<< 'DEVBOX_NFT_EOF'"));
        assert!(cmd.contains(RULESET_PATH));
        assert!(cmd.contains("drop"), "isolated must actually drop traffic");
        assert!(!cmd.contains("DEVBOX_NFT_EOF\nDEVBOX_NFT_EOF"));
    }

    #[test]
    fn clearing_succeeds_on_a_box_that_never_had_a_policy() {
        let cmd = clear_command();
        // `destroy` and the `|| true` are both load-bearing: `delete` on a
        // missing table is an error, and this runs on every `policy set open`.
        assert!(cmd.contains("destroy table inet devbox"));
        assert!(cmd.contains("|| true"));
        assert!(cmd.contains(RULESET_PATH));
    }
}
