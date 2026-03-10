use anyhow::Result;
use clap::Args;

use crate::runtime::cmd::run_cmd;
use crate::runtime::detect::detect_runtime;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct DoctorArgs {}

pub async fn run(_args: DoctorArgs, manager: &SandboxManager) -> Result<()> {
    #[allow(unused_assignments)]
    let mut has_incus = false;
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
        has_incus = found;

        // QEMU and virtiofsd are required for Incus VMs on Linux
        if found {
            println!("\nIncus VM dependencies:");
            check_binary_with_install(
                "  QEMU",
                "qemu-system-x86_64",
                "sudo apt install qemu-system-x86 qemu-utils -y",
            );
            check_binary_with_install(
                "  virtiofsd",
                "virtiofsd",
                "sudo apt install virtiofsd -y  # or: sudo apt install qemu-system-common -y",
            );
        }
    }

    if os == "macos" {
        let found = check_binary_with_install("  Lima", "limactl", "brew install lima");
        if found {
            has_any_runtime = true;
        } else {
            let found =
                check_binary_with_install("  Multipass", "multipass", "brew install multipass");
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

    // Incus network diagnostics (Linux only)
    if os == "linux" && has_incus {
        println!("\nIncus network:");
        check_incus_network().await;
    }

    println!("\nAll checks complete.");
    Ok(())
}

/// Check Incus network configuration: bridge, NAT, IP forwarding, iptables FORWARD rules.
async fn check_incus_network() {
    // 1. Check incusbr0 exists and has NAT enabled
    let bridge = run_cmd("incus", &["network", "show", "incusbr0"]).await;
    match bridge {
        Ok(r) if r.exit_code == 0 => {
            let has_nat = r.stdout.contains("ipv4.nat") && r.stdout.contains("\"true\"");
            if has_nat {
                println!("  Bridge (incusbr0): \x1b[32mok\x1b[0m (NAT enabled)");
            } else {
                println!("  Bridge (incusbr0): \x1b[33mexists but NAT may be off\x1b[0m");
                println!("    Fix: incus network set incusbr0 ipv4.nat true");
            }
        }
        _ => {
            println!("  Bridge (incusbr0): \x1b[31mnot found\x1b[0m");
            println!("    Fix: incus network create incusbr0");
            return;
        }
    }

    // 2. Check IP forwarding
    let fwd = run_cmd("sysctl", &["-n", "net.ipv4.ip_forward"]).await;
    match fwd {
        Ok(r) if r.stdout.trim() == "1" => {
            println!("  IP forwarding: \x1b[32menabled\x1b[0m");
        }
        _ => {
            println!("  IP forwarding: \x1b[31mdisabled\x1b[0m");
            println!("    Fix: sudo sysctl -w net.ipv4.ip_forward=1");
            println!("    Persist: echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-incus.conf");
        }
    }

    // 3. Check iptables FORWARD chain for incusbr0 rules
    let fwd_rules = run_cmd("iptables", &["-S", "FORWARD"]).await;
    let has_forward_rule = match &fwd_rules {
        Ok(r) => r.stdout.contains("incusbr0") && r.stdout.contains("ACCEPT"),
        Err(_) => false,
    };

    if has_forward_rule {
        println!("  iptables FORWARD: \x1b[32mincusbr0 allowed\x1b[0m");
    } else {
        // Check FORWARD policy
        let policy_drop = match &fwd_rules {
            Ok(r) => r.stdout.contains("-P FORWARD DROP"),
            Err(_) => false,
        };
        if policy_drop {
            println!("  iptables FORWARD: \x1b[31mDROP policy, no incusbr0 rule\x1b[0m");
            println!("    VM traffic is being blocked by the firewall.");
            println!("    Fix:");
            println!("      sudo iptables -I FORWARD -i incusbr0 -j ACCEPT");
            println!("      sudo iptables -I FORWARD -o incusbr0 -m state --state RELATED,ESTABLISHED -j ACCEPT");
        } else {
            println!("  iptables FORWARD: \x1b[32mACCEPT policy\x1b[0m");
        }
    }

    // 4. Check NAT masquerade for Incus subnet
    let nat_rules = run_cmd("iptables", &["-t", "nat", "-S", "POSTROUTING"]).await;
    let has_masq = match &nat_rules {
        Ok(r) => r.stdout.contains("incusbr0") || r.stdout.contains("10.195.64"),
        Err(_) => false,
    };

    if has_masq {
        println!("  iptables NAT: \x1b[32mmasquerade configured\x1b[0m");
    } else {
        println!("  iptables NAT: \x1b[33mno masquerade for Incus subnet\x1b[0m");
        println!("    Fix: sudo iptables -t nat -A POSTROUTING -s 10.195.64.0/24 ! -o incusbr0 -j MASQUERADE");
    }

    // 5. Quick connectivity test if any running VM exists
    let list = run_cmd("incus", &["list", "devbox-", "--format", "json"]).await;
    if let Ok(r) = list {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&r.stdout) {
            for v in &arr {
                if v["status"].as_str() == Some("Running") {
                    let vm_name = v["name"].as_str().unwrap_or("");
                    if !vm_name.is_empty() {
                        let ping = run_cmd(
                            "incus",
                            &["exec", vm_name, "--", "ping", "-c", "1", "-W", "3", "8.8.8.8"],
                        )
                        .await;
                        match ping {
                            Ok(p) if p.exit_code == 0 => {
                                println!(
                                    "  VM connectivity ({vm_name}): \x1b[32mok\x1b[0m"
                                );
                            }
                            _ => {
                                println!(
                                    "  VM connectivity ({vm_name}): \x1b[31mno internet\x1b[0m"
                                );
                            }
                        }
                        break; // Only test one VM
                    }
                }
            }
        }
    }
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
