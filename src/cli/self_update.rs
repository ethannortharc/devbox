use anyhow::{Result, bail};
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

const REPO: &str = "northarc/devbox";
const BINARY_NAME: &str = "devbox";

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
                    "-sL",
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
    let target = detect_target()?;
    let asset = format!("{BINARY_NAME}-{target}.tar.gz");
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/{asset}");

    println!("Downloading {asset}...");

    // Download to temp location
    let temp_dir = std::env::temp_dir().join("devbox-update");
    std::fs::create_dir_all(&temp_dir)?;
    let archive_path = temp_dir.join(&asset);

    let status = tokio::process::Command::new("curl")
        .args(["-sL", "-o", archive_path.to_str().unwrap(), &url])
        .status()
        .await?;

    if !status.success() {
        bail!("Failed to download release {version} for {target}");
    }

    // Extract
    let status = tokio::process::Command::new("tar")
        .args([
            "xzf",
            archive_path.to_str().unwrap(),
            "-C",
            temp_dir.to_str().unwrap(),
        ])
        .status()
        .await?;

    if !status.success() {
        bail!("Failed to extract archive");
    }

    // Find current binary location and replace
    let current_exe = std::env::current_exe()?;
    let new_binary = temp_dir.join(BINARY_NAME);

    if !new_binary.exists() {
        bail!(
            "Binary not found in archive. Expected: {}",
            new_binary.display()
        );
    }

    // Replace in place
    std::fs::copy(&new_binary, &current_exe)?;

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!("Updated to version {version}");
    Ok(())
}

fn detect_target() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let target = match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
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
        assert!(!target.is_empty());
        // Should match current platform
        assert!(
            target.contains(std::env::consts::OS.replace("macos", "apple").as_str())
                || target.contains("linux")
        );
    }
}
