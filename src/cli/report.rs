//! `devbox report <RUN_ID>` — print or re-render a run's report (§4.6).
//!
//! The run id alone identifies the run, but the *store* is per box, so this
//! has to find which box a run belongs to. It looks in the stored report first
//! (`~/.devbox/runs/<box>/<id>/report.json`), which is also what makes the
//! command keep working after the events have aged out of the store: the JSON
//! is the model, and markdown and HTML are renderings of it.

use anyhow::{Context, Result};
use clap::Args;

use crate::obs::Store;
use crate::obs::collector::store_path;
use crate::obs::run::is_run_id;
use crate::report::{self, RunReport};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// The run to report on
    pub run_id: String,

    /// Which rendering to print
    #[arg(long, value_enum, default_value = "md")]
    pub format: Format,

    /// Open the HTML report in a browser
    #[arg(long)]
    pub open: bool,

    /// Box name; defaults to searching every box for the run
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Md,
    Json,
    Html,
}

pub async fn run(args: ReportArgs, manager: &SandboxManager) -> Result<()> {
    if !is_run_id(&args.run_id) {
        anyhow::bail!(
            "'{}' is not a run id. Run ids are 26 characters; `devbox runs` lists them.",
            args.run_id
        );
    }

    let (box_id, report) = locate(manager, &args.run_id, args.name.as_deref())?;

    match args.format {
        Format::Md => print!("{}", report::markdown::render(&report)),
        Format::Json => println!("{}", report::json::render(&report)?),
        Format::Html => print!("{}", report::html::render(&report)?),
    }

    if args.open {
        let path =
            RunReport::directory(&manager.state_dir, &box_id, &args.run_id).join("report.html");
        if !path.exists() {
            // Re-render rather than refuse: a run recorded with `--no-report`
            // still has everything a report needs.
            report::write(&manager.state_dir, &report)?;
        }
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        std::process::Command::new(opener)
            .arg(&path)
            .spawn()
            .with_context(|| format!("could not open {}", path.display()))?;
    }
    Ok(())
}

/// Find a run: the report on disk first, then the boxes' stores.
pub(crate) fn locate(
    manager: &SandboxManager,
    run_id: &str,
    named: Option<&str>,
) -> Result<(String, RunReport)> {
    let boxes: Vec<String> = match named {
        Some(name) => vec![manager.resolve_name(Some(name))?],
        None => manager
            .list_sandboxes()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect(),
    };

    for box_id in &boxes {
        if let Ok(Some(report)) = report::load(&manager.state_dir, box_id, run_id) {
            return Ok((box_id.clone(), report));
        }
    }

    // No stored report — rebuild from the store, which is the path a run
    // recorded with `--no-report` takes.
    for box_id in &boxes {
        let path = store_path(&manager.state_dir, box_id);
        if !path.exists() {
            continue;
        }
        let Ok(store) = Store::open(&path) else {
            continue;
        };
        let Ok(Some(record)) = store.get_run(run_id) else {
            continue;
        };
        let events = store.query(&crate::obs::Query {
            run_id: Some(run_id.to_string()),
            limit: Some(crate::obs::Query::MAX_LIMIT),
            ..Default::default()
        })?;
        let attribution = store.attribution_counts(run_id)?;
        let unattributed =
            store.unattributed_in_window(&record.started_at, record.ended_at.as_deref())?;
        let report = RunReport::build(
            record,
            &events,
            // No overlay diff here: `devbox report` is a read of a run that has
            // already ended, and the box's overlay has moved on since. A file
            // section built now would describe a different moment and say so
            // nowhere. The empty section is the honest one.
            Box::new(|| Ok(Vec::new())),
            report::SCOPE_BOX,
            attribution,
            unattributed,
        );
        return Ok((box_id.clone(), report));
    }

    anyhow::bail!("No run '{run_id}'. `devbox runs [NAME]` lists the runs a box has recorded.")
}
