//! Run reports — one execution, three renderings (§4.4).
//!
//! [`model`] is the contract: the JSON *is* [`model::RunReport`], and the
//! markdown and HTML are views of the same value rather than three programs
//! that each read the store and hope to agree. That is what makes the terminal
//! summary, the file on disk, and the console page describe the same run.
//!
//! Reports live at `~/.devbox/runs/<box>/<run_id>/report.{md,json,html}`.

pub mod html;
pub mod json;
pub mod markdown;
pub mod model;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use model::{RunReport, SCOPE_BOX, SCOPE_RUN};

/// The three files a rendered report leaves behind.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub markdown: PathBuf,
    pub json: PathBuf,
    pub html: PathBuf,
}

/// Write all three renderings under `~/.devbox/runs/<box>/<run_id>/`.
///
/// The directory is the run id, which [`crate::obs::run::is_run_id`]
/// constrains to base32 — checked here too rather than trusted, because this
/// is the one place a run id becomes a path.
pub fn write(state_dir: &Path, report: &RunReport) -> Result<Rendered> {
    if !crate::obs::run::is_run_id(&report.run.run_id) {
        anyhow::bail!("refusing to write a report for {:?}", report.run.run_id);
    }
    if !crate::sandbox::state::is_safe_name(&report.run.box_id) {
        anyhow::bail!("refusing to write a report for box {:?}", report.run.box_id);
    }

    let directory = RunReport::directory(state_dir, &report.run.box_id, &report.run.run_id);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create report directory {}", directory.display()))?;

    let rendered = Rendered {
        markdown: directory.join("report.md"),
        json: directory.join("report.json"),
        html: directory.join("report.html"),
    };
    write_file(&rendered.markdown, &markdown::render(report))?;
    write_file(&rendered.json, &json::render(report)?)?;
    write_file(&rendered.html, &html::render(report)?)?;
    Ok(rendered)
}

/// Read a report back from disk.
///
/// The JSON is the source: markdown and HTML are renderings, so re-rendering
/// from the stored model is how `devbox report <id> --format html` still works
/// after the events themselves have aged out of the store.
pub fn load(state_dir: &Path, box_id: &str, run_id: &str) -> Result<Option<RunReport>> {
    if !crate::obs::run::is_run_id(run_id) || !crate::sandbox::state::is_safe_name(box_id) {
        return Ok(None);
    }
    let path = RunReport::directory(state_dir, box_id, run_id).join("report.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    json::parse(&raw).map(Some)
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    // A report names every host the command reached and every file it changed.
    // On a shared host that is nobody else's business.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect {}", path.display()))?;
    Ok(())
}
