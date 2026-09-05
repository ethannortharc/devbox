//! Export the event store into formats other tools already read (§8).
//!
//! Three renderings of the same rows:
//!
//! * `jsonl` — the canonical devbox event, one per line. Lossless, and the
//!   only format that survives a schema this crate has not seen.
//! * `ocsf` — OCSF 1.3 classes, one per line. Lossy by construction: a devbox
//!   event type with no class in §8's table is *not* written, and is counted,
//!   because inventing a class for it would put a claim into an audit record
//!   that nothing observed.
//! * `otlp-json` — one `ExportLogsServiceRequest`, JSON-encoded per the
//!   opentelemetry-proto JSON mapping. Every event maps: the envelope alone is
//!   a complete log record, so an unrecognised type still exports.
//!
//! Everything here is hand-rendered JSON on top of `serde_json` (ADR-0018:
//! no SDK for a wire format we emit and never parse).
//!
//! The reader is a cursor, not a `SELECT *`. A box's store holds millions of
//! rows; an export that materialised them would be the one command that cannot
//! run on the box whose record matters most.

pub mod jsonl;
pub mod ocsf;
pub mod otlp;

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::obs::store::Store;

/// The wire format an export is rendered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// OCSF 1.3 events, one JSON object per line.
    Ocsf,
    /// One OTLP/JSON `ExportLogsServiceRequest`.
    OtlpJson,
    /// The canonical devbox event, one JSON object per line.
    Jsonl,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Ocsf => "ocsf",
            Format::OtlpJson => "otlp-json",
            Format::Jsonl => "jsonl",
        }
    }
}

/// Everything an export knows that is not in the event itself.
#[derive(Debug, Clone)]
pub struct Context {
    /// The box the events came from. `device.hostname` in OCSF, `devbox.box`
    /// in OTLP.
    pub box_name: String,
    /// This binary's version, stamped into `metadata.product.version` and
    /// `service.version` so a record says which mapping produced it.
    pub product_version: String,
    /// The run these events belong to, once runs exist (component A).
    ///
    /// `None` for the whole of wave 1, which is why `metadata.correlation_uid`
    /// and the `devbox.run.id` resource attribute are absent rather than
    /// empty: an empty correlation id in an audit record reads as "correlated
    /// with nothing", which is a different claim from "not yet correlated".
    pub run_id: Option<String>,
}

impl Context {
    pub fn new(box_name: impl Into<String>) -> Self {
        Self {
            box_name: box_name.into(),
            product_version: env!("CARGO_PKG_VERSION").to_string(),
            run_id: None,
        }
    }
}

/// What an export did, so the command can say it out loud on stderr.
///
/// The invariant every format holds, and every test asserts:
/// `matched == written + unmapped`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Rows the cursor read out of the store, window or no window.
    pub scanned: u64,
    /// Rows inside the requested window — the "query count".
    pub matched: u64,
    /// Records actually emitted.
    pub written: u64,
    /// Matched rows this build has no mapping for, so did not write.
    pub unmapped: u64,
    /// Which types those were, for the one line that tells the operator what
    /// is missing from their export.
    pub unmapped_kinds: BTreeMap<String, u64>,
}

impl Stats {
    fn record_unmapped(&mut self, kind: &str) {
        self.unmapped += 1;
        *self.unmapped_kinds.entry(kind.to_string()).or_insert(0) += 1;
    }

    /// `matched == written + unmapped`, which is the only thing that makes
    /// "nothing was silently dropped" checkable from the outside.
    pub fn balances(&self) -> bool {
        self.matched == self.written + self.unmapped
    }

    /// `exec=3, syscall=1`, or empty when everything mapped.
    pub fn unmapped_summary(&self) -> String {
        self.unmapped_kinds
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The time window an export covers, already normalised to the store's form.
#[derive(Debug, Clone, Default)]
pub struct Window {
    /// Inclusive lower bound on `ts_wall`.
    pub from: Option<String>,
    /// Exclusive upper bound on `ts_wall`, matching `Query::until`.
    pub until: Option<String>,
}

impl Window {
    /// Parse the two CLI timestamps into the store's normal form.
    ///
    /// Rejects what it cannot parse rather than passing it through. The store's
    /// own normaliser falls back to the raw string, which for a query means a
    /// typo quietly selects a different window; for an export it would mean an
    /// audit trail that covers a window nobody asked for.
    pub fn parse(from: Option<&str>, until: Option<&str>) -> Result<Self> {
        Ok(Self {
            from: from.map(normalize_ts).transpose()?,
            until: until.map(normalize_ts).transpose()?,
        })
    }

    /// Whether an event's `ts_wall` falls inside the window.
    ///
    /// String comparison, deliberately: `ts_wall` is fixed-width UTC with
    /// millisecond precision, so it sorts chronologically, and both bounds
    /// have been put into the same form. This is the same comparison the store
    /// does in SQL, so `--from`/`--to` mean here what they mean there.
    pub fn contains(&self, ts_wall: &str) -> bool {
        if let Some(from) = &self.from
            && ts_wall < from.as_str()
        {
            return false;
        }
        if let Some(until) = &self.until
            && ts_wall >= until.as_str()
        {
            return false;
        }
        true
    }
}

/// RFC 3339 in, the store's `ts_wall` spelling out.
fn normalize_ts(ts: &str) -> Result<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts)
        .with_context(|| format!("{ts:?} is not an RFC 3339 timestamp"))?;
    Ok(dt
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string())
}

/// Rows read from the store per cursor step.
///
/// The point of the cursor is that peak memory does not grow with the store,
/// so this is the number that has to stay small. The transport admits frames
/// up to a megabyte, so a page is bounded by `PAGE * 1 MiB` in the worst case
/// and by a few tens of kilobytes in every real one.
const PAGE: usize = 256;

/// Byte cap handed to the store's scan.
///
/// `tail_scan` nulls — and therefore silently skips — any row larger than its
/// cap. A console can afford that; an export cannot, because the row it drops
/// is exactly the megabyte-long command line someone will ask about. `i64::MAX`
/// makes the SQL predicate always true, which is how you say "no cap" through
/// an API that requires one.
const NO_BYTE_CAP: usize = i64::MAX as usize;

/// Stream one export from `store` into `out`.
///
/// Returns what it did rather than printing it: the caller decides whether an
/// export with unmapped rows is a warning or a failure.
pub fn run<W: Write>(
    store: &Store,
    window: &Window,
    ctx: &Context,
    format: Format,
    out: &mut W,
) -> Result<Stats> {
    let mut stats = Stats::default();

    // Pin the high-water mark before the first page. Without it, rows written
    // while the export runs land inside a later page but outside the window
    // the export claims to cover.
    let up_to = store
        .max_id()
        .context("failed to read the store's high-water mark")?;

    if format == Format::OtlpJson {
        out.write_all(otlp::request_prefix(ctx)?.as_bytes())
            .context("failed to write the OTLP request header")?;
    }
    let mut first_record = true;

    let mut cursor = 0i64;
    while cursor < up_to {
        let (events, scanned_to) = store
            .tail_scan(cursor, up_to, PAGE, NO_BYTE_CAP)
            .context("failed to read a page of events")?;
        stats.scanned += events.len() as u64;

        for event in &events {
            if !window.contains(&event.ts_wall) {
                continue;
            }
            stats.matched += 1;
            match format {
                Format::Jsonl => {
                    write_line(out, &jsonl::render(event)?)?;
                    stats.written += 1;
                }
                Format::Ocsf => match ocsf::render(event, ctx) {
                    Some(value) => {
                        write_line(out, &value)?;
                        stats.written += 1;
                    }
                    None => stats.record_unmapped(event.kind.as_str()),
                },
                Format::OtlpJson => {
                    if !first_record {
                        out.write_all(b",")
                            .context("failed to write an OTLP separator")?;
                    }
                    first_record = false;
                    let record = otlp::log_record(event, ctx);
                    serde_json::to_writer(&mut *out, &record)
                        .context("failed to write an OTLP log record")?;
                    stats.written += 1;
                }
            }
        }

        // `scanned_to` is the position the scan *reached*, not the id of the
        // last row that decoded, so a row written by an older schema advances
        // the cursor instead of wedging it.
        if scanned_to <= cursor {
            break;
        }
        cursor = scanned_to;
    }

    if format == Format::OtlpJson {
        out.write_all(otlp::REQUEST_SUFFIX.as_bytes())
            .context("failed to close the OTLP request")?;
        out.write_all(b"\n")
            .context("failed to close the OTLP request")?;
    }
    out.flush().context("failed to flush the export")?;

    debug_assert!(stats.balances(), "export lost rows: {stats:?}");
    Ok(stats)
}

fn write_line<W: Write>(out: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value).context("failed to write an exported record")?;
    out.write_all(b"\n")
        .context("failed to write an exported record")?;
    Ok(())
}

/// `ts_wall` as milliseconds since the epoch, which is OCSF's `time`.
///
/// A row whose timestamp does not parse exports as `0` rather than being
/// dropped: a visibly wrong timestamp on a present event is recoverable, an
/// absent event is not.
pub(crate) fn epoch_millis(ts_wall: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(ts_wall) {
        Ok(dt) => dt.timestamp_millis(),
        Err(e) => {
            tracing::warn!(ts = %ts_wall, error = %e, "event timestamp does not parse");
            0
        }
    }
}

/// `ts_wall` as nanoseconds since the epoch, which is OTLP's `timeUnixNano`.
pub(crate) fn epoch_nanos(ts_wall: &str) -> i128 {
    match chrono::DateTime::parse_from_rfc3339(ts_wall) {
        Ok(dt) => dt.timestamp_nanos_opt().map(i128::from).unwrap_or_else(|| {
            i128::from(dt.timestamp()) * 1_000_000_000 + i128::from(dt.timestamp_subsec_nanos())
        }),
        Err(e) => {
            tracing::warn!(ts = %ts_wall, error = %e, "event timestamp does not parse");
            0
        }
    }
}

/// The last path segment, for `file.name` and `process.name`.
pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
