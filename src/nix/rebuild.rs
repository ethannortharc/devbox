use anyhow::{Result, bail};

use crate::runtime::Runtime;

/// Execute `nixos-rebuild switch` inside a sandbox VM.
/// Returns Ok(()) on success, Err with rollback attempt on failure.
pub async fn nixos_rebuild(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    println!("Running nixos-rebuild switch...");

    let result = runtime
        .run_as_root(sandbox_name, "nixos-rebuild switch", false)
        .await?;

    if result.exit_code != 0 {
        eprintln!("nixos-rebuild failed:\n{}", result.stderr.trim());
        eprintln!("Attempting rollback...");

        let rollback = runtime
            .run_as_root(sandbox_name, "nixos-rebuild switch --rollback", false)
            .await;

        match rollback {
            Ok(r) if r.exit_code == 0 => {
                bail!(
                    "nixos-rebuild failed. Rolled back to previous generation.\n{}",
                    result.stderr.trim()
                );
            }
            _ => {
                bail!(
                    "nixos-rebuild failed and rollback also failed.\n{}",
                    result.stderr.trim()
                );
            }
        }
    }

    println!("nixos-rebuild switch completed successfully.");
    Ok(())
}

/// Write the devbox-state.toml to /etc/devbox/ inside the VM.
pub async fn write_state_toml(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    toml_content: &str,
) -> Result<()> {
    let cmd = format!(
        "mkdir -p /etc/devbox && tee /etc/devbox/devbox-state.toml > /dev/null << 'DEVBOX_EOF'\n{toml_content}\nDEVBOX_EOF"
    );
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to write devbox-state.toml: {}",
            result.stderr.trim()
        );
    }

    Ok(())
}

/// Write a Nix file to /etc/devbox/sets/ inside the VM.
pub async fn write_nix_file(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    filename: &str,
    content: &str,
) -> Result<()> {
    let cmd = format!(
        "mkdir -p /etc/devbox/sets && tee /etc/devbox/sets/{filename} > /dev/null << 'DEVBOX_EOF'\n{content}\nDEVBOX_EOF"
    );
    let result = runtime.run_as_root(sandbox_name, &cmd, false).await?;

    if result.exit_code != 0 {
        bail!("Failed to write {filename}: {}", result.stderr.trim());
    }

    Ok(())
}
