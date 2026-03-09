use anyhow::{Result, bail};
use async_trait::async_trait;

use super::cmd::{run_ok, run_cmd, run_interactive};
use super::{CreateOpts, ExecResult, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo};

/// Docker runtime — fallback (weaker isolation, shared kernel).
pub struct DockerRuntime;

impl DockerRuntime {
    /// All Docker containers managed by devbox are prefixed with "devbox-".
    fn container_name(name: &str) -> String {
        format!("devbox-{name}")
    }

    /// Base NixOS Docker image name.
    fn image_name() -> &'static str {
        "devbox-nixos:latest"
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    fn name(&self) -> &str {
        "docker"
    }

    fn is_available(&self) -> bool {
        which::which("docker").is_ok()
    }

    fn priority(&self) -> u32 {
        10
    }

    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo> {
        let container = Self::container_name(&opts.name);

        // Check if container already exists
        let result = run_cmd("docker", &["container", "inspect", &container]).await?;
        if result.exit_code == 0 {
            bail!(
                "Docker container '{}' already exists. Use `devbox destroy {}` first.",
                container,
                opts.name
            );
        }

        // Build docker run args
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            container.clone(),
            "--hostname".to_string(),
            format!("devbox-{}", opts.name),
        ];

        // CPU/memory limits
        if opts.cpu > 0 {
            args.push("--cpus".to_string());
            args.push(opts.cpu.to_string());
        }
        if !opts.memory.is_empty() {
            args.push("--memory".to_string());
            args.push(opts.memory.clone());
        }

        // Mounts
        for m in &opts.mounts {
            let host = m.host_path.display();
            let target = &m.container_path;
            let ro = if m.read_only { ",readonly" } else { "" };
            args.push("-v".to_string());
            args.push(format!("{host}:{target}{ro}"));
        }

        // Environment variables
        for (k, v) in &opts.env {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        // Env file
        if let Some(env_file) = &opts.env_file {
            args.push("--env-file".to_string());
            args.push(env_file.display().to_string());
        }

        // Label for devbox management
        args.push("--label".to_string());
        args.push("devbox=true".to_string());

        // Image
        args.push(Self::image_name().to_string());

        println!("Creating Docker container '{container}'...");
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_ok("docker", &args_ref).await?;

        Ok(SandboxInfo {
            name: opts.name.clone(),
            status: SandboxStatus::Running,
            runtime: "docker".to_string(),
            created_at: Some(chrono_now()),
            ip_address: None,
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        let container = Self::container_name(name);
        run_ok("docker", &["start", &container]).await?;
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let container = Self::container_name(name);
        run_ok("docker", &["stop", &container]).await?;
        Ok(())
    }

    async fn exec_cmd(
        &self,
        name: &str,
        cmd: &[&str],
        interactive: bool,
    ) -> Result<ExecResult> {
        let container = Self::container_name(name);

        if interactive {
            let mut args = vec!["exec", "-it", &container];
            args.extend_from_slice(cmd);
            run_interactive("docker", &args).await
        } else {
            let mut args = vec!["exec", &container];
            args.extend_from_slice(cmd);
            run_cmd("docker", &args).await
        }
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        let container = Self::container_name(name);
        // Force remove (stops if running)
        run_ok("docker", &["rm", "-f", &container]).await?;
        Ok(())
    }

    async fn status(&self, name: &str) -> Result<SandboxStatus> {
        let container = Self::container_name(name);
        let result = run_cmd(
            "docker",
            &["container", "inspect", "--format", "{{.State.Status}}", &container],
        )
        .await?;

        if result.exit_code != 0 {
            return Ok(SandboxStatus::NotFound);
        }

        Ok(match result.stdout.trim() {
            "running" => SandboxStatus::Running,
            "exited" | "created" | "dead" => SandboxStatus::Stopped,
            other => SandboxStatus::Unknown(other.to_string()),
        })
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let result = run_cmd(
            "docker",
            &[
                "ps",
                "-a",
                "--filter",
                "label=devbox=true",
                "--format",
                "{{.Names}}\t{{.Status}}",
            ],
        )
        .await?;

        let mut infos = vec![];
        for line in result.stdout.lines() {
            let parts: Vec<&str> = line.splitn(2, '\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let full_name = parts[0];
            if !full_name.starts_with("devbox-") {
                continue;
            }
            let sandbox_name = full_name.strip_prefix("devbox-").unwrap_or(full_name);
            let status_str = parts[1].to_lowercase();
            let status = if status_str.starts_with("up") {
                SandboxStatus::Running
            } else {
                SandboxStatus::Stopped
            };

            infos.push(SandboxInfo {
                name: sandbox_name.to_string(),
                status,
                runtime: "docker".to_string(),
                created_at: None,
                ip_address: None,
            });
        }

        Ok(infos)
    }

    async fn snapshot_create(&self, _name: &str, _snap: &str) -> Result<()> {
        todo!("Phase 6: Docker snapshot create via docker commit")
    }

    async fn snapshot_restore(&self, _name: &str, _snap: &str) -> Result<()> {
        todo!("Phase 6: Docker snapshot restore")
    }

    async fn snapshot_list(&self, _name: &str) -> Result<Vec<SnapshotInfo>> {
        todo!("Phase 6: Docker snapshot list")
    }

    async fn upgrade(&self, _name: &str, _tools: &[String]) -> Result<()> {
        todo!("Phase 5: Docker upgrade")
    }

    async fn update_mounts(&self, _name: &str, _mounts: &[super::Mount]) -> Result<()> {
        bail!("Updating mounts is not supported for the Docker runtime")
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
    fn container_name_prefix() {
        assert_eq!(DockerRuntime::container_name("myapp"), "devbox-myapp");
    }

    #[test]
    fn image_name_is_set() {
        assert_eq!(DockerRuntime::image_name(), "devbox-nixos:latest");
    }
}
