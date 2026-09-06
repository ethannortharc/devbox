use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::cmd::{run_cmd, run_interactive, run_ok};
use super::{
    CreateOpts, ExecResult, MountUpdate, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo,
};

/// Docker runtime — explicit restricted mode (weaker isolation, shared kernel).
pub struct DockerRuntime;

impl DockerRuntime {
    /// All Docker containers managed by devbox are prefixed with "devbox-".
    fn container_name(name: &str) -> String {
        format!("devbox-{name}")
    }

    /// Published base used by the supported Docker product shape.
    pub const DEFAULT_UBUNTU_IMAGE: &'static str = "ubuntu:24.04";

    /// Environment variable that overrides the base image.
    pub const IMAGE_ENV: &str = "DEVBOX_DOCKER_IMAGE";

    /// Base image for Docker boxes.
    ///
    /// Overridable so an e2e run (or anyone with their own base image) can
    /// point at something other than the NixOS image, which has to be built
    /// locally and is not on any registry.
    fn image_name(requested: &str) -> Result<(String, bool)> {
        if let Some(custom) = std::env::var(Self::IMAGE_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return Ok((custom, false));
        }
        match requested {
            // The stock Ubuntu image has no long-running default command, so
            // the bool asks create() to supply one after the image name.
            "ubuntu" => Ok((Self::DEFAULT_UBUNTU_IMAGE.to_string(), true)),
            other => bail!(
                "Docker has no published devbox image for '{other}'. Choose `--image ubuntu --writable --bare`, or set {} to a compatible custom image",
                Self::IMAGE_ENV
            ),
        }
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn copy_from(&self, name: &str, guest_path: &str, host_path: &Path) -> Result<()> {
        // `docker cp CONTAINER:SRC_PATH DEST_PATH` — `docker cp --help`.
        let source = format!("{}:{guest_path}", Self::container_name(name));
        run_ok("docker", &["cp", &source, &host_path.to_string_lossy()])
            .await
            .with_context(|| format!("copy {guest_path} out of box '{name}'"))?;
        Ok(())
    }

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
        let (image, needs_keepalive) = Self::image_name(&opts.image)?;

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
            // Without this the egress posture cannot be applied at all.
            //
            // A container gets no `CAP_NET_ADMIN`, so `nft` inside one fails
            // with EPERM — and `devbox policy set` saved the posture first and
            // discovered that second, leaving the box recorded as `isolated`
            // and running wide open. Docker is a documented runtime, so the
            // choice is to grant the capability or to admit the posture cannot
            // be enforced there; granting it makes the documentation true.
            //
            // Scoped to the container's own network namespace, which is the
            // one the rules are for. It does mean a process inside the box can
            // also tear those rules down — but that is already true of every
            // other runtime, where the box's user has passwordless sudo. The
            // posture is a guard rail for what runs in the box, not a cage
            // around someone actively trying to leave it.
            "--cap-add=NET_ADMIN".to_string(),
            // The observability agent's DNS/TLS source opens an AF_PACKET
            // socket. Without NET_RAW every Docker box silently degrades to
            // process-only capture and domain allowlists cannot learn the
            // resolver's addresses after a reboot.
            "--cap-add=NET_RAW".to_string(),
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
        args.push(image);
        if needs_keepalive {
            args.push("sleep".to_string());
            args.push("infinity".to_string());
        }

        println!("Creating Docker container '{container}'...");
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_ok("docker", &args_ref).await?;

        Ok(SandboxInfo {
            name: opts.name.clone(),
            status: SandboxStatus::Running,
            runtime: "docker".to_string(),
            created_at: Some(super::now_rfc3339()),
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

    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult> {
        let container = Self::container_name(name);

        if interactive {
            // Only CLI callers request an inherited interactive terminal.
            // Web provisioning uses `Runtime::argv(..., false)` with piped
            // stdio so it cannot seize the terminal that launched the server.
            let mut args = vec!["exec", "-it", &container];
            args.extend_from_slice(cmd);
            run_interactive("docker", &args).await
        } else {
            let mut args = vec!["exec", &container];
            args.extend_from_slice(cmd);
            run_cmd("docker", &args).await
        }
    }

    fn argv(&self, name: &str, cmd: &[&str], interactive: bool) -> Vec<String> {
        let mut argv = vec!["docker".to_string(), "exec".to_string()];
        if interactive {
            argv.push("-it".to_string());
        } else {
            // Non-interactive callers may still own a protocol on stdin. In
            // particular, Docker Desktop boxes carry observability over the
            // exec stream because their Linux VM cannot connect to a macOS
            // Unix socket exposed through virtiofs.
            argv.push("-i".to_string());
        }
        argv.push(Self::container_name(name));
        argv.extend(cmd.iter().map(|s| s.to_string()));
        argv
    }

    /// **Unverified on the machine this was built on** — no Docker daemon is
    /// running here, so this follows §6.6 rather than a measurement.
    ///
    /// `host.docker.internal` is resolvable inside a container on Docker
    /// Desktop but not on plain Linux Docker, where the bridge gateway is the
    /// address that works; both are offered and the probe picks whichever
    /// answers. §6.6 lists no fallback for Docker and none is invented here.
    async fn host_reach(&self, name: &str, port: u16) -> Result<crate::broker::reach::HostReach> {
        let mut candidates = vec!["host.docker.internal".to_string()];
        if let Some(gateway) = crate::broker::reach::default_gateway(self, name).await
            && !candidates.contains(&gateway)
        {
            candidates.push(gateway);
        }
        crate::broker::reach::probe(self, name, port, &candidates, "docker host gateway").await
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
            &[
                "container",
                "inspect",
                "--format",
                "{{.State.Status}}",
                &container,
            ],
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
        bail!(
            "Snapshots are not supported by the Docker runtime. Use Lima, Incus, or Multipass for VM snapshots."
        )
    }

    async fn snapshot_restore(&self, _name: &str, _snap: &str) -> Result<()> {
        bail!(
            "Snapshots are not supported by the Docker runtime. Use Lima, Incus, or Multipass for VM snapshots."
        )
    }

    async fn snapshot_list(&self, _name: &str) -> Result<Vec<SnapshotInfo>> {
        bail!(
            "Snapshots are not supported by the Docker runtime. Use Lima, Incus, or Multipass for VM snapshots."
        )
    }

    async fn upgrade(&self, _name: &str, _tools: &[String]) -> Result<()> {
        bail!("Runtime-level upgrades are not supported by Docker")
    }

    async fn update_mounts(&self, _name: &str, _mounts: &[super::Mount]) -> Result<MountUpdate> {
        bail!("Updating mounts is not supported for the Docker runtime")
    }

    async fn rollback_mounts(&self, _name: &str, _update: &MountUpdate) -> Result<()> {
        bail!("Mount rollback is not supported for the Docker runtime")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_prefix() {
        assert_eq!(DockerRuntime::container_name("myapp"), "devbox-myapp");
    }

    #[test]
    fn project_mount_switches_are_rejected_before_runtime_mutation() {
        assert!(!DockerRuntime.supports_mount_updates());
    }

    #[test]
    fn published_ubuntu_image_is_used_without_an_override() {
        if std::env::var_os(DockerRuntime::IMAGE_ENV).is_none() {
            assert_eq!(
                DockerRuntime::image_name("ubuntu").unwrap(),
                ("ubuntu:24.04".to_string(), true)
            );
        }
    }

    #[tokio::test]
    async fn unsupported_snapshots_return_errors_instead_of_panicking() {
        let runtime = DockerRuntime;
        assert!(runtime.snapshot_create("box", "snap").await.is_err());
        assert!(runtime.snapshot_restore("box", "snap").await.is_err());
        assert!(runtime.snapshot_list("box").await.is_err());
    }

    #[test]
    fn argv_allocates_a_tty_only_when_interactive() {
        assert_eq!(
            DockerRuntime.argv("myapp", &["zsh", "-l"], true),
            vec!["docker", "exec", "-it", "devbox-myapp", "zsh", "-l"]
        );
        assert_eq!(
            DockerRuntime.argv("myapp", &["true"], false),
            vec!["docker", "exec", "-i", "devbox-myapp", "true"]
        );
    }
}
