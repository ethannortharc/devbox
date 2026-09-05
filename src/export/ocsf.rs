//! OCSF 1.3 rendering — the §8 mapping table, made executable.
//!
//! Every field here was checked against the schema server rather than
//! remembered: the required-attribute list of each class came from
//! `GET https://schema.ocsf.io/api/1.3.0/classes/<name>?profiles=`, and each
//! rendered record was checked with
//! `POST https://schema.ocsf.io/1.3.0/api/v2/validate`. The two rules that
//! shape the whole file:
//!
//! * `category_uid = class_uid / 1000` and `type_uid = class_uid * 100 +
//!   activity_id`. Both are OCSF arithmetic, not devbox convention.
//! * a required attribute of a *nested* object is enforced too, which is why
//!   `file.type_id`, `tls.version`, `finding_info.uid` and
//!   `connection_info.direction_id` all appear even where devbox has nothing
//!   better than "unknown" to say.
//!
//! What is deliberately *not* here: a class for `syscall`, which §8's table
//! does not map. It is counted as unmapped and left out, because the honest
//! alternative — filing it under some neighbouring class — puts a claim into
//! an audit record that nothing observed.

use serde_json::{Map, Value, json};

use super::{Context, basename, epoch_millis};
use crate::obs::event::{Event, EventType, Net};

/// The schema version every record declares in `metadata.version`.
pub const SCHEMA_VERSION: &str = "1.3.0";

/// File System Activity.
pub const CLASS_FILE_ACTIVITY: i64 = 1001;
/// Process Activity.
pub const CLASS_PROCESS_ACTIVITY: i64 = 1007;
/// Detection Finding.
pub const CLASS_DETECTION_FINDING: i64 = 2004;
/// Network Activity.
pub const CLASS_NETWORK_ACTIVITY: i64 = 4001;
/// HTTP Activity.
pub const CLASS_HTTP_ACTIVITY: i64 = 4002;
/// DNS Activity.
pub const CLASS_DNS_ACTIVITY: i64 = 4003;
/// API Activity — reserved for the `credential` event type, which component B
/// adds. Nothing maps to it yet; see [`classify`].
pub const CLASS_API_ACTIVITY: i64 = 6003;

/// `severity_id` — OCSF's scale, of which this mapping uses the bottom four.
const SEV_INFORMATIONAL: i64 = 1;
const SEV_MEDIUM: i64 = 3;
const SEV_HIGH: i64 = 4;

/// `device.type_id = 6`, Virtual. A devbox box is a VM on every runtime.
const DEVICE_TYPE_VIRTUAL: i64 = 6;

/// `file.type_id` — 0 Unknown, 1 Regular File.
///
/// An `exec` names something the kernel just ran, so "regular file" is
/// observed. A `file` event names a path an eBPF hook saw opened, which may be
/// a directory, a device, or a symlink; claiming "regular file" there would be
/// a guess written into evidence.
const FILE_TYPE_UNKNOWN: i64 = 0;
const FILE_TYPE_REGULAR: i64 = 1;

/// `connection_info.direction_id` — 0 Unknown, 1 Inbound, 2 Outbound.
const DIRECTION_UNKNOWN: i64 = 0;
const DIRECTION_INBOUND: i64 = 1;
const DIRECTION_OUTBOUND: i64 = 2;

/// Which OCSF class and activity an event belongs to, or `None` when this
/// build has no mapping for it.
///
/// The catch-all arm is load-bearing, not defensive: `EventType` is a shared
/// contract that other tracks extend, and an export that panicked on a type it
/// had not been taught would take down the one command an operator runs when
/// something has already gone wrong. `Close` arrived that way and cost one
/// line. The type still to come:
///
/// * `Credential` (broker use) → `(CLASS_API_ACTIVITY, from the method)` —
///   API Activity, with `actor.session.uid = run_id`. Note that
///   `api_activity` has no `device` attribute, so [`base`] must drop it for
///   that class.
///
/// Add the arm above the `_`; nothing else in this file needs to change.
pub fn classify(event: &Event) -> Option<(i64, i64)> {
    let activity = match event.kind {
        EventType::Exec => (CLASS_PROCESS_ACTIVITY, 1), // Launch
        EventType::Exit => (CLASS_PROCESS_ACTIVITY, 2), // Terminate
        EventType::File => (CLASS_FILE_ACTIVITY, file_activity_id(event)),
        EventType::Connect | EventType::Accept | EventType::Tls => (CLASS_NETWORK_ACTIVITY, 1), // Open
        EventType::Close => (CLASS_NETWORK_ACTIVITY, 2), // Close
        EventType::Dns => (
            CLASS_DNS_ACTIVITY,
            match event.net.as_ref().is_some_and(|n| n.response) {
                true => 2,  // Response
                false => 1, // Query
            },
        ),
        EventType::Api => (CLASS_HTTP_ACTIVITY, http_activity_id(event)),
        EventType::Policy => (CLASS_DETECTION_FINDING, 1), // Create
        // `syscall` has no class in §8's table, and neither does any type this
        // build has not been taught. Counted, not guessed at.
        _ => return None,
    };
    Some(activity)
}

/// File System Activity `activity_id` from the event's `op`.
///
/// An `op` this build does not recognise is "Other", not unmapped: the class is
/// still File System Activity and the path is still evidence.
fn file_activity_id(event: &Event) -> i64 {
    let op = event.file.as_ref().map(|f| f.op.as_str()).unwrap_or("");
    match op {
        "create" => 1,
        "read" => 2,
        "write" | "update" | "modify" => 3,
        "delete" | "unlink" => 4,
        "rename" | "move" => 5,
        "open" => 14,
        _ => 99,
    }
}

/// HTTP Activity `activity_id` from the request method.
fn http_activity_id(event: &Event) -> i64 {
    let method = event.api.as_ref().map(|a| a.method.as_str()).unwrap_or("");
    match method.to_ascii_uppercase().as_str() {
        "CONNECT" => 1,
        "DELETE" => 2,
        "GET" => 3,
        "HEAD" => 4,
        "OPTIONS" => 5,
        "POST" => 6,
        "PUT" => 7,
        "TRACE" => 8,
        _ => 99,
    }
}

/// Render one event as an OCSF 1.3 object, or `None` when it has no class.
pub fn render(event: &Event, ctx: &Context) -> Option<Value> {
    let (class_uid, activity_id) = classify(event)?;
    let mut out = base(event, ctx, class_uid, activity_id);

    match class_uid {
        CLASS_PROCESS_ACTIVITY => process_activity(event, &mut out),
        CLASS_FILE_ACTIVITY => file_activity(event, &mut out),
        CLASS_NETWORK_ACTIVITY => network_activity(event, &mut out),
        CLASS_DNS_ACTIVITY => dns_activity(event, &mut out),
        CLASS_HTTP_ACTIVITY => http_activity(event, &mut out),
        CLASS_DETECTION_FINDING => detection_finding(event, &mut out),
        _ => {}
    }
    Some(Value::Object(out))
}

/// The attributes every class in this mapping shares.
fn base(event: &Event, ctx: &Context, class_uid: i64, activity_id: i64) -> Map<String, Value> {
    let mut metadata = json!({
        "version": SCHEMA_VERSION,
        "product": {
            "name": "devbox",
            "vendor_name": "devbox",
            "version": ctx.product_version,
        },
    });
    // Not decoration. On the network, DNS, HTTP and finding classes `actor`
    // and `device` are contributed by the `host` profile, and a record that
    // uses a profile attribute without declaring the profile is rejected —
    // "Unknown attribute at \"device\"", which is what the schema server said
    // about the first draft of this mapping.
    if let Some(profiles) = profiles_for(class_uid) {
        metadata["profiles"] = json!(profiles);
    }
    // Present only once runs exist. An empty correlation id would say
    // "correlated with nothing", which is a different claim from "not yet
    // correlated" — and this export is evidence.
    if let Some(run_id) = &ctx.run_id {
        metadata["correlation_uid"] = json!(run_id);
    }

    let mut out = Map::new();
    out.insert("class_uid".into(), json!(class_uid));
    // OCSF arithmetic, both of them: a class id encodes its category, and a
    // type id encodes its class and activity.
    out.insert("category_uid".into(), json!(class_uid / 1000));
    out.insert("activity_id".into(), json!(activity_id));
    out.insert("type_uid".into(), json!(type_uid(class_uid, activity_id)));
    out.insert("severity_id".into(), json!(severity_id(event)));
    out.insert("time".into(), json!(epoch_millis(&event.ts_wall)));
    out.insert("message".into(), json!(event.summary()));
    out.insert("metadata".into(), metadata);
    // `api_activity` is the one class in this mapping with no `device`
    // attribute; emitting it there would be an unknown attribute.
    if class_uid != CLASS_API_ACTIVITY {
        out.insert(
            "device".into(),
            json!({
                "type_id": DEVICE_TYPE_VIRTUAL,
                "hostname": ctx.box_name,
                "name": ctx.box_name,
            }),
        );
    }
    out.insert("actor".into(), json!({ "process": actor_process(event) }));
    out
}

/// The OCSF profiles a class needs declared before it may carry `actor` and
/// `device`.
///
/// `process_activity` and `file_activity` define both in their own core, so
/// they declare nothing. `api_activity` has `actor` in core and no `device` at
/// all. Everything else inherits them from `host`.
fn profiles_for(class_uid: i64) -> Option<&'static [&'static str]> {
    match class_uid {
        CLASS_NETWORK_ACTIVITY
        | CLASS_HTTP_ACTIVITY
        | CLASS_DNS_ACTIVITY
        | CLASS_DETECTION_FINDING => Some(&["host"]),
        _ => None,
    }
}

/// `type_uid = class_uid * 100 + activity_id`.
pub fn type_uid(class_uid: i64, activity_id: i64) -> i64 {
    class_uid * 100 + activity_id
}

/// Informational for everything a box does, unless a policy said otherwise.
fn severity_id(event: &Event) -> i64 {
    match event.policy.as_ref().map(|p| p.verdict.as_str()) {
        Some("block") => SEV_HIGH,
        Some("flag") => SEV_MEDIUM,
        _ => SEV_INFORMATIONAL,
    }
}

/// The process the event is attributed to.
///
/// For every class but Process Activity that is the event's own pid: the
/// process that opened the file, made the connection, or asked the resolver.
/// `exec` is the exception — see [`process_activity`].
fn actor_process(event: &Event) -> Value {
    match event.kind {
        // The actor of a launch is the parent; the launched process is the
        // `process` attribute.
        EventType::Exec => {
            let mut p = Map::new();
            p.insert("pid".into(), json!(event.ppid));
            Value::Object(p)
        }
        _ => Value::Object(process_object(event, false)),
    }
}

/// `pid`, `name`, `tid`, and the user, from the envelope.
fn process_object(event: &Event, with_parent: bool) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("pid".into(), json!(event.pid));
    if !event.comm.is_empty() {
        p.insert("name".into(), json!(event.comm));
    }
    if event.tid != 0 {
        p.insert("tid".into(), json!(event.tid));
    }
    // OCSF `user.uid` is a string; a POSIX uid is the value it carries here.
    p.insert("user".into(), json!({ "uid": event.uid.to_string() }));
    if with_parent && event.ppid != 0 {
        p.insert("parent_process".into(), json!({ "pid": event.ppid }));
    }
    p
}

fn process_activity(event: &Event, out: &mut Map<String, Value>) {
    let mut process = process_object(event, true);
    if let Some(exec) = &event.exec {
        if !exec.path.is_empty() {
            process.insert(
                "file".into(),
                json!({
                    "name": basename(&exec.path),
                    "path": exec.path,
                    // Observed: the kernel ran it.
                    "type_id": FILE_TYPE_REGULAR,
                }),
            );
            if !process.contains_key("name") {
                process.insert("name".into(), json!(basename(&exec.path)));
            }
        }
        if !exec.argv.is_empty() {
            process.insert("cmd_line".into(), json!(exec.argv.join(" ")));
        }
        if !exec.cwd.is_empty() {
            unmapped(out).insert("cwd".into(), json!(exec.cwd));
        }
    }
    out.insert("process".into(), Value::Object(process));
}

fn file_activity(event: &Event, out: &mut Map<String, Value>) {
    let Some(file) = &event.file else { return };
    let mut f = Map::new();
    f.insert("name".into(), json!(basename(&file.path)));
    f.insert("path".into(), json!(file.path));
    // Not `FILE_TYPE_REGULAR`: the hook saw a path, not a stat.
    f.insert("type_id".into(), json!(FILE_TYPE_UNKNOWN));
    out.insert("file".into(), Value::Object(f));

    if !file.op.is_empty() {
        unmapped(out).insert("op".into(), json!(file.op));
    }
    if file.flags != 0 {
        unmapped(out).insert("flags".into(), json!(file.flags));
    }
}

/// The peer, by address and — when one was observed — by name.
///
/// The name goes in as well as the address because the name is what an analyst
/// searches for; `151.101.0.223` is not what anyone remembers about pypi.
fn dst_endpoint(net: &Net) -> Map<String, Value> {
    let mut dst = Map::new();
    if !net.daddr.is_empty() {
        dst.insert("ip".into(), json!(net.daddr));
    }
    if net.dport != 0 {
        dst.insert("port".into(), json!(net.dport));
    }
    if let Some(name) = first_non_empty(&[&net.domain, &net.sni]) {
        dst.insert("hostname".into(), json!(name));
        dst.insert("domain".into(), json!(name));
    }
    dst
}

/// The local side, when the capture recorded one.
fn src_endpoint(net: &Net) -> Option<Map<String, Value>> {
    let mut src = Map::new();
    if !net.saddr.is_empty() {
        src.insert("ip".into(), json!(net.saddr));
    }
    if net.sport != 0 {
        src.insert("port".into(), json!(net.sport));
    }
    (!src.is_empty()).then_some(src)
}

/// Direction and protocol.
///
/// A `close` does not say which way it went in its type, so it carries `dir`
/// instead — and carries it empty exactly when the connection was opened
/// before the agent was watching. That case is `Unknown`, not a guess at
/// `Outbound`: the whole point of the orphan flag is that this connection's
/// origin was never observed.
fn connection_info(event: &Event, net: &Net) -> Map<String, Value> {
    let direction = match net.dir.as_str() {
        "in" => DIRECTION_INBOUND,
        "out" => DIRECTION_OUTBOUND,
        _ => match event.kind {
            EventType::Accept => DIRECTION_INBOUND,
            EventType::Close => DIRECTION_UNKNOWN,
            _ => DIRECTION_OUTBOUND,
        },
    };
    let mut conn = Map::new();
    conn.insert("direction_id".into(), json!(direction));
    if !net.proto.is_empty() {
        conn.insert("protocol_name".into(), json!(net.proto));
    }
    conn
}

fn network_activity(event: &Event, out: &mut Map<String, Value>) {
    let Some(net) = &event.net else { return };

    // Required by the class.
    out.insert("dst_endpoint".into(), Value::Object(dst_endpoint(net)));
    if let Some(src) = src_endpoint(net) {
        out.insert("src_endpoint".into(), Value::Object(src));
    }
    out.insert(
        "connection_info".into(),
        Value::Object(connection_info(event, net)),
    );

    // Settled on `close` and nowhere else — a `connect` carrying a byte count
    // would be carrying a guess.
    if net.bytes_tx != 0 || net.bytes_rx != 0 {
        out.insert(
            "traffic".into(),
            json!({
                "bytes_out": net.bytes_tx,
                "bytes_in": net.bytes_rx,
                "bytes": net.bytes_tx + net.bytes_rx,
            }),
        );
    }
    if net.dur_ms != 0 {
        out.insert("duration".into(), json!(net.dur_ms));
    }
    if net.orphan {
        // The bytes are real; the process on this record is whoever closed the
        // socket, not whoever opened it. A reader adding them to that process's
        // total would be wrong, so the record says so rather than leaving it to
        // be inferred from an empty direction.
        unmapped(out).insert("orphan".into(), json!(true));
    }

    if event.kind == EventType::Tls {
        let mut tls = Map::new();
        // Required by the schema and not observed: the agent reports the SNI
        // and the ALPN out of the ClientHello, never the negotiated version.
        // "Unknown" is the honest filler; when the agent starts reporting a
        // version, this is the line to change.
        tls.insert("version".into(), json!("Unknown"));
        if !net.sni.is_empty() {
            tls.insert("sni".into(), json!(net.sni));
        }
        if !net.alpn.is_empty() {
            // ALPN has no attribute of its own on the `tls` object; it is TLS
            // extension 16, which does have one.
            tls.insert(
                "tls_extension_list".into(),
                json!([{
                    "type_id": 16,
                    "type": "application_layer_protocol_negotiation",
                    "data": net.alpn,
                }]),
            );
        }
        out.insert("tls".into(), Value::Object(tls));
    }
}

fn dns_activity(event: &Event, out: &mut Map<String, Value>) {
    let Some(net) = &event.net else { return };

    if !net.qname.is_empty() {
        let mut q = Map::new();
        q.insert("hostname".into(), json!(net.qname));
        if !net.qtype.is_empty() {
            q.insert("type".into(), json!(net.qtype));
        }
        out.insert("query".into(), Value::Object(q));
    }
    if !net.answers.is_empty() {
        out.insert(
            "answers".into(),
            Value::Array(
                net.answers
                    .iter()
                    .map(|a| {
                        let mut ans = json!({ "rdata": a });
                        if !net.qtype.is_empty() {
                            ans["type"] = json!(net.qtype);
                        }
                        ans
                    })
                    .collect(),
            ),
        );
    }

    // Unlike Network Activity, DNS Activity does not require an endpoint, so
    // these go in only when the resolver exchange was actually observed.
    let dst = dst_endpoint(net);
    if !dst.is_empty() {
        out.insert("dst_endpoint".into(), Value::Object(dst));
    }
    if let Some(src) = src_endpoint(net) {
        out.insert("src_endpoint".into(), Value::Object(src));
    }
    if !net.proto.is_empty() {
        out.insert(
            "connection_info".into(),
            Value::Object(connection_info(event, net)),
        );
    }
}

fn http_activity(event: &Event, out: &mut Map<String, Value>) {
    let Some(api) = &event.api else { return };

    let mut request = Map::new();
    if !api.method.is_empty() {
        request.insert("http_method".into(), json!(api.method.to_ascii_uppercase()));
    }
    let mut url = Map::new();
    if !api.host.is_empty() {
        url.insert("hostname".into(), json!(api.host));
    }
    if !api.path.is_empty() {
        url.insert("path".into(), json!(api.path));
    }
    if !api.host.is_empty() {
        // The capture is of TLS-terminated API traffic; the scheme is the one
        // thing about the URL that is not in doubt.
        url.insert("scheme".into(), json!("https"));
        url.insert(
            "url_string".into(),
            json!(format!("https://{}{}", api.host, api.path)),
        );
    }
    if !url.is_empty() {
        request.insert("url".into(), Value::Object(url));
    }
    out.insert("http_request".into(), Value::Object(request));
    // `code` is required. Zero is what the capture has when it saw a request
    // and no response, and it reads as "no status" rather than as a status.
    out.insert("http_response".into(), json!({ "code": api.status }));

    out.insert(
        "dst_endpoint".into(),
        json!({ "hostname": api.host, "domain": api.host }),
    );

    if api.tokens != 0 {
        unmapped(out).insert("tokens".into(), json!(api.tokens));
    }
    if !api.endpoint.is_empty() {
        unmapped(out).insert("endpoint".into(), json!(api.endpoint));
    }
}

fn detection_finding(event: &Event, out: &mut Map<String, Value>) {
    let Some(policy) = &event.policy else { return };

    let target = match policy.target.is_empty() {
        true => "(no target)",
        false => policy.target.as_str(),
    };
    let mut info = Map::new();
    info.insert(
        "title".into(),
        json!(format!("egress policy {}: {target}", policy.verdict)),
    );
    // Required, and it has to be stable: the same decision exported twice is
    // one finding, not two. `ts_mono_ns` is the box's total order, so box plus
    // monotonic timestamp identifies the decision without a random id that
    // would change on every export.
    info.insert(
        "uid".into(),
        json!(format!(
            "devbox:{}:policy:{}",
            event.box_id, event.ts_mono_ns
        )),
    );
    if !policy.reason.is_empty() {
        info.insert("desc".into(), json!(policy.reason));
    }
    info.insert(
        "types".into(),
        json!([format!("egress-policy-{}", policy.verdict)]),
    );
    out.insert("finding_info".into(), Value::Object(info));

    // The connection the verdict was about. Without it the finding says a
    // block happened and not what was blocked — the `net` sub-object is on the
    // event, and `evidences` is where OCSF puts exactly this.
    if let Some(net) = &event.net {
        let mut artifact = Map::new();
        let dst = dst_endpoint(net);
        if !dst.is_empty() {
            artifact.insert("dst_endpoint".into(), Value::Object(dst));
        }
        if let Some(src) = src_endpoint(net) {
            artifact.insert("src_endpoint".into(), Value::Object(src));
        }
        if !net.proto.is_empty() {
            artifact.insert(
                "connection_info".into(),
                Value::Object(connection_info(event, net)),
            );
        }
        artifact.insert(
            "process".into(),
            Value::Object(process_object(event, false)),
        );
        out.insert("evidences".into(), json!([Value::Object(artifact)]));
    }

    let u = unmapped(out);
    u.insert("verdict".into(), json!(policy.verdict));
    u.insert("mode".into(), json!(policy.mode));
    if !policy.target.is_empty() {
        u.insert("target".into(), json!(policy.target));
    }
}

/// The `unmapped` bag, created on first use.
///
/// OCSF's own answer to "this source has a field the schema does not": keep it,
/// namespaced under the event, rather than dropping it or bending an unrelated
/// attribute to hold it.
fn unmapped(out: &mut Map<String, Value>) -> &mut Map<String, Value> {
    out.entry("unmapped")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("unmapped is an object")
}

fn first_non_empty<'a>(candidates: &[&'a String]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|s| s.as_str())
        .find(|s| !s.is_empty())
}
