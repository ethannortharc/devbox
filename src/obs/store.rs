//! Event store — embedded SQLite, one database per box (§7.4).
//!
//! The table is append-only. Columns that queries filter on are lifted out of
//! the event and indexed; the whole event is kept as JSON alongside them, so
//! the schema can gain a filter later without a migration that has to
//! reconstruct data it never stored.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::event::{Event, EventType};

/// Opens and owns a box's event database.
pub struct Store {
    conn: Connection,
}

/// What to select when querying events.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Inclusive lower bound on `ts_wall`.
    pub since: Option<String>,
    /// Exclusive upper bound on `ts_wall`.
    pub until: Option<String>,
    pub pid: Option<u32>,
    /// Exclusive lower bound on the row id.
    ///
    /// The live tail's anchor. `ts_wall` cannot serve: it ties for events
    /// captured in the same millisecond, so resuming from the last timestamp
    /// either repeats a row or skips one. `id` is the insertion order and is
    /// unique by construction.
    pub after_id: Option<i64>,
    /// Inclusive upper bound on the row id.
    ///
    /// Pins a page to the store as it was when its high-water mark was read.
    /// Without it, rows written between reading the mark and running the query
    /// are inside the page but outside the cursor it advances to, and are
    /// therefore delivered twice.
    pub before_id: Option<i64>,
    pub kinds: Vec<EventType>,
    /// Substring match against the peer (domain, SNI, or address).
    pub peer: Option<String>,
    /// Substring match against the path.
    pub path: Option<String>,
    /// Drop rows whose stored JSON is larger than this from the result.
    ///
    /// A row limit bounds the *count*, not the bytes. The frame limit lets one
    /// event be a megabyte, so two hundred of them is two hundred megabytes —
    /// cloned into several derived views and rendered into HTML, on a timer.
    /// The console asks for a size bound; exports and the CLI do not, because
    /// they are the tools you reach for when you want the whole record.
    pub max_bytes: Option<usize>,
    /// Maximum rows to return. Defaults to [`Query::DEFAULT_LIMIT`].
    pub limit: Option<usize>,
    /// Newest first when true, which is what a live view wants.
    pub newest_first: bool,
}

impl Query {
    /// Rows returned when a query does not say.
    ///
    /// A box can produce millions of events; an unbounded default would let a
    /// single console request try to render all of them.
    pub const DEFAULT_LIMIT: usize = 500;
    /// Hard ceiling, so an explicit limit cannot ask for everything either.
    pub const MAX_LIMIT: usize = 50_000;

    /// The effective limit after defaults and clamping.
    pub fn effective_limit(&self) -> usize {
        self.limit
            .unwrap_or(Self::DEFAULT_LIMIT)
            .clamp(1, Self::MAX_LIMIT)
    }
}

/// Retention policy for a box's store (§7.4).
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub max_events: u64,
    /// Bytes the database file may occupy before the oldest events go.
    pub max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_events: 2_000_000,
            // A row count alone is not a size. The transport accepts frames up
            // to a megabyte, so two million rows is two terabytes in the worst
            // case — a guest can fill the host volume without ever reaching
            // the row limit that was supposed to bound it. The page count is
            // a pragma, so asking is cheap.
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

impl Store {
    /// Open (creating if needed) the store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open event store {}", path.display()))?;
        Self::init(conn)
    }

    /// Open an in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("failed to open an in-memory store")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // A second process should normally be excluded by the supervisor's
        // per-box flock. Readers and a handoff can still overlap briefly; wait
        // for SQLite's WAL writer instead of turning SQLITE_BUSY into a lost
        // event batch immediately.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("failed to set SQLite busy timeout")?;
        // WAL keeps the collector's writes from blocking the console's reads,
        // which happen on every SSE-driven refresh.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL")?;
        // The store is a cache of observations, not a ledger: losing the last
        // few events to a power cut is fine, and fsync per insert is not.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("failed to set synchronous mode")?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_wall     TEXT    NOT NULL,
                ts_mono_ns  INTEGER NOT NULL,
                box_id      TEXT    NOT NULL,
                cgroup_id   INTEGER NOT NULL,
                pid         INTEGER NOT NULL,
                tid         INTEGER NOT NULL,
                ppid        INTEGER NOT NULL,
                comm        TEXT    NOT NULL,
                uid         INTEGER NOT NULL,
                type        TEXT    NOT NULL,
                peer        TEXT,
                dport       INTEGER,
                path        TEXT,
                raw         TEXT    NOT NULL
            );

            CREATE INDEX IF NOT EXISTS events_ts    ON events (ts_wall);
            CREATE INDEX IF NOT EXISTS events_pid   ON events (pid);
            CREATE INDEX IF NOT EXISTS events_type  ON events (type);
            CREATE INDEX IF NOT EXISTS events_peer  ON events (peer);
            CREATE INDEX IF NOT EXISTS events_path  ON events (path);

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .context("failed to create the event schema")?;

        // A generation, written once when the store is created.
        //
        // Readers need to tell one store from another: a box destroyed and
        // recreated under the same name gets a fresh database whose ids start
        // again at one, and a console holding a position in the old one must
        // notice rather than carry on. File metadata cannot answer that. The
        // inode is reused; `ctime` is the inode-change time, so an ordinary
        // WAL checkpoint moves it and every reader concludes the store was
        // replaced. This is the identity of the store as a thing, not as a
        // file, and it is read back through the same handle as the rows.
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('generation', ?)",
            params![rand::random::<u64>().max(1).to_string()],
        )
        .context("failed to stamp the store generation")?;

        Ok(Self { conn })
    }

    /// Append one event.
    pub fn insert(&self, event: &Event) -> Result<i64> {
        event.validate()?;
        let raw = serde_json::to_string(event).context("failed to serialize an event")?;

        self.conn
            .execute(
                "INSERT INTO events
                   (ts_wall, ts_mono_ns, box_id, cgroup_id, pid, tid, ppid, comm, uid,
                    type, peer, dport, path, raw)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    event.ts_wall,
                    event.ts_mono_ns,
                    event.box_id,
                    event.cgroup_id,
                    event.pid,
                    event.tid,
                    event.ppid,
                    event.comm,
                    event.uid,
                    event.kind.as_str(),
                    event.peer(),
                    event.net.as_ref().map(|n| n.dport),
                    event.path(),
                    raw,
                ],
            )
            .context("failed to insert an event")?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Append many events in one transaction.
    ///
    /// The collector batches: one transaction per burst is the difference
    /// between a few thousand and a few hundred thousand events per second.
    pub fn insert_batch(&mut self, events: &[Event]) -> Result<usize> {
        let tx = self.conn.transaction().context("failed to begin")?;
        let mut written = 0;
        for event in events {
            if event.validate().is_err() {
                continue;
            }
            let raw = serde_json::to_string(event)?;
            tx.execute(
                "INSERT INTO events
                   (ts_wall, ts_mono_ns, box_id, cgroup_id, pid, tid, ppid, comm, uid,
                    type, peer, dport, path, raw)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    event.ts_wall,
                    event.ts_mono_ns,
                    event.box_id,
                    event.cgroup_id,
                    event.pid,
                    event.tid,
                    event.ppid,
                    event.comm,
                    event.uid,
                    event.kind.as_str(),
                    event.peer(),
                    event.net.as_ref().map(|n| n.dport),
                    event.path(),
                    raw,
                ],
            )?;
            written += 1;
        }
        tx.commit().context("failed to commit")?;
        Ok(written)
    }

    /// Read the next page of a live tail, within `(after_id, up_to_id]`.
    ///
    /// Returns the decoded events *and the highest row id examined*, which are
    /// not the same thing: a row written by an older schema no longer decodes,
    /// and anchoring on the newest row that happened to decode would hand the
    /// caller the same undecodable rows on every request forever. The scan
    /// position is a property of the scan, not of what survived it.
    ///
    /// Bounded above so a caller that has already decided how big the backlog
    /// is reads exactly the backlog it measured — otherwise a batch committed
    /// between the two makes the page and the decision disagree.
    pub fn tail_scan(
        &self,
        after_id: i64,
        up_to_id: i64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<(Vec<Event>, i64)> {
        // The size test is in the projection, not the predicate: an oversized
        // row must still advance the scan, or it wedges the tail exactly the
        // way an undecodable one did.
        let mut stmt = self
            .conn
            .prepare(
                // Not `LENGTH(raw)`. On a TEXT value SQLite's `LENGTH` counts
                // *characters*, so a cap meant as sixty-four kilobytes admitted
                // four times that for any event carrying multibyte content —
                // which a command line or a domain name routinely does. The
                // cast measures storage.
                "SELECT id, CASE WHEN LENGTH(CAST(raw AS BLOB)) <= ? THEN raw ELSE NULL END
                   FROM events WHERE id > ? AND id <= ? ORDER BY id ASC LIMIT ?",
            )
            .context("failed to prepare the tail scan")?;
        let rows = stmt
            .query_map(
                params![max_bytes as i64, after_id, up_to_id, limit as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .context("failed to run the tail scan")?;

        let mut events = Vec::new();
        let mut scanned_to = after_id;
        for row in rows {
            let (id, raw) = row.context("failed to read a row")?;
            scanned_to = scanned_to.max(id);
            let Some(raw) = raw else {
                tracing::debug!(id, "skipping an oversized event in a live tail");
                continue;
            };
            match serde_json::from_str(&raw) {
                Ok(event) => events.push(event),
                Err(e) => tracing::warn!(error = %e, "stored event no longer decodes"),
            }
        }
        Ok((events, scanned_to))
    }

    /// Run a query.
    pub fn query(&self, q: &Query) -> Result<Vec<Event>> {
        // The size cap is a projection, not a predicate, so it costs nothing
        // before `LIMIT`. As a predicate it turned `LIMIT 200` into "scan
        // until two hundred small rows are found", which on a store whose
        // recent history is all oversized is a scan of the whole two million.
        let mut sql = match q.max_bytes {
            Some(_) => String::from(
                "SELECT id, CASE WHEN LENGTH(CAST(raw AS BLOB)) <= ? THEN raw ELSE NULL END \
                 FROM events WHERE 1=1",
            ),
            None => String::from("SELECT id, raw FROM events WHERE 1=1"),
        };
        // Placeholders are positional, so the projection's binds first.
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(max_bytes) = q.max_bytes {
            args.push(Box::new(max_bytes as i64));
        }

        // `ts_wall` is compared as text, which only works if both sides are in
        // the same normal form. A user-supplied `--since 2026-08-06T14:00:00+02:00`
        // sorts as the literal string it is, so it would select events by the
        // digits of a different timezone's clock. Normalizing to UTC makes the
        // comparison mean what the caller wrote.
        if let Some(since) = &q.since {
            sql.push_str(" AND ts_wall >= ?");
            args.push(Box::new(normalize_ts(since)));
        }
        if let Some(until) = &q.until {
            sql.push_str(" AND ts_wall < ?");
            args.push(Box::new(normalize_ts(until)));
        }
        if let Some(pid) = q.pid {
            sql.push_str(" AND pid = ?");
            args.push(Box::new(pid));
        }
        if let Some(after) = q.after_id {
            sql.push_str(" AND id > ?");
            args.push(Box::new(after));
        }
        if let Some(before) = q.before_id {
            sql.push_str(" AND id <= ?");
            args.push(Box::new(before));
        }
        if !q.kinds.is_empty() {
            let holes = vec!["?"; q.kinds.len()].join(",");
            sql.push_str(&format!(" AND type IN ({holes})"));
            for k in &q.kinds {
                args.push(Box::new(k.as_str().to_string()));
            }
        }
        if let Some(peer) = &q.peer {
            sql.push_str(" AND peer LIKE ?");
            args.push(Box::new(format!("%{peer}%")));
        }
        if let Some(path) = &q.path {
            sql.push_str(" AND path LIKE ?");
            args.push(Box::new(format!("%{path}%")));
        }

        // `id` rather than a timestamp: it is the insertion order, it is
        // unique, and it does not tie for events captured in the same
        // millisecond.
        sql.push_str(if q.newest_first {
            " ORDER BY id DESC"
        } else {
            " ORDER BY id ASC"
        });
        sql.push_str(" LIMIT ?");
        args.push(Box::new(q.effective_limit() as i64));

        let mut stmt = self.conn.prepare(&sql).context("failed to prepare query")?;
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .context("failed to run query")?;

        let mut out = Vec::new();
        for row in rows {
            let (_, raw) = row.context("failed to read a row")?;
            // Nulled by the size cap. A window that is mostly oversized comes
            // back short, which is the honest answer: those events are in the
            // store and in every view that does not re-read itself on a timer.
            let Some(raw) = raw else { continue };
            match serde_json::from_str(&raw) {
                Ok(event) => out.push(event),
                // A row that no longer parses is a schema drift, not a reason
                // to fail the whole query — log it and keep the rest usable.
                Err(e) => tracing::warn!(error = %e, "stored event no longer decodes"),
            }
        }
        Ok(out)
    }

    /// This store's generation — its identity as a thing rather than a file.
    ///
    /// Stable for the life of the database and different for its replacement,
    /// which is exactly what a page's cursor has to be able to check. Never
    /// zero: readers use zero for "no store at all".
    pub fn generation(&self) -> Result<u64> {
        let raw: String = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
                r.get(0)
            })
            .context("failed to read the store generation")?;
        Ok(raw.parse::<u64>().unwrap_or(1).max(1))
    }

    /// Read a window for export, bounded by rows *and* by bytes.
    ///
    /// Returns the events and whether the window was cut short. Two limits,
    /// because a row limit does not bound memory: the transport accepts frames
    /// up to a megabyte, so fifty thousand rows is fifty gigabytes in the worst
    /// case — enough to end the process that was asked for an audit trail.
    ///
    /// Truncation is reported from what the scan *reached*, not from what
    /// decoded. A single row written by an older schema is dropped on the way
    /// out, and inferring "not truncated" from a short result then let an
    /// export claim to be complete while missing its tail.
    pub fn export(
        &self,
        since: Option<&str>,
        rows: usize,
        bytes: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let mut sql = String::from("SELECT raw FROM events WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(since) = since {
            sql.push_str(" AND ts_wall >= ?");
            args.push(Box::new(normalize_ts(since)));
        }
        // One more than asked for, so "there is another row" is observed
        // rather than inferred. Scanning exactly the limit told a window that
        // happened to hold precisely fifty thousand events that it had been
        // cut off, and stamped a complete audit as partial.
        sql.push_str(" ORDER BY id ASC LIMIT ?");
        args.push(Box::new(rows as i64 + 1));

        let mut stmt = self
            .conn
            .prepare(&sql)
            .context("failed to prepare export")?;
        let mut scanned = 0usize;
        let mut budget = bytes;
        let mut truncated = false;
        let mut out = Vec::new();
        let mut cursor = stmt
            .query(params_from_iter(args.iter().map(|a| a.as_ref())))
            .context("failed to run export")?;
        while let Some(row) = cursor.next().context("failed to read a row")? {
            if scanned == rows {
                // The extra row exists, so the window really was cut off.
                truncated = true;
                break;
            }
            scanned += 1;
            let raw: String = row.get(0)?;
            match budget.checked_sub(raw.len()) {
                Some(left) => budget = left,
                None => {
                    truncated = true;
                    break;
                }
            }
            match serde_json::from_str(&raw) {
                Ok(event) => out.push(event),
                Err(e) => {
                    // The export is now missing an event it was asked for.
                    // Silence here let an incomplete audit present itself as a
                    // complete one, which is the one thing this flag exists to
                    // prevent.
                    truncated = true;
                    tracing::warn!(error = %e, "stored event no longer decodes");
                }
            }
        }
        Ok((out, truncated))
    }

    /// The highest row id, or zero for an empty store.
    ///
    /// A live tail starts here so it shows what happens *next* rather than
    /// replaying the whole database into a console that already rendered it.
    pub fn max_id(&self) -> Result<i64> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))
            .context("failed to read the newest event id")?;
        Ok(id.unwrap_or(0))
    }

    /// Total events stored.
    pub fn count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .context("failed to count events")?;
        Ok(n as u64)
    }

    /// Count by type, for the metrics exporter and the summary header.
    pub fn count_by_type(&self) -> Result<Vec<(EventType, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT type, COUNT(*) FROM events GROUP BY type ORDER BY type")
            .context("failed to prepare the type histogram")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .context("failed to run the type histogram")?;

        let mut out = Vec::new();
        for row in rows {
            let (kind, n) = row?;
            if let Ok(kind) = kind.parse::<EventType>() {
                out.push((kind, n as u64));
            }
        }
        Ok(out)
    }

    /// The wall-clock timestamp of the newest event, if any.
    pub fn latest_ts(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT ts_wall FROM events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("failed to read the latest timestamp")
    }

    /// Every distinct peer contacted in a window, newest first.
    pub fn distinct_peers(&self, since: Option<&str>) -> Result<Vec<String>> {
        let mut sql =
            String::from("SELECT DISTINCT peer FROM events WHERE peer IS NOT NULL AND peer != ''");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(since) = since {
            sql.push_str(" AND ts_wall >= ?");
            args.push(Box::new(since.to_string()));
        }
        sql.push_str(" ORDER BY peer");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |row| {
            row.get::<_, String>(0)
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drop the oldest events beyond the retention cap.
    ///
    /// Returns how many rows were removed.
    pub fn enforce_retention(&self, retention: Retention) -> Result<u64> {
        let mut removed = self
            .conn
            .execute(
                "DELETE FROM events WHERE id <= (
                     SELECT MAX(id) - ?1 FROM events
                 )",
                params![retention.max_events as i64],
            )
            .context("failed to enforce retention")?;

        // Then by size. A row count is not a size: at the frame limit, two
        // million rows is two terabytes, so a guest sending large events fills
        // the volume without ever approaching the count that was meant to stop
        // it. Trimmed in slices rather than in one statement, because the file
        // only shrinks as pages are freed and one oversized batch should not
        // take the whole window with it.
        let mut size = self.size_bytes()?;
        for _ in 0..RETENTION_TRIM_PASSES {
            if size <= retention.max_bytes {
                break;
            }
            let rows: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .context("failed to count events for a size trim")?;
            if rows <= 0 {
                break;
            }

            // Sized to the overage, not a fixed slab.
            //
            // A constant batch is only ever right for one row size. At the
            // frame limit five hundred events fill the ceiling, and deleting
            // two thousand of them took the entire history to reclaim a little
            // — a retention sweep is supposed to bound the store, not empty
            // it. Estimating from the average makes one pass remove roughly
            // the excess whatever the events weigh, and the halving cap keeps
            // a bad estimate from running away.
            let average = (size / rows as u64).max(1);
            let excess = size.saturating_sub(retention.max_bytes);
            let want = excess.div_ceil(average).clamp(1, (rows as u64).div_ceil(2));

            let cut = self
                .conn
                .execute(
                    "DELETE FROM events WHERE id IN (
                         SELECT id FROM events ORDER BY id ASC LIMIT ?1
                     )",
                    params![want as i64],
                )
                .context("failed to trim the store to its size limit")?;
            if cut == 0 {
                break;
            }
            removed += cut;

            // Stop if deleting did not move the measure. Any size that does
            // not respond to deletion, put in a loop that deletes, empties the
            // store — which is how the first version of this behaved, because
            // `page_count` counts the pages the *file* holds and freeing them
            // does not shrink it.
            let after = self.size_bytes()?;
            if after >= size {
                break;
            }
            size = after;
        }
        Ok(removed as u64)
    }

    /// Bytes the database's live data occupies, from SQLite's page counters.
    ///
    /// Pages *in use*, not pages the file holds. Deleting rows moves pages to
    /// the freelist and leaves `page_count` where it was, so a limit compared
    /// against it can never be satisfied by deleting — the trim loop simply
    /// runs until the table is empty. Subtracting the freelist measures what
    /// the data is actually costing, which is the number retention is for.
    ///
    /// Three pragmas rather than a scan, so this can be asked after a batch.
    fn size_bytes(&self) -> Result<u64> {
        let pragma = |name: &str| -> Result<i64> {
            self.conn
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .with_context(|| format!("failed to read the store's {name}"))
        };
        let pages = pragma("page_count")?.max(0) as u64;
        let free = pragma("freelist_count")?.max(0) as u64;
        let size = pragma("page_size")?.max(0) as u64;
        Ok(pages.saturating_sub(free).saturating_mul(size))
    }
}

/// Most trim passes one enforcement will make.
///
/// Bounded so a single batch cannot spend unbounded time deleting; whatever is
/// still over the limit is trimmed after the next one.
const RETENTION_TRIM_PASSES: usize = 16;

/// Put an RFC 3339 timestamp into the same normal form the store writes.
///
/// Events are stored with a UTC `Z` suffix and compared as text, so a bound in
/// another offset would be compared digit by digit against a different clock.
/// Anything unparseable is passed through: a caller who writes a bare date
/// gets a prefix comparison, which is what they almost certainly meant.
fn normalize_ts(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string(),
        Err(_) => ts.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Exec, Net};

    #[test]
    fn writers_wait_for_a_short_sqlite_handoff_instead_of_dropping_immediately() {
        let store = Store::open_in_memory().unwrap();
        let timeout: u64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5_000);
    }

    fn event(kind: EventType, pid: u32, ts: &str) -> Event {
        Event {
            ts_wall: ts.to_string(),
            ts_mono_ns: 0,
            box_id: "myapp".into(),
            cgroup_id: 1,
            pid,
            tid: pid,
            ppid: 1,
            comm: "pip".into(),
            uid: 1000,
            kind,
            net: matches!(
                kind,
                EventType::Connect | EventType::Accept | EventType::Dns | EventType::Tls
            )
            .then(|| Net {
                proto: "tcp".into(),
                daddr: "151.101.0.223".into(),
                dport: 443,
                domain: "pypi.org".into(),
                ..Default::default()
            }),
            exec: (kind == EventType::Exec).then(|| Exec {
                path: "/bin/pip".into(),
                argv: vec!["pip".into(), "install".into()],
                ..Default::default()
            }),
            file: None,
            api: None,
            policy: None,
        }
    }

    #[test]
    fn insert_and_read_back() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .insert(&event(EventType::Exec, 812, "2026-08-06T22:00:00.000Z"))
            .unwrap();
        assert!(id > 0);
        assert_eq!(store.count().unwrap(), 1);

        let got = store.query(&Query::default()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, EventType::Exec);
        assert_eq!(got[0].exec.as_ref().unwrap().path, "/bin/pip");
    }

    #[test]
    fn rejects_an_invalid_event() {
        let store = Store::open_in_memory().unwrap();
        let mut bad = event(EventType::Exec, 812, "2026-08-06T22:00:00.000Z");
        bad.exec = None;
        assert!(store.insert(&bad).is_err());
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn batch_insert_skips_invalid_events_without_failing_the_batch() {
        let mut store = Store::open_in_memory().unwrap();
        let mut bad = event(EventType::Exec, 1, "2026-08-06T22:00:00.000Z");
        bad.exec = None;

        let written = store
            .insert_batch(&[
                event(EventType::Exec, 812, "2026-08-06T22:00:00.000Z"),
                bad,
                event(EventType::Connect, 812, "2026-08-06T22:00:01.000Z"),
            ])
            .unwrap();

        assert_eq!(written, 2, "the bad event is skipped, the good ones land");
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn filters_by_pid_type_peer_and_time() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                event(EventType::Exec, 812, "2026-08-06T22:00:00.000Z"),
                event(EventType::Connect, 812, "2026-08-06T22:00:01.000Z"),
                event(EventType::Connect, 900, "2026-08-06T22:00:02.000Z"),
                event(EventType::Exit, 900, "2026-08-06T22:00:03.000Z"),
            ])
            .unwrap();

        let by_pid = store
            .query(&Query {
                pid: Some(812),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_pid.len(), 2);

        let by_kind = store
            .query(&Query {
                kinds: vec![EventType::Connect],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_kind.len(), 2);

        let by_peer = store
            .query(&Query {
                peer: Some("pypi".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_peer.len(), 2, "only the network events name a peer");

        let by_time = store
            .query(&Query {
                since: Some("2026-08-06T22:00:02.000Z".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_time.len(), 2);

        let windowed = store
            .query(&Query {
                since: Some("2026-08-06T22:00:01.000Z".into()),
                until: Some("2026-08-06T22:00:03.000Z".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(windowed.len(), 2, "until is exclusive");
    }

    #[test]
    fn combined_filters_are_conjunctive() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                event(EventType::Connect, 812, "2026-08-06T22:00:01.000Z"),
                event(EventType::Connect, 900, "2026-08-06T22:00:02.000Z"),
                event(EventType::Exec, 812, "2026-08-06T22:00:03.000Z"),
            ])
            .unwrap();

        let got = store
            .query(&Query {
                pid: Some(812),
                kinds: vec![EventType::Connect],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn ordering_is_selectable_and_stable_within_a_millisecond() {
        let mut store = Store::open_in_memory().unwrap();
        // Three events sharing a timestamp: insertion order must still decide.
        store
            .insert_batch(&[
                event(EventType::Exec, 1, "2026-08-06T22:00:00.000Z"),
                event(EventType::Exit, 2, "2026-08-06T22:00:00.000Z"),
                event(EventType::Exit, 3, "2026-08-06T22:00:00.000Z"),
            ])
            .unwrap();

        let oldest = store.query(&Query::default()).unwrap();
        assert_eq!(
            oldest.iter().map(|e| e.pid).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let newest = store
            .query(&Query {
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            newest.iter().map(|e| e.pid).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn limits_are_defaulted_and_clamped() {
        assert_eq!(Query::default().effective_limit(), Query::DEFAULT_LIMIT);
        assert_eq!(
            Query {
                limit: Some(0),
                ..Default::default()
            }
            .effective_limit(),
            1,
            "a zero limit would return nothing and look like a bug"
        );
        assert_eq!(
            Query {
                limit: Some(usize::MAX),
                ..Default::default()
            }
            .effective_limit(),
            Query::MAX_LIMIT
        );

        let mut store = Store::open_in_memory().unwrap();
        let many: Vec<Event> = (1..=20)
            .map(|i| event(EventType::Exit, i, "2026-08-06T22:00:00.000Z"))
            .collect();
        store.insert_batch(&many).unwrap();

        let got = store
            .query(&Query {
                limit: Some(5),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn counts_by_type_and_reports_the_latest_timestamp() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.latest_ts().unwrap().is_none());

        store
            .insert_batch(&[
                event(EventType::Exec, 1, "2026-08-06T22:00:00.000Z"),
                event(EventType::Connect, 1, "2026-08-06T22:00:01.000Z"),
                event(EventType::Connect, 1, "2026-08-06T22:00:02.000Z"),
            ])
            .unwrap();

        let hist = store.count_by_type().unwrap();
        assert_eq!(hist.len(), 2);
        let connect = hist.iter().find(|(k, _)| *k == EventType::Connect).unwrap();
        assert_eq!(connect.1, 2);

        assert_eq!(
            store.latest_ts().unwrap().as_deref(),
            Some("2026-08-06T22:00:02.000Z")
        );
    }

    #[test]
    fn distinct_peers_deduplicates() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                event(EventType::Connect, 1, "2026-08-06T22:00:00.000Z"),
                event(EventType::Connect, 2, "2026-08-06T22:00:01.000Z"),
                event(EventType::Exec, 3, "2026-08-06T22:00:02.000Z"),
            ])
            .unwrap();

        let peers = store.distinct_peers(None).unwrap();
        assert_eq!(peers, vec!["pypi.org"]);
    }

    #[test]
    fn retention_trims_by_size_as_well_as_by_count() {
        // A row count is not a size: at the transport's frame limit, the
        // default two million rows is two terabytes, so a guest sending large
        // events fills the volume without ever approaching the count.
        let store = Store::open_in_memory().unwrap();
        let retention = Retention {
            max_events: 1_000_000,
            max_bytes: 256 * 1024,
        };
        for pid in 1..=400u32 {
            let mut e = event(EventType::Exec, pid, "2026-08-06T22:14:01.000Z");
            e.comm = "x".repeat(2048);
            store.insert(&e).unwrap();
            store.enforce_retention(retention).unwrap();
        }

        assert!(
            store.count().unwrap() < 400,
            "nothing was trimmed, so the count was doing all the work"
        );
        assert!(
            store.count().unwrap() > 20,
            "a sweep meant to bound the store emptied it instead"
        );
        assert!(
            store.size_bytes().unwrap() <= retention.max_bytes * 2,
            "the store stayed far above its byte ceiling"
        );
        // And the survivors are the newest ones.
        let newest = store
            .query(&Query {
                limit: Some(1),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(newest[0].pid, 400);
    }

    #[test]
    fn retention_drops_the_oldest() {
        let mut store = Store::open_in_memory().unwrap();
        let many: Vec<Event> = (1..=100)
            .map(|i| event(EventType::Exit, i, "2026-08-06T22:00:00.000Z"))
            .collect();
        store.insert_batch(&many).unwrap();

        let removed = store
            .enforce_retention(Retention {
                max_events: 10,
                ..Retention::default()
            })
            .unwrap();
        assert_eq!(removed, 90);
        assert_eq!(store.count().unwrap(), 10);

        // The survivors are the newest.
        let left = store.query(&Query::default()).unwrap();
        assert_eq!(left.first().unwrap().pid, 91);

        // Running it again with room to spare removes nothing.
        assert_eq!(
            store
                .enforce_retention(Retention {
                    max_events: 1000,
                    ..Retention::default()
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn opens_a_file_backed_store_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes/myapp/events.db");

        {
            let store = Store::open(&path).unwrap();
            store
                .insert(&event(EventType::Exec, 1, "2026-08-06T22:00:00.000Z"))
                .unwrap();
        }

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.count().unwrap(), 1, "the store survived a reopen");
    }

    #[test]
    fn a_stores_generation_is_stable_for_its_life_and_new_for_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");

        let first = Store::open(&path).unwrap();
        let generation = first.generation().unwrap();
        assert_ne!(generation, 0, "zero means no store at all");

        // Writing, and the checkpointing that follows it, must not change it —
        // file metadata does, which is why this does not come from the file.
        first
            .insert(&event(EventType::Exec, 1, "2026-08-06T22:14:01.000Z"))
            .unwrap();
        first
            .conn
            .execute_batch("PRAGMA wal_checkpoint(FULL);")
            .unwrap();
        assert_eq!(
            first.generation().unwrap(),
            generation,
            "a write is not a replacement"
        );
        drop(first);
        assert_eq!(
            Store::open(&path).unwrap().generation().unwrap(),
            generation
        );

        // A box destroyed and recreated under the same name gets a new one.
        std::fs::remove_file(&path).unwrap();
        assert_ne!(
            Store::open(&path).unwrap().generation().unwrap(),
            generation
        );
    }

    #[test]
    fn an_empty_store_has_a_tail_anchor_of_zero() {
        // Zero rather than an error or an Option: a live tail's first request
        // is `id > 0`, which is every row there will ever be.
        assert_eq!(Store::open_in_memory().unwrap().max_id().unwrap(), 0);
    }

    #[test]
    fn tailing_from_an_anchor_returns_only_what_arrived_after_it() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert(&event(EventType::Exec, 1, "2026-08-06T22:14:01.000Z"))
            .unwrap();
        let anchor = store.max_id().unwrap();
        store
            .insert(&event(EventType::Exec, 2, "2026-08-06T22:14:02.000Z"))
            .unwrap();
        store
            .insert(&event(EventType::Exec, 3, "2026-08-06T22:14:03.000Z"))
            .unwrap();

        let (fresh, scanned_to) = store
            .tail_scan(anchor, store.max_id().unwrap(), 100, 1 << 20)
            .unwrap();

        assert_eq!(fresh.len(), 2, "the anchored row is excluded");
        assert_eq!(fresh[0].pid, 2, "oldest first, so the console appends");
        assert_eq!(scanned_to, store.max_id().unwrap(), "the next anchor");
    }

    #[test]
    fn a_tail_advances_past_rows_it_could_not_decode() {
        // Otherwise an upgrade that leaves undecodable rows in front of the
        // anchor wedges the live stream permanently: every request rediscovers
        // the same rows, discards them, returns nothing, and never moves.
        let store = Store::open_in_memory().unwrap();
        let anchor = store.max_id().unwrap();
        for _ in 0..3 {
            store
                .conn
                .execute(
                    "INSERT INTO events
                       (ts_wall, ts_mono_ns, box_id, cgroup_id, pid, tid, ppid, comm, uid,
                        type, peer, dport, path, raw)
                     VALUES ('2026-08-06T22:14:01.000Z',1,'myapp',1,1,1,1,'x',0,
                             'exec',NULL,NULL,NULL,'{\"from\":\"the future\"}')",
                    [],
                )
                .unwrap();
        }
        store
            .insert(&event(EventType::Exec, 9, "2026-08-06T22:14:02.000Z"))
            .unwrap();

        // A page-sized scan that only reaches the undecodable rows still moves.
        let (events, scanned_to) = store
            .tail_scan(anchor, store.max_id().unwrap(), 3, 1 << 20)
            .unwrap();
        assert!(events.is_empty(), "none of them decode");
        assert!(scanned_to > anchor, "but the scan position advanced");

        let (events, _) = store
            .tail_scan(scanned_to, store.max_id().unwrap(), 3, 1 << 20)
            .unwrap();
        assert_eq!(events.len(), 1, "and the next page reaches the good row");
        assert_eq!(events[0].pid, 9);
    }

    #[test]
    fn two_events_in_the_same_millisecond_both_survive_a_tail() {
        // The reason the anchor is an id and not a timestamp. A `ts_wall`
        // anchor either repeats the tied row or drops it; neither is a live
        // stream anyone can trust.
        let store = Store::open_in_memory().unwrap();
        let anchor = store.max_id().unwrap();
        store
            .insert(&event(EventType::Exec, 7, "2026-08-06T22:14:01.412Z"))
            .unwrap();
        store
            .insert(&event(EventType::Connect, 7, "2026-08-06T22:14:01.412Z"))
            .unwrap();

        assert_eq!(
            store
                .tail_scan(anchor, store.max_id().unwrap(), 100, 1 << 20)
                .unwrap()
                .0
                .len(),
            2
        );
    }

    #[test]
    fn an_export_is_bounded_by_bytes_and_says_when_it_was_cut() {
        let store = Store::open_in_memory().unwrap();
        for i in 1..=6u32 {
            let mut e = event(EventType::Exec, i, "2026-08-06T22:14:01.000Z");
            e.comm = "x".repeat(400);
            store.insert(&e).unwrap();
        }

        // Whole window, no cut.
        let (all, truncated) = store.export(None, 100, 1 << 20).unwrap();
        assert_eq!(all.len(), 6);
        assert!(!truncated);

        // A byte budget stops it, and says so — a row limit alone bounds the
        // count and not the memory.
        let (some, truncated) = store.export(None, 100, 900).unwrap();
        assert!(some.len() < 6);
        assert!(truncated, "a cut window must not read as a complete one");
    }

    #[test]
    fn an_export_reports_truncation_from_the_scan_not_from_what_decoded() {
        // A row an older schema wrote is dropped on the way out. Inferring
        // "not truncated" from a short result then let an export claim to
        // cover a window whose tail it had never reached.
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO events
                   (ts_wall, ts_mono_ns, box_id, cgroup_id, pid, tid, ppid, comm, uid,
                    type, peer, dport, path, raw)
                 VALUES ('2026-08-06T22:14:01.000Z',1,'myapp',1,1,1,1,'x',0,
                         'exec',NULL,NULL,NULL,'{\"from\":\"the future\"}')",
                [],
            )
            .unwrap();
        store
            .insert(&event(EventType::Exec, 2, "2026-08-06T22:14:02.000Z"))
            .unwrap();
        store
            .insert(&event(EventType::Exec, 3, "2026-08-06T22:14:03.000Z"))
            .unwrap();

        let (events, truncated) = store.export(None, 2, 1 << 20).unwrap();
        assert_eq!(events.len(), 1, "one of the two scanned rows decoded");
        assert!(truncated, "the scan stopped at its row limit");

        // And a window that ends exactly on the limit is complete, not cut.
        let (events, truncated) = store.export(None, 3, 1 << 20).unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            truncated,
            "still incomplete: one row of the three did not decode"
        );
        let clean = Store::open_in_memory().unwrap();
        for pid in 1..=3u32 {
            clean
                .insert(&event(EventType::Exec, pid, "2026-08-06T22:14:01.000Z"))
                .unwrap();
        }
        let (events, truncated) = clean.export(None, 3, 1 << 20).unwrap();
        assert_eq!(events.len(), 3);
        assert!(!truncated, "exactly the limit is not a cut-off window");
    }

    #[test]
    fn an_oversized_event_is_skipped_without_stalling_the_scan() {
        // The transport accepts frames up to a megabyte. A row limit bounds
        // the count and not the bytes, so the console asks for a size bound —
        // but an oversized row must still advance the scan, or it wedges the
        // tail the way an undecodable one did.
        let store = Store::open_in_memory().unwrap();
        let anchor = store.max_id().unwrap();
        let mut fat = event(EventType::Exec, 1, "2026-08-06T22:14:01.000Z");
        fat.comm = "x".repeat(4096);
        store.insert(&fat).unwrap();
        store
            .insert(&event(EventType::Exec, 2, "2026-08-06T22:14:02.000Z"))
            .unwrap();

        let newest = store.max_id().unwrap();
        let (events, scanned_to) = store.tail_scan(anchor, newest, 10, 1024).unwrap();
        assert_eq!(events.len(), 1, "only the small one is rendered");
        assert_eq!(events[0].pid, 2);
        assert_eq!(scanned_to, newest, "the scan still reached the end");

        // Measured in bytes. SQLite's `LENGTH` on TEXT counts characters, so
        // a cap enforced that way lets a multibyte payload through at several
        // times its stated size.
        let mut wide = event(EventType::Exec, 3, "2026-08-06T22:14:03.000Z");
        wide.comm = "\u{1f600}".repeat(400); // 400 chars, 1600 bytes
        store.insert(&wide).unwrap();
        let bounded = store
            .query(&Query {
                max_bytes: Some(1024),
                ..Default::default()
            })
            .unwrap();
        assert!(
            bounded.iter().all(|e| e.pid != 3),
            "a 1600-byte comm passed a 1 KiB cap"
        );

        // The same bound as a plain filter, for the window query.
        let window = store
            .query(&Query {
                max_bytes: Some(1024),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(window.len(), 1);

        // And nothing is lost: the export path asks for no bound.
        assert_eq!(store.query(&Query::default()).unwrap().len(), 3);
    }

    #[test]
    fn a_page_can_be_pinned_to_the_store_as_it_was() {
        // The truncation path reads the high-water mark, then queries. Without
        // an upper bound, a row inserted between the two is inside the page
        // but outside the cursor the page advances to — and arrives twice.
        let store = Store::open_in_memory().unwrap();
        let anchor = store.max_id().unwrap();
        store
            .insert(&event(EventType::Exec, 1, "2026-08-06T22:14:01.000Z"))
            .unwrap();
        let mark = store.max_id().unwrap();
        store
            .insert(&event(EventType::Exec, 2, "2026-08-06T22:14:02.000Z"))
            .unwrap();

        let page = store
            .query(&Query {
                after_id: Some(anchor),
                before_id: Some(mark),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.len(), 1, "the later row is not in this page");
        assert_eq!(page[0].pid, 1);
    }

    #[test]
    fn a_tail_anchor_composes_with_the_other_filters() {
        let store = Store::open_in_memory().unwrap();
        let anchor = store.max_id().unwrap();
        store
            .insert(&event(EventType::Exec, 1, "2026-08-06T22:14:01.000Z"))
            .unwrap();
        store
            .insert(&event(EventType::Connect, 1, "2026-08-06T22:14:02.000Z"))
            .unwrap();

        let fresh = store
            .query(&Query {
                after_id: Some(anchor),
                kinds: vec![EventType::Connect],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(fresh.len(), 1, "a filtered live view stays filtered");
        assert_eq!(fresh[0].kind, EventType::Connect);
    }
}
