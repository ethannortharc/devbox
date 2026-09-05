//! One self-contained HTML file (§4.4).
//!
//! The same document is written to disk and served by the console, so a report
//! someone forwards and a report someone bookmarks cannot drift. It carries no
//! external reference of any kind — inline CSS, no JavaScript, no fonts — and
//! embeds the full JSON model, which makes the file people look at and the
//! file machines read the same file.

use anyhow::{Context, Result};
use askama::Template;

use super::model::RunReport;

#[derive(Template)]
#[template(path = "run_report.html")]
struct ReportTemplate<'a> {
    report: &'a RunReport,
    version: &'static str,
    duration: String,
    exit_code: String,
    posture: String,
    sources: String,
    /// `(kind, count)` rather than the map, because Askama cannot destructure
    /// a tuple in a `for` and `kv.0` reads better than a second projection.
    attribution: Vec<(String, u64)>,
    json: String,
}

/// Render the report.
pub fn render(report: &RunReport) -> Result<String> {
    let posture = if report.run.posture_before == report.run.posture_during {
        blank(&report.run.posture_during)
    } else {
        format!(
            "{} (restored to {} after)",
            report.run.posture_during, report.run.posture_before
        )
    };

    ReportTemplate {
        report,
        version: env!("CARGO_PKG_VERSION"),
        duration: super::model::human_duration(report.duration_ms),
        exit_code: report
            .run
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into()),
        posture,
        sources: if report.coverage.sources.is_empty() {
            "no capture source recorded".to_string()
        } else {
            report.coverage.sources.clone()
        },
        attribution: report
            .coverage
            .attribution
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        json: embeddable_json(report)?,
    }
    .render()
    .context("failed to render the run report")
}

/// JSON safe to place inside a `<script>` element.
///
/// `</script>` anywhere in the data — a file path, a command line, a domain —
/// ends the element early and drops the rest of the document into the page as
/// markup. Escaping the slash is the standard fix and stays valid JSON, since
/// `\/` decodes to `/`. `<!--` gets the same treatment: it opens a comment in
/// HTML's script-data state, which swallows everything after it.
fn embeddable_json(report: &RunReport) -> Result<String> {
    Ok(super::json::render(report)?
        .replace("</", "<\\/")
        .replace("<!--", "<\\u0021--"))
}

fn blank(value: &str) -> String {
    if value.is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::run::RunRecord;
    use crate::report::model::{Coverage, FileChanges, Network, RunReport, SCOPE_BOX};

    fn report_with(command: &str) -> RunReport {
        RunReport {
            report_version: 1,
            run: RunRecord {
                run_id: "01ABCDEFGHJKMNPQRSTVWXYZ00".into(),
                box_id: "devtest".into(),
                kind: "run".into(),
                argv: vec![command.to_string()],
                started_at: "2026-09-05T00:00:00.000Z".into(),
                status: "finished".into(),
                ..Default::default()
            },
            duration_ms: Some(10),
            files: FileChanges {
                scope: SCOPE_BOX.into(),
                changes: Vec::new(),
                directories: 0,
            },
            network: Network::default(),
            processes: Vec::new(),
            violations: Vec::new(),
            credential_use: Vec::new(),
            coverage: Coverage::default(),
        }
    }

    #[test]
    fn the_page_carries_no_external_reference() {
        let html = render(&report_with("true")).unwrap();
        for forbidden in ["src=\"http", "href=\"http", "@import", "/assets/"] {
            assert!(
                !html.contains(forbidden),
                "a self-contained report must not reference {forbidden}"
            );
        }
    }

    #[test]
    fn a_command_containing_a_script_tag_cannot_close_the_json_block() {
        // A command line is guest-influenced text that reaches both the visible
        // page and the embedded JSON. The visible half is escaped by Askama;
        // the JSON half is inside a `<script>`, where HTML escaping does not
        // apply and only the slash escape saves it.
        let html = render(&report_with("</script><img src=x onerror=alert(1)>")).unwrap();
        let json_block = html
            .split(r#"<script type="application/json" id="report">"#)
            .nth(1)
            .expect("the JSON block");
        let (payload, _) = json_block.split_once("</script>").expect("its close tag");
        assert!(
            !payload.contains("</script>"),
            "the payload closed its own element"
        );
        assert!(payload.contains(r"<\/script>"), "expected the slash escape");
        // And it is still JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(&payload.replace(r"<\/", "</")).expect("valid JSON");
        assert_eq!(parsed["run"]["run_id"], "01ABCDEFGHJKMNPQRSTVWXYZ00");
    }

    #[test]
    fn the_coverage_badge_says_partial_without_ebpf() {
        let mut report = report_with("true");
        report.coverage.sources = "proc+packet".into();
        let html = render(&report).unwrap();
        assert!(html.contains("coverage: partial"), "{html}");
        report.coverage.sources = "ebpf+packet+netfilter".into();
        assert!(render(&report).unwrap().contains("coverage: full"));
    }
}
