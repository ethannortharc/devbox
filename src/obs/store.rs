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
    pub kinds: Vec<EventType>,
    /// Substring match against the peer (domain, SNI, or address).
    pub peer: Option<String>,
    /// Substring match against the path.
    pub path: Option<String>,
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
}

impl Default for Retention {
    fn default() -> Self {
        // Roughly the 500 MiB the design suggests, at a few hundred bytes per
        // event. Counting rows rather than bytes keeps enforcement a single
        // cheap statement instead of a page-count query per insert.
        Self {
            max_events: 2_000_000,
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
            "#,
        )
        .context("failed to create the event schema")?;

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

    /// Run a query.
    pub fn query(&self, q: &Query) -> Result<Vec<Event>> {
        let mut sql = String::from("SELECT raw FROM events WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(since) = &q.since {
            sql.push_str(" AND ts_wall >= ?");
            args.push(Box::new(since.clone()));
        }
        if let Some(until) = &q.until {
            sql.push_str(" AND ts_wall < ?");
            args.push(Box::new(until.clone()));
        }
        if let Some(pid) = q.pid {
            sql.push_str(" AND pid = ?");
            args.push(Box::new(pid));
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
                row.get::<_, String>(0)
            })
            .context("failed to run query")?;

        let mut out = Vec::new();
        for row in rows {
            let raw = row.context("failed to read a row")?;
            match serde_json::from_str(&raw) {
                Ok(event) => out.push(event),
                // A row that no longer parses is a schema drift, not a reason
                // to fail the whole query — log it and keep the rest usable.
                Err(e) => tracing::warn!(error = %e, "stored event no longer decodes"),
            }
        }
        Ok(out)
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
        let removed = self
            .conn
            .execute(
                "DELETE FROM events WHERE id <= (
                     SELECT MAX(id) - ?1 FROM events
                 )",
                params![retention.max_events as i64],
            )
            .context("failed to enforce retention")?;
        Ok(removed as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::event::{Exec, Net};

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
    fn retention_drops_the_oldest() {
        let mut store = Store::open_in_memory().unwrap();
        let many: Vec<Event> = (1..=100)
            .map(|i| event(EventType::Exit, i, "2026-08-06T22:00:00.000Z"))
            .collect();
        store.insert_batch(&many).unwrap();

        let removed = store
            .enforce_retention(Retention { max_events: 10 })
            .unwrap();
        assert_eq!(removed, 90);
        assert_eq!(store.count().unwrap(), 10);

        // The survivors are the newest.
        let left = store.query(&Query::default()).unwrap();
        assert_eq!(left.first().unwrap().pid, 91);

        // Running it again with room to spare removes nothing.
        assert_eq!(
            store
                .enforce_retention(Retention { max_events: 1000 })
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
}
