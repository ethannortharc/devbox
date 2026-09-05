//! `devbox export` — the mapping, the accounting, and the two wire formats.
//!
//! The goldens are the contract. A mapping change that is deliberate shows up
//! as a reviewable diff in `tests/fixtures/export/`; one that is accidental
//! shows up as a failure. Regenerate them with `UPDATE_GOLDEN=1 cargo test
//! --test export` and read the diff before committing it.
//!
//! `tests/fixtures/export/events.jsonl` is a copy of the cross-language
//! fixture rather than a reference to it, so that another track adding an
//! event type changes exactly one test — [`the_shared_contract_maps_or_counts`],
//! which is written to tolerate it — instead of breaking every golden.

use std::path::{Path, PathBuf};

use devbox::export::{self, Format, Window};
use devbox::obs::event::{Event, EventType};
use devbox::obs::store::Store;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export")
}

fn read_events(path: &Path) -> Vec<Event> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line {} decodes: {e}", i + 1))
        })
        .collect()
}

fn events() -> Vec<Event> {
    read_events(&fixture_dir().join("events.jsonl"))
}

/// A context with a pinned version, so bumping the crate version does not
/// rewrite every golden.
fn ctx() -> export::Context {
    export::Context {
        box_name: "myapp".into(),
        product_version: "0.0.0-test".into(),
        run_id: None,
    }
}

fn store_with(events: &[Event]) -> Store {
    let store = Store::open_in_memory().unwrap();
    for e in events {
        store.insert(e).unwrap();
    }
    store
}

fn export_to_string(store: &Store, window: &Window, format: Format) -> (String, export::Stats) {
    let mut out: Vec<u8> = Vec::new();
    let stats = export::run(store, window, &ctx(), format, &mut out).unwrap();
    (String::from_utf8(out).unwrap(), stats)
}

/// Compare against a golden, or rewrite it under `UPDATE_GOLDEN=1`.
fn golden(rel: &str, actual: &Value) {
    let path = fixture_dir().join(rel);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
        std::fs::write(&path, text).unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "golden {} is missing ({e}); regenerate with UPDATE_GOLDEN=1 and review the diff",
            path.display()
        )
    });
    let want: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(actual, &want, "golden {rel} differs");
}

// ---------------------------------------------------------------- goldens

#[test]
fn every_mapped_event_type_renders_its_golden_ocsf() {
    let ctx = ctx();
    let mut rendered = 0;
    for event in events() {
        match devbox::export::ocsf::render(&event, &ctx) {
            Some(value) => {
                golden(&format!("ocsf/{}.json", event.kind.as_str()), &value);
                rendered += 1;
            }
            None => assert_eq!(
                event.kind,
                EventType::Syscall,
                "{} has no OCSF class; §8's table maps everything but syscall",
                event.kind
            ),
        }
    }
    assert_eq!(rendered, 9, "nine of the ten fixture types map to a class");
}

#[test]
fn every_event_type_renders_its_golden_otlp() {
    let ctx = ctx();
    for event in events() {
        let value = devbox::export::otlp::log_record(&event, &ctx);
        golden(&format!("otlp/{}.json", event.kind.as_str()), &value);
    }
}

/// The one arithmetic rule OCSF consumers key off: a type id names its class
/// and its activity. Asserted against the goldens themselves, so a hand-edited
/// golden cannot smuggle an inconsistent record in.
#[test]
fn type_uid_is_class_uid_times_a_hundred_plus_activity_id() {
    let ctx = ctx();
    let mut checked = 0;
    for event in events() {
        let Some(value) = devbox::export::ocsf::render(&event, &ctx) else {
            continue;
        };
        let class_uid = value["class_uid"].as_i64().unwrap();
        let activity_id = value["activity_id"].as_i64().unwrap();
        let type_uid = value["type_uid"].as_i64().unwrap();
        assert_eq!(
            type_uid,
            class_uid * 100 + activity_id,
            "{}: type_uid {type_uid} does not decompose into {class_uid}/{activity_id}",
            event.kind
        );
        // The other OCSF identity: a class id encodes its category.
        assert_eq!(
            value["category_uid"].as_i64().unwrap(),
            class_uid / 1000,
            "{}: category_uid does not match its class",
            event.kind
        );
        checked += 1;
    }
    assert_eq!(checked, 9);
}

/// The §8 table, asserted as a table.
#[test]
fn the_mapping_table_is_the_one_in_the_design() {
    use devbox::export::ocsf::classify;
    let by_kind: std::collections::BTreeMap<EventType, (i64, i64)> = events()
        .iter()
        .filter_map(|e| classify(e).map(|c| (e.kind, c)))
        .collect();

    assert_eq!(
        by_kind[&EventType::Exec],
        (1007, 1),
        "Process Activity / Launch"
    );
    assert_eq!(
        by_kind[&EventType::Exit],
        (1007, 2),
        "Process Activity / Terminate"
    );
    assert_eq!(
        by_kind[&EventType::File],
        (1001, 3),
        "File System Activity / Update, from op=write"
    );
    assert_eq!(
        by_kind[&EventType::Connect],
        (4001, 1),
        "Network Activity / Open"
    );
    assert_eq!(
        by_kind[&EventType::Accept],
        (4001, 1),
        "Network Activity / Open"
    );
    assert_eq!(
        by_kind[&EventType::Tls],
        (4001, 1),
        "Network Activity / Open"
    );
    assert_eq!(
        by_kind[&EventType::Dns],
        (4003, 2),
        "DNS Activity / Response, from response=true"
    );
    assert_eq!(
        by_kind[&EventType::Api],
        (4002, 6),
        "HTTP Activity / Post, from method"
    );
    assert_eq!(
        by_kind[&EventType::Policy],
        (2004, 1),
        "Detection Finding / Create"
    );
    assert!(
        !by_kind.contains_key(&EventType::Syscall),
        "syscall has no class"
    );
}

// ------------------------------------------------------------- accounting

#[test]
fn counts_balance_in_every_format() {
    let events = events();
    let store = store_with(&events);
    let window = Window::default();

    for format in [Format::Jsonl, Format::Ocsf, Format::OtlpJson] {
        let (_, stats) = export_to_string(&store, &window, format);
        assert!(
            stats.balances(),
            "{}: {} matched != {} written + {} unmapped",
            format.as_str(),
            stats.matched,
            stats.written,
            stats.unmapped
        );
        assert_eq!(stats.scanned, events.len() as u64, "{}", format.as_str());
        assert_eq!(stats.matched, events.len() as u64, "{}", format.as_str());
    }
}

#[test]
fn ocsf_drops_only_the_types_it_has_no_class_for_and_says_which() {
    let events = events();
    let store = store_with(&events);
    let (text, stats) = export_to_string(&store, &Window::default(), Format::Ocsf);

    assert_eq!(stats.written, 9);
    assert_eq!(stats.unmapped, 1);
    assert_eq!(stats.unmapped_summary(), "syscall=1");
    assert_eq!(
        text.lines().count(),
        9,
        "one line per written record, and no blank tail"
    );
    for line in text.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(v["class_uid"].is_i64());
    }
}

#[test]
fn jsonl_is_lossless_and_writes_every_row() {
    let events = events();
    let store = store_with(&events);
    let (text, stats) = export_to_string(&store, &Window::default(), Format::Jsonl);

    assert_eq!(stats.unmapped, 0, "jsonl has nothing to be unable to map");
    assert_eq!(stats.written, events.len() as u64);

    let back: Vec<Event> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(back, events, "jsonl round trip changed an event");
}

// ----------------------------------------------------------------- window

#[test]
fn the_window_selects_on_ts_wall_with_an_exclusive_upper_bound() {
    let events = events();
    let store = store_with(&events);

    // The fixture runs 22:14:07.412 .. 22:14:10.000.
    let window = Window::parse(
        Some("2026-08-06T22:14:08.000Z"),
        Some("2026-08-06T22:14:09.310Z"),
    )
    .unwrap();
    let (text, stats) = export_to_string(&store, &window, Format::Jsonl);

    assert_eq!(
        stats.scanned,
        events.len() as u64,
        "the cursor still reads the store"
    );
    let kept: Vec<Event> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = kept.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["accept", "file", "syscall", "api"],
        "the lower bound is inclusive and the upper bound excludes the 09.310 policy event"
    );
    assert_eq!(stats.matched, 4);
}

#[test]
fn the_window_accepts_any_rfc_3339_offset_and_rejects_anything_else() {
    // 21:14 in UTC+1 is 20:14 UTC, which is before the whole fixture.
    let window = Window::parse(Some("2026-08-06T23:14:08.000+01:00"), None).unwrap();
    assert_eq!(window.from.as_deref(), Some("2026-08-06T22:14:08.000Z"));

    let err = Window::parse(Some("yesterday"), None).unwrap_err();
    assert!(
        err.to_string().contains("RFC 3339"),
        "unhelpful error: {err}"
    );
}

// ------------------------------------------------------------------- OTLP

#[test]
fn the_export_is_one_otlp_request_with_the_resource_on_it() {
    let events = events();
    let store = store_with(&events);
    let (text, stats) = export_to_string(&store, &Window::default(), Format::OtlpJson);

    let req: Value = serde_json::from_str(&text).expect("the streamed request is valid JSON");
    let resource_logs = req["resourceLogs"].as_array().unwrap();
    assert_eq!(resource_logs.len(), 1);
    let scope_logs = resource_logs[0]["scopeLogs"].as_array().unwrap();
    assert_eq!(scope_logs.len(), 1);
    let records = scope_logs[0]["logRecords"].as_array().unwrap();
    assert_eq!(records.len(), events.len(), "every event maps to a record");
    assert_eq!(stats.written, events.len() as u64);
    assert_eq!(stats.unmapped, 0);

    let attrs = &resource_logs[0]["resource"]["attributes"];
    let by_key: std::collections::BTreeMap<&str, &Value> = attrs
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["key"].as_str().unwrap(), &a["value"]))
        .collect();
    assert_eq!(by_key["service.name"]["stringValue"], "devbox");
    assert_eq!(by_key["service.version"]["stringValue"], "0.0.0-test");
    assert_eq!(by_key["devbox.box"]["stringValue"], "myapp");
    assert!(
        !by_key.contains_key("devbox.run.id"),
        "no run id until component A is wired; absent, not empty"
    );
}

/// The deviation that most often makes a collector reject a hand-rolled
/// payload: 64-bit fields are decimal strings, and enums are integers.
#[test]
fn otlp_encodes_64_bit_fields_as_strings_and_enums_as_integers() {
    let store = store_with(&events());
    let (text, _) = export_to_string(&store, &Window::default(), Format::OtlpJson);
    let req: Value = serde_json::from_str(&text).unwrap();
    let records = req["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .unwrap();

    for record in records {
        let ts = record["timeUnixNano"]
            .as_str()
            .expect("timeUnixNano is a decimal string, not a number");
        assert!(ts.parse::<u64>().is_ok(), "timeUnixNano is decimal: {ts}");
        assert!(
            record["severityNumber"].is_i64(),
            "severityNumber is an integer, never an enum name"
        );
        for attr in record["attributes"].as_array().unwrap() {
            if let Some(int) = attr["value"].get("intValue") {
                let s = int
                    .as_str()
                    .unwrap_or_else(|| panic!("{} intValue must be a string", attr["key"]));
                assert!(s.parse::<i64>().is_ok(), "intValue is decimal: {s}");
            }
        }
    }
}

#[test]
fn severity_text_comes_from_the_policy_verdict_and_is_info_otherwise() {
    let ctx = ctx();
    for event in events() {
        let record = devbox::export::otlp::log_record(&event, &ctx);
        let want = match event.policy.as_ref().map(|p| p.verdict.as_str()) {
            Some("block") => ("ERROR", 17),
            Some("flag") => ("WARN", 13),
            _ => ("INFO", 9),
        };
        assert_eq!(record["severityText"], want.0, "{}", event.kind);
        assert_eq!(record["severityNumber"], want.1, "{}", event.kind);
    }
}

#[test]
fn an_empty_window_is_still_a_well_formed_otlp_request() {
    let store = store_with(&events());
    // Entirely after the fixture.
    let window = Window::parse(Some("2030-01-01T00:00:00Z"), None).unwrap();
    let (text, stats) = export_to_string(&store, &window, Format::OtlpJson);

    assert_eq!(stats.matched, 0);
    assert_eq!(stats.written, 0);
    let req: Value = serde_json::from_str(&text).expect("still valid JSON");
    assert_eq!(
        req["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn an_empty_store_exports_cleanly_in_every_format() {
    let store = Store::open_in_memory().unwrap();
    for format in [Format::Jsonl, Format::Ocsf, Format::OtlpJson] {
        let (text, stats) = export_to_string(&store, &Window::default(), format);
        assert_eq!(stats, export::Stats::default(), "{}", format.as_str());
        match format {
            Format::OtlpJson => {
                serde_json::from_str::<Value>(&text).expect("an empty OTLP request is a request");
            }
            _ => assert!(text.is_empty(), "{}: {text:?}", format.as_str()),
        }
    }
}

// --------------------------------------------------------------- contract

/// The cross-language fixture is the shared contract, and other v5 tracks add
/// types to it. This test therefore asserts a property rather than a list: an
/// event either maps or is counted, and nothing goes missing in between.
#[test]
fn the_shared_contract_maps_or_counts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/event/testdata/events.jsonl");
    let events = read_events(&path);
    let store = store_with(&events);

    let (text, stats) = export_to_string(&store, &Window::default(), Format::Ocsf);
    assert_eq!(stats.matched, events.len() as u64);
    assert!(stats.balances(), "{stats:?}");
    assert_eq!(text.lines().count() as u64, stats.written);

    // Whatever is unmapped, the report names it — a silent drop is the failure
    // mode this whole counter exists to prevent.
    if stats.unmapped > 0 {
        let summary = stats.unmapped_summary();
        assert!(!summary.is_empty(), "unmapped rows with no explanation");
        let total: u64 = stats.unmapped_kinds.values().sum();
        assert_eq!(total, stats.unmapped);
    }
}

/// Larger than one page of the cursor, so the paging is exercised rather than
/// assumed.
#[test]
fn a_store_larger_than_one_page_exports_every_row_once() {
    let template = events();
    let mut many = Vec::new();
    for i in 0..1000u32 {
        let mut e = template[i as usize % template.len()].clone();
        e.pid = 1 + i;
        e.ts_mono_ns = 1_000 + u64::from(i);
        many.push(e);
    }
    let store = store_with(&many);

    let (text, stats) = export_to_string(&store, &Window::default(), Format::Jsonl);
    assert_eq!(stats.scanned, 1000);
    assert_eq!(stats.written, 1000);
    assert_eq!(text.lines().count(), 1000);

    let pids: std::collections::BTreeSet<u32> = text
        .lines()
        .map(|l| serde_json::from_str::<Event>(l).unwrap().pid)
        .collect();
    assert_eq!(pids.len(), 1000, "a row was emitted twice or skipped");
}
