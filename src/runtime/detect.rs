use anyhow::{Result, bail};

use super::Runtime;
use super::docker::DockerRuntime;
use super::incus::IncusRuntime;
use super::lima::LimaRuntime;
use super::multipass::MultipassRuntime;

/// Detect the best available runtime, ordered by priority.
///
/// Priority: Incus (30) > Lima (20) > Multipass (15) > Docker (10)
pub fn detect_runtime() -> Result<Box<dyn Runtime>> {
    let runtimes: Vec<Box<dyn Runtime>> = vec![
        Box::new(IncusRuntime),
        Box::new(LimaRuntime),
        Box::new(MultipassRuntime),
        Box::new(DockerRuntime),
    ];

    let mut available: Vec<Box<dyn Runtime>> = runtimes
        .into_iter()
        .filter(|r| r.is_available())
        .collect();

    available.sort_by(|a, b| b.priority().cmp(&a.priority()));

    match available.into_iter().next() {
        Some(rt) => {
            if rt.name() == "docker" {
                eprintln!(
                    "\x1b[33m\u{26a0} Docker provides weaker isolation (shared kernel).\n  \
                     For full isolation, install Incus (Linux) or Lima (macOS).\x1b[0m"
                );
            }
            Ok(rt)
        }
        None => {
            bail!(
                "No supported runtime found.\n\
                 Install one of:\n  \
                 - Incus (Linux): https://linuxcontainers.org/incus/\n  \
                 - Lima (macOS):  brew install lima\n  \
                 - Docker:        https://docs.docker.com/get-docker/"
            )
        }
    }
}

/// Select a specific runtime by name.
pub fn select_runtime(name: &str) -> Result<Box<dyn Runtime>> {
    let rt: Box<dyn Runtime> = match name {
        "incus" => Box::new(IncusRuntime),
        "lima" => Box::new(LimaRuntime),
        "multipass" => Box::new(MultipassRuntime),
        "docker" => Box::new(DockerRuntime),
        other => bail!("Unknown runtime: {other}. Options: incus, lima, multipass, docker"),
    };

    if !rt.is_available() {
        bail!("Runtime '{}' is not available on this system", rt.name());
    }

    Ok(rt)
}
