use anyhow::Result;
use clap::Args;

use crate::runtime::detect::detect_runtime;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct DoctorArgs {}

pub async fn run(_args: DoctorArgs, manager: &SandboxManager) -> Result<()> {
    println!("devbox doctor\n");

    // Check runtimes
    println!("Runtime availability:");
    check_binary("  Incus", "incus");
    check_binary("  Lima", "limactl");
    check_binary("  Multipass", "multipass");
    check_binary("  Docker", "docker");

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
    check_binary("  Zellij", "zellij");
    check_binary("  Nix", "nix");

    println!("\nAll checks complete.");
    Ok(())
}

fn check_binary(label: &str, name: &str) {
    if which::which(name).is_ok() {
        println!("{label}: \x1b[32minstalled\x1b[0m");
    } else {
        println!("{label}: \x1b[31mnot found\x1b[0m");
    }
}
