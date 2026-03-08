use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Output format
    #[arg(long, value_enum)]
    pub output: Option<super::OutputFormat>,
}

pub async fn run(args: ListArgs, manager: &SandboxManager) -> Result<()> {
    let sandboxes = manager.list_sandboxes()?;

    if sandboxes.is_empty() {
        println!("No sandboxes found.");
        return Ok(());
    }

    let use_json = matches!(args.output, Some(super::OutputFormat::Json));

    if use_json {
        let json = serde_json::to_string_pretty(&sandboxes)?;
        println!("{json}");
    } else {
        println!(
            "{:<20} {:<12} {:<10} {:<30}",
            "NAME", "RUNTIME", "LAYOUT", "PROJECT DIR"
        );
        println!("{}", "-".repeat(72));
        for s in &sandboxes {
            println!(
                "{:<20} {:<12} {:<10} {:<30}",
                s.name,
                s.runtime,
                s.layout,
                s.project_dir.display()
            );
        }
        println!("\n{} sandbox(es)", sandboxes.len());
    }

    Ok(())
}
