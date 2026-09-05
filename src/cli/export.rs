//! `devbox export` — the box's record, in a format someone else's tool reads.
//!
//! `devbox behavior summary` answers "what did this box do"; this answers
//! "put what it did where my SIEM can see it". Three formats, one cursor over
//! the store, and a count on stderr that says how many rows were read, how many
//! were written, and — for OCSF, which has no class for every devbox event
//! type — how many were not.

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::cli::box_arg::BoxArg;
use crate::export::{self, Format, Window};
use crate::obs::store::Store;
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct ExportArgs {
    #[command(flatten)]
    pub boxarg: BoxArg,

    /// Only events at or after this RFC 3339 timestamp
    #[arg(long, value_name = "T")]
    pub from: Option<String>,

    /// Only events before this RFC 3339 timestamp
    #[arg(long, value_name = "T")]
    pub to: Option<String>,

    /// Export one run's events (`devbox runs` lists the ids)
    #[arg(long, value_name = "ID", conflicts_with_all = ["from", "to"])]
    pub run: Option<String>,

    /// Output format
    #[arg(long, value_enum)]
    pub format: Format,

    /// Write to this file instead of stdout
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

pub async fn run(args: ExportArgs, manager: &SandboxManager) -> Result<()> {
    if let Some(run_id) = &args.run
        && !crate::obs::run::is_run_id(run_id)
    {
        bail!(
            "'{run_id}' is not a run id. Run ids are 26 characters; \
             `devbox runs [NAME]` lists them."
        );
    }

    let name = manager.resolve_name(args.boxarg.name())?;
    // `store_path` only joins, and this name can come from an explicit flag.
    if !crate::sandbox::state::is_safe_name(&name) {
        bail!("{name:?} is not a box name");
    }
    let path = crate::obs::collector::store_path(&manager.state_dir, &name);
    if !path.exists() {
        eprintln!("No collected events are available for box '{name}'.");
        eprintln!("Start the box with a devbox command to collect its timeline.");
        eprintln!("The collector store is {}.", path.display());
        return Ok(());
    }

    let window = Window::parse(args.from.as_deref(), args.to.as_deref())?;
    if let (Some(from), Some(until)) = (&window.from, &window.until)
        && from >= until
    {
        bail!("--from {from} is not before --to {until}; the window is empty");
    }

    let store = Store::open(&path)?;
    let run_id = args.run.as_deref();
    // The run is what makes an export evidence rather than a log dump, and it
    // has to reach the records themselves — `metadata.correlation_uid`,
    // `actor.session.uid`, and the `devbox.run.id` resource attribute all read
    // it from here. Selecting the run's rows without stamping them left every
    // record saying "correlated with nothing".
    let ctx = export::Context {
        run_id: run_id.map(str::to_string),
        ..export::Context::new(&name)
    };

    // Say so rather than exporting nothing. An empty OCSF document for a run
    // id that does not exist on this box is indistinguishable from a run that
    // did nothing, and only one of those is worth investigating.
    if let Some(run_id) = run_id
        && store.get_run(run_id)?.is_none()
    {
        bail!(
            "box '{name}' has no run '{run_id}'. `devbox runs {name}` lists the \
             runs it has recorded."
        );
    }

    let stats = match &args.out {
        None => {
            let stdout = std::io::stdout();
            let mut out = BufWriter::new(stdout.lock());
            let stats =
                export::run_selection(&store, &window, run_id, &ctx, args.format, &mut out)?;
            out.flush().context("failed to flush the export")?;
            stats
        }
        Some(dest) => write_to_file(&store, &window, run_id, &ctx, args.format, dest)?,
    };

    // An export that lost rows is worse than one that failed: it is a record
    // that looks complete. The counter is the only thing that can notice.
    if !stats.balances() {
        bail!(
            "export accounting does not balance: {} matched, {} written, {} unmapped",
            stats.matched,
            stats.written,
            stats.unmapped
        );
    }

    let where_to = match &args.out {
        Some(p) => format!(" to {}", p.display()),
        None => String::new(),
    };
    eprintln!(
        "Exported {} of {} event(s) as {}{} (scanned {}).",
        stats.written,
        stats.matched,
        args.format.as_str(),
        where_to,
        stats.scanned,
    );
    if stats.unmapped > 0 {
        eprintln!(
            "warning: {} event(s) have no {} class in this build and were not \
             written: {}. Use --format jsonl for the complete record.",
            stats.unmapped,
            args.format.as_str(),
            stats.unmapped_summary(),
        );
    }
    Ok(())
}

/// Write through a temporary file in the destination directory, then rename.
///
/// An export that dies halfway through — a full disk, an interrupted terminal —
/// must not leave a file that reads as a complete audit trail. The rename is
/// atomic within the directory, so the path either has the whole export or
/// nothing at all.
fn write_to_file(
    store: &Store,
    window: &Window,
    run_id: Option<&str>,
    ctx: &export::Context,
    format: Format,
    dest: &PathBuf,
) -> Result<export::Stats> {
    let dir = dest.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let temp = tempfile::NamedTempFile::new_in(&dir)
        .with_context(|| format!("failed to open a temporary file in {}", dir.display()))?;
    let stats = {
        let mut out = BufWriter::new(&temp);
        let stats = export::run_selection(store, window, run_id, ctx, format, &mut out)?;
        out.flush().context("failed to flush the export")?;
        stats
    };
    temp.persist(dest)
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(stats)
}
