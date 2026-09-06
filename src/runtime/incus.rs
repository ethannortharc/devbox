use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::cmd::{run_cmd, run_interactive, run_ok};
use super::{
    CreateOpts, ExecResult, MountUpdate, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo,
};

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

    fn is_managed_mount_device(name: &str) -> bool {
        name.strip_prefix("mount")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
    }

    fn parse_managed_mount_devices(
        output: &str,
    ) -> Result<Vec<(String, BTreeMap<String, String>)>> {
        let value: serde_json::Value =
            serde_json::from_str(output).context("parse Incus instance configuration")?;
        // `incus query` normally prints metadata directly. Accept an API
        // envelope as well so this stays compatible with older clients.
        let instance = value.get("metadata").unwrap_or(&value);
        let devices = instance
            .get("devices")
            .and_then(serde_json::Value::as_object)
            .context("Incus instance configuration has no devices object")?;

        let mut managed = Vec::new();
        for (name, raw) in devices {
            if !Self::is_managed_mount_device(name) {
                continue;
            }
            let object = raw
                .as_object()
                .with_context(|| format!("Incus device '{name}' is not an object"))?;
            let mut properties = BTreeMap::new();
            for (key, value) in object {
                let value = value.as_str().with_context(|| {
                    format!("Incus device '{name}' property '{key}' is not a string")
                })?;
                properties.insert(key.clone(), value.to_string());
            }
            if properties.get("type").map(String::as_str) != Some("disk") {
                bail!(
                    "Incus device '{name}' collides with devbox's managed mount names but is not a disk; rename it before using `devbox use`"
                );
            }
            managed.push((name.clone(), properties));
        }
        managed.sort_by_key(|(name, _)| {
            name.trim_start_matches("mount")
                .parse::<usize>()
                .unwrap_or(usize::MAX)
        });
        Ok(managed)
    }

    async fn managed_mount_devices(vm: &str) -> Result<Vec<(String, BTreeMap<String, String>)>> {
        let endpoint = format!("/1.0/instances/{}", crate::web::encode_segment(vm));
        let result = run_cmd("incus", &["query", &endpoint]).await?;
        if result.exit_code != 0 {
            bail!(
                "cannot read Incus devices for '{vm}': {}",
                result.stderr.trim()
            );
        }
        Self::parse_managed_mount_devices(&result.stdout)
    }

    fn desired_mount_devices(mounts: &[super::Mount]) -> Vec<(String, BTreeMap<String, String>)> {
        mounts
            .iter()
            .enumerate()
            .map(|(index, mount)| {
                let mut properties = BTreeMap::from([
                    ("type".to_string(), "disk".to_string()),
                    ("source".to_string(), mount.host_path.display().to_string()),
                    ("path".to_string(), mount.container_path.clone()),
                ]);
                if mount.read_only {
                    properties.insert("readonly".to_string(), "true".to_string());
                }
                (format!("mount{index}"), properties)
            })
            .collect()
    }

    async fn add_mount_device(
        vm: &str,
        name: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<()> {
        let kind = properties
            .get("type")
            .with_context(|| format!("Incus device '{name}' has no type"))?;
        let mut owned = vec![
            "config".to_string(),
            "device".to_string(),
            "add".to_string(),
            vm.to_string(),
            name.to_string(),
            kind.clone(),
        ];
        owned.extend(
            properties
                .iter()
                .filter(|(key, _)| key.as_str() != "type")
                .map(|(key, value)| format!("{key}={value}")),
        );
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        run_ok("incus", &args).await?;
        Ok(())
    }

    async fn replace_managed_mount_devices(
        vm: &str,
        target: &[(String, BTreeMap<String, String>)],
    ) -> Result<()> {
        for (name, _) in Self::managed_mount_devices(vm).await? {
            run_ok("incus", &["config", "device", "remove", vm, &name]).await?;
        }
        for (name, properties) in target {
            Self::add_mount_device(vm, name, properties).await?;
        }
        Ok(())
    }

    async fn ensure_stopped(&self, name: &str) -> Result<()> {
        match <Self as Runtime>::status(self, name).await? {
            SandboxStatus::Running | SandboxStatus::Unreachable(_) => {
                let vm = Self::vm_name(name);
                run_ok("incus", &["stop", &vm, "--force"]).await?;
                Ok(())
            }
            SandboxStatus::Stopped => Ok(()),
            SandboxStatus::NotFound => bail!("Incus VM '{}' does not exist", Self::vm_name(name)),
            SandboxStatus::Unknown(detail) => bail!(
                "cannot safely change Incus mounts for '{}': {detail}",
                Self::vm_name(name)
            ),
        }
    }

    async fn restore_mount_devices(
        &self,
        name: &str,
        original: &[(String, BTreeMap<String, String>)],
        restart: bool,
    ) -> Result<()> {
        self.ensure_stopped(name).await?;
        Self::replace_managed_mount_devices(&Self::vm_name(name), original).await?;
        if restart {
            <Self as Runtime>::start(self, name).await?;
        }
        Ok(())
    }

    /// Detect the UID of the first non-root user in the VM.
    /// Filters to users with home under /home/ to exclude NixOS nixbld* users
    /// (UID 30001+ with home /var/empty).
    async fn detect_vm_uid(vm: &str) -> Option<String> {
        let result = run_cmd(
            "incus",
            &["exec", vm, "--", "bash", "-lc",
              "awk -F: '$3 >= 1000 && $3 < 65534 && $6 ~ /^\\/home\\// { print $3; exit }' /etc/passwd"],
        ).await.ok()?;
        let uid = result.stdout.trim().to_string();
        if uid.is_empty() { None } else { Some(uid) }
    }

    /// Detect the HOME directory for a given UID in the VM.
    async fn detect_vm_home(vm: &str, uid: &str) -> String {
        // Use bash -lc to get login shell PATH (getent is in /run/current-system/sw/bin/ on NixOS)
        let result = run_cmd(
            "incus",
            &[
                "exec",
                vm,
                "--",
                "bash",
                "-lc",
                &format!("getent passwd {uid} | cut -d: -f6"),
            ],
        )
        .await;
        match result {
            Ok(r) if !r.stdout.trim().is_empty() => {
                format!("HOME={}", r.stdout.trim())
            }
            _ => "HOME=/home/dev".to_string(),
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
        bail!(
            "VM agent did not become ready within 120 seconds. The VM may still be booting — try `devbox shell` in a minute."
        )
    }
}

#[async_trait]
impl Runtime for IncusRuntime {
    async fn copy_from(&self, name: &str, guest_path: &str, host_path: &Path) -> Result<()> {
        // `incus file pull [<remote>:]<instance>/<path> <target path>`. Note
        // the separator: Incus joins the instance and the path with a **slash**
        // where Lima, Docker and Multipass all use a colon — a colon here would
        // be read as a remote name. Checked against the Incus manpage for
        // `incus file pull` (linuxcontainers.org/incus/docs/main, `main`), whose
        // own example is `incus file pull foo/etc/hosts .`.
        //
        // Not verified on a live Incus host: this machine has none.
        let source = format!("{}{guest_path}", Self::vm_name(name));
        run_ok(
            "incus",
            &["file", "pull", &source, &host_path.to_string_lossy()],
        )
        .await
        .with_context(|| format!("copy {guest_path} out of box '{name}'"))?;
        Ok(())
    }

    fn name(&self) -> &str {
        "incus"
    }

    fn is_available(&self) -> bool {
        which::which("incus").is_ok()
    }

    fn priority(&self) -> u32 {
        30
    }

    fn exec_runs_as_root(&self) -> bool {
        true
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

        // Determine which image to launch from: cached or base
        let image = if let Some(cached) = &opts.cached_image {
            println!("Launching from cached image '{cached}'...");
            cached.clone()
        } else {
            Self::ensure_image(&opts.image).await?;
            Self::image_alias(&opts.image).to_string()
        };

        // Launch the VM
        println!("Creating Incus VM '{vm}'...");
        let mut launch_args = vec![
            "launch",
            &image,
            &vm,
            "--vm",
            "-c",
            "security.secureboot=false",
        ];

        let cpu_str;
        if opts.cpu > 0 {
            cpu_str = format!("limits.cpu={}", opts.cpu);
            launch_args.push("-c");
            launch_args.push(&cpu_str);
        }

        // Default to 4GiB memory for Incus VMs — NixOS rebuild needs 2-4GB
        // for evaluating the full module system. The default Incus 1GB is too little.
        let memory = if opts.memory.is_empty() {
            "4GiB"
        } else {
            &opts.memory
        };
        let mem_str = format!("limits.memory={memory}");
        launch_args.push("-c");
        launch_args.push(&mem_str);

        run_ok("incus", &launch_args).await?;

        // Expand disk only for base images (cached images already have 20GB)
        if opts.cached_image.is_none() {
            let _ = run_ok(
                "incus",
                &["config", "device", "override", &vm, "root", "size=20GiB"],
            )
            .await;
        }

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
            created_at: Some(super::now_rfc3339()),
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

    fn argv(&self, name: &str, cmd: &[&str], interactive: bool) -> Vec<String> {
        let mut argv = vec!["incus".to_string(), "exec".to_string(), Self::vm_name(name)];
        if !interactive {
            // Without a tty incus still wants to know not to allocate one.
            argv.push("--force-noninteractive".to_string());
        }
        argv.push("--".to_string());
        argv.extend(cmd.iter().map(|s| s.to_string()));
        argv
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

    /// **Unverified on the machine this was built on** — no Incus is
    /// installed here, so unlike the Lima implementation this one was written
    /// from §6.6 and not measured.
    ///
    /// Incus containers sit on a managed bridge (`incusbr0` by default) whose
    /// host end holds the address the guest sees as its default gateway, so
    /// the gateway is the first and only candidate. It is still *probed*
    /// rather than assumed: if the bridge is configured without a host
    /// address, or egress policy blocks it, the probe fails and the broker
    /// simply injects nothing rather than handing the box a dead address.
    ///
    /// The §6.6 fallback — a reverse tunnel over `incus exec` and socat — is
    /// not implemented; a box that cannot reach the gateway gets no broker
    /// variables and `doctor` says so.
    async fn host_reach(&self, name: &str, port: u16) -> Result<crate::broker::reach::HostReach> {
        let mut candidates = Vec::new();
        if let Some(gateway) = crate::broker::reach::default_gateway(self, name).await {
            candidates.push(gateway);
        }
        crate::broker::reach::probe(self, name, port, &candidates, "incus host bridge").await
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
        bail!("Runtime-level upgrades are not supported by Incus")
    }

    fn supports_mount_updates(&self) -> bool {
        true
    }

    async fn cached_image(&self, cache_key: &str) -> Option<String> {
        let alias = format!("devbox-cache-{cache_key}");
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
        .await
        .ok()?;
        if result.exit_code == 0
            && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&result.stdout)
            && !arr.is_empty()
        {
            return Some(alias);
        }
        None
    }

    async fn cache_image(&self, name: &str, cache_key: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        let alias = format!("devbox-cache-{cache_key}");

        println!("Caching provisioned image as '{alias}'...");

        // Stop VM before publishing (required by incus publish)
        let _ = run_cmd("incus", &["stop", &vm]).await;

        // Publish the VM as a reusable image
        run_ok("incus", &["publish", &vm, "--alias", &alias]).await?;

        // Restart the VM
        run_ok("incus", &["start", &vm]).await?;
        Self::wait_for_agent(&vm).await?;

        println!("Image cached successfully.");
        Ok(())
    }

    async fn update_mounts(&self, name: &str, mounts: &[super::Mount]) -> Result<MountUpdate> {
        let vm = Self::vm_name(name);

        // Query and validate both the old and desired device sets before the
        // first stop/remove. Errors here leave a running box untouched.
        let original = Self::managed_mount_devices(&vm).await?;
        let desired = Self::desired_mount_devices(mounts);
        let was_running = match self.status(name).await? {
            SandboxStatus::Running => true,
            SandboxStatus::Stopped => false,
            SandboxStatus::NotFound => bail!("Incus VM '{vm}' does not exist"),
            SandboxStatus::Unreachable(detail) | SandboxStatus::Unknown(detail) => {
                bail!("cannot safely update mounts for Incus VM '{vm}': {detail}")
            }
        };

        if was_running
            && let Err(stop_error) = self.stop(name).await
            && let Err(force_error) = self.ensure_stopped(name).await
        {
            bail!(
                "Incus VM '{vm}' could not be stopped before changing mounts: {stop_error:#}; the force-stop recovery also failed: {force_error:#}. No mount device was changed"
            );
        }

        if let Err(update_error) = Self::replace_managed_mount_devices(&vm, &desired).await {
            return match self
                .restore_mount_devices(name, &original, was_running)
                .await
            {
                Ok(()) => Err(update_error).context(format!(
                    "updating Incus mounts failed; VM '{vm}' was restored to its original mounts and running state"
                )),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "updating Incus mounts failed: {update_error:#}; restoring the original mounts also failed: {rollback_error:#}"
                )),
            };
        }

        // Provisioning and policy restoration happen before the final attach,
        // so the switched guest must be running even if it began stopped.
        if let Err(start_error) = self.start(name).await {
            return match self
                .restore_mount_devices(name, &original, was_running)
                .await
            {
                Ok(()) => Err(start_error).context(format!(
                    "Incus could not start with the new mounts; VM '{vm}' was restored to its original mounts and prior running state"
                )),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "Incus could not start with the new mounts: {start_error:#}; restoring the original mounts also failed: {rollback_error:#}"
                )),
            };
        }

        Ok(MountUpdate::Incus { original })
    }

    async fn rollback_mounts(&self, name: &str, update: &MountUpdate) -> Result<()> {
        let original = match update {
            MountUpdate::Incus { original, .. } => original,
            MountUpdate::Lima { .. } => {
                bail!("received a Lima mount rollback token in the Incus runtime")
            }
        };

        // State persistence failed, so the old state remains authoritative.
        // Restore those mounts but deliberately leave the VM stopped: no guest
        // may continue on a project switch the control plane did not commit.
        self.restore_mount_devices(name, original, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn vm_name_prefix() {
        assert_eq!(IncusRuntime::vm_name("myapp"), "devbox-myapp");
    }

    #[test]
    fn argv_targets_the_instance() {
        assert_eq!(
            IncusRuntime.argv("myapp", &["bash"], true),
            vec!["incus", "exec", "devbox-myapp", "--", "bash"]
        );
        assert!(
            IncusRuntime
                .argv("myapp", &["true"], false)
                .contains(&"--force-noninteractive".to_string())
        );
    }

    #[test]
    fn mount_updates_are_advertised_for_the_primary_linux_runtime() {
        assert!(IncusRuntime.supports_mount_updates());
    }

    #[test]
    fn managed_mount_snapshot_is_exact_and_ignores_unowned_devices() {
        let devices = IncusRuntime::parse_managed_mount_devices(
            r#"{
                "devices": {
                    "root": {"type":"disk","path":"/","pool":"default"},
                    "eth0": {"type":"nic","network":"incusbr0"},
                    "mount10": {"type":"disk","source":"/ten","path":"/workspace/ten"},
                    "mount0": {"type":"disk","source":"/old","path":"/workspace","readonly":"true"},
                    "mount-note": {"type":"disk","source":"/keep","path":"/keep"}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].0, "mount0");
        assert_eq!(devices[0].1["source"], "/old");
        assert_eq!(devices[0].1["readonly"], "true");
        assert_eq!(devices[1].0, "mount10");
    }

    #[test]
    fn desired_mount_snapshot_preserves_paths_and_read_only_mode() {
        let devices = IncusRuntime::desired_mount_devices(&[
            super::super::Mount {
                host_path: PathBuf::from("/host/project"),
                container_path: "/mnt/host".to_string(),
                read_only: true,
            },
            super::super::Mount {
                host_path: PathBuf::from("/host/socket"),
                container_path: "/run/devbox-host".to_string(),
                read_only: false,
            },
        ]);

        assert_eq!(devices[0].0, "mount0");
        assert_eq!(devices[0].1["type"], "disk");
        assert_eq!(devices[0].1["readonly"], "true");
        assert!(!devices[1].1.contains_key("readonly"));
    }

    #[test]
    fn a_non_disk_collision_is_rejected_before_mutation() {
        let error = IncusRuntime::parse_managed_mount_devices(
            r#"{"devices":{"mount0":{"type":"nic","network":"incusbr0"}}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("collides"));
    }
}
