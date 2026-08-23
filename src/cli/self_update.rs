use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct SelfUpdateArgs {
    /// Check for updates without installing
    #[arg(long)]
    pub check: bool,

    /// Specific version to install (e.g., 0.2.0)
    #[arg(long)]
    pub version: Option<String>,
}

const REPO: &str = "ethannortharc/devbox";

pub async fn run(args: SelfUpdateArgs, _manager: &SandboxManager) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: {current}");

    if args.check {
        return check_latest(current).await;
    }

    let target_version = match &args.version {
        Some(v) => v.clone(),
        None => {
            let latest = fetch_latest_version().await?;
            if latest == current {
                println!("Already up to date.");
                return Ok(());
            }
            println!("New version available: {latest}");
            latest
        }
    };

    install_version(&target_version).await
}

async fn check_latest(current: &str) -> Result<()> {
    let latest = fetch_latest_version().await?;
    if latest == current {
        println!("Up to date (latest: {latest})");
    } else {
        println!("Update available: {current} -> {latest}");
        println!("Run `devbox self-update` to install.");
    }
    Ok(())
}

async fn fetch_latest_version() -> Result<String> {
    // Use gh CLI if available, otherwise curl
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{REPO}/releases/latest"),
            "--jq",
            ".tag_name",
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok(tag.trim_start_matches('v').to_string())
        }
        _ => {
            // Fallback to curl
            let output = tokio::process::Command::new("curl")
                .args([
                    "-fsSL",
                    &format!("https://api.github.com/repos/{REPO}/releases/latest"),
                ])
                .output()
                .await?;

            if !output.status.success() {
                bail!("Failed to fetch latest release. Check your internet connection.");
            }

            let body = String::from_utf8_lossy(&output.stdout);
            // Parse tag_name from JSON (minimal parsing to avoid extra deps)
            let tag = body
                .split("\"tag_name\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1))
                .ok_or_else(|| anyhow::anyhow!("Failed to parse release info"))?;

            Ok(tag.trim_start_matches('v').to_string())
        }
    }
}

async fn install_version(version: &str) -> Result<()> {
    let asset = detect_target()?;
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/{asset}");

    println!("Downloading {asset}...");

    // The temporary must be beside the installed binary. Rename is atomic and
    // can replace an executing file on Unix; copying bytes over the running
    // inode fails with ETXTBSY on Linux and risks a partial executable if the
    // process is interrupted.
    let current_exe = std::env::current_exe()?;
    let install_dir = current_exe
        .parent()
        .context("the current devbox executable has no parent directory")?;
    let temp = tempfile::Builder::new()
        .prefix(".devbox-update-")
        .tempfile_in(install_dir)
        .with_context(|| {
            format!(
                "cannot create an update beside {}; check directory permissions",
                current_exe.display()
            )
        })?;

    let status = tokio::process::Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(temp.path())
        .arg(&url)
        .status()
        .await?;

    if !status.success() {
        bail!("Failed to download release {version} asset {asset}");
    }

    if temp.as_file().metadata()?.len() == 0 {
        bail!("Downloaded release {version} asset {asset} is empty");
    }
    temp.as_file().sync_all()?;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))?;
    temp.persist(&current_exe).map_err(|error| {
        anyhow::anyhow!(
            "failed to atomically replace {}: {}",
            current_exe.display(),
            error.error
        )
    })?;

    println!("Updated to version {version}");
    Ok(())
}

fn detect_target() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let target = match (os, arch) {
        ("macos", "aarch64") => "devbox-darwin-arm64",
        ("linux", "x86_64") => "devbox-linux-amd64",
        _ => bail!("Unsupported platform: {os}/{arch}"),
    };

    Ok(target.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_target_works() {
        let target = detect_target().unwrap();
        assert!(matches!(
            target.as_str(),
            "devbox-darwin-arm64" | "devbox-linux-amd64"
        ));
    }

    #[test]
    fn updater_and_release_workflow_name_the_same_assets() {
        let release = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.github/workflows/release.yml"
        ));
        for asset in ["devbox-darwin-arm64", "devbox-linux-amd64"] {
            assert!(
                release.contains(asset),
                "release workflow does not publish {asset}"
            );
            assert!(!asset.ends_with(".tar.gz"));
        }
        assert_eq!(REPO, "ethannortharc/devbox");
    }
}
