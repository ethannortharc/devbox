//! The full model as JSON — the contract for tooling and for the export (§4.4).
//!
//! Pretty-printed rather than compact. It is written to a file people open, it
//! is embedded in the HTML where someone will read it with the browser's
//! devtools, and it is the input to a diff between two runs — all three of
//! which want line-oriented output far more than they want the bytes back.

use anyhow::{Context, Result};

use super::model::RunReport;

pub fn render(report: &RunReport) -> Result<String> {
    serde_json::to_string_pretty(report).context("failed to encode the run report as JSON")
}

/// Parse a report back — how `devbox report --format md` re-renders a run
/// whose events have since aged out of the store.
pub fn parse(raw: &str) -> Result<RunReport> {
    serde_json::from_str(raw).context("failed to decode a run report")
}
