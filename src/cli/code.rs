use anyhow::{Result, bail};
use clap::Args;

use crate::runtime::cmd::run_cmd;
use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;

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

    let ssh_config = result.stdout.trim().to_string();
    write_ssh_config(ssh_host, &ssh_config)?;

    launch_editor(editor, ssh_host, path)
}

/// Incus: get VM IP address, configure SSH, then launch editor.
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

    // Build SSH config block for this VM
    let ssh_config = format!(
        "Host {ssh_host}\n  HostName {ip}\n  User devbox\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null"
    );
    write_ssh_config(ssh_host, &ssh_config)?;

    launch_editor(editor, ssh_host, path)
}

/// Extract the first IPv4 address from `incus list --format json` output.
fn extract_incus_ip(json_output: &str) -> Result<String> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json_output)
        .map_err(|e| anyhow::anyhow!("Failed to parse Incus JSON: {e}"))?;

    for vm in &arr {
        if let Some(state) = vm.get("state") {
            if let Some(network) = state.get("network") {
                if let Some(obj) = network.as_object() {
                    for (iface, data) in obj {
                        if iface == "lo" {
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
