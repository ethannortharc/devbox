use anyhow::{Result, bail};

use super::Runtime;
use super::docker::DockerRuntime;
use super::incus::IncusRuntime;
use super::lima::LimaRuntime;
use super::multipass::MultipassRuntime;

/// Detect the best available runtime, ordered by priority.
///
/// Only runtimes that implement the default NixOS + protected-overlay
/// contract participate. Docker remains explicitly available for its narrow
/// bare/Ubuntu/writable mode; Multipass remains manageable for existing boxes.
pub fn detect_runtime() -> Result<Box<dyn Runtime>> {
    let runtimes: Vec<Box<dyn Runtime>> = vec![Box::new(IncusRuntime), Box::new(LimaRuntime)];

    let mut available: Vec<Box<dyn Runtime>> =
        runtimes.into_iter().filter(|r| r.is_available()).collect();

    available.sort_by_key(|b| std::cmp::Reverse(b.priority()));

    match available.into_iter().next() {
        Some(rt) => Ok(rt),
        None => {
            bail!(
                "No supported runtime found.\n\
                 Install one of:\n  \
                 - Incus (Linux): https://linuxcontainers.org/incus/\n  \
                 - Lima (macOS):  brew install lima\n\n\
                 Docker is available explicitly only for bare Ubuntu writable boxes:\n  \
                 devbox create --runtime docker --image ubuntu --writable --bare"
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
