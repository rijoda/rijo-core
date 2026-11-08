//! Debug logging — structured per-tool-call logging with timing + trace file
//!
//! Every tool call goes through `Registry::dispatch`, which calls `log_call`.
//! Logs go to two places:
//! 1. `tracing` (human-readable, RUST_LOG-filtered)
//! 2. `~/.rijo/data/tool_trace.jsonl` (structured, machine-readable)
//!    — one JSON line per call with ts, tool, args-summary, duration, ok.

use serde_json::{json, Value};
use std::time::Duration;

fn trace_path() -> std::path::PathBuf {
    let dir = crate::paths::rijo_home().join("data");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("tool_trace.jsonl")
}

/// Compact args summary: pick the most identifying field per tool.
fn args_summary(tool: &str, args: &Value) -> Value {
    let keys = match tool {
        "terminal" => vec!["command", "background"],
        "read_file" | "write_file" | "patch" | "delete_file" => vec!["path", "offset", "limit"],
        "search_files" | "web_search" | "session_search" => vec!["pattern", "query", "path"],
        "web_extract" => vec!["urls"],
        // Without this the trace showed `{}` for brain_search — you could see
        // that the model searched, but not what for.
        "brain_search" => vec!["query", "top_k", "path_prefix", "all_workspaces"],
        // Which action on which repository — that is what makes a GitHub call
        // traceable later. A token is never an argument of this tool.
        "github" => vec!["action", "repo", "number", "name", "query", "private"],
        "context" => vec!["action", "query", "limit"],
        "skills_list" => vec!["category"],
        "skill_view" | "skill_manage" | "memory" | "delegate_task" | "cronjob_manage" => {
            vec!["name", "action", "target"]
        }
        // The target of a destructive action is the most important information in
        // the trace — previously this was `{}` and therefore nothing traceable.
        "execute_code" => vec!["code"],
        "vision_analyze" => vec!["image_url", "question"],
        "text_to_speech" => vec!["text"],
        "todo_list" | "clarify" => vec![],
        // Background processes: action + target id are what you look for in the trace
        "process_manage" => vec!["action", "session_id", "offset", "limit"],
        _ => vec![],
    };

    let mut obj = serde_json::Map::new();
    for k in keys {
        if let Some(v) = args.get(k) {
            // Store long payloads (code, text) truncated: the trace should stay
            // one line per call.
            let v = match v.as_str() {
                Some(s) if s.chars().count() > 400 => Value::String(format!(
                    "{}… [+{} chars]",
                    s.chars().take(400).collect::<String>(),
                    s.chars().count() - 400
                )),
                _ => v.clone(),
            };
            obj.insert(k.to_string(), v);
        }
    }
    Value::Object(obj)
}

/// Called by Registry::dispatch after every tool execution.
pub fn is_ok(result: &str) -> bool {
    // Proper check: parse the JSON and check if "error" is a non-empty string
    // field. Never use raw substring matching — terminal writes `"error": ""`
    // on SUCCESS (stderr field) which previously logged every call as failed.
    match serde_json::from_str::<Value>(result) {
        Ok(v) => v
            .get("error")
            .and_then(|e| e.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true),
        Err(_) => true, // non-JSON output = heuristic: treat as Ok
    }
}

pub fn log_call(tool: &str, args: &Value, duration: Duration, result: &str) {
    let ok = is_ok(result);
    let ms = duration.as_millis() as u64;

    // Log the error text too: without it the trace is useless when ok=false
    // (you see THAT something failed, but not WHY).
    let err_excerpt: Option<String> = if ok {
        tracing::debug!(tool, duration_ms = ms, "tool ok");
        None
    } else {
        let msg: String = serde_json::from_str::<Value>(result)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.chars().take(200).collect())
            })
            .unwrap_or_else(|| result.chars().take(200).collect());
        tracing::warn!(tool, duration_ms = ms, "tool failed: {msg}");
        Some(msg)
    };

    let record = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "tool": tool,
        "args": args_summary(tool, args),
        "duration_ms": ms,
        "ok": ok,
        "error": err_excerpt,
    });

    // Best-effort append; never fail the tool because of logging.
    // Serialized process-wide: parallel tool batches write here concurrently and
    // interleaved writes corrupt lines (unparseable JSON in the trace).
    let _guard = TRACE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(trace_path())
    {
        use std::io::Write;
        let _ = f.write_all(format!("{record}\n").as_bytes());
    }
}

/// Serializes trace appends (see log_call).
static TRACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ─── Decision trace ──────────────────────────────────────────────────────

/// One turn of the tool decision: what the router predicted, what was sent
/// to the model, and what the model actually called.
///
/// Written for EVERY turn, even while the router is off (`predicted` empty,
/// `fallback: true`) — this file is the live training data for the retrain:
/// prompt → tools the model chose. Structure must stay stable; new fields
/// only get added, none removed (the dataset exporter parses it).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DecisionRecord {
    /// First 800 chars of the user message — the router's input (truncated
    /// like the model sees long prompts cut, and so the trace stays one line).
    pub prompt_trunc: String,
    /// Tools the router scored above threshold (empty = router off/fell back).
    pub predicted: Vec<String>,
    /// Tool schemas actually sent to the model for this turn.
    pub sent_tools: Vec<String>,
    /// Characters of the FULL tool schema set (the no-router baseline).
    pub tools_chars_full: usize,
    /// Characters of the set actually sent (equal to full while off).
    pub tools_chars_sent: usize,
    /// true = the full set was sent because the router was off, failed, or
    /// scored below `min_score` — never because it decided "no tools".
    pub fallback: bool,
    /// Time the router scoring took (0 while off).
    pub router_ms: u64,
    /// Tools the model called, in call order — the label side of training.
    pub llm_final_tools: Vec<String>,
    /// Every tool call of the turn came back without error.
    pub turn_ok: bool,
}

/// Appends one decision record to `<data_dir>/decision_trace.jsonl`.
///
/// Follows `data_dir` (unlike `trace_path`, which is pinned to `~/.rijo` on
/// purpose): A/B runs with their own data dir must not mix their decision
/// data, and the retrain reads exactly this directory.
pub fn log_decision(data_dir: &std::path::Path, rec: &DecisionRecord) {
    let mut record = serde_json::to_value(rec).unwrap_or(Value::Null);
    if let Some(obj) = record.as_object_mut() {
        obj.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
    }
    let _guard = DECISION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(data_dir.join("decision_trace.jsonl"))
    {
        use std::io::Write;
        let _ = f.write_all(format!("{record}\n").as_bytes());
    }
}

/// Serializes decision-trace appends (see `log_decision`).
static DECISION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// First `cap` chars of the prompt for the decision trace (byte-safe via
/// chars: the prompts are German/English, truncating bytes could split one).
pub fn truncate_for_trace(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let cut: String = s.chars().take(cap).collect();
    format!("{cut}…")
}

/// Read the last N tool-call trace entries (for debugging / benchmarks)
pub fn last_calls(n: usize) -> Vec<Value> {
    let content = match std::fs::read_to_string(trace_path()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Aggregate per-tool stats from the trace (count, p50, p95 in ms)
pub fn tool_stats() -> Value {
    let content = match std::fs::read_to_string(trace_path()) {
        Ok(c) => c,
        Err(_) => return json!({}),
    };
    let mut by_tool: std::collections::HashMap<String, Vec<u64>> = std::collections::HashMap::new();
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            let tool = v["tool"].as_str().unwrap_or("?").to_string();
            let ms = v["duration_ms"].as_u64().unwrap_or(0);
            by_tool.entry(tool).or_default().push(ms);
        }
    }
    let mut out = serde_json::Map::new();
    for (tool, mut times) in by_tool {
        times.sort_unstable();
        let n = times.len();
        let p50 = times[n / 2];
        let p95 = times[(n as f32 * 0.95).clamp(0.0, (n - 1) as f32) as usize];
        out.insert(tool, json!({"count": n, "p50_ms": p50, "p95_ms": p95}));
    }
    Value::Object(out)
}

/// Diagnostics from the turn path that must never be written straight to stderr
/// while the TUI owns the terminal.
///
/// Real report (2026-09-17): the context-pruning note appeared in the middle of
/// the status row ("... Ctrl+O=details   d, 0 message(s) dropped (35 in the
/// request)") and the layout below it was left shifted, because `eprintln!`
/// moves the cursor behind ratatui's back — its buffer no longer matches the
/// screen.
///
/// Always appended to `<data_dir>/diagnostics.log`. While the TUI owns the
/// terminal nothing is drawn for it — there used to be a toast in the right part
/// of the chat area, which the user explicitly did not want. Headless runs keep
/// printing to stderr, where stderr IS the channel.
pub fn note(state: &crate::AppState, text: &str) {
    use std::io::Write;
    let line = format!("{} {text}\n", chrono::Utc::now().to_rfc3339());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.config.data_dir.join("diagnostics.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
    let tui_attached = state
        .ui_tx
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    if !tui_attached {
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> DecisionRecord {
        DecisionRecord {
            prompt_trunc: "Zeig mir den Inhalt von crates/…".into(),
            predicted: vec![],
            sent_tools: vec!["read_file".into(), "terminal".into()],
            tools_chars_full: 10278,
            tools_chars_sent: 10278,
            fallback: true,
            router_ms: 0,
            llm_final_tools: vec!["read_file".into()],
            turn_ok: true,
        }
    }

    /// The record must land as ONE parseable JSON line, with ts and all
    /// fields — the dataset exporter depends on exactly that.
    #[test]
    fn decision_trace_writes_one_parseable_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        log_decision(dir.path(), &sample_record());
        let text =
            std::fs::read_to_string(dir.path().join("decision_trace.jsonl")).expect("trace file");
        assert_eq!(text.lines().count(), 1, "one line per turn");
        let v: Value = serde_json::from_str(text.trim()).expect("valid JSON line");
        assert!(v["ts"].is_string(), "ts present");
        assert_eq!(v["tools_chars_full"], 10278);
        assert_eq!(v["llm_final_tools"][0], "read_file");
        assert_eq!(v["fallback"], true);
        assert_eq!(v["turn_ok"], true);
    }

    /// Second turn appends, does not overwrite.
    #[test]
    fn decision_trace_appends() {
        let dir = tempfile::tempdir().expect("tempdir");
        log_decision(dir.path(), &sample_record());
        log_decision(dir.path(), &sample_record());
        let text = std::fs::read_to_string(dir.path().join("decision_trace.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    /// Char-safe truncation: German umlauts must not be split.
    #[test]
    fn truncate_is_char_safe() {
        let s = "Lösche die Datei äöüß.txt";
        let cut = truncate_for_trace(s, 10);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 11);
        // No replacement char from a split UTF-8 sequence.
        assert!(!cut.contains('\u{FFFD}'));
        assert_eq!(truncate_for_trace("kurz", 800), "kurz");
    }
}
