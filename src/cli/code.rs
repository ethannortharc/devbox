use anyhow::{Result, bail};
use clap::Args;

use crate::runtime::cmd::run_cmd;
use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;
use crate::sandbox::overlay;

#[derive(Args, Debug)]
pub struct CodeArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,

    /// Editor command to use (code, cursor, windsurf, etc.)
    #[arg(long, default_value = "code")]
    pub editor: String,

    /// Path inside the sandbox to open (default: /workspace)
    #[arg(long, default_value = "/workspace")]
    pub path: String,
}

pub async fn run(args: CodeArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let state = manager.get_sandbox(&name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    // Ensure sandbox is running
    let status = runtime.status(&name).await?;
    match status {
        SandboxStatus::Running => {}
        SandboxStatus::Stopped => {
            println!("Starting sandbox '{name}'...");
            runtime.start(&name).await?;
        }
        SandboxStatus::NotFound => bail!("Sandbox '{name}' not found."),
        SandboxStatus::Unknown(s) => bail!("Sandbox '{name}' is in unknown state: {s}"),
    }

    // Refresh overlay before opening editor to avoid stale file handles.
    // If a Zellij session is still attached, /workspace will be busy — that's
    // fine; the editor will still work with the current overlay state.
    if state.mount_mode != "writable" {
        println!("Refreshing overlay layer...");
        if let Err(e) = overlay::refresh(runtime.as_ref(), &name).await {
            let msg = e.to_string();
            if msg.contains("target is busy") || msg.contains("device is busy") {
                eprintln!("Note: overlay refresh skipped — /workspace is in use (e.g. Zellij session). This is normal.");
            } else {
                eprintln!("Warning: overlay refresh failed: {e}");
            }
        }
    }

    let vm_name = format!("devbox-{name}");
    let ssh_host = format!("devbox-{name}");

    match runtime.name() {
        "lima" => open_via_lima(&ssh_host, &vm_name, &args.editor, &args.path).await,
        "incus" => open_via_incus(&ssh_host, &vm_name, &args.editor, &args.path).await,
        other => bail!("Runtime '{other}' does not support `devbox code` yet."),
    }
}

/// Lima: extract SSH config and configure ~/.ssh/config, then launch editor.
async fn open_via_lima(
    ssh_host: &str,
    vm_name: &str,
    editor: &str,
    path: &str,
) -> Result<()> {
    // Get SSH config from Lima
    let result = run_cmd(
        "limactl",
        &["show-ssh", "--format", "config", vm_name],
    )
    .await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to get SSH config from Lima: {}",
            result.stderr.trim()
        );
    }

    // Lima's output has its own Host line (e.g. "Host lima-devbox-test2").
    // Replace it with our ssh_host so VS Code can find the right config entry.
    let ssh_config = rewrite_ssh_host(ssh_host, result.stdout.trim());
    write_ssh_config(ssh_host, &ssh_config)?;

    launch_editor(editor, ssh_host, path)
}

/// Incus: get VM IP address, configure SSH key auth, then launch editor.
async fn open_via_incus(
    ssh_host: &str,
    vm_name: &str,
    editor: &str,
    path: &str,
) -> Result<()> {
    // Get IP from incus list
    let result = run_cmd(
        "incus",
        &["list", vm_name, "--format", "json"],
    )
    .await?;

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
    let pub_key_path = if pub_path.exists() { &pub_path } else { &ssh_dir.join("id_rsa.pub") };
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
    let result = run_cmd(
        "incus",
        &["exec", vm_name, "--", "bash", "-lc", &setup_cmd],
    ).await?;

    if result.exit_code != 0 {
        eprintln!("Warning: SSH key setup may have failed: {}", result.stderr.trim());
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
        if let Some(state) = vm.get("state") {
            if let Some(network) = state.get("network") {
                if let Some(obj) = network.as_object() {
                    for (iface, data) in obj {
                        if is_bridge_interface(iface) {
                            continue;
                        }
                        if let Some(addrs) = data.get("addresses") {
                            if let Some(addrs_arr) = addrs.as_array() {
                                for addr in addrs_arr {
                                    if addr.get("family").and_then(|f| f.as_str())
                                        == Some("inet")
                                    {
                                        if let Some(ip) =
                                            addr.get("address").and_then(|a| a.as_str())
                                        {
                                            return Ok(ip.to_string());
                                        }
                                    }
                                }
                            }
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

/// Write or update an SSH config block in ~/.ssh/config for the devbox host.
fn write_ssh_config(host: &str, config_block: &str) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir)?;

    let config_path = ssh_dir.join("config");
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Check if we already have a block for this host
    let marker_start = format!("# devbox-start:{host}");
    let marker_end = format!("# devbox-end:{host}");

    let new_block = format!("{marker_start}\n{config_block}\n{marker_end}");

    let updated = if existing.contains(&marker_start) {
        // Replace existing block
        let mut result = String::new();
        let mut skip = false;
        for line in existing.lines() {
            if line.trim() == marker_start {
                skip = true;
                result.push_str(&new_block);
                result.push('\n');
            } else if line.trim() == marker_end {
                skip = false;
            } else if !skip {
                result.push_str(line);
                result.push('\n');
            }
        }
        result
    } else {
        // Append new block
        let mut result = existing;
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push('\n');
        result.push_str(&new_block);
        result.push('\n');
        result
    };

    std::fs::write(&config_path, &updated)?;
    Ok(())
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
