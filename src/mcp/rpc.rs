//! `devbox mcp self` — devbox as an MCP server (§7.3).
//!
//! The inverse of [`super::shim`]. There, devbox is the transport and someone
//! else's server is at the far end; here devbox *is* the server, and what it
//! exposes is the evidence plane: what runs happened, what one of them did,
//! what a box has been doing, and the raw event stream. An agent that can ask
//! those questions can check its own work — "did the change I just made reach
//! the network?" — without a human reading a report to it.
//!
//! JSON-RPC 2.0 over stdio, one message per line, which is the framing every
//! stdio MCP client speaks. Two rules the rest of this file exists to keep:
//!
//! - **Nothing but JSON-RPC reaches stdout.** A stray `println!` ends the
//!   session with a parse error the agent reports as "server crashed". Every
//!   diagnostic goes to stderr.
//! - **No request panics.** A malformed message, an unknown box, a store that
//!   will not open — each becomes an error *object*, because a server that
//!   exits on bad input is a server the agent cannot recover from.
//!
//! No new dependencies: `serde_json` was already here, and the protocol is
//! small enough that a schema-driven client library would be more code than
//! the four tools it would serve.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::obs::store::{Query, Store};
use crate::obs::{Event, EventType};
use crate::sandbox::SandboxManager;

/// Protocol revisions this server implements.
///
/// Both are the same wire protocol for what these four tools do; the newer one
/// adds capabilities (audio content, completions, tool annotations) that a
/// read-only evidence server does not use. Listing both means a client pinned
/// to either gets its own version echoed rather than a downgrade it has to
/// decide about.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-03-26", "2024-11-05"];

/// What we answer with when the client asks for something we do not know.
pub const LATEST_PROTOCOL: &str = "2025-03-26";

/// The most events one `watch` call will return.
const WATCH_MAX: usize = 200;

/// The most runs one `list_runs` call will return.
const RUNS_MAX: usize = 200;

/// Longest single JSON-RPC message accepted, in bytes.
///
/// A line-framed protocol has no other bound: without this, a client that
/// never sends a newline is a client that makes the server grow until the host
/// runs out of memory.
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// The JSON-RPC error codes this server uses.
///
/// `-32603` (internal error) is deliberately absent: everything devbox itself
/// could not do comes back as a *tool* result with `isError`, because a client
/// treats a protocol error as "this server is broken" and stops, while an
/// `isError` result is "that did not work, try something else".
mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

/// Serve until stdin closes.
pub async fn serve(manager: &SandboxManager) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut message = Vec::with_capacity(4096);

    loop {
        let Some(within_bounds) = read_message(&mut input, &mut message).await? else {
            // EOF: the client closed its end, which is how a session ends.
            return Ok(());
        };
        let response = if within_bounds {
            let text = String::from_utf8_lossy(&message);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            handle(manager, trimmed).await
        } else {
            Some(error_response(
                Value::Null,
                code::PARSE,
                "message exceeds the maximum size this server will read",
            ))
        };
        if let Some(response) = response {
            output.write_all(response.as_bytes()).await?;
            output.write_all(b"\n").await?;
            // Per message, not per batch: the client is blocked on this answer.
            output.flush().await?;
        }
    }
}

/// One newline-terminated message, bounded.
///
/// `Ok(None)` at end of input. `Ok(Some(false))` for a message longer than
/// [`MAX_MESSAGE`]: it is consumed to its newline and rejected, so one
/// oversized message costs the session an error rather than the host its
/// memory — and the session continues, which a hard exit would not allow.
///
/// Assembled by hand rather than with `read_line`, because bounding it needs
/// `AsyncReadExt::take`, and what `take` returns is not an `AsyncBufRead`.
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(
    input: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut oversize = false;
    loop {
        let chunk = match input.fill_buf().await {
            Ok([]) => {
                return Ok(if out.is_empty() && !oversize {
                    None
                } else {
                    Some(!oversize)
                });
            }
            Ok(chunk) => chunk,
            Err(error) => return Err(error),
        };
        let (taken, complete) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (chunk.len(), false),
        };
        if !oversize {
            if out.len() + taken > MAX_MESSAGE {
                oversize = true;
                out.clear();
            } else {
                out.extend_from_slice(&chunk[..taken]);
            }
        }
        input.consume(taken);
        if complete {
            return Ok(Some(!oversize));
        }
    }
}

/// One message in, at most one message out.
///
/// `None` for a notification, which by the specification gets no reply — and
/// for a request whose `id` we could not read, because a response without an
/// id is one the client cannot match to anything.
pub async fn handle(manager: &SandboxManager, line: &str) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                code::PARSE,
                &format!("invalid JSON: {e}"),
            ));
        }
    };

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        return Some(error_response(
            id,
            code::INVALID_REQUEST,
            "not a JSON-RPC request: no `method`",
        ));
    };
    // A response has an `id`; a notification does not, and answering one is a
    // protocol error rather than a courtesy.
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    let result = dispatch(manager, method, &params).await;
    let id = id?;

    Some(match result {
        Ok(Some(value)) => success_response(id, value),
        // A method that is only meaningful as a notification, called as a
        // request: an empty result is the specification's answer.
        Ok(None) => success_response(id, json!({})),
        Err(e) => error_response(id, e.code, &e.message),
    })
}

/// Everything that can go wrong, as the client will see it.
struct RpcError {
    code: i64,
    message: String,
}

fn invalid_params(message: impl Into<String>) -> RpcError {
    RpcError {
        code: code::INVALID_PARAMS,
        message: message.into(),
    }
}

async fn dispatch(
    manager: &SandboxManager,
    method: &str,
    params: &Value,
) -> std::result::Result<Option<Value>, RpcError> {
    match method {
        "initialize" => Ok(Some(initialize(params))),
        // Notifications. Acknowledged silently: the client is telling us
        // something, not asking.
        m if m.starts_with("notifications/") => Ok(None),
        "ping" => Ok(Some(json!({}))),
        "tools/list" => Ok(Some(json!({ "tools": tool_definitions() }))),
        "tools/call" => call_tool(manager, params).await.map(Some),
        // The two the specification defines but this server does not offer.
        // Answering "method not found" is what tells a client to stop asking.
        other => Err(RpcError {
            code: code::METHOD_NOT_FOUND,
            message: format!("devbox mcp self does not implement '{other}'"),
        }),
    }
}

/// The handshake.
///
/// The protocol version is *echoed* when we know it and replaced with our
/// latest when we do not — which is what the specification asks for, and what
/// keeps a client pinned to `2024-11-05` from being told a version it will
/// then refuse.
fn initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL);
    let version = if SUPPORTED_PROTOCOLS.contains(&asked) {
        asked
    } else {
        LATEST_PROTOCOL
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "devbox", "version": env!("CARGO_PKG_VERSION") },
        "instructions": concat!(
            "devbox records what every box and every run did: processes, files, ",
            "DNS, TLS names, bytes, and policy decisions. Use list_runs to find ",
            "a run, run_report for the full account of one, behavior_summary for ",
            "what a box has been doing lately, and watch for the raw events."
        ),
    })
}

/// The four tools, with the JSON Schema a client validates against.
pub fn tool_definitions() -> Vec<Value> {
    let box_property = json!({
        "type": "string",
        "description": "Box name. Defaults to the box for the current directory.",
    });
    vec![
        json!({
            "name": "list_runs",
            "description": "List a box's recorded runs, newest first: id, kind, \
                            status, exit code, duration and command.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "box": box_property,
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": RUNS_MAX,
                        "description": "How many runs to return (default 20).",
                    },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "run_report",
            "description": "The full report for one run: files changed, processes, \
                            network, credentials, policy violations, and how much \
                            of it the capture actually saw.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": {
                        "type": "string",
                        "description": "A run id, as list_runs returns.",
                    },
                    "format": {
                        "type": "string",
                        "enum": ["md", "json"],
                        "description": "Markdown to read, JSON to compute with (default md).",
                    },
                },
                "required": ["run_id"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "behavior_summary",
            "description": "What a box has been doing: the domains it reached, the \
                            files it wrote, the processes it ran, and any policy \
                            violations — across every run, not one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "box": box_property,
                    "since": {
                        "type": "string",
                        "description": "RFC 3339 lower bound, e.g. 2026-09-05T00:00:00Z.",
                    },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "watch",
            "description": "Raw events from a box's timeline, oldest first, with \
                            addresses labelled by the DNS answer that resolved them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "box": box_property,
                    "types": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": EventType::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                        },
                        "description": "Event kinds to include; all of them when omitted.",
                    },
                    "since": {
                        "type": "string",
                        "description": "RFC 3339 lower bound.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": WATCH_MAX,
                        "description": "How many events to return (default 50).",
                    },
                },
                "additionalProperties": false,
            },
        }),
    ]
}

async fn call_tool(
    manager: &SandboxManager,
    params: &Value,
) -> std::result::Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("tools/call needs a `name`"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // A tool that fails is *not* a JSON-RPC error: the call succeeded and the
    // tool reported a problem, which is the distinction `isError` exists for.
    // A client treats the first as "the server is broken" and the second as
    // "that did not work, try something else".
    let outcome = match name {
        "list_runs" => list_runs(manager, &arguments),
        "run_report" => run_report(manager, &arguments),
        "behavior_summary" => behavior_summary(manager, &arguments),
        "watch" => watch(manager, &arguments),
        other => {
            return Err(RpcError {
                code: code::METHOD_NOT_FOUND,
                message: format!("no tool named '{other}'"),
            });
        }
    };

    Ok(match outcome {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    })
}

/// The box a tool call is about: the argument, or the current directory's.
fn box_of(manager: &SandboxManager, arguments: &Value) -> std::result::Result<String, String> {
    let named = arguments.get("box").and_then(Value::as_str);
    let name = manager
        .resolve_name(named)
        .map_err(|e| format!("could not determine which box: {e}"))?;
    if !crate::sandbox::state::is_safe_name(&name) {
        return Err(format!("{name:?} is not a box name"));
    }
    Ok(name)
}

/// Open a box's event store, or say why there is nothing to read.
fn store_of(manager: &SandboxManager, name: &str) -> std::result::Result<Store, String> {
    let path = crate::obs::collector::store_path(&manager.state_dir, name);
    if !path.exists() {
        return Err(format!(
            "Box '{name}' has recorded nothing yet. Start it with a devbox command first."
        ));
    }
    Store::open(&path).map_err(|e| format!("could not open the event store for '{name}': {e}"))
}

fn usize_arg(arguments: &Value, key: &str, default: usize, max: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, max))
        .unwrap_or(default)
}

fn list_runs(manager: &SandboxManager, arguments: &Value) -> std::result::Result<String, String> {
    let name = box_of(manager, arguments)?;
    let store = store_of(manager, &name)?;
    let limit = usize_arg(arguments, "limit", 20, RUNS_MAX);
    let runs = store
        .list_runs(limit)
        .map_err(|e| format!("could not list runs: {e}"))?;
    if runs.is_empty() {
        return Ok(format!("Box '{name}' has no recorded runs."));
    }

    let mut out = format!("# Runs on '{name}'\n\n");
    for run in &runs {
        let duration = run
            .duration_ms()
            .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
            .unwrap_or_else(|| "—".to_string());
        let exit = run
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "- `{}` {} {} exit={} {} {}\n",
            run.run_id,
            run.kind,
            run.status,
            exit,
            duration,
            one_line(&run.command_line()),
        ));
        if !run.label.is_empty() {
            out.push_str(&format!("  label: {}\n", one_line(&run.label)));
        }
        if let Some(ended_by) = &run.ended_by
            && ended_by != "exit"
        {
            out.push_str(&format!("  ended by: {ended_by}\n"));
        }
    }
    Ok(out)
}

fn run_report(manager: &SandboxManager, arguments: &Value) -> std::result::Result<String, String> {
    let run_id = arguments
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "run_report needs a `run_id`".to_string())?;
    if !crate::obs::run::is_run_id(run_id) {
        return Err(format!(
            "'{run_id}' is not a run id. Run ids are 26 characters; list_runs returns them."
        ));
    }
    let named = arguments.get("box").and_then(Value::as_str);
    let (_, report) =
        crate::cli::report::locate(manager, run_id, named).map_err(|e| e.to_string())?;

    match arguments.get("format").and_then(Value::as_str) {
        Some("json") => {
            crate::report::json::render(&report).map_err(|e| format!("could not render: {e}"))
        }
        _ => Ok(crate::report::markdown::render(&report)),
    }
}

fn behavior_summary(
    manager: &SandboxManager,
    arguments: &Value,
) -> std::result::Result<String, String> {
    let name = box_of(manager, arguments)?;
    let store = store_of(manager, &name)?;
    let since = arguments.get("since").and_then(Value::as_str);
    let (events, truncated) = store
        .recent(since, Query::MAX_LIMIT, 64 * 1024 * 1024)
        .map_err(|e| format!("could not read the event store: {e}"))?;
    let summary = crate::obs::behavior::summarize(&name, &events);
    let mut out = crate::obs::behavior::render_markdown(&summary);
    // Stated, not implied (G4). A summary that silently covers part of a
    // window reports "no violations" for a box that had them.
    if truncated {
        let first = events
            .first()
            .map(|e| e.ts_wall.as_str())
            .unwrap_or("the cut-off point");
        out.push_str(&format!(
            "\n> This window holds more than one scan can read. The summary covers \
             the most recent events, back to {first}.\n"
        ));
    }
    Ok(out)
}

fn watch(manager: &SandboxManager, arguments: &Value) -> std::result::Result<String, String> {
    let name = box_of(manager, arguments)?;
    let store = store_of(manager, &name)?;
    let limit = usize_arg(arguments, "limit", 50, WATCH_MAX);

    let mut kinds = Vec::new();
    if let Some(types) = arguments.get("types").and_then(Value::as_array) {
        for entry in types {
            let raw = entry
                .as_str()
                .ok_or_else(|| "each entry in `types` must be a string".to_string())?;
            kinds.push(
                raw.parse::<EventType>()
                    .map_err(|_| format!("unknown event type '{raw}'"))?,
            );
        }
    }

    let mut events = store
        .query(&Query {
            since: arguments
                .get("since")
                .and_then(Value::as_str)
                .map(str::to_string),
            kinds,
            limit: Some(limit),
            // Newest first so the limit keeps the *recent* events; reversed
            // below so the answer reads forwards.
            newest_first: true,
            ..Default::default()
        })
        .map_err(|e| format!("could not read the event store: {e}"))?;

    // Label addresses with the name that resolved them, exactly as
    // `devbox watch` does — an agent reading `pypi.org` can act on it, and one
    // reading `151.101.0.223` cannot.
    let map = crate::obs::correlate::dns_map(&events);
    crate::obs::correlate::apply_dns_map(&mut events, &map);
    events.reverse();

    if events.is_empty() {
        return Ok(format!("No events matched on box '{name}'."));
    }
    let mut out = String::new();
    for event in &events {
        out.push_str(&line_for(event));
        out.push('\n');
    }
    out.push_str(&format!("\n{} event(s).", events.len()));
    Ok(out)
}

/// One event, in the same shape `devbox watch` prints.
fn line_for(event: &Event) -> String {
    format!(
        "{}  {:<8} pid={:<6} {}",
        event.ts_wall,
        event.kind.to_string(),
        event.pid,
        one_line(&event.summary()),
    )
}

/// Collapse a value onto one line.
///
/// Tool output is text a model reads; a command containing a newline would
/// otherwise turn one row of a list into two that look like separate entries.
fn one_line(value: &str) -> String {
    value.replace('\n', "\\n").replace('\r', "\\r")
}

fn success_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (tempfile::TempDir, SandboxManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager {
            state_dir: dir.path().to_path_buf(),
        };
        (dir, manager)
    }

    async fn ask(manager: &SandboxManager, request: Value) -> Value {
        let raw = handle(manager, &request.to_string())
            .await
            .expect("a request gets a response");
        serde_json::from_str(&raw).expect("the response is JSON")
    }

    #[tokio::test]
    async fn initialize_echoes_a_protocol_version_it_knows() {
        let (_dir, manager) = manager();
        for asked in SUPPORTED_PROTOCOLS {
            let response = ask(
                &manager,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                       "params":{"protocolVersion":asked,"capabilities":{},
                                 "clientInfo":{"name":"t","version":"0"}}}),
            )
            .await;
            assert_eq!(response["result"]["protocolVersion"], *asked);
            assert_eq!(response["result"]["serverInfo"]["name"], "devbox");
        }
    }

    /// ...and substitutes its own for one it does not, rather than echoing a
    /// version it cannot actually speak.
    #[tokio::test]
    async fn an_unknown_protocol_version_is_answered_with_ours() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                   "params":{"protocolVersion":"1999-01-01"}}),
        )
        .await;
        assert_eq!(response["result"]["protocolVersion"], LATEST_PROTOCOL);
    }

    #[tokio::test]
    async fn a_notification_gets_no_reply() {
        let (_dir, manager) = manager();
        assert!(
            handle(
                &manager,
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string()
            )
            .await
            .is_none(),
            "answering a notification is a protocol error"
        );
    }

    #[tokio::test]
    async fn tools_list_describes_all_four_with_schemas() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let tools = response["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["list_runs", "run_report", "behavior_summary", "watch"]
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(
                tool["description"].as_str().is_some_and(|d| d.len() > 20),
                "{tool}"
            );
            // The descriptions are what a model chooses a tool from, so a
            // rustfmt-mangled continuation would be user-visible here too.
            let text = tool["description"].as_str().unwrap();
            assert!(!text.contains("  "), "double space in {text:?}");
        }
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["run_id"]));
    }

    #[tokio::test]
    async fn ping_is_answered_empty() {
        let (_dir, manager) = manager();
        let response = ask(&manager, json!({"jsonrpc":"2.0","id":3,"method":"ping"})).await;
        assert_eq!(response["result"], json!({}));
    }

    #[tokio::test]
    async fn an_unknown_method_is_an_error_object_not_a_crash() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}),
        )
        .await;
        assert_eq!(response["error"]["code"], code::METHOD_NOT_FOUND);
        assert!(response.get("result").is_none());
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error_and_the_session_survives() {
        let (_dir, manager) = manager();
        let raw = handle(&manager, "{not json").await.unwrap();
        let response: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(response["error"]["code"], code::PARSE);
        // And the next request is still served.
        let next = ask(&manager, json!({"jsonrpc":"2.0","id":5,"method":"ping"})).await;
        assert_eq!(next["result"], json!({}));
    }

    #[tokio::test]
    async fn a_message_with_no_method_is_an_invalid_request() {
        let (_dir, manager) = manager();
        let response = ask(&manager, json!({"jsonrpc":"2.0","id":6,"result":{}})).await;
        assert_eq!(response["error"]["code"], code::INVALID_REQUEST);
    }

    /// A tool that cannot answer reports `isError`, not a JSON-RPC error: the
    /// call worked, the answer is "there is nothing there".
    #[tokio::test]
    async fn a_box_with_no_store_is_a_tool_error_not_a_protocol_error() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                   "params":{"name":"list_runs","arguments":{"box":"nosuchbox"}}}),
        )
        .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("nosuchbox"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_method_not_found() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call",
                   "params":{"name":"rm_rf","arguments":{}}}),
        )
        .await;
        assert_eq!(response["error"]["code"], code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_run_id_that_is_not_one_is_refused_before_any_box_is_searched() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
                   "params":{"name":"run_report","arguments":{"run_id":"../../etc/passwd"}}}),
        )
        .await;
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not a run id"), "{text}");
    }

    #[tokio::test]
    async fn a_box_name_that_is_a_path_is_refused() {
        let (_dir, manager) = manager();
        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":10,"method":"tools/call",
                   "params":{"name":"watch","arguments":{"box":"../../tmp/evil"}}}),
        )
        .await;
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not a box name"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_event_type_names_itself_rather_than_returning_nothing() {
        let (_dir, manager) = manager();
        std::fs::create_dir_all(manager.state_dir.join("boxes")).unwrap();
        let path = crate::obs::collector::store_path(&manager.state_dir, "b");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Store::open(&path).unwrap();

        let response = ask(
            &manager,
            json!({"jsonrpc":"2.0","id":11,"method":"tools/call",
                   "params":{"name":"watch","arguments":{"box":"b","types":["nonsense"]}}}),
        )
        .await;
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("nonsense"), "{text}");
    }

    #[test]
    fn every_event_type_is_offered_to_the_client() {
        let tools = tool_definitions();
        let watch = tools.iter().find(|t| t["name"] == "watch").unwrap();
        let offered = watch["inputSchema"]["properties"]["types"]["items"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(
            offered.len(),
            EventType::ALL.len(),
            "the schema and the parser disagree about which event types exist"
        );
    }
}
