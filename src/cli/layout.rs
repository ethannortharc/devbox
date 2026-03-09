use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::sandbox::SandboxManager;
use crate::tui::{find_layout, LAYOUTS};

#[derive(Args, Debug)]
pub struct LayoutArgs {
    #[command(subcommand)]
    pub action: LayoutAction,
}

#[derive(Subcommand, Debug)]
pub enum LayoutAction {
    /// List available layouts
    List,
    /// Preview a layout (ASCII)
    Preview { name: String },
    /// Edit a layout in $EDITOR
    Edit { name: String },
    /// Create a new layout from template
    Create { name: String },
    /// Set the default layout
    SetDefault { name: String },
    /// Save current Zellij layout for next login
    Save {
        /// Sandbox name (default: derived from directory)
        #[arg(long)]
        name: Option<String>,
    },
    /// Reset to built-in layout (remove saved layout)
    Reset {
        /// Sandbox name (default: derived from directory)
        #[arg(long)]
        name: Option<String>,
    },
}

pub async fn run(args: LayoutArgs, manager: &SandboxManager) -> Result<()> {
    match args.action {
        LayoutAction::List => {
            println!("{:<16} {}", "NAME", "DESCRIPTION");
            println!("{}", "-".repeat(60));
            for l in LAYOUTS {
                println!("{:<16} {}", l.name, l.description);
            }
            println!("\n{} layout(s) available", LAYOUTS.len());
            Ok(())
        }
        LayoutAction::Preview { name } => {
            match find_layout(&name) {
                Some(l) => {
                    println!("Layout: {}", l.name);
                    println!("{}", l.description);
                    println!("{}", l.preview);
                }
                None => {
                    anyhow::bail!("Layout '{}' not found. Run `devbox layout list` to see options.", name);
                }
            }
            Ok(())
        }
        LayoutAction::Edit { name } => {
            let path = layout_path(&name);
            if !path.exists() {
                anyhow::bail!(
                    "Layout file not found: {}\nBuilt-in layouts are at: layouts/",
                    path.display()
                );
            }

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()?;
            if !status.success() {
                anyhow::bail!("Editor exited with non-zero status");
            }
            Ok(())
        }
        LayoutAction::Create { name } => {
            let path = layout_path(&name);
            if path.exists() {
                anyhow::bail!("Layout '{}' already exists at {}", name, path.display());
            }

            let template = format!(
                r#"// Devbox — Custom layout: {name}
layout {{
    default_tab_template {{
        pane size=1 borderless=true {{
            plugin location="zellij:status-bar"
        }}
        children
    }}

    tab name="{name}" focus=true {{
        pane split_direction="vertical" {{
            pane name="editor" size="50%" {{
                command "nvim"
                args "."
            }}
            pane name="terminal" size="50%"
        }}
    }}

    tab name="shell" {{
        pane name="main"
    }}
}}
"#
            );

            // Ensure layouts dir exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, template)?;
            println!("Created layout '{}' at {}", name, path.display());
            println!("Edit with: devbox layout edit {name}");
            Ok(())
        }
        LayoutAction::Save { name: sandbox_name } => {
            let sb_name = manager.resolve_name(sandbox_name.as_deref())?;
            let state = manager.get_sandbox(&sb_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            // Use $HOME inside the VM to get the correct path
            let save_cmd = concat!(
                "d=\"$HOME/.config/devbox\"; ",
                "mkdir -p \"$d\" && ",
                "zellij action dump-layout > \"$d/saved-layout.kdl\" 2>/dev/null && ",
                "echo OK || echo FAIL"
            );
            let result = runtime
                .exec_cmd(&sb_name, &["bash", "-c", save_cmd], false)
                .await?;

            if !result.stdout.trim().contains("OK") {
                anyhow::bail!(
                    "Failed to save layout. Is Zellij running in '{}'?\n\
                     Tip: Run this from inside the devbox, or attach first with `devbox shell`.",
                    sb_name
                );
            }

            println!("Layout saved for sandbox '{sb_name}'.");
            println!("Next login will use the saved layout automatically.");
            println!("To reset: devbox layout reset --name {sb_name}");
            Ok(())
        }
        LayoutAction::Reset { name: sandbox_name } => {
            let sb_name = manager.resolve_name(sandbox_name.as_deref())?;
            let state = manager.get_sandbox(&sb_name)?;
            let runtime = manager.runtime_for_sandbox(&state)?;

            // Use $HOME inside the VM
            let check_cmd = "f=\"$HOME/.config/devbox/saved-layout.kdl\"; [ -f \"$f\" ] && echo EXISTS";
            let check = runtime
                .exec_cmd(&sb_name, &["bash", "-c", check_cmd], false)
                .await;
            let exists = check.is_ok() && check.unwrap().stdout.trim().contains("EXISTS");

            if !exists {
                println!("No saved layout found for sandbox '{sb_name}'. Already using built-in layout.");
                return Ok(());
            }

            // Remove saved layout
            runtime
                .exec_cmd(
                    &sb_name,
                    &["bash", "-c", "rm -f \"$HOME/.config/devbox/saved-layout.kdl\""],
                    false,
                )
                .await?;

            println!("Saved layout removed for sandbox '{sb_name}'.");
            println!("Next login will use the built-in '{}' layout.", state.layout);
            Ok(())
        }
        LayoutAction::SetDefault { name } => {
            if find_layout(&name).is_none() {
                // Check if custom layout file exists
                let path = layout_path(&name);
                if !path.exists() {
                    anyhow::bail!("Layout '{}' not found.", name);
                }
            }

            let mut config = manager.load_global_config()?;
            config.set("default.layout", &name)?;
            manager.save_global_config(&config)?;
            println!("Default layout set to '{name}'.");
            Ok(())
        }
    }
}

/// Get the path to a layout KDL file.
fn layout_path(name: &str) -> PathBuf {
    // Check for custom layouts in ~/.devbox/layouts/ first,
    // then fall back to the built-in layouts directory.
    let home_layouts = dirs::home_dir()
        .unwrap_or_default()
        .join(".devbox")
        .join("layouts");

    let custom = home_layouts.join(format!("{name}.kdl"));
    if custom.exists() {
        return custom;
    }

    // Built-in layouts (relative to binary or install path)
    home_layouts.join(format!("{name}.kdl"))
}
