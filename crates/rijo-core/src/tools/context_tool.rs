//! `context` — see and manage the conversation context.
//!
//! Why a tool: the pruning in the executor is message-count based and invisible.
//! Nothing told the model (or the user) how full the context is, what it consists
//! of, or what a compression would save — and nothing could bring an old part of
//! the conversation back once it was pruned.
//!
//! Actions:
//! - `status` — fill against the detected model window, message breakdown of the
//!   session, watermark state, and how much a compression would save.
//! - `recall`  — fetch older messages of THIS session back by keyword. Uses the
//!   FTS5 index that already exists over every message (measured 0.23-0.40 ms
//!   in-process), so recalling costs a fraction of what re-summarizing costs.
//! - `show`    — the stored compression blocks of the session.
//!
//! `summarize` (the LLM side) lands in a later step together with the executor
//! trigger; the store it needs already exists (`session_tool::save_summary`).

use crate::tools::session_tool::{
    init_db, messages_after, newest_message_id, summary_blocks, SummaryBlock,
};
use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ContextTool;

/// Excerpt per recall hit: enough to recognise the passage, not enough to
/// replace reading it back in full.
const EXCERPT_CHARS: usize = 300;
const MAX_HITS: usize = 20;
/// Characters per token, the same convention the status line uses for its
/// estimates.
const CHARS_PER_TOKEN: usize = 4;

lazy_static::lazy_static! {
    static ref CONTEXT_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "status = how full the context is and what a compression would save; recall = search this session's older messages by keyword; show = the stored compression blocks.",
                "enum": ["status", "recall", "show"]
            },
            "query": {
                "type": "string",
                "description": "Keywords for 'recall' (e.g. 'websurfx port 8080')."
            },
            "limit": {
                "type": "integer",
                "description": "Max hits for 'recall' (default 5, max 20)."
            }
        },
        "required": ["action"]
    }));
}

/// True only for the read actions — the batching logic asks this, and `summarize`
/// (which writes and calls the model) must never end up in a read batch.
pub fn action_is_read(action: &str) -> bool {
    matches!(action, "status" | "recall" | "show")
}

/// Builds the FTS5 expression for a recall: every alphanumeric run becomes a
/// prefix term, AND-combined (`websurfx* port* 8080*`).
///
/// Two traps this avoids, both found in real use:
/// - A phrase search (`"websurfx port 8080"`) only matches words directly next to
///   each other in that order — almost never how a passage is worded.
/// - Punctuation INSIDE a term is FTS5 syntax: `MATCH 'ZIMT-4711*'` fails with
///   "no such column: 4711" (the hyphen is parsed as a column filter), so a query
///   with a hyphen or a colon used to return nothing at all. Splitting on
///   everything except letters, digits and `_` keeps the expression valid for any
///   input the model produces.
fn fts_expression(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("{term}*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Tokens a text roughly is, with the project-wide 4-chars-per-token convention.
fn approx_tokens(chars: usize) -> u64 {
    (chars / CHARS_PER_TOKEN) as u64
}

fn human(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.0}M", n as f64 / 1_000_000.0)
    }
}

/// Last real prompt size of the session, read from the metrics file — the file is
/// the source of truth for what the provider actually counted, and it costs one
/// line of I/O instead of threading another counter through the app state.
fn last_prompt_tokens(state: &AppState) -> Option<u32> {
    let path = crate::metrics::metrics_path(&state.config.data_dir);
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v.get("kind").and_then(|k| k.as_str()) == Some("turn"))
        .and_then(|v| v.get("prompt_tokens").and_then(|t| t.as_u64()))
        .map(|t| t as u32)
}

fn blocks_line(blocks: &[SummaryBlock]) -> String {
    if blocks.is_empty() {
        return "compression blocks: none yet".to_string();
    }
    let watermark = blocks.last().map(|b| b.upto_message_id).unwrap_or(0);
    let tokens: i64 = blocks.iter().map(|b| b.summary_tokens).sum();
    format!(
        "compression blocks: {} (watermark at message #{watermark}, {} tokens total)",
        blocks.len(),
        tokens
    )
}

fn status(state: &AppState) -> ToolResult {
    let conn = match init_db(state) {
        Ok(conn) => conn,
        Err(e) => return tool_error(format!("sessions db: {e}")),
    };
    let session = state.active_session_id();

    // Message breakdown of the session (what the request is built from).
    let mut counts = (0i64, 0i64, 0i64);
    let mut chars = 0i64;
    {
        let Ok(mut stmt) = conn.prepare(
            "SELECT role, count(*), sum(length(content)) FROM messages
             WHERE session_id = ?1 GROUP BY role",
        ) else {
            return tool_error("cannot read the session");
        };
        let rows = stmt.query_map(rusqlite::params![session], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        });
        if let Ok(rows) = rows {
            for (role, n, c) in rows.flatten() {
                chars += c;
                match role.as_str() {
                    "user" => counts.0 += n,
                    "assistant" => counts.1 += n,
                    _ => counts.2 += n,
                }
            }
        }
    }
    let total = counts.0 + counts.1 + counts.2;

    let watermark = match crate::tools::session_tool::latest_summary(&conn, &session) {
        Ok(Some(block)) => block.upto_message_id,
        _ => 0,
    };
    let newest = newest_message_id(&conn, &session).unwrap_or(0);
    let compressible = messages_after(&conn, &session, watermark).unwrap_or_default();
    let compressible_chars: i64 = compressible.iter().map(|(_, _, c)| c.len() as i64).sum();
    let keep = state.config.context.keep_recent_messages as i64;
    let budget = state.config.context.summary_max_tokens as i64;
    let compressible_relevant = (compressible.len() as i64 - keep).max(0);
    let compressible_tokens = approx_tokens(compressible_chars as usize);
    let saving = compressible_tokens.saturating_sub(budget as u64);

    let window = state.config.llm.effective_context_window().unwrap_or(0);
    let fill = match (last_prompt_tokens(state), window) {
        (Some(prompt), w) if w > 0 => format!(
            "{} / {} ({} %, last request, provider-counted)",
            human(prompt as u64),
            human(w),
            (prompt as f64 / w as f64 * 100.0).round() as u32
        ),
        (Some(prompt), _) => format!(
            "{} (last request; model window unknown)",
            human(prompt as u64)
        ),
        (None, w) if w > 0 => format!("unknown yet — model window {}", human(w)),
        (None, _) => "unknown yet".to_string(),
    };

    let blocks = summary_blocks(&conn, &session).unwrap_or_default();
    let out = format!(
        "model: {}   window: {}\n\
         fill: {}\n\
         session {session}: {total} messages (user {}, assistant {}, other {}) ≈ {} tokens ({CHARS_PER_TOKEN} chars/token)\n\
         messages: newest #{newest}\n\
         {}\n\
         compressible: {compressible_relevant} messages above the watermark ≈ {} tokens → one block of up to {budget} would save ≈ {}\n\
         recall: `context recall` searches those older messages by keyword (FTS5, no model call)",
        state.config.llm.model,
        if window > 0 { human(window) } else { "unknown".into() },
        fill,
        counts.0,
        counts.1,
        counts.2,
        human(approx_tokens(chars as usize)),
        blocks_line(&blocks),
        human(compressible_tokens),
        human(saving),
    );
    out
}

fn recall(state: &AppState, args: &Value) -> ToolResult {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return tool_error("recall needs 'query'");
    }
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(MAX_HITS as u64) as i64;

    let conn = match init_db(state) {
        Ok(conn) => conn,
        Err(e) => return tool_error(format!("sessions db: {e}")),
    };
    let session = state.active_session_id();
    let Some(match_expr) = fts_expression(&query) else {
        return tool_error("recall needs at least one searchable word");
    };
    let mut stmt = match conn.prepare(
        "SELECT m.id, m.role, m.ts, m.content
         FROM message_fts f JOIN messages m ON m.id = f.rowid
         WHERE message_fts MATCH ?1 AND m.session_id = ?2
         ORDER BY rank LIMIT ?3",
    ) {
        Ok(stmt) => stmt,
        Err(e) => return tool_error(format!("query failed: {e}")),
    };
    let rows = stmt.query_map(rusqlite::params![match_expr, session, limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    });
    let hits: Vec<_> = match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(e) => return tool_error(format!("query failed: {e}")),
    };
    if hits.is_empty() {
        return format!("no hits for {query:?} in {session}");
    }
    let mut out = format!("{} hit(s) for {query:?} in {session}:\n", hits.len());
    for (id, role, ts, content) in hits {
        let excerpt: String = content
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(EXCERPT_CHARS)
            .collect();
        let day = ts.chars().take(16).collect::<String>();
        out.push_str(&format!("  #{id} {role} {day}  {excerpt}\n"));
    }
    out
}

fn show(state: &AppState) -> ToolResult {
    let conn = match init_db(state) {
        Ok(conn) => conn,
        Err(e) => return tool_error(format!("sessions db: {e}")),
    };
    let session = state.active_session_id();
    let blocks = match summary_blocks(&conn, &session) {
        Ok(blocks) => blocks,
        Err(e) => return tool_error(format!("cannot read summaries: {e}")),
    };
    if blocks.is_empty() {
        return format!("no compression blocks for {session} yet");
    }
    let mut out = format!("{} compression block(s) for {session}:\n", blocks.len());
    for block in blocks {
        out.push_str(&format!(
            "\n── up to message #{} ({} tokens, {}) ──\n{}\n",
            block.upto_message_id,
            block.summary_tokens,
            block.created_at.chars().take(16).collect::<String>(),
            block.summary
        ));
    }
    out
}

impl Tool for ContextTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "context",
            description: "Inspect and manage the conversation context: how full it is \
                (status), search this session's older messages by keyword (recall), and \
                the stored compression blocks (show). Recall uses the local full-text \
                index — no model call.",
            emoji: "🧠",
            toolset: "core",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &CONTEXT_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        match action.as_str() {
            "status" => status(&state),
            "recall" => recall(&state, &args),
            "show" => show(&state),
            other => tool_error(format!(
                "unknown action '{other}'. Available: status, recall, show."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::session_tool::{save_summary, SessionWriter};
    use std::sync::Arc;

    fn state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir = dir.path().to_path_buf();
        config.llm.model = "deepseek-flash".into();
        config.llm.context_window = 0; // auto → 1M
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

    fn seed(state: &Arc<AppState>, messages: &[(&str, &str)]) {
        let session = state.active_session_id();
        let writer = SessionWriter::open(state, &session).unwrap();
        for (role, content) in messages {
            writer.push(role, content).unwrap();
        }
    }

    #[test]
    fn status_follows_the_selected_saved_session_not_today_by_default() {
        let (state, _dir) = state();
        state.set_active_session_id("sess_archive-7").unwrap();
        seed(&state, &[("user", "old saved session question")]);

        let out = ContextTool.call(json!({"action": "status"}), state.clone());

        assert!(out.contains("session sess_archive-7"), "{out}");
        assert!(out.contains("1 messages"), "{out}");
    }

    #[test]
    fn status_reports_fill_window_and_saving() {
        let (state, _dir) = state();
        seed(
            &state,
            &[
                ("user", "start the websurfx server on port 8080"),
                ("assistant", "started, curl answered 200"),
                ("tool", "a".repeat(4000).as_str()),
            ],
        );
        let out = ContextTool.call(json!({"action": "status"}), state.clone());
        assert!(
            out.contains("window: 1.0M"),
            "detected window missing: {out}"
        );
        assert!(out.contains("3 messages"), "{out}");
        assert!(out.contains("user 1, assistant 1, other 1"), "{out}");
        assert!(out.contains("compression blocks: none yet"), "{out}");
        assert!(out.contains("messages: newest #3"), "{out}");
        // 4000 chars of tool output ≈ 1000 tokens; nothing may be invented.
        assert!(out.contains("≈ 1.0k tokens"), "{out}");
    }

    #[test]
    fn status_counts_messages_above_the_watermark_only() {
        let (state, _dir) = state();
        let many: Vec<(String, String)> = (1..=20)
            .map(|i| {
                (
                    "user".to_string(),
                    format!("message {i} {}", "x".repeat(200)),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str)> =
            many.iter().map(|(r, c)| (r.as_str(), c.as_str())).collect();
        seed(&state, &borrowed);

        let session = crate::tools::session_tool::current_session_id();
        let conn = init_db(&state).unwrap();
        save_summary(&conn, &session, 8, "compressed the first eight", 100).unwrap();

        let out = ContextTool.call(json!({"action": "status"}), state.clone());
        assert!(
            out.contains("watermark at message #8"),
            "watermark missing: {out}"
        );
        // 12 messages remain above the watermark, of which 12-12 = 0 are
        // compressible with the keep_recent default — the tool must not claim
        // savings it cannot deliver.
        assert!(out.contains("compressible: 0 messages"), "{out}");
    }

    #[test]
    fn recall_finds_older_messages_with_pointers() {
        let (state, _dir) = state();
        seed(
            &state,
            &[
                ("user", "the websurfx daemon hangs on port 8080"),
                ("assistant", "restarted it and measured 1.3 s per query"),
                ("user", "unrelated topic about sqlite wal mode"),
            ],
        );
        let out = ContextTool.call(
            json!({"action": "recall", "query": "websurfx port 8080"}),
            state.clone(),
        );
        assert!(out.contains("hit(s) for"), "{out}");
        assert!(out.contains("websurfx daemon hangs"), "{out}");
        assert!(out.contains("#1 user"), "pointer missing: {out}");
        assert!(!out.contains("sqlite wal"), "irrelevant hit: {out}");
    }

    #[test]
    fn recall_says_so_when_nothing_matches() {
        let (state, _dir) = state();
        seed(&state, &[("user", "nothing relevant here")]);
        let out = ContextTool.call(
            json!({"action": "recall", "query": "zzz-not-present"}),
            state.clone(),
        );
        assert!(out.contains("no hits"), "{out}");
    }

    /// A hyphenated key like `ZIMT-4711` must be findable: as an FTS term it used
    /// to be a syntax error ("no such column: 4711"), so a recall came back empty
    /// although the message was right there.
    #[test]
    fn recall_finds_terms_with_hyphens_and_other_punctuation() {
        let (state, _dir) = state();
        seed(
            &state,
            &[
                ("user", "der Testschluessel lautet ZIMT-4711, bitte merken"),
                ("assistant", "notiert"),
            ],
        );
        for query in ["ZIMT-4711", "zimt-4711", "ZIMT", "4711"] {
            let out = ContextTool.call(json!({"action": "recall", "query": query}), state.clone());
            assert!(
                out.contains("Testschluessel"),
                "query {query:?} found nothing: {out}"
            );
        }
        // The expression itself must never contain FTS operators.
        let expr = fts_expression("ZIMT-4711").unwrap();
        assert_eq!(expr, "ZIMT* 4711*");
        assert!(!expr.contains('-'));
        assert_eq!(fts_expression("a/b:c*d"), Some("a* b* c* d*".to_string()));
        assert_eq!(fts_expression("((("), None);
    }

    #[test]
    fn recall_survives_punctuation_in_the_query() {
        let (state, _dir) = state();
        seed(&state, &[("user", "the cache hit ratio was 26 percent")]);
        // Quotes, colons and stars are FTS syntax — a query full of them must
        // still work (or report no hits), never blow up.
        for query in ["cache: \"hit\" ratio*", "26%", "(((", "*"] {
            let out = ContextTool.call(json!({"action": "recall", "query": query}), state.clone());
            // Either real hits, a clean "no hits", or the explicit note that the
            // query had no searchable word left ("(((") — never an FTS syntax
            // error like "unterminated string".
            let ok = out.contains("hit(s) for")
                || out.contains("no hits")
                || out.contains("searchable word");
            assert!(ok, "query {query:?} produced {out}");
        }
        let hits = ContextTool.call(
            json!({"action": "recall", "query": "cache ratio"}),
            state.clone(),
        );
        assert!(hits.contains("26 percent"), "{hits}");
    }

    #[test]
    fn show_prints_stored_blocks_and_their_watermarks() {
        let (state, _dir) = state();
        seed(&state, &[("user", "one"), ("assistant", "two")]);
        let session = crate::tools::session_tool::current_session_id();
        let conn = init_db(&state).unwrap();

        let empty = ContextTool.call(json!({"action": "show"}), state.clone());
        assert!(empty.contains("no compression blocks"), "{empty}");

        save_summary(
            &conn,
            &session,
            2,
            "user asked for one, assistant answered two",
            12,
        )
        .unwrap();
        let out = ContextTool.call(json!({"action": "show"}), state.clone());
        assert!(out.contains("up to message #2"), "{out}");
        assert!(out.contains("assistant answered two"), "{out}");
    }

    #[test]
    fn read_actions_are_classified_as_reads() {
        for action in ["status", "recall", "show"] {
            assert!(action_is_read(action), "{action}");
        }
        // summarize is not implemented yet and must never count as a read.
        assert!(!action_is_read("summarize"));
        assert!(!action_is_read("nonsense"));
    }
}
