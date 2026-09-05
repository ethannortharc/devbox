//! The canonical devbox event, one JSON object per line.
//!
//! The lossless format, and the reason it exists: OCSF drops what it has no
//! class for and OTLP flattens sub-objects into attributes, so when a question
//! turns out to need a field neither of them carries, this is the export that
//! still has it. It is also the only format that survives an event type this
//! build has never seen — `serde` round-trips the envelope either way.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::obs::event::Event;

/// Render one event exactly as the store holds it.
pub fn render(event: &Event) -> Result<Value> {
    serde_json::to_value(event).context("failed to encode an event as JSON")
}
