use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::cmd::{run_cmd, run_interactive, run_ok};
use super::{
    CreateOpts, ExecResult, MountUpdate, Runtime, SandboxInfo, SandboxStatus, SnapshotInfo,
};

/// Multipass runtime — secondary on macOS (Canonical's VM manager).
pub struct MultipassRuntime;

impl MultipassRuntime {
    /// All Multipass VMs managed by devbox are prefixed with "devbox-".
    fn vm_name(name: &str) -> String {
        format!("devbox-{name}")
    }
}

#[async_trait]
impl Runtime for MultipassRuntime {
    async fn copy_from(&self, name: &str, guest_path: &str, host_path: &Path) -> Result<()> {
        // `multipass transfer <instance>:<path> <destination>` — the Multipass
        // CLI reference for `transfer`, whose example is
        // `multipass transfer ample-pigeon:remote_file.txt .`.
        //
        // Not verified on a live Multipass host: this machine has none.
        let source = format!("{}:{guest_path}", Self::vm_name(name));
        run_ok(
            "multipass",
            &["transfer", &source, &host_path.to_string_lossy()],
        )
        .await
        .with_context(|| format!("copy {guest_path} out of box '{name}'"))?;
        Ok(())
    }

    fn name(&self) -> &str {
        "multipass"
    }

    fn is_available(&self) -> bool {
        which::which("multipass").is_ok()
    }

    fn priority(&self) -> u32 {
        15
    }

    async fn create(&self, opts: &CreateOpts) -> Result<SandboxInfo> {
        let vm = Self::vm_name(&opts.name);

        // Check if already exists
        let result = run_cmd("multipass", &["info", &vm]).await?;
        if result.exit_code == 0 {
            bail!(
                "Multipass VM '{}' already exists. Use `devbox destroy {}` first.",
                vm,
                opts.name
            );
        }

        // Build launch args
        let mut args = vec!["launch".to_string(), "--name".to_string(), vm.clone()];

        if opts.cpu > 0 {
            args.push("--cpus".to_string());
            args.push(opts.cpu.to_string());
        }
        if !opts.memory.is_empty() {
            args.push("--memory".to_string());
            args.push(opts.memory.clone());
        }

        println!("Creating Multipass VM '{vm}'...");
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_ok("multipass", &args_ref).await?;

        // Add mounts
        for m in &opts.mounts {
            let host = m.host_path.display().to_string();
            let mount_target = format!("{vm}:{}", m.container_path);
            let mount_args = vec!["mount", &host, &mount_target, "--type", "native"];
            // Ignore mount errors (best-effort)
            let _ = run_cmd("multipass", &mount_args).await;
        }

        Ok(SandboxInfo {
            name: opts.name.clone(),
            status: SandboxStatus::Running,
            runtime: "multipass".to_string(),
            created_at: Some(super::now_rfc3339()),
            ip_address: None,
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("multipass", &["start", &vm]).await?;
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("multipass", &["stop", &vm]).await?;
        Ok(())
    }

    async fn exec_cmd(&self, name: &str, cmd: &[&str], interactive: bool) -> Result<ExecResult> {
        let vm = Self::vm_name(name);

        if interactive {
            // Multipass shell for interactive
            run_interactive("multipass", &["shell", &vm]).await
        } else {
            let mut args = vec!["exec", &vm, "--"];
            args.extend_from_slice(cmd);
            run_cmd("multipass", &args).await
        }
    }

    fn argv(&self, name: &str, cmd: &[&str], interactive: bool) -> Vec<String> {
        // `multipass exec` does not allocate a tty; `multipass shell` does,
        // but only for a login shell. An interactive request therefore routes
        // through `shell` and ignores the requested argv.
        if interactive {
            return vec![
                "multipass".to_string(),
                "shell".to_string(),
                Self::vm_name(name),
            ];
        }
        let mut argv = vec![
            "multipass".to_string(),
            "exec".to_string(),
            Self::vm_name(name),
            "--".to_string(),
        ];
        argv.extend(cmd.iter().map(|s| s.to_string()));
        argv
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        // Stop first (ignore errors)
        let _ = run_cmd("multipass", &["stop", &vm]).await;
        run_ok("multipass", &["delete", &vm, "--purge"]).await?;
        Ok(())
    }

    async fn status(&self, name: &str) -> Result<SandboxStatus> {
        let vm = Self::vm_name(name);
        let result = run_cmd("multipass", &["info", &vm, "--format", "json"]).await?;

        if result.exit_code != 0 {
            return Ok(SandboxStatus::NotFound);
        }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result.stdout)
            && let Some(state) = v["info"][&vm]["state"].as_str()
        {
            return Ok(match state {
                "Running" => SandboxStatus::Running,
                "Stopped" => SandboxStatus::Stopped,
                other => SandboxStatus::Unknown(other.to_string()),
            });
        }

        Ok(SandboxStatus::Unknown("parse error".to_string()))
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let result = run_cmd("multipass", &["list", "--format", "json"]).await?;

        if result.exit_code != 0 {
            return Ok(vec![]);
        }

        let mut infos = vec![];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result.stdout)
            && let Some(list) = v["list"].as_array()
        {
            for item in list {
                let name = item["name"].as_str().unwrap_or("").to_string();
                if !name.starts_with("devbox-") {
                    continue;
                }
                let sandbox_name = name.strip_prefix("devbox-").unwrap_or(&name).to_string();
                let state = item["state"].as_str().unwrap_or("");
                let status = match state {
                    "Running" => SandboxStatus::Running,
                    "Stopped" => SandboxStatus::Stopped,
                    other => SandboxStatus::Unknown(other.to_string()),
                };
                let ip = item["ipv4"]
                    .as_array()
                    .and_then(|arr| arr.first().and_then(|v| v.as_str().map(|s| s.to_string())));

                infos.push(SandboxInfo {
                    name: sandbox_name,
                    status,
                    runtime: "multipass".to_string(),
                    created_at: None,
                    ip_address: ip,
                });
            }
        }

        Ok(infos)
    }

    async fn snapshot_create(&self, name: &str, snap: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("multipass", &["snapshot", &vm, "--name", snap]).await?;
        Ok(())
    }

    async fn snapshot_restore(&self, name: &str, snap: &str) -> Result<()> {
        let vm = Self::vm_name(name);
        run_ok("multipass", &["restore", &format!("{vm}.{snap}")]).await?;
        Ok(())
    }

    async fn snapshot_list(&self, name: &str) -> Result<Vec<SnapshotInfo>> {
        let vm = Self::vm_name(name);
        let result = run_cmd("multipass", &["snapshot", "list", &vm, "--format", "json"]).await?;

        if result.exit_code != 0 {
            return Ok(vec![]);
        }

        let mut snaps = vec![];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result.stdout)
            && let Some(snapshots) = v["snapshots"].as_array()
        {
            for s in snapshots {
                snaps.push(SnapshotInfo {
                    name: s["name"].as_str().unwrap_or("").to_string(),
                    created_at: s["created"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        Ok(snaps)
    }

    async fn upgrade(&self, _name: &str, _tools: &[String]) -> Result<()> {
        bail!("Runtime-level upgrades are not supported by Multipass")
    }

    async fn update_mounts(&self, _name: &str, _mounts: &[super::Mount]) -> Result<MountUpdate> {
        bail!("Updating mounts is not supported for the Multipass runtime")
    }

    async fn rollback_mounts(&self, _name: &str, _update: &MountUpdate) -> Result<()> {
        bail!("Mount rollback is not supported for the Multipass runtime")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_name_prefix() {
        assert_eq!(MultipassRuntime::vm_name("myapp"), "devbox-myapp");
    }

    #[test]
    fn project_mount_switches_are_rejected_before_runtime_mutation() {
        assert!(!MultipassRuntime.supports_mount_updates());
    }

    #[test]
    fn argv_uses_shell_because_exec_has_no_tty() {
        assert_eq!(
            MultipassRuntime.argv("myapp", &["zsh", "-l"], true),
            vec!["multipass", "shell", "devbox-myapp"]
        );
        assert_eq!(
            MultipassRuntime.argv("myapp", &["true"], false),
            vec!["multipass", "exec", "devbox-myapp", "--", "true"]
        );
    }
}
