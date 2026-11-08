//! Session search — SQLite + FTS5 over past conversation turns
//!
//! DB: ~/.rijo/data/sessions.db
//! Schema:
//!   sessions(id TEXT PRIMARY KEY, created_at TEXT)
//!   messages(id INTEGER PK AUTOINCREMENT, session_id TEXT, role TEXT,
//!            content TEXT, ts TEXT)
//!   message_fts (FTS5, content)  — full-text index over messages.content
//!
//!   summaries(id INTEGER PK AUTOINCREMENT, session_id TEXT, upto_message_id INTEGER,
//!             summary TEXT, summary_tokens INTEGER, created_at TEXT)
//!     — compression blocks with a watermark (messages.id). The watermark is what
//!       makes a compression cost O(new messages) instead of O(the whole
//!       history): only messages above the last watermark are ever summarized
//!       again, and the result is durable across restarts.
//!
//! Tools see results as {session_id, role, ts, excerpt}.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct SessionSearch;

fn db_path(state: &AppState) -> std::path::PathBuf {
    state.config.data_dir.join("sessions.db")
}

/// Id of the running session. One place, because the executor writes into it and
/// the context tool reads from it — two copies would drift apart silently.
pub fn current_session_id() -> String {
    format!("sess_{}", chrono::Utc::now().format("%Y%m%d"))
}

pub fn init_db(state: &AppState) -> Result<Connection, String> {
    let path = db_path(state);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            ts TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            content, content='messages', content_rowid='id'
        );
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO message_fts(rowid, content) VALUES (new.id, new.content);
        END;
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
        CREATE TABLE IF NOT EXISTS summaries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            upto_message_id INTEGER NOT NULL,
            summary TEXT NOT NULL,
            summary_tokens INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_summaries_session
            ON summaries(session_id, upto_message_id);",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Record a message into the session store (called by executor)
pub fn record_message(state: &AppState, session_id: &str, role: &str, content: &str) {
    if let Ok(conn) = init_db(state) {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES (?1, ?2)",
            rusqlite::params![session_id, chrono::Utc::now().to_rfc3339()],
        );
        let _ = conn.execute(
            "INSERT INTO messages(session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, role, content, chrono::Utc::now().to_rfc3339()],
        );
    }
}

#[async_trait]
impl AsyncTool for SessionSearch {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "session_search",
            description: "Search past sessions. Use for 'what did we do about X' recall.",
            emoji: "🔍",
            toolset: "core",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &SESSION_SEARCH_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if query.is_empty() {
            return tool_error("query is required");
        }
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as i64;

        let conn =
            match tokio::task::spawn_blocking(move || Connection::open(db_path(&state))).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return tool_error(format!("Cannot open sessions db: {e}")),
                Err(_) => return tool_error("db task failed"),
            };

        let query = format!("\"{query}\"*"); // phrase prefix search
        let mut stmt = match conn.prepare(
            "SELECT m.session_id, m.role, m.content, m.ts
             FROM message_fts f JOIN messages m ON m.id = f.rowid
             WHERE message_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => return tool_error(format!("FTS query failed: {e}")),
        };

        let rows = match stmt.query_map(rusqlite::params![query, limit], |row| {
            Ok(json!({
                "session_id": row.get::<_, String>(0)?,
                "role": row.get::<_, String>(1)?,
                "content": row.get::<_, String>(2)?,
                "ts": row.get::<_, String>(3)?,
            }))
        }) {
            Ok(r) => r,
            Err(e) => return tool_error(format!("query: {e}")),
        };
        let results: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        json!({ "results": results, "count": results.len() }).to_string()
    }
}

lazy_static::lazy_static! {
    static ref SESSION_SEARCH_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query" },
            "limit": { "type": "integer", "description": "Max results (default 5)" }
        },
        "required": ["query"]
    }));
}

/// Load the last N messages of the most recent session (for --resume).
/// Returns Vec<(role, content)> in chronological order.
/// Writes many messages over ONE connection.
///
/// `record_message` opens the database for every single message (fine for the
/// interactive loop, where one message arrives at a time), but a turn that
/// persists its exchange or a compression that imports a block would pay a
/// connection setup per line. The writer also lets the caller use the returned
/// ids — the compression needs the watermark of the block it just wrote.
pub struct SessionWriter {
    conn: Connection,
    session_id: String,
}

impl SessionWriter {
    pub fn open(state: &AppState, session_id: &str) -> Result<Self, String> {
        let conn = init_db(state)?;
        conn.execute(
            "INSERT OR IGNORE INTO sessions(id, created_at) VALUES (?1, ?2)",
            rusqlite::params![session_id, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn,
            session_id: session_id.to_string(),
        })
    }

    /// Appends one message and returns its id (the watermark unit).
    pub fn push(&self, role: &str, content: &str) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO messages(session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    self.session_id,
                    role,
                    content,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Id of the newest message of this session (0 when there is none).
    pub fn newest_message_id(&self) -> Result<i64, String> {
        newest_message_id(&self.conn, &self.session_id)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

pub fn load_recent_messages(state: &AppState, limit: usize) -> Option<Vec<(String, String)>> {
    load_recent_session(state, limit).map(|(_, messages)| messages)
}

/// Load the recent messages AND their persistence id for resuming the last session.
pub fn load_recent_session(
    state: &AppState,
    limit: usize,
) -> Option<(String, Vec<(String, String)>)> {
    let conn = Connection::open(db_path(state)).ok()?;
    let last_sid: String = {
        let mut stmt = conn
            .prepare("SELECT session_id FROM messages ORDER BY ts DESC LIMIT 1")
            .ok()?;
        stmt.query_row([], |r| r.get(0)).ok()?
    };
    if last_sid.is_empty() {
        return None;
    }
    let all: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY ts ASC")
            .ok()?;
        let q = stmt
            .query_map(rusqlite::params![last_sid], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()?;
        q.flatten().collect()
    };
    let start = all.len().saturating_sub(limit);
    Some((last_sid, all[start..].to_vec()))
}

/// Load the recent context of one previously indexed session. The session id is
/// always bound as a SQL parameter; callers cannot turn it into SQL syntax.
pub fn load_session_messages(
    state: &AppState,
    session_id: &str,
    limit: usize,
) -> Result<Vec<(String, String)>, String> {
    let conn = init_db(state)?;
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("check session: {e}"))?;
    if !exists {
        return Err(format!("session '{session_id}' not found"));
    }
    let limit = limit.clamp(1, 200) as i64;
    let mut statement = conn
        .prepare(
            "SELECT role, content FROM messages
             WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )
        .map_err(|e| format!("prepare session load: {e}"))?;
    let rows = statement
        .query_map(rusqlite::params![session_id, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("load session: {e}"))?;
    let mut messages: Vec<_> = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read session: {e}"))?;
    messages.reverse();
    Ok(messages)
}

// ── Compression blocks (context compression) ───────────────────────────────

/// One compressed block of a session: everything up to and including
/// `upto_message_id` is represented by `summary`.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryBlock {
    pub id: i64,
    pub session_id: String,
    pub upto_message_id: i64,
    pub summary: String,
    pub summary_tokens: i64,
    pub created_at: String,
}

/// Append a compressed block. Blocks are never overwritten: the sequence of
/// watermarks is the record of what was compressed when.
pub fn save_summary(
    conn: &Connection,
    session_id: &str,
    upto_message_id: i64,
    summary: &str,
    summary_tokens: i64,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO summaries(session_id, upto_message_id, summary, summary_tokens, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session_id,
            upto_message_id,
            summary,
            summary_tokens,
            chrono::Utc::now().to_rfc3339()
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// All blocks of a session in compression order (oldest watermark first).
pub fn summary_blocks(conn: &Connection, session_id: &str) -> Result<Vec<SummaryBlock>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, upto_message_id, summary, summary_tokens, created_at
             FROM summaries WHERE session_id = ?1 ORDER BY upto_message_id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok(SummaryBlock {
                id: row.get(0)?,
                session_id: row.get(1)?,
                upto_message_id: row.get(2)?,
                summary: row.get(3)?,
                summary_tokens: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// The newest block — its watermark is the point from which new messages still
/// have to be summarized.
pub fn latest_summary(conn: &Connection, session_id: &str) -> Result<Option<SummaryBlock>, String> {
    Ok(summary_blocks(conn, session_id)?.pop())
}

/// Id of the newest message of a session (0 when the session is empty).
pub fn newest_message_id(conn: &Connection, session_id: &str) -> Result<i64, String> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

/// Messages of a session above a watermark, oldest first.
pub fn messages_after(
    conn: &Connection,
    session_id: &str,
    after_id: i64,
) -> Result<Vec<(i64, String, String)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, role, content FROM messages
             WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![session_id, after_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let state = Arc::new(AppState {
            brain: Arc::new(
                crate::brain::db::BrainDb::open(&config.data_dir.join("brain.db"), 384).unwrap(),
            ),
            embedder: tokio::sync::Mutex::new(None),
            config,
            session_id: crate::AppState::new_session_id(),
            tools: Arc::new(crate::tools::Registry::build_default()),
            ui_tx: std::sync::Mutex::new(None),
            files_read: std::sync::Mutex::new(std::collections::HashSet::new()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            router: std::sync::Mutex::new(None),
        });
        (state, dir)
    }

    #[test]
    fn loading_a_selected_session_returns_its_recent_messages_in_order() {
        let (state, _dir) = test_state();
        record_message(&state, "sess_archive", "user", "first question");
        record_message(&state, "sess_archive", "assistant", "first answer");
        record_message(&state, "sess_archive", "user", "second question");

        let recent = load_session_messages(&state, "sess_archive", 2).unwrap();

        assert_eq!(
            recent,
            vec![
                ("assistant".into(), "first answer".into()),
                ("user".into(), "second question".into())
            ]
        );
        assert!(load_session_messages(&state, "sess_missing", 10).is_err());
    }

    #[test]
    fn summaries_are_stored_with_a_watermark() {
        let (state, _dir) = test_state();
        let conn = init_db(&state).unwrap();

        save_summary(&conn, "s1", 42, "compressed block one", 120).unwrap();
        let got = latest_summary(&conn, "s1").unwrap().unwrap();
        assert_eq!(got.upto_message_id, 42);
        assert_eq!(got.summary_tokens, 120);
        assert_eq!(got.summary, "compressed block one");

        // A later block appends, it does not overwrite.
        save_summary(&conn, "s1", 60, "compressed block two", 90).unwrap();
        let blocks = summary_blocks(&conn, "s1").unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].upto_message_id, 42, "oldest watermark first");
        assert_eq!(blocks[1].upto_message_id, 60);
        assert_eq!(
            latest_summary(&conn, "s1")
                .unwrap()
                .unwrap()
                .upto_message_id,
            60
        );

        // Sessions are independent.
        assert!(summary_blocks(&conn, "other").unwrap().is_empty());
    }

    #[test]
    fn the_writer_batches_many_messages_over_one_connection() {
        let (state, _dir) = test_state();
        let writer = SessionWriter::open(&state, "batch").unwrap();
        let mut ids = Vec::new();
        for i in 1..=50 {
            ids.push(
                writer
                    .push("user", &format!("batched message {i}"))
                    .unwrap(),
            );
        }
        // Ids are strictly increasing — the compression relies on that for its
        // watermark.
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "{ids:?}");
        assert_eq!(writer.newest_message_id().unwrap(), ids[49]);

        let conn = init_db(&state).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE session_id = 'batch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 50);

        // The FTS index is maintained by the trigger, so the last message is
        // findable without any reindexing.
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM message_fts WHERE message_fts MATCH 'batched'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 50);
    }

    #[test]
    fn record_message_still_works_alongside_the_writer() {
        // Compatibility: the interactive path keeps using record_message.
        let (state, _dir) = test_state();
        record_message(&state, "tui", "user", "from the tui");
        let writer = SessionWriter::open(&state, "tui").unwrap();
        let id = writer.push("assistant", "from the writer").unwrap();
        assert!(id > 0);
        let conn = init_db(&state).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE session_id = 'tui'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn messages_and_watermarks_line_up() {
        let (state, _dir) = test_state();
        let conn = init_db(&state).unwrap();
        for i in 1..=5 {
            record_message(
                &state,
                "s1",
                if i % 2 == 0 { "assistant" } else { "user" },
                &format!("message {i}"),
            );
        }
        let newest = newest_message_id(&conn, "s1").unwrap();
        assert_eq!(newest, 5);

        // Everything above message 2 is exactly messages 3..5 — this is what an
        // incremental compression must send to the model.
        let after = messages_after(&conn, "s1", 2).unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(after[0].1, "user");
        assert_eq!(after[0].2, "message 3");
        assert_eq!(after[2].2, "message 5");

        // Nothing above the newest message: a compression would be a no-op.
        assert!(messages_after(&conn, "s1", newest).unwrap().is_empty());
        // An empty session has watermark 0.
        assert_eq!(newest_message_id(&conn, "empty").unwrap(), 0);
    }
}
