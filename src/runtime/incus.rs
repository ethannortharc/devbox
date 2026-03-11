use anyhow::{Result, bail};
use async_trait::async_trait;

use super::cmd::{run_cmd, run_interactive, run_ok};
use super::{CreateOpts, ExecResult, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo};

/// Incus runtime — primary on Linux (QEMU/KVM VM).
pub struct IncusRuntime;

impl IncusRuntime {
    /// All Incus VMs managed by devbox are prefixed with "devbox-".
    fn vm_name(name: &str) -> String {
        format!("devbox-{name}")
    }

    /// Local image alias for the given image type.
    fn image_alias(image_type: &str) -> &'static str {
        match image_type {
            "ubuntu" => "devbox-ubuntu",
            _ => "devbox-nixos",
        }
    }

    /// Remote source in the official images: remote for each supported image type.
    fn remote_image(image_type: &str) -> &'static str {
        match image_type {
            "ubuntu" => "images:ubuntu/24.04",
            _ => "images:nixos/25.11",
        }
    }

    /// Ensure the base image exists locally, downloading it from the official
    /// `images:` remote if necessary.
    async fn ensure_image(image_type: &str) -> Result<()> {
        let alias = Self::image_alias(image_type);

        // Check whether the image already exists locally.
        let result = run_cmd(
            "incus",
            &[
                "image",
                "list",
                &format!("local:{alias}"),
                "--format",
                "json",
            ],
        )
        .await?;

        if result.exit_code == 0 {
            // Parse the JSON array — an empty array means no match.
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&result.stdout)
                && !arr.is_empty()
            {
                return Ok(());
            }
        }

        let remote = Self::remote_image(image_type);

        println!(
            "Base image '{alias}' not found locally. Downloading from {remote} — this may take a few minutes..."
        );

        run_ok(
            "incus",
            &["image", "copy", remote, "local:", "--alias", alias, "--vm"],
        )
        .await?;

        println!("Image '{alias}' imported successfully.");
        Ok(())
    }
    /// Detect the UID of the first non-root user in the VM.
    async fn detect_vm_uid(vm: &str) -> Option<String> {
        // Find first user with UID >= 1000 (standard non-system user)
        let result = run_cmd(
            "incus",
            &["exec", vm, "--", "bash", "-c",
              "awk -F: '$3 >= 1000 && $3 < 65534 { print $3; exit }' /etc/passwd"],
        ).await.ok()?;
        let uid = result.stdout.trim().to_string();
        if uid.is_empty() { None } else { Some(uid) }
    }

    /// Detect the HOME directory for a given UID in the VM.
    async fn detect_vm_home(vm: &str, uid: &str) -> String {
        let result = run_cmd(
            "incus",
            &["exec", vm, "--", "bash", "-c",
              &format!("getent passwd {uid} | cut -d: -f6")],
        ).await;
        match result {
            Ok(r) if !r.stdout.trim().is_empty() => {
                format!("HOME={}", r.stdout.trim())
            }
            _ => format!("HOME=/home/dev"),
        }
    }

    /// Wait for the Incus VM agent to become ready (up to 120 seconds).
    /// The agent starts after the guest OS boots and runs incus-agent.
    async fn wait_for_agent(vm: &str) -> Result<()> {
        let max_attempts = 40; // 40 * 3s = 120s
        for i in 0..max_attempts {
            let result = run_cmd("incus", &["exec", vm, "--", "echo", "ready"]).await?;
            if result.exit_code == 0 && result.stdout.trim() == "ready" {
                println!("VM agent is ready.");
                return Ok(());
            }
            if i > 0 && i % 10 == 0 {
                println!("  Still waiting for VM agent... ({i}s)", i = i * 3);
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        bail!("VM agent did not become ready within 120 seconds. The VM may still be booting — try `devbox shell` in a minute.")
    }
}

#[async_trait]
impl Runtime for IncusRuntime {
    fn name(&self) -> &str {
        "incus"
    }

    fn is_available(&self) -> bool {
        which::which("incus").is_ok()
    }

    fn priority(&self) -> u32 {
        30
    }

    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo> {
        let vm = Self::vm_name(&opts.name);

        // Check if already exists
        let result = run_cmd("incus", &["info", &vm]).await?;
        if result.exit_code == 0 {
            bail!(
                "Incus VM '{}' already exists. Use `devbox destroy {}` first.",
                vm,
                opts.name
            );
        }

        // Ensure the base image is available (auto-download if missing).
        Self::ensure_image(&opts.image).await?;

        // Launch the VM
        println!("Creating Incus VM '{vm}'...");
        let image = Self::image_alias(&opts.image);
        let mut launch_args = vec!["launch", image, &vm, "--vm", "-c", "security.secureboot=false"];

        let cpu_str;
        if opts.cpu > 0 {
            cpu_str = format!("limits.cpu={}", opts.cpu);
            launch_args.push("-c");
            launch_args.push(&cpu_str);
        }

        // Default to 4GiB memory for Incus VMs — NixOS rebuild needs 2-4GB
        // for evaluating the full module system. The default Incus 1GB is too little.
        let mem_str;
        let memory = if opts.memory.is_empty() { "4GiB" } else { &opts.memory };
        mem_str = format!("limits.memory={memory}");
        launch_args.push("-c");
        launch_args.push(&mem_str);

        run_ok("incus", &launch_args).await?;

        // Wait for the VM agent to be ready before provisioning.
        // The guest agent takes time to start after boot.
        println!("Waiting for VM agent to be ready...");
        Self::wait_for_agent(&vm).await?;

        // Add mounts
        for (i, m) in opts.mounts.iter().enumerate() {
            let device_name = format!("mount{i}");
            let host = m.host_path.display().to_string();
            let source_arg = format!("source={host}");
            let path_arg = format!("path={}", m.container_path);

            let mut mount_args = vec!["config", "device", "add", &vm, &device_name, "disk"];
            mount_args.push(&source_arg);
            mount_args.push(&path_arg);

            if m.read_only {
                mount_args.push("readonly=true");
            }

            run_ok("incus", &mount_args).await?;
        }

        Ok(SandboxInfo {
            name: opts.name.clone(),
            status: SandboxStatus::Running,
            runtime: "incus".to_string(),
            created_at: Some(chrono_now()),
            ip_address: None,
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("incus", &["start", &vm]).await?;
        Self::wait_for_agent(&vm).await?;
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("incus", &["stop", &vm]).await?;
        Ok(())
    }

    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult> {
        let vm = Self::vm_name(name);

        if interactive {
            let mut args = vec!["exec", &vm, "--"];
            args.extend_from_slice(cmd);
            run_interactive("incus", &args).await
        } else {
            let mut args = vec!["exec", &vm, "--"];
            args.extend_from_slice(cmd);
            run_cmd("incus", &args).await
        }
    }

    /// Execute an interactive command as the non-root user.
    /// Incus exec defaults to root, so we detect the first UID >= 1000
    /// and set --user, HOME, and CWD for proper user sessions.
    async fn exec_as_user(&self, name: &str, cmd: &[&str]) -> Result<ExecResult> {
        let vm = Self::vm_name(name);
        let uid_str = Self::detect_vm_uid(&vm).await.unwrap_or("1000".to_string());
        let home_env = Self::detect_vm_home(&vm, &uid_str).await;

        // Detect the user's home directory from the HOME env string
        let home_dir = home_env.strip_prefix("HOME=").unwrap_or("/home/dev");
        let path_env = format!(
            "PATH={home_dir}/.npm-global/bin:{home_dir}/.local/bin:{home_dir}/.claude/bin:\
             /run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:\
             /usr/local/bin:/usr/bin:/bin"
        );

        let mut args = vec![
            "exec".to_string(),
            vm,
            "--user".to_string(),
            uid_str,
            "--cwd".to_string(),
            "/workspace".to_string(),
            "--env".to_string(),
            home_env,
            "--env".to_string(),
            path_env,
            "--".to_string(),
        ];
        for c in cmd {
            args.push(c.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_interactive("incus", &arg_refs).await
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        // Stop first (ignore errors if already stopped)
        let _ = run_cmd("incus", &["stop", &vm, "--force"]).await;
        run_ok("incus", &["delete", &vm, "--force"]).await?;
        Ok(())
    }

    async fn status(&self, name: &str) -> Result<SandboxStatus> {
        let vm = Self::vm_name(name);
        let result = run_cmd("incus", &["info", &vm]).await?;

        if result.exit_code != 0 {
            return Ok(SandboxStatus::NotFound);
        }

        // Parse "Status: RUNNING" or "Status: STOPPED" from info output
        for line in result.stdout.lines() {
            let line = line.trim();
            if let Some(status_val) = line.strip_prefix("Status:") {
                return Ok(match status_val.trim().to_uppercase().as_str() {
                    "RUNNING" => SandboxStatus::Running,
                    "STOPPED" => SandboxStatus::Stopped,
                    other => SandboxStatus::Unknown(other.to_string()),
                });
            }
        }

        Ok(SandboxStatus::Unknown("no status found".to_string()))
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let result = run_cmd("incus", &["list", "--format", "json"]).await?;

        if result.exit_code != 0 {
            return Ok(vec![]);
        }

        let mut infos = vec![];
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&result.stdout) {
            for v in arr {
                let name = v["name"].as_str().unwrap_or("").to_string();
                if !name.starts_with("devbox-") {
                    continue;
                }
                let sandbox_name = name.strip_prefix("devbox-").unwrap_or(&name).to_string();
                let status_str = v["status"].as_str().unwrap_or("").to_uppercase();
                let status = match status_str.as_str() {
                    "RUNNING" => SandboxStatus::Running,
                    "STOPPED" => SandboxStatus::Stopped,
                    other => SandboxStatus::Unknown(other.to_string()),
                };

                infos.push(SandboxInfo {
                    name: sandbox_name,
                    status,
                    runtime: "incus".to_string(),
                    created_at: v["created_at"].as_str().map(|s| s.to_string()),
                    ip_address: None,
                });
            }
        }

        Ok(infos)
    }

    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("incus", &["snapshot", "create", &vm, snap]).await?;
        Ok(())
    }

    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("incus", &["snapshot", "restore", &vm, snap]).await?;
        Ok(())
    }

    async fn snapshot_list(&self, name: &str) -> Result<Vec<SnapshotInfo>> {
        let vm = Self::vm_name(name);
        let result = run_cmd("incus", &["snapshot", "list", &vm, "--format", "json"]).await?;

        if result.exit_code != 0 {
            return Ok(vec![]);
        }

        let mut snaps = vec![];
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&result.stdout) {
            for v in arr {
                snaps.push(SnapshotInfo {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    created_at: v["created_at"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        Ok(snaps)
    }

    async fn upgrade(&self, _name: &str, _tools: &[String]) -> Result<()> {
        todo!("Phase 5: Incus upgrade")
    }

    async fn update_mounts(&self, _name: &str, _mounts: &[super::Mount]) -> Result<()> {
        bail!("Updating mounts is not supported for the Incus runtime")
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s-since-epoch", now.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_name_prefix() {
        assert_eq!(IncusRuntime::vm_name("myapp"), "devbox-myapp");
    }
}
