use anyhow::{Result, bail};

use crate::runtime::Runtime;

/// The shell command that rebuilds a NixOS box.
///
/// NixOS Lima images ship a flake-based `NIX_PATH` with no `nixos-config`
/// entry, so a bare `nixos-rebuild switch` fails before it evaluates anything.
/// Provisioning has always exported the path; every other rebuild caller has to
/// do the same, which is why it lives here rather than being repeated.
pub const REBUILD_SHELL: &str = "export NIX_PATH=\"nixos-config=/etc/nixos/configuration.nix:$NIX_PATH\" && \
     nixos-rebuild switch";

/// Argv that rebuilds a NixOS box, for callers that spawn the process
/// themselves (the streamed console rebuild).
pub fn rebuild_argv() -> [&'static str; 4] {
    ["sudo", "bash", "-lc", REBUILD_SHELL]
}

/// Execute `nixos-rebuild switch` inside a sandbox VM.
/// Returns Ok(()) on success, Err with rollback attempt on failure.
pub async fn nixos_rebuild(runtime: &dyn Runtime, sandbox_name: &str) -> Result<()> {
    println!("Running nixos-rebuild switch...");

    let result = runtime
        .exec_cmd(sandbox_name, &rebuild_argv(), false)
        .await?;

    if result.exit_code != 0 {
        eprintln!("nixos-rebuild failed:\n{}", result.stderr.trim());
        eprintln!("Attempting rollback...");

        let rollback = runtime
            .exec_cmd(
                sandbox_name,
                &["sudo", "nixos-rebuild", "switch", "--rollback"],
                false,
            )
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
    // Use tee to write the file as root
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "sudo", "bash", "-c",
                &format!(
                    "mkdir -p /etc/devbox && cat > /etc/devbox/devbox-state.toml << 'DEVBOX_EOF'\n{toml_content}\nDEVBOX_EOF"
                ),
            ],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to write devbox-state.toml: {}",
            result.stderr.trim()
        );
    }

    Ok(())
}

/// Path of the composed configuration inside a box.
///
/// The NixOS configuration imports this file; devbox owns it exclusively and
/// regenerates it from the set checklist on every rebuild (§6.3).
pub const DEVBOX_NIX_PATH: &str = "/etc/devbox/devbox.nix";

/// Write the composed `configuration.nix` fragment inside the VM.
pub async fn write_devbox_nix(
    runtime: &dyn Runtime,
    sandbox_name: &str,
    content: &str,
) -> Result<()> {
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "sudo", "bash", "-c",
                &format!(
                    "mkdir -p /etc/devbox && cat > {DEVBOX_NIX_PATH} << 'DEVBOX_EOF'\n{content}\nDEVBOX_EOF"
                ),
            ],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        bail!(
            "Failed to write {DEVBOX_NIX_PATH}: {}",
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
    let result = runtime
        .exec_cmd(
            sandbox_name,
            &[
                "sudo", "bash", "-c",
                &format!(
                    "mkdir -p /etc/devbox/sets && cat > /etc/devbox/sets/{filename} << 'DEVBOX_EOF'\n{content}\nDEVBOX_EOF"
                ),
            ],
            false,
        )
        .await?;

    if result.exit_code != 0 {
        bail!("Failed to write {filename}: {}", result.stderr.trim());
    }

    Ok(())
}
