use std::path::Path;

use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct HelpArgs {
    /// Tool name (e.g., zellij, lazygit, nvim, fzf)
    pub tool: Option<String>,
}

pub async fn run(args: HelpArgs, _manager: &SandboxManager) -> Result<()> {
    match args.tool.as_deref() {
        None => show_index(),
        Some(name) => show_cheat_sheet(name),
    }
}

fn show_index() -> Result<()> {
    // Try rendering via glow if available, otherwise print directly
    let content = CHEAT_SHEETS
        .iter()
        .find(|(name, _)| *name == "index")
        .map(|(_, c)| *c)
        .unwrap_or("No index available.");

    render_markdown(content)
}

fn show_cheat_sheet(name: &str) -> Result<()> {
    // 1. Check /etc/devbox/help/ (inside VM)
    let vm_path = Path::new("/etc/devbox/help").join(format!("{name}.md"));
    if vm_path.exists() {
        if try_glow(&vm_path) {
            return Ok(());
        }
        // Fall back to reading file directly
        let content = std::fs::read_to_string(&vm_path)?;
        println!("{content}");
        return Ok(());
    }

    // 2. Check embedded cheat sheets
    if let Some(content) = lookup_embedded(name) {
        return render_markdown(content);
    }

    eprintln!(
        "No cheat sheet for '{name}'. Run `devbox guide` to see available topics."
    );
    Ok(())
}

fn render_markdown(content: &str) -> Result<()> {
    // Try glow for nice rendering
    if which::which("glow").is_ok() {
        use std::process::{Command, Stdio};
        use std::io::Write;

        let mut child = Command::new("glow")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(content.as_bytes())?;
        }
        child.wait()?;
    } else {
        // Plain output
        println!("{content}");
    }
    Ok(())
}

fn try_glow(path: &Path) -> bool {
    if which::which("glow").is_ok() {
        std::process::Command::new("glow")
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    }
}

fn lookup_embedded(name: &str) -> Option<&'static str> {
    CHEAT_SHEETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| *content)
}

/// Embedded cheat sheets — compiled into the binary so they work
/// both on the host CLI and inside the VM.
pub static CHEAT_SHEETS: &[(&str, &str)] = &[
    ("index", include_str!("../../help/index.md")),
    ("devbox", include_str!("../../help/devbox.md")),
    ("zellij", include_str!("../../help/zellij.md")),
    ("lazygit", include_str!("../../help/lazygit.md")),
    ("nvim", include_str!("../../help/nvim.md")),
    ("fzf", include_str!("../../help/fzf.md")),
    ("yazi", include_str!("../../help/yazi.md")),
    ("rg", include_str!("../../help/rg.md")),
    ("fd", include_str!("../../help/fd.md")),
    ("bat", include_str!("../../help/bat.md")),
    ("docker", include_str!("../../help/docker.md")),
    ("git", include_str!("../../help/git.md")),
    ("delta", include_str!("../../help/delta.md")),
    ("httpie", include_str!("../../help/httpie.md")),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_cheat_sheets_embedded() {
        assert_eq!(CHEAT_SHEETS.len(), 14);
    }

    #[test]
    fn lookup_known_sheet() {
        assert!(lookup_embedded("zellij").is_some());
        assert!(lookup_embedded("git").is_some());
        assert!(lookup_embedded("nonexistent").is_none());
    }

    #[test]
    fn index_contains_topics() {
        let index = lookup_embedded("index").unwrap();
        assert!(index.contains("zellij"));
        assert!(index.contains("lazygit"));
        assert!(index.contains("devbox"));
    }
}
