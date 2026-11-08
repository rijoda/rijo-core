//! Brain DB — SQLite-powered knowledge base with vector search and graph
//!
//! Schema:
//! ```text
//! nodes    — knowledge nodes with label, content, embedding, metadata
//! edges    — directed relations between nodes
//! nodes_fts — FTS5 full text over label + content
//! ```
//!
//! Persisted in `~/.rijo/brain.db`. SQLite = one file, atomic writes,
//! no corruption risk, readable in parallel.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub content: String,
    pub kind: String,   // 'memory' | 'code' | 'concept' | 'file' | 'session'
    pub source: String, // file path, skill name, etc.
    /// Absolute workspace root this node was indexed from. Empty for nodes from
    /// before the column existed; the search filters on it so a session never
    /// gets pointers into another project.
    pub workspace: String,
    pub token_cost: usize, // estimated tokens
    pub created_at: i64,
    pub updated_at: i64,
    pub access_cnt: u64,
    pub relevance: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: i64,
    pub src: String,
    pub dst: String,
    pub rel: String, // 'depends_on' | 'implements' | 'extends' | 'references' | 'describes'
    pub weight: f64,
}

/// Search result with score
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub node: Node,
    pub score: f64,         // cosine similarity (0-1) or FTS5 rank
    pub match_type: String, // 'vector' | 'fulltext' | 'graph'
}

/// Everything that steers a search. Kept in one struct because the parameter
/// list grew (workspace scoping, score floor, top-k, kind filter) and positional
/// booleans invite mistakes.
#[derive(Debug, Clone)]
pub struct SearchQuery<'a> {
    pub text: &'a str,
    pub embedding: Option<&'a [f32]>,
    pub kind: Option<&'a str>,
    /// Absolute workspace root; `None` searches every workspace (tests, tools
    /// that intentionally look across projects).
    pub workspace: Option<&'a str>,
    pub top_k: usize,
    /// Cosine floor for vector hits. Full-text hits have no comparable score and
    /// are never filtered by it.
    pub min_score: f64,
}

/// Which path produced the hits — the caller reports it when it had to degrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchMode {
    Vector,
    FullText,
    /// Vector and full text were both available and fused (reciprocal rank
    /// fusion). The default case whenever the embedder is free.
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub mode: SearchMode,
}

// ─── DB ───────────────────────────────────────────────────────────────────

pub struct BrainDb {
    conn: Mutex<Connection>,
    dim: usize,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl BrainDb {
    /// Open/create brain.db at the given path.
    pub fn open(path: &Path, dim: usize) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        let db = Self {
            conn: Mutex::new(conn),
            dim,
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS nodes (
                 id         TEXT PRIMARY KEY,
                 label      TEXT NOT NULL,
                 content    TEXT NOT NULL,
                 kind       TEXT NOT NULL DEFAULT 'memory',
                 source     TEXT NOT NULL DEFAULT '',
                 embedding  BLOB,
                 token_cost INTEGER DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 access_cnt INTEGER DEFAULT 0,
                 relevance  REAL DEFAULT 1.0
             );

             CREATE TABLE IF NOT EXISTS edges (
                 id    INTEGER PRIMARY KEY AUTOINCREMENT,
                 src   TEXT NOT NULL REFERENCES nodes(id),
                 dst   TEXT NOT NULL REFERENCES nodes(id),
                 rel   TEXT NOT NULL,
                 weight REAL DEFAULT 1.0
             );

             CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src);
             CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst);
             CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);
             CREATE INDEX IF NOT EXISTS idx_nodes_source ON nodes(source);

             CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
                 label, content, content=nodes, content_rowid=rowid
             );

             CREATE TABLE IF NOT EXISTS indexed_files (
                 path       TEXT NOT NULL,
                 workspace  TEXT NOT NULL,
                 mtime      INTEGER NOT NULL,
                 size       INTEGER NOT NULL,
                 chunks     INTEGER NOT NULL DEFAULT 0,
                 indexed_at INTEGER NOT NULL,
                 PRIMARY KEY (path, workspace)
             );

             CREATE INDEX IF NOT EXISTS idx_indexed_files_workspace ON indexed_files(workspace);

             CREATE TABLE IF NOT EXISTS kv_cache (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );",
        )
        .map_err(|e| e.to_string())?;
        // Additive migration: older databases have no `workspace` column.
        let has_ws: bool = conn
            .prepare("PRAGMA table_info(nodes)")
            .and_then(|mut s| {
                let names = s.query_map([], |r| r.get::<_, String>(1))?;
                Ok(names.filter_map(|n| n.ok()).any(|n| n == "workspace"))
            })
            .unwrap_or(false);
        if !has_ws {
            conn.execute_batch("ALTER TABLE nodes ADD COLUMN workspace TEXT NOT NULL DEFAULT '';")
                .map_err(|e| e.to_string())?;
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_nodes_workspace ON nodes(workspace);")
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── CRUD ───────────────────────────────────────────────────────────

    pub fn upsert_node(&self, node: &Node, embedding: Option<&[f32]>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let embed_blob = embedding.map(|v| {
            let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
            bytes
        });
        conn.execute(
            "INSERT INTO nodes(id, label, content, kind, source, embedding, token_cost, created_at, updated_at, access_cnt, relevance, workspace)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                 label=excluded.label, content=excluded.content, kind=excluded.kind,
                 source=excluded.source,
                 -- COALESCE: an upsert without an embedding (e.g. merging an
                 -- existing sync node) must not wipe the stored vectors. That
                 -- happened in a real sync run: the second import nulled every
                 -- embedding and left the vector search empty.
                 embedding=COALESCE(excluded.embedding, nodes.embedding),
                 token_cost=excluded.token_cost, updated_at=excluded.updated_at,
                 relevance=excluded.relevance, workspace=excluded.workspace",
            params![
                node.id, node.label, node.content, node.kind, node.source,
                embed_blob, node.token_cost as i64, node.created_at, node.updated_at,
                node.access_cnt as i64, node.relevance, node.workspace,
            ],
        ).map_err(|e| e.to_string())?;

        // FTS5 sync: DELETE+INSERT — UPSERT is NOT implemented on virtual tables
        // (SQLite error: "UPSERT not implemented for virtual table")
        let rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM nodes WHERE id = ?1",
                params![node.id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let _ = conn.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid]);
        conn.execute(
            "INSERT INTO nodes_fts(rowid, label, content) VALUES (?1, ?2, ?3)",
            params![rowid, node.label, node.content],
        )
        .map_err(|e| format!("FTS sync failed: {e}"))?;
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Result<Option<Node>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare(
            "SELECT id, label, content, kind, source, COALESCE(token_cost,0), created_at, updated_at, COALESCE(access_cnt,0), COALESCE(relevance,1.0), COALESCE(workspace,'')
             FROM nodes WHERE id = ?1"
        ).map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(row) => Ok(Some(Node {
                id: row.get(0).map_err(|e| e.to_string())?,
                label: row.get(1).map_err(|e| e.to_string())?,
                content: row.get(2).map_err(|e| e.to_string())?,
                kind: row.get(3).map_err(|e| e.to_string())?,
                source: row.get(4).map_err(|e| e.to_string())?,
                token_cost: row.get::<_, i64>(5).map_err(|e| e.to_string())? as usize,
                created_at: row.get(6).map_err(|e| e.to_string())?,
                updated_at: row.get(7).map_err(|e| e.to_string())?,
                access_cnt: row.get::<_, i64>(8).map_err(|e| e.to_string())? as u64,
                relevance: row.get(9).map_err(|e| e.to_string())?,
                workspace: row.get(10).map_err(|e| e.to_string())?,
            })),
            None => Ok(None),
        }
    }

    pub fn delete_node(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM edges WHERE src=?1 OR dst=?1", params![id])
            .map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM nodes WHERE id=?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── Edges ──────────────────────────────────────────────────────────

    pub fn add_edge(&self, src: &str, dst: &str, rel: &str, weight: f64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO edges(src, dst, rel, weight) VALUES (?1, ?2, ?3, ?4)",
            params![src, dst, rel, weight],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_edges(&self, node_id: &str) -> Result<Vec<Edge>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare(
            "SELECT id, src, dst, rel, COALESCE(weight,1.0) FROM edges WHERE src=?1 OR dst=?1 ORDER BY weight DESC"
        ).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![node_id], |row| {
                Ok(Edge {
                    id: row.get(0)?,
                    src: row.get(1)?,
                    dst: row.get(2)?,
                    rel: row.get(3)?,
                    weight: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    // ── Vector search (bruteforce cosine) ───────────────────────────────

    /// Search the top-K nodes by cosine similarity.
    ///
    /// `embedding == None` means the embedder was busy (indexing holds it) and
    /// the query falls back to FTS5 — the caller learns that from
    /// `SearchResult::mode` and can say so instead of silently degrading.
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResult, String> {
        match q.embedding {
            Some(emb) => {
                // Both paths run: the embedding finds paraphrases, FTS5 finds the
                // exact identifier the model typed. Taking only one of them threw
                // away half the signal — an exact hit used to be invisible when
                // the vector score was mediocre (and a paraphrase was invisible
                // without an embedding).
                let overfetch = q.top_k.max(10);
                let vector =
                    self.vector_search(emb, q.kind, q.workspace, overfetch, q.min_score)?;
                let fulltext = self.fulltext_search(q.text, q.kind, q.workspace, overfetch)?;
                // The single-path branches were fetched with `overfetch`, so they
                // must be cut back to what the caller asked for (a test caught
                // this: top_k=1 returned two hits).
                let (hits, mode) = match (vector.is_empty(), fulltext.is_empty()) {
                    (true, false) => (
                        fulltext.into_iter().take(q.top_k).collect(),
                        SearchMode::FullText,
                    ),
                    (false, true) => (
                        vector.into_iter().take(q.top_k).collect(),
                        SearchMode::Vector,
                    ),
                    (false, false) => (
                        fuse_rrf(&vector, &fulltext, RRF_K, q.top_k),
                        SearchMode::Hybrid,
                    ),
                    (true, true) => (Vec::new(), SearchMode::Hybrid),
                };
                Ok(SearchResult { hits, mode })
            }
            None => {
                let hits = self.fulltext_search(q.text, q.kind, q.workspace, q.top_k)?;
                Ok(SearchResult {
                    hits,
                    mode: SearchMode::FullText,
                })
            }
        }
    }

    fn vector_search(
        &self,
        query_emb: &[f32],
        kind: Option<&str>,
        workspace: Option<&str>,
        top_k: usize,
        min_score: f64,
    ) -> Result<Vec<SearchHit>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut filters = String::from("embedding IS NOT NULL");
        if let Some(k) = kind {
            filters.push_str(&format!(" AND kind = '{}'", k.replace('\'', "''")));
        }
        if let Some(w) = workspace {
            filters.push_str(&format!(" AND workspace = '{}'", w.replace('\'', "''")));
        }
        let sql = format!(
            "SELECT id, label, content, kind, source, COALESCE(token_cost,0), created_at, updated_at, COALESCE(access_cnt,0), COALESCE(relevance,1.0), embedding
             FROM nodes WHERE {filters}"
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let embed_blob: Option<Vec<u8>> = row.get(10).ok();
                let embedding: Option<Vec<f32>> = embed_blob.map(|b| {
                    b.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect()
                });
                let node = Node {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    content: row.get(2)?,
                    kind: row.get(3)?,
                    source: row.get(4)?,
                    workspace: row.get(11).unwrap_or_default(),
                    token_cost: row.get::<_, i64>(5)? as usize,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    access_cnt: row.get::<_, i64>(8)? as u64,
                    relevance: row.get(9)?,
                };
                let score = embedding
                    .as_ref()
                    .map(|e| cosine_similarity(query_emb, e))
                    .unwrap_or(0.0);
                Ok((node, score))
            })
            .map_err(|e| e.to_string())?;

        let mut hits: Vec<SearchHit> = rows
            .filter_map(|r| r.ok())
            .map(|(node, score)| SearchHit {
                node,
                score,
                match_type: "vector".into(),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Floor: an irrelevant hit is worse than no hit — it spends prompt tokens
        // and points the model somewhere wrong.
        hits.retain(|h| h.score >= min_score);
        hits.truncate(top_k);
        Ok(hits)
    }

    fn fulltext_search(
        &self,
        query: &str,
        kind: Option<&str>,
        workspace: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut kind_filter = String::new();
        if let Some(k) = kind {
            kind_filter.push_str(&format!("AND n.kind = '{}'", k.replace('\'', "''")));
        }
        if let Some(w) = workspace {
            kind_filter.push_str(&format!("AND n.workspace = '{}'", w.replace('\'', "''")));
        }
        let sql = format!(
            "SELECT n.id, n.label, n.content, n.kind, n.source, COALESCE(n.token_cost,0), n.created_at, n.updated_at, COALESCE(n.access_cnt,0), COALESCE(n.relevance,1.0), COALESCE(n.workspace,''), rank
             FROM nodes_fts f JOIN nodes n ON f.rowid = n.rowid
             WHERE nodes_fts MATCH ?1 {kind_filter}
             ORDER BY rank LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        // FTS5 searches for query terms: escape for FTS5 syntax
        let fts_query = query
            .split_whitespace()
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{}\"", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = stmt
            .query_map(params![fts_query, top_k as i64], |row| {
                Ok(SearchHit {
                    node: Node {
                        id: row.get(0)?,
                        label: row.get(1)?,
                        content: row.get(2)?,
                        kind: row.get(3)?,
                        source: row.get(4)?,
                        workspace: row.get(11).unwrap_or_default(),
                        token_cost: row.get::<_, i64>(5)? as usize,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        access_cnt: row.get::<_, i64>(8)? as u64,
                        relevance: row.get(9)?,
                    },
                    // FTS5 rank sits at index 12 now that the workspace column is selected.
                    score: row.get::<_, f64>(12).unwrap_or(0.0),
                    match_type: "fulltext".into(),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    // ── File bookkeeping (step 3: incremental indexing) ─────────────────

    /// What we know about an already indexed file: (mtime, size, chunks).
    /// A cheap stat comparison against this decides whether the file has to be
    /// read, chunked and embedded again.
    pub fn file_state(
        &self,
        workspace: &str,
        path: &str,
    ) -> Result<Option<(i64, i64, i64)>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT mtime, size, chunks FROM indexed_files WHERE workspace = ?1 AND path = ?2",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(params![workspace, path])
            .map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(r) => Ok(Some((
                r.get(0).map_err(|e| e.to_string())?,
                r.get(1).map_err(|e| e.to_string())?,
                r.get(2).map_err(|e| e.to_string())?,
            ))),
            None => Ok(None),
        }
    }

    pub fn upsert_file_state(
        &self,
        workspace: &str,
        path: &str,
        mtime: i64,
        size: i64,
        chunks: i64,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO indexed_files(path, workspace, mtime, size, chunks, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path, workspace) DO UPDATE SET
                 mtime=excluded.mtime, size=excluded.size, chunks=excluded.chunks, indexed_at=excluded.indexed_at",
            params![path, workspace, mtime, size, chunks, now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// All files this workspace has indexed (for the removal pass).
    pub fn indexed_files_of(&self, workspace: &str) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT path FROM indexed_files WHERE workspace = ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![workspace], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn forget_file(&self, workspace: &str, path: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM indexed_files WHERE workspace = ?1 AND path = ?2",
            params![workspace, path],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Removes every node of one file. Called before re-indexing a changed file
    /// so deleted functions do not linger as ghost hits.
    pub fn delete_nodes_for_source(&self, workspace: &str, source: &str) -> Result<u64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM nodes WHERE workspace = ?1 AND source = ?2")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![workspace, source], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for id in &ids {
            conn.execute("DELETE FROM edges WHERE src=?1 OR dst=?1", params![id])
                .map_err(|e| e.to_string())?;
            conn.execute(
                "DELETE FROM nodes_fts WHERE rowid = (SELECT rowid FROM nodes WHERE id = ?1)",
                params![id],
            )
            .map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM nodes WHERE id = ?1", params![id])
                .map_err(|e| e.to_string())?;
        }
        Ok(ids.len() as u64)
    }

    /// Drops nodes that no longer have a file: gone projects and, in one clean
    /// sweep, the pre-workspace-column leftovers (workspace = ''), which the
    /// scoped search would never return anyway.
    pub fn drop_unattributed_nodes(&self) -> Result<u64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute("DELETE FROM nodes WHERE workspace = ''", [])
            .map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM nodes_fts WHERE rowid NOT IN (SELECT rowid FROM nodes)",
            [],
        )
        .map_err(|e| e.to_string())?;
        Ok(n as u64)
    }

    // ── Feedback (step 5) ───────────────────────────────────────────────

    /// Counts a hit as used. `brain_search` calls this for every hit it hands to
    /// the model: that is the only honest signal we have about which pointers
    /// actually help. The counter is a tie-break in the ranking (see fuse_rrf),
    /// not a score boost — a pointer that was useful before should win when two
    /// hits are otherwise equal, and nothing more.
    pub fn bump_access(&self, ids: &[String]) -> Result<u64, String> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut updated = 0u64;
        for id in ids {
            updated += conn
                .execute(
                    "UPDATE nodes SET access_cnt = COALESCE(access_cnt,0) + 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| e.to_string())? as u64;
        }
        Ok(updated)
    }

    // ── Export ──────────────────────────────────────────────────────────

    /// Every node of a workspace together with its embedding, for the sync
    /// export. Reading embeddings back out is the only way to hand a warm index
    /// to another machine without re-embedding (a cold index of this repo cost
    /// 870 s — measured).
    pub fn nodes_for_export(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<(Node, Option<Vec<f32>>)>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut filter = String::new();
        if let Some(w) = workspace {
            filter = format!("WHERE workspace = '{}'", w.replace('\'', "''"));
        }
        let sql = format!(
            "SELECT id, label, content, kind, source, COALESCE(token_cost,0), created_at, updated_at,
                    COALESCE(access_cnt,0), COALESCE(relevance,1.0), COALESCE(workspace,''), embedding
             FROM nodes {filter} ORDER BY id"
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let blob: Option<Vec<u8>> = row.get(11).ok();
                let embedding = blob.map(|b| {
                    b.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect::<Vec<f32>>()
                });
                Ok((
                    Node {
                        id: row.get(0)?,
                        label: row.get(1)?,
                        content: row.get(2)?,
                        kind: row.get(3)?,
                        source: row.get(4)?,
                        workspace: row.get(10).unwrap_or_default(),
                        token_cost: row.get::<_, i64>(5)? as usize,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        access_cnt: row.get::<_, i64>(8)? as u64,
                        relevance: row.get(9)?,
                    },
                    embedding,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    // ── Statistics ──────────────────────────────────────────────────────

    /// Number of nodes, optionally only for one workspace. Used by the
    /// `brain_search` tool to tell "nothing indexed here yet" apart from
    /// "your query did not match".
    pub fn count_nodes(&self, workspace: Option<&str>) -> Result<u64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n: i64 = match workspace {
            Some(w) => conn
                .query_row(
                    "SELECT COUNT(*) FROM nodes WHERE workspace = ?1",
                    params![w],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?,
            None => conn
                .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
                .map_err(|e| e.to_string())?,
        };
        Ok(n as u64)
    }

    /// Look up a key in kv_cache
    pub fn kv_cache_get(&self, key: &str) -> Option<(String, i64)> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT value, created_at FROM kv_cache WHERE key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    }

    pub fn kv_cache_put(&self, key: &str, value: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO kv_cache(key, value, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, created_at=excluded.created_at",
                rusqlite::params![key, value, { now() }],
            );
        }
    }

    pub fn stats(&self) -> Result<(u64, u64), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let nodes: u64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let edges: u64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok((nodes, edges))
    }

    pub fn node_count(&self) -> usize {
        self.stats().map(|(n, _)| n as usize).unwrap_or(0)
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
}

// ── Cosine Similarity ─────────────────────────────────────────────────────

/// Rank-fusion constant from the original reciprocal-rank-fusion recipe. 60 is
/// the usual choice: it flattens the influence of the exact rank, so a hit has
/// to be good in *both* lists instead of excellent in one.
const RRF_K: f64 = 60.0;

/// Reciprocal rank fusion of two hit lists (vector + full text).
///
/// Why not just score: cosine similarity and an FTS5 rank are not comparable —
/// one is 0..1, the other is a negative BM25-ish value. Ranks are comparable, so
/// a document that both paths like rises, and each path can contribute its own
/// kind of match: identifiers from full text, paraphrases from embeddings.
fn fuse_rrf(vector: &[SearchHit], fulltext: &[SearchHit], k: f64, top_k: usize) -> Vec<SearchHit> {
    use std::collections::HashMap;
    let mut fused: HashMap<String, (SearchHit, f64)> = HashMap::new();

    for (rank, hit) in vector.iter().enumerate() {
        let score = 1.0 / (k + (rank + 1) as f64);
        fused
            .entry(hit.node.id.clone())
            .and_modify(|(_, s)| *s += score)
            .or_insert_with(|| (hit.clone(), score));
    }
    for (rank, hit) in fulltext.iter().enumerate() {
        let score = 1.0 / (k + (rank + 1) as f64);
        fused
            .entry(hit.node.id.clone())
            .and_modify(|(_, s)| *s += score)
            .or_insert_with(|| (hit.clone(), score));
    }

    let mut out: Vec<(SearchHit, f64)> = fused.into_values().collect();
    // Ties: first the hit that was useful before (access_cnt via brain_search),
    // then the id — so the order stays stable across runs despite the async
    // HashMap iteration order (a changing prompt prefix would break the cache).
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.node.access_cnt.cmp(&a.0.node.access_cnt))
            .then_with(|| a.0.node.id.cmp(&b.0.node.id))
    });
    out.into_iter()
        .take(top_k)
        .map(|(hit, score)| SearchHit { score, ..hit })
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let dot: f32 = a[..len]
        .iter()
        .zip(b[..len].iter())
        .map(|(x, y)| x * y)
        .sum();
    let na: f32 = a[..len].iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b[..len].iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na * nb)) as f64
    }
}

// ── Node-Helper ───────────────────────────────────────────────────────────

impl Node {
    pub fn new(id: String, label: String, content: String, kind: &str) -> Self {
        let t = now();
        // rough token estimate: ~4 chars per token
        let token_cost = content.len() / 4;
        Self {
            id,
            label,
            content,
            kind: kind.to_string(),
            source: String::new(),
            workspace: String::new(),
            token_cost,
            created_at: t,
            updated_at: t,
            access_cnt: 0,
            relevance: 1.0,
        }
    }

    pub fn with_source(mut self, source: &str) -> Self {
        self.source = source.to_string();
        self
    }

    /// Marks the node as belonging to a workspace root (see SearchQuery).
    pub fn with_workspace(mut self, workspace: &str) -> Self {
        self.workspace = workspace.to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> BrainDb {
        let dir = tempdir().unwrap();
        BrainDb::open(&dir.path().join("brain.db"), 384).unwrap()
    }

    #[test]
    fn test_create_and_get() {
        let db = test_db();
        let node = Node::new(
            "test:1".into(),
            "Test Node".into(),
            "Hello World".into(),
            "memory",
        );
        db.upsert_node(&node, None).unwrap();
        let fetched = db.get_node("test:1").unwrap().unwrap();
        assert_eq!(fetched.label, "Test Node");
        assert_eq!(fetched.content, "Hello World");
    }

    #[test]
    fn test_update() {
        let db = test_db();
        let node = Node::new(
            "test:2".into(),
            "Old".into(),
            "old content".into(),
            "memory",
        );
        db.upsert_node(&node, None).unwrap();
        let updated = Node {
            label: "New".into(),
            content: "new content".into(),
            ..node
        };
        db.upsert_node(&updated, None).unwrap();
        let fetched = db.get_node("test:2").unwrap().unwrap();
        assert_eq!(fetched.label, "New");
        assert_eq!(fetched.content, "new content");
    }

    #[test]
    fn test_delete() {
        let db = test_db();
        let node = Node::new("test:3".into(), "Delete Me".into(), "bye".into(), "memory");
        db.upsert_node(&node, None).unwrap();
        db.delete_node("test:3").unwrap();
        assert!(db.get_node("test:3").unwrap().is_none());
    }

    #[test]
    fn test_edges() {
        let db = test_db();
        let a = Node::new("edge:a".into(), "A".into(), "aaa".into(), "memory");
        let b = Node::new("edge:b".into(), "B".into(), "bbb".into(), "memory");
        db.upsert_node(&a, None).unwrap();
        db.upsert_node(&b, None).unwrap();
        db.add_edge("edge:a", "edge:b", "references", 1.0).unwrap();
        let edges = db.get_edges("edge:a").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].rel, "references");
    }

    #[test]
    fn test_fulltext_search() {
        let db = test_db();
        let node = Node::new(
            "fts:1".into(),
            "Authentication Module".into(),
            "Handles user login and JWT tokens".into(),
            "code",
        );
        db.upsert_node(&node, None).unwrap();
        let hits = db.fulltext_search("login", None, None, 5).unwrap();
        assert!(!hits.is_empty(), "should find 'login' in content");
        assert_eq!(hits[0].match_type, "fulltext");
    }

    #[test]
    fn test_vector_search() {
        let db = test_db();
        let emb: Vec<f32> = (0..384).map(|i| (i as f32) / 384.0).collect();
        let node = Node::new(
            "vec:1".into(),
            "Vector Test".into(),
            "some content".into(),
            "memory",
        );
        db.upsert_node(&node, Some(&emb)).unwrap();

        let query_emb: Vec<f32> = (0..384).map(|i| (i as f32) / 384.0).collect();
        let hits = db.vector_search(&query_emb, None, None, 5, 0.0).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].score > 0.99); // almost identical
    }

    #[test]
    fn test_stats() {
        let db = test_db();
        let a = Node::new("s:1".into(), "A".into(), "x".into(), "memory");
        let b = Node::new("s:2".into(), "B".into(), "y".into(), "memory");
        db.upsert_node(&a, None).unwrap();
        db.upsert_node(&b, None).unwrap();
        db.add_edge("s:1", "s:2", "relates", 1.0).unwrap();
        let (nodes, edges) = db.stats().unwrap();
        assert_eq!(nodes, 2);
        assert_eq!(edges, 1);
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);

        let c = vec![1.0, 0.0, 0.0];
        let d = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&c, &d) - 0.0).abs() < 0.001);
    }

    // ── Workspace scoping + score floor (step 1) ────────────────────────────
    // Before this: the search ignored the workspace, so 8 of 74 sources came
    // from other projects and the model could be pointed at files that do not
    // exist in the current one. And without a floor the top 3 were injected even
    // when all of them were irrelevant.

    fn node_in(ws: &str, id: &str, vec: [f32; 3]) -> (Node, Vec<f32>) {
        (
            Node::new(
                id.into(),
                format!("{id} — test"),
                format!("content of {id}"),
                "code",
            )
            .with_source(id)
            .with_workspace(ws),
            vec.to_vec(),
        )
    }

    fn seeded_db() -> (tempfile::TempDir, BrainDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = BrainDb::open(&dir.path().join("brain.db"), 3).unwrap();
        let (a, va) = node_in("/ws/a", "a.rs", [1.0, 0.0, 0.0]);
        let (b, vb) = node_in("/ws/b", "b.rs", [1.0, 0.0, 0.0]);
        let (c, vc) = node_in("/ws/a", "ortho.rs", [0.0, 1.0, 0.0]);
        db.upsert_node(&a, Some(&va)).unwrap();
        db.upsert_node(&b, Some(&vb)).unwrap();
        db.upsert_node(&c, Some(&vc)).unwrap();
        (dir, db)
    }

    #[test]
    fn vector_search_only_returns_the_current_workspace() {
        let (_d, db) = seeded_db();
        let q = [1.0, 0.0, 0.0];
        let res = db
            .search(&SearchQuery {
                text: "anything",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 10,
                min_score: 0.0,
            })
            .unwrap();
        let sources: Vec<&str> = res.hits.iter().map(|h| h.node.source.as_str()).collect();
        assert_eq!(res.mode, SearchMode::Vector);
        assert!(
            sources.contains(&"a.rs"),
            "own workspace missing: {sources:?}"
        );
        assert!(
            !sources.contains(&"b.rs"),
            "node from another workspace leaked: {sources:?}"
        );
    }

    #[test]
    fn score_floor_drops_irrelevant_hits_instead_of_injecting_noise() {
        let (_d, db) = seeded_db();
        let q = [0.0, 1.0, 0.0]; // orthogonal to a.rs (0.0), identical to ortho.rs (1.0)
        let with_floor = db
            .search(&SearchQuery {
                text: "q",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 10,
                min_score: 0.5,
            })
            .unwrap();
        assert_eq!(
            with_floor.hits.len(),
            1,
            "floor did not filter: {:?}",
            with_floor.hits.len()
        );
        assert_eq!(with_floor.hits[0].node.source, "ortho.rs");

        let without_floor = db
            .search(&SearchQuery {
                text: "q",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 10,
                min_score: 0.0,
            })
            .unwrap();
        assert_eq!(
            without_floor.hits.len(),
            2,
            "floor 0.0 must keep everything"
        );
    }

    #[test]
    fn full_text_search_is_workspace_scoped_too() {
        let (_d, db) = seeded_db();
        let res = db
            .search(&SearchQuery {
                text: "test",
                embedding: None, // embedder busy -> FTS path
                kind: None,
                workspace: Some("/ws/b"),
                top_k: 10,
                min_score: 0.0,
            })
            .unwrap();
        assert_eq!(res.mode, SearchMode::FullText);
        let sources: Vec<&str> = res.hits.iter().map(|h| h.node.source.as_str()).collect();
        assert_eq!(
            sources,
            vec!["b.rs"],
            "FTS leaked another workspace: {sources:?}"
        );
    }

    #[test]
    fn top_k_limits_the_hits() {
        let (_d, db) = seeded_db();
        let q = [1.0, 0.0, 0.0];
        let res = db
            .search(&SearchQuery {
                text: "q",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 1,
                min_score: 0.0,
            })
            .unwrap();
        assert_eq!(res.hits.len(), 1);
    }

    // ── Hybrid retrieval (step 4) ───────────────────────────────────────────
    // Before: either vector or full text, never both. An exact identifier the
    // model typed was invisible when its cosine score was mediocre, and a
    // paraphrase was invisible without an embedding.

    fn hit(id: &str, score: f64) -> SearchHit {
        SearchHit {
            node: Node::new(
                id.into(),
                format!("{id} — label"),
                format!("content {id}"),
                "code",
            )
            .with_source(id),
            score,
            match_type: "test".into(),
        }
    }

    #[test]
    fn rrf_promotes_documents_both_paths_agree_on() {
        // `both` is second in each list, `only_vec` first in the vector list and
        // `only_fts` first in the full-text list.
        let vector = vec![hit("only_vec", 0.9), hit("both", 0.8)];
        let fulltext = vec![hit("only_fts", -1.0), hit("both", -2.0)];
        let fused = fuse_rrf(&vector, &fulltext, RRF_K, 3);
        let order: Vec<&str> = fused.iter().map(|h| h.node.id.as_str()).collect();
        assert_eq!(
            order[0], "both",
            "rank fusion did not promote the shared hit: {order:?}"
        );
        assert_eq!(order.len(), 3, "no document may be lost: {order:?}");
    }

    #[test]
    fn rrf_keeps_each_path_contribution_and_dedupes() {
        let vector = vec![hit("a", 0.5), hit("b", 0.4)];
        let fulltext = vec![hit("a", -0.1)];
        let fused = fuse_rrf(&vector, &fulltext, RRF_K, 10);
        assert_eq!(fused.len(), 2, "duplicate id must be merged: {fused:?}");
        assert!(fused[0].score > fused[1].score);
        // A hit in both lists scores higher than the same rank in one list.
        let single = fuse_rrf(&vector[..1], &[], RRF_K, 10);
        assert!(fused[0].score > single[0].score);
    }

    #[test]
    fn rrf_truncates_to_top_k_and_handles_empty_input() {
        assert!(fuse_rrf(&[], &[], RRF_K, 5).is_empty());
        let vector: Vec<SearchHit> = (0..10).map(|i| hit(&format!("v{i}"), 0.5)).collect();
        assert_eq!(fuse_rrf(&vector, &[], RRF_K, 3).len(), 3);
    }

    #[test]
    fn hybrid_search_finds_both_the_identifier_and_the_paraphrase() {
        let (_d, db) = seeded_db();
        // Query embedding points at a.rs (cosine 1.0); the words match ortho.rs,
        // which is orthogonal in embedding space and would be lost with vector
        // search alone.
        let q = [1.0, 0.0, 0.0];
        let res = db
            .search(&SearchQuery {
                text: "content of ortho.rs",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 5,
                min_score: 0.3,
            })
            .unwrap();
        assert_eq!(res.mode, SearchMode::Hybrid, "expected both paths to run");
        let sources: Vec<&str> = res.hits.iter().map(|h| h.node.source.as_str()).collect();
        assert!(
            sources.contains(&"ortho.rs"),
            "full-text contribution lost: {sources:?}"
        );
        assert!(
            sources.contains(&"a.rs"),
            "vector contribution lost: {sources:?}"
        );
    }

    #[test]
    fn vector_only_when_the_words_do_not_match() {
        let (_d, db) = seeded_db();
        let q = [1.0, 0.0, 0.0];
        let res = db
            .search(&SearchQuery {
                text: "zzzz-nothing-matches-this",
                embedding: Some(&q),
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 5,
                min_score: 0.3,
            })
            .unwrap();
        assert_eq!(
            res.mode,
            SearchMode::Vector,
            "no FTS match -> plain vector result"
        );
    }

    // ── Feedback loop (step 5) ──────────────────────────────────────────────

    #[test]
    fn bump_access_counts_only_the_given_ids() {
        let (_d, db) = seeded_db();
        let a = "code:a.rs".to_string();
        // The seeded helper uses plain ids; look up the real one.
        let real = db
            .search(&SearchQuery {
                text: "content of a.rs",
                embedding: None,
                kind: None,
                workspace: Some("/ws/a"),
                top_k: 1,
                min_score: 0.0,
            })
            .unwrap()
            .hits[0]
            .node
            .id
            .clone();
        assert_eq!(
            db.bump_access(&[real.clone(), a]).unwrap(),
            1,
            "only the existing id counts"
        );
        let after = db.get_node(&real).unwrap().unwrap();
        assert_eq!(after.access_cnt, 1);
        db.bump_access(std::slice::from_ref(&real)).unwrap();
        assert_eq!(db.get_node(&real).unwrap().unwrap().access_cnt, 2);
    }

    #[test]
    fn access_count_breaks_ties_but_does_not_outrank_a_better_rank() {
        // Equal fused score (rank 1 in one list each): the hit that was useful
        // before must come first.
        let mut proven = hit("proven", 0.1);
        proven.node.access_cnt = 7;
        let fresh = hit("fresh", 0.1);
        let tie = fuse_rrf(&[proven.clone()], std::slice::from_ref(&fresh), RRF_K, 2);
        let order: Vec<&str> = tie.iter().map(|h| h.node.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["proven", "fresh"],
            "access_cnt must break the tie: {order:?}"
        );

        // A better rank always wins, even against a much-used hit.
        let mut often_used = hit("often-used", 0.1);
        often_used.node.access_cnt = 99;
        let ranked = fuse_rrf(&[fresh, often_used], &[], RRF_K, 2);
        assert_eq!(
            ranked[0].node.id, "fresh",
            "access_cnt must not outrank a better rank"
        );
    }
}
