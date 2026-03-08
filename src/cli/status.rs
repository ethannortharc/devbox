use anyhow::Result;
use clap::Args;

use crate::runtime::SandboxStatus;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Sandbox name (default: current directory's sandbox)
    pub name: Option<String>,
}

pub async fn run(args: StatusArgs, manager: &SandboxManager) -> Result<()> {
    let name = manager.resolve_name(args.name.as_deref())?;

    if !manager.sandbox_exists(&name) {
        anyhow::bail!("Sandbox '{}' not found.", name);
    }

    let state = manager.get_sandbox(&name)?;
    let runtime = manager.runtime_for_sandbox(&state)?;
    let status = runtime.status(&name).await?;

    let status_str = match &status {
        SandboxStatus::Running => "\x1b[32mRunning\x1b[0m",
        SandboxStatus::Stopped => "\x1b[33mStopped\x1b[0m",
        SandboxStatus::NotFound => "\x1b[31mNot Found\x1b[0m",
        SandboxStatus::Unknown(s) => s.as_str(),
    };

    println!("Sandbox:     {}", name);
    println!("Status:      {}", status_str);
    println!("Runtime:     {}", state.runtime);
    println!("Image:       {}", state.image);
    println!("Project:     {}", state.project_dir.display());
    println!("Mount mode:  {}", state.mount_mode);
    println!("Layout:      {}", state.layout);
    println!("Created:     {}", state.created_at);

    if !state.sets.is_empty() {
        println!("Sets:        {}", state.sets.join(", "));
    }
    if !state.languages.is_empty() {
        println!("Languages:   {}", state.languages.join(", "));
    }

    Ok(())
}
