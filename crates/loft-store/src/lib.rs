//! SQLite persistence (ADR-0006).
//!
//! Domain events are appended to `events` and applied to projection tables in
//! the same transaction. Projections store the serialized entity in `data` plus
//! a few indexed columns, so the wire model stays the single source of shape.

use anyhow::{Context as _, Result};
use loft_protocol::{
    Event, EventEnvelope, Item, Project, ProjectId, RuntimeRequest, Thread, ThreadId, Turn,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE events (
        seq       INTEGER PRIMARY KEY AUTOINCREMENT,
        thread_id TEXT,
        at_ms     INTEGER NOT NULL,
        type      TEXT NOT NULL,
        payload   TEXT NOT NULL
    );
    CREATE INDEX events_thread ON events(thread_id, seq);

    CREATE TABLE projects (
        id            TEXT PRIMARY KEY,
        root_path     TEXT NOT NULL UNIQUE,
        created_at_ms INTEGER NOT NULL,
        data          TEXT NOT NULL
    );
    CREATE TABLE threads (
        id            TEXT PRIMARY KEY,
        project_id    TEXT NOT NULL REFERENCES projects(id),
        updated_at_ms INTEGER NOT NULL,
        data          TEXT NOT NULL
    );
    CREATE INDEX threads_project ON threads(project_id);
    CREATE TABLE turns (
        id        TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL,
        ordinal   INTEGER NOT NULL,
        data      TEXT NOT NULL
    );
    CREATE INDEX turns_thread ON turns(thread_id, ordinal);
    CREATE TABLE items (
        id        TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL,
        ordinal   INTEGER NOT NULL,
        data      TEXT NOT NULL
    );
    CREATE INDEX items_thread ON items(thread_id, ordinal);
    CREATE TABLE requests (
        id        TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL,
        data      TEXT NOT NULL
    );
    CREATE INDEX requests_thread ON requests(thread_id);
    CREATE TABLE processes (
        pid           INTEGER PRIMARY KEY,
        started_at_ms INTEGER NOT NULL,
        kind          TEXT NOT NULL,
        thread_id     TEXT
    );
    "#,
];

pub struct Store {
    conn: Connection,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: usize =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
            tracing::info!(version = i + 1, "applied migration");
        }
        Ok(())
    }

    /// Online backup (`VACUUM INTO`), taken before upgrades.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
        Ok(())
    }

    // -- writes -------------------------------------------------------------

    /// Append events and apply them to projections atomically.
    pub fn commit(&mut self, events: Vec<Event>) -> Result<Vec<EventEnvelope>> {
        let tx = self.conn.transaction()?;
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            let at_ms = now_ms();
            let payload = serde_json::to_string(&event)?;
            tx.execute(
                "INSERT INTO events(thread_id, at_ms, type, payload) VALUES (?1, ?2, ?3, ?4)",
                params![
                    event.thread_id().map(|t| t.as_str()),
                    at_ms,
                    event_type(&event),
                    payload
                ],
            )?;
            let seq = tx.last_insert_rowid() as u64;
            apply(&tx, &event)?;
            out.push(EventEnvelope { seq, at_ms, event });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Projection-only update for a streaming item (no event appended).
    pub fn snapshot_item(&mut self, item: &Item) -> Result<()> {
        upsert_item(&self.conn, item)
    }

    pub fn record_process(&self, pid: u32, kind: &str, thread_id: Option<&ThreadId>) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO processes(pid, started_at_ms, kind, thread_id) VALUES (?1, ?2, ?3, ?4)",
            params![pid, now_ms(), kind, thread_id.map(|t| t.as_str())],
        )?;
        Ok(())
    }

    pub fn forget_process(&self, pid: u32) -> Result<()> {
        self.conn
            .execute("DELETE FROM processes WHERE pid = ?1", params![pid])?;
        Ok(())
    }

    pub fn processes(&self) -> Result<Vec<(u32, String)>> {
        let mut stmt = self.conn.prepare("SELECT pid, kind FROM processes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // -- reads --------------------------------------------------------------

    pub fn max_seq(&self) -> Result<u64> {
        let seq: Option<i64> = self
            .conn
            .query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))?;
        Ok(seq.unwrap_or(0) as u64)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        self.query_data("SELECT data FROM projects ORDER BY created_at_ms", [])
    }

    pub fn project(&self, id: &ProjectId) -> Result<Option<Project>> {
        self.query_one("SELECT data FROM projects WHERE id = ?1", [id.as_str()])
    }

    pub fn project_by_path(&self, root_path: &str) -> Result<Option<Project>> {
        self.query_one(
            "SELECT data FROM projects WHERE root_path = ?1",
            [root_path],
        )
    }

    pub fn threads(&self) -> Result<Vec<Thread>> {
        self.query_data("SELECT data FROM threads ORDER BY updated_at_ms DESC", [])
    }

    pub fn thread(&self, id: &ThreadId) -> Result<Option<Thread>> {
        self.query_one("SELECT data FROM threads WHERE id = ?1", [id.as_str()])
    }

    pub fn turns(&self, thread: &ThreadId) -> Result<Vec<Turn>> {
        self.query_data(
            "SELECT data FROM turns WHERE thread_id = ?1 ORDER BY ordinal",
            [thread.as_str()],
        )
    }

    pub fn items(&self, thread: &ThreadId) -> Result<Vec<Item>> {
        self.query_data(
            "SELECT data FROM items WHERE thread_id = ?1 ORDER BY ordinal",
            [thread.as_str()],
        )
    }

    /// Items whose stored form mentions `text` (case-insensitive for
    /// ASCII), in timeline order per thread. A coarse prefilter: callers
    /// check the fields they care about.
    pub fn items_mentioning(&self, text: &str) -> Result<Vec<Item>> {
        // Match the text as it appears inside the JSON (quotes, newlines
        // and backslashes are escaped there), as a literal LIKE pattern.
        let json = serde_json::to_string(text)?;
        let inner = &json[1..json.len() - 1];
        let mut pattern = String::from("%");
        for c in inner.chars() {
            if matches!(c, '%' | '_' | '|') {
                pattern.push('|');
            }
            pattern.push(c);
        }
        pattern.push('%');
        self.query_data(
            "SELECT data FROM items WHERE data LIKE ?1 ESCAPE '|' ORDER BY thread_id, ordinal",
            [pattern],
        )
    }

    pub fn requests(&self, thread: &ThreadId) -> Result<Vec<RuntimeRequest>> {
        self.query_data(
            "SELECT data FROM requests WHERE thread_id = ?1",
            [thread.as_str()],
        )
    }

    /// All turns that were left running (crash recovery).
    pub fn running_turns(&self) -> Result<Vec<Turn>> {
        self.query_data(
            "SELECT data FROM turns WHERE json_extract(data, '$.status') = 'running'",
            [],
        )
    }

    pub fn open_requests(&self) -> Result<Vec<RuntimeRequest>> {
        self.query_data(
            "SELECT data FROM requests WHERE json_extract(data, '$.status') = 'open'",
            [],
        )
    }

    pub fn in_progress_items(&self) -> Result<Vec<Item>> {
        self.query_data(
            "SELECT data FROM items WHERE json_extract(data, '$.status') = 'in_progress'",
            [],
        )
    }

    pub fn next_item_ordinal(&self, thread: &ThreadId) -> Result<u64> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(ordinal) FROM items WHERE thread_id = ?1",
            [thread.as_str()],
            |r| r.get(0),
        )?;
        Ok(max.map(|m| m as u64 + 1).unwrap_or(0))
    }

    pub fn next_turn_ordinal(&self, thread: &ThreadId) -> Result<u32> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(ordinal) FROM turns WHERE thread_id = ?1",
            [thread.as_str()],
            |r| r.get(0),
        )?;
        Ok(max.map(|m| m as u32 + 1).unwrap_or(1))
    }

    /// Events after `seq`, optionally limited to one thread.
    pub fn events_after(&self, seq: u64, thread: Option<&ThreadId>) -> Result<Vec<EventEnvelope>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, at_ms, payload FROM events
             WHERE seq > ?1 AND (?2 IS NULL OR thread_id = ?2) ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![seq as i64, thread.map(|t| t.as_str())], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, at_ms, payload) = row?;
            out.push(EventEnvelope {
                seq: seq as u64,
                at_ms,
                event: serde_json::from_str(&payload)?,
            });
        }
        Ok(out)
    }

    fn query_data<T: DeserializeOwned, P: rusqlite::Params>(
        &self,
        sql: &str,
        p: P,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    fn query_one<T: DeserializeOwned, P: rusqlite::Params>(
        &self,
        sql: &str,
        p: P,
    ) -> Result<Option<T>> {
        let data: Option<String> = self.conn.query_row(sql, p, |r| r.get(0)).optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into))
            .transpose()
    }
}

fn event_type(event: &Event) -> &'static str {
    match event {
        Event::ProjectAdded { .. } => "project_added",
        Event::ProjectUpdated { .. } => "project_updated",
        Event::ProjectRemoved { .. } => "project_removed",
        Event::ThreadCreated { .. } => "thread_created",
        Event::ThreadUpdated { .. } => "thread_updated",
        Event::TurnStarted { .. } => "turn_started",
        Event::TurnUpdated { .. } => "turn_updated",
        Event::ItemUpserted { .. } => "item_upserted",
        Event::RequestOpened { .. } => "request_opened",
        Event::RequestUpdated { .. } => "request_updated",
        Event::ThreadDeleted { .. } => "thread_deleted",
    }
}

fn json<T: Serialize>(v: &T) -> rusqlite::Result<String> {
    serde_json::to_string(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Apply one event to the projection tables.
fn apply(conn: &Connection, event: &Event) -> Result<()> {
    match event {
        Event::ProjectAdded { project } => {
            conn.execute(
                "INSERT INTO projects(id, root_path, created_at_ms, data) VALUES (?1, ?2, ?3, ?4)",
                params![
                    project.id.as_str(),
                    project.root_path,
                    project.created_at_ms,
                    json(project)?
                ],
            )?;
        }
        Event::ProjectUpdated { project } => {
            conn.execute(
                "UPDATE projects SET data = ?2 WHERE id = ?1",
                params![project.id.as_str(), json(project)?],
            )?;
        }
        Event::ProjectRemoved { project_id } => {
            conn.execute(
                "DELETE FROM projects WHERE id = ?1",
                params![project_id.as_str()],
            )?;
        }
        Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => {
            conn.execute(
                "INSERT INTO threads(id, project_id, updated_at_ms, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET updated_at_ms = excluded.updated_at_ms, data = excluded.data",
                params![thread.id.as_str(), thread.project_id.as_str(), thread.updated_at_ms, json(thread)?],
            )?;
        }
        Event::TurnStarted { turn } | Event::TurnUpdated { turn } => {
            conn.execute(
                "INSERT INTO turns(id, thread_id, ordinal, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET data = excluded.data",
                params![
                    turn.id.as_str(),
                    turn.thread_id.as_str(),
                    turn.ordinal,
                    json(turn)?
                ],
            )?;
        }
        Event::ItemUpserted { item } => upsert_item(conn, item)?,
        Event::ThreadDeleted { thread_id } => {
            let id = thread_id.as_str();
            for sql in [
                "DELETE FROM items WHERE thread_id = ?1",
                "DELETE FROM turns WHERE thread_id = ?1",
                "DELETE FROM requests WHERE thread_id = ?1",
                "DELETE FROM threads WHERE id = ?1",
                // The log keeps only the deletion itself.
                "DELETE FROM events WHERE thread_id = ?1 AND type != 'thread_deleted'",
            ] {
                conn.execute(sql, params![id])?;
            }
        }
        Event::RequestOpened { request } | Event::RequestUpdated { request } => {
            conn.execute(
                "INSERT INTO requests(id, thread_id, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET data = excluded.data",
                params![
                    request.id.as_str(),
                    request.thread_id.as_str(),
                    json(request)?
                ],
            )?;
        }
    }
    Ok(())
}

fn upsert_item(conn: &Connection, item: &Item) -> Result<()> {
    conn.execute(
        "INSERT INTO items(id, thread_id, ordinal, data) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        params![
            item.id.as_str(),
            item.thread_id.as_str(),
            item.ordinal as i64,
            json(item)?
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::*;

    fn project() -> Project {
        Project {
            id: ProjectId::new(),
            name: "p".into(),
            root_path: "/tmp/p".into(),
            created_at_ms: 1,
        }
    }

    fn thread(project: &Project) -> Thread {
        Thread {
            id: ThreadId::new(),
            project_id: project.id.clone(),
            title: "t".into(),
            harness: HarnessId::claude(),
            cwd: project.root_path.clone(),
            model: None,
            permission_mode: None,
            effort: None,
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: 1,
            updated_at_ms: 1,
            workspace: Default::default(),
            parent: None,
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: None,
            draft: None,
            waiting_on: Vec::new(),
        }
    }

    #[test]
    fn commit_applies_projections_and_assigns_seq() {
        let mut store = Store::open_in_memory().unwrap();
        let p = project();
        let mut t = thread(&p);
        let env = store
            .commit(vec![
                Event::ProjectAdded { project: p.clone() },
                Event::ThreadCreated { thread: t.clone() },
            ])
            .unwrap();
        assert_eq!(env.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        t.status = ThreadStatus::Running;
        store
            .commit(vec![Event::ThreadUpdated { thread: t.clone() }])
            .unwrap();

        assert_eq!(store.projects().unwrap(), vec![p.clone()]);
        assert_eq!(
            store.thread(&t.id).unwrap().unwrap().status,
            ThreadStatus::Running
        );
        assert_eq!(store.project_by_path("/tmp/p").unwrap().unwrap().id, p.id);
        assert_eq!(store.max_seq().unwrap(), 3);
        assert_eq!(store.events_after(1, Some(&t.id)).unwrap().len(), 2);
        assert_eq!(store.events_after(0, None).unwrap().len(), 3);
    }

    #[test]
    fn running_turn_queries() {
        let mut store = Store::open_in_memory().unwrap();
        let p = project();
        let t = thread(&p);
        let turn = Turn {
            id: TurnId::new(),
            thread_id: t.id.clone(),
            ordinal: store.next_turn_ordinal(&t.id).unwrap(),
            status: TurnStatus::Running,
            started_at_ms: 1,
            ended_at_ms: None,
            usage: None,
            error: None,
            checkpoint_before: None,
            checkpoint_after: None,
            native_turn: None,
        };
        store
            .commit(vec![
                Event::ProjectAdded { project: p },
                Event::ThreadCreated { thread: t.clone() },
                Event::TurnStarted { turn: turn.clone() },
            ])
            .unwrap();
        assert_eq!(turn.ordinal, 1);
        assert_eq!(store.running_turns().unwrap().len(), 1);
        assert_eq!(store.next_turn_ordinal(&t.id).unwrap(), 2);
    }

    #[test]
    fn reopening_keeps_data_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .commit(vec![Event::ProjectAdded { project: project() }])
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.projects().unwrap().len(), 1);
        store.backup_to(&dir.path().join("backup.sqlite")).unwrap();
    }
}
