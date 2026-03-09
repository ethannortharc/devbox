use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::runtime::Mount;
use crate::sandbox::SandboxManager;
use crate::sandbox::config::DevboxConfig;

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Sandbox name (default: derived from directory name)
    #[arg(long)]
    pub name: Option<String>,

    /// Runtime to use
    #[arg(long, value_enum)]
    pub runtime: Option<RuntimeChoice>,

    /// Tools/sets to install (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub tools: Option<Vec<String>>,

    /// CPU cores (0 = runtime default)
    #[arg(long, default_value = "0")]
    pub cpu: u32,

    /// Memory limit (e.g., "4G", "" = runtime default)
    #[arg(long, default_value = "")]
    pub memory: String,

    /// Additional mount points (host:container or host:container:ro)
    #[arg(long)]
    pub mount: Option<Vec<String>>,

    /// Environment file
    #[arg(long)]
    pub env_file: Option<String>,

    /// Environment variable (KEY=VALUE)
    #[arg(long, short)]
    pub env: Option<Vec<String>>,

    /// Direct host writes (bypass OverlayFS)
    #[arg(long)]
    pub writable: bool,

    /// Mount mode
    #[arg(long, value_enum)]
    pub mount_mode: Option<MountMode>,

    /// Skip auto-detection, minimal install
    #[arg(long)]
    pub bare: bool,

    /// Zellij layout to use
    #[arg(long)]
    pub layout: Option<String>,

    /// Base image: nixos (default, declarative) or ubuntu (familiar + Nix packages)
    #[arg(long, value_enum, default_value = "nixos")]
    pub image: ImageChoice,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ImageChoice {
    Nixos,
    Ubuntu,
}

impl ImageChoice {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Nixos => "nixos",
            Self::Ubuntu => "ubuntu",
        }
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum RuntimeChoice {
    Incus,
    Lima,
    Multipass,
    Docker,
}

impl RuntimeChoice {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Incus => "incus",
            Self::Lima => "lima",
            Self::Multipass => "multipass",
            Self::Docker => "docker",
        }
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum MountMode {
    Overlay,
    Writable,
}

pub async fn run(args: CreateArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;
    let runtime_name = args.runtime.as_ref().map(|r| r.as_str());
    let runtime = manager.resolve_runtime(runtime_name)?;

    // Load or generate config
    let cwd = std::env::current_dir()?;
    let mut config = DevboxConfig::load_or_default(&cwd);

    // Apply CLI overrides
    if let Some(tools) = &args.tools {
        config.apply_tools(tools);
    }
    if args.cpu > 0 {
        config.resources.cpu = args.cpu;
    }
    if !args.memory.is_empty() {
        config.resources.memory = args.memory.clone();
    }
    if let Some(layout) = &args.layout {
        config.sandbox.layout = layout.clone();
    }
    if args.writable {
        config.sandbox.mount_mode = "writable".to_string();
    }
    if let Some(mode) = &args.mount_mode {
        config.sandbox.mount_mode = match mode {
            MountMode::Overlay => "overlay",
            MountMode::Writable => "writable",
        }
        .to_string();
    }
    config.sandbox.image = args.image.as_str().to_string();

    // Parse extra mounts
    let extra_mounts = parse_mounts(&args.mount.unwrap_or_default())?;

    // Parse env vars
    let mut env_vars = HashMap::new();
    if let Some(envs) = &args.env {
        for e in envs {
            if let Some((k, v)) = e.split_once('=') {
                env_vars.insert(k.to_string(), v.to_string());
            }
        }
    }

    let env_file = args.env_file.map(PathBuf::from);

    manager
        .create_sandbox(
            &name,
            runtime.as_ref(),
            &config,
            &extra_mounts,
            &env_vars,
            env_file,
            args.bare,
        )
        .await?;

    // Attach immediately after create
    manager.attach(&name, None, false).await
}

fn parse_mounts(mounts: &[String]) -> Result<Vec<Mount>> {
    let mut result = vec![];
    for m in mounts {
        let parts: Vec<&str> = m.splitn(3, ':').collect();
        match parts.len() {
            2 => {
                result.push(Mount {
                    host_path: PathBuf::from(parts[0]),
                    container_path: parts[1].to_string(),
                    read_only: false,
                });
            }
            3 => {
                result.push(Mount {
                    host_path: PathBuf::from(parts[0]),
                    container_path: parts[1].to_string(),
                    read_only: parts[2] == "ro",
                });
            }
            _ => {
                anyhow::bail!("Invalid mount format: '{}'. Use host:container or host:container:ro", m);
            }
        }
    }
    Ok(result)
}
