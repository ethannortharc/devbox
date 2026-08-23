use std::collections::BTreeMap;

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
            _ => "images:nixos/24.11",
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
        let mut launch_args = vec!["launch", image, &vm, "--vm"];

        let cpu_str;
        if opts.cpu > 0 {
            cpu_str = format!("limits.cpu={}", opts.cpu);
            launch_args.push("-c");
            launch_args.push(&cpu_str);
        }

        let mem_str;
        if !opts.memory.is_empty() {
            mem_str = format!("limits.memory={}", opts.memory);
            launch_args.push("-c");
            launch_args.push(&mem_str);
        }

        run_ok("incus", &launch_args).await?;

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
