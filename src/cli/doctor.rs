use std::path::Path;

use anyhow::Result;
use clap::Args;

use crate::runtime::SandboxStatus;
use crate::runtime::detect::detect_runtime;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct DoctorArgs {}

pub async fn run(_args: DoctorArgs, manager: &SandboxManager) -> Result<()> {
    println!("devbox doctor\n");

    let os = std::env::consts::OS;
    let mut has_any_runtime = false;

    println!("Runtime availability:");
    if os == "linux" {
        has_any_runtime |= check_binary_with_install(
            "  Incus",
            "incus",
            "sudo apt install incus  # or: snap install incus",
        );
    }
    if os == "macos" {
        let found = check_binary_with_install("  Lima", "limactl", "brew install lima");
        has_any_runtime |= found;
    }
    let restricted_docker = if !has_any_runtime {
        let available = check_binary_with_install(
            "  Docker (restricted)",
            "docker",
            if os == "macos" {
                "brew install --cask docker  # or: https://docker.com/get-started"
            } else {
                "sudo apt install docker.io  # or: https://docker.com/get-started"
            },
        );
        if available {
            println!(
                "    usable explicitly with: devbox create --runtime docker --image ubuntu --writable --bare"
            );
        }
        available
    } else {
        false
    };
    if !has_any_runtime {
        println!("\n  \x1b[31mNo runtime found!\x1b[0m Install at least one:");
        if os == "macos" {
            println!("    brew install lima          # Recommended for macOS");
        } else {
            println!("    sudo apt install incus     # Recommended for Linux");
        }
        if restricted_docker {
            println!(
                "    Docker is not an auto runtime because it cannot provide protected overlay mounts"
            );
        }
    }

    print!("\nAuto-detected runtime: ");
    match detect_runtime() {
        Ok(runtime) => println!("{} (priority {})", runtime.name(), runtime.priority()),
        Err(error) => println!("NONE — {error}"),
    }

    println!("\nGlobal config:");
    match manager.load_global_config() {
        Ok(config) => {
            println!("  Runtime:  {}", config.default.runtime);
            if config.default.tools.is_empty() {
                println!("  Tools:    (none)");
            } else {
                println!("  Tools:    {}", config.default.tools.join(", "));
            }
        }
        Err(_) => println!("  (using defaults)"),
    }

    println!("\nState directory: {}", manager.state_dir.display());
    let sandboxes = manager.list_sandboxes().unwrap_or_default();
    println!("Sandboxes registered: {}", sandboxes.len());

    println!("\nObservability collector:");
    match crate::obs::daemon::status(manager) {
        Ok(Some(identity)) => {
            println!("  Process:  \x1b[32mrunning\x1b[0m ({identity})");
            match crate::obs::daemon::stats_snapshot(manager) {
                Ok(stats) => println!(
                    "  Events:   received={} stored={} dropped={} rejected={} persist_failed={} agents={}",
                    stats.received,
                    stats.stored,
                    stats.dropped,
                    stats.rejected,
                    stats.persist_failed,
                    stats.agents_connected,
                ),
                Err(error) => println!("  Metrics:  \x1b[31munreadable\x1b[0m — {error}"),
            }
        }
        Ok(None) => println!(
            "  Process:  \x1b[33mnot running\x1b[0m — it starts automatically with box lifecycle commands"
        ),
        Err(error) => println!("  Process:  \x1b[31munknown\x1b[0m — {error}"),
    }

    println!("\nHost kernel capabilities:");
    if os == "linux" {
        print_file_capability(
            "  BTF",
            Path::new("/sys/kernel/btf/vmlinux"),
            "use a kernel built with CONFIG_DEBUG_INFO_BTF",
        );
        print_binary_capability("  nftables", "nft", "install the nftables package");
        print_file_capability(
            "  vsock",
            Path::new("/dev/vsock"),
            "load the vsock device, or use the supported Unix-socket container transport",
        );
    } else {
        println!("  n/a on {os}; these capabilities are checked inside each running Linux guest");
    }

    println!("\nRunning box capabilities:");
    let mut running = 0usize;
    for state in &sandboxes {
        let runtime = match manager.runtime_for_sandbox(state) {
            Ok(runtime) => runtime,
            Err(error) => {
                println!(
                    "  {}: \x1b[31mruntime unavailable\x1b[0m — {error}",
                    state.name
                );
                continue;
            }
        };
        match runtime.status(&state.name).await {
            Ok(SandboxStatus::Running) => {
                running += 1;
                println!("  {} ({}):", state.name, state.runtime);
                match runtime
                    .exec_cmd(&state.name, &["sh", "-c", GUEST_PROBE], false)
                    .await
                {
                    Ok(result) if result.exit_code == 0 => {
                        print_guest_probe(&result.stdout, &state.runtime)
                    }
                    // Keep going: which capture source is live is host-side
                    // knowledge and stays answerable when the guest shell does
                    // not.
                    Ok(result) => println!(
                        "    \x1b[31mprobe failed\x1b[0m (exit {}): {}{}",
                        result.exit_code,
                        result.stdout.trim(),
                        result.stderr.trim()
                    ),
                    Err(error) => println!("    \x1b[31mprobe failed\x1b[0m — {error}"),
                }
                print_capture_source(&manager.state_dir, &state.name);
            }
            Ok(SandboxStatus::Stopped | SandboxStatus::NotFound) => {}
            Ok(SandboxStatus::Unreachable(reason)) => println!(
                "  {}: \x1b[31mguest shell unreachable\x1b[0m — {reason}",
                state.name
            ),
            Ok(SandboxStatus::Unknown(status)) => {
                println!("  {}: status unknown ({status})", state.name)
            }
            Err(error) => println!("  {}: status probe failed — {error}", state.name),
        }
    }
    if running == 0 {
        println!(
            "  (no running boxes; start one to inspect guest BTF/nftables/vsock/agent support)"
        );
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let devbox_toml = cwd.join("devbox.toml");
    if devbox_toml.exists() {
        println!("\nProject config: {} (found)", devbox_toml.display());
    } else {
        println!("\nProject config: not found (run `devbox init` to create)");
    }

    println!("\nSupporting tools:");
    check_binary_with_install(
        "  Nix",
        "nix",
        "curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh",
    );

    println!("\nAll checks complete.");
    Ok(())
}

const GUEST_PROBE: &str = r#"
kernel=$(uname -r 2>/dev/null || printf unknown)
test -r /sys/kernel/btf/vmlinux && btf=ready || btf=missing
command -v nft >/dev/null 2>&1 && nft=ready || nft=missing
test -c /dev/vsock && vsock=ready || vsock=missing
if test -x /usr/local/bin/devbox-obsd; then
  agent=$(/usr/local/bin/devbox-obsd -version 2>/dev/null || printf broken)
else
  agent=missing
fi
missing=
for tool in ip zebra bgpd dnsmasq chronyd busybox; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
# mgmtd, but only where its absence is a fault.
#
# FRR 10 moved interface configuration into it, so without mgmtd a router
# loads its BGP config and none of its addresses -- silently, because zebra
# logs "No such command" per line and carries on. Before FRR 9 there is no
# such binary and zebra still owns that config, so demanding it there would
# report a fault on a substrate that works.
if command -v zebra >/dev/null 2>&1; then
  frrmaj=$(zebra --version 2>/dev/null | sed -n 's/^zebra version \([0-9][0-9]*\).*/\1/p')
  if [ -n "$frrmaj" ] && [ "$frrmaj" -ge 10 ] 2>/dev/null; then
    command -v mgmtd >/dev/null 2>&1 || missing="$missing mgmtd"
  fi
fi
test -z "$missing" && lab=ready || lab="missing:$missing"
printf 'kernel=%s\nbtf=%s\nnftables=%s\nvsock=%s\nagent=%s\nlab=%s\n' \
  "$kernel" "$btf" "$nft" "$vsock" "$agent" "$lab"
"#;

fn print_guest_probe(stdout: &str, runtime: &str) {
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Some((name, value)) = line.split_once('=') else {
            println!("    {line}");
            continue;
        };
        if name == "vsock" && value == "missing" && runtime == "docker" {
            println!("    vsock:    n/a (Docker uses the Unix-socket transport)");
        } else if value == "ready" || (name == "agent" && value != "missing" && value != "broken") {
            println!("    {name}: \x1b[32m{value}\x1b[0m");
        } else if value.starts_with("missing") || value == "broken" {
            println!("    {name}: \x1b[31m{value}\x1b[0m");
        } else {
            println!("    {name}: {value}");
        }
    }
}

/// Name what is actually collecting events inside one running box.
///
/// The guest probe above can only report that the agent *binary* is present.
/// Whether it attached the kernel probes is decided at the agent's preflight
/// and told to the collector in the handshake, and the difference is the whole
/// question a reader has when `devbox watch` shows connections with no process
/// against them: proc polling reads /proc/net/tcp, which has no pid column.
fn print_capture_source(state_dir: &Path, name: &str) {
    use crate::obs::health::{CaptureState, capture_source, load};

    match load(state_dir, name) {
        Ok(Some(health)) if health.state == CaptureState::Streaming => {
            let colour = if health.ebpf { "32" } else { "33" };
            println!(
                "    capture: \x1b[{colour}m{}\x1b[0m",
                capture_source(&health)
            );
        }
        Ok(Some(health)) => {
            let detail = if health.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", health.detail.trim())
            };
            println!(
                "    capture: \x1b[33m{}\x1b[0m{detail}",
                health.state.as_str()
            );
        }
        // No record at all is the ordinary state of a box whose collector has
        // not reached it yet, not a fault.
        Ok(None) => println!("    capture: \x1b[33mno agent has connected yet\x1b[0m"),
        Err(error) => println!("    capture: \x1b[31munreadable\x1b[0m — {error}"),
    }
}

fn print_file_capability(label: &str, path: &Path, hint: &str) {
    if path.exists() {
        println!("{label}: \x1b[32mavailable\x1b[0m ({})", path.display());
    } else {
        println!("{label}: \x1b[31munavailable\x1b[0m — {hint}");
    }
}

fn print_binary_capability(label: &str, name: &str, hint: &str) {
    if which::which(name).is_ok() {
        println!("{label}: \x1b[32mavailable\x1b[0m");
    } else {
        println!("{label}: \x1b[31munavailable\x1b[0m — {hint}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_probe_has_every_v4_capability() {
        for field in ["kernel=", "btf=", "nftables=", "vsock=", "agent=", "lab="] {
            assert!(GUEST_PROBE.contains(field));
        }
        assert!(GUEST_PROBE.contains("/usr/local/bin/devbox-obsd -version"));
    }
}
