use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use super::ExecResult;

/// Run a command and capture output. Returns ExecResult.
pub async fn run_cmd(program: &str, args: &[&str]) -> Result<ExecResult> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("Failed to execute: {} {}", program, args.join(" ")))?;

    Ok(ExecResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

/// Run a command and return Ok(stdout) on success, Err on non-zero exit.
pub async fn run_ok(program: &str, args: &[&str]) -> Result<String> {
    let result = run_cmd(program, args).await?;
    if result.exit_code != 0 {
        bail!(
            "{} {} failed (exit {}):\n{}",
            program,
            args.join(" "),
            result.exit_code,
            result.stderr.trim()
        );
    }
    Ok(result.stdout)
}

/// Run a command interactively (inheriting stdin/stdout/stderr).
pub async fn run_interactive(program: &str, args: &[&str]) -> Result<ExecResult> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("Failed to execute: {} {}", program, args.join(" ")))?;

    Ok(ExecResult {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::new(),
        stderr: String::new(),
    })
}
