use anyhow::{Context, Result, bail};
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::runtime::cmd::run_cmd;
use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct CodeArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Editor command to use (code, cursor, windsurf, etc.)
    #[arg(long, default_value = "code")]
    pub editor: String,

    /// Path inside the sandbox to open (default: /workspace)
    #[arg(long, default_value = "/workspace")]
    pub path: String,
}

pub async fn run(args: CodeArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.boxarg.name())?;
    let crate::sandbox::Prepared {
        state,
        runtime,
        claim,
    } = manager.prepare_running_for_use(&name).await?;

    // Refresh overlay before opening editor to avoid stale file handles.
    // If a Zellij session is still attached, /workspace will be busy — that's
    // fine; the editor will still work with the current overlay state.
    if state.mount_mode != "writable" {
        println!("Refreshing overlay layer...");
        if let Err(e) = overlay::refresh(runtime.as_ref(), &name).await {
            let msg = e.to_string();
            if msg.contains("target is busy") || msg.contains("device is busy") {
                eprintln!(
                    "Note: overlay refresh skipped — /workspace is in use (e.g. Zellij session). This is normal."
                );
            } else {
                eprintln!("Warning: overlay refresh failed: {e}");
            }
        }
    }

    // The broker variables reach the editor's remote terminal as ssh
    // environment, not as an argv wrapper: `devbox code` launches VS Code on
    // the *host* and never spawns a guest process itself, so there is nothing
    // for `broker::with_env` to wrap. Resolved before the claim is dropped,
    // because the probe it does talks to a box whose mounts this command has
    // just settled.
    let broker_env = manager.broker_env(runtime.as_ref(), &name).await;

    // Mounts and posture are now coherent. Do not make the editor process own
    // the lifecycle claim after it launches.
    drop(claim);

    let vm_name = format!("devbox-{name}");
    let ssh_host = format!("devbox-{name}");

    match runtime.name() {
        "lima" => open_via_lima(&ssh_host, &vm_name, &args.editor, &args.path, &broker_env).await,
        "incus" => open_via_incus(&ssh_host, &vm_name, &args.editor, &args.path, &broker_env).await,
        other => bail!("Runtime '{other}' does not support `devbox code` yet."),
    }
}

/// Lima: extract SSH config and configure ~/.ssh/config, then launch editor.
async fn open_via_lima(
    ssh_host: &str,
    vm_name: &str,
    editor: &str,
    path: &str,
    broker_env: &[(String, String)],
) -> Result<()> {
    // Get SSH config from Lima
    let result = run_cmd("limactl", &["show-ssh", "--format", "config", vm_name]).await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to get SSH config from Lima: {}",
            result.stderr.trim()
        );
    }

    // Lima's output has its own Host line (e.g. "Host lima-devbox-test2").
    // Replace it with our ssh_host so VS Code can find the right config entry.
    let ssh_config = rewrite_ssh_host(ssh_host, result.stdout.trim());
    let ssh_config = resolve_block(&ssh_config, ssh_host, broker_env).await;
    write_ssh_config(ssh_host, &ssh_config)?;

    launch_editor(editor, ssh_host, path)
}

/// Incus: get VM IP address, configure SSH key auth, then launch editor.
async fn open_via_incus(
    ssh_host: &str,
    vm_name: &str,
    editor: &str,
    path: &str,
    broker_env: &[(String, String)],
) -> Result<()> {
    // Get IP from incus list
    let result = run_cmd("incus", &["list", vm_name, "--format", "json"]).await?;

    if result.exit_code != 0 {
        bail!("Failed to query Incus VM: {}", result.stderr.trim());
    }

    let ip = extract_incus_ip(&result.stdout)?;

    // Detect actual username in the VM (filter to /home/ users to skip nixbld*)
    let uid_result = run_cmd(
        "incus",
        &["exec", vm_name, "--", "bash", "-lc",
          "awk -F: '$3 >= 1000 && $3 < 65534 && $6 ~ /^\\/home\\// { print $1; exit }' /etc/passwd"],
    ).await?;
    let username = uid_result.stdout.trim();
    let username = if username.is_empty() { "dev" } else { username };

    // Ensure SSH key-based auth is set up (inject host pubkey into VM)
    ensure_ssh_key_auth(vm_name, username).await?;

    // Build SSH config block for this VM
    let home = dirs::home_dir().unwrap_or_default();
    let key_path = home.join(".ssh").join("id_ed25519");
    let key_fallback = home.join(".ssh").join("id_rsa");
    let identity = if key_path.exists() {
        key_path.to_string_lossy().to_string()
    } else if key_fallback.exists() {
        key_fallback.to_string_lossy().to_string()
    } else {
        // Will be created by ensure_ssh_key_auth
        key_path.to_string_lossy().to_string()
    };

    let ssh_config = format!(
        "Host {ssh_host}\n  HostName {ip}\n  User {username}\n  IdentityFile {identity}\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null"
    );
    let ssh_config = resolve_block(&ssh_config, ssh_host, broker_env).await;
    write_ssh_config(ssh_host, &ssh_config)?;

    launch_editor(editor, ssh_host, path)
}

/// Ensure SSH key-based auth is configured between host and Incus VM.
/// Generates a host key if needed, then injects the public key into the VM.
async fn ensure_ssh_key_auth(vm_name: &str, username: &str) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home dir"))?;
    let ssh_dir = home.join(".ssh");

    // Find or generate host SSH key
    let key_path = ssh_dir.join("id_ed25519");
    let pub_path = ssh_dir.join("id_ed25519.pub");

    if !pub_path.exists() {
        let rsa_pub = ssh_dir.join("id_rsa.pub");
        if !rsa_pub.exists() {
            // Generate a new key
            println!("Generating SSH key for devbox...");
            std::fs::create_dir_all(&ssh_dir)?;
            let status = std::process::Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f"])
                .arg(&key_path)
                .args(["-N", "", "-q"])
                .status()?;
            if !status.success() {
                bail!("Failed to generate SSH key");
            }
        }
    }

    // Read the public key
    let pub_key_path = if pub_path.exists() {
        &pub_path
    } else {
        &ssh_dir.join("id_rsa.pub")
    };
    let pubkey = std::fs::read_to_string(pub_key_path)
        .map_err(|e| anyhow::anyhow!("Cannot read SSH public key: {e}"))?;
    let pubkey = pubkey.trim();

    // Ensure sshd is enabled and the user's authorized_keys has our pubkey
    let setup_cmd = format!(
        "mkdir -p /home/{username}/.ssh && \
         chmod 700 /home/{username}/.ssh && \
         touch /home/{username}/.ssh/authorized_keys && \
         chmod 600 /home/{username}/.ssh/authorized_keys && \
         grep -qF '{pubkey}' /home/{username}/.ssh/authorized_keys 2>/dev/null || \
         echo '{pubkey}' >> /home/{username}/.ssh/authorized_keys && \
         chown -R $(id -u {username}):users /home/{username}/.ssh"
    );
    let result = run_cmd("incus", &["exec", vm_name, "--", "bash", "-lc", &setup_cmd]).await?;

    if result.exit_code != 0 {
        eprintln!(
            "Warning: SSH key setup may have failed: {}",
            result.stderr.trim()
        );
    }

    // Ensure sshd is running
    let _ = run_cmd(
        "incus",
        &["exec", vm_name, "--", "bash", "-lc", "systemctl enable --now sshd 2>/dev/null || systemctl enable --now ssh 2>/dev/null; true"],
    ).await;

    Ok(())
}

/// Check if a network interface name belongs to a container/virtual bridge
/// that should be skipped when looking for the VM's primary IP.
fn is_bridge_interface(iface: &str) -> bool {
    iface == "lo"
        || iface == "docker0"
        || iface.starts_with("br-")
        || iface.starts_with("veth")
        || iface.starts_with("virbr")
        || iface.starts_with("lxdbr")
        || iface.starts_with("incusbr")
}

/// Extract the first IPv4 address from `incus list --format json` output.
/// Skips loopback, Docker bridge, and other virtual bridge interfaces.
fn extract_incus_ip(json_output: &str) -> Result<String> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json_output)
        .map_err(|e| anyhow::anyhow!("Failed to parse Incus JSON: {e}"))?;

    for vm in &arr {
        if let Some(state) = vm.get("state")
            && let Some(network) = state.get("network")
            && let Some(obj) = network.as_object()
        {
            for (iface, data) in obj {
                if is_bridge_interface(iface) {
                    continue;
                }
                if let Some(addrs) = data.get("addresses")
                    && let Some(addrs_arr) = addrs.as_array()
                {
                    for addr in addrs_arr {
                        if addr.get("family").and_then(|f| f.as_str()) == Some("inet")
                            && let Some(ip) = addr.get("address").and_then(|a| a.as_str())
                        {
                            return Ok(ip.to_string());
                        }
                    }
                }
            }
        }
    }

    bail!("Could not find IP address for Incus VM. Is it running?")
}

/// Replace the `Host` line in an SSH config block with our desired host alias.
fn rewrite_ssh_host(desired_host: &str, config: &str) -> String {
    config
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("Host ") {
                format!("Host {desired_host}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Put the broker's variables into an ssh `Host` block as `SetEnv` lines.
///
/// Any `SetEnv` already in the block is dropped first, so calling this twice —
/// which box start and `devbox code` between them do — replaces rather than
/// accumulates. That matters: the box token is rotated on every start, and two
/// `SetEnv` lines for one name leave ssh sending whichever it read first.
pub fn with_broker_env(block: &str, env: &[(String, String)]) -> String {
    let mut out = String::new();
    for line in block.lines() {
        if line.trim_start().starts_with("SetEnv ") || line.trim() == "SetEnv" {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out.push_str(&crate::broker::set_env_line(env));
    out.trim_end().to_string()
}

/// Strip the connection-sharing directives from a `Host` block.
///
/// ssh's connection multiplexer does not carry `SetEnv` — measured: through
/// Lima's shared master a session received none of the three variables the
/// block sets; the same block with `ControlPath none` received all three. So a
/// block that inherits someone else's master cannot deliver the broker.
///
/// Removing them is only safe when the block can authenticate on its own,
/// which is what [`direct_connection_works`] establishes before this is used.
fn without_connection_sharing(block: &str) -> String {
    block
        .lines()
        .filter(|line| {
            let key = line.split_whitespace().next().unwrap_or("");
            !key.eq_ignore_ascii_case("ControlMaster")
                && !key.eq_ignore_ascii_case("ControlPath")
                && !key.eq_ignore_ascii_case("ControlPersist")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Can this block reach the box without borrowing an existing master?
///
/// Asked rather than assumed, for the same reason `Runtime::host_reach` probes
/// instead of trusting a documented address. A Lima box's `authorized_keys` is
/// installed under the home devbox gives the guest user, while sshd looks under
/// the one in `/etc/passwd`; where those differ, the only reason `devbox code`
/// works at all is the master `limactl` already authenticated. Guessing wrong
/// here would turn a working editor into "Permission denied".
async fn direct_connection_works(block: &str, host: &str) -> bool {
    let Ok(file) = tempfile::Builder::new()
        .prefix("devbox-ssh-")
        .suffix(".conf")
        .tempfile()
    else {
        return false;
    };
    if std::fs::write(file.path(), format!("{block}\n")).is_err() {
        return false;
    }
    let path = file.path().display().to_string();
    let result = run_cmd(
        "ssh",
        &[
            "-F",
            &path,
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlPath=none",
            "-o",
            "ControlMaster=no",
            "-o",
            "ConnectTimeout=8",
            host,
            "true",
        ],
    )
    .await;
    matches!(result, Ok(result) if result.exit_code == 0)
}

/// Build the block `devbox code` writes, resolving how it will connect.
///
/// Three outcomes, and the middle one is the point of the whole function:
///
/// * nothing to send — the block is left exactly as the runtime produced it;
/// * something to send and the block can stand on its own — connection sharing
///   is dropped so ssh opens its own session and carries the environment;
/// * something to send but the block cannot authenticate without the borrowed
///   master — the variables are written anyway (they cost nothing and start
///   working the moment the box's ssh is fixed) and the user is told plainly
///   that the editor's terminal will not have them.
async fn resolve_block(block: &str, host: &str, broker_env: &[(String, String)]) -> String {
    if broker_env.is_empty() {
        return block.to_string();
    }
    let with_env = with_broker_env(block, broker_env);
    let standalone = without_connection_sharing(&with_env);
    if !uses_connection_sharing(block) || direct_connection_works(&standalone, host).await {
        return standalone;
    }
    eprintln!(
        "{}",
        concat!(
            "Note: this box's ssh authenticates only through the connection its ",
            "runtime already opened, and ssh does not carry environment over a ",
            "shared connection. The editor's remote terminal will not have the ",
            "credential broker's variables; `devbox shell` and `devbox run` still do."
        )
    );
    with_env
}

/// Whether a block borrows a multiplexed connection.
fn uses_connection_sharing(block: &str) -> bool {
    block.lines().any(|line| {
        let key = line.split_whitespace().next().unwrap_or("");
        key.eq_ignore_ascii_case("ControlPath")
    })
}

/// The marker pair that delimits one box's block.
fn markers(host: &str) -> (String, String) {
    (
        format!("# devbox-start:{host}"),
        format!("# devbox-end:{host}"),
    )
}

/// Splice a block into an ssh config, replacing any block already there.
///
/// Pure, because the thing it edits is `~/.ssh/config` — a file whose other
/// contents belong to the user and are not devbox's to lose.
///
/// A start marker with no matching end marker is refused rather than repaired.
/// The previous version skipped to end-of-file in that case, which silently
/// deleted every host defined after devbox's block. There is no reading of a
/// half-open marker that is obviously right, and guessing wrong destroys
/// configuration the user cannot get back from us.
pub fn merge_ssh_block(existing: &str, host: &str, block: &str) -> Result<String> {
    let (marker_start, marker_end) = markers(host);
    let new_block = format!("{marker_start}\n{block}\n{marker_end}");

    let lines: Vec<&str> = existing.lines().collect();
    let start = lines.iter().position(|line| line.trim() == marker_start);
    let Some(start) = start else {
        let mut result = existing.to_string();
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&new_block);
        result.push('\n');
        return Ok(result);
    };

    let end = lines[start..]
        .iter()
        .position(|line| line.trim() == marker_end)
        .map(|offset| start + offset);
    let Some(end) = end else {
        bail!(
            "~/.ssh/config has a '{marker_start}' line with no matching '{marker_end}'. \
             Devbox will not rewrite a block it cannot find the end of — remove or repair \
             that section by hand, then run the command again."
        );
    };

    let mut result = String::new();
    for line in &lines[..start] {
        result.push_str(line);
        result.push('\n');
    }
    result.push_str(&new_block);
    result.push('\n');
    for line in &lines[end + 1..] {
        result.push_str(line);
        result.push('\n');
    }
    Ok(result)
}

/// Whether this host already has a devbox-managed block.
pub fn has_ssh_block(existing: &str, host: &str) -> bool {
    let (marker_start, _) = markers(host);
    existing.lines().any(|line| line.trim() == marker_start)
}

/// `~/.ssh/config`.
fn ssh_config_path() -> Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join(".ssh").join("config"))
}

/// Write or update an SSH config block in ~/.ssh/config for the devbox host.
fn write_ssh_config(host: &str, config_block: &str) -> Result<()> {
    let path = ssh_config_path()?;
    let ssh_dir = path.parent().context("ssh config has no parent")?;
    std::fs::create_dir_all(ssh_dir)?;

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = merge_ssh_block(&existing, host, config_block)?;
    std::fs::write(&path, &updated)?;
    protect_ssh_config(&path)?;
    Ok(())
}

/// Keep `~/.ssh/config` readable only by its owner.
///
/// The block devbox writes carries the box's broker token. That token is not
/// an API key — it buys scoped, logged, brokered access and nothing else — but
/// it is still a credential, and the default `std::fs::write` mode leaves it
/// group- and world-readable. Tightening is always safe for ssh, which refuses
/// a config that is *too* open and never one that is too closed.
fn protect_ssh_config(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protect {}", path.display()))?;
    }
    Ok(())
}

/// Whether `~/.ssh/config` already has a devbox block for this host.
///
/// Separate from [`refresh_broker_env`] so the lifecycle path can answer "is
/// there anything to refresh" without first probing the guest for the
/// environment it would write.
pub fn has_managed_block(host: &str) -> Result<bool> {
    let existing = std::fs::read_to_string(ssh_config_path()?).unwrap_or_default();
    Ok(has_ssh_block(&existing, host))
}

/// Refresh the `SetEnv` lines of an existing block, for a box that just started.
///
/// **Only refreshes; never creates.** A user who has never run `devbox code`
/// has no devbox block in `~/.ssh/config`, and a lifecycle command that put one
/// there would be writing to the user's ssh configuration for a feature they
/// have not asked for. Once `devbox code` has created the block, this is what
/// keeps its token from going stale — it is rotated on every box start, and a
/// stale one fails as a 401 from the broker, which reads like a broker bug
/// rather than like a config that needs refreshing.
pub fn refresh_broker_env(host: &str, env: &[(String, String)]) -> Result<bool> {
    let path = ssh_config_path()?;
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    if !has_ssh_block(&existing, host) {
        return Ok(false);
    }
    let (marker_start, marker_end) = markers(host);
    let lines: Vec<&str> = existing.lines().collect();
    let Some(start) = lines.iter().position(|line| line.trim() == marker_start) else {
        return Ok(false);
    };
    let Some(end) = lines[start..]
        .iter()
        .position(|line| line.trim() == marker_end)
        .map(|offset| start + offset)
    else {
        return Ok(false);
    };
    let block = lines[start + 1..end].join("\n");
    let updated = merge_ssh_block(&existing, host, &with_broker_env(&block, env))?;
    if updated == existing {
        return Ok(false);
    }
    std::fs::write(&path, &updated)?;
    protect_ssh_config(&path)?;
    Ok(true)
}

/// Launch the editor with Remote SSH targeting the sandbox.
fn launch_editor(editor: &str, ssh_host: &str, path: &str) -> Result<()> {
    // Check if the editor is installed
    if which::which(editor).is_err() {
        bail!(
            "'{editor}' not found in PATH. Install it or use --editor to specify another editor.\n\
             Supported: code (VS Code), cursor, windsurf, or any editor with Remote SSH support."
        );
    }

    println!("Opening {editor} → {ssh_host}:{path}");

    let remote_arg = format!("ssh-remote+{ssh_host}");
    let status = std::process::Command::new(editor)
        .arg("--remote")
        .arg(&remote_arg)
        .arg(path)
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to launch {editor}: {e}"))?;

    if !status.success() {
        bail!("{editor} exited with status: {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const LIMA_BLOCK: &str = "\
Host devbox-devtest
  HostName 127.0.0.1
  Port 60349
  User ethan";

    #[test]
    fn the_broker_variables_land_inside_the_host_block() {
        let block = with_broker_env(
            LIMA_BLOCK,
            &env(&[
                ("DEVBOX_BROKER_URL", "http://host.lima.internal:7879"),
                ("DEVBOX_BROKER_TOKEN", "deadbeef"),
            ]),
        );
        assert!(block.starts_with("Host devbox-devtest\n"));
        assert!(block.contains("  Port 60349\n"));
        // Indented, so ssh reads them as part of the block rather than as a
        // new top-level stanza — and on one line, because ssh honours only the
        // first `SetEnv` it sees for a host.
        assert!(block.contains(
            "\n  SetEnv DEVBOX_BROKER_URL=http://host.lima.internal:7879 \
             DEVBOX_BROKER_TOKEN=deadbeef"
        ));
        assert_eq!(block.matches("SetEnv").count(), 1);
    }

    #[test]
    fn a_rotated_token_replaces_the_previous_one_instead_of_joining_it() {
        // The token is rotated on every box start, and ssh takes the first
        // value it reads — so an appended `SetEnv` would leave the box
        // authenticating with the session before last, forever.
        let once = with_broker_env(LIMA_BLOCK, &env(&[("DEVBOX_BROKER_TOKEN", "first")]));
        let twice = with_broker_env(&once, &env(&[("DEVBOX_BROKER_TOKEN", "second")]));
        assert_eq!(twice.matches("SetEnv DEVBOX_BROKER_TOKEN").count(), 1);
        assert!(twice.contains("SetEnv DEVBOX_BROKER_TOKEN=second"));
        assert!(!twice.contains("first"));
        assert!(twice.contains("Port 60349"));
    }

    #[test]
    fn a_host_with_no_broker_gets_a_block_with_no_set_env_at_all() {
        let block = with_broker_env(LIMA_BLOCK, &[]);
        assert!(!block.contains("SetEnv"));
        assert_eq!(block, LIMA_BLOCK);

        // And a block that had them, when the secrets are gone, loses them.
        let had = with_broker_env(LIMA_BLOCK, &env(&[("DEVBOX_BROKER_TOKEN", "x")]));
        assert_eq!(with_broker_env(&had, &[]), LIMA_BLOCK);
    }

    #[test]
    fn a_new_block_is_appended_without_disturbing_the_rest_of_the_config() {
        let existing = "Host prod\n  HostName prod.example\n";
        let merged = merge_ssh_block(existing, "devbox-devtest", LIMA_BLOCK).unwrap();
        assert!(merged.starts_with("Host prod\n  HostName prod.example\n"));
        assert!(merged.contains("# devbox-start:devbox-devtest\n"));
        assert!(merged.contains("# devbox-end:devbox-devtest\n"));
        assert!(merged.contains("Host devbox-devtest"));

        // An empty config gets a block and no leading blank line.
        let fresh = merge_ssh_block("", "devbox-devtest", LIMA_BLOCK).unwrap();
        assert!(fresh.starts_with("# devbox-start:devbox-devtest\n"));
    }

    #[test]
    fn an_existing_block_is_replaced_and_the_hosts_around_it_survive() {
        let existing = merge_ssh_block("Host before\n  HostName a\n", "devbox-devtest", LIMA_BLOCK)
            .unwrap()
            + "\nHost after\n  HostName b\n";

        let replaced = merge_ssh_block(
            &existing,
            "devbox-devtest",
            "Host devbox-devtest\n  Port 61000",
        )
        .unwrap();

        assert_eq!(replaced.matches("# devbox-start:devbox-devtest").count(), 1);
        assert!(replaced.contains("  Port 61000"));
        assert!(!replaced.contains("60349"));
        assert!(
            replaced.contains("Host before") && replaced.contains("Host after"),
            "the user's own hosts must survive: {replaced}"
        );
    }

    /// The previous implementation skipped to end-of-file when the end marker
    /// was missing, which deleted every host defined after devbox's block.
    #[test]
    fn a_block_with_no_end_marker_is_refused_rather_than_swallowing_the_file() {
        let mangled = "\
Host before
  HostName a
# devbox-start:devbox-devtest
Host devbox-devtest
  Port 1
Host after
  HostName b
";
        let error = merge_ssh_block(mangled, "devbox-devtest", LIMA_BLOCK).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("devbox-end:devbox-devtest"), "{text}");
        assert!(text.contains("by hand"), "{text}");
    }

    #[test]
    fn a_block_is_recognised_only_for_its_own_host() {
        let merged = merge_ssh_block("", "devbox-devtest", LIMA_BLOCK).unwrap();
        assert!(has_ssh_block(&merged, "devbox-devtest"));
        assert!(!has_ssh_block(&merged, "devbox-other"));
        assert!(!has_ssh_block("", "devbox-devtest"));
    }

    /// The whole round trip a box start does: read the config, refresh only
    /// the `SetEnv` lines of an existing block, leave everything else alone.
    #[test]
    fn refreshing_touches_only_the_set_env_lines() {
        let existing = merge_ssh_block(
            "Host prod\n  HostName prod.example\n",
            "devbox-devtest",
            &with_broker_env(LIMA_BLOCK, &env(&[("DEVBOX_BROKER_TOKEN", "old")])),
        )
        .unwrap();

        // Same operation `refresh_broker_env` performs, on the text.
        let lines: Vec<&str> = existing.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.trim() == "# devbox-start:devbox-devtest")
            .unwrap();
        let end = lines
            .iter()
            .position(|l| l.trim() == "# devbox-end:devbox-devtest")
            .unwrap();
        let block = lines[start + 1..end].join("\n");
        let refreshed = merge_ssh_block(
            &existing,
            "devbox-devtest",
            &with_broker_env(&block, &env(&[("DEVBOX_BROKER_TOKEN", "new")])),
        )
        .unwrap();

        assert!(refreshed.contains("SetEnv DEVBOX_BROKER_TOKEN=new"));
        assert!(!refreshed.contains("old"));
        assert!(refreshed.contains("  Port 60349"));
        assert!(refreshed.contains("Host prod"));
        assert_eq!(refreshed.matches("SetEnv").count(), 1);
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    const SHARED: &str = "\
Host devbox-w34box
  Hostname 127.0.0.1
  Port 49695
  ControlMaster auto
  ControlPath /Users/x/.lima/devbox-w34box/ssh.sock
  ControlPersist yes
  User ethan";

    #[test]
    fn a_block_that_borrows_a_master_is_recognised() {
        assert!(uses_connection_sharing(SHARED));
        assert!(!uses_connection_sharing("Host x\n  Hostname 1.2.3.4"));
        // Incus builds its own block and shares nothing, which is why the
        // environment reaches an Incus box without any of this.
        assert!(!uses_connection_sharing(
            "Host devbox-x\n  HostName 10.0.0.2\n  User dev\n  StrictHostKeyChecking no"
        ));
    }

    #[test]
    fn stripping_connection_sharing_leaves_everything_else_intact() {
        let stripped = without_connection_sharing(SHARED);
        assert!(!stripped.contains("Control"));
        assert!(stripped.contains("  Port 49695"));
        assert!(stripped.contains("  User ethan"));
        assert!(stripped.starts_with("Host devbox-w34box"));
        // Case-insensitively, because ssh config keywords are.
        assert!(!without_connection_sharing("Host x\n  controlpath /s").contains("controlpath"));
        // A host *named* like the keyword is not a directive.
        let keep = "Host x\n  HostName controlpath.example\n";
        assert!(without_connection_sharing(keep).contains("controlpath.example"));
    }

    #[tokio::test]
    async fn nothing_to_send_leaves_the_runtimes_block_untouched() {
        assert_eq!(resolve_block(SHARED, "devbox-w34box", &[]).await, SHARED);
    }

    #[tokio::test]
    async fn a_block_that_shares_nothing_gets_the_variables_without_a_probe() {
        // No ControlPath, so there is nothing to strip and no reason to spend
        // an ssh round trip asking whether stripping would be safe.
        let plain = "Host devbox-x\n  HostName 10.0.0.2\n  User dev";
        let block = resolve_block(
            plain,
            "devbox-x",
            &[("DEVBOX_BROKER_TOKEN".to_string(), "abc".to_string())],
        )
        .await;
        assert!(block.contains("  SetEnv DEVBOX_BROKER_TOKEN=abc"));
        assert!(block.contains("  HostName 10.0.0.2"));
    }
}
