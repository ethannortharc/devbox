//! Component B — the credential broker (§6 of the v5 design).
//!
//! The promise is one sentence: **no API key, OAuth token, or git credential
//! is written into the guest or its environment.** What the guest gets is a
//! per-box token and a URL; the host-side broker holds the real credential,
//! injects it into one outbound request at a time, enforces a scope, and
//! records every use as a `credential` event in that box's own store.
//!
//! The pieces:
//!
//! - [`secrets`] — where the credential lives (Keychain / Secret Service / 0600 file).
//! - [`tokens`] — the per-box token, minted at box start.
//! - [`providers`] — provider → upstream + injection header, and what to strip.
//! - [`scope`] — methods, path prefixes, and the GitHub repository allowlist.
//! - [`reach`] — the verified address the guest uses to reach the host.
//! - [`server`] — the streaming reverse proxy.
//! - [`daemon`] — the supervised, one-per-user `devbox __broker` process.

pub mod daemon;
pub mod providers;
pub mod reach;
pub mod scope;
pub mod secrets;
pub mod server;
pub mod tokens;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime::Runtime;

/// The port the broker prefers.
///
/// Adjacent to the console's 7878 so the two read as one family in `lsof`. If
/// it is taken the broker binds an ephemeral port instead and publishes it;
/// nothing hardcodes this number except the first `bind` attempt.
pub const DEFAULT_PORT: u16 = 7879;

/// Set to disable the broker entirely, the way `DEVBOX_NO_COLLECTOR_DAEMON`
/// disables capture. Used by tests that must not touch a real listener.
pub const DISABLE_ENV: &str = "DEVBOX_NO_BROKER";

/// `~/.devbox/broker`.
pub fn broker_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("broker")
}

/// The published endpoint record, so any CLI process can find the listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub pid: i32,
    pub version: String,
}

pub fn endpoint_path(state_dir: &Path) -> PathBuf {
    broker_dir(state_dir).join("endpoint.json")
}

/// Read the endpoint the running broker published, if any.
pub fn endpoint(state_dir: &Path) -> Option<Endpoint> {
    let text = std::fs::read_to_string(endpoint_path(state_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Publish the endpoint atomically, the way the collector publishes its stats.
pub fn publish_endpoint(state_dir: &Path, endpoint: &Endpoint) -> Result<()> {
    let dir = broker_dir(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect {}", dir.display()))?;
    let path = endpoint_path(state_dir);
    let temporary = dir.join("endpoint.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(endpoint)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

use std::os::unix::fs::PermissionsExt as _;

/// Where a provider's scope is persisted.
pub fn scope_path(state_dir: &Path, provider: &str) -> Option<PathBuf> {
    secrets::check_name(provider).ok()?;
    Some(
        broker_dir(state_dir)
            .join("scopes")
            .join(format!("{provider}.json")),
    )
}

/// Where an `http:<name>` provider's upstream and header spec is persisted.
pub fn http_provider_path(state_dir: &Path, name: &str) -> Option<PathBuf> {
    secrets::check_name(name).ok()?;
    Some(
        broker_dir(state_dir)
            .join("http")
            .join(format!("{name}.json")),
    )
}

/// Persist a JSON document under the broker directory at 0600.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

/// The index of provider names that have a secret.
///
/// One empty 0600 marker file per provider. It exists because
/// [`configured_providers`] runs on the path of *every* `exec` and `shell`,
/// and answering it from the macOS Keychain means three `security(1)`
/// subprocesses per command — three chances for a keychain-access prompt, on
/// a command that has not yet decided it needs a credential at all. The
/// keychain is read only when a request actually needs the value.
///
/// The index can drift from the store if someone deletes a keychain item by
/// hand. That degrades to a 403 from the broker naming the provider and the
/// command to fix it, which is a better failure than a prompt on `devbox ls`.
pub fn provider_index_dir(state_dir: &Path) -> PathBuf {
    broker_dir(state_dir).join("providers")
}

/// Record that a provider has a secret.
pub fn index_add(state_dir: &Path, provider: &str) -> Result<()> {
    secrets::check_name(provider)?;
    let dir = provider_index_dir(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect {}", dir.display()))?;
    let path = dir.join(provider);
    std::fs::write(&path, b"").with_context(|| format!("write {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect {}", path.display()))?;
    Ok(())
}

/// Forget that a provider has a secret.
pub fn index_remove(state_dir: &Path, provider: &str) {
    if secrets::check_name(provider).is_ok() {
        let _ = std::fs::remove_file(provider_index_dir(state_dir).join(provider));
    }
}

/// Every provider name in the index, in a stable order: the built-ins first,
/// in the order §6.3 lists them, then the generic ones alphabetically.
pub fn indexed_providers(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(provider_index_dir(state_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| !name.starts_with('.') && secrets::check_name(name).is_ok())
        .collect();
    let rank = |name: &str| match name {
        providers::ANTHROPIC => 0,
        providers::OPENAI => 1,
        providers::GITHUB => 2,
        _ => 3,
    };
    names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    names
}

/// Every generic `http:<name>` provider configured on this host.
pub fn http_providers(state_dir: &Path) -> Vec<(String, providers::HttpProvider)> {
    let dir = broker_dir(state_dir).join("http");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, providers::HttpProvider)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let name = name.strip_suffix(".json")?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let text = std::fs::read_to_string(entry.path()).ok()?;
            Some((name, serde_json::from_str(&text).ok()?))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The environment variables devbox injects into a session it starts — §6.3.
///
/// This is the integration point track A's `run` wrapper calls: it returns the
/// pairs, and the caller decides how to get them into the guest process. It is
/// deliberately *not* a file written into the box, and deliberately contains
/// no secret: `DEVBOX_BROKER_TOKEN` names the box to the broker and nothing
/// more, and the per-provider variables point the tools at the broker rather
/// than at the upstream.
///
/// A provider with no stored secret gets no variables at all. Setting
/// `ANTHROPIC_BASE_URL` when there is nothing behind it would break a box that
/// was working before the broker existed, and the failure would look like a
/// network problem rather than a missing secret.
pub fn session_env(
    base_url: &str,
    token: &str,
    providers_present: &[String],
) -> Vec<(String, String)> {
    let base = base_url.trim_end_matches('/');
    let mut env = vec![
        ("DEVBOX_BROKER_URL".to_string(), base.to_string()),
        ("DEVBOX_BROKER_TOKEN".to_string(), token.to_string()),
    ];
    for provider in providers_present {
        match provider.as_str() {
            providers::ANTHROPIC => {
                env.push(("ANTHROPIC_BASE_URL".into(), format!("{base}/anthropic")));
                // Measured: this becomes `Authorization: Bearer <token>`,
                // which is exactly the header the broker reads the box token
                // from. `ANTHROPIC_API_KEY` is deliberately not set — it would
                // take precedence in some clients and put the same value in
                // `x-api-key` instead, for no gain.
                env.push(("ANTHROPIC_AUTH_TOKEN".into(), token.to_string()));
            }
            providers::OPENAI => {
                env.push(("OPENAI_BASE_URL".into(), format!("{base}/openai/v1")));
                // OpenAI clients refuse to start without a key set; the box
                // token is what they get, and the broker replaces it.
                env.push(("OPENAI_API_KEY".into(), token.to_string()));
            }
            providers::GITHUB => {
                // git is wired through the guest gitconfig's `insteadOf`
                // (written by provisioning), not through an environment
                // variable. `gh` cannot be pointed at a plain-HTTP broker at
                // all — it forces https on `GH_HOST` — so no `GH_HOST` is set;
                // setting one would break `gh` rather than broker it.
                env.push(("DEVBOX_BROKER_GITHUB_URL".into(), format!("{base}/github")));
            }
            name => {
                env.push((
                    format!("DEVBOX_SECRET_{}_URL", env_suffix(name)),
                    format!("{base}/{name}"),
                ));
            }
        }
    }
    env
}

/// Turn a provider name into the part of an environment variable name.
fn env_suffix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The providers that have a secret on this host.
///
/// Read from the on-disk index, not from the secret store: see
/// [`provider_index_dir`] for why a keychain lookup does not belong on the
/// path of every command.
pub fn configured_providers(state_dir: &Path) -> Vec<String> {
    indexed_providers(state_dir)
}

/// Everything a session needs, resolved against a live box.
///
/// Best effort by design: this runs on the path of every `exec` and `shell`,
/// and a broker that is not running, a box that cannot be probed, or a host
/// with no secrets at all must all mean "no broker variables", not "the
/// command fails".
pub async fn guest_env(
    state_dir: &Path,
    runtime: &dyn Runtime,
    box_id: &str,
) -> Vec<(String, String)> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return Vec::new();
    }
    let configured = configured_providers(state_dir);
    if configured.is_empty() {
        return Vec::new();
    }
    let Some(endpoint) = endpoint(state_dir) else {
        return Vec::new();
    };
    let Some(token) = tokens::current(state_dir, box_id) else {
        return Vec::new();
    };
    let reach = match tokio::time::timeout(
        reach::REACH_TIMEOUT,
        runtime.host_reach(box_id, endpoint.port),
    )
    .await
    {
        Ok(Ok(reach)) => reach,
        Ok(Err(error)) => {
            tracing::debug!(box_id, %error, "no host-reachable broker address for this box");
            return Vec::new();
        }
        Err(_) => {
            tracing::debug!(box_id, "host reachability probe timed out");
            return Vec::new();
        }
    };
    session_env(&reach.base_url(), &token, &configured)
}

/// The broker's address and port as seen from inside a box, for the egress
/// policy's exemption (§6.7).
///
/// Returns `None` — meaning "no exemption" — whenever there is nothing to
/// broker, no broker running, or no verified route from the box to the host.
/// That direction matters: a wrong guess here would punch a hole in a
/// default-deny firewall for an address that is not the broker.
///
/// The address is resolved *inside the box*, because `host_reach` may return
/// a name (`host.lima.internal`) and nftables takes addresses.
pub async fn policy_exemption(runtime: &dyn Runtime, name: &str) -> Option<(String, u16)> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return None;
    }
    let manager = crate::sandbox::SandboxManager::new().ok()?;
    if configured_providers(&manager.state_dir).is_empty() {
        return None;
    }
    let endpoint = endpoint(&manager.state_dir)?;
    let reach = tokio::time::timeout(
        reach::REACH_TIMEOUT,
        runtime.host_reach(name, endpoint.port),
    )
    .await
    .ok()?
    .ok()?;

    let address = if reach::is_ipv4_literal(&reach.host) {
        reach.host.clone()
    } else {
        let result = runtime
            .exec_cmd(
                name,
                &[
                    "sh",
                    "-c",
                    &format!(
                        "getent ahostsv4 {} 2>/dev/null | head -1",
                        shell_word(&reach.host)?
                    ),
                ],
                false,
            )
            .await
            .ok()?;
        let first = result.stdout.split_whitespace().next()?.to_string();
        if !reach::is_ipv4_literal(&first) {
            return None;
        }
        first
    };
    Some((address, endpoint.port))
}

/// Accept only a host that is safe to interpolate into a shell word.
fn shell_word(value: &str) -> Option<&str> {
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        .then_some(value)
}

/// The `AcceptEnv` patterns a box's sshd is configured with — §6.3, via ssh.
///
/// `devbox code` hands VS Code or Cursor a Remote SSH target and then gets out
/// of the way, so there is no guest argv for [`with_env`] to wrap. The other
/// two places a variable could come from are both worse: writing the token
/// into a guest shell profile is exactly what v5 removed from `provision.rs`,
/// and `sshd_config`'s own `SetEnv` would put it on the box's disk. What is
/// left is ssh's environment passing — the *host's* `Host` block sends the
/// pairs with `SetEnv`, and sshd lets them through because of this list. The
/// token never lands in the box.
///
/// Patterns rather than plain names in two places, because
/// [`session_env`] can produce a variable per generic provider and their names
/// are not knowable here. sshd matches `AcceptEnv` by glob.
pub const SSH_ACCEPT_ENV: &[&str] = &[
    "DEVBOX_BROKER_URL",
    "DEVBOX_BROKER_TOKEN",
    "DEVBOX_BROKER_GITHUB_URL",
    "DEVBOX_RUN_ID",
    "DEVBOX_SECRET_*_URL",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
];

/// The single `AcceptEnv` line devbox writes, in both images.
pub fn accept_env_line() -> String {
    format!("AcceptEnv {}", SSH_ACCEPT_ENV.join(" "))
}

/// Whether this host has any reason to reconfigure a box's sshd.
///
/// The guest half of the ssh path is a `nixos-rebuild` on NixOS — minutes, and
/// a window where the box's firewall is down and then restored. Spending that
/// on a host that has no credential to send would be indefensible: there is
/// nothing for sshd to accept. So the check is gated on the broker being
/// configured at all, which is also the moment it starts being useful.
///
/// The consequence, stated plainly: the first devbox command that enters a box
/// after `devbox secret set` reconfigures that box. That is a one-time cost
/// per box, and it is why it is not paid by anyone who has not asked for a
/// broker.
pub fn wants_ssh_env(state_dir: &Path) -> bool {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return false;
    }
    !configured_providers(state_dir).is_empty()
}

/// Whether a box's effective `AcceptEnv` already covers what devbox sends.
///
/// Deliberately exact-token containment rather than pattern subsumption. The
/// question "does `DEVBOX_*` subsume `DEVBOX_SECRET_*_URL`" has a real answer,
/// but getting it wrong in the permissive direction leaves a box that silently
/// drops the broker variables — the failure this whole path exists to prevent.
/// Getting it wrong the other way costs one redundant reconfigure, which is
/// the same trade [`crate::sandbox::agent_sync::parse_probe`] already makes.
pub fn sshd_accepts_broker_env(configured: &[String]) -> bool {
    SSH_ACCEPT_ENV
        .iter()
        .all(|wanted| configured.iter().any(|have| have == wanted))
}

/// Render the `SetEnv` line for an ssh `Host` block.
///
/// **One line, every pair.** ssh takes the first obtained value for a keyword
/// and ignores later ones, so a block with three `SetEnv` lines sends only the
/// first variable — measured against a real box: three lines delivered one
/// variable into the session, one line with the same three pairs delivered all
/// three. Several pairs on a single line is the spelling that works.
///
/// Returns the empty string when there is nothing to send: a bare `SetEnv`
/// with no arguments is a config error, not a no-op.
///
/// A value that cannot be represented in ssh's config grammar is *dropped with
/// a warning* rather than escaped-by-hope. OpenSSH groups a token with double
/// quotes and offers no escape for a quote inside one, so a value containing
/// `"`, a backslash, or a newline has no faithful spelling; emitting a mangled
/// one would send the wrong token to the broker and produce a 401 that reads
/// like a broker bug. Every value devbox generates is a URL or hex, so this is
/// a guard against a future change, not a case that happens today.
pub fn set_env_line(env: &[(String, String)]) -> String {
    let tokens: Vec<String> = env
        .iter()
        .filter_map(|(name, value)| match ssh_config_token(name, value) {
            Some(token) => Some(token),
            None => {
                tracing::warn!(
                    variable = %name,
                    "cannot represent this value in an ssh config; `devbox code` will not send it"
                );
                None
            }
        })
        .collect();
    if tokens.is_empty() {
        return String::new();
    }
    format!("  SetEnv {}\n", tokens.join(" "))
}

/// One `NAME=value` token for an ssh config, or `None` if it cannot be one.
fn ssh_config_token(name: &str, value: &str) -> Option<String> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_name {
        return None;
    }
    if value.contains(['"', '\\', '\n', '\r']) || value.chars().any(|c| c.is_control()) {
        return None;
    }
    if value.contains(char::is_whitespace) || value.contains('#') {
        return Some(format!("\"{name}={value}\""));
    }
    Some(format!("{name}={value}"))
}

/// Wrap a guest command so it runs with `env`.
///
/// Every runtime's `exec_cmd`/`argv` takes an argv and no environment, and two
/// of the three silently drop `CreateOpts.env` (only Docker reads it) — so the
/// only place that works uniformly is the command line. `env` rather than a
/// shell, so no value is ever re-parsed by a shell in the guest.
///
/// The `--` goes *before* the assignments, not after. `env FOO=1 -- cmd` reads
/// `--` as the program name and fails with "No such file or directory", which
/// is exactly what the first run against a real box did.
pub fn with_env(env: &[(String, String)], cmd: &[String]) -> Vec<String> {
    if env.is_empty() {
        return cmd.to_vec();
    }
    let mut argv = vec!["env".to_string(), "--".to_string()];
    for (key, value) in env {
        argv.push(format!("{key}={value}"));
    }
    argv.extend(cmd.iter().cloned());
    argv
}

/// The `insteadOf` stanza provisioning writes into the guest gitconfig.
///
/// Measured shape: `git ls-remote https://github.com/o/r.git` under this
/// rewrite requests `GET /github/o/r.git/info/refs?service=git-upload-pack`
/// from the broker, which is exactly what [`providers::route`] maps back onto
/// `github.com`.
pub fn gitconfig_insteadof(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!(
        "[url \"{base}/github/\"]\n\tinsteadOf = https://github.com/\n\tinsteadOf = git@github.com:\n"
    )
}

/// Marker bracketing the devbox-managed part of the guest gitconfig.
///
/// The address changes whenever the box restarts onto a different port, so
/// this section is rewritten in place rather than appended — an append would
/// leave a stale `insteadOf` above the fresh one, and git takes the last.
pub const GITCONFIG_BEGIN: &str = "# >>> devbox broker (managed) >>>";
pub const GITCONFIG_END: &str = "# <<< devbox broker (managed) <<<";

/// Replace, or add, the managed section of a gitconfig.
pub fn apply_gitconfig_section(existing: &str, base_url: Option<&str>) -> String {
    let mut kept = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line.trim() == GITCONFIG_BEGIN {
            skipping = true;
            continue;
        }
        if line.trim() == GITCONFIG_END {
            skipping = false;
            continue;
        }
        if !skipping {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    while kept.ends_with("\n\n") {
        kept.pop();
    }
    let Some(base_url) = base_url else {
        return kept;
    };
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    format!(
        "{kept}{GITCONFIG_BEGIN}\n{}{GITCONFIG_END}\n",
        gitconfig_insteadof(base_url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_with_no_secrets_injects_nothing() {
        assert_eq!(
            session_env("http://h:1", "tok", &[]),
            vec![
                ("DEVBOX_BROKER_URL".to_string(), "http://h:1".to_string()),
                ("DEVBOX_BROKER_TOKEN".to_string(), "tok".to_string()),
            ]
        );
    }

    #[test]
    fn anthropic_gets_the_base_url_and_the_auth_token_but_never_an_api_key() {
        let env = session_env(
            "http://host.lima.internal:7879/",
            "tok",
            &["anthropic".to_string()],
        );
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://host.lima.internal:7879/anthropic")
        );
        assert_eq!(
            map.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("tok")
        );
        assert!(
            !map.contains_key("ANTHROPIC_API_KEY"),
            "setting both would put the token in x-api-key for no gain"
        );
    }

    #[test]
    fn openai_carries_the_v1_suffix_the_clients_expect() {
        let map: std::collections::HashMap<_, _> =
            session_env("http://h:1", "tok", &["openai".to_string()])
                .into_iter()
                .collect();
        assert_eq!(
            map.get("OPENAI_BASE_URL").map(String::as_str),
            Some("http://h:1/openai/v1")
        );
        assert_eq!(map.get("OPENAI_API_KEY").map(String::as_str), Some("tok"));
    }

    #[test]
    fn github_sets_no_gh_host_because_gh_forces_https() {
        let map: std::collections::HashMap<_, _> =
            session_env("http://h:1", "tok", &["github".to_string()])
                .into_iter()
                .collect();
        assert!(!map.contains_key("GH_HOST"));
        assert_eq!(
            map.get("DEVBOX_BROKER_GITHUB_URL").map(String::as_str),
            Some("http://h:1/github")
        );
    }

    #[test]
    fn a_generic_provider_gets_a_named_url_variable() {
        let map: std::collections::HashMap<_, _> =
            session_env("http://h:1", "tok", &["w1b-test".to_string()])
                .into_iter()
                .collect();
        assert_eq!(
            map.get("DEVBOX_SECRET_W1B_TEST_URL").map(String::as_str),
            Some("http://h:1/w1b-test")
        );
        assert_eq!(env_suffix("my.secret-1"), "MY_SECRET_1");
    }

    #[test]
    fn no_injected_variable_ever_carries_a_secret_shaped_value() {
        // The whole point of the component, asserted mechanically.
        let env = session_env(
            "http://h:1",
            "boxtoken",
            &[
                "anthropic".to_string(),
                "openai".to_string(),
                "github".to_string(),
                "w1b-test".to_string(),
            ],
        );
        for (key, value) in &env {
            assert!(
                !value.starts_with("sk-") && !value.starts_with("ghp_"),
                "{key} carries a credential-shaped value"
            );
        }
    }

    #[test]
    fn every_variable_session_env_can_produce_is_accepted_over_ssh() {
        // The two halves have to agree or `devbox code` silently drops
        // variables: what `session_env` sets is what sshd must let through.
        let produced = session_env(
            "http://h:1",
            "tok",
            &[
                "anthropic".to_string(),
                "openai".to_string(),
                "github".to_string(),
                "w1b-test".to_string(),
            ],
        );
        for (name, _) in &produced {
            assert!(
                SSH_ACCEPT_ENV
                    .iter()
                    .any(|pattern| glob_matches(pattern, name)),
                "{name} is set for a session but no AcceptEnv pattern covers it"
            );
        }
        // Plus the one the run wrapper adds.
        assert!(SSH_ACCEPT_ENV.contains(&"DEVBOX_RUN_ID"));
    }

    /// sshd's own `AcceptEnv` matching, only for the test above.
    fn glob_matches(pattern: &str, name: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == name,
            Some((head, tail)) => {
                name.len() >= head.len() + tail.len()
                    && name.starts_with(head)
                    && name.ends_with(tail)
            }
        }
    }

    #[test]
    fn a_box_is_current_only_when_it_has_every_token_devbox_writes() {
        let ours: Vec<String> = SSH_ACCEPT_ENV.iter().map(|s| s.to_string()).collect();
        assert!(sshd_accepts_broker_env(&ours));

        assert!(!sshd_accepts_broker_env(&[]));
        assert!(
            !sshd_accepts_broker_env(&["LANG".into(), "LC_*".into()]),
            "a stock Debian AcceptEnv is not enough"
        );

        // A broader hand-written pattern is not credited: see the doc comment.
        // The cost is one redundant reconfigure, not a silently broken box.
        assert!(!sshd_accepts_broker_env(&["DEVBOX_*".into()]));

        // Order does not matter, and extra entries are fine.
        let mut shuffled = ours.clone();
        shuffled.reverse();
        shuffled.push("LANG".into());
        assert!(sshd_accepts_broker_env(&shuffled));

        // Dropping any single one makes it stale.
        for index in 0..ours.len() {
            let mut missing = ours.clone();
            let removed = missing.remove(index);
            assert!(
                !sshd_accepts_broker_env(&missing),
                "a box missing {removed} must not read as current"
            );
        }
    }

    #[test]
    fn the_accept_env_line_is_one_line_of_valid_sshd_syntax() {
        let line = accept_env_line();
        assert!(line.starts_with("AcceptEnv "));
        assert!(!line.contains('\n'));
        assert!(line.contains("DEVBOX_BROKER_TOKEN"));
        assert!(line.contains("DEVBOX_SECRET_*_URL"));
    }

    /// Measured against a real box: a block carrying one `SetEnv` line per
    /// variable delivered exactly *one* variable into the session, because ssh
    /// takes the first obtained value for a keyword and ignores the rest. One
    /// line with the same pairs delivered all of them.
    #[test]
    fn every_variable_goes_on_one_set_env_line() {
        let env = vec![
            ("DEVBOX_BROKER_URL".to_string(), "http://h:7879".to_string()),
            ("DEVBOX_BROKER_TOKEN".to_string(), "deadbeef".to_string()),
        ];
        let rendered = set_env_line(&env);
        assert_eq!(
            rendered,
            "  SetEnv DEVBOX_BROKER_URL=http://h:7879 DEVBOX_BROKER_TOKEN=deadbeef\n"
        );
        assert_eq!(
            rendered.matches("SetEnv").count(),
            1,
            "a second SetEnv line would be dropped by ssh, silently"
        );
        // Nothing to send means no directive at all: a bare `SetEnv` with no
        // argument is a config error, not an empty setting.
        assert_eq!(set_env_line(&[]), "");
    }

    #[test]
    fn a_value_with_no_faithful_spelling_is_dropped_rather_than_mangled() {
        // Quotable: whitespace and `#` survive inside double quotes.
        assert_eq!(
            set_env_line(&[("A".into(), "one two".into())]),
            "  SetEnv \"A=one two\"\n"
        );
        assert_eq!(
            set_env_line(&[("A".into(), "x#y".into())]),
            "  SetEnv \"A=x#y\"\n"
        );

        // Not quotable: OpenSSH has no escape for these inside a token.
        for bad in ["a\"b", "a\\b", "a\nb", "a\rb", "a\u{7}b"] {
            assert_eq!(
                set_env_line(&[("A".into(), bad.to_string())]),
                "",
                "{bad:?} must not be emitted"
            );
        }
        // A name that is not a shell identifier is refused too.
        for bad in ["", "1A", "A-B", "A B", "A=B"] {
            assert_eq!(
                set_env_line(&[(bad.to_string(), "v".into())]),
                "",
                "{bad:?}"
            );
        }
        // One unrepresentable value does not take the others down with it.
        assert_eq!(
            set_env_line(&[("A".into(), "ok".into()), ("B".into(), "a\"b".into())]),
            "  SetEnv A=ok\n"
        );
        {}
    }

    #[test]
    fn wrapping_a_command_leaves_it_alone_when_there_is_nothing_to_inject() {
        let cmd = vec!["bash".to_string(), "-l".to_string()];
        assert_eq!(with_env(&[], &cmd), cmd);
        // `--` first: `env A=1 -- bash` reads `--` as the program.
        assert_eq!(
            with_env(&[("A".into(), "1".into())], &cmd),
            vec!["env", "--", "A=1", "bash", "-l"]
        );
    }

    #[test]
    fn the_gitconfig_section_is_replaced_rather_than_appended() {
        let base = "[user]\n\tname = Ethan\n";
        let once = apply_gitconfig_section(base, Some("http://h:1"));
        assert!(once.contains("[user]"));
        assert!(once.contains("http://h:1/github/"));
        assert!(once.contains("insteadOf = https://github.com/"));

        let twice = apply_gitconfig_section(&once, Some("http://h:2"));
        assert_eq!(twice.matches(GITCONFIG_BEGIN).count(), 1);
        assert!(twice.contains("http://h:2/github/"));
        assert!(
            !twice.contains("http://h:1/github/"),
            "a stale rewrite left above the fresh one wins in git"
        );
        assert!(twice.contains("[user]"));

        let removed = apply_gitconfig_section(&twice, None);
        assert!(!removed.contains(GITCONFIG_BEGIN));
        assert!(!removed.contains("/github/"));
        assert!(removed.contains("[user]"));
    }

    #[test]
    fn the_endpoint_record_round_trips_at_0600() {
        let dir = tempfile::tempdir().unwrap();
        assert!(endpoint(dir.path()).is_none());
        let want = Endpoint {
            port: 7879,
            pid: 4242,
            version: "1.2.3".into(),
        };
        publish_endpoint(dir.path(), &want).unwrap();
        assert_eq!(endpoint(dir.path()).unwrap(), want);
        let mode = std::fs::metadata(endpoint_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn the_provider_index_orders_the_builtins_first_and_survives_removal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(configured_providers(dir.path()).is_empty());

        for name in ["w1b-test", "github", "anthropic", "a-generic"] {
            index_add(dir.path(), name).unwrap();
        }
        assert_eq!(
            configured_providers(dir.path()),
            vec!["anthropic", "github", "a-generic", "w1b-test"]
        );

        let mode = std::fs::metadata(provider_index_dir(dir.path()).join("github"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        index_remove(dir.path(), "github");
        assert_eq!(
            configured_providers(dir.path()),
            vec!["anthropic", "a-generic", "w1b-test"]
        );
        // Removing something that was never there is not an error.
        index_remove(dir.path(), "github");
        index_remove(dir.path(), "../escape");
    }

    #[test]
    fn the_index_never_names_a_provider_that_could_escape_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(index_add(dir.path(), "../escape").is_err());
        std::fs::create_dir_all(provider_index_dir(dir.path())).unwrap();
        std::fs::write(provider_index_dir(dir.path()).join(".hidden"), b"").unwrap();
        assert!(configured_providers(dir.path()).is_empty());
    }

    #[test]
    fn paths_refuse_a_provider_name_that_would_escape_the_broker_directory() {
        let root = Path::new("/tmp/devbox-test-state");
        assert!(scope_path(root, "../../etc/passwd").is_none());
        assert!(http_provider_path(root, "a/b").is_none());
        assert_eq!(
            scope_path(root, "github").unwrap(),
            root.join("broker").join("scopes").join("github.json")
        );
    }
}
