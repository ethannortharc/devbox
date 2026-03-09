use anyhow::Result;
use clap::Args;

use crate::runtime::detect::detect_runtime;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct DoctorArgs {}

pub async fn run(_args: DoctorArgs, manager: &SandboxManager) -> Result<()> {
    println!("devbox doctor\n");

    let os = std::env::consts::OS;
    let mut has_any_runtime = false;

    // Platform-appropriate runtime checks
    println!("Runtime availability:");

    if os == "linux" {
        let found = check_binary_with_install(
            "  Incus",
            "incus",
            "sudo apt install incus  # or: snap install incus",
        );
        has_any_runtime |= found;
    }

    if os == "macos" || os == "linux" {
        let found = check_binary_with_install(
            "  Lima",
            "limactl",
            if os == "macos" {
                "brew install lima"
            } else {
                "brew install lima  # or: https://lima-vm.io/docs/installation/"
            },
        );
        if found {
            has_any_runtime = true;
        } else if !has_any_runtime {
            let found = check_binary_with_install(
                "  Multipass",
                "multipass",
                if os == "macos" {
                    "brew install multipass"
                } else {
                    "sudo snap install multipass"
                },
            );
            has_any_runtime |= found;
        }
    }

    if !has_any_runtime {
        let found = check_binary_with_install(
            "  Docker",
            "docker",
            if os == "macos" {
                "brew install --cask docker  # or: https://docker.com/get-started"
            } else {
                "sudo apt install docker.io  # or: https://docker.com/get-started"
            },
        );
        has_any_runtime |= found;
    }

    if !has_any_runtime {
        println!("\n  \x1b[31mNo runtime found!\x1b[0m Install at least one:");
        if os == "macos" {
            println!("    brew install lima          # Recommended for macOS");
        } else {
            println!("    sudo apt install incus     # Recommended for Linux");
        }
    }

    // Auto-detected runtime
    print!("\nAuto-detected runtime: ");
    match detect_runtime() {
        Ok(rt) => println!("{} (priority {})", rt.name(), rt.priority()),
        Err(e) => println!("NONE — {e}"),
    }

    // Global config
    println!("\nGlobal config:");
    match manager.load_global_config() {
        Ok(config) => {
            println!("  Runtime:  {}", config.default.runtime);
            println!("  Layout:   {}", config.default.layout);
            if config.default.tools.is_empty() {
                println!("  Tools:    (none)");
            } else {
                println!("  Tools:    {}", config.default.tools.join(", "));
            }
        }
        Err(_) => println!("  (using defaults)"),
    }

    // State directory
    println!("\nState directory: {}", manager.state_dir.display());
    let sandboxes = manager.list_sandboxes().unwrap_or_default();
    println!("Sandboxes registered: {}", sandboxes.len());

    // Project config
    let cwd = std::env::current_dir().unwrap_or_default();
    let devbox_toml = cwd.join("devbox.toml");
    if devbox_toml.exists() {
        println!("\nProject config: {} (found)", devbox_toml.display());
    } else {
        println!("\nProject config: not found (run `devbox init` to create)");
    }

    // Supporting tools
    println!("\nSupporting tools:");
    check_binary_with_install(
        "  Zellij",
        "zellij",
        if os == "macos" {
            "brew install zellij"
        } else {
            "cargo install zellij  # or: https://zellij.dev/documentation/installation"
        },
    );
    check_binary_with_install(
        "  Nix",
        "nix",
        "curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh",
    );

    println!("\nAll checks complete.");
    Ok(())
}

/// Check if a binary is available. If missing, print install instructions.
/// Returns true if found.
fn check_binary_with_install(label: &str, name: &str, install_hint: &str) -> bool {
    if which::which(name).is_ok() {
        println!("{label}: \x1b[32minstalled\x1b[0m");
        true
    } else {
        println!("{label}: \x1b[31mnot found\x1b[0m");
        println!("    Install: {install_hint}");
        false
    }
}
