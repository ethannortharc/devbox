//! The terminal summary, and what `devbox report` prints by default (§4.4).
//!
//! Hand-rendered. A markdown crate would be a dependency for string
//! concatenation, and a golden test over hand-rendered output is a test of the
//! report; over a library's it is a test of the library.

use super::model::{RunReport, human_bytes, human_duration};

/// Render the whole report.
pub fn render(report: &RunReport) -> String {
    let mut out = String::new();
    header(&mut out, report);
    coverage(&mut out, report);
    files(&mut out, report);
    network(&mut out, report);
    processes(&mut out, report);
    credentials(&mut out, report);
    violations(&mut out, report);
    out
}

/// The few lines `devbox run` prints when the command exits.
///
/// Not `render()` truncated: the terminal wants the verdict, and the file has
/// the evidence. Printing eighty lines of process tree after every command is
/// how people learn to stop reading the output.
pub fn render_summary(report: &RunReport) -> String {
    let mut out = String::new();
    let exit = report
        .run
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "—".into());
    out.push_str(&format!(
        "run {} · {} · exit {} · {}\n",
        report.run.run_id,
        human_duration(report.duration_ms),
        exit,
        report.run.status,
    ));
    out.push_str(&format!(
        "  files    {} changed ({} added, {} modified, {} deleted) · scope: {}\n",
        report.files.changes.len(),
        report.files.added(),
        report.files.modified(),
        report.files.deleted(),
        report.files.scope,
    ));
    out.push_str(&format!(
        "  network  {} peers · {} DNS · ↑{} ↓{}\n",
        report.network.domains.len(),
        report.network.dns.len(),
        human_bytes(report.network.bytes_tx),
        human_bytes(report.network.bytes_rx),
    ));
    out.push_str(&format!(
        "  process  {} in the tree\n",
        report.processes.len()
    ));
    if !report.violations.is_empty() {
        out.push_str(&format!(
            "  policy   {} refusals\n",
            report.violations.len()
        ));
    }
    out.push_str(&format!(
        "  coverage {} ({}) · {} events · {} dropped\n",
        report.coverage.badge(),
        if report.coverage.sources.is_empty() {
            "no capture source recorded"
        } else {
            &report.coverage.sources
        },
        report.coverage.events,
        report.coverage.dropped_events,
    ));
    out
}

fn header(out: &mut String, report: &RunReport) {
    out.push_str(&format!("# Run {}\n\n", report.run.run_id));
    if !report.run.label.is_empty() {
        out.push_str(&format!("**{}**\n\n", report.run.label));
    }
    out.push_str(&format!("- box: `{}`\n", report.run.box_id));
    out.push_str(&format!("- kind: {}\n", report.run.kind));
    out.push_str(&format!(
        "- command: `{}`\n",
        report.run.command_line().replace('`', "'")
    ));
    if !report.run.cwd.is_empty() {
        out.push_str(&format!("- cwd: `{}`\n", report.run.cwd));
    }
    out.push_str(&format!("- started: {}\n", report.run.started_at));
    out.push_str(&format!(
        "- duration: {}\n",
        human_duration(report.duration_ms)
    ));
    out.push_str(&format!(
        "- exit: {}\n",
        report
            .run
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into())
    ));
    out.push_str(&format!("- status: {}\n", report.run.status));
    let posture = if report.run.posture_before == report.run.posture_during {
        report.run.posture_during.clone()
    } else {
        format!(
            "{} (restored to {} after)",
            report.run.posture_during, report.run.posture_before
        )
    };
    if !posture.is_empty() {
        out.push_str(&format!("- posture: {posture}\n"));
    }
    out.push('\n');
}

fn coverage(out: &mut String, report: &RunReport) {
    out.push_str("## Coverage\n\n");
    out.push_str(&format!(
        "`{}` — {}\n\n",
        report.coverage.badge(),
        if report.coverage.sources.is_empty() {
            "no capture source recorded"
        } else {
            &report.coverage.sources
        }
    ));
    out.push_str("| | |\n|---|---|\n");
    out.push_str(&format!(
        "| agent | {} |\n",
        blank(&report.coverage.agent_version)
    ));
    out.push_str(&format!(
        "| events attributed | {} |\n",
        report.coverage.events
    ));
    for (kind, n) in &report.coverage.attribution {
        out.push_str(&format!("| … by {kind} | {n} |\n"));
    }
    out.push_str(&format!(
        "| dropped during the run | {} |\n",
        report.coverage.dropped_events
    ));
    // Three tiers, and only one of them is a warning. A stream that was
    // re-published without changing agent lost nothing, and saying so in the
    // same voice as a real interruption is how a warning stops being read.
    if let Some(gap) = &report.capture_gap {
        // Both ends, not just the restart. "Capture restarted at Y" leaves a
        // reader to assume the whole run is suspect; the window tells them
        // which part to doubt, and how long it was.
        // One literal, not a `\`-continued one: rustfmt joins a continued
        // string back onto a single line and leaves the continuation's
        // indentation inside the message.
        out.push_str(&format!(
            "| **capture was interrupted** | the last event recorded for this run was at {}; a new agent attached at {}; nothing the sandbox did in between reached this report |\n",
            gap.from, gap.to
        ));
    } else if !report.run.capture_reattached_at.is_empty() {
        out.push_str(&format!(
            "| capture re-attached | {} — same agent, nothing lost |\n",
            report.run.capture_reattached_at
        ));
    }
    out.push_str(&format!(
        "| unattributed in the window | {} |\n",
        report.coverage.unattributed_in_window
    ));
    out.push('\n');
}

fn files(out: &mut String, report: &RunReport) {
    out.push_str("## Files\n\n");
    out.push_str(&format!("_scope: {}_\n\n", report.files.scope));
    // What the agent was watching, so "nothing outside the workspace" can be
    // told apart from "nothing was looked for outside the workspace".
    if !report.run.file_scope.is_empty() {
        out.push_str(&format!(
            "_file events observed under: {}_\n\n",
            report.run.file_scope
        ));
    }
    if report.files.is_empty() {
        out.push_str("No changes to the workspace overlay.\n\n");
        outside(out, report);
        return;
    }
    out.push_str("| | path |\n|---|---|\n");
    for change in &report.files.changes {
        let mark = match change.status.as_str() {
            "added" => "+",
            "modified" => "~",
            _ => "-",
        };
        out.push_str(&format!("| {mark} | `{}` |\n", change.path));
    }
    out.push_str(&format!(
        "\n{} file(s): {} added, {} modified, {} deleted",
        report.files.changes.len(),
        report.files.added(),
        report.files.modified(),
        report.files.deleted(),
    ));
    if report.files.directories > 0 {
        out.push_str(&format!(" (and {} directories)", report.files.directories));
    }
    out.push_str("\n\n");
    outside(out, report);
}

/// Writes the overlay does not carry, which is most of what a package manager
/// does.
fn outside(out: &mut String, report: &RunReport) {
    if !report.files.has_outside() {
        return;
    }
    out.push_str("### Writes outside the workspace overlay\n\n");
    out.push_str("| directory | opens | files |\n|---|---|---|\n");
    for row in &report.files.outside {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            row.prefix, row.writes, row.paths
        ));
    }
    out.push_str(
        "\nThese are inside the box's file scope but not part of what \
`devbox commit` would sync.\n\n",
    );
}

fn network(out: &mut String, report: &RunReport) {
    out.push_str("## Network\n\n");
    if report.network.domains.is_empty() && report.network.dns.is_empty() {
        out.push_str("No network activity.\n\n");
        return;
    }
    if !report.network.domains.is_empty() {
        out.push_str(
            "| peer | conns | ports | tls | ↑ | ↓ | time |\n|---|---|---|---|---|---|---|\n",
        );
        for row in &report.network.domains {
            let ports = row.ports_human();
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} |\n",
                row.peer,
                row.conns_human(),
                blank(&ports),
                if row.tls { "yes" } else { "" },
                row.tx_human(),
                row.rx_human(),
                row.dur_human(),
            ));
        }
        out.push('\n');
    }
    if !report.network.tls.is_empty() {
        out.push_str(&format!(
            "TLS server names: {}\n\n",
            report
                .network
                .tls
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !report.network.dns.is_empty() {
        out.push_str(&format!(
            "DNS: {}\n\n",
            report
                .network
                .dns
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push_str(&format!(
        "Total: ↑{} ↓{}\n\n",
        report.network.tx_human(),
        report.network.rx_human(),
    ));
}

fn processes(out: &mut String, report: &RunReport) {
    out.push_str("## Processes\n\n");
    if report.processes.is_empty() {
        out.push_str("No processes captured.\n\n");
        return;
    }
    out.push_str("```\n");
    for row in &report.processes {
        out.push_str(&format!(
            "{}{} [{}] {}\n",
            row.indent(),
            row.comm,
            row.pid,
            row.display_command(),
        ));
    }
    out.push_str("```\n\n");
}

fn credentials(out: &mut String, report: &RunReport) {
    out.push_str("## Credentials\n\n");
    if report.credential_use.is_empty() {
        out.push_str("No credential use recorded.\n\n");
        return;
    }
    out.push_str("| provider | upstream | method | uses | last |\n|---|---|---|---|---|\n");
    for row in &report.credential_use {
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} |\n",
            row.provider,
            blank(&row.host),
            blank(&row.method),
            row.uses_human(),
            row.last_use,
        ));
    }
    out.push('\n');
}

fn violations(out: &mut String, report: &RunReport) {
    out.push_str("## Policy\n\n");
    if report.violations.is_empty() {
        out.push_str("No refusals.\n\n");
        return;
    }
    out.push_str("| target | verdict | reason | at |\n|---|---|---|---|\n");
    for violation in &report.violations {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            violation.target, violation.verdict, violation.reason, violation.ts
        ));
    }
    out.push('\n');
}

fn blank(value: &str) -> &str {
    if value.is_empty() { "—" } else { value }
}
