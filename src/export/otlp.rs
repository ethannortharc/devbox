//! OTLP/JSON rendering — one `ExportLogsServiceRequest` (§8).
//!
//! The encoding is proto3 JSON with the deviations opentelemetry-proto's
//! `docs/specification.md` lists, and two of them decide most of this file:
//!
//! * 64-bit integers — `fixed64`, `int64`, `uint64` — are **decimal strings**.
//!   That covers `timeUnixNano` and every `AnyValue.intValue`. A JSON number
//!   there is what makes a collector reject the payload.
//! * enum fields are **integers**, never their names. `severityNumber` is `9`,
//!   not `"SEVERITY_NUMBER_INFO"`.
//!
//! Field names are lowerCamelCase, because they are proto field names run
//! through the standard mapping — `resourceLogs`, not `resource_logs`.
//!
//! The request is written as a stream, not built and serialised: a prefix, the
//! records with commas between them, and a suffix. An export of a large store
//! is one pass with a bounded page in memory, and an export of an empty window
//! is a well-formed request with an empty `logRecords`.

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::{Context, epoch_nanos};
use crate::obs::event::{Event, EventType};

/// Everything before the first log record.
pub fn request_prefix(ctx: &Context) -> Result<String> {
    let resource = json!({ "attributes": resource_attributes(ctx) });
    let scope = json!({ "name": "devbox", "version": ctx.product_version });
    Ok(format!(
        "{{\"resourceLogs\":[{{\"resource\":{},\"scopeLogs\":[{{\"scope\":{},\"logRecords\":[",
        serde_json::to_string(&resource).context("failed to encode the OTLP resource")?,
        serde_json::to_string(&scope).context("failed to encode the OTLP scope")?,
    ))
}

/// Everything after the last log record: close `logRecords`, the scope, the
/// scope list, the resource, the resource list, and the request.
pub const REQUEST_SUFFIX: &str = "]}]}]}";

/// `service.name`, `service.version`, `devbox.box`, and — once runs exist —
/// `devbox.run.id`.
fn resource_attributes(ctx: &Context) -> Vec<Value> {
    let mut attrs = vec![
        string_attr("service.name", "devbox"),
        string_attr("service.version", &ctx.product_version),
        string_attr("devbox.box", &ctx.box_name),
    ];
    // Absent rather than empty until component A wires runs through.
    if let Some(run_id) = &ctx.run_id {
        attrs.push(string_attr("devbox.run.id", run_id));
    }
    attrs
}

/// One OTLP `LogRecord`.
///
/// Every event maps, including one whose type this build has never seen: the
/// envelope alone — time, pid, comm, box — is a complete record, and the type
/// rides along in `eventName`. That is why OTLP has no unmapped count while
/// OCSF does; there is no class to be wrong about.
pub fn log_record(event: &Event, ctx: &Context) -> Value {
    let (severity_number, severity_text) = severity(event);
    let event_name = format!("devbox.{}", event.kind.as_str());
    json!({
        // fixed64 → decimal string.
        "timeUnixNano": epoch_nanos(&event.ts_wall).to_string(),
        // enum → integer, never the name.
        "severityNumber": severity_number,
        "severityText": severity_text,
        "body": { "stringValue": event.summary() },
        // The proto field, since logs.proto 1.5. §8 also asks for the
        // `event.name` attribute, which is what every collector released
        // before that field reads; both are emitted, and a receiver that does
        // not know `eventName` is required to ignore it.
        "eventName": event_name,
        "attributes": record_attributes(event, ctx, &event_name),
    })
}

/// `severityText` is decided by a policy verdict and is `INFO` for everything
/// else — a box doing its job is not a warning.
fn severity(event: &Event) -> (i64, &'static str) {
    match event.policy.as_ref().map(|p| p.verdict.as_str()) {
        Some("block") => (17, "ERROR"),
        Some("flag") => (13, "WARN"),
        _ => (9, "INFO"),
    }
}

/// The record attributes, in a fixed order so an export is reproducible.
///
/// Three groups: the semantic conventions §8 lists; three more conventions
/// that carry fields the §8 list would otherwise drop on the floor
/// (`process.executable.name` for `comm`, `http.response.status_code`,
/// `server.address` for the resolved peer name); and `devbox.*` for everything
/// with no convention at all, rather than bending an unrelated one to fit.
fn record_attributes(event: &Event, _ctx: &Context, event_name: &str) -> Vec<Value> {
    let mut attrs = vec![
        string_attr("event.name", event_name),
        int_attr("process.pid", i64::from(event.pid)),
    ];
    if event.ppid != 0 {
        attrs.push(int_attr("process.parent_pid", i64::from(event.ppid)));
    }
    if !event.comm.is_empty() {
        attrs.push(string_attr("process.executable.name", &event.comm));
    }
    if event.tid != 0 {
        attrs.push(int_attr("devbox.tid", i64::from(event.tid)));
    }
    attrs.push(int_attr("devbox.uid", i64::from(event.uid)));

    if let Some(exec) = &event.exec {
        if !exec.path.is_empty() {
            attrs.push(string_attr("process.executable.path", &exec.path));
        }
        if !exec.argv.is_empty() {
            attrs.push(string_attr("process.command_line", &exec.argv.join(" ")));
        }
        if !exec.cwd.is_empty() {
            attrs.push(string_attr("devbox.exec.cwd", &exec.cwd));
        }
    }

    if let Some(file) = &event.file {
        if !file.path.is_empty() {
            attrs.push(string_attr("file.path", &file.path));
        }
        if !file.op.is_empty() {
            attrs.push(string_attr("devbox.file.op", &file.op));
        }
        if file.flags != 0 {
            attrs.push(int_attr("devbox.file.flags", i64::from(file.flags)));
        }
    }

    if let Some(net) = &event.net {
        if !net.daddr.is_empty() {
            attrs.push(string_attr("network.peer.address", &net.daddr));
        }
        if net.dport != 0 {
            attrs.push(int_attr("network.peer.port", i64::from(net.dport)));
        }
        if !net.proto.is_empty() {
            attrs.push(string_attr("network.transport", &net.proto));
        }
        // The peer as a name: the resolved domain when there is one, the SNI
        // otherwise. This is what an analyst searches for.
        if let Some(name) = [&net.domain, &net.sni].into_iter().find(|s| !s.is_empty()) {
            attrs.push(string_attr("server.address", name));
        }
        if !net.qname.is_empty() {
            attrs.push(string_attr("dns.question.name", &net.qname));
        }
        if !net.qtype.is_empty() {
            attrs.push(string_attr("devbox.dns.qtype", &net.qtype));
        }
        if !net.answers.is_empty() {
            attrs.push(string_array_attr("devbox.dns.answers", &net.answers));
        }
        if event.kind == EventType::Dns {
            attrs.push(bool_attr("devbox.dns.response", net.response));
        }
        if !net.sni.is_empty() {
            attrs.push(string_attr("tls.client.server_name", &net.sni));
        }
        if !net.alpn.is_empty() {
            attrs.push(string_attr("devbox.tls.alpn", &net.alpn));
        }
        // `close` says which way the connection went, since its type no longer
        // does, and says when it never saw the connection opened at all.
        if !net.dir.is_empty() {
            attrs.push(string_attr("devbox.net.direction", &net.dir));
        }
        if net.orphan {
            attrs.push(bool_attr("devbox.net.orphan", true));
        }
        // Zero-valued on an open; the connection-settlement event carries the
        // totals.
        if net.bytes_tx != 0 {
            attrs.push(uint_attr("devbox.net.bytes_tx", net.bytes_tx));
        }
        if net.bytes_rx != 0 {
            attrs.push(uint_attr("devbox.net.bytes_rx", net.bytes_rx));
        }
        if net.dur_ms != 0 {
            attrs.push(uint_attr("devbox.net.duration_ms", net.dur_ms));
        }
    }

    if let Some(api) = &event.api {
        if !api.method.is_empty() {
            attrs.push(string_attr(
                "http.request.method",
                &api.method.to_ascii_uppercase(),
            ));
        }
        if !api.host.is_empty() {
            attrs.push(string_attr(
                "url.full",
                &format!("https://{}{}", api.host, api.path),
            ));
            attrs.push(string_attr("server.address", &api.host));
        }
        if api.status != 0 {
            attrs.push(int_attr("http.response.status_code", i64::from(api.status)));
        }
        if api.tokens != 0 {
            attrs.push(uint_attr("devbox.api.tokens", api.tokens));
        }
        if !api.endpoint.is_empty() {
            attrs.push(string_attr("devbox.api.endpoint", &api.endpoint));
        }
    }

    if let Some(policy) = &event.policy {
        attrs.push(string_attr("devbox.policy.verdict", &policy.verdict));
        attrs.push(string_attr("devbox.policy.mode", &policy.mode));
        if !policy.target.is_empty() {
            attrs.push(string_attr("devbox.policy.target", &policy.target));
        }
        if !policy.reason.is_empty() {
            attrs.push(string_attr("devbox.policy.reason", &policy.reason));
        }
    }

    attrs
}

fn string_attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

/// `AnyValue.intValue` is an `int64`, so proto3 JSON spells it as a decimal
/// string. A bare number is accepted by some receivers and rejected by others;
/// the string is what the mapping specifies.
fn int_attr(key: &str, value: i64) -> Value {
    json!({ "key": key, "value": { "intValue": value.to_string() } })
}

fn uint_attr(key: &str, value: u64) -> Value {
    json!({ "key": key, "value": { "intValue": value.to_string() } })
}

fn bool_attr(key: &str, value: bool) -> Value {
    json!({ "key": key, "value": { "boolValue": value } })
}

fn string_array_attr(key: &str, values: &[String]) -> Value {
    json!({
        "key": key,
        "value": {
            "arrayValue": {
                "values": values.iter().map(|v| json!({ "stringValue": v })).collect::<Vec<_>>(),
            }
        }
    })
}
